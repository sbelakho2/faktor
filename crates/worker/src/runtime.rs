//! The worker-side execution transport runtime (additive).
//!
//! One [`WorkerRuntime`] owns the worker half of the plane's protocol:
//!
//! 1. **register** with a registration token (the same
//!    `WorkerPlane::register` path every wire adapter uses);
//! 2. **long-poll/claim** eligible leases for its organization/trust domain
//!    AND capability advertisement ([`WorkerPlane::claim_next`] semantics —
//!    a job whose required toolchains/sandbox/network profile the worker
//!    does not advertise is never handed to it);
//! 3. **execute** the claimed job through the injected [`JobExecutor`] in an
//!    ISOLATED CANDIDATE WORKSPACE on the worker ([`CandidateWorkspace`]).
//!    The executor is the host's EXISTING pipeline (the daemon wires the
//!    same `TaskExecutor`/`AgentRuntime` stack the local path uses — there
//!    is no second engine here);
//! 4. **renew heartbeats** on a dedicated monitor thread while the executor
//!    runs; a heartbeat refusal is a lease LOSS: the runtime trips the
//!    executor's cancellation flag and DISCARDS the result;
//! 5. **submit** the digest-bound result (the digest is the job's immutable
//!    payload digest; the plane refuses anything else) together with the
//!    worker's verification claim, only while the lease generation is
//!    current;
//! 6. on lease loss — supersession by a newer generation, expiry, revocation
//!    or a non-live lease — cancel the local execution, remove the candidate
//!    workspace and return the typed [`RunOutcome::Superseded`]; the result
//!    is never submitted and never landed.
//!
//! The transport is a SYNC seam ([`WorkerTransport`]): a wire adapter (the
//! CLI's HTTP client over the daemon's checked transport) blocks inside its
//! methods, so the runtime loop is runtime-agnostic and deterministic under
//! a scripted transport (the tests drive the REAL `WorkerPlane` through an
//! in-process transport — "fake transport, real plane").
//!
//! Fail-closed rules:
//!
//! - a job whose payload the transport cannot produce is NOT executed and
//!   NOT submitted ([`RunOutcome::PayloadUnavailable`]); the lease then
//!   expires and requeues under the bounded policy;
//! - a fetched payload whose BLAKE3 digest does not equal the job's
//!   `payload_digest` is refused typed before any byte reaches the executor;
//! - a result submitted after any lease transition the plane refuses is
//!   discarded (never parked, never retried blindly);
//! - `enabled = false` (the default of the `[worker_node]` config) refuses
//!   to run at all ([`WorkerRuntimeError::Disabled`]) — no claim, no
//!   heartbeat, no submission.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use faktor_cloud::{Clock, SecretToken, SystemClock};
use serde::{Deserialize, Serialize};

use crate::error::{WorkerError, WORKER_PROTOCOL_VERSION};
use crate::ids::{ExecutionJobId, JobGeneration, WorkerId};
use crate::model::{
    clamp_heartbeat_interval, ExecutionJob, JobResultOutcome, JobVerificationClaim,
    WorkerCapabilities, WorkerLease, HEARTBEAT_MAX_INTERVAL_MS, HEARTBEAT_MIN_INTERVAL_MS,
    RESULT_DIGEST_BYTES,
};
use crate::service::{HeartbeatOutcome, RegistrationOutcome, ResultOutcome};

/// Bound on the payload one claim may carry (bounded everything: the worker
/// never buffers an unbounded job body).
pub const MAX_PAYLOAD_BYTES: usize = 1024 * 1024;
/// Bound on the long-poll deadline one `run_once` waits for work.
pub const MAX_CLAIM_DEADLINE_MS: i64 = 60_000;
/// Bound on the poll interval between claim attempts.
pub const MIN_CLAIM_INTERVAL_MS: i64 = 10;
/// Default cadence floor for heartbeats when no lease interval is available.
pub const DEFAULT_HEARTBEAT_CADENCE_MS: i64 = 1_000;
/// Bound on the iterations one `run_loop` pass executes (the caller loops).
pub const MAX_LOOP_ITERATIONS: u32 = 10_000;

/// One job payload as the transport delivers it (UTF-8 text: goals, plans,
/// prompts). The runtime verifies the BLAKE3 digest against the job's
/// immutable `payload_digest` before the executor sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobPayload {
    pub media_type: String,
    pub body: String,
}

