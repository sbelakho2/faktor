//! Reproducible multi-agent contention benchmark (audit 27; audit 5 fixes).
//!
//! Fixed-shape workload: 32 concurrent sessions, each with a child session
//! (orchestrator identity + budget scope), driving concurrent turn
//! bookkeeping (message + part + journal event), tool-run records, durable
//! budget-cap writes; direct journal readers; real SSE readers against the
//! daemon's `/native/session/{id}/events` stream; REAL on-disk repository
//! index rebuilds through `faktor-index`'s durable `IndexService` (generation
//! publishing, store rows, coverage/fingerprint continuation, watcher and
//! writer interaction); and a WAL sampler.
//!
//! Audit-5 accounting rules this harness obeys:
//! * Event latency is keyed by `(SessionId, EventSeq)` — event sequences are
//!   per session, so a seq-only map mixes 32 sessions' appends.
//! * Writer wait is taken from PER-JOB receipts (`label`, queue wait,
//!   execution) drained from the store — never from before/after deltas of
//!   global cumulative counters, which mix every concurrent thread's jobs.
//! * `messages_page` (full call duration) and `reader_pool_wait` (the
//!   permit wait instrumented inside `Store::read`) are separate axes; the
//!   store exposes the permit wait, the caller exposes the page duration.
//! * Index rebuilds are real: files are mutated on disk between generations
//!   and each measured generation is driven through
//!   `IndexService::request_build` + reconcile until it is durably PUBLISHED
//!   (Ready row, complete content coverage, complete fingerprint round,
//!   served symbols), not an in-memory `WorkspaceIndex` toy.
//!
//! It is `#[ignore]`d on purpose so normal PR runs stay fast: the trusted
//! `perf` lane runs `cargo test -p faktor-tests-performance --release --
//! --ignored`, and the nightly `contention` lane runs this file directly.
//!
//! Reproducibility: no randomness, no wall-clock-dependent iteration counts
//! (all loops are bounded by the fixed workload); the report carries the
//! commit and profile metadata. Timing values are machine dependent by
//! nature; the BOUNDS are deliberately loose (tail-latency teeth only).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use faktor_core::event::EventKind;
use faktor_core::id::{EventSeq, OpId, SessionId, TaskId, WorkspaceId, WorktreeId};
use faktor_core::state::AgentState;
use faktor_index::{EvidenceFreshness, IndexService, IndexView};
use faktor_session::SessionManager;
use faktor_tests_performance::{build_meta, format_pct, rss_kb, Dist};
use serde_json::json;
use tempfile::tempdir;

/// The fixed workload shape (audit 27: 32 concurrent sessions).
const SESSIONS: usize = 32;
const TURNS_PER_SESSION: usize = 48;
const READER_THREADS: usize = 4;
const INDEX_THREADS: usize = 2;
const INDEX_FILES: usize = 64;
const SSE_CONNECTIONS: usize = 4;
const TURNS_PER_TOOL: usize = 1;
/// Hard wall cap: a stuck workload fails loudly instead of hanging CI.
const HARD_CAP: Duration = Duration::from_secs(300);
/// One real index generation (mutate -> fingerprint -> dirty -> rebuild ->
/// durable publish) must complete within this or the benchmark fails.
const INDEX_REBUILD_DEADLINE: Duration = Duration::from_secs(60);

struct Ctl {
    stop: AtomicBool,
    /// Append times keyed by `(SessionId, seq)`: event sequences are
    /// per-session, so a seq-only key would mix sessions.
    append_times: Mutex<HashMap<(SessionId, u64), Instant>>,
    errors: Mutex<Vec<String>>,
    events_appended: AtomicU64,
}

impl Ctl {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            stop: AtomicBool::new(false),
            append_times: Mutex::new(HashMap::new()),
            errors: Mutex::new(Vec::new()),
            events_appended: AtomicU64::new(0),
        })
    }

    fn fail(&self, message: String) {
        self.errors
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(message);
    }

    fn note_append(&self, session: SessionId, seq: u64, at: Instant) {
        self.append_times
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert((session, seq), at);
        self.events_appended.fetch_add(1, Ordering::Relaxed);
    }

    fn lag_since(
        &self,
        session: SessionId,
        seq: u64,
        floor: Instant,
        now: Instant,
    ) -> Option<Duration> {
        let map = self.append_times.lock().unwrap_or_else(|p| p.into_inner());
        let at = *map.get(&(session, seq))?;
        if at < floor {
            return None;
        }
        now.checked_duration_since(at)
    }
}

