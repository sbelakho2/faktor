// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! `connection`: cohesive slice of the mechanically decomposed parent module.

use super::*;
use std::collections::VecDeque;
use std::sync::atomic::AtomicBool;

/// Max concurrent read connections. This is a concurrency limit (semaphore
/// permits), not just a retention limit.
pub(crate) const READER_POOL: usize = 4;

/// Hard bound on the retained per-read permit-wait receipt ring (mirrors the
/// writer's receipt bound; evictions are counted, never silent).
pub(crate) const READER_RECEIPT_CAPACITY: usize = 4096;

/// Hard bound on one `memory_fact` kind scan (`doctor --deep` orphan
/// checks): beyond this the scan refuses loudly instead of truncating.
pub(crate) const MAX_ORCHESTRATOR_FACT_SCAN_ROWS: i64 = 250_000;

/// How long `read()` waits for a permit before failing with `Busy`. The
/// engine-level writer busy timeout is deliberately LONGER (30s): a second
/// process settling the same data dir may hold the write lock through its
/// own durable receipt, and a transient lock must not fail a turn.
pub(crate) const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// On-disk WAL high-water bound (mirrors `PRAGMA journal_size_limit` in
/// [`configure`]). A PASSIVE checkpoint folds frames but never shrinks the
/// file; once the sidecar exceeds this bound the idle maintenance tick runs
/// a NON-BLOCKING truncate checkpoint.
pub const WAL_TRUNCATE_THRESHOLD_BYTES: u64 = 64 * 1024 * 1024;

fn is_locked_error(e: &rusqlite::Error) -> bool {
    matches!(
        e.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy) | Some(rusqlite::ErrorCode::DatabaseLocked)
    )
}

/// A counting semaphore on stable std (there is no `std::sync::Semaphore` on
/// stable Rust as of 1.98): `acquire_timeout` blocks on a condvar until a
/// permit frees or the deadline passes. Readers hold a `Permit` for the
/// whole borrow, which is what caps live read connections at `READER_POOL`.
#[derive(Debug)]
pub(crate) struct Semaphore {
    pub(crate) permits: Mutex<usize>,
    pub(crate) available: Condvar,
}

/// RAII permit; releases one permit and wakes one waiter on drop.
#[derive(Debug)]
pub(crate) struct Permit(pub(crate) Arc<Semaphore>);

impl Drop for Permit {
    fn drop(&mut self) {
        let mut p = self.0.permits.lock().unwrap_or_else(|e| e.into_inner());
        *p += 1;
        self.0.available.notify_one();
    }
}

/// RAII connection-level durability lift: sets `PRAGMA synchronous = FULL`
/// for the duration of the guard so a grouped actor batch's single COMMIT
/// fsyncs the WAL before any caller ack, then restores the crate's configured
/// `NORMAL` on drop (also on panic/error paths). Connection-scoped: the
/// whole batch is ONE writer-service command, so no other writer observes
/// the lifted mode.
pub(crate) struct StrongSync<'a> {
    pub(crate) conn: &'a Connection,
}

impl<'a> StrongSync<'a> {
    pub(crate) fn on(conn: &'a Connection) -> StoreResult<Self> {
        conn.execute_batch("PRAGMA synchronous = FULL")?;
        Ok(Self { conn })
    }
}

impl Drop for StrongSync<'_> {
    fn drop(&mut self) {
        let _ = self.conn.execute_batch("PRAGMA synchronous = NORMAL");
    }
}

impl Semaphore {
    pub(crate) fn new(permits: usize) -> Self {
        Self {
            permits: Mutex::new(permits),
            available: Condvar::new(),
        }
    }

    /// Block until a permit is free or `deadline` passes (`Busy`).
    ///
    /// Poisoning policy: the permit counter is ephemeral in-process
    /// bookkeeping, so a panic in one holder recovers the inner count
    /// instead of disabling every reader.
    pub(crate) fn acquire_timeout(self: &Arc<Self>, deadline: Instant) -> StoreResult<Permit> {
        let mut p = self.permits.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if *p > 0 {
                *p -= 1;
                return Ok(Permit(Arc::clone(self)));
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(StoreError::Busy(format!(
                    "no reader permit within {}ms ({} readers already active)",
                    BUSY_TIMEOUT.as_millis(),
                    READER_POOL
                )));
            }
            let (guard, _) = self
                .available
                .wait_timeout(p, deadline - now)
                .unwrap_or_else(|p| p.into_inner());
            p = guard;
        }
    }
}

/// Bounded pool of idle read connections + the semaphore that caps how many
/// readers may borrow one at a time.
#[derive(Debug)]
pub(crate) struct ReaderPool {
    pub(crate) conns: Mutex<Vec<Connection>>,
    pub(crate) sem: Arc<Semaphore>,
    /// Connections ever opened (doctor/test probe: proves the cap held).
    pub(crate) created: AtomicU64,
    /// Read calls that acquired a permit (monotonic).
    pub(crate) reads: AtomicU64,
    /// Cumulative + peak time callers waited for a reader-pool permit. This
    /// is deliberately NOT the caller's read duration: `read()` only ever
    /// blocks on the permit, while `messages_page()` also pays query, JSON
    /// and part-load time that this pool never sees.
    pub(crate) permit_wait_total_ns: AtomicU64,
    pub(crate) permit_wait_max_ns: AtomicU64,
    /// Bounded per-read permit-wait receipts (ns), oldest first.
    pub(crate) receipts: Mutex<VecDeque<u64>>,
    pub(crate) receipts_dropped: AtomicU64,
}

impl ReaderPool {
    pub(crate) fn new() -> Self {
        Self {
            conns: Mutex::new(Vec::with_capacity(READER_POOL)),
            sem: Arc::new(Semaphore::new(READER_POOL)),
            created: AtomicU64::new(0),
            reads: AtomicU64::new(0),
            permit_wait_total_ns: AtomicU64::new(0),
            permit_wait_max_ns: AtomicU64::new(0),
            receipts: Mutex::new(VecDeque::new()),
            receipts_dropped: AtomicU64::new(0),
        }
    }

    /// Record one completed permit acquisition (audit-5 read instrumentation).
    pub(crate) fn record_read(&self, wait: Duration) {
        let ns = wait.as_nanos().min(u64::MAX as u128) as u64;
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.permit_wait_total_ns.fetch_add(ns, Ordering::Relaxed);
        self.permit_wait_max_ns.fetch_max(ns, Ordering::Relaxed);
        // Ephemeral diagnostic ring: recover on poison (see the crate's
        // poisoning policy) and evict the oldest receipt at the bound.
        let mut ring = self.receipts.lock().unwrap_or_else(|p| p.into_inner());
        if ring.len() >= READER_RECEIPT_CAPACITY {
            ring.pop_front();
            self.receipts_dropped.fetch_add(1, Ordering::Relaxed);
        }
        ring.push_back(ns);
    }
}

/// One completed read's permit-wait receipt: how long `Store::read` blocked
/// on the reader-pool semaphore before borrowing a connection. End-to-end
/// call latency (`messages_page()`) is a separate measurement on purpose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReaderPermitReceipt {
    pub permit_wait_ns: u64,
}

/// Typed cumulative read-path instrumentation (audit-5): reads completed,
/// permit wait total/max and receipt-ring drops. The full caller duration is
/// not tracked here — measure around your own call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReaderTelemetry {
    pub reads: u64,
    pub permit_wait_total_ns: u64,
    pub permit_wait_max_ns: u64,
    pub receipts_dropped: u64,
}

/// A borrowed read connection; returned to the pool on drop. The semaphore
/// permit is held for the borrow's lifetime, which is what bounds concurrent
/// readers at `READER_POOL`.
pub struct ReadConn {
    pub(crate) conn: Option<Connection>,
    pub(crate) pool: Arc<ReaderPool>,
    pub(crate) _permit: Permit,
}

impl ReadConn {
    pub fn get(&self) -> &Connection {
        // In-process invariant: a ReadConn is handed out exactly once and its
        // connection is only `take`n by Drop, so a live ReadConn always has
        // its connection. Never reachable from DB state.
        self.conn.as_ref().expect("read conn already returned")
    }
}

impl std::ops::Deref for ReadConn {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        self.get()
    }
}

impl Drop for ReadConn {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            // Ephemeral connection cache: a panic in another holder must not
            // wedge every later reader; recover the pooled vector.
            let mut conns = self.pool.conns.lock().unwrap_or_else(|p| p.into_inner());
            if conns.len() < READER_POOL {
                conns.push(conn);
                // The semaphore permit is released right after this method,
                // via the `_permit` field's drop.
                return;
            }
            drop(conns);
            drop(conn);
        }
    }
}

// ---------------------------------------------------------------------------
// Deterministic crash seam (fault-certification campaigns only)
//
// One-shot fault injection at named DURABILITY BOUNDARIES: the instant a
// group/transaction crosses the boundary (its COMMIT executed) the state is
// durable; before the boundary the whole in-flight operation is rolled back
// by SQLite on the next open — exactly like a process death at that point
// (verified: dropping a rusqlite connection that holds an open transaction
// rolls the transaction back and the file reopens cleanly). The seam is
// inert unless armed, and the panic fires at most once per arm.
// ---------------------------------------------------------------------------

/// One-shot fault-injection target of [`CrashSeam`]: crash at the
/// `ordinal`-th crossing (0-based) of durability boundary `point`.
///
/// Compiled only for the crate's own tests or with the `fault-injection`
/// feature (fault campaigns); the production profile has no arming API.
#[cfg(any(test, feature = "fault-injection"))]
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CrashArm {
    pub point: &'static str,
    pub ordinal: u64,
}

#[cfg(any(test, feature = "fault-injection"))]
#[derive(Default)]
pub(crate) struct SeamState {
    pub(crate) armed: Option<CrashArm>,
    /// Crossings of the ARMED point observed so far.
    pub(crate) crossings: u64,
}

/// Per-store-instance deterministic crash seam. Additive and default-off:
/// while unarmed every `trip` is a single uncontended mutex check and no
/// behavior or format changes. The fault campaigns arm exactly one
/// boundary per interrupted run, panic the store mid-operation, drop the
/// instance (the "process death") and reopen from disk.
#[cfg(any(test, feature = "fault-injection"))]
#[doc(hidden)]
pub struct CrashSeam {
    pub(crate) state: Mutex<SeamState>,
}

/// Production profile: the seam payload is compiled out; the no-op keeps
/// the surrounding plumbing (a `Store` field, `trip` call sites) uniform at
/// zero runtime cost.
#[cfg(not(any(test, feature = "fault-injection")))]
#[derive(Debug, Default)]
pub(crate) struct CrashSeam {
    _compiled_out: (),
}

#[cfg(any(test, feature = "fault-injection"))]
impl std::fmt::Debug for CrashSeam {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Ephemeral seam bookkeeping: a panic in one holder must not disable
        // the seam (or Debug), so recover the inner state.
        let s = self.state.lock().unwrap_or_else(|p| p.into_inner()).armed;
        f.debug_struct("CrashSeam").field("armed", &s).finish()
    }
}

#[cfg(any(test, feature = "fault-injection"))]
impl Default for CrashSeam {
    fn default() -> Self {
        Self {
            state: Mutex::new(SeamState::default()),
        }
    }
}

#[cfg(any(test, feature = "fault-injection"))]
impl CrashSeam {
    /// Arm ONE crossing, replacing any previous arm and resetting the
    /// crossing counter. The panic fires exactly once when `point` is
    /// crossed for the `ordinal`-th time.
    pub fn arm(&self, arm: CrashArm) {
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        *s = SeamState {
            armed: Some(arm),
            crossings: 0,
        };
    }

    /// Trip the seam at `point`. Panics when the armed crossing is hit.
    pub(crate) fn trip(&self, point: &'static str) {
        let mut s = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let Some(arm) = s.armed else {
            return;
        };
        if arm.point != point {
            return;
        }
        s.crossings += 1;
        if s.crossings - 1 != arm.ordinal {
            return;
        }
        s.armed = None;
        drop(s);
        panic!(
            "[fault-seam] simulated crash at store durability boundary `{point}` (crossing {})",
            arm.ordinal
        );
    }
}

#[cfg(not(any(test, feature = "fault-injection")))]
impl CrashSeam {
    #[inline(always)]
    pub(crate) fn trip(&self, _point: &'static str) {}
}

/// One active maintenance task: its cancellation token plus the SQLite
/// interrupt handle of its dedicated connection. `cancel` alone is enough for
/// the page-batched backup loop (SQLite's backup API does not poll
/// `sqlite3_interrupt`); the handle is delivered too so any VM work on the
/// maintenance connection aborts immediately.
struct ActiveMaintenance {
    id: u64,
    cancel: Arc<AtomicBool>,
    interrupt: rusqlite::InterruptHandle,
}

/// Bounded registry of active maintenance tasks (audit item 6/7): the store
/// keeps weak bookkeeping — id, cancellation token, interrupt handle — for
/// every online backup running on its own snapshot connection. Shutdown
/// cancels every task and waits (bounded) for the registry to drain; a task
/// that misses its bound is not joined (its caller still receives the typed
/// cancellation), so shutdown can never hang on maintenance.
#[derive(Default)]
pub(crate) struct MaintenanceRegistry {
    active: Mutex<Vec<ActiveMaintenance>>,
    next_id: AtomicU64,
    idle: Condvar,
    /// Closed once a shutdown/drop has begun: a maintenance task that tries to
    /// register after cancellation would otherwise escape the cancel+wait and
    /// let `Store::shutdown` report success with a live store-owned thread
    /// (the exact authority leak the success condition forbids).
    closed: AtomicBool,
}

impl std::fmt::Debug for MaintenanceRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MaintenanceRegistry")
            .field(
                "active",
                &self.active.lock().unwrap_or_else(|p| p.into_inner()).len(),
            )
            .finish_non_exhaustive()
    }
}

impl MaintenanceRegistry {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Register one task; the returned id deregisters it when the task ends.
    /// `None` once the registry is closed for shutdown: a late registrant must
    /// never escape cancellation, so the caller refuses typed instead.
    ///
    /// The closed check and the push happen under the `active` lock, and
    /// `close()` takes the same lock, so a registration racing a shutdown
    /// either lands before `close()` (and is therefore visible to
    /// `cancel_all`) or observes the closed flag and is refused.
    fn register(
        &self,
        cancel: Arc<AtomicBool>,
        interrupt: rusqlite::InterruptHandle,
    ) -> Option<u64> {
        let mut active = self.active.lock().unwrap_or_else(|p| p.into_inner());
        if self.closed.load(Ordering::Acquire) {
            return None;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        active.push(ActiveMaintenance {
            id,
            cancel,
            interrupt,
        });
        Some(id)
    }

    /// Close the registry to new registrations (shutdown/drop). Idempotent;
    /// must be called BEFORE [`Self::cancel_all`] so a registration cannot slip
    /// in after the cancel sweep.
    pub(crate) fn close(&self) {
        let _active = self.active.lock().unwrap_or_else(|p| p.into_inner());
        self.closed.store(true, Ordering::Release);
    }

    fn unregister(&self, id: u64) {
        self.active
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|task| task.id != id);
        self.idle.notify_all();
    }

    /// Signal cancellation to every task (flag + SQLite interrupt) and report
    /// how many were active. Idempotent: already-cancelled tasks overwrite the
    /// flag.
    pub(crate) fn cancel_all(&self) -> usize {
        let active = self.active.lock().unwrap_or_else(|p| p.into_inner());
        for task in active.iter() {
            task.cancel.store(true, Ordering::Release);
            task.interrupt.interrupt();
        }
        active.len()
    }