impl JobPayload {
    /// The BLAKE3 hex digest of the payload body (the same digest the
    /// orchestrator's placement spec binds).
    pub fn digest(&self) -> String {
        blake3::hash(self.body.as_bytes()).to_hex().to_string()
    }
}

/// One claimed job as the transport hands it to the runtime: the immutable
/// job root, the live lease of its current generation, and the job payload
/// when the transport could resolve it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedJob {
    pub job: ExecutionJob,
    pub lease: WorkerLease,
    /// `None` = the payload could not be resolved; the runtime refuses to
    /// execute (fail closed) and never submits.
    pub payload: Option<JobPayload>,
}

/// Why a lease was lost mid-run. Every variant maps onto the plane's own
/// refusal surface; the runtime cancels locally and discards the result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseLoss {
    /// A newer generation exists: any result under this lease is stale.
    Superseded {
        job: String,
        generation: u64,
        current: u64,
    },
    /// The heartbeat window closed.
    Expired { lease: String, expired_at_ms: i64 },
    /// The lease is no longer live (revoked/completed/superseded-by-state).
    NotLive { lease: String, state: String },
    /// The transport itself failed; the lease may or may not be alive — the
    /// runtime still cancels (a result can never be submitted without a
    /// confirmed current lease).
    Transport(String),
}

impl LeaseLoss {
    pub fn detail(&self) -> String {
        match self {
            LeaseLoss::Superseded {
                job,
                generation,
                current,
            } => format!("job {job} generation {generation} superseded by {current}"),
            LeaseLoss::Expired {
                lease,
                expired_at_ms,
            } => format!("lease {lease} expired at {expired_at_ms} ms"),
            LeaseLoss::NotLive { lease, state } => {
                format!("lease {lease} is no longer live ({state})")
            }
            LeaseLoss::Transport(detail) => format!("lease transport failure: {detail}"),
        }
    }
}

/// The typed worker-runtime refusal surface.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkerRuntimeError {
    /// The runtime is disabled (`[worker_node] enabled = false`, the only
    /// state of every pre-existing config): nothing is claimed or executed.
    #[error("worker runtime is disabled (enable the [worker_node] section to run it)")]
    Disabled,
    #[error("worker runtime misconfigured: {0}")]
    Malformed(String),
    #[error("worker transport failed: {0}")]
    Transport(String),
    #[error("worker plane refused: {0}")]
    Refused(String),
}

impl From<WorkerError> for WorkerRuntimeError {
    fn from(e: WorkerError) -> Self {
        match e {
            WorkerError::Backend(m) => WorkerRuntimeError::Transport(m),
            other => WorkerRuntimeError::Refused(other.to_string()),
        }
    }
}

impl From<LeaseLoss> for WorkerRuntimeError {
    fn from(loss: LeaseLoss) -> Self {
        WorkerRuntimeError::Refused(loss.detail())
    }
}

/// The worker's remote transport: the control plane's claim/heartbeat/result
/// surface, as the host executes it (an HTTP client over the checked
/// transport in production, an in-process plane in tests).
///
/// Implementations MUST be blocking and `Send + Sync`. A refused mutation
/// (unknown lease, revoked worker, protocol skew) is returned as
/// [`WorkerRuntimeError::Refused`]; a failed connection as
/// [`WorkerRuntimeError::Transport`].
pub trait WorkerTransport: Send + Sync {
    /// Register/re-register this worker (the capability advertisement is
    /// reconciled by the plane).
    fn register(
        &self,
        worker_id: &WorkerId,
        token: &SecretToken,
        display_name: &str,
        capabilities: &WorkerCapabilities,
    ) -> Result<RegistrationOutcome, WorkerRuntimeError>;

    /// Long-poll one eligible lease. `Ok(None)` = no eligible work; the
    /// transport may block up to its own poll window before answering.
    fn claim(
        &self,
        worker_id: &WorkerId,
        token: &SecretToken,
        capabilities: &WorkerCapabilities,
    ) -> Result<Option<ClaimedJob>, WorkerRuntimeError>;

    /// Renew the lease while the job runs.
    fn heartbeat(
        &self,
        job: &ExecutionJob,
        lease: &WorkerLease,
    ) -> Result<HeartbeatOutcome, WorkerRuntimeError>;

