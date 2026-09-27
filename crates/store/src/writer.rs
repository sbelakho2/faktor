//! Dedicated bounded database-writer service (audit item 7).
//!
//! The store used to serialize every mutation behind a process-wide
//! `Mutex<Connection>`: callers performed JSON serialization, hashing,
//! filesystem work and multi-statement transactions while holding the lock,
//! so one slow holder stalled every other domain and the lock's poisoning
//! made a single panic permanent.
//!
//! The writer service replaces the lock with a queue:
//!
//! * Commands are prepared ENTIRELY before enqueueing — JSON serialization,
//!   CAS lookups, hashing, filesystem opens and time stamps all happen on the
//!   caller's thread. A submitted job body must only execute SQLite work
//!   (statements/transactions) and map rows.
//! * One owner thread owns the [`rusqlite::Connection`] and executes jobs
//!   FIFO; no caller ever touches the connection or holds a transaction
//!   across calls.
//! * The queue is bounded ([`DEFAULT_WRITER_QUEUE_DEPTH`] by default). A
//!   caller that cannot enqueue within the bounded wait fails with the typed
//!   [`StoreError::WriterQueueFull`] backpressure error instead of blocking
//!   forever or growing memory.
//! * Results travel over a typed oneshot channel: `execute` returns the
//!   caller's `StoreResult<R>` with no `Any`/downcast.
//! * A panic inside a job is caught at the service boundary. The service is a
//!   DURABLE AUTHORITY: continuing to write on a connection after a panic is
//!   not proven safe, so the writer is marked unavailable and every later
//!   mutation fails typed with [`StoreError::WriterUnavailable`] — never a
//!   daemon panic and never silent corruption. Reopening the store (which
//!   rolls back any open transaction and re-validates) yields a fresh,
//!   healthy service.
//! * Instrumentation: queue wait, transaction duration, pending depth,
//!   WAL-checkpoint duration and slow transactions are recorded as atomics
//!   and surfaced via [`WriterTelemetry`]; slow ones also emit `tracing`
//!   warnings.
//! * Bounded shutdown ([`WriterService::shutdown`], audit item 7): admissions
//!   stop immediately, jobs already queued are DRAINED FIFO within the
//!   caller's bound, and when the bound expires the in-flight statement is
//!   interrupted (SQLite `interrupt` + a progress handler that aborts once
//!   the interrupt is requested) and the still-queued jobs are REJECTED with
//!   the typed [`StoreError::WriterUnavailable`]. The owner thread is joined
//!   only after a finite wait; a thread that cannot be reaped inside the
//!   interrupt grace is detached and the caller gets the typed
//!   [`WriterShutdownOutcome::Detached`] (surfaced by `Store::shutdown` as
//!   [`StoreError::WriterShutdownTimeout`]). Drop uses the same bounded path:
//!   there is no bare unbounded join anywhere.
//!
//! Ephemeral synchronization inside this module (queue mutex, condition
//! variables, health reason) is poison-tolerant by construction: the state is
//! plain bounded bookkeeping, so a panic in one holder must not disable the
//! service.

use std::collections::VecDeque;
use std::ffi::c_int;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rusqlite::{Connection, InterruptHandle};

use crate::StoreError;

/// Default pending-job bound. Callers block on their own result, so the
/// queue only ever holds concurrent-submitter backlog; 256 is a hard bound
/// that absorbs a burst while refusing unbounded growth.
pub const DEFAULT_WRITER_QUEUE_DEPTH: usize = 256;

/// Default bounded enqueue wait, matching the store's `busy_timeout` so
/// queue-level and SQLite-level backpressure agree.
pub const DEFAULT_WRITER_ENQUEUE_TIMEOUT: Duration = Duration::from_secs(5);

/// A job whose transaction duration exceeds this is instrumented as slow
/// (telemetry + `tracing` warning).
pub const SLOW_TRANSACTION_THRESHOLD: Duration = Duration::from_millis(250);

