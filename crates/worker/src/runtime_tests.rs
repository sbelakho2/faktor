//! Adversarial tests of the worker-side execution transport runtime: the
//! full fake-transport round trip (place -> claim -> execute -> submit),
//! superseded-lease cancellation that discards results, heartbeat loss that
//! cancels locally and requeues under the bounded policy, capability
//! mismatches that are never assigned, protocol skew and disabled parity.
//!
//! Every scenario drives the REAL [`crate::service::WorkerPlane`] through the
//! in-process transport: nothing is stubbed except the wire, the clock and
//! the sleeper.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use faktor_cloud::{ManualClock, OrganizationId, SecretToken};

use crate::error::WORKER_PROTOCOL_VERSION;
use crate::ids::{JobKey, WorkerId};
use crate::model::{
    JobRequirements, JobResultOutcome, JobState, LeaseState, NetworkProfile, RequeuePolicy,
    SandboxCapability, WorkerCapabilities, WorkerLease,
};
use crate::runtime::{
    ClaimedJob, ExecutionControl, InProcessTransport, JobExecutionRequest, JobExecutionResult,
    JobExecutor, LeaseLoss, RunOutcome, WorkerRuntime, WorkerRuntimeConfig, WorkerRuntimeError,
    WorkerSleeper, WorkerTransport,
};
use crate::service::{HeartbeatOutcome, RegistrationOutcome, ResultOutcome, WorkerPlane};
use crate::store::MemoryWorkerStore;

fn org() -> OrganizationId {
    OrganizationId::try_new("org_1").unwrap()
}

fn worker_id(id: &str) -> WorkerId {
    WorkerId::try_new(id).unwrap()
}

fn caps(toolchains: &[&str]) -> WorkerCapabilities {
    WorkerCapabilities {
        os: "linux".into(),
        arch: "x86_64".into(),
        toolchains: toolchains.iter().map(|t| t.to_string()).collect(),
        sandbox: vec![SandboxCapability::try_new("seccomp").unwrap()],
        network: NetworkProfile::EgressRestricted,
        cpu_cores: 4,
        memory_mb: 8_192,
        gpu: None,
        region: "eu-west".into(),
        trust_domain: "org_1".into(),
        protocol_version: WORKER_PROTOCOL_VERSION,
    }
}

fn requirements(toolchains: &[&str]) -> JobRequirements {
    JobRequirements {
        os: Some("linux".into()),
        arch: Some("x86_64".into()),
        toolchains: toolchains.iter().map(|t| t.to_string()).collect(),
        sandbox: vec![SandboxCapability::try_new("seccomp").unwrap()],
        network: Some(NetworkProfile::EgressRestricted),
        min_cpu_cores: 1,
        min_memory_mb: 1_024,
        gpu: false,
        region: Some("eu-west".into()),
        trust_domain: "org_1".into(),
    }
}

/// The real plane over an in-memory store with a manual clock.
struct Harness {
    plane: Arc<WorkerPlane>,
    clock: Arc<ManualClock>,
}

impl Harness {
    fn new() -> Self {
        let store = Arc::new(MemoryWorkerStore::new());
        let clock = Arc::new(ManualClock::new(1_000_000));
        Self {
            plane: WorkerPlane::new(store, clock.clone()),
            clock,
        }
    }

    fn register(&self, worker: &str, toolchains: &[&str]) -> (WorkerId, SecretToken) {
        let issued = self
            .plane
            .mint_registration_token(&org(), "org_1", "test")
            .unwrap();
        let token = SecretToken::try_new(issued.token.expose().to_string()).unwrap();
        let id = worker_id(worker);
        self.plane
            .register(&org(), &id, &token, caps(toolchains), worker)
            .unwrap();
        (id, token)
    }

    fn schedule(
        &self,
        key: &str,
        digest: &str,
        toolchains: &[&str],
    ) -> crate::service::ScheduledJob {
        self.plane
            .schedule_job(
                &org(),
                "org_1",
                &JobKey::try_new(key).unwrap(),
                requirements(toolchains),
                digest,
                RequeuePolicy { max_attempts: 3 },
                None,
            )
            .unwrap()
    }
}

/// A sleeper that never blocks (the monitor observes cancellation promptly in
/// tests); production uses the system sleeper.
struct YieldSleeper;