    /// Submit the digest-bound result with the worker's verification claim.
    fn submit(
        &self,
        job: &ExecutionJob,
        lease: &WorkerLease,
        digest: &str,
        outcome: JobResultOutcome,
        verification: &JobVerificationClaim,
    ) -> Result<ResultOutcome, WorkerRuntimeError>;
}

/// One execution request: the immutable job, the payload the executor runs,
/// and the isolated candidate workspace the run must stay inside.
#[derive(Debug, Clone)]
pub struct JobExecutionRequest<'a> {
    pub job: &'a ExecutionJob,
    pub payload: &'a JobPayload,
    /// The isolated candidate workspace allocated for THIS attempt.
    pub candidate_root: &'a Path,
}

/// What the host pipeline reports back. `self_verified` may only be `true`
/// when the host's own deterministic verification ran to completion over the
/// produced work; `produced_digest` names the produced tree when the run
/// mutated one (`None` = read-only run).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobExecutionResult {
    pub succeeded: bool,
    pub self_verified: bool,
    pub produced_digest: Option<String>,
    pub detail: String,
}

impl JobExecutionResult {
    pub fn success(self_verified: bool, produced_digest: Option<String>) -> Self {
        Self {
            succeeded: true,
            self_verified,
            produced_digest,
            detail: String::new(),
        }
    }

    pub fn failure(detail: impl Into<String>) -> Self {
        Self {
            succeeded: false,
            self_verified: false,
            produced_digest: None,
            detail: detail.into(),
        }
    }

    pub fn claim(&self) -> JobVerificationClaim {
        JobVerificationClaim {
            self_verified: self.self_verified && self.succeeded,
            produced_digest: self.produced_digest.clone(),
        }
    }
}

/// The host's EXISTING execution machinery behind one seam. Implementations
/// drive the daemon's own pipeline (no second engine); they MUST poll
/// [`ExecutionControl::is_cancelled`] and abort promptly when it trips (the
/// runtime joins the thread before deciding the result).
pub trait JobExecutor: Send + Sync {
    fn execute(
        &self,
        request: JobExecutionRequest<'_>,
        control: &ExecutionControl,
    ) -> Result<JobExecutionResult, String>;
}

/// The cancellation/lease-loss channel the heartbeat monitor publishes and
/// the executor observes.
pub struct ExecutionControl {
    cancelled: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
    loss: Arc<Mutex<Option<LeaseLoss>>>,
}

impl std::fmt::Debug for ExecutionControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutionControl")
            .field("cancelled", &self.is_cancelled())
            .field("finished", &self.finished.load(Ordering::SeqCst))
            .field("loss", &self.lease_loss())
            .finish()
    }
}

impl ExecutionControl {
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            finished: Arc::new(AtomicBool::new(false)),
            loss: Arc::new(Mutex::new(None)),
        }
    }

    /// Whether the local execution must abort (the lease was lost).
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    /// The lease loss that tripped the cancellation, if any.
    pub fn lease_loss(&self) -> Option<LeaseLoss> {
        self.loss.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn lose(&self, loss: LeaseLoss) {
        let mut slot = self.loss.lock().unwrap_or_else(|p| p.into_inner());
        if slot.is_none() {
            *slot = Some(loss);
        }
        self.cancelled.store(true, Ordering::SeqCst);
    }

    fn finish(&self) {
        self.finished.store(true, Ordering::SeqCst);
    }

    fn is_finished(&self) -> bool {
        self.finished.load(Ordering::SeqCst)
    }
}

impl Default for ExecutionControl {
    fn default() -> Self {
        Self::new()
    }
}

/// The sleep seam (tests inject a no-op/scripted sleeper; production sleeps).
pub trait WorkerSleeper: Send + Sync {
    fn sleep_ms(&self, ms: i64);
}

/// The production sleeper.
pub struct SystemSleeper;

impl WorkerSleeper for SystemSleeper {
    fn sleep_ms(&self, ms: i64) {
        if ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(ms as u64));
        }
    }
}

