//! SQLite persistence done correctly: WAL, single logical writer + genuinely
//! bounded reader pool, busy timeout, explicit transactional migrations,
//! integrity checks, automatic backups.
//!
//! Large blobs never live here — they go to the CAS; SQLite stores hashes.
//! Message/part rows store JSON payloads so the store stays protocol-agnostic.
//!
//! # Reader pool bound
//!
//! `read()` acquires a semaphore permit before touching a connection, so at
//! most `READER_POOL` (4) connections exist concurrently; a 20-reader storm
//! therefore uses at most 4 connections and the remaining callers block on
//! the permit, bounded by the busy timeout (`StoreError::Busy`). The pool is
//! a concurrency limit, not merely a retention limit.
//!
//! # Async boundary
//!
//! This crate is intentionally synchronous; do not introduce tokio here.
//! The daemon's HOT append paths (message append / part append / journal
//! event / usage settlement) run through `faktor_session`'s `DbActor`: a
//! dedicated `std::thread` owning this store, fronted by a bounded async
//! request channel. The actor executes grouped writes through
//! [`Store::batch_hot_writes`] — ONE transaction and ONE fsync per batch —
//! so a Tokio worker never executes a SQLite statement for those paths.
//!
//! Every OTHER call stays direct and synchronous on the shared
//! [`Store`](crate::Store) (reads, compound transitions, recovery,
//! checkpoints, ...). Both surfaces submit to the SAME single-owner
//! [`writer::WriterService`]: commands are prepared on the caller's thread,
//! enqueued on a bounded queue and executed FIFO by one owner thread that
//! holds the one `rusqlite::Connection`. See [`Store::direct`] for the
//! deliberate sync-access marker. Callers that need a write observed by a
//! later direct read must await the actor response (the actor fsyncs before
//! replying), or serialize through [`Store::direct`].
//!
//! # Writer service (audit item 7)
//!
//! There is no process-wide `Mutex<Connection>` any more. Mutations are
//! commands on a dedicated single-owner service (see [`writer`]): all inputs
//! are prepared FIRST — JSON serialization of payloads (`HotWrite` batches,
//! messages, parts, ledger blobs/entries/heads, task/verification columns,
//! tool args/recovery/postcondition, queue files), destination connections
//! for backups, and time stamps — and only SQLite statements/transactions
//! run on the owner thread. The queue is bounded (typed
//! [`StoreError::WriterQueueFull`] backpressure), results are typed oneshots,
//! and queue wait / transaction duration / pending depth / checkpoint
//! duration / slow transactions are instrumented ([`WriterTelemetry`]).
//! The prepared-write contract is statically enforced (audit item 8): test
//! `faktor-tests-static-authority` refuses filesystem I/O, serialization,
//! hashing and sleeps inside production writer job closures.
//!
//! Shutdown is bounded ([`WriterShutdownOutcome`]): admissions stop, queued
//! mutations drain, and at the bound in-flight SQL is interrupted (SQLite
//! interrupt + progress handler) and what is left is rejected with the typed
//! [`StoreError::WriterUnavailable`]; the owner thread is joined only after
//! its finite stop signal (a straggler is detached and `Store::shutdown`
//! reports the typed [`StoreError::WriterShutdownTimeout`]). Drop uses the
//! same finite path — there is no unbounded join anywhere.
//!
//! Online backups (`Store::backup_to`) run on a dedicated maintenance thread
//! with their own read/snapshot connection (audit item 6): the single writer
//! owner is never held for a copy, and a store shutdown cancels a running
//! backup through its bounded maintenance token.
//!
//! Per-job receipts ([`WriterJobReceipt`], drained with
//! [`Store::take_writer_receipts`]) carry the label + queue wait + execution
//! of each completed command, and the reader pool's permit wait is exposed
//! separately ([`ReaderTelemetry`], [`Store::take_reader_receipts`]) from any
//! caller's end-to-end read duration.
//!
//! # Stability rule
//!
//! Every value read back from the database is parsed fallibly: corrupt or
//! version-skewed rows surface as `StoreError::Corrupt` (or `Sqlite`) —
//! never a panic. `unwrap`/`expect` appear only where the input is provably
//! constructed in-process this session (each site is commented).
//!
//! # Poisoning policy (audit item 8)
//!
//! Ephemeral in-process state (reader-pool permits/cache, crash seam) is
//! recovered from a poisoned mutex with `into_inner()`. The durable writer
//! authority does not panic: a panic inside a command (including a
//! deliberate crash-seam panic) is caught at the service boundary, the
//! writer stops admitting mutations, and every later mutation fails typed
//! with [`StoreError::WriterUnavailable`] until the store is reopened.