impl WorkerSleeper for YieldSleeper {
    fn sleep_ms(&self, _ms: i64) {
        std::thread::yield_now();
    }
}

#[derive(Clone)]
enum Mode {
    Success {
        self_verified: bool,
        produced_digest: Option<String>,
    },
    /// The first call blocks until the lease-loss cancellation trips (then
    /// returns a SUCCESS that must be discarded); later calls succeed.
    BlockFirstThenSucceed,
}

struct Observed {
    job_id: String,
    generation: u64,
    candidate_root: std::path::PathBuf,
}

struct ScriptedExecutor {
    calls: AtomicUsize,
    mode: Mode,
    observed: Mutex<Vec<Observed>>,
}

impl ScriptedExecutor {
    fn new(mode: Mode) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            mode,
            observed: Mutex::new(Vec::new()),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn observed(&self) -> Vec<Observed> {
        self.observed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|o| Observed {
                job_id: o.job_id.clone(),
                generation: o.generation,
                candidate_root: o.candidate_root.clone(),
            })
            .collect()
    }
}

impl JobExecutor for ScriptedExecutor {
    fn execute(
        &self,
        request: JobExecutionRequest<'_>,
        control: &ExecutionControl,
    ) -> Result<JobExecutionResult, String> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        self.observed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(Observed {
                job_id: request.job.job_id.to_string(),
                generation: request.job.current_generation.as_u64(),
                candidate_root: request.candidate_root.to_path_buf(),
            });
        if call == 0 {
            if let Mode::BlockFirstThenSucceed = self.mode {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while !control.is_cancelled() {
                    if std::time::Instant::now() > deadline {
                        return Err("executor never observed the lease-loss cancellation".into());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                // The result exists but MUST be discarded: the lease was lost.
                return Ok(JobExecutionResult::success(true, None));
            }
        }
        match &self.mode {
            Mode::Success {
                self_verified,
                produced_digest,
            } => Ok(JobExecutionResult {
                succeeded: true,
                self_verified: *self_verified,
                produced_digest: produced_digest.clone(),
                detail: String::new(),
            }),
            Mode::BlockFirstThenSucceed => Ok(JobExecutionResult::success(true, None)),
        }
    }
}

/// A transport wrapper that refuses the first `failures` heartbeat calls
/// (a lease loss from the worker's point of view), delegating everything
/// else to the real in-process transport.
struct FlakyHeartbeatTransport {
    inner: InProcessTransport,
    failures: AtomicUsize,
}

impl FlakyHeartbeatTransport {
    fn new(inner: InProcessTransport, failures: usize) -> Self {
        Self {
            inner,
            failures: AtomicUsize::new(failures),
        }
    }
}

impl WorkerTransport for FlakyHeartbeatTransport {
    fn register(
        &self,
        worker_id: &WorkerId,
        token: &SecretToken,
        display_name: &str,
        capabilities: &WorkerCapabilities,
    ) -> Result<RegistrationOutcome, WorkerRuntimeError> {
        self.inner
            .register(worker_id, token, display_name, capabilities)
    }

    fn claim(
        &self,
        worker_id: &WorkerId,
        token: &SecretToken,
        capabilities: &WorkerCapabilities,
    ) -> Result<Option<ClaimedJob>, WorkerRuntimeError> {
        self.inner.claim(worker_id, token, capabilities)
    }

    fn heartbeat(
        &self,
        job: &crate::model::ExecutionJob,
        lease: &WorkerLease,
    ) -> Result<HeartbeatOutcome, WorkerRuntimeError> {
        if self
            .failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                if left > 0 {
                    Some(left - 1)
                } else {
                    None
                }
            })
            .is_ok()
        {
            return Err(WorkerRuntimeError::Refused(
                "simulated heartbeat refusal (lease lost)".into(),
            ));
        }
        self.inner.heartbeat(job, lease)
    }

    fn submit(
        &self,
        job: &crate::model::ExecutionJob,
        lease: &WorkerLease,
        digest: &str,
        outcome: JobResultOutcome,
        verification: &crate::model::JobVerificationClaim,
    ) -> Result<ResultOutcome, WorkerRuntimeError> {
        self.inner.submit(job, lease, digest, outcome, verification)
    }
}

fn digest_of(body: &str) -> String {
    blake3::hash(body.as_bytes()).to_hex().to_string()
}