#[derive(Default)]
struct Report {
    turn: Dist,
    tool: Dist,
    budget: Dist,
    /// Per-job writer receipts: queue wait and execution duration for every
    /// enqueued command, all labels pooled (receipts are per job, never
    /// before/after deltas).
    writer_queue_wait: Dist,
    writer_exec: Dist,
    /// Full `messages_page()` duration at the caller.
    messages_page: Dist,
    /// Permit wait inside `Store::read`, drained from the store's bounded
    /// per-read receipts (separate axis from `messages_page`).
    reader_pool_wait: Dist,
    event_lag: Dist,
    sse_lag: Dist,
    index_rebuild: Dist,
    wal_peak_bytes: u64,
    wal_final_bytes: u64,
    rss_start_kb: u64,
    rss_end_kb: u64,
    turns: u64,
    turns_per_session: usize,
    workload_sessions: usize,
    workload_sse: usize,
    events_appended: u64,
    /// Receipt counts per writer command label (deterministic ordering).
    writer_receipts_by_label: BTreeMap<&'static str, u64>,
    writer_queue_wait_total_ns: u64,
    writer_queue_wait_max_ns: u64,
    writer_pending_max: usize,
    writer_refusals: u64,
    writer_receipts_dropped: u64,
    reader_reads: u64,
    reader_permit_wait_total_ns: u64,
    reader_permit_wait_max_ns: u64,
    reader_receipts_dropped: u64,
    /// Real index generations published inside the measured loop.
    index_generations: u64,
}

impl Report {
    fn summary(&self) -> serde_json::Value {
        let d = |dist: &Dist| {
            json!({
                "n": dist.len(),
                "p50": q(dist, 50.0) / 1e6,
                "p95": q(dist, 95.0) / 1e6,
                "p99": q(dist, 99.0) / 1e6,
                "max_ms": qmax(dist),
            })
        };
        json!({
            "schema": "faktor-contention-bench/v2",
            "workload": {
                "sessions": self.workload_sessions,
                "child_agents": self.workload_sessions,
                "turns_per_session": self.turns_per_session,
                "reader_threads": READER_THREADS,
                "index_threads": INDEX_THREADS,
                "index_files": INDEX_FILES,
                "sse_connections": self.workload_sse,
            },
            "turns": self.turns,
            "events_appended": self.events_appended,
            "latency_ms": {
                "turn_bookkeeping": d(&self.turn),
                "tool_record": d(&self.tool),
                "budget_write": d(&self.budget),
                "writer_queue_wait": d(&self.writer_queue_wait),
                "writer_exec": d(&self.writer_exec),
                "messages_page": d(&self.messages_page),
                "reader_pool_wait": d(&self.reader_pool_wait),
                "event_stream_lag": d(&self.event_lag),
                "sse_frame_lag": d(&self.sse_lag),
                "index_rebuild": d(&self.index_rebuild),
            },
            "wal": {
                "peak_bytes": self.wal_peak_bytes,
                "final_bytes": self.wal_final_bytes,
            },
            "memory": {
                "rss_start_kb": self.rss_start_kb,
                "rss_end_kb": self.rss_end_kb,
            },
            "writer": {
                "queue_wait_total_ns": self.writer_queue_wait_total_ns,
                "queue_wait_max_ns": self.writer_queue_wait_max_ns,
                "pending_max": self.writer_pending_max,
                "refusals": self.writer_refusals,
                "receipts_dropped": self.writer_receipts_dropped,
                "receipts_by_label": self.writer_receipts_by_label,
            },
            "reader": {
                "reads": self.reader_reads,
                "permit_wait_total_ns": self.reader_permit_wait_total_ns,
                "permit_wait_max_ns": self.reader_permit_wait_max_ns,
                "receipts_dropped": self.reader_receipts_dropped,
            },
            "index": {
                "generations": self.index_generations,
            },
            "meta": build_meta(),
        })
    }
}

/// Quantile that tolerates an empty series (report-only printing).
fn q(dist: &Dist, p: f64) -> f64 {
    if dist.is_empty() {
        0.0
    } else {
        dist.pct(p)
    }
}

fn qmax(dist: &Dist) -> f64 {
    if dist.is_empty() {
        0.0
    } else {
        dist.max() as f64 / 1e6
    }
}