/// The typed outcome of one `run_once` pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    /// No eligible work was claimed within the poll deadline.
    Idle { polls: u32 },
    /// The job executed and its digest-bound result landed (or was already
    /// landed — an idempotent replay).
    Completed {
        job_id: String,
        generation: u64,
        lease_id: String,
        outcome: JobResultOutcome,
        self_verified: bool,
        produced_digest: Option<String>,
        submit: ResultOutcome,
    },
    /// The lease was lost mid-run: the local execution was cancelled and the
    /// result DISCARDED (typed `SupersededLease` semantics; nothing was
    /// submitted). `loss` carries the typed cause.
    Superseded {
        job_id: String,
        generation: u64,
        lease_id: String,
        reason: String,
        loss: LeaseLoss,
    },
    /// The claim's payload could not be resolved (or failed its digest
    /// check): nothing executed, nothing submitted; the lease expires and
    /// requeues under the bounded policy.
    PayloadUnavailable {
        job_id: String,
        generation: u64,
        reason: String,
    },
    /// A local execution failure was reported to the plane as a `failed`
    /// result (the digest is still the job's payload digest).
    Failed {
        job_id: String,
        generation: u64,
        reason: String,
    },
}

/// The bounded report of one `run_loop` pass.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoopReport {
    pub idle: u32,
    pub completed: u32,
    pub superseded: u32,
    pub payload_unavailable: u32,
    pub failed: u32,
}

/// The worker-side runtime configuration. Strict and bounded; `enabled`
/// defaults to `false` everywhere the CLI builds it.
pub struct WorkerRuntimeConfig {
    /// `false` = the runtime refuses to run (disabled parity).
    pub enabled: bool,
    pub worker_id: WorkerId,
    pub display_name: String,
    pub capabilities: WorkerCapabilities,
    /// The worker's registration token (never logged; only the hash is
    /// durable plane-side).
    pub token: SecretToken,
    /// Long-poll deadline of one claim pass (clamped to
    /// `0..=MAX_CLAIM_DEADLINE_MS`).
    pub claim_deadline_ms: i64,
    /// Interval between claim attempts inside the deadline (>= 10 ms).
    pub claim_interval_ms: i64,
    /// Heartbeat cadence (clamped into the model's
    /// `HEARTBEAT_MIN_INTERVAL_MS..=HEARTBEAT_MAX_INTERVAL_MS`; the monitor
    /// never sleeps longer than the lease's own interval).
    pub heartbeat_interval_ms: i64,
    /// `true` (default) = remove the candidate workspace after a landed
    /// result (bounded disk); `false` = keep it for host-side evidence.
    pub discard_workspace_on_success: bool,
}

impl std::fmt::Debug for WorkerRuntimeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerRuntimeConfig")
            .field("enabled", &self.enabled)
            .field("worker_id", &self.worker_id)
            .field("display_name", &self.display_name)
            .field("claim_deadline_ms", &self.claim_deadline_ms)
            .field("claim_interval_ms", &self.claim_interval_ms)
            .field("heartbeat_interval_ms", &self.heartbeat_interval_ms)
            .field(
                "discard_workspace_on_success",
                &self.discard_workspace_on_success,
            )
            .finish()
    }
}

impl WorkerRuntimeConfig {
    /// Validate + normalize the bounded knobs. Called by the constructor
    /// (both enabled and disabled configurations must be well-formed; a
    /// disabled one is merely never run).
    pub fn validate(&mut self) -> Result<(), WorkerRuntimeError> {
        if self.display_name.is_empty() || self.display_name.len() > 256 {
            return Err(WorkerRuntimeError::Malformed(
                "display_name must be 1..=256 bytes".into(),
            ));
        }
        self.claim_deadline_ms = self.claim_deadline_ms.clamp(0, MAX_CLAIM_DEADLINE_MS);
        self.claim_interval_ms = self.claim_interval_ms.max(MIN_CLAIM_INTERVAL_MS);
        self.heartbeat_interval_ms = clamp_heartbeat_interval(self.heartbeat_interval_ms);
        self.capabilities
            .normalize()
            .map_err(|e| WorkerRuntimeError::Malformed(e.to_string()))?;
        Ok(())
    }
}

/// The isolated candidate workspace one attempt executes in: a bounded,
/// sanitized directory under the runtime's configured root, keyed by
/// `job_id/generation` so a replayed generation can never reuse a stale
/// tree. Removed on lease loss and (by default) after a landed result.
#[derive(Debug, Clone)]
pub struct CandidateWorkspace {
    root: PathBuf,
}