mod writer;

pub use writer::{
    WriterJobReceipt, WriterShutdownOutcome, WriterTelemetry, DEFAULT_WRITER_ENQUEUE_TIMEOUT,
    DEFAULT_WRITER_QUEUE_DEPTH, DEFAULT_WRITER_SHUTDOWN_TIMEOUT, SLOW_QUEUE_WAIT_THRESHOLD,
    SLOW_TRANSACTION_THRESHOLD, WRITER_RECEIPT_CAPACITY, WRITER_SHUTDOWN_INTERRUPT_GRACE,
};

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use writer::WriterService;

use faktor_core::attachment::AttachmentId;
use faktor_core::event::{Event, EventKind, JournalInvariants};
use faktor_core::id::{
    EventSeq, OpId, SessionId, TaskId, TaskRevision, VerificationRecordId, WorkspaceId, WorktreeId,
};
use faktor_core::model::{PricingSnapshot, RiskBucket, RouterPhase, TaskClass};
use faktor_core::state::{
    AgentState, CheckExecution, CriterionVerification, FileStateEvidence, SessionLifecycle,
    TaskState, VerificationStatus,
};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("store is corrupted: integrity check failed with {0:?}")]
    Corrupt(Vec<String>),
    #[error("event sequence gap or duplicate detected at {0}")]
    SeqViolation(u64),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("reader pool busy: {0}")]
    Busy(String),
    #[error("migration failed: {0}")]
    Migration(String),
    #[error("oversized value rejected: {0}")]
    Oversized(String),
    #[error("malformed value rejected: {0}")]
    Malformed(String),
    /// Typed backpressure: the bounded writer queue was full for longer than
    /// the configured enqueue wait, so the prepared command was refused
    /// before touching the database.
    #[error("writer queue full: {pending} pending (capacity {capacity}); `{operation}` refused")]
    WriterQueueFull {
        operation: &'static str,
        pending: usize,
        capacity: usize,
    },
    /// Typed durable-authority failure: a writer command panicked, so the
    /// store stopped admitting mutations instead of panicking or continuing
    /// on an unproven connection. Reopen the store to obtain a fresh,
    /// validated writer.
    #[error("durable store writer unavailable: {0}")]
    WriterUnavailable(String),
    /// Typed bounded-shutdown expiry (audit item 7): the writer owner did not
    /// stop within the shutdown bound plus the interrupt grace. The in-flight
    /// statement was interrupted, `rejected` queued mutations were refused
    /// with [`StoreError::WriterUnavailable`], and the owner thread was
    /// detached — it exits on its own once the interrupt is observed.
    #[error(
        "writer shutdown timed out: {rejected} queued mutations rejected; owner thread detached"
    )]
    WriterShutdownTimeout { rejected: usize },
    /// Typed bounded-shutdown expiry for the maintenance authority (audit item
    /// 6/7): every maintenance task was cancelled but `active` tasks had still
    /// not deregistered when the bound expired, so a store-owned authority is
    /// still alive and touching its own connection (and possibly the backup
    /// destination). `writer_outcome` carries what the writer authority did,
    /// so the caller sees BOTH authorities. A successful shutdown requires
    /// every store-owned authority — writer AND maintenance registry — to be
    /// quiescent.
    #[error(
        "maintenance shutdown timed out: {active} active task(s) did not drain; \
         writer outcome {writer_outcome:?}"
    )]
    MaintenanceShutdownTimeout {
        active: usize,
        writer_outcome: WriterShutdownOutcome,
    },
    /// Typed maintenance failure (audit item 6): the online-backup
    /// maintenance thread could not be spawned, died without a result, or was
    /// cancelled because the store is shutting down. Online backups never run
    /// on the mutation-owner connection.
    #[error("store maintenance failed: {0}")]
    Maintenance(String),
}

pub type StoreResult<T> = Result<T, StoreError>;

mod connection;
pub use connection::*;
mod schema;
pub(crate) use schema::*;
mod migration;
pub use migration::*;
mod sessions;
pub use sessions::*;
mod messages;
pub use messages::*;
mod tasks;
pub use tasks::*;
mod ledger;
pub use ledger::*;
mod attachments;
pub use attachments::*;
mod index;
pub use index::*;
mod billing;
pub use billing::*;