/// A job whose queue wait exceeds this is instrumented as slow.
pub const SLOW_QUEUE_WAIT_THRESHOLD: Duration = Duration::from_millis(250);

/// Default finite bound for [`WriterService::shutdown`]: how long a clean
/// stop may spend draining queued jobs before the owner is interrupted.
pub const DEFAULT_WRITER_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Finite grace AFTER the shutdown bound expires: the time the owner thread
/// gets to observe the delivered SQLite interrupt before the service detaches
/// it. The absolute shutdown bound is therefore `timeout + this grace`.
pub const WRITER_SHUTDOWN_INTERRUPT_GRACE: Duration = Duration::from_secs(1);

/// VDBE operations between progress-handler callbacks while a shutdown
/// interrupt is pending. Bounds how long an arbitrary long-running statement
/// can delay the abort (progress handlers run in the statement's own VM).
const SHUTDOWN_PROGRESS_OPS: c_int = 1000;

/// Outcome of [`WriterService::shutdown`] — typed so a caller can tell a
/// clean drain from an interrupted one and from a detach without inspecting
/// logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriterShutdownOutcome {
    /// Admissions were stopped and the owner stopped within the bound: the
    /// queued jobs drained (or a prior durable-authority failure had already
    /// rejected them), and the owner thread was joined.
    Drained,
    /// The bound expired: the in-flight statement was interrupted and the
    /// `rejected` jobs still queued were refused with the typed
    /// [`StoreError::WriterUnavailable`]. The owner thread joined within the
    /// interrupt grace.
    Interrupted { rejected: usize },
    /// The bound and the interrupt grace expired without the owner stopping
    /// (for example a job blocked in a non-SQLite operation). The `rejected`
    /// queued jobs were refused typed and the owner thread was detached; it
    /// exits on its own once the pending interrupt is observed. A reversed
    /// `Drop` cannot report this typed outcome (it logs a warning);
    /// `Store::shutdown` surfaces it as [`StoreError::WriterShutdownTimeout`].
    Detached { rejected: usize },
}

/// Hard bound on the retained per-job receipts ring. Receipts are a
/// best-effort diagnostic stream for benchmarks/observability: the ring never
/// grows past this, and evicted receipts are counted (`receipts_dropped`) so
/// a consumer can tell that its window was incomplete instead of seeing a
/// silently truncated series.
pub const WRITER_RECEIPT_CAPACITY: usize = 4096;

/// One completed writer command's receipt: the static label the store
/// submitted, how long the caller's command waited in the queue before the
/// owner thread started it, and how long its SQLite execution took. This is
/// the per-command accounting that `before`/`after` telemetry deltas can only
/// approximate (`WriterTelemetry` remains the cheap cumulative snapshot).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriterJobReceipt {
    /// The static command label (e.g. `append_event`, `put_message`).
    pub label: &'static str,
    /// Time between enqueue and the owner thread starting the job.
    pub queue_wait_ns: u64,
    /// Time the owner thread spent executing the job.
    pub run_ns: u64,
}

/// Bounded receipt ring; dropped is explicit so truncation is never silent.
#[derive(Default)]
struct ReceiptLog {
    ring: VecDeque<WriterJobReceipt>,
    dropped: u64,
}