fn runtime_config(worker_id: WorkerId, token: SecretToken, enabled: bool) -> WorkerRuntimeConfig {
    WorkerRuntimeConfig {
        enabled,
        worker_id,
        display_name: "test-worker".into(),
        capabilities: caps(&["rust"]),
        token,
        claim_deadline_ms: 0,
        claim_interval_ms: 10,
        heartbeat_interval_ms: 1_000,
        discard_workspace_on_success: true,
    }
}

fn runtime(
    config: WorkerRuntimeConfig,
    transport: Arc<dyn WorkerTransport>,
    executor: Arc<dyn JobExecutor>,
    clock: Arc<ManualClock>,
    workspace_root: &std::path::Path,
) -> WorkerRuntime {
    WorkerRuntime::new(
        config,
        transport,
        executor,
        Arc::new(YieldSleeper),
        clock,
        workspace_root.to_path_buf(),
    )
    .unwrap()
}

/// place (open generation) -> claim (worker CAS) -> execute -> submit.
#[test]
fn open_generation_round_trips_claim_execute_and_submit() {
    let h = Harness::new();
    let body = "implement the thing";
    let payload_digest = digest_of(body);
    // Scheduled BEFORE any worker registers: the generation is open, so the
    // worker's own CAS accept is the claim.
    let scheduled = h.schedule("job-key-1", &payload_digest, &["rust"]);
    assert!(
        scheduled.lease.is_none(),
        "no worker existed at schedule time"
    );
    let (id, token) = h.register("wrk_1", &["rust"]);

    let transport = Arc::new(InProcessTransport::new(h.plane.clone(), org()));
    transport.bind_token(&id, token.clone());
    transport.stage_payload(&payload_digest, "text/plain", body);
    let executor = Arc::new(ScriptedExecutor::new(Mode::Success {
        self_verified: true,
        produced_digest: None,
    }));
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(
        runtime_config(id.clone(), token, true),
        transport,
        executor.clone(),
        h.clock.clone(),
        &dir.path().join("ws"),
    );

    let outcome = runtime.run_once().unwrap();
    match outcome {
        RunOutcome::Completed {
            job_id,
            generation,
            outcome,
            self_verified,
            submit,
            ..
        } => {
            assert_eq!(job_id, scheduled.job.job_id.to_string());
            assert_eq!(generation, 1);
            assert_eq!(outcome, JobResultOutcome::Succeeded);
            assert!(self_verified);
            assert_eq!(submit, ResultOutcome::Landed);
        }
        other => panic!("expected a landed completion, got {other:?}"),
    }
    let status = h.plane.job_status(&org(), &scheduled.job.job_id).unwrap();
    assert_eq!(status.job.state, JobState::Completed);
    let result = status.result.expect("the bound result landed");
    assert_eq!(result.digest, payload_digest);
    assert!(result.verification.self_verified);
    assert_eq!(status.lease.expect("lease").state, LeaseState::Completed);

    // The executor ran inside an isolated candidate workspace under the
    // configured root and the workspace was discarded after the landing.
    let observed = executor.observed();
    assert_eq!(observed.len(), 1);
    assert!(observed[0]
        .candidate_root
        .starts_with(dir.path().join("ws")));
    assert!(!observed[0].candidate_root.exists());
}

/// place (scheduler leases for an eligible worker) -> claim (adopt) ->
/// execute -> submit.
#[test]
fn scheduler_leased_generation_is_adopted_and_completed() {
    let h = Harness::new();
    let body = "adopted work";
    let payload_digest = digest_of(body);
    let (id, token) = h.register("wrk_1", &["rust"]);
    let scheduled = h.schedule("job-key-2", &payload_digest, &["rust"]);
    assert!(
        scheduled.lease.is_some(),
        "the scheduler CAS-accepts the placement lease"
    );

    let transport = Arc::new(InProcessTransport::new(h.plane.clone(), org()));
    transport.bind_token(&id, token.clone());
    transport.stage_payload(&payload_digest, "text/plain", body);
    let executor = Arc::new(ScriptedExecutor::new(Mode::Success {
        self_verified: true,
        produced_digest: None,
    }));
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(
        runtime_config(id, token, true),
        transport,
        executor,
        h.clock.clone(),
        &dir.path().join("ws"),
    );
    assert!(matches!(
        runtime.run_once().unwrap(),
        RunOutcome::Completed { generation: 1, .. }
    ));
    let status = h.plane.job_status(&org(), &scheduled.job.job_id).unwrap();
    assert_eq!(status.job.state, JobState::Completed);
}

