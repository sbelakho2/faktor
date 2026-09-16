//! faktor-worker — the remote/VPC worker plane of the control plane.
//!
//! Layout:
//!
//! - [`ids`]: typed worker/job/lease identities and immutable
//!   [`ids::JobGeneration`] numbers;
//! - [`model`]: the versioned [`model::WorkerCapabilities`] advertisement,
//!   [`model::WorkerRegistration`], [`model::ExecutionJob`] roots,
//!   append-only [`model::JobGenerationRow`]s, [`model::WorkerLease`]s,
//!   bounded [`model::JobAttempt`]s, landed [`model::JobResult`]s and the
//!   append-only [`model::JournalEntry`] stream;
//! - [`store`]: the [`store::WorkerStore`] durable seam plus its in-memory
//!   and SQLite implementations (the SQLite store owns its OWN database
//!   file and migration ladder: enabling the worker plane never touches the
//!   commercial `user_version`);
//! - [`service`]: the ONE [`service::WorkerPlane`] authority — token
//!   minting, registration/reconciliation, scheduling of immutable
//!   generations, CAS lease acceptance, heartbeat renewal with bounded
//!   staleness, generation-checked result landing, revocation and
//!   deterministic recovery.
//!
//! Everything enters through the service: there is no other writer of
//! worker state, no bare lease mutation, and no result acceptance path that
//! skips the generation check.

pub mod error;
pub mod ids;
pub mod model;
pub mod service;
pub mod store;

pub use error::{WorkerError, WORKER_PROTOCOL_VERSION};
pub use ids::{
    ExecutionJobId, JobGeneration, JobKey, WorkerId, WorkerLeaseId, WorkerTokenLabel, MAX_ID_BYTES,
    MAX_JOB_KEY_BYTES,
};
pub use model::{
    clamp_heartbeat_interval, AttemptState, ExecutionJob, GenerationState, GpuCapability,
    JobAttempt, JobGenerationRow, JobRequirements, JobResult, JobResultOutcome, JobState,
    JobStatus, JournalEntry, LeaseState, NetworkProfile, RequeuePolicy, SandboxCapability,
    WorkerCapabilities, WorkerLease, WorkerPage, WorkerRegistration, WorkerTokenRow, WorkerView,
    DEFAULT_MAX_ATTEMPTS, HEARTBEAT_DEFAULT_INTERVAL_MS, HEARTBEAT_MAX_INTERVAL_MS,
    HEARTBEAT_MIN_INTERVAL_MS, MAX_LIVE_LEASES_PER_WORKER, MAX_REQUEUE_ATTEMPTS,
    RESULT_DIGEST_BYTES,
};
pub use service::{
    default_requeue, HeartbeatOutcome, IssuedWorkerToken, RecoveryReport, RegistrationOutcome,
    ResultOutcome, ScheduledJob, WorkerPlane, MAX_JOURNAL_PAGE, MAX_WORKER_PAGE,
};
pub use store::{
    schema_version, MemoryWorkerStore, ResultAppend, SqliteWorkerStore, WorkerStore,
    WORKER_SCHEMA_V1,
};

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