/// Typed snapshot of one store instance's writer instrumentation. All fields
/// are monotonic counters except `pending_depth`, `pending_max`, `available`
/// and `unavailable_reason`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriterTelemetry {
    /// Jobs that were enqueued and executed.
    pub jobs: u64,
    /// Total time callers' jobs spent waiting in the queue before execution.
    pub queue_wait_total_ns: u64,
    pub queue_wait_max_ns: u64,
    /// Refusals due to the bounded queue being full past the wait.
    pub queue_full_refusals: u64,
    /// Total transaction time observed by the owner thread.
    pub transaction_total_ns: u64,
    pub transaction_max_ns: u64,
    /// Transactions at/above [`SLOW_TRANSACTION_THRESHOLD`].
    pub slow_transactions: u64,
    /// Current queued-job depth (excluding the job being executed).
    pub pending_depth: usize,
    pub pending_max: usize,
    /// Receipts the bounded ring had to evict because no consumer drained it
    /// in time (see `Store::take_writer_receipts`).
    pub receipts_dropped: u64,
    /// PASSIVE WAL checkpoint durations.
    pub checkpoints: u64,
    pub checkpoint_total_ns: u64,
    pub checkpoint_max_ns: u64,
    /// False once a job panicked (durable authority stopped admitting
    /// mutations); `unavailable_reason` explains why.
    pub available: bool,
    pub unavailable_reason: Option<String>,
}

struct QueueState {
    jobs: VecDeque<Job>,
    /// Admissions closed (shutdown or durable-authority failure).
    closed: bool,
    /// When closed, queued jobs are rejected (cleared) instead of drained.
    /// Panic (`mark_unavailable`) and the shutdown timeout set this; a clean
    /// shutdown drains first.
    reject_queued: bool,
}

struct Job {
    label: &'static str,
    enqueued: Instant,
    run: Box<dyn FnOnce(&mut Connection) + Send>,
}

impl std::fmt::Debug for Job {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Job")
            .field("label", &self.label)
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct Metrics {
    jobs: AtomicU64,
    queue_wait_total_ns: AtomicU64,
    queue_wait_max_ns: AtomicU64,
    queue_full_refusals: AtomicU64,
    transaction_total_ns: AtomicU64,
    transaction_max_ns: AtomicU64,
    slow_transactions: AtomicU64,
    pending: AtomicUsize,
    pending_max: AtomicUsize,
    checkpoints: AtomicU64,
    checkpoint_total_ns: AtomicU64,
    checkpoint_max_ns: AtomicU64,
}

struct WriterInner {
    state: Mutex<QueueState>,
    not_empty: Condvar,
    not_full: Condvar,
    capacity: usize,
    enqueue_timeout: Duration,
    /// Durable-authority health. Once false, the service admits no mutation.
    available: AtomicBool,
    unavailable_reason: Mutex<Option<String>>,
    metrics: Metrics,
    receipts: Mutex<ReceiptLog>,
    /// Shutdown requested: admissions are closed and the owner drains the
    /// queue (unless the deadline later rejects it).
    shutdown: AtomicBool,
    /// Interrupt requested: the deadline expired, so the progress handler
    /// aborts any in-flight statement and `interrupt` fires.
    interrupt_requested: AtomicBool,
    /// SQLite interrupt handle for the owner connection (delivered once the
    /// shutdown deadline expires). Cloned before the connection moves into
    /// the owner thread.
    interrupt: InterruptHandle,
    /// Owner thread stopped (set once, just before the thread returns).
    stopped: Mutex<bool>,
    stopped_cv: Condvar,
}

impl std::fmt::Debug for WriterInner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriterInner")
            .field("capacity", &self.capacity)
            .field("available", &self.available.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// Owns the writer thread and the single database connection. `Store` holds
/// exactly one; the service is not cloneable, so there is exactly one SQL
/// writer per store instance.
pub(crate) struct WriterService {
    inner: Arc<WriterInner>,
    /// Owner-thread handle. `Mutex` (not `&mut self`) so the bounded
    /// `shutdown(&self, …)` can be called from shared access (a daemon
    /// shutting down an `Arc<Store>`), and is `None` once joined/detached.
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for WriterService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriterService")
            .field("inner", &self.inner)
            .field(
                "thread_alive",
                &self
                    .thread
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .is_some(),
            )
            .finish()
    }
}

impl WriterService {
    /// Start the owner thread with `conn`; the connection never returns.
    pub(crate) fn spawn(conn: Connection) -> crate::StoreResult<Self> {
        Self::spawn_with_limits(
            conn,
            DEFAULT_WRITER_QUEUE_DEPTH,
            DEFAULT_WRITER_ENQUEUE_TIMEOUT,
        )
    }