    /// Number of active maintenance tasks (tests/doctor probe).
    pub(crate) fn active_count(&self) -> usize {
        self.active.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    /// Wait, bounded by `deadline`, until every registered task deregisters.
    /// `false` means at least one task is still winding down — the caller
    /// proceeds (its backup was cancelled and fails typed) without joining it.
    pub(crate) fn wait_idle(&self, deadline: Instant) -> bool {
        let mut active = self.active.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if active.is_empty() {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let (guard, _) = self
                .idle
                .wait_timeout(active, deadline - now)
                .unwrap_or_else(|p| p.into_inner());
            active = guard;
        }
    }
}

/// The daemon's durable store. Mutations are prepared on the caller's thread
/// and submitted to the single-owner [`WriterService`] (bounded queue, one
/// SQL transaction at a time); `read` borrows a connection from a small pool
/// (SQLite WAL allows concurrent readers). All mutations happen inside
/// explicit transactions.
#[derive(Debug)]
pub struct Store {
    pub(crate) root: PathBuf,
    pub(crate) writer: WriterService,
    pub(crate) pool: Arc<ReaderPool>,
    pub(crate) seam: Arc<CrashSeam>,
    /// Boot-instance identity of THIS store open (audit P1): a fresh opaque
    /// token minted by every `Store::open`/`open_fast`. Admission claims are
    /// stamped with it so a `pending` row written by a previous daemon boot
    /// is distinguishable from a live claim of this one; startup recovery
    /// resolves every foreign-generation pending row.
    pub(crate) generation: String,
    /// Long-running maintenance tasks (online backups, audit item 6) on their
    /// own dedicated connections/threads — NEVER the mutation owner. The
    /// registry carries each task's cancellation token so a store shutdown
    /// can cancel it and wait (bounded) for it to stop.
    pub(crate) maintenance: Arc<MaintenanceRegistry>,
    /// Last `updated_ms` issued to a memory-fact row. Fact order is the
    /// paging contract ("an upsert only moves a row toward the NEWEST
    /// end"), so fact stamps are MONOTONIC: two writes inside the same
    /// wall-clock millisecond must still order strictly, otherwise a new
    /// row can tie the cursor's millisecond and sort BELOW an ongoing walk
    /// (kind/key tie-breaks can put it on the already-consumed side).
    pub(crate) fact_seq: AtomicU64,
    /// Monotonic epoch of DIRECT-SQL / purge mutations (the test/forensic
    /// escape hatches that bypass the typed writers). Session-open
    /// verification is cached per session; a direct mutation can invalidate
    /// any journal/ledger invariant, so it bumps this epoch and the next
    /// session open re-verifies from scratch. Typed writes (journal append,
    /// ledger append, checkpoint, message) maintain the invariants by
    /// construction and never bump it, so the cache survives steady-state
    /// concurrency.
    pub(crate) corruption_epoch: AtomicU64,
    /// Monotonic revision of the Engineering Outcome Ledger (engineering
    /// rows + settlement events). Read-side metric folds cache by this
    /// value; it advances only when a writer commits a row/event.
    pub(crate) outcome_revision: AtomicU64,
    /// Always-on storage governor (discovered beside the data dir), used for
    /// WAL maintenance and growth refusal. `None` in bare test stores.
    storage_governor: std::sync::Mutex<Option<Arc<faktor_storage::StorageGovernor>>>,
}

impl Drop for Store {
    fn drop(&mut self) {
        // Cancel every maintenance task non-blockingly (audit items 6/7):
        // each backup observes the token between page batches and fails its
        // caller typed. The writer's own `Drop` is the bounded stop (drain →
        // interrupt → reject → detach); it never joins unboundedly.
        self.maintenance.close();
        let cancelled = self.maintenance.cancel_all();
        if cancelled > 0 {
            tracing::info!(
                target: "faktor_store::maintenance",
                cancelled,
                "cancelling maintenance tasks on store drop"
            );
        }
    }
}

/// One grouped hot write (the `faktor-session` `DbActor` request surface).
/// The four fixed write shapes the daemon issues per message / per part / per
/// journal event / per usage-settlement frame — never free-form SQL.
#[derive(Debug, Clone)]
pub enum HotWrite {
    AppendEvent {
        session_id: SessionId,
        op_id: Option<OpId>,
        kind: EventKind,
        state: AgentState,
        ts_ms: i64,
        payload: Option<serde_json::Value>,
        /// Payload schema version (v11+; session writers stamp
        /// `PAYLOAD_SCHEMA_V`). Readers refuse unknown versions loudly.
        payload_ver: i64,
    },
    PutMessage {
        session_id: SessionId,
        seq: i64,
        role: String,
        data: serde_json::Value,
    },
    PutPart {
        message_id: i64,
        kind: String,
        data: serde_json::Value,
    },
    RecordProviderCall {
        session_id: SessionId,
        op_id: OpId,
        provider: String,
        model: String,
        status: String,
        tokens_in: Option<u64>,
        tokens_out: Option<u64>,
        error: Option<String>,
    },
}

/// [`HotWrite`] with every JSON body PRE-SERIALIZED and every row timestamp
/// PRE-CAPTURED before enqueueing (audit items 7/8). [`Store::batch_hot_writes`]
/// builds these on the caller's thread; the writer owner then executes
/// statements only — it never touches `serde_json` or a clock on the hot path.
pub(crate) enum PreparedHotWrite {
    AppendEvent {
        session_id: SessionId,
        op_id: Option<OpId>,
        kind: EventKind,
        state_json: String,
        ts_ms: i64,
        payload_json: Option<String>,
        payload_ver: i64,
        seq_messages: EventSeqMessages,
    },
    PutMessage {
        session_id: SessionId,
        seq: i64,
        role: String,
        data_json: String,
        created_ms: i64,
    },
    PutPart {
        message_id: i64,
        kind: String,
        data_json: String,
        created_ms: i64,
    },
    RecordProviderCall {
        session_id: SessionId,
        op_id: OpId,
        provider: String,
        model: String,
        status: String,
        tokens_in: Option<u64>,
        tokens_out: Option<u64>,
        error: Option<String>,
        ts_ms: i64,
    },
}

impl PreparedHotWrite {
    /// Serialize one caller command's JSON bodies and capture its row
    /// timestamp. Runs on the caller's thread, before the command is
    /// enqueued.
    pub(crate) fn prepare(w: &HotWrite) -> Self {
        match w {
            HotWrite::AppendEvent {
                session_id,
                op_id,
                kind,
                state,
                ts_ms,
                payload,
                payload_ver,
            } => Self::AppendEvent {
                session_id: *session_id,
                op_id: *op_id,
                kind: *kind,
                state_json: serde_json::to_string(state).unwrap(),
                ts_ms: *ts_ms,
                payload_json: payload.as_ref().map(|p| p.to_string()),
                payload_ver: *payload_ver,
                seq_messages: EventSeqMessages::for_session(*session_id),
            },
            HotWrite::PutMessage {
                session_id,
                seq,
                role,
                data,
            } => Self::PutMessage {
                session_id: *session_id,
                seq: *seq,
                role: role.clone(),
                data_json: data.to_string(),
                created_ms: now_ms(),
            },
            HotWrite::PutPart {
                message_id,
                kind,
                data,
            } => Self::PutPart {
                message_id: *message_id,
                kind: kind.clone(),
                data_json: data.to_string(),
                created_ms: now_ms(),
            },
            HotWrite::RecordProviderCall {
                session_id,
                op_id,
                provider,
                model,
                status,
                tokens_in,
                tokens_out,
                error,
            } => Self::RecordProviderCall {
                session_id: *session_id,
                op_id: *op_id,
                provider: provider.clone(),
                model: model.clone(),
                status: status.clone(),
                tokens_in: *tokens_in,
                tokens_out: *tokens_out,
                error: error.clone(),
                ts_ms: now_ms(),
            },
        }
    }
}

/// Microsecond timing split of one [`Store::batch_hot_writes`] group.
///
/// `work_us` covers everything up to the commit statement (queue wait is
/// excluded: it is measured by the writer service itself and attributed to
/// the caller, not to SQLite work). `commit_us` covers the COMMIT statement,
/// which under the group's `synchronous = FULL` includes the deliberate WAL
/// fsync that makes the actor's ack mean "durable".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BatchTiming {
    /// SQL work (BEGIN + per-write savepoints), microseconds.
    pub work_us: u64,
    /// COMMIT + fsync, microseconds.
    pub commit_us: u64,
}

/// Per-write result of a [`Store::batch_hot_writes`] group.
#[derive(Debug, Clone)]
pub enum HotWriteOutcome {
    /// The journal event's gapless per-session sequence.
    EventSeq(EventSeq),
    /// The inserted row id (message / part / provider-call).
    RowId(i64),
}

pub(crate) fn configure(conn: &Connection) -> StoreResult<()> {
    // `wal_autocheckpoint = 0` disables SQLite's automatic checkpoint on
    // commit. Auto-checkpoint executes INSIDE the committing statement, so on
    // the actor's interactive batch path a WAL that grew past the default
    // ~1000-page watermark turned into multi-millisecond SQLite work inside a
    // measured segment (the recurring 5 ms gate flake). Checkpointing is
    // scheduled explicitly instead: the session `DbActor` runs a PASSIVE
    // checkpoint from its idle flush tick via [`Store::wal_checkpoint_passive`],
    // outside every measured interactive segment.
    //
    // `journal_size_limit` bounds the WAL FILE's on-disk high-water mark:
    // without it a long burst (100k durable rows) left a multi-gigabyte
    // `-wal` sidecar even after every frame was folded (PASSIVE does not
    // shrink the file). SQLite truncates back to this limit after a
    // checkpoint whenever no reader pins an older frame; a pinned reader
    // simply defers the truncation, never corrupts or drops frames.
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA busy_timeout = 30000;
         PRAGMA foreign_keys = ON;
         PRAGMA wal_autocheckpoint = 0;
         PRAGMA journal_size_limit = 67108864;",
    )?;
    Ok(())
}

/// Pages copied per `sqlite3_backup_step` on the maintenance thread. A small
/// batch bounds how long one step holds the source read lock and how quickly
/// a cancellation is observed (checked before every batch).
const BACKUP_PAGES_PER_STEP: std::ffi::c_int = 64;

/// Pause between backup page batches: yields the source lock so concurrent
/// writer commits proceed, and bounds cancellation latency between batches.
const BACKUP_STEP_PAUSE: Duration = Duration::from_millis(10);

/// The page-batched copy of one online backup, running on the maintenance
/// thread with its own dedicated source connection (audit item 6).
///
/// The copy pins ONE WAL read snapshot for its whole duration. Without an
/// explicit read transaction on the source, SQLite restarts the backup
/// whenever an external connection commits (its pager cache is reset while
/// the backup is attached), so under continuous writes a large copy could
/// never finish. A held read transaction gives the copy a stable source;
/// WAL readers never block the writer, so mutations keep committing at full
/// speed while the snapshot is copied. The cancellation token is checked
/// before every batch; a cancelled copy removes its partial destination so a
/// half-written file can never be mistaken for a complete backup.
fn run_backup(src: Connection, dest: &Path, cancel: &AtomicBool) -> StoreResult<()> {
    let mut dst = Connection::open(dest)?;
    let snapshot = src.unchecked_transaction()?;
    // Force the read transaction to actually start (a deferred transaction
    // alone does not pin a snapshot).
    let _: i64 = snapshot.query_row("SELECT COUNT(*) FROM sqlite_master", [], |r| r.get(0))?;
    let backup = rusqlite::backup::Backup::new(&snapshot, &mut dst)?;
    loop {
        if cancel.load(Ordering::Acquire) {
            drop(backup);
            drop(snapshot);
            drop(dst);
            let _ = std::fs::remove_file(dest);
            return Err(StoreError::Maintenance(
                "online backup cancelled by store shutdown".into(),
            ));
        }
        // Test-only per-step seam (compiled out of production): lets a test
        // hold the backup INSIDE its page loop as an observable condition
        // instead of racing a wall clock. The hook sees the destination
        // path so unrelated concurrent backups are never affected.
        #[cfg(test)]
        backup_step_hook::invoke(dest, cancel);
        match backup.step(BACKUP_PAGES_PER_STEP)? {
            rusqlite::backup::StepResult::Done => {
                drop(backup);
                snapshot.rollback()?;
                return Ok(());
            }
            _ => std::thread::sleep(BACKUP_STEP_PAUSE),
        }
    }
}

/// Test-only hook invoked once per backup page-loop iteration, with the
/// store's cancellation flag. Production builds contain no such seam (the
/// module does not exist outside `cfg(test)`).
#[cfg(test)]
pub(crate) mod backup_step_hook {
    use std::path::Path;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Mutex, OnceLock};

    type Hook = Box<dyn Fn(&Path, &AtomicBool) + Send + Sync + 'static>;

    fn slot() -> &'static Mutex<Option<Hook>> {
        static SLOT: OnceLock<Mutex<Option<Hook>>> = OnceLock::new();
        SLOT.get_or_init(|| Mutex::new(None))
    }

    /// Install (or clear with `None`) the per-step hook. One hook at a time;
    /// tests that install one must clear it before finishing.
    pub(crate) fn set(hook: Option<Hook>) {
        *slot().lock().unwrap_or_else(|p| p.into_inner()) = hook;
    }

    pub(crate) fn invoke(dest: &Path, cancel: &AtomicBool) {
        let guard = slot().lock().unwrap_or_else(|p| p.into_inner());
        if let Some(hook) = guard.as_ref() {
            hook(dest, cancel);
        }
    }
}

pub(crate) fn check_integrity(conn: &Connection) -> StoreResult<Vec<String>> {
    let mut stmt = conn.prepare("PRAGMA integrity_check")?;
    let mut rows = stmt.query([])?;
    let mut issues = Vec::new();
    while let Some(row) = rows.next()? {
        let line: String = row.get(0)?;
        if line != "ok" {
            issues.push(line);
        }
    }
    Ok(issues)
}

/// `PRAGMA quick_check`: the bounded sibling of [`check_integrity`] — it
/// validates page structure and record round-trips but skips the full
/// scan's index-content/UNIQUE re-verification, so it runs in a fraction of
/// the time on large stores. Same shape: problem lines; empty = healthy.
pub(crate) fn check_quick(conn: &Connection) -> StoreResult<Vec<String>> {
    let mut stmt = conn.prepare("PRAGMA quick_check")?;
    let mut rows = stmt.query([])?;
    let mut issues = Vec::new();
    while let Some(row) = rows.next()? {
        let line: String = row.get(0)?;
        if line != "ok" {
            issues.push(line);
        }
    }
    Ok(issues)
}

/// Wall-clock milliseconds since the Unix epoch (`0` if the clock reads
/// before it). Callers capture the value on THEIR thread before enqueueing a
/// writer job (audit items 4/8): the job body must prepare nothing, so it
/// never reads a clock itself — the captured value travels into the closure.
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Newest durable fact stamp across BOTH scoped fact tables. `memory_fact`
/// and `workspace_memory_fact` share the monotone-stamp contract (a new row
/// must sort strictly newer than every existing row), so a restart seeds the
/// in-memory sequence above the maximum of both — seeding from the session
/// table alone let a backward wall clock re-enter an existing workspace
/// row's order position after a restart.
fn durable_max_fact_stamp(conn: &Connection) -> i64 {
    let session: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(updated_ms), 0) FROM memory_fact",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let workspace: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(updated_ms), 0) FROM workspace_memory_fact",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    session.max(workspace)
}

/// Mint the boot-instance identity of one store open (audit P1). Opaque,
/// bounded, collision-resistant across processes and reopens within one
/// process: pid + wall clock + a process-local boot counter + this frame's
/// address under a domain-separated BLAKE3. The value only ever needs to
/// differ between boots that could see each other's pending admission rows.
fn mint_boot_generation() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static BOOT_SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = BOOT_SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id() as u64;
    let now = now_ms() as u64;
    let stack = (&BOOT_SEQ as *const AtomicU64).addr() as u64;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"faktor.store.boot-generation/v1");
    hasher.update(&pid.to_le_bytes());
    hasher.update(&now.to_le_bytes());
    hasher.update(&seq.to_le_bytes());
    hasher.update(&stack.to_le_bytes());
    hasher.finalize().to_hex().to_string()
}

/// Checked narrowing of a persisted non-negative integer column to `u32`.
/// Every value written through the typed API fits, so one that does not is
/// corruption: it is refused by field name instead of silently truncated
/// (`as u32` wraps) or clamped (negative -> 0).
pub(crate) fn u32_field(ctx: &str, raw: i64) -> StoreResult<u32> {
    u32::try_from(raw)
        .map_err(|_| StoreError::Corrupt(vec![format!("{ctx} value {raw} is outside u32")]))
}

/// Checked decode of one persisted id column into its `u64`-backed newtype.
///
/// SQLite has no unsigned integers: every `u64` id is persisted through the
/// lossless two's-complement `raw as i64` bit cast, so the read side is the
/// inverse cast (`raw as u64`) and the upper half of the id space round-trips
/// exactly. A NEGATIVE `raw` therefore decodes to the high-half `u64`
/// (`-1` -> `u64::MAX`, `i64::MIN` -> `2^63`) BY DESIGN — it is the
/// intentional legacy encoding, never treated as corruption. Only a value
/// the typed id rejects (zero) is corruption: the id type's `TryFrom` maps
/// it to typed [`StoreError::Corrupt`] naming the row and column — never the
/// `OpId::new(0)` panic or a silently minted `1` under `.max(1)`.
pub(crate) fn id_field<T>(ctx: &str, raw: i64) -> StoreResult<T>
where
    T: TryFrom<u64>,
    T::Error: std::fmt::Display,
{
    T::try_from(raw as u64).map_err(|e| StoreError::Corrupt(vec![format!("{ctx}: {e}")]))
}

/// Optional variant of [`id_field`] for nullable id columns.
pub(crate) fn id_field_opt<T>(ctx: &str, raw: Option<i64>) -> StoreResult<Option<T>>
where
    T: TryFrom<u64>,
    T::Error: std::fmt::Display,
{
    raw.map(|v| id_field(ctx, v)).transpose()
}

