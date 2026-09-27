//! Reproducible multi-agent contention benchmark (audit 27).
//!
//! Fixed-shape workload: 32 concurrent sessions, each with a child session
//! (orchestrator identity + budget scope), driving concurrent turn
//! bookkeeping (message + part + journal event), tool-run records, durable
//! budget-cap writes; direct journal readers; real SSE readers against the
//! daemon's `/native/session/{id}/events` stream; index rebuild threads; and
//! a WAL sampler. The harness records p50/p95/p99 of every latency series,
//! WAL peak/final size and RSS, then asserts GENEROUS regression bounds.
//!
//! It is `#[ignore]`d on purpose so normal PR runs stay fast: the trusted
//! `perf` lane runs `cargo test -p faktor-tests-performance --release --
//! --ignored`, and the nightly `contention` lane runs this file directly.
//!
//! Reproducibility: no randomness, no wall-clock-dependent iteration counts
//! (all loops are bounded by the fixed workload); the report carries the
//! commit and profile metadata. Timing values are machine dependent by
//! nature; the BOUNDS are deliberately loose (tail-latency teeth only).

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use faktor_core::event::EventKind;
use faktor_core::id::{EventSeq, OpId, SessionId, TaskId, WorktreeId};
use faktor_core::state::AgentState;
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

struct Ctl {
    stop: AtomicBool,
    append_times: Mutex<HashMap<u64, Instant>>,
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

    fn note_append(&self, seq: u64, at: Instant) {
        self.append_times
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(seq, at);
        self.events_appended.fetch_add(1, Ordering::Relaxed);
    }