    /// [`WriterService::spawn`] with explicit bounds (adversarial tests drive
    /// the typed backpressure path without waiting the default timeout).
    pub(crate) fn spawn_with_limits(
        conn: Connection,
        capacity: usize,
        enqueue_timeout: Duration,
    ) -> crate::StoreResult<Self> {
        let capacity = capacity.max(1);
        // The interrupt handle and the shutdown progress handler are wired
        // BEFORE the connection moves to the owner thread: they are the
        // bounded-stop mechanism for a statement that would otherwise run
        // past the shutdown deadline.
        let interrupt = conn.get_interrupt_handle();
        let inner = Arc::new(WriterInner {
            state: Mutex::new(QueueState {
                jobs: VecDeque::with_capacity(capacity.min(64)),
                closed: false,
                reject_queued: false,
            }),
            not_empty: Condvar::new(),
            not_full: Condvar::new(),
            capacity,
            enqueue_timeout,
            available: AtomicBool::new(true),
            unavailable_reason: Mutex::new(None),
            metrics: Metrics::default(),
            receipts: Mutex::new(ReceiptLog::default()),
            shutdown: AtomicBool::new(false),
            interrupt_requested: AtomicBool::new(false),
            interrupt,
            stopped: Mutex::new(false),
            stopped_cv: Condvar::new(),
        });
        {
            let progress_inner = Arc::clone(&inner);
            conn.progress_handler(
                SHUTDOWN_PROGRESS_OPS,
                Some(move || progress_inner.interrupt_requested.load(Ordering::Acquire)),
            )
            .map_err(|e| {
                StoreError::Migration(format!(
                    "failed to install the writer shutdown progress handler: {e}"
                ))
            })?;
        }
        let thread_inner = Arc::clone(&inner);
        let handle = thread::Builder::new()
            .name("faktor-store-writer".into())
            .spawn(move || {
                // A panic in the loop machinery itself (outside the per-job
                // catch) must still stop the authority typed and signal
                // stop-waiters, never strand them or kill the thread silently.
                let loop_inner = Arc::clone(&thread_inner);
                let outcome = catch_unwind(AssertUnwindSafe(|| writer_loop(loop_inner, conn)));
                if let Err(payload) = outcome {
                    thread_inner.mark_unavailable(format!(
                        "writer owner thread panicked: {}",
                        panic_message(payload)
                    ));
                }
                thread_inner.signal_stopped();
            })
            // Thread spawn failure at open time is an environment failure:
            // surface it typed instead of panicking.
            .map_err(|e| {
                StoreError::Migration(format!("failed to spawn the store writer thread: {e}"))
            })?;
        Ok(Self {
            inner,
            thread: Mutex::new(Some(handle)),
        })
    }

    /// True while the durable authority still admits mutations (false once a
    /// shutdown was requested or a command panicked).
    pub(crate) fn is_available(&self) -> bool {
        self.inner.available.load(Ordering::Acquire) && !self.inner.is_shutdown_requested()
    }

    /// Typed one-line reason the writer stopped, when it did.
    pub(crate) fn unavailable_reason(&self) -> Option<String> {
        self.inner.reason_opt()
    }

    /// Stop admitting mutations and switch to drain mode WITHOUT waiting
    /// (the first half of [`WriterService::shutdown`], exposed so a caller
    /// can close admissions while it cancels other subsystems first). Later
    /// `execute` calls fail typed; queued jobs keep draining.
    pub(crate) fn begin_shutdown(&self) {
        self.inner.begin_shutdown();
    }