/// One optional row through a fallible ([`StoreResult`]) mapper. `rusqlite`
/// offers `query_row` (infallible mapper) and `query_row_and_then` (fallible
/// mapper, but absence stays an error); this keeps typed mapper errors and
/// maps SQL `QueryReturnedNoRows` to `None`.
pub(crate) fn query_row_optional<T>(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
    map: impl FnOnce(&rusqlite::Row<'_>) -> StoreResult<T>,
) -> StoreResult<Option<T>> {
    match conn.query_row_and_then(sql, params, map) {
        Ok(row) => Ok(Some(row)),
        Err(StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Fallible JSON parse of persisted data: corrupted or version-skewed rows
/// surface as `Corrupt`, never a panic.
pub(crate) fn parse_json<T: serde::de::DeserializeOwned>(ctx: &str, raw: &str) -> StoreResult<T> {
    serde_json::from_str(raw).map_err(|e| StoreError::Corrupt(vec![format!("{ctx}: {e}")]))
}

// ---------------------------------------------------------------------------
// Writer service certification (audit item 7) and poisoning policy (item 8).
//
// These tests are adversarial: they inject slow transactions, saturation,
// deliberate panics while locks are held, and concurrent multi-domain
// writers, then assert the typed refusal/recovery contract — never a daemon
// panic and never a wedged store.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod writer_service_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Condvar, Mutex, MutexGuard};
    use std::thread;
    use std::time::{Duration, Instant};

    fn store_with_session() -> (tempfile::TempDir, Arc<Store>, WorkspaceId, SessionId) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path(), true).unwrap());
        let ws = store.create_workspace("/w").unwrap();
        let sid = store.create_session(ws, "writer", "fake", "m").unwrap().id;
        (dir, store, ws, sid)
    }

    /// Deterministically drain the writer receipts: commands run FIFO on the
    /// single writer owner, so observing the receipt of a barrier job proves
    /// every job enqueued before the barrier (e.g. `create_session`) has
    /// executed. A bare `take_writer_receipts()` drains only what has already
    /// run, letting an earlier job's receipt land afterwards and breaking
    /// tests that assume an empty ring.
    fn drain_writer_receipts(store: &Store) {
        store
            .writer_debug_job("receipt_drain_barrier", |_conn| ())
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let receipts = store.take_writer_receipts();
            if receipts.iter().any(|r| r.label == "receipt_drain_barrier") {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "writer receipt drain barrier was never observed"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// Wait (bounded) until at least one receipt exists, then drain. The
    /// writer records a job's receipt AFTER handing the caller its result, so
    /// a completed command's receipt can lag its return by a scheduling
    /// window; this is the honest barrier for tests that need it.
    fn drain_after_completion(store: &Store) -> Vec<WriterJobReceipt> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let receipts = store.take_writer_receipts();
            if !receipts.is_empty() {
                return receipts;
            }
            assert!(
                Instant::now() < deadline,
                "no writer receipt became visible within the bound"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    fn wait_until(mut cond: impl FnMut() -> bool, what: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !cond() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// An injected slow transaction holds the single writer owner: reads and
    /// another domain's PREPARATION (JSON serialization + enqueue) proceed
    /// while it runs, and the queued writer commits only after it — queue
    /// semantics, not a process-wide lock.
    #[test]
    fn slow_holder_does_not_block_reads_or_preparation() {
        let (_d, store, _ws, sid) = store_with_session();
        let holder_store = Arc::clone(&store);
        let holders_started = Arc::new(AtomicBool::new(false));
        let started = Arc::clone(&holders_started);
        let holder = thread::spawn(move || {
            holder_store
                .writer_debug_job("cert_slow_holder", move |_conn| {
                    started.store(true, Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(800));
                })
                .unwrap();
        });
        wait_until(|| holders_started.load(Ordering::SeqCst), "slow holder");

        // Reads never take the writer owner: they complete while it sleeps.
        let read_started = Instant::now();
        let sessions = store.list_sessions(None).unwrap();
        assert!(!sessions.is_empty());
        assert!(
            read_started.elapsed() < Duration::from_millis(400),
            "read blocked behind the slow holder: {:?}",
            read_started.elapsed()
        );

        // Another domain's write: a large payload is serialized on this
        // thread BEFORE enqueueing, and the enqueue itself succeeds while
        // the holder still runs (only execution waits).
        let payload: serde_json::Value = serde_json::json!({ "blob": "x".repeat(512 * 1024) });
        let queued_started = Instant::now();
        store
            .append_ledger_entry(sid, "cert_probe", 1, payload)
            .unwrap();
        let queued_elapsed = queued_started.elapsed();
        assert!(
            queued_elapsed >= Duration::from_millis(100),
            "queued writer must wait for the owner, not run concurrently: {queued_elapsed:?}"
        );
        holder.join().unwrap();

        let telemetry = store.writer_telemetry();
        assert!(telemetry.available, "{telemetry:?}");
        assert!(telemetry.jobs >= 2, "{telemetry:?}");
        assert!(telemetry.slow_transactions >= 1, "{telemetry:?}");
        assert!(
            telemetry.transaction_max_ns >= Duration::from_millis(700).as_nanos() as u64,
            "{telemetry:?}"
        );
        assert!(telemetry.queue_wait_max_ns >= Duration::from_millis(100).as_nanos() as u64);
        assert!(telemetry.pending_max >= 1, "{telemetry:?}");
    }

    /// Sessions / messages / tasks / ledger / memory / evidence writers run
    /// concurrently on one store: every prepared command commits, the
    /// journal stays gapless and no domain can corrupt another's rows.
    #[test]
    fn concurrent_domain_writers_all_commit() {
        let (_d, store, ws, sid) = store_with_session();
        const N: usize = 16;
        let mut handles = Vec::new();

        handles.push(thread::spawn({
            let store = Arc::clone(&store);
            move || {
                for _ in 0..N {
                    store.create_session(ws, "t", "p", "m").unwrap();
                }
            }
        }));
        handles.push(thread::spawn({
            let store = Arc::clone(&store);
            move || {
                for i in 0..N {
                    store
                        .put_message(sid, i as i64 + 1, "user", serde_json::json!({"i": i}))
                        .unwrap();
                }
            }
        }));
        handles.push(thread::spawn({
            let store = Arc::clone(&store);
            move || {
                for i in 0..N {
                    store
                        .put_task_ledger(sid, serde_json::json!({ "task": i }))
                        .unwrap();
                }
            }
        }));
        handles.push(thread::spawn({
            let store = Arc::clone(&store);
            move || {
                for i in 0..N {
                    store
                        .append_ledger_entry(sid, "cert_domain", 1, serde_json::json!({ "i": i }))
                        .unwrap();
                }
            }
        }));
        handles.push(thread::spawn({
            let store = Arc::clone(&store);
            move || {
                for i in 0..N {
                    store
                        .upsert_memory_fact(sid, "cert", &format!("k{i}"), "v")
                        .unwrap();
                }
            }
        }));
        handles.push(thread::spawn({
            let store = Arc::clone(&store);
            move || {
                for i in 0..N {
                    store
                        .evidence_insert(&EvidenceRow {
                            id: 0,
                            session_id: sid,
                            workspace_id: ws,
                            task_id: None,
                            kind: "cert".into(),
                            revision: 1,
                            provenance_json: "{}".into(),
                            compressibility: "none".into(),
                            compression_json: "{}".into(),
                            retrieval_json: "{}".into(),
                            compact_json: "{}".into(),
                            backing_cas_hash: None,
                            completeness: "complete".into(),
                            created_ms: i as i64,
                        })
                        .unwrap();
                }
            }
        }));
        handles.push(thread::spawn({
            let store = Arc::clone(&store);
            move || {
                for i in 0..N {
                    store
                        .put_worktree(ws, &format!("/wt/{i}"), &format!("b{i}"))
                        .unwrap();
                }
            }
        }));
        handles.push(thread::spawn({
            let store = Arc::clone(&store);
            move || {
                for _ in 0..N {
                    store
                        .model_outcome_stats_append(
                            "cert-provider",
                            "cert-model",
                            RouterPhase::Plan,
                            TaskClass::Easy,
                            RiskBucket::Low,
                            ModelOutcomeSample {
                                verified_success: true,
                                rework_cost_micro: 0,
                                rework_turns: 0,
                            },
                        )
                        .unwrap();
                }
            }
        }));
        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(store.list_sessions(None).unwrap().len(), 1 + N);
        assert_eq!(store.messages_before(sid, None, 100).unwrap().len(), N);
        assert_eq!(store.ledger_entries(sid, None, 100).unwrap().len(), N);
        assert_eq!(store.evidence_list_by_scope(sid, ws, 100).unwrap().len(), N);
        // Journal seq is gapless after all cross-domain interleaving.
        let events = store.events_range(sid, 1, None).unwrap();
        for (idx, event) in events.iter().enumerate() {
            assert_eq!(event.seq.raw(), idx as u64 + 1, "gapless journal");
        }
        assert!(store.writer_telemetry().jobs >= (N * 8) as u64);
        assert_eq!(store.worktrees_of(ws).unwrap().len(), N);
    }

    /// The bounded queue refuses a command with a typed error once it is
    /// saturated past the configured wait — never an unbounded wait, never
    /// unbounded growth.
    #[test]
    fn bounded_queue_refusal_is_typed() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            Store::open_with_writer_limits(dir.path(), true, 1, Duration::from_millis(150))
                .unwrap(),
        );
        let started = Arc::new(AtomicBool::new(false));
        let s2 = Arc::clone(&started);
        let holder_store = Arc::clone(&store);
        let holder = thread::spawn(move || {
            holder_store
                .writer_debug_job("cert_hold", move |_conn| {
                    s2.store(true, Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(700));
                })
                .unwrap();
        });
        wait_until(|| started.load(Ordering::SeqCst), "holder");

        let queued_store = Arc::clone(&store);
        let queued =
            thread::spawn(move || queued_store.writer_debug_job("cert_queued", |_conn| ()));
        wait_until(
            || store.writer_telemetry().pending_depth == 1,
            "one queued command",
        );

        let refused = store.writer_debug_job("cert_refused", |_conn| ());
        match refused {
            Err(StoreError::WriterQueueFull {
                operation,
                pending,
                capacity,
            }) => {
                assert_eq!(operation, "cert_refused");
                assert_eq!(pending, 1);
                assert_eq!(capacity, 1);
            }
            other => panic!("expected typed WriterQueueFull, got {other:?}"),
        }
        assert_eq!(store.writer_telemetry().queue_full_refusals, 1);
        holder.join().unwrap();
        queued.join().unwrap().unwrap();
    }

    /// A panic inside a command is caught at the durable-authority boundary:
    /// the caller gets a typed refusal, every later mutation is refused with
    /// the same typed error (never a daemon panic), and reopening the store
    /// yields a fresh healthy authority.
    #[test]
    fn writer_panic_is_typed_unavailable_and_reopen_recovers() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let err = store
            .writer_debug_job("cert_panic", |_conn| panic!("deliberate writer panic"))
            .unwrap_err();
        assert!(matches!(err, StoreError::WriterUnavailable(_)), "{err:?}");
        assert!(!store.writer_available());
        assert!(store.writer_unavailable_reason().is_some());

        // The poisoned authority refuses mutations typed, reads still work.
        let err = store.create_session(ws, "t", "p", "m").unwrap_err();
        assert!(matches!(err, StoreError::WriterUnavailable(_)), "{err:?}");
        assert!(store.list_sessions(None).is_ok());
        let telemetry = store.writer_telemetry();
        assert!(!telemetry.available);
        assert!(telemetry.unavailable_reason.is_some());

        drop(store);
        let store = Store::open(dir.path(), true).unwrap();
        assert!(store.writer_available());
        let sid = store.create_session(ws, "t", "p", "m").unwrap().id;
        assert_eq!(store.list_sessions(None).unwrap().len(), 1);
        store
            .append_ledger_entry(sid, "after_reopen", 1, serde_json::json!({}))
            .unwrap();
    }

    /// Bounded contention probe (third-angle performance audit): 16 threads
    /// issue 2.5k reads each through the SAME reader-pool API. The pool is
    /// DELIBERATELY capped at `READER_POOL` connections (SQLite WAL allows
    /// concurrent readers; the cap is the resource bound), so permit waits
    /// are expected and measured here rather than removed.
    #[test]
    fn reader_pool_16_thread_contention_probe() {
        const THREADS: usize = 16;
        const PER_THREAD: usize = 2_500;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/reader-probe").unwrap();
        let started = Instant::now();
        std::thread::scope(|scope| {
            for _ in 0..THREADS {
                scope.spawn(|| {
                    for _ in 0..PER_THREAD {
                        assert_eq!(
                            store.workspace_root(ws).unwrap().as_deref(),
                            Some("/reader-probe")
                        );
                    }
                });
            }
        });
        let elapsed = started.elapsed();
        let telemetry = store.reader_telemetry();
        eprintln!(
            "store reader-pool probe: {THREADS} threads x {PER_THREAD} reads in {elapsed:?}              ({:.0} reads/s); reads={} permit_wait_total={}us max_wait={}us receipts_dropped={}",
            (THREADS * PER_THREAD) as f64 / elapsed.as_secs_f64(),
            telemetry.reads,
            telemetry.permit_wait_total_ns / 1_000,
            telemetry.permit_wait_max_ns / 1_000,
            telemetry.receipts_dropped,
        );
        assert_eq!(telemetry.reads, (THREADS * PER_THREAD) as u64);
        assert!(
            telemetry.permit_wait_max_ns < BUSY_TIMEOUT.as_nanos() as u64,
            "a reader waited past the busy timeout"
        );
        assert!(store.reader_pool_len() <= READER_POOL);
    }

    /// Ephemeral store locks (reader-pool semaphore + connection cache,
    /// crash-seam state) recover from a deliberate panic while held: later
    /// reads/writes are healthy, never a panic and never a stuck store.
    #[test]
    fn panic_while_ephemeral_locks_held_recovers() {
        let (_d, store) = {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(dir.path(), true).unwrap();
            (dir, store)
        };

        let poison = |f: &dyn Fn(&Store)| {
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&store)));
            assert!(caught.is_err(), "the deliberate panic must fire");
        };
        // Hold + poison the reader-pool connection cache mutex.
        poison(&|s: &Store| {
            let _guard: MutexGuard<'_, Vec<Connection>> =
                s.pool.conns.lock().unwrap_or_else(|p| p.into_inner());
            panic!("poison reader pool");
        });
        // Hold + poison the reader-pool semaphore permits mutex.
        poison(&|s: &Store| {
            let _guard = s.pool.sem.permits.lock().unwrap_or_else(|p| p.into_inner());
            panic!("poison reader semaphore");
        });
        // Hold + poison the crash-seam state mutex.
        poison(&|s: &Store| {
            let _guard = s.seam.state.lock().unwrap_or_else(|p| p.into_inner());
            panic!("poison crash seam");
        });

        // The writer service was never involved and stays healthy; ephemeral
        // state recovered from poison instead of wedging.
        assert!(store.writer_available());
        let ws = store.create_workspace("/after_poison").unwrap();
        let sid = store.create_session(ws, "t", "p", "m").unwrap().id;
        store
            .append_ledger_entry(sid, "after_poison", 1, serde_json::json!({}))
            .unwrap();
        assert_eq!(store.list_sessions(None).unwrap().len(), 1);
        // The recovered seam arms and trips normally.
        store.crash_arm(CrashArm {
            point: "never_crossed",
            ordinal: 0,
        });
        store.crash_arm(CrashArm {
            point: "never_crossed",
            ordinal: 0,
        });
    }

    /// WAL checkpoint duration is instrumented on the owner.
    #[test]
    fn checkpoint_duration_is_instrumented() {
        let (_d, store, _ws, _sid) = store_with_session();
        let before = store.writer_telemetry().checkpoints;
        store.wal_checkpoint_passive().unwrap();
        let telemetry = store.writer_telemetry();
        assert_eq!(telemetry.checkpoints, before + 1, "{telemetry:?}");
        assert!(telemetry.jobs >= 1, "{telemetry:?}");
    }

    /// Always-on SQLite governance: the WAL is checkpoint-truncated and a
    /// growth that cannot fit the class quota/aggregate budget is refused
    /// BEFORE the append (typed StoreError::StorageRefused).
    #[test]
    fn storage_governor_bounds_wal_growth_and_refuses_over_budget() {
        use faktor_storage::{StorageClass, StorageGovernor, StorageGovernorConfig};

        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("store"), true).unwrap();
        store
            .sql_execute("CREATE TABLE storage_probe(body BLOB NOT NULL)")
            .unwrap();
        for _ in 0..4 {
            store
                .sql_execute("INSERT INTO storage_probe(body) VALUES (randomblob(200000))")
                .unwrap();
        }
        let db = dir.path().join("store").join("faktor-plus.db");
        let wal = dir.path().join("store").join("faktor-plus.db-wal");
        let db_size = std::fs::metadata(&db).unwrap().len();
        let before = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
        assert!(before > 0, "the probe must leave a WAL");
        let quota = db_size + before / 2;
        let config = StorageGovernorConfig {
            total_budget_bytes: quota + 1_000_000,
            watermark_permille: 900,
            reservation_lease_ms: 60_000,
            quotas: StorageClass::ALL
                .into_iter()
                .map(|class| {
                    (
                        class,
                        if class == StorageClass::SqliteWal {
                            quota
                        } else {
                            1_000_000
                        },
                    )
                })
                .collect(),
            ..Default::default()
        }
        .validated()
        .unwrap();
        let governor = Arc::new(StorageGovernor::open(dir.path(), config).unwrap());
        store.attach_storage_governor(governor.clone());
        let reclaimed = store.storage_maintenance().unwrap();
        let after = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
        assert!(
            after < before || reclaimed > 0,
            "WAL must be truncated by maintenance (before {before}, after {after})"
        );
        let error = store.admit_storage_growth(quota).unwrap_err();
        assert!(
            matches!(error, StoreError::StorageRefused(_)),
            "over-budget SQLite growth must be refused typed: {error}"
        );
        assert!(governor.counters().unwrap().refusals >= 1);
        assert!(governor
            .health()
            .unwrap()
            .is_some_and(|health| health.class == Some(StorageClass::SqliteWal)));
    }

    /// Every typed store mutation (not only the hot batch) is admitted
    /// against the SQLite/WAL class quota: when the budget cannot hold the
    /// append the write is refused BEFORE the writer queue or SQLite sees it.
    #[test]
    fn every_typed_write_path_is_refused_before_sqlite_growth() {
        use faktor_storage::{StorageClass, StorageGovernor, StorageGovernorConfig};

        let dir = tempfile::tempdir().unwrap();
        let config = StorageGovernorConfig {
            total_budget_bytes: 2_000_000,
            watermark_permille: 900,
            reservation_lease_ms: 60_000,
            quotas: StorageClass::ALL
                .into_iter()
                .map(|class| {
                    (
                        class,
                        if class == StorageClass::SqliteWal {
                            1
                        } else {
                            1_000_000
                        },
                    )
                })
                .collect(),
            ..Default::default()
        }
        .validated()
        .unwrap();
        let governor = Arc::new(StorageGovernor::open(dir.path(), config).unwrap());
        let store = Store::open(dir.path().join("store"), true).unwrap();
        store.attach_storage_governor(governor.clone());
        let error = store.create_workspace("/refused").unwrap_err();
        assert!(
            matches!(error, StoreError::StorageRefused(_)),
            "a typed single write must be refused typed: {error}"
        );
        assert!(governor.counters().unwrap().refusals >= 1);
        assert!(governor
            .health()
            .unwrap()
            .is_some_and(|health| health.class == Some(StorageClass::SqliteWal)));
    }

    /// The writer thread never blocks reads even when the queue is saturated
    /// with slow commands (bounded pending depth is observable).
    #[test]
    fn saturated_queue_still_serves_reads() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(
            Store::open_with_writer_limits(dir.path(), true, 2, Duration::from_secs(5)).unwrap(),
        );
        let ws = store.create_workspace("/w").unwrap();
        let sid = store.create_session(ws, "t", "p", "m").unwrap().id;

        let started = Arc::new(AtomicBool::new(false));
        let s2 = Arc::clone(&started);
        let holder_store = Arc::clone(&store);
        let holder = thread::spawn(move || {
            holder_store
                .writer_debug_job("sat_holder", move |_conn| {
                    s2.store(true, Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(400));
                })
                .unwrap();
        });
        wait_until(|| started.load(Ordering::SeqCst), "holder");
        let queued_a = thread::spawn({
            let store = Arc::clone(&store);
            move || store.writer_debug_job("sat_a", |_conn| ())
        });
        wait_until(|| store.writer_telemetry().pending_depth >= 1, "queued a");

        let read_started = Instant::now();
        assert_eq!(store.list_sessions(None).unwrap().len(), 1);
        assert!(read_started.elapsed() < Duration::from_millis(200));
        holder.join().unwrap();
        queued_a.join().unwrap().unwrap();
        let telemetry = store.writer_telemetry();
        assert!(telemetry.pending_max >= 1, "{telemetry:?}");
        let _ = sid;
    }

    /// Audit 5: writer-wait accounting is PER JOB (label + queue wait +
    /// execution), drained explicitly, never inferred from before/after
    /// deltas of cumulative counters that mix every concurrent thread's jobs.
    #[test]
    fn writer_receipts_account_each_job_and_drain_once() {
        let (_d, store, _ws, sid) = store_with_session();
        drain_writer_receipts(&store);
        let before = store.writer_telemetry();
        let dropped_before = before.receipts_dropped;
        store
            .append_ledger_entry(sid, "receipt_probe", 1, serde_json::json!({"i": 1}))
            .unwrap();
        let receipts = drain_after_completion(&store);
        assert_eq!(receipts.len(), 1, "{receipts:?}");
        let receipt = &receipts[0];
        assert_eq!(receipt.label, "append_ledger_entry");
        assert!(receipt.run_ns > 0, "a committed transaction takes time");
        assert!(
            receipt.run_ns < Duration::from_secs(30).as_nanos() as u64,
            "bogus execution duration: {receipt:?}"
        );
        // Drain-once: a second drain observes nothing until a new job runs.
        assert!(store.take_writer_receipts().is_empty());
        let after = store.writer_telemetry();
        assert_eq!(after.jobs, before.jobs + 1);
        assert_eq!(
            after.receipts_dropped, dropped_before,
            "one job never evicts"
        );
    }

    /// The receipt ring is a BOUNDED diagnostic: a consumer that never drains
    /// loses the oldest receipts, and the loss is COUNTED (never a silently
    /// truncated series that passes as complete).
    #[test]
    fn writer_receipt_ring_is_bounded_and_counts_evictions() {
        let (_d, store, _ws, _sid) = store_with_session();
        drain_writer_receipts(&store);
        let telemetry_before = store.writer_telemetry();
        let dropped_before = telemetry_before.receipts_dropped;
        let jobs_before = telemetry_before.jobs;
        let overflow = WRITER_RECEIPT_CAPACITY + 8;
        for _ in 0..overflow {
            store
                .writer_debug_job("receipt_ring_probe", |_conn| ())
                .unwrap();
        }
        // A job's telemetry is recorded after its caller is unblocked (the
        // receipt ordering documented on `drain_after_completion`): wait for
        // all `overflow` completions before draining the ring.
        wait_until(
            || store.writer_telemetry().jobs >= jobs_before + overflow as u64,
            "writer jobs to be recorded",
        );
        let receipts = store.take_writer_receipts();
        assert_eq!(receipts.len(), WRITER_RECEIPT_CAPACITY);
        assert!(receipts.iter().all(|r| r.label == "receipt_ring_probe"));
        let telemetry = store.writer_telemetry();
        assert_eq!(
            (telemetry.receipts_dropped - dropped_before) as usize,
            overflow - WRITER_RECEIPT_CAPACITY
        );
        assert!(store.take_writer_receipts().is_empty());
    }

    // ------------------------------------------------------------------
    // Bounded shutdown (audit item 7)
    // ------------------------------------------------------------------

    /// A statement that runs for seconds on the owner: a recursive CTE the
    /// progress handler / SQLite interrupt can abort mid-VM.
    const SLOW_SQL: &str = "WITH RECURSIVE cnt(x) AS (
            SELECT 1 UNION ALL SELECT x + 1 FROM cnt WHERE x < 100000000
        ) SELECT COUNT(*) FROM cnt";

    /// Slow SQL on the owner => `shutdown` returns within the bound with the
    /// typed `Interrupted` outcome: the SQLite interrupt + progress handler
    /// abort the runaway statement, the caller of that job gets a typed
    /// error, and admissions stop (later mutations fail typed).
    #[test]
    fn shutdown_interrupts_slow_sql_within_the_bound() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path(), true).unwrap());
        let ws = store.create_workspace("/w").unwrap();
        store.create_session(ws, "t", "p", "m").unwrap();

        let started = Arc::new(AtomicBool::new(false));
        let interrupted = Arc::new(AtomicBool::new(false));
        let s2 = Arc::clone(&started);
        let saw_interrupt = Arc::clone(&interrupted);
        let job_store = Arc::clone(&store);
        let slow = thread::spawn(move || {
            job_store.writer_debug_job("slow_sql", move |conn| {
                s2.store(true, Ordering::SeqCst);
                let mut stmt = conn.prepare(SLOW_SQL).unwrap();
                let mut rows = stmt.query([]).unwrap();
                loop {
                    match rows.next() {
                        Ok(Some(row)) => {
                            let _: i64 = row.get(0).unwrap();
                        }
                        Ok(None) => break,
                        // The shutdown interrupt aborts the statement here:
                        // the job observes it and returns normally.
                        Err(_) => {
                            saw_interrupt.store(true, Ordering::SeqCst);
                            break;
                        }
                    }
                }
            })
        });
        wait_until(|| started.load(Ordering::SeqCst), "slow SQL job");

        let bounded = Duration::from_millis(300);
        let at = Instant::now();
        let outcome = store.shutdown(bounded);
        let elapsed = at.elapsed();
        assert!(
            elapsed < bounded + WRITER_SHUTDOWN_INTERRUPT_GRACE + Duration::from_secs(2),
            "shutdown must return within the documented bound: {elapsed:?}"
        );
        // The interrupt lands: the owner joins inside the grace period.
        assert_eq!(
            outcome.unwrap(),
            WriterShutdownOutcome::Interrupted { rejected: 0 }
        );
        // The job observed the SQLite interrupt and its caller never hangs.
        slow.join().unwrap().unwrap();
        assert!(
            interrupted.load(Ordering::SeqCst),
            "the slow statement must be aborted by the shutdown interrupt"
        );
        // Admissions stopped; the stop is observable and typed.
        assert!(!store.writer_available());
        assert!(store.writer_stopped());
        match store.create_session(ws, "after", "p", "m") {
            Err(StoreError::WriterUnavailable(_)) => {}
            other => panic!("post-shutdown mutation must be typed unavailable: {other:?}"),
        }
    }

    /// A clean shutdown DRAINS every queued mutation deterministically: the
    /// slow holder finishes, all queued jobs commit with their own outcomes,
    /// and only then does the service stop admitting new work.
    #[test]
    fn shutdown_drains_queued_mutations_deterministically() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path(), true).unwrap());
        let ws = store.create_workspace("/w").unwrap();
        let sid = store.create_session(ws, "t", "p", "m").unwrap().id;

        // A slow (but non-interruptible) holder, then queued mutations behind
        // it. Drain semantics must run them all, in FIFO order.
        let entered = Arc::new(AtomicBool::new(false));
        let e2 = Arc::clone(&entered);
        let holder_store = Arc::clone(&store);
        let holder = thread::spawn(move || {
            holder_store.writer_debug_job("drain_holder", move |_conn| {
                e2.store(true, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(250));
            })
        });
        wait_until(|| entered.load(Ordering::SeqCst), "drain holder");

        let mut queued = Vec::new();
        for i in 0..5 {
            let queued_store = Arc::clone(&store);
            queued.push(thread::spawn(move || {
                queued_store.append_ledger_entry(
                    sid,
                    "drain_queued",
                    1,
                    serde_json::json!({ "i": i }),
                )
            }));
        }
        wait_until(
            || store.writer_telemetry().pending_depth >= 5,
            "five queued mutations",
        );

        let outcome = store.shutdown(Duration::from_secs(5)).unwrap();
        assert_eq!(outcome, WriterShutdownOutcome::Drained);
        holder.join().unwrap().unwrap();
        for (i, handle) in queued.into_iter().enumerate() {
            let seq = handle
                .join()
                .unwrap()
                .unwrap_or_else(|e| panic!("queued mutation {i} must drain, got {e:?}"));
            assert!(seq >= 1, "drained ledger entry has a real seq: {seq}");
        }
        // Drained mutations are durable and the stop is typed for new work.
        assert_eq!(store.ledger_entries(sid, None, 100).unwrap().len(), 5);
        assert!(store.writer_stopped());
        match store.append_ledger_entry(sid, "after_shutdown", 1, serde_json::json!({})) {
            Err(StoreError::WriterUnavailable(_)) => {}
            other => panic!("post-shutdown mutation must be typed unavailable: {other:?}"),
        }
    }

    /// A job that cannot be interrupted (sleeping outside SQLite) forces the
    /// timeout path: `shutdown` returns the typed timeout, every queued
    /// mutation is REJECTED with the typed unavailable error, and the owner
    /// thread is detached — never joined unboundedly.
    #[test]
    fn shutdown_timeout_rejects_queued_mutations_and_detaches_typed() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path(), true).unwrap());

        let entered = Arc::new(AtomicBool::new(false));
        let e2 = Arc::clone(&entered);
        let holder_store = Arc::clone(&store);
        let holder = thread::spawn(move || {
            holder_store.writer_debug_job("uninterruptible_holder", move |_conn| {
                e2.store(true, Ordering::SeqCst);
                // No SQL: the interrupt cannot reach a plain sleep. Bounded,
                // deterministic non-interruptibility for the test.
                thread::sleep(Duration::from_millis(1500));
            })
        });
        wait_until(|| entered.load(Ordering::SeqCst), "holder");

        let mut queued = Vec::new();
        for i in 0..2 {
            let queued_store = Arc::clone(&store);
            queued.push(thread::spawn(move || {
                queued_store.writer_debug_job("rejected_queued", move |_conn| {
                    let _ = i;
                })
            }));
        }
        wait_until(
            || store.writer_telemetry().pending_depth >= 2,
            "two queued mutations",
        );

        let at = Instant::now();
        let err = store.shutdown(Duration::from_millis(150)).unwrap_err();
        let elapsed = at.elapsed();
        assert!(
            elapsed
                < Duration::from_millis(150)
                    + WRITER_SHUTDOWN_INTERRUPT_GRACE
                    + Duration::from_secs(2),
            "shutdown must stay bounded: {elapsed:?}"
        );
        match err {
            StoreError::WriterShutdownTimeout { rejected } => assert_eq!(rejected, 2),
            other => panic!("expected typed WriterShutdownTimeout, got {other:?}"),
        }
        // Deterministic rejection: both queued callers wake typed.
        for (i, handle) in queued.into_iter().enumerate() {
            match handle.join().unwrap() {
                Err(StoreError::WriterUnavailable(_)) => {}
                other => panic!("queued mutation {i} must be rejected typed: {other:?}"),
            }
        }
        // The detached owner still finishes its uninterruptible job (bounded)
        // and then stops on its own.
        holder.join().unwrap().unwrap();
        wait_until(|| store.writer_stopped(), "detached owner stop");
    }

    /// Audit item 6/7 seam: a large backup running on the maintenance thread
    /// is cancelled by `shutdown` within the bound; its caller receives the
    /// typed maintenance cancellation and the writer still stops typed.
    ///
    /// Determinism: the per-step test hook holds the backup INSIDE its page
    /// loop and only returns once shutdown's cancellation flag is set (an
    /// observable condition, not a wall-clock race between registration and
    /// the shutdown call). Every wait has a generous ceiling; the typed
    /// assertions keep their teeth.
    #[test]
    fn shutdown_cancels_a_running_backup_within_the_bound() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path(), true).unwrap());
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let mid = store
            .put_message(s.id, 1, "user", serde_json::json!({"text": "seed"}))
            .unwrap();
        super::tests::seed_padded_parts(&store, mid, 200_000);

        // Observable mid-backup condition: the hook signals entry into the
        // page loop, then blocks until shutdown cancels it. It only reacts
        // to this test's destination, so concurrent backups stay untouched.
        let backup_path = dir.path().join("cancelled.db");
        let watched = backup_path.clone();
        let entered = Arc::new((Mutex::new(false), Condvar::new()));
        let entered_hook = Arc::clone(&entered);
        backup_step_hook::set(Some(Box::new(move |dest, cancel| {
            if dest != watched {
                return;
            }
            {
                let (flag, cv) = &*entered_hook;
                *flag.lock().unwrap_or_else(|p| p.into_inner()) = true;
                cv.notify_all();
            }
            let deadline = Instant::now() + Duration::from_secs(30);
            while !cancel.load(Ordering::Acquire) {
                assert!(
                    Instant::now() < deadline,
                    "shutdown never cancelled the held backup"
                );
                thread::sleep(Duration::from_millis(1));
            }
        })));

        let backup_store = Arc::clone(&store);
        let backup_path_for_thread = backup_path.clone();
        let backup = thread::spawn(move || backup_store.backup_to(&backup_path_for_thread));
        {
            let (flag, cv) = &*entered;
            let mut entered_flag = flag.lock().unwrap_or_else(|p| p.into_inner());
            let deadline = Instant::now() + Duration::from_secs(30);
            while !*entered_flag {
                let remaining = deadline.saturating_duration_since(Instant::now());
                assert!(!remaining.is_zero(), "backup never entered its page loop");
                let (guard, _) = cv
                    .wait_timeout(entered_flag, remaining)
                    .unwrap_or_else(|p| p.into_inner());
                entered_flag = guard;
            }
        }
        assert_eq!(store.maintenance_active_count(), 1);

        // The hook below reacts to cancellation immediately, but the bound
        // must also absorb scheduler latency on a loaded host: a 500ms
        // wall-clock bound turned host contention into a false
        // MaintenanceShutdownTimeout. The typed "ignores cancellation" path
        // is covered separately by `shutdown_reports_typed_error_when_
        // maintenance_does_not_drain`, which uses a short bound on purpose.
        let bounded = Duration::from_secs(5);
        let at = Instant::now();
        let outcome = store.shutdown(bounded);
        let elapsed = at.elapsed();
        assert!(
            elapsed < bounded + WRITER_SHUTDOWN_INTERRUPT_GRACE + Duration::from_secs(2),
            "shutdown with a running backup must stay bounded: {elapsed:?}"
        );
        assert_eq!(outcome.unwrap(), WriterShutdownOutcome::Drained);
        // The maintenance cancellation is typed for the backup caller.
        let backup_result = backup.join().unwrap();
        match backup_result {
            Err(StoreError::Maintenance(msg)) => {
                assert!(msg.contains("cancelled"), "typed cancellation: {msg}");
            }
            other => panic!("cancelled backup must fail typed: {other:?}"),
        }
        // Cancellation removed the partial destination: a half-written file
        // can never be mistaken for a complete backup.
        assert!(!backup_path.exists(), "partial backup file must be removed");
        assert_eq!(store.maintenance_active_count(), 0);
        backup_step_hook::set(None);
    }

    /// Finding 3: a maintenance task that ignores cancellation keeps a
    /// store-owned authority alive, so `shutdown` must NOT report success even
    /// when the writer drained cleanly. The typed error names the surviving
    /// authority and carries the writer outcome, the call stays bounded, and
    /// the fake task is still registered afterwards.
    #[test]
    fn shutdown_reports_typed_error_when_maintenance_does_not_drain() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(dir.path(), true).unwrap());
        let alive = Arc::new(AtomicBool::new(false));
        let fake = store.spawn_nonresponsive_maintenance(Arc::clone(&alive));
        assert!(alive.load(Ordering::SeqCst), "fake maintenance is alive");
        assert_eq!(store.maintenance_active_count(), 1);

        let bounded = Duration::from_millis(200);
        let at = Instant::now();
        let err = store.shutdown(bounded).unwrap_err();
        let elapsed = at.elapsed();
        assert!(
            elapsed < bounded + WRITER_SHUTDOWN_INTERRUPT_GRACE + Duration::from_secs(2),
            "shutdown with a live maintenance authority must stay bounded: {elapsed:?}"
        );
        match err {
            StoreError::MaintenanceShutdownTimeout {
                active,
                writer_outcome,
            } => {
                assert_eq!(active, 1, "the surviving authority is named");
                assert_eq!(
                    writer_outcome,
                    WriterShutdownOutcome::Drained,
                    "the writer outcome is carried even though maintenance leaked"
                );
            }
            other => panic!(
                "a surviving maintenance authority must be a typed non-success, got {other:?}"
            ),
        }
        // The fake task is STILL alive and registered after shutdown returned.
        assert!(fake.is_alive(), "fake maintenance must survive the bound");
        assert_eq!(store.maintenance_active_count(), 1);
        // Dropping the test task releases the registration (it "exits" after
        // the observation) so teardown never waits on it.
        drop(fake);
        wait_until(
            || store.maintenance_active_count() == 0,
            "fake maintenance exit",
        );
    }

    /// Finding: shutdown is a TOCTOU unless the registry is CLOSED before the
    /// cancel sweep. A backup that registers during the writer-drain window
    /// would otherwise escape cancellation while `shutdown` still reported
    /// success. This certifies the closed gate: once shutdown began, a new
    /// registration is refused.
    #[test]
    fn register_after_shutdown_begins_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        store.shutdown(Duration::from_millis(200)).unwrap();
        assert_eq!(store.maintenance_active_count(), 0);
        let cancel = Arc::new(AtomicBool::new(false));
        let interrupt = rusqlite::Connection::open_in_memory()
            .unwrap()
            .get_interrupt_handle();
        assert!(
            store.maintenance.register(cancel, interrupt).is_none(),
            "a maintenance registration after the shutdown close gate must be refused"
        );
    }

    /// A deliberately nonresponsive maintenance registration: it ignores the
    /// cancel flag and never deregisters on its own, so the registry cannot
    /// drain inside a shutdown bound. Dropping it releases the registration
    /// and lets its worker exit, so the test process never waits on it at
    /// teardown.
    struct NonresponsiveMaintenance {
        id: u64,
        registry: Arc<MaintenanceRegistry>,
        alive: Arc<AtomicBool>,
        worker: Option<thread::JoinHandle<()>>,
    }

    impl NonresponsiveMaintenance {
        fn is_alive(&self) -> bool {
            self.alive.load(Ordering::SeqCst)
        }
    }

    impl Drop for NonresponsiveMaintenance {
        fn drop(&mut self) {
            self.alive.store(false, Ordering::SeqCst);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
            self.registry.unregister(self.id);
        }
    }

    impl Store {
        /// Register a maintenance task that never observes cancellation, so a
        /// test can certify that a store-owned maintenance authority surviving
        /// the bound makes `Store::shutdown` return a typed non-success.
        fn spawn_nonresponsive_maintenance(
            &self,
            alive: Arc<AtomicBool>,
        ) -> NonresponsiveMaintenance {
            let cancel = Arc::new(AtomicBool::new(false));
            let interrupt = rusqlite::Connection::open_in_memory()
                .expect("in-memory connection for a fake maintenance interrupt handle")
                .get_interrupt_handle();
            let id = self
                .maintenance
                .register(Arc::clone(&cancel), interrupt)
                .expect("the registry is open before shutdown begins");
            alive.store(true, Ordering::SeqCst);
            let task_alive = Arc::clone(&alive);
            let worker = thread::Builder::new()
                .name("faktor-store-fake-maintenance".into())
                .spawn(move || {
                    while task_alive.load(Ordering::SeqCst) {
                        thread::sleep(Duration::from_millis(2));
                    }
                })
                .expect("spawn fake maintenance thread");
            NonresponsiveMaintenance {
                id,
                registry: Arc::clone(&self.maintenance),
                alive,
                worker: Some(worker),
            }
        }
    }
}