impl CandidateWorkspace {
    /// The deterministic candidate root of one job generation.
    pub fn allocate(
        workspace_root: &Path,
        job: &ExecutionJobId,
        generation: JobGeneration,
    ) -> Result<Self, WorkerRuntimeError> {
        // Typed ids are already bounded/printable; the join is still
        // re-checked so no id can ever escape the configured root.
        let root = workspace_root
            .join(job.as_str())
            .join(format!("g{}", generation.as_u64()));
        if !root.starts_with(workspace_root) {
            return Err(WorkerRuntimeError::Malformed(format!(
                "candidate workspace {root:?} escapes the configured root {workspace_root:?}"
            )));
        }
        if root.exists() {
            std::fs::remove_dir_all(&root).map_err(|e| {
                WorkerRuntimeError::Malformed(format!(
                    "candidate workspace reset {}: {e}",
                    root.display()
                ))
            })?;
        }
        std::fs::create_dir_all(&root).map_err(|e| {
            WorkerRuntimeError::Malformed(format!(
                "candidate workspace create {}: {e}",
                root.display()
            ))
        })?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Remove the candidate tree (bounded disk; idempotent).
    pub fn discard(&self) -> Result<(), WorkerRuntimeError> {
        if self.root.exists() {
            std::fs::remove_dir_all(&self.root).map_err(|e| {
                WorkerRuntimeError::Malformed(format!(
                    "candidate workspace discard {}: {e}",
                    self.root.display()
                ))
            })?;
        }
        // Best effort: drop the (now empty) per-job directory.
        if let Some(parent) = self.root.parent() {
            let _ = std::fs::remove_dir(parent);
        }
        Ok(())
    }
}

/// The worker-side runtime loop.
pub struct WorkerRuntime {
    config: WorkerRuntimeConfig,
    transport: Arc<dyn WorkerTransport>,
    executor: Arc<dyn JobExecutor>,
    sleeper: Arc<dyn WorkerSleeper>,
    clock: Arc<dyn Clock>,
    workspace_root: PathBuf,
    stop: Arc<AtomicBool>,
}

impl std::fmt::Debug for WorkerRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkerRuntime")
            .field("config", &self.config)
            .field("workspace_root", &self.workspace_root)
            .finish()
    }
}

impl WorkerRuntime {
    /// Assemble the runtime over the host's transport + executor. The
    /// configuration is validated here; a disabled configuration still
    /// constructs (so the daemon can report its parity state) and only
    /// refuses to RUN.
    pub fn new(
        mut config: WorkerRuntimeConfig,
        transport: Arc<dyn WorkerTransport>,
        executor: Arc<dyn JobExecutor>,
        sleeper: Arc<dyn WorkerSleeper>,
        clock: Arc<dyn Clock>,
        workspace_root: PathBuf,
    ) -> Result<Self, WorkerRuntimeError> {
        config.validate()?;
        Ok(Self {
            config,
            transport,
            executor,
            sleeper,
            clock,
            workspace_root,
            stop: Arc::new(AtomicBool::new(false)),
        })
    }

    /// The production construction path (system clock + system sleeper).
    pub fn with_system_services(
        config: WorkerRuntimeConfig,
        transport: Arc<dyn WorkerTransport>,
        executor: Arc<dyn JobExecutor>,
        workspace_root: PathBuf,
    ) -> Result<Self, WorkerRuntimeError> {
        Self::new(
            config,
            transport,
            executor,
            Arc::new(SystemSleeper),
            Arc::new(SystemClock),
            workspace_root,
        )
    }