    /// Bounded, idempotent shutdown (audit item 7). Documented semantics:
    ///
    /// 1. **Stop admissions** — every later [`WriterService::execute`] fails
    ///    typed with [`StoreError::WriterUnavailable`]; callers already
    ///    blocked on a full queue wake and fail the same way.
    /// 2. **Drain** — jobs already queued execute FIFO until the queue is
    ///    empty, then the owner thread exits.
    /// 3. **Interrupt at the bound** — when `timeout` expires, the SQLite
    ///    interrupt fires and the progress handler aborts the in-flight
    ///    statement; every still-queued job is REJECTED with the typed
    ///    unavailable error (its caller never hangs).
    /// 4. **Finite join** — the owner is joined only after its `stopped`
    ///    signal; if that does not arrive within
    ///    [`WRITER_SHUTDOWN_INTERRUPT_GRACE`] the thread is detached (never
    ///    an unbounded join) and the outcome says so.
    ///
    /// The call is fully bounded by `timeout + WRITER_SHUTDOWN_INTERRUPT_GRACE`.
    pub(crate) fn shutdown(&self, timeout: Duration) -> WriterShutdownOutcome {
        let deadline = Instant::now() + timeout;
        self.inner.begin_shutdown();
        if self.inner.wait_stopped(deadline) {
            self.join_owner();
            return WriterShutdownOutcome::Drained;
        }
        // The bound expired with work still in flight: interrupt it and
        // reject what is still queued so no caller waits on a dead queue.
        self.inner
            .interrupt_requested
            .store(true, Ordering::Release);
        self.inner.interrupt.interrupt();
        let rejected = self.inner.reject_queued();
        if self
            .inner
            .wait_stopped(Instant::now() + WRITER_SHUTDOWN_INTERRUPT_GRACE)
        {
            self.join_owner();
            WriterShutdownOutcome::Interrupted { rejected }
        } else {
            // Detach: the owner exits on its own once the interrupt is
            // observed. Dropping the handle never blocks.
            self.thread.lock().unwrap_or_else(|p| p.into_inner()).take();
            WriterShutdownOutcome::Detached { rejected }
        }
    }

    /// True once the owner thread recorded its `stopped` signal.
    pub(crate) fn is_stopped(&self) -> bool {
        self.inner
            .stopped
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .to_owned()
    }

    /// Join the owner thread if it is still owned (the `stopped` signal has
    /// already arrived, so this cannot block for long: the thread is between
    /// its last statement and return).
    fn join_owner(&self) {
        let handle = self.thread.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(handle) = handle {
            let _ = handle.join();
        }
    }