impl Store {
    /// The directory this store was opened at. The durable evidence
    /// authority roots its backing CAS beside it (`<root>/evidence-cas`), so
    /// a daemon restart reopens the SAME backing files the rows reference.
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    /// Open (creating if needed) and migrate. `integrity_check: true` runs a
    /// full integrity check before use and refuses to open a corrupt store.
    pub fn open(root: impl Into<PathBuf>, integrity_check: bool) -> StoreResult<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        let db_path = root.join("faktor-plus.db");

        let mut conn = Connection::open(&db_path)?;
        configure(&conn)?;
        migrate(&mut conn, Some(db_path.as_path()))?;

        if integrity_check {
            let issues = check_integrity(&conn)?;
            if !issues.is_empty() {
                return Err(StoreError::Corrupt(issues));
            }
        }

        Self::finish_open(root, conn)
    }

    /// Fast normal-start open (production `serve`, plain `doctor`): WAL
    /// recovery, migrations, and the BOUNDED `PRAGMA quick_check` — never
    /// the full `PRAGMA integrity_check` scan. Audit 43: the production path
    /// ran the full scan on EVERY start; the deep scan belongs to
    /// `doctor --deep` and crash forensics, not to startup latency.
    ///
    /// "Fast" is not "blind": the WAL is recovered and folded into the main
    /// file BEFORE the check (a crashed predecessor's frames are validated,
    /// never shadowed), migrations always run, and quick_check still refuses
    /// a store whose pages are damaged.
    pub fn open_fast(root: impl Into<PathBuf>) -> StoreResult<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        let db_path = root.join("faktor-plus.db");

        let mut conn = Connection::open(&db_path)?;
        configure(&conn)?;
        migrate(&mut conn, Some(db_path.as_path()))?;
        // WAL recovery: opening + configuring recovered any frames a crashed
        // predecessor left in the -wal; the checkpoint folds them into the
        // main file so the quick check below validates the post-recovery
        // state and a stale -wal sidecar can never shadow newer content.
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        let issues = check_quick(&conn)?;
        if !issues.is_empty() {
            return Err(StoreError::Corrupt(issues));
        }

        Self::finish_open(root, conn)
    }

    /// [`Store::open`] with explicit writer queue bounds. Adversarial tests
    /// drive the typed backpressure path and slow-holder scenarios without
    /// waiting the production defaults; production callers use
    /// [`Store::open`]/[`Store::open_fast`].
    #[doc(hidden)]
    pub fn open_with_writer_limits(
        root: impl Into<PathBuf>,
        integrity_check: bool,
        queue_capacity: usize,
        enqueue_timeout: Duration,
    ) -> StoreResult<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        let db_path = root.join("faktor-plus.db");

        let mut conn = Connection::open(&db_path)?;
        configure(&conn)?;
        migrate(&mut conn, Some(db_path.as_path()))?;
        if integrity_check {
            let issues = check_integrity(&conn)?;
            if !issues.is_empty() {
                return Err(StoreError::Corrupt(issues));
            }
        }
        let max_ms = durable_max_fact_stamp(&conn);
        let writer = WriterService::spawn_with_limits(conn, queue_capacity, enqueue_timeout)?;
        Ok(Self::finish_open_with_writer(
            root,
            writer,
            AtomicU64::new(max_ms.max(now_ms()).max(0) as u64),
        ))
    }

    pub(crate) fn finish_open(root: PathBuf, conn: Connection) -> StoreResult<Self> {
        // Seed the fact sequence above every durable row (a burst that
        // crashed in the same millisecond as a write must not let the next
        // stamp tie an existing row) and above the wall clock (a machine
        // clock stepped backwards must not re-enter old order positions).
        // BOTH fact tables participate: workspace-scoped rows carry the same
        // monotone stamp contract and are paged by it after a restart.
        let max_ms = durable_max_fact_stamp(&conn);
        Ok(Self::finish_open_with_writer(
            root,
            WriterService::spawn(conn)?,
            AtomicU64::new(max_ms.max(now_ms()).max(0) as u64),
        ))
    }

    /// [`Store::finish_open`] over an already-spawned writer service
    /// (adversarial tests configure the queue bounds through
    /// [`Store::open_with_writer_limits`]).
    pub(crate) fn finish_open_with_writer(
        root: PathBuf,
        writer: WriterService,
        fact_seq: AtomicU64,
    ) -> Self {
        let pool = Arc::new(ReaderPool::new());
        let storage_governor = faktor_storage::StorageGovernor::discover(&root).map(Arc::new);
        if let Some(governor) = &storage_governor {
            writer.attach_storage_governor(governor.clone());
        }
        Self {
            root,
            writer,
            pool,
            seam: Arc::new(CrashSeam::default()),
            generation: mint_boot_generation(),
            fact_seq,
            corruption_epoch: AtomicU64::new(0),
            outcome_revision: AtomicU64::new(0),
            maintenance: Arc::new(MaintenanceRegistry::new()),
            storage_governor: std::sync::Mutex::new(storage_governor),
        }
    }

    /// Monotonic epoch of direct-SQL/purge mutations (see the field docs).
    /// Session-open verification caches key on this value.
    pub fn corruption_epoch(&self) -> u64 {
        self.corruption_epoch.load(Ordering::Acquire)
    }

    /// Attach the always-on governor explicitly (embedded hosts/tests).
    pub fn attach_storage_governor(&self, governor: Arc<faktor_storage::StorageGovernor>) {
        self.writer.attach_storage_governor(governor.clone());
        *self.storage_governor.lock().unwrap() = Some(governor);
    }

    pub fn storage_governor(&self) -> Option<Arc<faktor_storage::StorageGovernor>> {
        self.storage_governor.lock().unwrap().clone()
    }

    /// Refuse an append that would breach the SQLite/WAL class quota or the
    /// aggregate budget. WAL maintenance (checkpoint/truncate) runs first;
    /// the caller must not execute the write on `Err`.
    pub fn admit_storage_growth(&self, incoming_bytes: u64) -> StoreResult<()> {
        if let Some(governor) = self.storage_governor() {
            governor
                .admit_sqlite_growth(incoming_bytes)
                .map_err(|e| StoreError::StorageRefused(e.to_string()))?;
        }
        Ok(())
    }

    /// Checkpoint/truncate every discovered WAL and enforce the SQLite class
    /// quota. Returns the bytes reclaimed.
    pub fn storage_maintenance(&self) -> StoreResult<u64> {
        match self.storage_governor() {
            Some(governor) => governor
                .sqlite_maintenance()
                .map_err(|e| StoreError::StorageRefused(e.to_string())),
            None => Ok(0),
        }
    }

    /// Monotonic revision of the Engineering Outcome Ledger (engineering
    /// rows + settlement events). Read-side metric folds cache by this.
    pub fn outcome_revision(&self) -> u64 {
        self.outcome_revision.load(Ordering::Acquire)
    }

    /// This store instance's boot-instance identity (audit P1). A `pending`
    /// admission claim whose stored `owner_generation` differs from this
    /// value belongs to a DEAD previous boot and is resolved by startup
    /// recovery, never served as a live in-flight claim.
    pub fn generation(&self) -> &str {
        &self.generation
    }

    /// The next monotonic memory-fact stamp: the wall clock when it is
    /// ahead of every issued stamp (idle catch-up keeps stamps honest wall
    /// times), otherwise exactly one past the last issued stamp (bursts
    /// inside one millisecond stay strictly ordered).
    pub(crate) fn fact_timestamp(&self) -> i64 {
        let now = now_ms().max(0) as u64;
        let prev = self.fact_seq.load(Ordering::Relaxed);
        let next = now.max(prev.saturating_add(1));
        self.fact_seq.store(next, Ordering::Relaxed);
        next as i64
    }

    /// Arm this store instance's deterministic crash seam (fault
    /// certification only; see [`CrashSeam`]). Inert when never armed.
    #[cfg(any(test, feature = "fault-injection"))]
    #[doc(hidden)]
    pub fn crash_arm(&self, arm: CrashArm) {
        self.seam.arm(arm);
    }

    /// True when an armed [`CrashSeam`] boundary was observed for `caught`:
    /// either the caller unwound, or the seam panic fired inside a writer job
    /// (between the side-row write and the event write, still inside the
    /// transaction) and the durable authority stopped admitting mutations —
    /// the panic is contained at the [`WriterService`] boundary and surfaced
    /// as the typed `StoreError::WriterUnavailable`. Seam-atomicity tests
    /// assert this BEFORE reopening the store, so a boundary that never fired
    /// can never be mistaken for a clean old/new world.
    #[cfg(any(test, feature = "fault-injection"))]
    #[doc(hidden)]
    pub fn seam_crash_observed<T>(&self, caught: &std::thread::Result<T>) -> bool {
        caught.is_err() || !self.writer_available()
    }

    pub fn path(&self) -> PathBuf {
        self.root.join("faktor-plus.db")
    }

    /// Deliberate synchronous access to the shared store, coexisting with a
    /// `DbActor` (faktor-session) that batches the hot append paths through
    /// [`Store::batch_hot_writes`]. All reads and every non-hot write
    /// (compound transitions, queue ops, checkpoints, tool runs, recovery)
    /// go through this surface and share the same single-owner writer
    /// service + reader pool, so direct and actor writes never corrupt each
    /// other. Read-your-write
    /// across the two surfaces is only guaranteed once the actor response
    /// (post-fsync) has been observed.
    pub fn direct(&self) -> &Store {
        self
    }

    /// Typed writer-service instrumentation snapshot: queue wait,
    /// transaction duration, pending depth, slow transactions, checkpoint
    /// duration and durable-authority health. Cheap (atomics).
    pub fn writer_telemetry(&self) -> WriterTelemetry {
        self.writer.telemetry()
    }

    /// Drain the writer service's per-job receipts (audit-5 additive
    /// telemetry): the label, queue wait and execution duration of every
    /// command the owner thread completed since the last drain, oldest first.
    /// This is the honest per-command accounting — a caller never has to
    /// difference global cumulative counters (which mix every other thread's
    /// jobs into the same interval). Bounded ring: at most
    /// [`WRITER_RECEIPT_CAPACITY`] receipts are retained and any eviction is
    /// counted in [`WriterTelemetry::receipts_dropped`].
    pub fn take_writer_receipts(&self) -> Vec<WriterJobReceipt> {
        self.writer.take_receipts()
    }

    /// Typed reader-pool instrumentation snapshot (audit-5 additive
    /// surface): reads completed, cumulative/peak permit wait, and how many
    /// per-read receipts the bounded ring had to evict. The permit wait is
    /// the time `read()` blocks on the pool semaphore — it is NOT the
    /// caller's end-to-end duration (`messages_page()` also pays query, JSON
    /// and part-load time and must be measured by the caller).
    pub fn reader_telemetry(&self) -> ReaderTelemetry {
        ReaderTelemetry {
            reads: self.pool.reads.load(Ordering::Relaxed),
            permit_wait_total_ns: self.pool.permit_wait_total_ns.load(Ordering::Relaxed),
            permit_wait_max_ns: self.pool.permit_wait_max_ns.load(Ordering::Relaxed),
            receipts_dropped: self.pool.receipts_dropped.load(Ordering::Relaxed),
        }
    }

    /// Drain one permit-wait receipt per read completed since the last drain,
    /// oldest first. Bounded by the reader receipt ring; detect evictions
    /// through [`ReaderTelemetry::receipts_dropped`].
    pub fn take_reader_receipts(&self) -> Vec<ReaderPermitReceipt> {
        let mut ring = self.pool.receipts.lock().unwrap_or_else(|p| p.into_inner());
        ring.drain(..)
            .map(|permit_wait_ns| ReaderPermitReceipt { permit_wait_ns })
            .collect()
    }

    /// Run a raw closure on the single writer owner. `#[doc(hidden)]` test
    /// seam (the same pattern as [`Store::crash_arm`]): adversarial tests
    /// inject slow transactions and deliberate panics to certify the
    /// service's queue semantics and poisoning policy. The closure must only
    /// touch SQL state; production code never calls this.
    #[doc(hidden)]
    pub fn writer_debug_job<F>(&self, label: &'static str, f: F) -> StoreResult<()>
    where
        F: FnOnce(&mut Connection) + Send + 'static,
    {
        self.writer.execute_raw(label, f)
    }

    /// True while the durable writer authority still admits mutations.
    /// `#[doc(hidden)]` test/fault probe.
    #[doc(hidden)]
    pub fn writer_available(&self) -> bool {
        self.writer.is_available()
    }

    /// The typed reason the writer stopped admitting mutations, when it did.
    /// `#[doc(hidden)]` test/fault probe.
    #[doc(hidden)]
    pub fn writer_unavailable_reason(&self) -> Option<String> {
        self.writer.unavailable_reason()
    }

    /// Bounded, idempotent store shutdown (audit items 6/7):
    ///
    /// 1. every active maintenance task (online backup) is CANCELLED — its
    ///    token is set and its connection interrupted — and the registry is
    ///    given the remaining time (bounded) to drain;
    /// 2. the writer stops admissions, DRAINS the queued mutations FIFO
    ///    within the remaining bound, and on expiry interrupts the in-flight
    ///    statement and REJECTS whatever is still queued with the typed
    ///    [`StoreError::WriterUnavailable`];
    /// 3. the owner thread is joined within the finite bound; a thread that
    ///    survives the interrupt grace is detached and this returns
    ///    [`StoreError::WriterShutdownTimeout`] (it exits on its own — never
    ///    an unbounded join).
    ///
    /// SUCCESS CONDITION: `Ok` means EVERY store-owned authority is quiescent
    /// — the maintenance registry drained to empty AND the writer owner
    /// stopped within the bound. If maintenance did not drain in time this
    /// returns [`StoreError::MaintenanceShutdownTimeout`] carrying the writer
    /// outcome (even when the writer drained), so a surviving maintenance
    /// thread is never reported as a successful shutdown. Only when
    /// maintenance drained does a detached writer surface as
    /// [`StoreError::WriterShutdownTimeout`].
    ///
    /// Always returns within `timeout + WRITER_SHUTDOWN_INTERRUPT_GRACE`.
    #[doc(hidden)]
    pub fn shutdown(&self, timeout: Duration) -> StoreResult<WriterShutdownOutcome> {
        let deadline = Instant::now() + timeout;
        // Stop admissions FIRST: while maintenance winds down, no new
        // mutation may be admitted.
        self.writer.begin_shutdown();
        // Close the maintenance registry BEFORE cancelling: a task that tries
        // to register in the writer-drain window cannot slip past the cancel
        // sweep and let this report success with a live store-owned thread.
        self.maintenance.close();
        let cancelled = self.maintenance.cancel_all();
        if cancelled > 0 {
            tracing::info!(
                target: "faktor_store::maintenance",
                cancelled,
                "cancelling active maintenance tasks for shutdown"
            );
        }
        let maintenance_drained = self.maintenance.wait_idle(deadline);
        let remaining = deadline.saturating_duration_since(Instant::now());
        let outcome = self.writer.shutdown(remaining);
        if !maintenance_drained {
            let active = self.maintenance.active_count();
            tracing::warn!(
                target: "faktor_store::maintenance",
                active,
                ?outcome,
                "maintenance did not drain inside the shutdown bound; its caller \
                 still receives the typed cancellation and shutdown is NOT a success"
            );
            return Err(StoreError::MaintenanceShutdownTimeout {
                active,
                writer_outcome: outcome,
            });
        }
        match outcome {
            WriterShutdownOutcome::Detached { rejected } => {
                Err(StoreError::WriterShutdownTimeout { rejected })
            }
            other => Ok(other),
        }
    }

    /// Active maintenance tasks (online backups) right now. `#[doc(hidden)]`
    /// test probe: proves a backup runs off the mutation owner.
    #[doc(hidden)]
    pub fn maintenance_active_count(&self) -> usize {
        self.maintenance.active_count()
    }

    /// True once the writer owner thread recorded its stop. `#[doc(hidden)]`
    /// test probe for bounded shutdown.
    #[doc(hidden)]
    pub fn writer_stopped(&self) -> bool {
        self.writer.is_stopped()
    }

    /// Borrow a read connection. A semaphore permit is acquired first, so at
    /// most `READER_POOL` connections exist concurrently: 20 simultaneous
    /// readers use at most 4 connections and the rest wait on the permit,
    /// bounded by the busy timeout (`StoreError::Busy`). The permit wait is
    /// recorded separately from the caller's query time (see
    /// [`Store::reader_telemetry`]).
    pub(crate) fn read(&self) -> StoreResult<ReadConn> {
        let queued_at = Instant::now();
        let permit = self
            .pool
            .sem
            .acquire_timeout(Instant::now() + BUSY_TIMEOUT)?;
        self.pool.record_read(queued_at.elapsed());
        // Ephemeral connection cache: recover on poison (see Semaphore).
        let mut conns = self.pool.conns.lock().unwrap_or_else(|p| p.into_inner());
        let conn = match conns.pop() {
            Some(c) => c,
            None => {
                // WAL-correct readers: a SQLITE_OPEN_READ_ONLY connection may
                // read only the main database file (a stale snapshot) when it
                // cannot access the -shm/-wal sidecar; pooled readers then
                // serve rows that predate every recent commit. Opening
                // read-write (no CREATE) guarantees the reader participates in
                // WAL snapshotting, so a pooled connection always sees the
                // latest committed append (audit 42 regressions: SSE streams
                // polling the journal went blind mid-session).
                let c = Connection::open_with_flags(
                    self.path(),
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                        | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )?;
                configure(&c)?;
                self.pool.created.fetch_add(1, Ordering::Relaxed);
                c
            }
        };
        Ok(ReadConn {
            conn: Some(conn),
            pool: self.pool.clone(),
            _permit: permit,
        })
    }

    /// Raw test-only connection for adversarial fixture manipulation
    /// (corrupt rows, illegal transitions, forged timestamps). Tests
    /// deliberately bypass the writer service; production has no such
    /// surface. SQLite's WAL locking still serializes against the owner
    /// connection, so a test that races its own writer service observes the
    /// engine's ordering, not a store-level lock.
    #[cfg(test)]
    pub(crate) fn raw_conn(&self) -> Connection {
        let conn = Connection::open(self.path()).expect("open raw test connection");
        configure(&conn).expect("configure raw test connection");
        conn
    }

    /// Idle connections currently in the pool; at most `READER_POOL`.
    /// Test probe.
    #[cfg(test)]
    pub(crate) fn reader_pool_len(&self) -> usize {
        self.pool
            .conns
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .len()
    }

    /// Read connections ever opened since `open`; with the semaphore this
    /// never exceeds `READER_POOL` even under heavy contention.
    /// Test probe.
    #[cfg(test)]
    pub(crate) fn connections_created(&self) -> u64 {
        self.pool.created.load(Ordering::Relaxed)
    }

    // ---------------------------------------------------------------- workspaces

    pub fn create_workspace(&self, root: &str) -> StoreResult<WorkspaceId> {
        let root = root.to_owned();
        let root_label = root.clone();
        // Preparation BEFORE enqueueing: the timestamp is captured on the
        // caller's thread (audit item 8) — the writer job executes SQL only.
        let created_ms = now_ms();
        let id = self.writer.execute("create_workspace", move |conn| {
            conn.execute(
                "INSERT OR IGNORE INTO workspace(root, created_ms) VALUES (?1, ?2)",
                params![root, created_ms],
            )?;
            let id: i64 = conn.query_row(
                "SELECT id FROM workspace WHERE root = ?1",
                params![root],
                |r| r.get(0),
            )?;
            Ok(id)
        })?;
        id_field(&format!("workspace id {id} (root {root_label:?})"), id)
    }

    /// The recorded root path of a workspace; `None` when the workspace id is
    /// unknown (the revert/diff wire surface needs the on-disk root to open
    /// the file service handle).
    pub fn workspace_root(&self, id: WorkspaceId) -> StoreResult<Option<String>> {
        let conn = self.read()?;
        let out = conn
            .query_row(
                "SELECT root FROM workspace WHERE id = ?1",
                params![id.raw() as i64],
                |r| r.get(0),
            )
            .optional()?;
        Ok(out)
    }

    // ---------------------------------------------------------------- sessions

    /// Execute a FIFO group of hot writes as ONE SQLite transaction with ONE
    /// commit fsync (`PRAGMA synchronous = FULL` for the group; the
    /// connection's configured `NORMAL` is restored before returning). Each
    /// write runs in its own savepoint, so a failing write (duplicate
    /// `(session, seq)` message, FK violation, ...) rolls back only itself:
    /// the rest of the group still commits, and per-write results report
    /// their individual error. Responses may only be delivered after this
    /// returns, which is exactly what makes an actor ack mean "durable".
    ///
    /// Ordering is the caller's FIFO: writes execute in slice order, which
    /// preserves per-session causal order for interleaved event/message/
    /// usage streams as long as the caller enqueues causally.
    ///
    /// The returned [`BatchTiming`] splits the SQL work from the deliberate
    /// commit fsync so the actor's 5 ms instrumentation gate can count the
    /// work segments (what used to stall Tokio workers) while fsync waits —
    /// which no worker ever performs — stay visible as caller-side queue
    /// latency instead of being misattributed to SQLite work.
    pub fn batch_hot_writes(
        &self,
        writes: &[HotWrite],
    ) -> StoreResult<(Vec<StoreResult<HotWriteOutcome>>, BatchTiming)> {
        if writes.is_empty() {
            return Ok((Vec::new(), BatchTiming::default()));
        }
        // Admission happens BEFORE any writer enqueue/transaction: a batch
        // that cannot fit the SQLite/WAL budget is refused whole.
        self.admit_storage_growth((writes.len() as u64).saturating_mul(4_096))?;
        let seam = Arc::clone(&self.seam);
        // Preparation BEFORE enqueueing: every JSON body of the batch is
        // serialized here (caller's thread); the writer owner executes
        // statements only.
        let writes: Vec<PreparedHotWrite> = writes.iter().map(PreparedHotWrite::prepare).collect();
        self.writer.execute("batch_hot_writes", move |conn| {
            // Acknowledged appends must survive a process kill: force the WAL
            // commit fsync for the whole group (the actor replies only after
            // this method returns). Restored to the configured NORMAL on drop.
            let _strong = StrongSync::on(conn)?;
            let work_start = Instant::now();
            let run = (|| {
                conn.execute_batch("BEGIN IMMEDIATE")?;
                let mut out = Vec::with_capacity(writes.len());
                for w in writes {
                    // Per-write savepoint: one failing write must not roll the
                    // whole group back (a duplicate message seq on one session
                    // must not lose another session's parts).
                    conn.execute_batch("SAVEPOINT hot_write")?;
                    let r = match Self::hot_write_on(conn, &w) {
                        Ok(o) => {
                            conn.execute_batch("RELEASE hot_write")?;
                            Ok(o)
                        }
                        Err(e) => {
                            conn.execute_batch("ROLLBACK TO hot_write")?;
                            conn.execute_batch("RELEASE hot_write")?;
                            Err(e)
                        }
                    };
                    out.push(r);
                    // Durability boundary inside the group: crash right after
                    // write `out.len()` executed (its savepoint released) but
                    // before the group COMMIT — the whole group rolls back.
                    seam.trip("flush_progress");
                }
                let commit_start = Instant::now();
                let work_us = commit_start
                    .duration_since(work_start)
                    .as_micros()
                    .min(u64::MAX as u128) as u64;
                // Durability boundary: crash after every write executed, before
                // the fsynced COMMIT (the whole actor flush rolls back).
                seam.trip("flush_precommit");
                conn.execute_batch("COMMIT")?;
                // Crash right after the COMMIT fsync: the whole flush is
                // durable; the caller's ack was lost.
                seam.trip("flush_committed");
                Ok((
                    out,
                    BatchTiming {
                        work_us,
                        commit_us: commit_start.elapsed().as_micros().min(u64::MAX as u128) as u64,
                    },
                ))
            })();
            match run {
                Ok(out) => Ok(out),
                Err(e) => {
                    // Never leave the writer connection inside a transaction.
                    let _ = conn.execute_batch("ROLLBACK");
                    Err(e)
                }
            }
        })
    }

    /// Run a bounded PASSIVE WAL checkpoint on the writer owner's connection.
    ///
    /// Maintenance only: [`configure`] disables SQLite's own
    /// `wal_autocheckpoint`, so checkpoint work is scheduled by the caller
    /// (the session `DbActor` runs it from its idle flush tick, after the
    /// queue drained) and can never execute inside an interactive batch
    /// segment. `PASSIVE` folds every frame no reader pins, never waits for
    /// readers, never restarts the WAL, and returns the same typed
    /// [`StoreError`] surface as every other call: a failure is the caller's
    /// to log, never a corruption by itself.
    pub fn wal_checkpoint_passive(&self) -> StoreResult<()> {
        // The always-on governor owns WAL bounds: truncate when the class is
        // over quota, then fall through to the ordinary passive fold.
        let _ = self.storage_maintenance();
        // Instrumentation: the checkpoint's duration is measured on the
        // writer owner (the only place it runs) and recorded even when the
        // pragma itself fails.
        let (result, elapsed) = self
            .writer
            .execute_raw("wal_checkpoint_passive", move |conn| {
                let started = Instant::now();
                let result = conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");
                (result, started.elapsed())
            })?;
        self.writer.record_checkpoint(elapsed);
        result?;
        Ok(())
    }

    /// Bound the WAL FILE after an idle PASSIVE checkpoint folded its frames.
    ///
    /// PASSIVE never resets/truncates the WAL, so a long burst leaves the
    /// on-disk `-wal` sidecar at its high-water mark (the 100k-fact probe
    /// measured ~2.9 GB even though every frame was folded). When the file
    /// exceeds [`WAL_TRUNCATE_THRESHOLD_BYTES`] this runs a TRUNCATE
    /// checkpoint with `busy_timeout = 0`: if any reader pins an older frame
    /// SQLite returns BUSY immediately and the file is left intact (the next
    /// idle tick retries). The writer connection's normal 5 s timeout is
    /// restored on every path, so an interactive batch can never inherit it.
    ///
    /// Returns `true` when a truncate attempt ran, `false` when the file is
    /// below the bound or a reader is currently pinning frames.
    pub fn wal_checkpoint_truncate_bounded(&self) -> StoreResult<bool> {
        let wal = self.root.join("faktor-plus.db-wal");
        let size = std::fs::metadata(&wal).map(|m| m.len()).unwrap_or(0);
        if size <= WAL_TRUNCATE_THRESHOLD_BYTES {
            return Ok(false);
        }
        let outcome = self
            .writer
            .execute_raw("wal_checkpoint_truncate_bounded", move |conn| {
                conn.execute_batch("PRAGMA busy_timeout = 0;")?;
                let result = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);");
                let restore = conn.execute_batch("PRAGMA busy_timeout = 30000;");
                restore?;
                result
            })?;
        match outcome {
            Ok(()) => Ok(true),
            // A reader pinning frames is the documented non-blocking refusal:
            // nothing was damaged, the file simply stays until the next tick.
            Err(e) if is_locked_error(&e) => Ok(false),
            Err(e) => Err(StoreError::Sqlite(e)),
        }
    }

    pub(crate) fn hot_write_on(
        conn: &Connection,
        w: &PreparedHotWrite,
    ) -> StoreResult<HotWriteOutcome> {
        match w {
            PreparedHotWrite::AppendEvent {
                session_id,
                op_id,
                kind,
                state_json,
                ts_ms,
                payload_json,
                payload_ver,
                seq_messages,
            } => Self::insert_event_locked(
                conn,
                *session_id,
                *op_id,
                *kind,
                state_json,
                *ts_ms,
                payload_json.clone(),
                *payload_ver,
                seq_messages,
            )
            .map(HotWriteOutcome::EventSeq),
            PreparedHotWrite::PutMessage {
                session_id,
                seq,
                role,
                data_json,
                created_ms,
            } => Self::insert_message_on(conn, *session_id, *seq, role, data_json, *created_ms)
                .map(HotWriteOutcome::RowId),
            PreparedHotWrite::PutPart {
                message_id,
                kind,
                data_json,
                created_ms,
            } => Self::insert_part_on(conn, *message_id, kind, data_json, *created_ms)
                .map(HotWriteOutcome::RowId),
            PreparedHotWrite::RecordProviderCall {
                session_id,
                op_id,
                provider,
                model,
                status,
                tokens_in,
                tokens_out,
                error,
                ts_ms,
            } => Self::insert_provider_call_on(
                conn,
                *session_id,
                *op_id,
                provider,
                model,
                status,
                *tokens_in,
                *tokens_out,
                error.as_deref(),
                None,
                None,
                None,
                // Hot-write rows predate the v19 segment observation: no
                // per-call segments, the binary prefix rule stays.
                None,
                // Hot-write rows predate the v18 attempt surface: no
                // attempt identity, no reservation link (the actor is
                // migrated in the agent stream-loop wave).
                None,
                None,
                None,
                None,
                *ts_ms,
            )
            .map(HotWriteOutcome::RowId),
        }
    }

    /// Raw-SQL seam (adversarial tests + crash forensics only): executes one
    /// SQL batch on the writer owner's connection. Deliberately NOT
    /// cfg(test)-gated so downstream crate tests (faktor-session's typed
    /// ledger corruption tests) can craft corrupt rows; using it in
    /// production is equivalent to corrupting the database yourself.
    #[doc(hidden)]
    pub fn sql_execute(&self, sql: &str) -> StoreResult<()> {
        let sql = sql.to_owned();
        let result = self.writer.execute("sql_execute", move |conn| {
            conn.execute_batch(&sql)?;
            Ok(())
        });
        // Direct SQL can break any journal/ledger invariant. Bump even on a
        // partial batch failure: the typed writers are the only mutations
        // allowed to leave the open-verification cache intact.
        self.corruption_epoch.fetch_add(1, Ordering::Release);
        result
    }

    // ------------------------------------------------------- typed session ledger
    // (audits 27 / 71-72, schema v11: the append-only typed ledger plus its
    // materialized head checkpoint. Compaction policy — the never-FIFO-evict
    // watermark — lives in faktor-session; the store executes one atomic
    // delete+head-rewrite transaction.)

    pub fn integrity_check(&self) -> StoreResult<Vec<String>> {
        let conn = self.read()?;
        let out = check_integrity(&conn)?;
        Ok(out)
    }

    /// The FULL deep scan (`doctor --deep`, crash forensics): the complete
    /// `PRAGMA integrity_check` over the live store. Production starts use
    /// the bounded [`Store::open_fast`]/[`Store::quick_integrity_check`]
    /// path instead.
    pub fn deep_integrity_check(&self) -> StoreResult<Vec<String>> {
        self.integrity_check()
    }

    /// Bounded live-store check (plain `doctor`, post-open validation): the
    /// same `PRAGMA quick_check` the fast open runs. Detects damaged pages
    /// but skips the full scan's index-content re-verification.
    pub fn quick_integrity_check(&self) -> StoreResult<Vec<String>> {
        let conn = self.read()?;
        let out = check_quick(&conn)?;
        Ok(out)
    }

    /// Online backup via the SQLite backup API (safe while the daemon runs).
    ///
    /// Audit item 6: the copy NEVER runs on the mutation-owner connection. It
    /// opens a dedicated read/snapshot connection (read-write without CREATE,
    /// like the pooled readers, so it participates in WAL snapshotting) and
    /// runs the page-batched copy on a dedicated maintenance thread. Writers
    /// keep committing through the single writer owner while the snapshot is
    /// copied, so a large backup cannot stall any domain's mutation queue.
    /// The call blocks the CALLER until the snapshot completes — schedule it
    /// on a blocking pool, never a runtime worker.
    ///
    /// Cancellation (bounded shutdown): the maintenance thread checks its
    /// token before every page batch, so `Store::shutdown`/drop cancels a
    /// running backup and this call returns the typed
    /// [`StoreError::Maintenance`] instead of hanging.
    pub fn backup_to(&self, dest: &Path) -> StoreResult<()> {
        let dest = dest.to_path_buf();
        // The snapshot connection is opened on the caller's thread (the CLI
        // backup task already runs on a blocking pool); `configure` plus the
        // read-write/no-CREATE flags make it a WAL-participating reader.
        let src = Connection::open_with_flags(
            self.path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        configure(&src)?;
        let cancel = Arc::new(AtomicBool::new(false));
        let interrupt = src.get_interrupt_handle();
        let registry = Arc::clone(&self.maintenance);
        let task_registry = Arc::clone(&registry);
        let Some(id) = registry.register(Arc::clone(&cancel), interrupt) else {
            return Err(StoreError::Maintenance(
                "store is shutting down; backup refused before any page was copied".into(),
            ));
        };
        let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("faktor-store-backup".into())
            .spawn(move || {
                let result = run_backup(src, &dest, &cancel);
                task_registry.unregister(id);
                // The caller may have gone away with the store; a failed send
                // is not an error here.
                let _ = result_tx.send(result);
            })
            .map_err(|e| {
                registry.unregister(id);
                StoreError::Maintenance(format!("failed to spawn the backup thread: {e}"))
            })?;
        match result_rx.recv() {
            Ok(result) => result,
            Err(_) => Err(StoreError::Maintenance(
                "backup maintenance thread terminated without a result".into(),
            )),
        }
    }

    /// `doctor`-style diagnostic with the FULL integrity scan (`doctor
    /// --deep` depth; kept for legacy callers such as the session manager's
    /// `integrity_report`).
    pub fn diagnostics(&self) -> StoreResult<serde_json::Value> {
        self.diagnostics_with(check_integrity)
    }

    /// `doctor`-style diagnostic with the BOUNDED quick check (plain
    /// `doctor`, matching the fast open).
    pub fn diagnostics_quick(&self) -> StoreResult<serde_json::Value> {
        self.diagnostics_with(check_quick)
    }

    pub(crate) fn diagnostics_with(
        &self,
        integrity: fn(&Connection) -> StoreResult<Vec<String>>,
    ) -> StoreResult<serde_json::Value> {
        let conn = self.read()?;
        let sessions: i64 = conn.query_row("SELECT COUNT(*) FROM session", [], |r| r.get(0))?;
        let events: i64 = conn.query_row("SELECT COUNT(*) FROM event", [], |r| r.get(0))?;
        let messages: i64 = conn.query_row("SELECT COUNT(*) FROM message", [], |r| r.get(0))?;
        let journal_mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
        let integrity = integrity(&conn)?;
        Ok(serde_json::json!({
            "journal_mode": journal_mode,
            "sessions": sessions,
            "events": events,
            "messages": messages,
            "integrity": integrity,
        }))
    }

    // -------------------------------------------------- deep doctor queries
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn tmp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path(), true).unwrap();
        (dir, s)
    }

    #[test]
    fn corrupt_db_file_is_detected_on_open_and_never_reads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("faktor-plus.db");
        let garbage =
            b"this is not a sqlite database at all - definitely not valid magic header bytes";
        std::fs::write(&db, garbage).unwrap();
        // Catches a recovery that reinitializes hostile bytes or a lazy open
        // whose failed read is reported as an empty healthy store.
        match Store::open(dir.path(), true) {
            Err(StoreError::Sqlite(err)) => {
                assert!(err.to_string().contains("not a database"), "{err}");
            }
            other => panic!("corrupt db must be a typed Sqlite refusal, got {other:?}"),
        }
        assert_eq!(std::fs::read(&db).unwrap(), garbage);
        if let Ok(store) = Store::open(dir.path(), false) {
            let read = store.list_sessions(None);
            assert!(
                matches!(
                    read,
                    Err(StoreError::Sqlite(_)) | Err(StoreError::Corrupt(_))
                ),
                "a corrupt database must never read as an empty store: {read:?}"
            );
        }
    }

    #[test]
    fn crash_recovery_scanner_input_is_durable() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let session = store.create_session(ws, "t", "p", "m").unwrap();
        let op = OpId::new(77);
        store
            .start_tool_run(
                session.id,
                op,
                "write_file",
                serde_json::json!({"path": "/w/a.txt", "content": "x"}),
                serde_json::json!({"strategy": "verify_hash", "detail": {"path": "/w/a.txt", "expected": "ab".repeat(32)}}),
                Some("ab".repeat(32)),
                None,
            )
            .unwrap();
        // Crash: no finish. The scanner must find it with effect unknown.
        let pending = store.pending_tool_runs(session.id).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].op_id, op);
        assert_eq!(pending[0].effect_status, "unknown");
        assert_eq!(pending[0].status, "running");
        assert_eq!(
            pending[0].expected_hash.as_deref(),
            Some("ab".repeat(32).as_str())
        );
        // Finishing moves it out of the scanner set.
        store
            .finish_tool_run(session.id, op, "completed", "verified")
            .unwrap();
        assert!(store.pending_tool_runs(session.id).unwrap().is_empty());
        // finish on missing row is an error (loud, not silent)
        assert!(store
            .finish_tool_run(session.id, OpId::new(999), "completed", "verified")
            .is_err());
    }

    #[test]
    fn backup_restores_full_state() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        store
            .put_message(s.id, 1, "user", serde_json::json!({"text": "hi"}))
            .unwrap();
        let backup_path = dir.path().join("backup.db");
        store.backup_to(&backup_path).unwrap();
        // Reopen backup as a store; data must be complete.
        let restored_dir = tempfile::tempdir().unwrap();
        std::fs::copy(&backup_path, restored_dir.path().join("faktor-plus.db")).unwrap();
        let restored = Store::open(restored_dir.path(), true).unwrap();
        assert_eq!(restored.message_count(s.id).unwrap(), 1);
        assert_eq!(
            restored.messages_before(s.id, None, 10).unwrap()[0].data["text"],
            "hi"
        );
    }

    /// Seed `rows` padded `part` rows for one message through the writer
    /// (prepared statement, one transaction) so a backup has a page-dense
    /// source that takes real time to copy.
    pub(crate) fn seed_padded_parts(store: &Store, message_id: i64, rows: i64) {
        let padding = "x".repeat(200);
        store
            .sql_execute(&format!(
                "WITH RECURSIVE cnt(x) AS (
                     SELECT 1 UNION ALL SELECT x + 1 FROM cnt WHERE x < {rows}
                 )
                 INSERT INTO part(message_id, kind, data, created_ms)
                 SELECT {message_id}, 'text', '{{\"text\":\"{padding}\"}}', 1 FROM cnt;"
            ))
            .unwrap();
    }

    /// Audit item 6: `backup_to` must not hold the single writer owner for the
    /// whole copy. The snapshot runs on a dedicated maintenance thread with
    /// its own read/snapshot connection; continuous mutations keep committing
    /// through the single writer owner while it copies, so the writer-queue
    /// latency stays bounded (p95 asserted) instead of stalling for the whole
    /// backup. The resulting snapshot verifies.
    #[test]
    fn backup_overlaps_continuous_writes_without_stalling_the_writer_queue() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(Store::open(dir.path(), true).unwrap());
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let mid = store
            .put_message(s.id, 1, "user", serde_json::json!({"text": "seed"}))
            .unwrap();
        // ~200k padded part rows: the copy takes long enough (the page-batched
        // maintenance loop pauses between steps) for continuous writes to
        // overlap it.
        seed_padded_parts(&store, mid, 200_000);

        let backup_path = dir.path().join("backup.db");
        let backup_store = std::sync::Arc::clone(&store);
        let backup_path_for_thread = backup_path.clone();
        let backup = std::thread::spawn(move || backup_store.backup_to(&backup_path_for_thread));

        // Wait (bounded) until the maintenance thread is registered: the copy
        // runs OFF the mutation owner.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while store.maintenance_active_count() == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "backup maintenance thread never registered"
            );
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        // Continuous writes while the snapshot is copied: each one is
        // prepared on this thread, enqueued and committed by the owner.
        let mut latencies: Vec<std::time::Duration> = Vec::new();
        let mut overlapped = false;
        let mut i = 0i64;
        loop {
            let active = store.maintenance_active_count() > 0;
            overlapped |= active;
            if !active && latencies.len() >= 25 {
                break;
            }
            let started = std::time::Instant::now();
            store
                .append_ledger_entry(s.id, "backup_overlap", 1, serde_json::json!({"i": i}))
                .unwrap();
            latencies.push(started.elapsed());
            i += 1;
            // Bounded safety: never spin forever if the copy is stuck.
            assert!(i < 20_000, "backup never completed: {latencies:?}");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(
            overlapped,
            "the backup completed before any mutation overlapped it"
        );
        let mut sorted = latencies.clone();
        sorted.sort_unstable();
        let p95 = sorted[sorted.len() * 95 / 100];
        assert!(
            p95 < std::time::Duration::from_millis(250),
            "writer queue latency p95 {p95:?} exceeded the bound while a large \
             backup was copying (backup must not hold the writer owner); \
             latencies={latencies:?}"
        );
        backup.join().unwrap().unwrap();

        // The snapshot verifies: openable, intact, complete for the seed.
        let restored_dir = tempfile::tempdir().unwrap();
        std::fs::copy(&backup_path, restored_dir.path().join("faktor-plus.db")).unwrap();
        let restored = Store::open(restored_dir.path(), true).unwrap();
        assert_eq!(restored.message_count(s.id).unwrap(), 1);
        assert!(restored.integrity_check().unwrap().is_empty());
    }

    #[test]
    fn integrity_check_survives_normal_use_and_flags_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let _s = store.create_session(ws, "t", "p", "m").unwrap();
        assert!(store.integrity_check().unwrap().is_empty());
        // Corrupt the DB file on disk behind the store's back: replace the
        // main file with garbage AND remove the WAL/shm sidecars so nothing
        // can paper over the corruption. A lazy reopen must either refuse to
        // open or flag the corruption on the next integrity check — never
        // silently serve a fake store.
        drop(store);
        let path = dir.path().join("faktor-plus.db");
        std::fs::write(
            &path,
            b"this file is complete garbage now, no sqlite magic header at all - 1234567890",
        )
        .unwrap();
        let _ = std::fs::remove_file(dir.path().join("faktor-plus.db-wal"));
        let _ = std::fs::remove_file(dir.path().join("faktor-plus.db-shm"));
        match Store::open(dir.path(), false) {
            Err(e) => {
                assert!(
                    matches!(e, StoreError::Sqlite(_) | StoreError::Corrupt(_)),
                    "corrupt db must fail cleanly, got {e:?}"
                );
            }
            Ok(reopened) => {
                let issues = reopened.integrity_check();
                assert!(
                    issues.is_err() || !issues.unwrap().is_empty(),
                    "corruption must surface as an error or flagged rows"
                );
            }
        }
    }

    #[test]
    fn diagnostics_reports_exact_durable_counts_and_never_hides_a_torn_journal() {
        let (d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        store
            .put_message(s.id, 1, "user", serde_json::json!({"text": "hi"}))
            .unwrap();
        let diag = store.diagnostics().unwrap();
        // Catches a doctor shim that reports zero/stale counts or a bogus
        // integrity verdict for a healthy durable state.
        assert_eq!(diag["journal_mode"], "wal");
        assert_eq!(diag["sessions"], 1);
        assert_eq!(diag["events"], 1);
        assert_eq!(diag["messages"], 1);
        assert_eq!(diag["integrity"], serde_json::json!([]));
        // Page integrity is NOT journal consistency: deleting the seed row
        // keeps pages clean but must be visible to the authority sweep.
        store
            .raw_conn()
            .execute(
                "DELETE FROM event WHERE session_id = ?1",
                params![s.id.raw() as i64],
            )
            .unwrap();
        let diag = store.diagnostics().unwrap();
        assert_eq!(diag["events"], 0);
        assert_eq!(diag["integrity"], serde_json::json!([]));
        assert_eq!(store.journal_consistency_issues().unwrap().len(), 1);
        drop(store);
        let reopened = Store::open(d.path(), true).unwrap();
        assert_eq!(reopened.diagnostics().unwrap()["sessions"], 1);
    }

    #[test]
    fn reader_pool_is_concurrency_bounded() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let store = std::sync::Arc::new(store);
        let mut handles = vec![];
        for _ in 0..20 {
            let store = store.clone();
            let sid = s.id;
            handles.push(std::thread::spawn(move || {
                for _ in 0..25 {
                    let conn = store.read().unwrap();
                    let n: i64 = conn
                        .query_row(
                            "SELECT COUNT(*) FROM session WHERE id = ?1",
                            params![sid.raw() as i64],
                            |r| r.get(0),
                        )
                        .unwrap();
                    assert_eq!(n, 1);
                    drop(conn);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        // 20 readers finished with the pool intact: retention never exceeds
        // the cap, and — the strong invariant the semaphore guarantees — the
        // number of connections ever opened never exceeds the cap either
        // (the old pool opened a new connection whenever it was empty).
        assert!(
            store.reader_pool_len() <= READER_POOL,
            "idle pool exceeds cap: {}",
            store.reader_pool_len()
        );
        assert!(
            store.connections_created() <= READER_POOL as u64,
            "connections created {} exceeds cap {}",
            store.connections_created(),
            READER_POOL
        );
    }

    #[test]
    fn reader_pool_waits_and_never_starves() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let _s = store.create_session(ws, "t", "p", "m").unwrap();
        let store = std::sync::Arc::new(store);
        // Hold all 4 permits, then prove a 5th reader waits (bounded) and
        // succeeds once a permit frees.
        let held: Vec<ReadConn> = (0..READER_POOL).map(|_| store.read().unwrap()).collect();
        let store2 = store.clone();
        let late = std::thread::spawn(move || {
            let conn = store2.read().unwrap(); // must block, then succeed
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM session", [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 1);
        });
        std::thread::sleep(Duration::from_millis(50));
        assert!(!late.is_finished(), "5th reader must wait for a permit");
        drop(held);
        late.join().unwrap();
    }

    /// Audit 5: the reader-pool permit wait is instrumented on its own axis
    /// (cumulative + bounded per-read receipts) and is deliberately NOT the
    /// caller's read duration: a read that waited for a permit reports the
    /// wait even though the caller's SQL finishes instantly afterwards.
    #[test]
    fn reader_permit_wait_is_instrumented_separately_from_call_duration() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let _s = store.create_session(ws, "t", "p", "m").unwrap();
        let store = std::sync::Arc::new(store);
        store.take_reader_receipts(); // discard setup reads
        let before = store.reader_telemetry();

        // Hold every permit; a late reader must wait for a permit (and the
        // wait is what gets recorded — its own query is trivial).
        let held: Vec<ReadConn> = (0..READER_POOL).map(|_| store.read().unwrap()).collect();
        let store2 = store.clone();
        let late = std::thread::spawn(move || {
            let conn = store2.read().unwrap();
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM session", [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 1);
        });
        std::thread::sleep(Duration::from_millis(80));
        drop(held);
        late.join().unwrap();

        let after = store.reader_telemetry();
        assert_eq!(after.reads, before.reads + READER_POOL as u64 + 1);
        assert!(
            after.permit_wait_max_ns >= Duration::from_millis(40).as_nanos() as u64,
            "the late reader's permit wait must be recorded: {after:?}"
        );
        assert_eq!(after.receipts_dropped, 0);
        let receipts = store.take_reader_receipts();
        assert_eq!(receipts.len(), READER_POOL + 1);
        assert!(
            receipts
                .iter()
                .any(|r| r.permit_wait_ns >= Duration::from_millis(40).as_nanos() as u64),
            "one receipt must carry the contended wait: {receipts:?}"
        );
        // Drain-once.
        assert!(store.take_reader_receipts().is_empty());
    }

    #[test]
    fn corrupt_state_row_returns_error_not_panic() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        {
            let conn = store.raw_conn();
            conn.execute(
                "UPDATE session SET state = ?1 WHERE id = ?2",
                params!["\"not_a_state\"", s.id.raw() as i64],
            )
            .unwrap();
        }
        match store.get_session(s.id) {
            Err(StoreError::Corrupt(_)) => {}
            other => panic!("corrupt session state must error, not panic: {other:?}"),
        }
        match store.list_sessions(Some(ws)) {
            Err(StoreError::Corrupt(_)) => {}
            other => panic!("corrupt session state must error in list too: {other:?}"),
        }
    }

    #[test]
    fn corrupt_payload_returns_error_not_panic() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        {
            let conn = store.raw_conn();
            conn.execute(
                "INSERT INTO event(seq, session_id, op_id, kind, state, ts_ms, payload)
                 VALUES (2, ?1, NULL, 'model_started', '\"streaming\"', 0, 'not json at all')",
                params![s.id.raw() as i64],
            )
            .unwrap();
        }
        match store.events_range(s.id, 1, None) {
            Err(StoreError::Corrupt(_)) => {}
            other => panic!("corrupt payload must error, not panic: {other:?}"),
        }
    }

    pub(crate) fn prefix_hash(seed: u8) -> [u8; 32] {
        [seed; 32]
    }

    #[test]
    fn corrupt_injected_prefix_shapes_are_loud_never_silent_misreads() {
        // (d) Values injected behind the API's back (a raw connection) must
        // be rejected LOUDLY on read: wrong-length hash, out-of-range
        // stability, negative and oversized token counts.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let row = store
            .record_provider_call_with_prefix(
                s.id,
                OpId::new(1),
                "p",
                "m",
                "completed",
                None,
                None,
                None,
                Some(prefix_hash(1)),
                Some(100),
                Some(0.5),
            )
            .unwrap();
        let conn = store.raw_conn();
        // Truncated hash blob (7 bytes).
        conn.execute(
            "UPDATE provider_call SET prompt_prefix_hash = ?1 WHERE id = ?2",
            params![vec![0xabu8; 7], row],
        )
        .unwrap();
        assert!(matches!(
            store.provider_call_prefix_rows(s.id),
            Err(StoreError::Malformed(_))
        ));
        // Stability beyond [0, 1].
        conn.execute(
            "UPDATE provider_call SET prompt_prefix_hash = ?1, prefix_stability = 1.5 WHERE id = ?2",
            params![vec![0xabu8; 32], row],
        )
        .unwrap();
        assert!(matches!(
            store.provider_call_prefix_rows(s.id),
            Err(StoreError::Malformed(_))
        ));
        // Negative token count.
        conn.execute(
            "UPDATE provider_call SET prompt_tokens = -4 WHERE id = ?1",
            params![row],
        )
        .unwrap();
        assert!(matches!(
            store.provider_call_prefix_rows(s.id),
            Err(StoreError::Malformed(_))
        ));
        // Token count beyond the u32 bound (2^40).
        conn.execute(
            "UPDATE provider_call SET prompt_tokens = ?1 WHERE id = ?2",
            params![1i64 << 40, row],
        )
        .unwrap();
        assert!(matches!(
            store.provider_call_prefix_rows(s.id),
            Err(StoreError::Malformed(_))
        ));
        // The aggregate query ignores corrupt rows' hashes (never parses
        // them) but the out-of-range stability it DOES aggregate must be
        // rejected there too.
        conn.execute(
            "UPDATE provider_call SET prompt_prefix_hash = ?1, prompt_tokens = 100 WHERE id = ?2",
            params![vec![0xabu8; 32], row],
        )
        .unwrap();
        assert!(matches!(
            store.session_stored_prefix_stability(s.id),
            Err(StoreError::Malformed(_))
        ));
    }

    // ---- per-call segment observations (v19, audits 45/82) ----

    /// One strict observation payload: `n` distinct 64-char hex digests and
    /// one token count per segment.
    pub(crate) fn segments_json(n: usize, tokens: &[u64]) -> String {
        let hashes: Vec<String> = (0..n)
            .map(|i| format!("{:02x}", i as u8).repeat(32))
            .collect();
        serde_json::json!({
            "segment_hashes": hashes,
            "segment_token_counts": tokens,
            "cache_read_tokens": 7u64,
        })
        .to_string()
    }

    #[test]
    fn corrupt_injected_segment_payloads_fail_loud_on_read_not_silent() {
        // (v19 adversarial) A payload injected behind the API's back (raw
        // connection UPDATE) must fail the READ with a typed error: routing
        // must never silently fall back to a guessed observation.
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let row = store
            .record_provider_call_with_prefix_segments(
                s.id,
                OpId::new(1),
                "p",
                "m",
                "completed",
                None,
                None,
                None,
                Some(prefix_hash(1)),
                Some(100),
                Some(0.5),
                Some(&segments_json(2, &[10, 20])),
            )
            .unwrap();
        let conn = store.raw_conn();
        let corrupt = |json: &str| {
            conn.execute(
                "UPDATE provider_call SET prefix_segments_json = ?1 WHERE id = ?2",
                params![json, row],
            )
            .unwrap();
        };
        for json in [
            "not json",
            "{",
            r#"{"segment_hashes":[],"segment_token_counts":[1],"cache_read_tokens":0}"#,
            r#"{"segment_hashes":["zz"],"segment_token_counts":[1],"cache_read_tokens":0}"#,
            r#"{"segment_hashes":[],"segment_token_counts":[],"cache_read_tokens":0,"extra":1}"#,
        ] {
            corrupt(json);
            assert!(
                matches!(
                    store.provider_call_prefix_rows(s.id),
                    Err(StoreError::Malformed(_))
                ),
                "corrupt payload {json:?} must fail the read loudly"
            );
        }
        // Over the byte bound injected behind the API's back: Oversized.
        corrupt(&format!(
            r#"{{"segment_hashes":[],"segment_token_counts":[],"cache_read_tokens":0,"pad":"{}"}}"#,
            "x".repeat(MAX_PREFIX_SEGMENTS_JSON)
        ));
        assert!(matches!(
            store.provider_call_prefix_rows(s.id),
            Err(StoreError::Oversized(_))
        ));
        // Repairing the row restores the read (the error is data-typed, not
        // sticky state).
        corrupt(&segments_json(2, &[10, 20]));
        let rows = store.provider_call_prefix_rows(s.id).unwrap();
        assert!(rows[0].prefix_segments_json.is_some());
    }

    pub(crate) fn hot_session(store: &Store) -> SessionId {
        let ws = store.create_workspace("/w").unwrap();
        store.create_session(ws, "t", "p", "m").unwrap().id
    }

    #[test]
    fn batch_hot_writes_force_strong_sync_and_restore_configured_mode() {
        // The actor's fsync-before-ack contract is implemented by lifting the
        // connection to synchronous=FULL for the group; the connection must
        // be back at the crate default (NORMAL) afterwards so direct writers
        // keep their configured behavior.
        let (_d, store) = tmp_store();
        let sid = hot_session(&store);
        let _ = store
            .batch_hot_writes(&[HotWrite::PutMessage {
                session_id: sid,
                seq: 1,
                role: "assistant".into(),
                data: serde_json::json!({ "parts": [] }),
            }])
            .unwrap();
        // Reopen simulates a process kill right after an acked batch: every
        // acked append must be present (WAL fsynced by FULL commit).
        let s2 = Store::open_fast(_d.path()).unwrap();
        assert_eq!(
            s2.messages_before(sid, None, 10).unwrap().len(),
            1,
            "acked append must survive a simulated kill"
        );
        drop(s2);
    }
}