    pub fn config(&self) -> &WorkerRuntimeConfig {
        &self.config
    }

    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Ask the loop to stop after the in-flight iteration (idempotent).
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }

    fn require_enabled(&self) -> Result<(), WorkerRuntimeError> {
        if self.config.enabled {
            Ok(())
        } else {
            Err(WorkerRuntimeError::Disabled)
        }
    }

    /// Register/re-register this worker with the plane.
    pub fn register(&self) -> Result<RegistrationOutcome, WorkerRuntimeError> {
        self.require_enabled()?;
        self.transport.register(
            &self.config.worker_id,
            &self.config.token,
            &self.config.display_name,
            &self.config.capabilities,
        )
    }

    /// One claim/execute/submit pass. Disabled = typed refusal before any
    /// network or filesystem action.
    pub fn run_once(&self) -> Result<RunOutcome, WorkerRuntimeError> {
        self.require_enabled()?;
        let mut polls: u32 = 0;
        let deadline = self.config.claim_deadline_ms;
        let interval = self.config.claim_interval_ms;
        let started = self.now_ms();
        loop {
            polls += 1;
            let claimed = self.transport.claim(
                &self.config.worker_id,
                &self.config.token,
                &self.config.capabilities,
            )?;
            match claimed {
                Some(claimed) => return self.execute_claimed(claimed),
                None => {
                    if self.stop.load(Ordering::SeqCst)
                        || self.now_ms().saturating_sub(started) >= deadline
                    {
                        return Ok(RunOutcome::Idle { polls });
                    }
                    self.sleeper.sleep_ms(interval);
                }
            }
        }
    }

    /// The bounded loop: register once, then `run_once` until the iteration
    /// budget, the stop flag, or a typed loop failure. Returns the tally.
    pub fn run_loop(&self, max_iterations: u32) -> Result<LoopReport, WorkerRuntimeError> {
        self.require_enabled()?;
        self.register()?;
        let mut report = LoopReport::default();
        let iterations = max_iterations.min(MAX_LOOP_ITERATIONS);
        for _ in 0..iterations {
            if self.stop.load(Ordering::SeqCst) {
                break;
            }
            match self.run_once()? {
                RunOutcome::Idle { .. } => report.idle += 1,
                RunOutcome::Completed { .. } => report.completed += 1,
                RunOutcome::Superseded { .. } => report.superseded += 1,
                RunOutcome::PayloadUnavailable { .. } => report.payload_unavailable += 1,
                RunOutcome::Failed { .. } => report.failed += 1,
            }
        }
        Ok(report)
    }

    /// The execute/heartbeat/submit cycle of one claimed job.
    fn execute_claimed(&self, claimed: ClaimedJob) -> Result<RunOutcome, WorkerRuntimeError> {
        let job = claimed.job;
        let lease = claimed.lease;
        let job_id = job.job_id.to_string();
        let generation = job.current_generation;
        let lease_id = lease.lease_id.to_string();
        // Fail closed: an unresolvable payload is never executed.
        let Some(payload) = claimed.payload else {
            return Ok(RunOutcome::PayloadUnavailable {
                job_id,
                generation: generation.as_u64(),
                reason: "the transport could not resolve the job payload".into(),
            });
        };
        // The payload is bound to the job's immutable digest: a mismatch is
        // refused before any byte reaches the executor.
        let actual = payload.digest();
        if actual != job.payload_digest {
            return Ok(RunOutcome::PayloadUnavailable {
                job_id,
                generation: generation.as_u64(),
                reason: format!(
                    "payload digest mismatch: expected {}, got {actual}",
                    job.payload_digest
                ),
            });
        }
        // Cross-check the caller's claim: the job row itself must carry a
        // valid 64-hex digest (the plane already enforces it; defense in
        // depth).
        if job.payload_digest.len() != RESULT_DIGEST_BYTES
            || !job.payload_digest.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(WorkerRuntimeError::Malformed(
                "claimed job carries a malformed payload digest".into(),
            ));
        }
        let candidate =
            CandidateWorkspace::allocate(&self.workspace_root, &job.job_id, generation)?;
        let control = Arc::new(ExecutionControl::new());
        let executor = self.executor.clone();
        let transport = self.transport.clone();
        let sleeper = self.sleeper.clone();
        let cadence = self
            .config
            .heartbeat_interval_ms
            .min(lease.heartbeat_interval_ms.max(HEARTBEAT_MIN_INTERVAL_MS))
            .clamp(HEARTBEAT_MIN_INTERVAL_MS, HEARTBEAT_MAX_INTERVAL_MS);
        let job_for_monitor = job.clone();
        let lease_for_monitor = lease.clone();
        let control_for_monitor = control.clone();
        let slot: Mutex<Option<Result<JobExecutionResult, String>>> = Mutex::new(None);
        std::thread::scope(|scope| {
            let monitor = scope.spawn(move || {
                while !control_for_monitor.is_finished() {
                    sleeper.sleep_ms(cadence);
                    if control_for_monitor.is_finished() {
                        break;
                    }
                    match transport.heartbeat(&job_for_monitor, &lease_for_monitor) {
                        Ok(_) => {}
                        Err(e) => {
                            control_for_monitor.lose(transport_error_into_loss(&e));
                            break;
                        }
                    }
                }
            });
            let request = JobExecutionRequest {
                job: &job,
                payload: &payload,
                candidate_root: candidate.root(),
            };
            let result = executor.execute(request, &control);
            *slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(result);
            control.finish();
            // The monitor observes `finished` at most one cadence later; the
            // join bounds the runtime to one heartbeat worth of wait.
            let _ = monitor.join();
        });
        let exec_result = slot
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .unwrap_or_else(|| Err("executor returned no result".into()));
        // Lease loss wins over any execution result: the work is discarded.
        if let Some(loss) = control.lease_loss() {
            candidate.discard()?;
            return Ok(RunOutcome::Superseded {
                job_id,
                generation: generation.as_u64(),
                lease_id,
                reason: loss.detail(),
                loss,
            });
        }
        let (result, outcome) = match exec_result {
            Ok(result) if result.succeeded => (result, JobResultOutcome::Succeeded),
            Ok(result) => (result, JobResultOutcome::Failed),
            Err(detail) => (
                JobExecutionResult::failure(detail),
                JobResultOutcome::Failed,
            ),
        };
        let verification = result.claim();
        match self
            .transport
            .submit(&job, &lease, &job.payload_digest, outcome, &verification)
        {
            Ok(submit) => {
                if self.config.discard_workspace_on_success {
                    candidate.discard()?;
                }
                if outcome == JobResultOutcome::Failed {
                    return Ok(RunOutcome::Failed {
                        job_id,
                        generation: generation.as_u64(),
                        reason: result.detail,
                    });
                }
                Ok(RunOutcome::Completed {
                    job_id,
                    generation: generation.as_u64(),
                    lease_id,
                    outcome,
                    self_verified: verification.self_verified,
                    produced_digest: verification.produced_digest,
                    submit,
                })
            }
            // The plane refused the result (superseded/expired/not live or
            // the worker was revoked): the result is DISCARDED, never
            // parked.
            Err(e @ WorkerRuntimeError::Refused(_)) => {
                candidate.discard()?;
                Ok(RunOutcome::Superseded {
                    job_id,
                    generation: generation.as_u64(),
                    lease_id,
                    reason: e.to_string(),
                    loss: LeaseLoss::NotLive {
                        lease: String::new(),
                        state: e.to_string(),
                    },
                })
            }
            Err(e) => {
                candidate.discard()?;
                Err(e)
            }
        }
    }
}