    /// Enqueue one PREPARED job (SQL/transaction work only) and block on its
    /// typed result. Closure captures must be owned/`'static`, which is what
    /// statically prevents a job from re-entering the store: a job can never
    /// borrow `&Store`, so it can never enqueue another job and deadlock.
    pub(crate) fn execute<R, F>(&self, label: &'static str, f: F) -> crate::StoreResult<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut Connection) -> crate::StoreResult<R> + Send + 'static,
    {
        self.inner.check_available(label)?;
        let (result_tx, result_rx) = mpsc::sync_channel::<crate::StoreResult<R>>(1);
        let job_inner = Arc::clone(&self.inner);
        let job = Job {
            label,
            enqueued: Instant::now(),
            run: Box::new(move |conn| {
                match catch_unwind(AssertUnwindSafe(|| f(conn))) {
                    Ok(result) => {
                        // The caller may have gone away with the whole store;
                        // a failed send is not an error here.
                        let _ = result_tx.send(result);
                    }
                    Err(payload) => {
                        let reason = format!(
                            "writer command `{label}` panicked: {}",
                            panic_message(payload)
                        );
                        job_inner.mark_unavailable(reason.clone());
                        let _ = result_tx.send(Err(StoreError::WriterUnavailable(reason)));
                    }
                }
            }),
        };
        self.inner.submit(job, label)?;
        match result_rx.recv() {
            Ok(result) => result,
            Err(_) => Err(StoreError::WriterUnavailable(self.inner.reason_or_dead())),
        }
    }

    /// [`WriterService::execute`] for jobs returning a plain value (test
    /// seams and maintenance probes).
    pub(crate) fn execute_raw<R, F>(&self, label: &'static str, f: F) -> crate::StoreResult<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut Connection) -> R + Send + 'static,
    {
        self.execute(label, move |conn| Ok(f(conn)))
    }

    /// Record one WAL checkpoint duration (instrumentation).
    pub(crate) fn record_checkpoint(&self, duration: Duration) {
        let ns = duration.as_nanos().min(u64::MAX as u128) as u64;
        self.inner
            .metrics
            .checkpoints
            .fetch_add(1, Ordering::Relaxed);
        self.inner
            .metrics
            .checkpoint_total_ns
            .fetch_add(ns, Ordering::Relaxed);
        self.inner
            .metrics
            .checkpoint_max_ns
            .fetch_max(ns, Ordering::Relaxed);
        if duration >= SLOW_TRANSACTION_THRESHOLD {
            tracing::warn!(
                target: "faktor_store::writer",
                op = "wal_checkpoint_passive",
                duration_ms = duration.as_millis() as u64,
                "slow WAL checkpoint"
            );
        }
    }

    /// Drain every per-job receipt recorded since the last drain, oldest
    /// first. Bounded by [`WRITER_RECEIPT_CAPACITY`]; check
    /// [`WriterTelemetry::receipts_dropped`] to detect a truncated window.
    pub(crate) fn take_receipts(&self) -> Vec<WriterJobReceipt> {
        let mut log = self
            .inner
            .receipts
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        log.ring.drain(..).collect()
    }

    /// Typed instrumentation snapshot.
    pub(crate) fn telemetry(&self) -> WriterTelemetry {
        let m = &self.inner.metrics;
        WriterTelemetry {
            jobs: m.jobs.load(Ordering::Relaxed),
            queue_wait_total_ns: m.queue_wait_total_ns.load(Ordering::Relaxed),
            queue_wait_max_ns: m.queue_wait_max_ns.load(Ordering::Relaxed),
            queue_full_refusals: m.queue_full_refusals.load(Ordering::Relaxed),
            transaction_total_ns: m.transaction_total_ns.load(Ordering::Relaxed),
            transaction_max_ns: m.transaction_max_ns.load(Ordering::Relaxed),
            slow_transactions: m.slow_transactions.load(Ordering::Relaxed),
            pending_depth: m.pending.load(Ordering::Relaxed),
            pending_max: m.pending_max.load(Ordering::Relaxed),
            receipts_dropped: self.receipts_dropped(),
            checkpoints: m.checkpoints.load(Ordering::Relaxed),
            checkpoint_total_ns: m.checkpoint_total_ns.load(Ordering::Relaxed),
            checkpoint_max_ns: m.checkpoint_max_ns.load(Ordering::Relaxed),
            available: self.is_available(),
            unavailable_reason: self.unavailable_reason(),
        }
    }

    fn receipts_dropped(&self) -> u64 {
        self.inner
            .receipts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .dropped
    }
}

impl Drop for WriterService {
    fn drop(&mut self) {
        // Bounded by construction: drain within the default bound, then
        // interrupt, reject what is left and detach if the owner still does
        // not stop. There is deliberately NO bare unbounded join here.
        let outcome = self.shutdown(DEFAULT_WRITER_SHUTDOWN_TIMEOUT);
        if !matches!(outcome, WriterShutdownOutcome::Drained) {
            tracing::warn!(
                target: "faktor_store::writer",
                outcome = ?outcome,
                "writer service did not drain before the shutdown bound; \
                 in-flight SQL was interrupted and queued mutations were rejected typed"
            );
        }
    }
}

impl WriterInner {
    fn check_available(&self, _label: &'static str) -> crate::StoreResult<()> {
        if self.available.load(Ordering::Acquire) && !self.is_shutdown_requested() {
            Ok(())
        } else {
            Err(StoreError::WriterUnavailable(self.reason_or_dead()))
        }
    }

    fn is_shutdown_requested(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }

    fn reason_opt(&self) -> Option<String> {
        self.unavailable_reason
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// The one-shot reason string (or a generic one if the thread vanished
    /// without recording a reason).
    fn reason_or_dead(&self) -> String {
        self.reason_opt().unwrap_or_else(|| {
            if self.is_shutdown_requested() {
                "writer service is shutting down".to_string()
            } else {
                "writer service stopped".to_string()
            }
        })
    }

    /// Stop admissions and start a DRAIN shutdown: the owner keeps executing
    /// queued jobs until the queue empties. Idempotent; the timeout path
    /// later calls [`WriterInner::reject_queued`] to switch to rejection.
    fn begin_shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            state.closed = true;
            state.reject_queued = false;
        }
        self.not_empty.notify_all();
        self.not_full.notify_all();
    }

    /// Wait (bounded by `deadline`) for the owner thread's `stopped` signal.
    fn wait_stopped(&self, deadline: Instant) -> bool {
        let mut stopped = self.stopped.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if *stopped {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let (guard, _) = self
                .stopped_cv
                .wait_timeout(stopped, deadline - now)
                .unwrap_or_else(|p| p.into_inner());
            stopped = guard;
        }
    }

    /// Record that the owner thread is done (called exactly once, after the
    /// loop returns, before the thread exits).
    fn signal_stopped(&self) {
        *self.stopped.lock().unwrap_or_else(|p| p.into_inner()) = true;
        self.stopped_cv.notify_all();
    }

    /// Typed durable-authority failure: stop admitting mutations, REJECT
    /// every queued job (blocked callers wake with the typed unavailable
    /// error — never a silent execute on an unproven connection), and wake
    /// every waiter. Continuing after a panic is not proven safe.
    fn mark_unavailable(&self, reason: String) {
        if self.available.swap(false, Ordering::AcqRel) {
            *self
                .unavailable_reason
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = Some(reason);
        }
        self.reject_queued();
        self.not_empty.notify_all();
    }

    /// Bounded enqueue. Blocks until a slot frees or the bounded wait
    /// expires; a full queue past the wait is a typed refusal, never an
    /// unbounded wait and never an unbounded queue.
    fn submit(&self, job: Job, label: &'static str) -> crate::StoreResult<()> {
        let deadline = Instant::now() + self.enqueue_timeout;
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if state.closed {
                return Err(StoreError::WriterUnavailable(self.reason_or_dead()));
            }
            if state.jobs.len() < self.capacity {
                state.jobs.push_back(job);
                let pending = self.metrics.pending.fetch_add(1, Ordering::AcqRel) + 1;
                self.metrics
                    .pending_max
                    .fetch_max(pending, Ordering::Relaxed);
                drop(state);
                self.not_empty.notify_one();
                return Ok(());
            }
            let now = Instant::now();
            if now >= deadline {
                self.metrics
                    .queue_full_refusals
                    .fetch_add(1, Ordering::Relaxed);
                return Err(StoreError::WriterQueueFull {
                    operation: label,
                    pending: state.jobs.len(),
                    capacity: self.capacity,
                });
            }
            let (guard, _) = self
                .not_full
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|p| p.into_inner());
            state = guard;
        }
    }