/// A lease lost mid-run cancels the local execution, discards its result
/// (typed `SupersededLease` semantics) and the interrupted attempt requeues
/// under the bounded policy; the new generation then executes normally.
#[test]
fn heartbeat_loss_cancels_locally_discards_and_requeues() {
    let h = Harness::new();
    let body = "long running work";
    let payload_digest = digest_of(body);
    let (id, token) = h.register("wrk_1", &["rust"]);
    let scheduled = h.schedule("job-key-3", &payload_digest, &["rust"]);

    let inner = InProcessTransport::new(h.plane.clone(), org());
    inner.bind_token(&id, token.clone());
    inner.stage_payload(&payload_digest, "text/plain", body);
    let transport = Arc::new(FlakyHeartbeatTransport::new(inner, 1));
    let executor = Arc::new(ScriptedExecutor::new(Mode::BlockFirstThenSucceed));
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(
        runtime_config(id.clone(), token.clone(), true),
        transport.clone(),
        executor.clone(),
        h.clock.clone(),
        &dir.path().join("ws"),
    );

    // First pass: the heartbeat refusal trips the lease loss; the executor
    // observes cancellation and its SUCCESS result is discarded.
    let outcome = runtime.run_once().unwrap();
    match outcome {
        RunOutcome::Superseded {
            job_id,
            generation,
            loss,
            ..
        } => {
            assert_eq!(job_id, scheduled.job.job_id.to_string());
            assert_eq!(generation, 1);
            assert!(
                matches!(loss, LeaseLoss::NotLive { .. }),
                "typed lease-loss cause, got {loss:?}"
            );
        }
        other => panic!("expected the typed SupersededLease outcome, got {other:?}"),
    }
    assert_eq!(executor.calls(), 1, "the executor ran exactly once");
    let status = h.plane.job_status(&org(), &scheduled.job.job_id).unwrap();
    assert!(
        status.result.is_none(),
        "a superseded result is never landed"
    );

    // The lease window closes without heartbeats: recovery expires it,
    // terminals the attempt and requeues generation 2 under the policy.
    h.clock.advance(60_000);
    let report = h.plane.recover(&org()).unwrap();
    assert_eq!(report.expired.len(), 1);
    assert_eq!(report.requeued.len(), 1);
    let status = h.plane.job_status(&org(), &scheduled.job.job_id).unwrap();
    assert_eq!(status.job.current_generation.as_u64(), 2);
    assert_eq!(
        status.job.state,
        JobState::Leased,
        "the requeued generation is immediately re-assigned to the eligible worker"
    );
    assert_eq!(status.generations.len(), 2);
    assert_eq!(
        status.generations[0].state,
        crate::model::GenerationState::Lost
    );
    assert_eq!(
        status.generations[1].state,
        crate::model::GenerationState::Leased,
        "the re-assigned requeued generation is leased again"
    );
    assert_eq!(
        status.lease.as_ref().map(|lease| lease.generation.as_u64()),
        Some(2)
    );

    // The same worker claims generation 2 and completes it: the requeue is
    // real work, not a terminal.
    let outcome = runtime.run_once().unwrap();
    assert!(matches!(
        outcome,
        RunOutcome::Completed { generation: 2, .. }
    ));
    let status = h.plane.job_status(&org(), &scheduled.job.job_id).unwrap();
    assert_eq!(status.job.state, JobState::Completed);
    assert_eq!(status.result.unwrap().generation.as_u64(), 2);
}

/// A capability mismatch is never assigned: the long-poll returns no work,
/// nothing executes and no lease exists.
#[test]
fn capability_mismatch_is_never_assigned() {
    let h = Harness::new();
    let body = "rust work";
    let payload_digest = digest_of(body);
    let (id, token) = h.register("wrk_1", &["go"]);
    let scheduled = h.schedule("job-key-4", &payload_digest, &["rust"]);

    let transport = Arc::new(InProcessTransport::new(h.plane.clone(), org()));
    transport.bind_token(&id, token.clone());
    transport.stage_payload(&payload_digest, "text/plain", body);
    let executor = Arc::new(ScriptedExecutor::new(Mode::Success {
        self_verified: true,
        produced_digest: None,
    }));
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(
        runtime_config(id, token, true),
        transport,
        executor.clone(),
        h.clock.clone(),
        &dir.path().join("ws"),
    );
    assert!(matches!(
        runtime.run_once().unwrap(),
        RunOutcome::Idle { .. }
    ));
    assert_eq!(executor.calls(), 0);
    let status = h.plane.job_status(&org(), &scheduled.job.job_id).unwrap();
    assert_eq!(status.job.state, JobState::Assigned);
    assert!(status.lease.is_none());
    assert!(status.result.is_none());
}

