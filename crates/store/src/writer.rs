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
//!
//! Ephemeral synchronization inside this module (queue mutex, condition
//! variables, health reason) is poison-tolerant by construction: the state is
//! plain bounded bookkeeping, so a panic in one holder must not disable the
//! service.

use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rusqlite::Connection;

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
    closed: bool,
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
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for WriterService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WriterService")
            .field("inner", &self.inner)
            .field("thread_alive", &self.thread.is_some())
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
        let inner = Arc::new(WriterInner {
            state: Mutex::new(QueueState {
                jobs: VecDeque::with_capacity(capacity.min(64)),
                closed: false,
            }),
            not_empty: Condvar::new(),
            not_full: Condvar::new(),
            capacity,
            enqueue_timeout,
            available: AtomicBool::new(true),
            unavailable_reason: Mutex::new(None),
            metrics: Metrics::default(),
        });
        let thread_inner = Arc::clone(&inner);
        let handle = thread::Builder::new()
            .name("faktor-store-writer".into())
            .spawn(move || writer_loop(thread_inner, conn))
            // Thread spawn failure at open time is an environment failure:
            // surface it typed instead of panicking.
            .map_err(|e| {
                StoreError::Migration(format!("failed to spawn the store writer thread: {e}"))
            })?;
        Ok(Self {
            inner,
            thread: Some(handle),
        })
    }

    /// True while the durable authority still admits mutations.
    pub(crate) fn is_available(&self) -> bool {
        self.inner.available.load(Ordering::Acquire)
    }

    /// Typed one-line reason the writer stopped, when it did.
    pub(crate) fn unavailable_reason(&self) -> Option<String> {
        self.inner.reason_opt()
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
            checkpoints: m.checkpoints.load(Ordering::Relaxed),
            checkpoint_total_ns: m.checkpoint_total_ns.load(Ordering::Relaxed),
            checkpoint_max_ns: m.checkpoint_max_ns.load(Ordering::Relaxed),
            available: self.is_available(),
            unavailable_reason: self.unavailable_reason(),
        }
    }
}

impl Drop for WriterService {
    fn drop(&mut self) {
        {
            let mut state = self.inner.state.lock().unwrap_or_else(|p| p.into_inner());
            state.closed = true;
        }
        self.inner.not_empty.notify_all();
        self.inner.not_full.notify_all();
        if let Some(handle) = self.thread.take() {
            // The owner thread drains and drops any queued jobs (waking their
            // blocked callers with the typed unavailable error) before it
            // observes `closed`. Join is bounded by the current job.
            let _ = handle.join();
        }
    }
}

impl WriterInner {
    fn check_available(&self, _label: &'static str) -> crate::StoreResult<()> {
        if self.available.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(StoreError::WriterUnavailable(self.reason_or_dead()))
        }
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
        self.reason_opt()
            .unwrap_or_else(|| "writer service stopped".to_string())
    }

    /// Typed durable-authority failure: stop admitting mutations, wake every
    /// waiter, and let the owner thread drain the queue.
    fn mark_unavailable(&self, reason: String) {
        if self.available.swap(false, Ordering::AcqRel) {
            *self
                .unavailable_reason
                .lock()
                .unwrap_or_else(|p| p.into_inner()) = Some(reason);
        }
        {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            state.closed = true;
        }
        self.not_empty.notify_all();
        self.not_full.notify_all();
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

    /// Wait for the next job; `None` when the service is closing.
    fn next_job(&self) -> Option<Job> {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if state.closed {
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

    /// Drop every queued job (waking blocked submitters with the typed
    /// unavailable error) and mark the queue closed.
    fn drain(&self) {
        {
            let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            state.closed = true;
            state.jobs.clear();
            self.metrics.pending.store(0, Ordering::Release);
        }
        self.not_full.notify_all();
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
            inner.drain();
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
            inner.drain();
            return;
        }
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