    /// Wait for the next job. A clean shutdown (`reject_queued == false`)
    /// keeps draining the queued jobs FIFO; a panic/timeout shutdown rejects,
    /// so a closed queue with `reject_queued` returns `None` immediately.
    fn next_job(&self) -> Option<Job> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if state.reject_queued || (state.closed && state.jobs.is_empty()) {
                return None;
            }
            if let Some(job) = state.jobs.pop_front() {
                self.metrics.pending.fetch_sub(1, Ordering::AcqRel);
                return Some(job);
            }
            state = self
                .not_empty
                .wait(state)
                .unwrap_or_else(|p| p.into_inner());
        }
    }

    /// Reject every queued job (dropping each sender wakes its blocked caller
    /// with the typed unavailable error), mark the queue closed and report
    /// how many jobs were refused. Idempotent.
    fn reject_queued(&self) -> usize {
        let rejected = {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            state.closed = true;
            state.reject_queued = true;
            let rejected = state.jobs.len();
            state.jobs.clear();
            self.metrics.pending.store(0, Ordering::Release);
            rejected
        };
        self.not_full.notify_all();
        self.not_empty.notify_all();
        rejected
    }

    fn record_completed(&self, label: &'static str, wait: Duration, run: Duration) {
        let wait_ns = wait.as_nanos().min(u64::MAX as u128) as u64;
        let run_ns = run.as_nanos().min(u64::MAX as u128) as u64;
        self.metrics.jobs.fetch_add(1, Ordering::Relaxed);
        self.metrics
            .queue_wait_total_ns
            .fetch_add(wait_ns, Ordering::Relaxed);
        self.metrics
            .queue_wait_max_ns
            .fetch_max(wait_ns, Ordering::Relaxed);
        self.metrics
            .transaction_total_ns
            .fetch_add(run_ns, Ordering::Relaxed);
        self.metrics
            .transaction_max_ns
            .fetch_max(run_ns, Ordering::Relaxed);
        let pending = self.metrics.pending.load(Ordering::Relaxed);
        {
            // Per-job receipt ring is ephemeral diagnostics: recover from a
            // poisoned lock (see the module's poisoning policy) and evict the
            // oldest receipt instead of growing without bound.
            let mut log = self.receipts.lock().unwrap_or_else(|p| p.into_inner());
            if log.ring.len() >= WRITER_RECEIPT_CAPACITY {
                log.ring.pop_front();
                log.dropped += 1;
            }
            log.ring.push_back(WriterJobReceipt {
                label,
                queue_wait_ns: wait_ns,
                run_ns,
            });
        }
        if wait >= SLOW_QUEUE_WAIT_THRESHOLD {
            tracing::warn!(
                target: "faktor_store::writer",
                command = label,
                wait_ms = wait.as_millis() as u64,
                pending_depth = pending,
                "slow writer queue wait"
            );
        }
        if run >= SLOW_TRANSACTION_THRESHOLD {
            self.metrics
                .slow_transactions
                .fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                target: "faktor_store::writer",
                command = label,
                transaction_ms = run.as_millis() as u64,
                pending_depth = pending,
                "slow writer transaction"
            );
        }
    }
}