/// Protocol skew is refused by the plane before any claim or execution.
#[test]
fn protocol_skew_is_refused() {
    let h = Harness::new();
    let (id, token) = h.register("wrk_1", &["rust"]);
    let transport = Arc::new(
        InProcessTransport::new(h.plane.clone(), org())
            .with_protocol_version(WORKER_PROTOCOL_VERSION + 1),
    );
    let executor = Arc::new(ScriptedExecutor::new(Mode::Success {
        self_verified: true,
        produced_digest: None,
    }));
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(
        runtime_config(id, token, true),
        transport,
        executor.clone(),
        h.clock.clone(),
        &dir.path().join("ws"),
    );
    let err = runtime.run_once().unwrap_err();
    assert!(
        matches!(err, WorkerRuntimeError::Refused(ref message) if message.contains("protocol")),
        "expected a typed protocol refusal, got {err:?}"
    );
    assert_eq!(executor.calls(), 0);
}

/// A payload the transport cannot resolve, or one whose digest does not bind
/// to the job, is never executed and never submitted (fail closed).
#[test]
fn unbound_payload_is_never_executed() {
    let h = Harness::new();
    let body = "the real goal";
    let payload_digest = digest_of(body);
    let (id, token) = h.register("wrk_1", &["rust"]);
    let scheduled = h.schedule("job-key-5", &payload_digest, &["rust"]);

    let transport = Arc::new(InProcessTransport::new(h.plane.clone(), org()));
    transport.bind_token(&id, token.clone());
    // A DIFFERENT body staged under the job's digest: the digest check must
    // refuse before any byte reaches the executor.
    transport.stage_payload(&payload_digest, "text/plain", "tampered goal");
    let executor = Arc::new(ScriptedExecutor::new(Mode::Success {
        self_verified: true,
        produced_digest: None,
    }));
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(
        runtime_config(id, token, true),
        transport,
        executor.clone(),
        h.clock.clone(),
        &dir.path().join("ws"),
    );
    match runtime.run_once().unwrap() {
        RunOutcome::PayloadUnavailable { reason, .. } => {
            assert!(reason.contains("digest mismatch"), "{reason}");
        }
        other => panic!("expected a payload refusal, got {other:?}"),
    }
    assert_eq!(executor.calls(), 0);
    let status = h.plane.job_status(&org(), &scheduled.job.job_id).unwrap();
    assert!(status.result.is_none());
}

/// Disabled parity: a disabled configuration refuses to register, claim,
/// heartbeat or submit — nothing touches the plane and no executor runs.
#[test]
fn disabled_runtime_refuses_before_any_effect() {
    let h = Harness::new();
    let (id, token) = h.register("wrk_1", &["rust"]);
    let transport = Arc::new(InProcessTransport::new(h.plane.clone(), org()));
    transport.bind_token(&id, token.clone());
    let executor = Arc::new(ScriptedExecutor::new(Mode::Success {
        self_verified: true,
        produced_digest: None,
    }));
    let dir = tempfile::tempdir().unwrap();
    let runtime = runtime(
        runtime_config(id, token, false),
        transport,
        executor.clone(),
        h.clock.clone(),
        &dir.path().join("ws"),
    );
    assert!(!runtime.is_enabled());
    assert!(matches!(
        runtime.register().unwrap_err(),
        WorkerRuntimeError::Disabled
    ));
    assert!(matches!(
        runtime.run_once().unwrap_err(),
        WorkerRuntimeError::Disabled
    ));
    assert!(matches!(
        runtime.run_loop(4).unwrap_err(),
        WorkerRuntimeError::Disabled
    ));
    assert_eq!(executor.calls(), 0);
    let page = h.plane.journal_page(&org(), None, 16).unwrap();
    assert!(
        !page.iter().any(|entry| entry.kind == "job_claimed"),
        "a disabled runtime claims nothing"
    );
}