fn wal_bytes(root: &Path) -> u64 {
    let path = root.join("faktor-plus.db-wal");
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn payload(turn: usize) -> serde_json::Value {
    json!({"text": format!("bench turn {turn}"), "tokens": turn % 97})
}

/// Drain the store's per-job writer receipts and per-read permit receipts
/// into the report. Every receipt is attributed to its own command/read; a
/// consumer never differences global counters.
fn drain_receipts(store: &faktor_store::Store, report: &Mutex<Report>) {
    let receipts = store.take_writer_receipts();
    let reader_receipts = store.take_reader_receipts();
    if receipts.is_empty() && reader_receipts.is_empty() {
        return;
    }
    let mut r = report.lock().unwrap_or_else(|p| p.into_inner());
    for receipt in receipts {
        r.writer_queue_wait.push(receipt.queue_wait_ns);
        r.writer_exec.push(receipt.run_ns);
        *r.writer_receipts_by_label.entry(receipt.label).or_insert(0) += 1;
    }
    for receipt in reader_receipts {
        r.reader_pool_wait.push(receipt.permit_wait_ns);
    }
}

#[allow(clippy::too_many_arguments)]
fn writer_thread(
    label: usize,
    handle: faktor_session::SessionHandle,
    child_session: SessionId,
    budgets: Arc<faktor_session::DurableBudgetLedger>,
    store: Arc<faktor_store::Store>,
    ctl: Arc<Ctl>,
    report: Arc<Mutex<Report>>,
    turns: usize,
) {
    let sid = handle.id();
    let started = Instant::now();
    for turn in 0..turns {
        if started.elapsed() > HARD_CAP {
            ctl.fail(format!("writer {label}: hard cap exceeded"));
            break;
        }
        let seq = turn as i64 + 1;
        // ---- turn bookkeeping: message + part + journal event.
        let t0 = Instant::now();
        let message_id = handle
            .put_message(seq, "assistant", payload(turn))
            .unwrap_or_else(|e| panic!("writer {label}: put_message: {e}"));
        handle
            .put_text_part(message_id, "bench part")
            .unwrap_or_else(|e| panic!("writer {label}: put_text_part: {e}"));
        let appended_at = Instant::now();
        let event_seq = store
            .append_event(
                sid,
                None,
                EventKind::PhaseChanged,
                AgentState::WaitingForModel,
                handle.now_ms(),
                Some(json!({"bench": turn})),
            )
            .unwrap_or_else(|e| panic!("writer {label}: append_event: {e}"));
        ctl.note_append(sid, event_seq.raw(), appended_at);
        let turn_ns = t0.elapsed().as_nanos() as u64;
        {
            let mut r = report.lock().unwrap_or_else(|p| p.into_inner());
            r.turn.push(turn_ns);
            r.turns += 1;
        }

        // ---- tool records (start + finish through the writer service).
        for tool in 0..TURNS_PER_TOOL {
            // tool_run.op_id is a store-wide key: scope it by writer and turn.
            let op = OpId::new((label as u64) * 1_000_000 + (turn * 16 + tool + 1) as u64);
            let t0 = Instant::now();
            store
                .start_tool_run(
                    sid,
                    op,
                    "bench_tool",
                    json!({"turn": turn}),
                    json!({"strategy": "none"}),
                    None,
                    None,
                )
                .unwrap_or_else(|e| panic!("writer {label}: start_tool_run: {e}"));
            store
                .finish_tool_run(sid, op, "completed", "verified")
                .unwrap_or_else(|e| panic!("writer {label}: finish_tool_run: {e}"));
            report
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .tool
                .push(t0.elapsed().as_nanos() as u64);
        }

        // ---- durable budget write on the child scope.
        let t0 = Instant::now();
        budgets
            .change_child_scope_cap(
                child_session,
                &format!("bench-child-{label}"),
                Some((turn as u64 + 1) * 1_000),
            )
            .unwrap_or_else(|e| panic!("writer {label}: change_child_scope_cap: {e}"));
        report
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .budget
            .push(t0.elapsed().as_nanos() as u64);

        // ---- drain this turn's per-job receipts (writer + reader pool).
        drain_receipts(&store, &report);
    }
}

fn direct_reader_thread(
    label: usize,
    sessions: Vec<faktor_session::SessionHandle>,
    store: Arc<faktor_store::Store>,
    ctl: Arc<Ctl>,
    report: Arc<Mutex<Report>>,
) {
    let started = Instant::now();
    let mut last: HashMap<SessionId, u64> = HashMap::new();
    let mut first_pass = true;
    while first_pass || !ctl.stop.load(Ordering::Relaxed) {
        first_pass = false;
        if started.elapsed() > HARD_CAP {
            ctl.fail(format!("reader {label}: hard cap exceeded"));
            return;
        }
        for handle in &sessions {
            let t0 = Instant::now();
            let page = handle.messages_page(None, 20);
            let page_ns = t0.elapsed().as_nanos() as u64;
            if let Err(e) = page {
                ctl.fail(format!("reader {label}: messages_page: {e}"));
                return;
            }
            let after = last.get(&handle.id()).copied().unwrap_or(0);
            match handle.events_after(EventSeq::new(after + 1)) {
                Ok(events) => {
                    let now = Instant::now();
                    let mut max_seq = after;
                    for event in &events {
                        max_seq = max_seq.max(event.seq.raw());
                        if let Some(lag) = ctl.lag_since(handle.id(), event.seq.raw(), started, now)
                        {
                            report
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .event_lag
                                .push(lag.as_nanos() as u64);
                        }
                    }
                    if max_seq > after {
                        last.insert(handle.id(), max_seq);
                    }
                }
                Err(e) => {
                    ctl.fail(format!("reader {label}: events_after: {e}"));
                    return;
                }
            }
            // The full messages_page() duration is its own axis; the
            // reader-pool permit wait comes from the store's per-read
            // receipts, never from this call duration.
            report
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .messages_page
                .push(page_ns);
            drain_receipts(&store, &report);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Write one generation's worth of real files into the on-disk repository.
/// Every round mutates every file, so each measured rebuild is a genuine
/// fingerprint-diff -> Dirty -> rebuild -> publish cycle.
fn write_bench_files(repo: &Path, round: u64, ctl: &Ctl, label: usize) -> bool {
    for file in 0..INDEX_FILES {
        let rel = format!("bench/f{file}.rs");
        let path = repo.join(&rel);
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                ctl.fail(format!("index {label}: create_dir_all: {e}"));
                return false;
            }
        }
        let src = format!(
            "pub fn alpha_{file}() -> i64 {{ {file} }}\n\
             pub fn bench_round_{round}_marker_{file}() {{}}\n"
        );
        if let Err(e) = std::fs::write(&path, src) {
            ctl.fail(format!("index {label}: write {}: {e}", path.display()));
            return false;
        }
    }
    true
}

/// Drive one real generation to durable PUBLICATION: reconcile the machine
/// (`request_build` was issued by the caller) until the newest generation is
/// at rest, then verify the durable row, complete content coverage, complete
/// fingerprint round, served view and identities. Returns the published view.
fn drive_index_generation(
    label: usize,
    ws: WorkspaceId,
    svc: &IndexService,
    store: &faktor_store::Store,
    previous: u64,
    ctl: &Ctl,
) -> Option<IndexView> {
    let deadline = Instant::now() + INDEX_REBUILD_DEADLINE;
    loop {
        if let Err(e) = svc.reconcile_now(ws) {
            ctl.fail(format!("index {label}: reconcile_now: {e}"));
            return None;
        }
        if let Some(snapshot) = svc.coverage_snapshot(ws) {
            let published_new = snapshot.published_generation == Some(snapshot.generation)
                && snapshot.generation > previous;
            let at_rest = published_new
                && snapshot.freshness == EvidenceFreshness::Current
                && snapshot.serving;
            if at_rest {
                // Durable row must name the published generation.
                match store.index_state_get(ws) {
                    Ok(Some(row))
                        if row.generation >= 0 && row.generation as u64 == snapshot.generation => {}
                    Ok(other) => {
                        ctl.fail(format!(
                            "index {label}: durable row does not name the published generation: {other:?}"
                        ));
                        return None;
                    }
                    Err(e) => {
                        ctl.fail(format!("index {label}: index_state_get: {e}"));
                        return None;
                    }
                }
                // Continuation: the generation is COMPLETE (no bounded
                // fallback owed) and the fingerprint round completed.
                if snapshot.coverage.needs_fallback_on_miss() || !snapshot.fingerprint.complete {
                    ctl.fail(format!(
                        "index {label}: published generation {} is incomplete",
                        snapshot.generation
                    ));
                    return None;
                }
                match svc.view(ws) {
                    Some(view) if view.generation() == snapshot.generation => {
                        if view.content_identity().is_empty()
                            || view.fingerprint_identity().is_empty()
                        {
                            ctl.fail(format!(
                                "index {label}: generation {} published without identities",
                                snapshot.generation
                            ));
                            return None;
                        }
                        return Some(view);
                    }
                    other => {
                        ctl.fail(format!("index {label}: view generation drift: {other:?}"));
                        return None;
                    }
                }
            }
        }
        if Instant::now() >= deadline {
            ctl.fail(format!(
                "index {label}: no published generation newer than {previous} within {INDEX_REBUILD_DEADLINE:?}"
            ));
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Real on-disk index rebuild loop: mutate files, `request_build`, reconcile
/// to a durably published generation, verify the served content, record the
/// measured rebuild. The body runs at least once so the smoke always
/// exercises the real index path even when writers finish first.
fn index_rebuild_thread(
    label: usize,
    ws: WorkspaceId,
    repo: PathBuf,
    svc: Arc<IndexService>,
    store: Arc<faktor_store::Store>,
    ctl: Arc<Ctl>,
    report: Arc<Mutex<Report>>,
) {
    let started = Instant::now();
    // Attach + initial real build (untimed): the measured loop rebuilds an
    // existing published generation.
    if let Err(e) = svc.request_build(ws) {
        ctl.fail(format!("index {label}: request_build: {e}"));
        return;
    }
    let Some(view) = drive_index_generation(label, ws, &svc, &store, 0, &ctl) else {
        return;
    };
    let mut previous = view.generation();
    let mut round = 0u64;
    loop {
        if started.elapsed() > HARD_CAP {
            ctl.fail(format!("index {label}: hard cap exceeded"));
            return;
        }
        round += 1;
        if !write_bench_files(&repo, round, &ctl, label) {
            return;
        }
        if let Err(e) = svc.request_build(ws) {
            ctl.fail(format!("index {label}: request_build: {e}"));
            return;
        }
        let t0 = Instant::now();
        let Some(view) = drive_index_generation(label, ws, &svc, &store, previous, &ctl) else {
            return;
        };
        let rebuild_ns = t0.elapsed().as_nanos() as u64;
        // The published generation really serves this round's files/symbols.
        {
            let arc = view.index();
            let index = arc.lock().unwrap_or_else(|p| p.into_inner());
            let files = index.file_paths(ws);
            if files.len() != INDEX_FILES {
                ctl.fail(format!(
                    "index {label}: published generation has {} files, expected {INDEX_FILES}",
                    files.len()
                ));
                return;
            }
            let probe = format!("bench_round_{round}_marker_0");
            if index.symbol_lookup(ws, &probe, 10).is_empty() {
                ctl.fail(format!(
                    "index {label}: published generation lost symbol {probe}"
                ));
                return;
            }
        }
        previous = view.generation();
        {
            let mut r = report.lock().unwrap_or_else(|p| p.into_inner());
            r.index_rebuild.push(rebuild_ns);
            r.index_generations += 1;
        }
        drain_receipts(&store, &report);
        // At least one measured generation per worker, then stop when the
        // writers are done (one extra rebuild after `stop` is the point of
        // the do-while).
        if ctl.stop.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn wal_sampler_thread(root: std::path::PathBuf, ctl: Arc<Ctl>, peak: Arc<AtomicU64>) {
    while !ctl.stop.load(Ordering::Relaxed) {
        let bytes = wal_bytes(&root);
        peak.fetch_max(bytes, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(25));
    }
    peak.fetch_max(wal_bytes(&root), Ordering::Relaxed);
}

/// Minimal raw-SSE reader: opens the native journal stream over TCP and
/// records the lag from journal append to observed frame, keyed by the
/// stream's own session (event seqs are per session).
async fn sse_reader_task(
    addr: std::net::SocketAddr,
    token: String,
    session: SessionId,
    ctl: Arc<Ctl>,
    report: Arc<Mutex<Report>>,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let session_id = session.to_string();
    let started = Instant::now();
    let Ok(stream) = tokio::net::TcpStream::connect(addr).await else {
        ctl.fail(format!("sse {session_id}: connect failed"));
        return;
    };
    let mut stream = stream;
    let request = format!(
        "GET /native/session/{session_id}/events?after=0 HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         Authorization: Bearer {token}\r\n\
         Accept: text/event-stream\r\n\
         Connection: keep-alive\r\n\r\n"
    );
    if let Err(e) = stream.write_all(request.as_bytes()).await {
        ctl.fail(format!("sse {session_id}: write failed: {e}"));
        return;
    }
    if let Err(e) = stream.flush().await {
        ctl.fail(format!("sse {session_id}: flush failed: {e}"));
        return;
    }
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        match tokio::time::timeout(Duration::from_millis(300), reader.read_line(&mut line)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(_)) => {
                if let Some(seq) = line.trim_end().strip_prefix("id: ") {
                    if let Ok(seq) = seq.trim().parse::<u64>() {
                        let now = Instant::now();
                        if let Some(lag) = ctl.lag_since(session, seq, started, now) {
                            report
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .sse_lag
                                .push(lag.as_nanos() as u64);
                        }
                    }
                }
            }
            Ok(Err(e)) => {
                ctl.fail(format!("sse {session_id}: read failed: {e}"));
                break;
            }
            Err(_) => {
                if ctl.stop.load(Ordering::Relaxed) {
                    break;
                }
            }
        }
    }
    let _ = reader.into_inner().shutdown().await;
}

/// Runs the fixed workload. `with_sse` starts the real daemon and opens the
/// SSE readers; the smoke variant skips the daemon to stay fast.
fn run_contention(scale_sessions: usize, turns: usize, with_sse: bool) -> Report {
    let dir = tempdir().unwrap();
    let manager = SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
        .expect("session manager");
    let store = manager.store();
    let budgets = faktor_session::DurableBudgetLedger::new(manager.clone());
    let ctl = Ctl::new();
    let report = Arc::new(Mutex::new(Report::default()));
    let rss_start = rss_kb();

    let mut root_sessions = Vec::new();
    let mut child_sessions = Vec::new();
    let mut workspaces = Vec::new();
    let mut repos = Vec::new();
    for i in 0..scale_sessions {
        // Real on-disk repository per workspace: the index rebuilds below
        // are real IndexService generations, not in-memory toys.
        let repo = dir.path().join(format!("repo-{i}"));
        std::fs::create_dir_all(&repo).expect("repo dir");
        assert!(
            write_bench_files(&repo, 0, &ctl, i),
            "initial bench files must be writable"
        );
        let ws = manager
            .create_workspace(repo.to_str().expect("utf8 repo path"))
            .expect("workspace");
        workspaces.push(ws);
        repos.push(repo);
        let root = manager
            .create_session(ws, &format!("bench-root-{i}"), "fake", "m")
            .expect("root session");
        let child = manager
            .create_child_session(
                root.id(),
                ws,
                WorktreeId::new(i as u64 + 1),
                TaskId::new(i as u64 + 1),
                "fake",
                "m",
                &format!("bench-child-{i}"),
                faktor_session::child::ChildOwnership::ExclusivePaths,
            )
            .expect("child session");
        child_sessions.push(child.id());
        root_sessions.push(root);
    }

    // The REAL durable index service over the same store: generations are
    // published to disk, mirrored in store rows and driven through
    // request_build/reconcile (watcher + fingerprint + writer interaction).
    let index_svc = IndexService::open(
        store.clone(),
        dir.path().join("index-data"),
        faktor_fs::WorkspaceFileService::new(),
    )
    .expect("index service");
    index_svc.set_config(faktor_index::ServiceConfig {
        poll: Duration::from_millis(25),
        fingerprint_interval: Duration::from_millis(50),
        build_lease: Duration::from_secs(60),
        ..faktor_index::ServiceConfig::default()
    });

    // The real daemon + SSE readers (skipped by the fast smoke).
    let mut runtime = None;
    let mut server = None;
    let mut sse_tasks = Vec::new();
    if with_sse {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("tokio runtime");
        let perm =
            faktor_server::permission::ChannelPermissionRequester::new(Duration::from_secs(5));
        let registry = Arc::new(faktor_provider::ProviderRegistry::new());
        let agent = {
            let mut deps = faktor_agent::AgentDeps {
                session: manager.clone(),
                providers: registry,
                chunk_sink: None,
                permission_requester: perm.clone(),
                evidence: Arc::new(faktor_agent::NoEvidence),
                tools: Arc::new(faktor_agent::ToolRegistry::new()),
                cas: None,
                workspaces: faktor_fs::WorkspaceFileService::new(),
                edit: None,
                snapshots: None,
                sandbox: None,
                supervisor: None,
                verification: faktor_agent::VerificationService::disabled(),
                hooks: None,
                instructions_resolver: faktor_instructions::no_roots_resolver(),
                routing: faktor_agent::FixedRoutingPolicy::passthrough(),
                budgets: Arc::new(faktor_session::NoopBudget),
                model: "m".into(),
                compaction_model: None,
                compact_at_usage: 0.65,
                instructions: "i".into(),
                clock: Arc::new(faktor_core::time::SystemClock),
                tool_call_mode: faktor_agent::ToolCallMode::Native,
                tool_deadline_ms: 1000,
                retry_policy: faktor_core::retry::RetryPolicy::default(),
                semantic: faktor_agent::fallback_semantic_registry(),
                context_prior: None,
                efficiency: Default::default(),
            };
            deps.permission_requester = perm.clone();
            faktor_agent::AgentRuntime::new(deps).expect("agent runtime")
        };
        let deps =
            faktor_server::ServerDeps::new(manager.clone(), agent, perm).expect("server deps");
        let token = deps.auth_token.as_str().to_owned();
        let handle = rt
            .block_on(async { faktor_server::serve(deps, 0).await })
            .expect("serve");
        let addr = handle.addr;
        for i in 0..SSE_CONNECTIONS {
            let root = &root_sessions[i % root_sessions.len()];
            sse_tasks.push(rt.spawn(sse_reader_task(
                addr,
                token.clone(),
                root.id(),
                ctl.clone(),
                report.clone(),
            )));
        }
        runtime = Some(rt);
        server = Some(handle);
    }

    // Real index rebuilders (one durable IndexService shared by both).
    let mut index_handles = Vec::new();
    for i in 0..INDEX_THREADS {
        let ws = workspaces[i % workspaces.len()];
        let repo = repos[i % repos.len()].clone();
        let svc = index_svc.clone();
        let store = store.clone();
        let ctl = ctl.clone();
        let report = report.clone();
        index_handles.push(std::thread::spawn(move || {
            index_rebuild_thread(i, ws, repo, svc, store, ctl, report)
        }));
    }

    // Direct journal readers.
    let mut reader_handles = Vec::new();
    for r in 0..READER_THREADS {
        let own: Vec<faktor_session::SessionHandle> = root_sessions
            .iter()
            .enumerate()
            .filter(|(i, _)| i % READER_THREADS == r)
            .map(|(_, h)| h.clone())
            .collect();
        let ctl = ctl.clone();
        let report = report.clone();
        let store = store.clone();
        reader_handles.push(std::thread::spawn(move || {
            direct_reader_thread(r, own, store, ctl, report)
        }));
    }

    // WAL sampler.
    let wal_peak = Arc::new(AtomicU64::new(0));
    let ctl_wal = ctl.clone();
    let root_path = dir.path().join("store");
    let peak = wal_peak.clone();
    let sampler_root = root_path.clone();
    let wal_handle = std::thread::spawn(move || wal_sampler_thread(sampler_root, ctl_wal, peak));

    // Writers: the contention core.
    let started = Instant::now();
    let mut writer_handles = Vec::new();
    for i in 0..scale_sessions {
        let handle = root_sessions[i].clone();
        let child_session = child_sessions[i];
        let budgets = budgets.clone();
        let store = store.clone();
        let ctl = ctl.clone();
        let report = report.clone();
        writer_handles.push(std::thread::spawn(move || {
            writer_thread(i, handle, child_session, budgets, store, ctl, report, turns)
        }));
    }
    for handle in writer_handles {
        handle.join().expect("writer thread");
    }
    let elapsed = started.elapsed();
    ctl.stop.store(true, Ordering::Relaxed);
    for handle in reader_handles {
        handle.join().expect("reader thread");
    }
    for handle in index_handles {
        handle.join().expect("index thread");
    }
    wal_handle.join().expect("wal thread");

    if let (Some(rt), Some(handle)) = (runtime.take(), server.take()) {
        for task in sse_tasks {
            task.abort();
        }
        rt.block_on(handle.shutdown()).expect("server shutdown");
    }

    // Final receipt drain, then the cumulative telemetry snapshot.
    drain_receipts(&store, &report);
    let telemetry = store.writer_telemetry();
    let reader_telemetry = store.reader_telemetry();
    let mut r = report.lock().unwrap_or_else(|p| p.into_inner());
    r.wal_peak_bytes = wal_peak.load(Ordering::Relaxed);
    r.wal_final_bytes = wal_bytes(&root_path);
    r.rss_start_kb = rss_start;
    r.rss_end_kb = rss_kb();
    r.turns_per_session = turns;
    r.workload_sessions = scale_sessions;
    r.workload_sse = if with_sse { SSE_CONNECTIONS } else { 0 };
    r.events_appended = ctl.events_appended.load(Ordering::Relaxed);
    r.writer_queue_wait_total_ns = telemetry.queue_wait_total_ns;
    r.writer_queue_wait_max_ns = telemetry.queue_wait_max_ns;
    r.writer_pending_max = telemetry.pending_max;
    r.writer_refusals = telemetry.queue_full_refusals;
    r.writer_receipts_dropped = telemetry.receipts_dropped;
    r.reader_reads = reader_telemetry.reads;
    r.reader_permit_wait_total_ns = reader_telemetry.permit_wait_total_ns;
    r.reader_permit_wait_max_ns = reader_telemetry.permit_wait_max_ns;
    r.reader_receipts_dropped = reader_telemetry.receipts_dropped;
    let errors = ctl.errors.lock().unwrap_or_else(|p| p.into_inner()).clone();
    assert!(
        errors.is_empty(),
        "contention workload reported errors: {errors:?}"
    );
    assert!(
        !r.turn.is_empty(),
        "the benchmark must have measured turn bookkeeping"
    );
    println!(
        "[contention] workload={}sessions/{}turns elapsed={:.2}s \
         turn_p50={} turn_p95={} writer_queue_wait_p95={} writer_exec_p95={} \
         messages_page_p95={} reader_pool_wait_p95={} event_lag_p99={} \
         sse_lag_p99={} index_rebuild_p95={} wal_final={} bytes rss={}KB",
        scale_sessions,
        turns,
        elapsed.as_secs_f64(),
        format_pct(q(&r.turn, 50.0)),
        format_pct(q(&r.turn, 95.0)),
        format_pct(q(&r.writer_queue_wait, 95.0)),
        format_pct(q(&r.writer_exec, 95.0)),
        format_pct(q(&r.messages_page, 95.0)),
        format_pct(q(&r.reader_pool_wait, 95.0)),
        format_pct(q(&r.event_lag, 99.0)),
        format_pct(q(&r.sse_lag, 99.0)),
        format_pct(q(&r.index_rebuild, 95.0)),
        r.wal_final_bytes,
        r.rss_end_kb,
    );
    println!("[contention] {}", r.summary());
    std::mem::take(&mut *r)
}

/// Fast PR smoke: the harness mechanics (writers, readers, REAL index
/// rebuilds, WAL sampling, per-job receipt telemetry) at a tiny scale — no
/// daemon, no SSE, no bounds.
#[test]
fn contention_harness_smoke() {
    let report = run_contention(4, 4, false);
    assert_eq!(
        report.turns, 16,
        "the `turns` argument controls the workload"
    );
    assert_eq!(report.turns_per_session, 4);
    assert_eq!(report.events_appended, 16);
    assert!(!report.tool.is_empty() && !report.budget.is_empty());
    assert!(
        report.index_generations >= INDEX_THREADS as u64,
        "every index worker must publish at least one real generation: {}",
        report.index_generations
    );
    assert!(
        !report.index_rebuild.is_empty(),
        "real index rebuilds must run"
    );
    assert!(
        !report.messages_page.is_empty(),
        "readers must page messages"
    );
    assert!(
        !report.reader_pool_wait.is_empty(),
        "reader-pool permit wait must be instrumented"
    );
    assert!(
        !report.writer_queue_wait.is_empty() && !report.writer_exec.is_empty(),
        "per-job writer receipts must be recorded"
    );
    assert!(
        report
            .writer_receipts_by_label
            .keys()
            .any(|label| label.starts_with("index_state")),
        "index state transitions must go through the store writer: {:?}",
        report.writer_receipts_by_label
    );
    assert_eq!(report.writer_receipts_dropped, 0);
    assert_eq!(report.reader_receipts_dropped, 0);
    assert_eq!(report.writer_refusals, 0);
}

/// The reproducibility anchor: two smoke runs of the same fixed workload
/// have the same SHAPE (counts identical; timings are machine dependent).
#[test]
fn contention_workload_is_reproducible() {
    let a = run_contention(3, 3, false);
    let b = run_contention(3, 3, false);
    assert_eq!(a.turns, 9);
    assert_eq!(a.turns, b.turns);
    assert_eq!(a.turns_per_session, b.turns_per_session);
    assert_eq!(a.events_appended, b.events_appended);
    assert_eq!(a.tool.len(), b.tool.len());
    assert_eq!(a.budget.len(), b.budget.len());
}

/// Audit 27 (+audit 5): 32 concurrent sessions with child agents, SSE
/// readers, REAL on-disk IndexService rebuilds, tool records and budget
/// writes; per-job writer receipts + reader permit waits as separate axes;
/// p50/p95/p99 + WAL + memory recorded; GENEROUS regression bounds
/// (tail-latency teeth only, so shared CI machines cannot flake on
/// machine-class spread).
#[test]
#[ignore = "[perf] multi-agent contention (32 sessions) — run explicitly"]
fn perf_multi_agent_contention_32_sessions() {
    let report = run_contention(SESSIONS, TURNS_PER_SESSION, true);

    // The full workload must have exercised every axis.
    assert!(
        report.turns as usize == SESSIONS * TURNS_PER_SESSION,
        "every session must finish its turns: {}",
        report.turns
    );
    assert!(report.events_appended >= report.turns);
    assert!(
        !report.sse_lag.is_empty(),
        "SSE readers must observe frames"
    );
    assert!(!report.event_lag.is_empty());
    assert!(!report.index_rebuild.is_empty());
    assert!(
        report.index_generations >= INDEX_THREADS as u64,
        "both index workers must publish measured generations"
    );
    assert!(report.messages_page.len() > report.turns as usize / 4);
    assert!(!report.reader_pool_wait.is_empty());
    assert!(!report.writer_queue_wait.is_empty() && !report.writer_exec.is_empty());
    assert_eq!(report.writer_receipts_dropped, 0, "receipt ring overflowed");
    assert_eq!(report.reader_receipts_dropped, 0, "receipt ring overflowed");

    // Generous regression bounds (all on the slower shared CI class; see the
    // per-axis rationale below). Each is >= 10x the locally observed p95/p99
    // (measured 2026-09-27, arm64 release, shared host under load: turn 45 ms
    // p95, writer queue 9.6 ms p95, writer exec 7.7 ms p95, messages page
    // 41 ms p95, reader permit 0.4 ms p95, event lag 386 ms p99, SSE lag
    // 272 ms p99, index rebuild 163 ms p95).
    let bounds: &[(&str, f64, f64)] = &[
        // axis, observed p95 (or p99 where the axis is a lag), bound_ns
        ("turn_bookkeeping", report.turn.pct(95.0), 2_000_000_000.0),
        (
            "writer_queue_wait",
            report.writer_queue_wait.pct(95.0),
            5_000_000_000.0,
        ),
        ("writer_exec", report.writer_exec.pct(95.0), 5_000_000_000.0),
        (
            "messages_page",
            report.messages_page.pct(95.0),
            3_000_000_000.0,
        ),
        (
            "reader_pool_wait",
            report.reader_pool_wait.pct(95.0),
            3_000_000_000.0,
        ),
        (
            "event_stream_lag",
            report.event_lag.pct(99.0),
            15_000_000_000.0,
        ),
        ("sse_frame_lag", report.sse_lag.pct(99.0), 20_000_000_000.0),
        (
            "index_rebuild",
            report.index_rebuild.pct(95.0),
            10_000_000_000.0,
        ),
    ];
    for (axis, observed, bound) in bounds {
        assert!(
            *observed < *bound,
            "contention {axis}: p95/p99 {} exceeds the generous bound {}",
            format_pct(*observed),
            format_pct(*bound)
        );
    }
    // Bounded resources: WAL never balloons, memory stays sane, the writer
    // queue never refuses under this fixed load.
    assert!(
        report.wal_final_bytes < 512 * 1024 * 1024,
        "WAL grew to {} bytes",
        report.wal_final_bytes
    );
    assert!(
        report.rss_end_kb < report.rss_start_kb.saturating_add(4 * 1024 * 1024),
        "RSS grew by more than 4 GiB: {} -> {} KB",
        report.rss_start_kb,
        report.rss_end_kb
    );
    assert_eq!(
        report.writer_refusals, 0,
        "the writer queue refused work under the fixed contention load"
    );
}