#[cfg(test)]
mod typed_ledger_tests {
    use super::*;

    pub(crate) fn tmp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path(), true).unwrap();
        (dir, store)
    }

    pub(crate) fn seed_task(
        store: &Store,
        session_id: SessionId,
        task_id: TaskId,
        criteria: Vec<String>,
        state: TaskState,
    ) -> TaskRow {
        let row = TaskRow {
            task_id,
            session_id,
            goal: "g".into(),
            acceptance_criteria: criteria,
            plan: vec![],
            attachments: vec![],
            contract: Default::default(),
            max_tokens: None,
            max_turns: None,
            spent_tokens: 0,
            spent_turns: 0,
            state,
            revision: TaskRevision::new(1),
            created_ms: 1,
            updated_ms: 1,
        };
        store.upsert_task(&row).unwrap();
        row
    }

    /// Assert a read path refused a corrupt id COLUMN as a typed `Corrupt`
    /// naming the row and column — never a panic, a wrap or a minted id.
    pub(crate) fn assert_corrupt_id<T>(outcome: StoreResult<T>, what: &str, column: &str) {
        match outcome {
            Err(StoreError::Corrupt(msgs)) => assert!(
                msgs.iter().any(|m| m.contains(column)),
                "{what}: the refusal must name {column}: {msgs:?}"
            ),
            Err(e) => panic!("{what}: a corrupt {column} must refuse typed, got {e}"),
            Ok(_) => panic!("{what}: a corrupt {column} must refuse typed, got a decoded value"),
        }
    }

    /// Run one raw write with foreign keys and CHECK constraints OFF, then
    /// restore them: corrupting a referenced id column (or a `CHECK`-guarded
    /// seq) behind the typed API's back is exactly the hand-corrupted
    /// database the read-time decodes must survive.
    pub(crate) fn corrupt_ignoring_fks(store: &Store, sql: &str, params: &[&dyn rusqlite::ToSql]) {
        let conn = store.raw_conn();
        conn.execute_batch("PRAGMA foreign_keys = OFF; PRAGMA ignore_check_constraints = ON")
            .unwrap();
        conn.execute(sql, params).unwrap();
        conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA ignore_check_constraints = OFF")
            .unwrap();
    }

    /// The persisted-id decode class across the id COLUMN types: a zero
    /// `session.id` (SessionId), `session.workspace_id` (WorkspaceId),
    /// `task.task_id` (TaskId), `task.revision` (TaskRevision) or
    /// `event.seq` (EventSeq) could never have been written by the typed API,
    /// so every read path refuses it as a typed `Corrupt` naming the row and
    /// column — never `Id::new(0)` panicking or a silent `.max(1)` mint.
    /// NEGATIVE raw values are the bit-cast upper half of the u64 id space
    /// (the store writes every id through `raw as i64`), so they must decode
    /// back to the exact u64 — never be rejected as corruption. Repairing the
    /// row makes the same read decode it again (valid data round-trips).
    #[test]
    fn corrupt_id_columns_refuse_typed_never_panic_or_mint() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let task = seed_task(&store, s.id, TaskId::new(1), vec![], TaskState::Running);

        // Valid rows round-trip before corruption.
        assert_eq!(store.session_ids().unwrap(), vec![s.id]);
        assert_eq!(store.get_session(s.id).unwrap().unwrap().workspace_id, ws);
        assert_eq!(store.list_tasks(s.id).unwrap()[0].task_id, TaskId::new(1));
        assert_eq!(
            store
                .get_task(s.id, TaskId::new(1))
                .unwrap()
                .unwrap()
                .revision,
            task.revision
        );
        assert_eq!(store.last_event_seq(s.id).unwrap(), Some(EventSeq::new(1)));

        // Zero is structurally invalid for every one of these id types: each
        // read path refuses it typed, naming the row and column.
        {
            // (SessionId) `session.id`, decoded by the unscoped id scan.
            corrupt_ignoring_fks(
                &store,
                "UPDATE session SET id = 0 WHERE id = ?1",
                &[&(s.id.raw() as i64)],
            );
            assert_corrupt_id(store.session_ids(), "session_ids", "session");
            corrupt_ignoring_fks(
                &store,
                "UPDATE session SET id = ?1 WHERE id = 0",
                &[&(s.id.raw() as i64)],
            );

            // (WorkspaceId) `session.workspace_id` via session_row_map.
            corrupt_ignoring_fks(
                &store,
                "UPDATE session SET workspace_id = 0 WHERE id = ?1",
                &[&(s.id.raw() as i64)],
            );
            assert_corrupt_id(store.get_session(s.id), "get_session", "workspace_id");
            corrupt_ignoring_fks(
                &store,
                "UPDATE session SET workspace_id = ?2 WHERE id = ?1",
                &[&(s.id.raw() as i64), &(ws.raw() as i64)],
            );

            // (TaskId) `task.task_id` via the per-session task list.
            corrupt_ignoring_fks(
                &store,
                "UPDATE task SET task_id = 0 WHERE session_id = ?1 AND task_id = 1",
                &[&(s.id.raw() as i64)],
            );
            assert_corrupt_id(store.list_tasks(s.id), "list_tasks", "task_id");
            corrupt_ignoring_fks(
                &store,
                "UPDATE task SET task_id = 1 WHERE session_id = ?1 AND task_id = 0",
                &[&(s.id.raw() as i64)],
            );

            // (TaskRevision) `task.revision` via the single-row read.
            corrupt_ignoring_fks(
                &store,
                "UPDATE task SET revision = 0 WHERE session_id = ?1 AND task_id = 1",
                &[&(s.id.raw() as i64)],
            );
            assert_corrupt_id(store.get_task(s.id, TaskId::new(1)), "get_task", "revision");
            corrupt_ignoring_fks(
                &store,
                "UPDATE task SET revision = 1 WHERE session_id = ?1 AND task_id = 1",
                &[&(s.id.raw() as i64)],
            );

            // (EventSeq) `event.seq` via the journal high-water read.
            corrupt_ignoring_fks(
                &store,
                "UPDATE event SET seq = 0 WHERE session_id = ?1 AND seq = 1",
                &[&(s.id.raw() as i64)],
            );
            assert_corrupt_id(store.last_event_seq(s.id), "last_event_seq", "seq");
            corrupt_ignoring_fks(
                &store,
                "UPDATE event SET seq = 1 WHERE session_id = ?1 AND seq = 0",
                &[&(s.id.raw() as i64)],
            );
        }

        // The upper half of the u64 id space (negative i64 encodings) decodes
        // back exactly: u64::MAX (-1), u64::MAX-2 (-3) and 2^63 (i64::MIN).
        for wanted in [u64::MAX, u64::MAX - 2, 2u64.pow(63)] {
            let bad = wanted as i64;
            // (SessionId) `session.id`, decoded by the unscoped id scan.
            corrupt_ignoring_fks(
                &store,
                "UPDATE session SET id = ?2 WHERE id = ?1",
                &[&(s.id.raw() as i64), &bad],
            );
            assert_eq!(
                store.session_ids().unwrap(),
                vec![SessionId::new(wanted)],
                "session.id {wanted} must round-trip exactly"
            );
            corrupt_ignoring_fks(
                &store,
                "UPDATE session SET id = ?2 WHERE id = ?1",
                &[&bad, &(s.id.raw() as i64)],
            );
            assert_eq!(store.session_ids().unwrap(), vec![s.id]);

            // (WorkspaceId) `session.workspace_id` via session_row_map.
            corrupt_ignoring_fks(
                &store,
                "UPDATE session SET workspace_id = ?2 WHERE id = ?1",
                &[&(s.id.raw() as i64), &bad],
            );
            assert_eq!(
                store.get_session(s.id).unwrap().unwrap().workspace_id.raw(),
                wanted,
                "session.workspace_id {wanted} must round-trip exactly"
            );
            corrupt_ignoring_fks(
                &store,
                "UPDATE session SET workspace_id = ?2 WHERE id = ?1",
                &[&(s.id.raw() as i64), &(ws.raw() as i64)],
            );

            // (TaskId) `task.task_id` via the per-session task list.
            corrupt_ignoring_fks(
                &store,
                "UPDATE task SET task_id = ?2 WHERE session_id = ?1 AND task_id = 1",
                &[&(s.id.raw() as i64), &bad],
            );
            assert_eq!(
                store.list_tasks(s.id).unwrap()[0].task_id.raw(),
                wanted,
                "task.task_id {wanted} must round-trip exactly"
            );
            corrupt_ignoring_fks(
                &store,
                "UPDATE task SET task_id = ?2 WHERE session_id = ?1 AND task_id = ?3",
                &[&(s.id.raw() as i64), &1i64, &bad],
            );

            // (TaskRevision) `task.revision` via the single-row read.
            corrupt_ignoring_fks(
                &store,
                "UPDATE task SET revision = ?2 WHERE session_id = ?1 AND task_id = 1",
                &[&(s.id.raw() as i64), &bad],
            );
            assert_eq!(
                store
                    .get_task(s.id, TaskId::new(1))
                    .unwrap()
                    .unwrap()
                    .revision
                    .raw(),
                wanted,
                "task.revision {wanted} must round-trip exactly"
            );
            corrupt_ignoring_fks(
                &store,
                "UPDATE task SET revision = 1 WHERE session_id = ?1 AND task_id = 1",
                &[&(s.id.raw() as i64)],
            );

            // (EventSeq) `event.seq` via the journal high-water read.
            corrupt_ignoring_fks(
                &store,
                "UPDATE event SET seq = ?2 WHERE session_id = ?1 AND seq = 1",
                &[&(s.id.raw() as i64), &bad],
            );
            assert_eq!(
                store.last_event_seq(s.id).unwrap(),
                Some(EventSeq::new(wanted)),
                "event.seq {wanted} must round-trip exactly"
            );
            corrupt_ignoring_fks(
                &store,
                "UPDATE event SET seq = 1 WHERE session_id = ?1 AND seq = ?2",
                &[&(s.id.raw() as i64), &bad],
            );
            assert_eq!(
                store.last_event_seq(s.id).unwrap(),
                Some(EventSeq::new(1)),
                "repairing event.seq restores the journal"
            );
        }
    }

    /// Child-table id columns read by UNSCOPED doctor scans (no WHERE on the
    /// corrupt column, so the decode is really exercised): a zero
    /// `turn_record.session_id` and `cost_reservation.session_id/task_id`
    /// refuse typed, negative bit-cast raws decode back to the exact u64, and
    /// repairing the row re-enables the scan unchanged.
    #[test]
    fn corrupt_child_id_columns_refuse_typed_in_unscoped_scans() {
        let (_d, store) = tmp_store();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let task = seed_task(&store, s.id, TaskId::new(1), vec![], TaskState::Running);
        let turn_id = store
            .start_turn_record(s.id, OpId::new(5), None, None, "p", "m", None)
            .unwrap();
        let CostReserveOutcome::Granted(reservation) = store
            .cost_reserve(s.id, task.task_id, OpId::new(6), 10, now_ms())
            .unwrap()
        else {
            panic!("reservation must be granted");
        };

        // Baseline: both scans decode the valid rows.
        assert_eq!(store.all_active_turns().unwrap()[0].session_id, s.id);
        assert!(store
            .active_turn_ownership_invariants()
            .unwrap()
            .unrecoverable
            .iter()
            .any(|row| row.session_id == s.id));

        // Make the reservation dangling (no task row matches it): doctor's
        // unscoped dangling scan then decodes its id columns directly.
        corrupt_ignoring_fks(
            &store,
            "DELETE FROM task WHERE session_id = ?1 AND task_id = ?2",
            &[&(s.id.raw() as i64), &(task.task_id.raw() as i64)],
        );
        let dangling = store.cost_reservation_invariants().unwrap().dangling;
        assert_eq!(dangling.len(), 1);
        assert_eq!(dangling[0].session_id, s.id);
        assert_eq!(dangling[0].task_id, task.task_id);

        // Zero is structurally invalid: both unscoped scans refuse it typed.
        corrupt_ignoring_fks(
            &store,
            "UPDATE turn_record SET session_id = 0 WHERE id = ?1",
            &[&turn_id],
        );
        assert_corrupt_id(store.all_active_turns(), "all_active_turns", "session_id");
        assert_corrupt_id(
            store.active_turn_ownership_invariants(),
            "active_turn_ownership_invariants",
            "session_id",
        );
        corrupt_ignoring_fks(
            &store,
            "UPDATE turn_record SET session_id = ?2 WHERE id = ?1",
            &[&(s.id.raw() as i64), &turn_id],
        );
        corrupt_ignoring_fks(
            &store,
            "UPDATE cost_reservation SET session_id = 0 WHERE reservation_id = ?1",
            &[&reservation],
        );
        assert_corrupt_id(
            store.cost_reservation_invariants(),
            "cost_reservation_invariants",
            "session_id",
        );
        corrupt_ignoring_fks(
            &store,
            "UPDATE cost_reservation SET session_id = ?2 WHERE reservation_id = ?1",
            &[&(s.id.raw() as i64), &reservation],
        );
        corrupt_ignoring_fks(
            &store,
            "UPDATE cost_reservation SET task_id = 0 WHERE reservation_id = ?1",
            &[&reservation],
        );
        assert_corrupt_id(
            store.cost_reservation_invariants(),
            "cost_reservation_invariants",
            "task_id",
        );
        corrupt_ignoring_fks(
            &store,
            "UPDATE cost_reservation SET task_id = ?2 WHERE reservation_id = ?1",
            &[&(task.task_id.raw() as i64), &reservation],
        );

        // Negative raw values are the bit-cast upper half: the same scans
        // decode them back to the exact u64 (u64::MAX, u64::MAX-2, 2^63).
        for wanted in [u64::MAX, u64::MAX - 2, 2u64.pow(63)] {
            let bad = wanted as i64;
            corrupt_ignoring_fks(
                &store,
                "UPDATE turn_record SET session_id = ?2 WHERE id = ?1",
                &[&turn_id, &bad],
            );
            assert_eq!(
                store.all_active_turns().unwrap()[0].session_id.raw(),
                wanted,
                "turn_record.session_id {wanted} must round-trip exactly"
            );
            assert!(store
                .active_turn_ownership_invariants()
                .unwrap()
                .unrecoverable
                .iter()
                .any(|row| row.session_id.raw() == wanted));
            corrupt_ignoring_fks(
                &store,
                "UPDATE turn_record SET session_id = ?2 WHERE id = ?1",
                &[&(s.id.raw() as i64), &turn_id],
            );
            assert_eq!(store.all_active_turns().unwrap()[0].session_id, s.id);

            corrupt_ignoring_fks(
                &store,
                "UPDATE cost_reservation SET session_id = ?2 WHERE reservation_id = ?1",
                &[&reservation, &bad],
            );
            let by_session = store.cost_reservation_invariants().unwrap().dangling;
            assert_eq!(
                by_session
                    .iter()
                    .find(|row| row.reservation_id == reservation)
                    .unwrap()
                    .session_id
                    .raw(),
                wanted,
                "cost_reservation.session_id {wanted} must round-trip exactly"
            );
            corrupt_ignoring_fks(
                &store,
                "UPDATE cost_reservation SET session_id = ?2 WHERE reservation_id = ?1",
                &[&(s.id.raw() as i64), &reservation],
            );

            corrupt_ignoring_fks(
                &store,
                "UPDATE cost_reservation SET task_id = ?2 WHERE reservation_id = ?1",
                &[&reservation, &bad],
            );
            let by_task = store.cost_reservation_invariants().unwrap().dangling;
            assert_eq!(
                by_task
                    .iter()
                    .find(|row| row.reservation_id == reservation)
                    .unwrap()
                    .task_id
                    .raw(),
                wanted,
                "cost_reservation.task_id {wanted} must round-trip exactly"
            );
            corrupt_ignoring_fks(
                &store,
                "UPDATE cost_reservation SET task_id = ?2 WHERE reservation_id = ?1",
                &[&(task.task_id.raw() as i64), &reservation],
            );
            let repaired = store.cost_reservation_invariants().unwrap().dangling;
            assert_eq!(repaired.len(), 1);
            assert_eq!(repaired[0].task_id, task.task_id);
        }
    }
}