fn writer_loop(inner: Arc<WriterInner>, mut conn: Connection) {
    loop {
        let Some(job) = inner.next_job() else {
            // `next_job` returns None only when the queue is closed and
            // either empty (drained) or rejecting; clear anything left so a
            // caller can never block on a dead queue.
            inner.reject_queued();
            return;
        };
        let Job {
            label,
            enqueued,
            run,
        } = job;
        let wait = enqueued.elapsed();
        let started = Instant::now();
        // Defense in depth: `execute` already catches panics inside the job
        // wrapper, but a panic in the wrapper itself (or in a captured
        // destructor) must still stop the authority typed instead of killing
        // the thread silently and stranding queued callers.
        let outcome = catch_unwind(AssertUnwindSafe(|| run(&mut conn)));
        let elapsed = started.elapsed();
        inner.record_completed(label, wait, elapsed);
        if let Err(payload) = outcome {
            inner.mark_unavailable(format!(
                "writer command `{label}` panicked: {}",
                panic_message(payload)
            ));
        }
        if !inner.available.load(Ordering::Acquire) {
            // Durable-authority failure: reject what is left and stop.
            inner.reject_queued();
            return;
        }
        // A clean shutdown keeps looping: `next_job` drains the remaining
        // queued jobs FIFO and returns None once the queue is empty.
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}