    fn lag_since(&self, seq: u64, floor: Instant, now: Instant) -> Option<Duration> {
        let map = self.append_times.lock().unwrap_or_else(|p| p.into_inner());
        let at = *map.get(&seq)?;
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
    writer_wait: Dist,
    reader_wait: Dist,
    event_lag: Dist,
    sse_lag: Dist,
    index_rebuild: Dist,
    wal_peak_bytes: u64,
    wal_final_bytes: u64,
    rss_start_kb: u64,
    rss_end_kb: u64,
    turns: u64,
    events_appended: u64,
    writer_jobs: u64,
    writer_queue_wait_total_ns: u64,
    writer_queue_wait_max_ns: u64,
    writer_pending_max: usize,
    writer_refusals: u64,
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
            "schema": "faktor-contention-bench/v1",
            "workload": {
                "sessions": SESSIONS,
                "child_agents": SESSIONS,
                "turns_per_session": TURNS_PER_SESSION,
                "reader_threads": READER_THREADS,
                "index_threads": INDEX_THREADS,
                "index_files": INDEX_FILES,
                "sse_connections": SSE_CONNECTIONS,
            },
            "turns": self.turns,
            "events_appended": self.events_appended,
            "latency_ms": {
                "turn_bookkeeping": d(&self.turn),
                "tool_record": d(&self.tool),
                "budget_write": d(&self.budget),
                "db_writer_wait": d(&self.writer_wait),
                "reader_pool_wait": d(&self.reader_wait),
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
                "jobs": self.writer_jobs,
                "queue_wait_total_ns": self.writer_queue_wait_total_ns,
                "queue_wait_max_ns": self.writer_queue_wait_max_ns,
                "pending_max": self.writer_pending_max,
                "refusals": self.writer_refusals,
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

fn writer_wait_delta(
    before: faktor_store::WriterTelemetry,
    after: faktor_store::WriterTelemetry,
) -> Option<u64> {
    let jobs = after.jobs.saturating_sub(before.jobs);
    if jobs == 0 {
        return None;
    }
    let wait = after
        .queue_wait_total_ns
        .saturating_sub(before.queue_wait_total_ns);
    Some(wait / jobs)
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
) {
    let sid = handle.id();
    let base_seq = 0i64;
    let started = Instant::now();
    for turn in 0..TURNS_PER_SESSION {
        if started.elapsed() > HARD_CAP {
            ctl.fail(format!("writer {label}: hard cap exceeded"));
            break;
        }
        let seq = base_seq + turn as i64 + 1;
        // ---- turn bookkeeping: message + part + journal event.
        let before = store.writer_telemetry();
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
        ctl.note_append(event_seq.raw(), appended_at);
        let turn_ns = t0.elapsed().as_nanos() as u64;
        let wait = writer_wait_delta(before, store.writer_telemetry());
        {
            let mut r = report.lock().unwrap_or_else(|p| p.into_inner());
            r.turn.push(turn_ns);
            r.turns += 1;
            if let Some(wait) = wait {
                r.writer_wait.push(wait);
            }
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
    }
}

fn direct_reader_thread(
    label: usize,
    sessions: Vec<faktor_session::SessionHandle>,
    ctl: Arc<Ctl>,
    report: Arc<Mutex<Report>>,
) {
    let started = Instant::now();
    let mut last: HashMap<SessionId, u64> = HashMap::new();
    while !ctl.stop.load(Ordering::Relaxed) {
        if started.elapsed() > HARD_CAP {
            ctl.fail(format!("reader {label}: hard cap exceeded"));
            return;
        }
        for handle in &sessions {
            let t0 = Instant::now();
            let page = handle.messages_page(None, 20);
            let read_ns = t0.elapsed().as_nanos() as u64;
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
                        if let Some(lag) = ctl.lag_since(event.seq.raw(), started, now) {
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
            let mut r = report.lock().unwrap_or_else(|p| p.into_inner());
            r.reader_wait.push(read_ns);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn index_rebuild_thread(
    label: usize,
    ws: faktor_core::id::WorkspaceId,
    ctl: Arc<Ctl>,
    report: Arc<Mutex<Report>>,
) {
    let started = Instant::now();
    let mut generation = 1i64;
    while !ctl.stop.load(Ordering::Relaxed) {
        if started.elapsed() > HARD_CAP {
            ctl.fail(format!("index {label}: hard cap exceeded"));
            return;
        }
        let t0 = Instant::now();
        let mut index = faktor_index::WorkspaceIndex::new();
        for file in 0..INDEX_FILES {
            let rel = format!("bench/f{file}.rs");
            let src = format!("pub fn alpha_{file}() -> i64 {{ {file} }}\n");
            if let Err(e) = index.index_file(ws, Path::new(&rel), src.as_bytes(), generation) {
                ctl.fail(format!("index {label}: index_file: {e}"));
                return;
            }
        }
        if index.symbol_lookup(ws, "alpha_1", 10).is_empty() {
            ctl.fail(format!("index {label}: rebuild lost its symbols"));
            return;
        }
        report
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .index_rebuild
            .push(t0.elapsed().as_nanos() as u64);
        generation += 1;
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
/// records the lag from journal append to observed frame.
async fn sse_reader_task(
    addr: std::net::SocketAddr,
    token: String,
    session_id: String,
    ctl: Arc<Ctl>,
    report: Arc<Mutex<Report>>,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
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
                        if let Some(lag) = ctl.lag_since(seq, started, now) {
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
    for i in 0..scale_sessions {
        let ws = manager
            .create_workspace(&format!("/bench-{i}"))
            .expect("workspace");
        workspaces.push(ws);
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
                root.id().to_string(),
                ctl.clone(),
                report.clone(),
            )));
        }
        runtime = Some(rt);
        server = Some(handle);
    }

    // Index rebuilders.
    let mut index_handles = Vec::new();
    for i in 0..INDEX_THREADS {
        let ws = workspaces[i % workspaces.len()];
        let ctl = ctl.clone();
        let report = report.clone();
        index_handles.push(std::thread::spawn(move || {
            index_rebuild_thread(i, ws, ctl, report)
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
        reader_handles.push(std::thread::spawn(move || {
            direct_reader_thread(r, own, ctl, report)
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
            writer_thread(i, handle, child_session, budgets, store, ctl, report)
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

    let telemetry = store.writer_telemetry();
    let mut r = report.lock().unwrap_or_else(|p| p.into_inner());
    r.wal_peak_bytes = wal_peak.load(Ordering::Relaxed);
    r.wal_final_bytes = wal_bytes(&root_path);
    r.rss_start_kb = rss_start;
    r.rss_end_kb = rss_kb();
    r.events_appended = ctl.events_appended.load(Ordering::Relaxed);
    r.writer_jobs = telemetry.jobs;
    r.writer_queue_wait_total_ns = telemetry.queue_wait_total_ns;
    r.writer_queue_wait_max_ns = telemetry.queue_wait_max_ns;
    r.writer_pending_max = telemetry.pending_max;
    r.writer_refusals = telemetry.queue_full_refusals;
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
        "[contention] workload={}sessions/{turns}turns elapsed={:.2}s \
         turn_p50={} turn_p95={} writer_wait_p95={} reader_wait_p95={} \
         event_lag_p99={} sse_lag_p99={} wal_final={} bytes rss={}KB",
        scale_sessions,
        elapsed.as_secs_f64(),
        format_pct(q(&r.turn, 50.0)),
        format_pct(q(&r.turn, 95.0)),
        format_pct(q(&r.writer_wait, 95.0)),
        format_pct(q(&r.reader_wait, 95.0)),
        format_pct(q(&r.event_lag, 99.0)),
        format_pct(q(&r.sse_lag, 99.0)),
        r.wal_final_bytes,
        r.rss_end_kb,
    );
    println!("[contention] {}", r.summary());
    std::mem::take(&mut *r)
}

/// Fast PR smoke: the harness mechanics (writers, readers, index rebuilds,
/// WAL sampling, telemetry) at a tiny scale — no daemon, no SSE, no bounds.
#[test]
fn contention_harness_smoke() {
    let report = run_contention(4, 4, false);
    assert!(report.turns >= 16, "smoke must run the fixed turns");
    assert!(report.events_appended >= 16);
    assert!(!report.tool.is_empty() && !report.budget.is_empty());
    assert!(!report.index_rebuild.is_empty(), "index rebuilds must run");
}

/// The reproducibility anchor: two smoke runs of the same fixed workload
/// have the same SHAPE (counts identical; timings are machine dependent).
#[test]
fn contention_workload_is_reproducible() {
    let a = run_contention(3, 3, false);
    let b = run_contention(3, 3, false);
    assert_eq!(a.turns, b.turns);
    assert_eq!(a.events_appended, b.events_appended);
    assert_eq!(a.tool.len(), b.tool.len());
    assert_eq!(a.budget.len(), b.budget.len());
}

/// Audit 27: 32 concurrent sessions with child agents, SSE readers, index
/// rebuilds, tool records and budget writes; p50/p95/p99 + WAL + memory
/// recorded; GENEROUS regression bounds (tail-latency teeth only, so shared
/// CI machines cannot flake on machine-class spread).
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
    assert!(report.reader_wait.len() > report.turns as usize / 4);

    // Generous regression bounds (all on the slower shared CI class; see the
    // per-axis rationale below). Each is >= 10x the locally observed p95.
    let bounds: &[(&str, f64, f64)] = &[
        // axis, observed_p95_ns (arm64 release local), bound_ns (30x-50x)
        ("turn_bookkeeping", report.turn.pct(95.0), 2_000_000_000.0),
        (
            "db_writer_wait",
            report.writer_wait.pct(95.0),
            5_000_000_000.0,
        ),
        (
            "reader_pool_wait",
            report.reader_wait.pct(95.0),
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
            5_000_000_000.0,
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