/// Map one transport heartbeat/submit failure onto a lease loss.
fn transport_error_into_loss(e: &WorkerRuntimeError) -> LeaseLoss {
    match e {
        WorkerRuntimeError::Refused(detail) => LeaseLoss::NotLive {
            lease: String::new(),
            state: detail.clone(),
        },
        other => LeaseLoss::Transport(other.to_string()),
    }
}

/// An in-process transport over a REAL [`crate::service::WorkerPlane`]: the
/// deterministic "fake transport, real plane" used by the tests (and by the
/// embedded host). Every mutation runs through the plane's actual CAS,
/// heartbeat and landing logic — nothing is stubbed except the wire.
pub struct InProcessTransport {
    plane: Arc<crate::service::WorkerPlane>,
    organization: faktor_cloud::OrganizationId,
    payloads: Mutex<BTreeMap<String, JobPayload>>,
    tokens: Mutex<BTreeMap<String, SecretToken>>,
    protocol_version: u32,
}

impl InProcessTransport {
    pub fn new(
        plane: Arc<crate::service::WorkerPlane>,
        organization: faktor_cloud::OrganizationId,
    ) -> Self {
        Self {
            plane,
            organization,
            payloads: Mutex::new(BTreeMap::new()),
            tokens: Mutex::new(BTreeMap::new()),
            protocol_version: WORKER_PROTOCOL_VERSION,
        }
    }

    /// Bind one worker's registration token (the embedded host holds the
    /// token of the worker it IS; the wire transport carries it in a header).
    pub fn bind_token(&self, worker_id: &WorkerId, token: SecretToken) {
        self.tokens
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(worker_id.as_str().to_string(), token);
    }

    /// Stage one job payload (keyed by the job's immutable digest), exactly
    /// like the daemon's placement adapter stages the run goal before the
    /// plane mints the generation.
    pub fn stage_payload(&self, digest: &str, media_type: &str, body: &str) {
        self.payloads
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                digest.to_string(),
                JobPayload {
                    media_type: media_type.to_string(),
                    body: body.to_string(),
                },
            );
    }

    /// Override the presented protocol version (adversarial skew tests).
    pub fn with_protocol_version(mut self, version: u32) -> Self {
        self.protocol_version = version;
        self
    }
}

impl WorkerTransport for InProcessTransport {
    fn register(
        &self,
        worker_id: &WorkerId,
        token: &SecretToken,
        display_name: &str,
        capabilities: &WorkerCapabilities,
    ) -> Result<RegistrationOutcome, WorkerRuntimeError> {
        Ok(self.plane.register(
            &self.organization,
            worker_id,
            token,
            capabilities.clone(),
            display_name,
        )?)
    }

    fn claim(
        &self,
        worker_id: &WorkerId,
        token: &SecretToken,
        _capabilities: &WorkerCapabilities,
    ) -> Result<Option<ClaimedJob>, WorkerRuntimeError> {
        // Open generations first (the worker's own CAS accept); a generation
        // the scheduler already leased FOR this worker is adopted second, so
        // a long-polling worker never sits idle on work already assigned to
        // it by the placement seam.
        let claimed = match self.plane.claim_next(
            &self.organization,
            worker_id,
            token,
            self.protocol_version,
        )? {
            Some(claimed) => Some(claimed),
            None => self.plane.adoptable_lease(
                &self.organization,
                worker_id,
                token,
                self.protocol_version,
            )?,
        };
        let Some(claimed) = claimed else {
            return Ok(None);
        };
        let payload = self
            .payloads
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&claimed.job.payload_digest)
            .cloned();
        Ok(Some(ClaimedJob {
            job: claimed.job,
            lease: claimed.lease,
            payload,
        }))
    }

    fn heartbeat(
        &self,
        _job: &ExecutionJob,
        lease: &WorkerLease,
    ) -> Result<HeartbeatOutcome, WorkerRuntimeError> {
        Ok(self.plane.heartbeat(
            &self.organization,
            &lease.worker_id,
            &self.token_for(lease)?,
            self.protocol_version,
            &lease.lease_id,
            lease.generation,
        )?)
    }

    fn submit(
        &self,
        job: &ExecutionJob,
        lease: &WorkerLease,
        digest: &str,
        outcome: JobResultOutcome,
        verification: &JobVerificationClaim,
    ) -> Result<ResultOutcome, WorkerRuntimeError> {
        Ok(self.plane.submit_result_with_claim(
            &self.organization,
            &lease.worker_id,
            &self.token_for(lease)?,
            self.protocol_version,
            &job.job_id,
            lease.generation,
            &lease.lease_id,
            digest,
            outcome,
            verification.clone(),
        )?)
    }
}

impl InProcessTransport {
    fn token_for(&self, lease: &WorkerLease) -> Result<SecretToken, WorkerRuntimeError> {
        // The in-process transport holds the worker's own token: the lease
        // names the worker, the token is the credential for it. The host
        // supplies both (a wire transport carries the token in the header).
        self.tokens
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(lease.worker_id.as_str())
            .cloned()
            .ok_or_else(|| {
                WorkerRuntimeError::Refused(format!(
                    "no in-process token registered for worker {}",
                    lease.worker_id
                ))
            })
    }
}

/// A transport that refuses every mutation before any effect (disabled /
/// misconfigured host: the typed refusal is loud, never a silent local run).
pub struct RefusingTransport {
    pub reason: String,
}

impl WorkerTransport for RefusingTransport {
    fn register(
        &self,
        _worker_id: &WorkerId,
        _token: &SecretToken,
        _display_name: &str,
        _capabilities: &WorkerCapabilities,
    ) -> Result<RegistrationOutcome, WorkerRuntimeError> {
        Err(WorkerRuntimeError::Transport(self.reason.clone()))
    }

    fn claim(
        &self,
        _worker_id: &WorkerId,
        _token: &SecretToken,
        _capabilities: &WorkerCapabilities,
    ) -> Result<Option<ClaimedJob>, WorkerRuntimeError> {
        Err(WorkerRuntimeError::Transport(self.reason.clone()))
    }

    fn heartbeat(
        &self,
        _job: &ExecutionJob,
        _lease: &WorkerLease,
    ) -> Result<HeartbeatOutcome, WorkerRuntimeError> {
        Err(WorkerRuntimeError::Transport(self.reason.clone()))
    }

    fn submit(
        &self,
        _job: &ExecutionJob,
        _lease: &WorkerLease,
        _digest: &str,
        _outcome: JobResultOutcome,
        _verification: &JobVerificationClaim,
    ) -> Result<ResultOutcome, WorkerRuntimeError> {
        Err(WorkerRuntimeError::Transport(self.reason.clone()))
    }
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod runtime_tests;
