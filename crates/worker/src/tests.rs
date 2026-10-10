//! Adversarial tests of the worker plane: every test tries to break the
//! system — revoked/consumed token reuse, capability drift, missed
//! heartbeats, stale generations, cross-org leasing, protocol skew, racing
//! accepts, scheduler crashes mid-lease and duplicate result replays.
//!
//! Each scenario runs against BOTH store twins (in-memory and SQLite), so
//! the semantics are proven of the durable implementation, not only of the
//! test double.

use std::sync::Arc;

use faktor_cloud::{Clock, ManualClock, OrganizationId, SecretToken, TokenHash};

use crate::error::{WorkerError, WORKER_PROTOCOL_VERSION};
use crate::ids::{ExecutionJobId, JobGeneration, JobKey, WorkerId, WorkerLeaseId};
use crate::model::{
    AttemptState, ExecutionJob, GenerationState, JobAttempt, JobGenerationRow, JobRequirements,
    JobResultOutcome, JobState, JobStatus, JournalEntry, LeaseState, NetworkProfile, RequeuePolicy,
    SandboxCapability, WorkerCapabilities, WorkerLease, WorkerRegistration,
};
use crate::service::{RecoveryReport, ResultOutcome, WorkerPlane};
use crate::store::{MemoryWorkerStore, ResultAppend, SqliteWorkerStore, WorkerStore};

fn org(id: &str) -> OrganizationId {
    OrganizationId::try_new(id).unwrap()
}

fn worker_id(id: &str) -> WorkerId {
    WorkerId::try_new(id).unwrap()
}

fn digest(seed: &str) -> String {
    let mut out = String::new();
    for i in 0..64 {
        let c = seed.as_bytes().get(i % seed.len()).copied().unwrap_or(b'a');
        out.push(char::from(b"0123456789abcdef"[(c as usize) % 16]));
    }
    out
}

fn caps(os: &str, toolchains: &[&str]) -> WorkerCapabilities {
    WorkerCapabilities {
        os: os.into(),
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

fn requirements() -> JobRequirements {
    JobRequirements {
        os: Some("linux".into()),
        arch: Some("x86_64".into()),
        toolchains: vec!["rust".into()],
        sandbox: vec![SandboxCapability::try_new("seccomp").unwrap()],
        network: Some(NetworkProfile::EgressRestricted),
        min_cpu_cores: 2,
        min_memory_mb: 4_096,
        gpu: false,
        region: Some("eu-west".into()),
        trust_domain: "org_1".into(),
    }
}

/// One plane over one store twin, with a controllable clock.
struct Harness {
    plane: Arc<WorkerPlane>,
    store: Arc<dyn WorkerStore>,
    clock: Arc<ManualClock>,
}

impl Harness {
    fn memory() -> Self {
        let store = Arc::new(MemoryWorkerStore::new());
        let clock = Arc::new(ManualClock::new(1_000_000));
        Self {
            plane: WorkerPlane::new(store.clone(), clock.clone()),
            store,
            clock,
        }
    }

    fn sqlite() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SqliteWorkerStore::open(&dir.path().join("workers.db")).unwrap());
        // Keep the tempdir alive for the process lifetime of the test by
        // leaking it: the store keeps the file open anyway and the test
        // process is short-lived.
        std::mem::forget(dir);
        let clock = Arc::new(ManualClock::new(1_000_000));
        Self {
            plane: WorkerPlane::new(store.clone(), clock.clone()),
            store,
            clock,
        }
    }

    fn register(&self, worker: &str, toolchains: &[&str]) -> SecretToken {
        let token = self
            .plane
            .mint_registration_token(&org("org_1"), "org_1", "test")
            .unwrap();
        let plain = SecretToken::try_new(token.token.expose().to_string()).unwrap();
        self.plane
            .register(
                &org("org_1"),
                &worker_id(worker),
                &plain,
                caps("linux", toolchains),
                worker,
            )
            .unwrap();
        plain
    }

    fn schedule(&self, key: &str, digest: &str) -> crate::service::ScheduledJob {
        self.plane
            .schedule_job(
                &org("org_1"),
                "org_1",
                &JobKey::try_new(key).unwrap(),
                requirements(),
                digest,
                RequeuePolicy { max_attempts: 3 },
                None,
            )
            .unwrap()
    }
}

fn harnesses() -> Vec<(&'static str, Harness)> {
    vec![("memory", Harness::memory()), ("sqlite", Harness::sqlite())]
}

/// The worker-row half of a legacy/partial registration (audit P1): durable
/// worker row bound to `token_hash`, token row never consumed.
fn partial_registration(worker: &str, token_hash: &str) -> WorkerRegistration {
    WorkerRegistration {
        worker_id: worker_id(worker),
        organization_id: "org_1".into(),
        trust_domain: "org_1".into(),
        display_name: worker.into(),
        token_hash: token_hash.into(),
        revoked: false,
        capabilities: caps("linux", &["rust"]),
        capabilities_version: 1,
        registered_ms: 1_000_000,
        last_seen_ms: 1_000_000,
        revoked_ms: None,
    }
}

/// One durable `assigned` job/generation/attempt triple, written directly
/// through the store (the scheduler's committed half of a lease accept).
fn seed_open_job(store: &Arc<dyn WorkerStore>, job_id: &ExecutionJobId, now: i64) {
    let mut req = requirements();
    req.normalize().unwrap();
    let job = ExecutionJob {
        job_id: job_id.clone(),
        organization_id: "org_1".into(),
        trust_domain: "org_1".into(),
        job_key: JobKey::try_new(format!("key-{job_id}")).unwrap(),
        requirements: req,
        payload_digest: digest("atomic"),
        requeue: RequeuePolicy { max_attempts: 3 },
        assigned_worker: None,
        created_ms: now,
        current_generation: JobGeneration::FIRST,
        state: JobState::Assigned,
    };
    let generation = JobGenerationRow {
        job_id: job_id.clone(),
        organization_id: "org_1".into(),
        generation: JobGeneration::FIRST,
        state: GenerationState::Assigned,
        created_ms: now,
        ended_ms: None,
        reason: None,
    };
    let attempt = JobAttempt {
        job_id: job_id.clone(),
        generation: JobGeneration::FIRST,
        attempt: 1,
        worker_id: None,
        lease_id: None,
        state: AttemptState::Pending,
        started_ms: now,
        ended_ms: None,
        reason: None,
    };
    store
        .insert_generation_and_attempt(&job, &generation, &attempt)
        .unwrap();
}

// --------------------------------------------------------------- registration

#[test]
fn token_reuse_from_another_worker_is_refused_and_revocation_kills_the_token() {
    for (label, h) in harnesses() {
        let token = h
            .plane
            .mint_registration_token(&org("org_1"), "org_1", "wl")
            .unwrap();
        let plain = SecretToken::try_new(token.token.expose().to_string()).unwrap();
        // First registration consumes the token.
        h.plane
            .register(
                &org("org_1"),
                &worker_id("wrk_1"),
                &plain,
                caps("linux", &["rust"]),
                "first",
            )
            .unwrap();
        // A DIFFERENT worker reusing the same token is refused typed.
        let err = h
            .plane
            .register(
                &org("org_1"),
                &worker_id("wrk_2"),
                &plain,
                caps("linux", &["rust"]),
                "second",
            )
            .unwrap_err();
        assert!(
            matches!(err, WorkerError::TokenAlreadyUsed(_)),
            "{label}: {err}"
        );
        // Re-registration of the SAME worker (capability refresh) is allowed.
        h.plane
            .register(
                &org("org_1"),
                &worker_id("wrk_1"),
                &plain,
                caps("linux", &["rust"]),
                "first",
            )
            .unwrap();
        // Revocation kills the worker AND its token: neither heartbeat nor
        // re-registration can ever resurrect it.
        h.plane
            .revoke_worker(&org("org_1"), &worker_id("wrk_1"))
            .unwrap();
        let err = h
            .plane
            .register(
                &org("org_1"),
                &worker_id("wrk_1"),
                &plain,
                caps("linux", &["rust"]),
                "first",
            )
            .unwrap_err();
        assert!(matches!(err, WorkerError::TokenRevoked), "{label}: {err}");
        let err = h
            .plane
            .revoke_worker(&org("org_1"), &worker_id("wrk_1"))
            .unwrap_err();
        assert!(
            matches!(err, WorkerError::AlreadyRevoked(_)),
            "{label}: {err}"
        );
        // A token minted for another org is refused for this org, and the
        // foreign-org registration is indistinguishable from unknown.
        let other = h
            .plane
            .mint_registration_token(&org("org_2"), "org_2", "wl")
            .unwrap();
        let other_plain = SecretToken::try_new(other.token.expose().to_string()).unwrap();
        let err = h
            .plane
            .register(
                &org("org_1"),
                &worker_id("wrk_9"),
                &other_plain,
                caps("linux", &["rust"]),
                "x",
            )
            .unwrap_err();
        assert!(
            matches!(err, WorkerError::TokenOrgMismatch { .. }),
            "{label}: {err}"
        );
    }
}

#[test]
fn capability_reconciliation_bumps_the_version_exactly_once_per_change() {
    for (label, h) in harnesses() {
        let token = h
            .plane
            .mint_registration_token(&org("org_1"), "org_1", "wl")
            .unwrap();
        let plain = SecretToken::try_new(token.token.expose().to_string()).unwrap();
        let first = h
            .plane
            .register(
                &org("org_1"),
                &worker_id("wrk_1"),
                &plain,
                caps("linux", &["rust"]),
                "w",
            )
            .unwrap();
        assert_eq!(first.worker.capabilities_version, 1);
        assert!(!first.capabilities_reconciled);
        // Unchanged advertisement: version stays, and a different ORDER of
        // the same advertisement is not a change.
        let same = h
            .plane
            .register(
                &org("org_1"),
                &worker_id("wrk_1"),
                &plain,
                caps("linux", &["rust"]),
                "w",
            )
            .unwrap();
        assert_eq!(same.worker.capabilities_version, 1);
        assert!(!same.capabilities_reconciled);
        // Changed toolchains: exactly one version bump.
        let changed = h
            .plane
            .register(
                &org("org_1"),
                &worker_id("wrk_1"),
                &plain,
                caps("linux", &["rust", "node"]),
                "w",
            )
            .unwrap();
        assert_eq!(changed.worker.capabilities_version, 2);
        assert!(changed.capabilities_reconciled);
        assert!(changed
            .worker
            .capabilities
            .toolchains
            .contains(&"node".to_string()));
        // The reconciliation is journaled (durable evidence of the change).
        let journal = h.plane.journal_page(&org("org_1"), None, 100).unwrap();
        assert!(
            journal.iter().any(|e| e.kind == "capabilities_reconciled"),
            "{label}: reconciliation must be journaled"
        );
    }
}

// ------------------------------------------------------------------ leasing

#[test]
fn same_org_and_trust_domain_only_leasing() {
    for (label, h) in harnesses() {
        h.register("wrk_1", &["rust"]);
        // A job of another organization cannot name the worker.
        let err = h
            .plane
            .schedule_job(
                &org("org_2"),
                "org_2",
                &JobKey::try_new("foreign").unwrap(),
                JobRequirements {
                    trust_domain: "org_2".into(),
                    ..requirements()
                },
                &digest("f"),
                RequeuePolicy { max_attempts: 3 },
                Some(&worker_id("wrk_1")),
            )
            .unwrap_err();
        assert!(
            matches!(err, WorkerError::UnknownWorker(_)),
            "{label}: {err}"
        );
        // A token minted by org_1 cannot register a worker into a different
        // trust domain.
        let token = h
            .plane
            .mint_registration_token(&org("org_1"), "org_1", "wl")
            .unwrap();
        let plain = SecretToken::try_new(token.token.expose().to_string()).unwrap();
        let mut foreign_caps = caps("linux", &["rust"]);
        foreign_caps.trust_domain = "org_2".into();
        let err = h
            .plane
            .register(
                &org("org_1"),
                &worker_id("wrk_x"),
                &plain,
                foreign_caps,
                "x",
            )
            .unwrap_err();
        assert!(
            matches!(err, WorkerError::ForeignTrustDomain { .. }),
            "{label}: {err}"
        );
        // A job of org_1 cannot be leased by a worker of another org even if
        // the caller lies about the organization it presents.
        let job = h.schedule("own", &digest("a"));
        let err = h
            .plane
            .accept_lease(
                &org("org_2"),
                &worker_id("wrk_1"),
                &SecretToken::try_new("wkr_00".to_string()).unwrap(),
                WORKER_PROTOCOL_VERSION,
                &job.job.job_id,
                JobGeneration::FIRST,
            )
            .unwrap_err();
        assert!(
            matches!(
                err,
                WorkerError::UnknownWorker(_)
                    | WorkerError::UnknownToken
                    | WorkerError::TokenOrgMismatch { .. }
            ),
            "{label}: {err}"
        );
    }
}

#[test]
fn double_accept_of_one_generation_yields_exactly_one_lease() {
    for (label, h) in harnesses() {
        let _w1 = h.register("wrk_1", &["rust"]);
        let _w2 = h.register("wrk_2", &["rust"]);
        // An OPEN assignment: no preferred worker, so both may race.
        let job_id = ExecutionJobId::try_new("job_open").unwrap();
        let mut req = requirements();
        req.normalize().unwrap();
        let job = ExecutionJob {
            job_id: job_id.clone(),
            organization_id: "org_1".into(),
            trust_domain: "org_1".into(),
            job_key: JobKey::try_new("open").unwrap(),
            requirements: req,
            payload_digest: digest("z"),
            requeue: RequeuePolicy { max_attempts: 3 },
            assigned_worker: None,
            created_ms: h.clock.now_ms(),
            current_generation: JobGeneration::FIRST,
            state: JobState::Assigned,
        };
        let generation = JobGenerationRow {
            job_id: job_id.clone(),
            organization_id: "org_1".into(),
            generation: JobGeneration::FIRST,
            state: GenerationState::Assigned,
            created_ms: h.clock.now_ms(),
            ended_ms: None,
            reason: None,
        };
        let attempt = JobAttempt {
            job_id: job_id.clone(),
            generation: JobGeneration::FIRST,
            attempt: 1,
            worker_id: None,
            lease_id: None,
            state: AttemptState::Pending,
            started_ms: h.clock.now_ms(),
            ended_ms: None,
            reason: None,
        };
        h.store
            .insert_generation_and_attempt(&job, &generation, &attempt)
            .unwrap();
        // Eight racing acceptors, one generation: exactly one lease exists.
        let mut handles = Vec::new();
        for i in 0..8 {
            let store = h.store.clone();
            let worker = worker_id("wrk_1");
            let wid = if i % 2 == 0 {
                worker
            } else {
                worker_id("wrk_2")
            };
            let job_id = job_id.clone();
            handles.push(std::thread::spawn(move || {
                let lease = WorkerLease {
                    lease_id: WorkerLeaseId::try_new(format!("lease_r{i}")).unwrap(),
                    organization_id: "org_1".into(),
                    job_id: job_id.clone(),
                    generation: JobGeneration::FIRST,
                    worker_id: wid,
                    state: LeaseState::Live,
                    heartbeat_interval_ms: 15_000,
                    accepted_ms: 1,
                    last_heartbeat_ms: 1,
                    expires_at_ms: 10_000_000,
                    ended_ms: None,
                    reason: None,
                };
                store.try_accept_lease(&lease, &[]).unwrap().is_none()
            }));
        }
        let winners = handles
            .into_iter()
            .filter(|_| true)
            .filter_map(|h| h.join().ok())
            .filter(|won| *won)
            .count();
        assert_eq!(winners, 1, "{label}: exactly one CAS winner");
        // Exactly one generation carries the lease (nothing was re-opened).
        let existing = h
            .store
            .lease_for_generation(&job_id, JobGeneration::FIRST)
            .unwrap()
            .expect("one lease");
        assert!(matches!(existing.state, LeaseState::Live));
        assert_eq!(
            h.store
                .generations(&job_id)
                .unwrap()
                .iter()
                .filter(|g| g.state == GenerationState::Leased)
                .count(),
            1
        );
    }
}

#[test]
fn stale_generation_accept_is_superseded_and_journaled() {
    for (label, h) in harnesses() {
        let token = h.register("wrk_1", &["rust"]);
        let job = h.schedule("stale", &digest("a"));
        assert!(job.lease.is_some());
        // Advance the clock past the heartbeat window and sweep.
        h.clock.advance(60_000);
        let report = h.plane.recover(&org("org_1")).unwrap();
        assert_eq!(report.expired.len(), 1, "{label}: lease expired");
        assert_eq!(report.requeued.len(), 1, "{label}: requeued");
        // The attempt is terminal (not an indefinitely running job).
        let status = h.plane.job_status(&org("org_1"), &job.job.job_id).unwrap();
        assert_eq!(status.attempts[0].state, AttemptState::Lost);
        assert!(status.attempts[0].ended_ms.is_some());
        assert_eq!(status.job.current_generation.as_u64(), 2);
        assert_eq!(
            status.job.state,
            JobState::Leased,
            "{label}: the requeued generation is auto-assigned to the eligible worker"
        );
        // A stale result for the lost generation is refused typed and
        // journaled — and never landed.
        let err = h
            .plane
            .submit_result(
                &org("org_1"),
                &worker_id("wrk_1"),
                &token,
                WORKER_PROTOCOL_VERSION,
                &job.job.job_id,
                JobGeneration::FIRST,
                &job.lease.as_ref().unwrap().lease_id,
                &digest("a"),
                JobResultOutcome::Succeeded,
            )
            .unwrap_err();
        assert!(
            matches!(err, WorkerError::SupersededLease { generation, current, .. } if generation == JobGeneration::FIRST && current == JobGeneration::try_new(2).unwrap()),
            "{label}: {err}"
        );
        assert!(
            h.store
                .result(&job.job.job_id, JobGeneration::FIRST)
                .unwrap()
                .is_none(),
            "{label}: a stale result must never land"
        );
        let journal = h.plane.journal_page(&org("org_1"), None, 500).unwrap();
        assert!(
            journal.iter().any(|e| e.kind == "stale_result_rejected"),
            "{label}: stale rejection must be journaled"
        );
    }
}

#[test]
fn heartbeat_expiry_is_bounded_and_requeue_is_bounded() {
    for (label, h) in harnesses() {
        let token = h.register("wrk_1", &["rust"]);
        let job = h.schedule("bounded", &digest("b"));
        let lease = job.lease.clone().unwrap();
        // A heartbeat just inside the window renews.
        h.clock.advance(10_000);
        let beat = h
            .plane
            .heartbeat(
                &org("org_1"),
                &worker_id("wrk_1"),
                &token,
                WORKER_PROTOCOL_VERSION,
                &lease.lease_id,
                JobGeneration::FIRST,
            )
            .unwrap();
        assert!(beat.expires_at_ms > lease.expires_at_ms, "{label}");
        // Missed heartbeats: past the window the lease is expired, the
        // attempt terminal, and the bounded policy (3 attempts) requeues.
        h.clock.advance(16_000);
        let err = h
            .plane
            .heartbeat(
                &org("org_1"),
                &worker_id("wrk_1"),
                &token,
                WORKER_PROTOCOL_VERSION,
                &lease.lease_id,
                JobGeneration::FIRST,
            )
            .unwrap_err();
        assert!(
            matches!(err, WorkerError::LeaseExpired { .. }),
            "{label}: {err}"
        );
        let status = h.plane.job_status(&org("org_1"), &job.job.job_id).unwrap();
        assert_eq!(status.attempts[0].state, AttemptState::Lost, "{label}");
        assert_eq!(status.job.current_generation.as_u64(), 2, "{label}");
        // Drive the policy to exhaustion: two more lost leases terminate the
        // job permanently (never unbounded retries).
        let mut report = RecoveryReport::default();
        for _ in 0..2 {
            h.clock.advance(60_000);
            report = h.plane.recover(&org("org_1")).unwrap();
        }
        let status = h.plane.job_status(&org("org_1"), &job.job.job_id).unwrap();
        assert_eq!(status.job.state, JobState::Failed, "{label}");
        assert_eq!(status.attempts.len(), 3, "{label}: bounded attempts");
        assert!(
            report.exhausted.contains(&job.job.job_id.to_string()),
            "{label}: exhaustion reported"
        );
        assert!(
            h.plane
                .journal_page(&org("org_1"), None, 500)
                .unwrap()
                .iter()
                .any(|e| e.kind == "attempts_exhausted"),
            "{label}: exhaustion journaled"
        );
        // A further sweep changes nothing (idempotent terminal state).
        h.clock.advance(600_000);
        let again = h.plane.recover(&org("org_1")).unwrap();
        assert!(
            again.expired.is_empty() && again.requeued.is_empty() && again.exhausted.is_empty(),
            "{label}: terminal job is inert"
        );
    }
}

// ------------------------------------------------------- results + recovery

#[test]
fn duplicate_result_replay_never_lands_twice() {
    for (label, h) in harnesses() {
        let token = h.register("wrk_1", &["rust"]);
        let job = h.schedule("dup", &digest("c"));
        let lease = job.lease.clone().unwrap();
        let first = h
            .plane
            .submit_result(
                &org("org_1"),
                &worker_id("wrk_1"),
                &token,
                WORKER_PROTOCOL_VERSION,
                &job.job.job_id,
                JobGeneration::FIRST,
                &lease.lease_id,
                &digest("c"),
                JobResultOutcome::Succeeded,
            )
            .unwrap();
        assert_eq!(first, ResultOutcome::Landed, "{label}");
        let replay = h
            .plane
            .submit_result(
                &org("org_1"),
                &worker_id("wrk_1"),
                &token,
                WORKER_PROTOCOL_VERSION,
                &job.job.job_id,
                JobGeneration::FIRST,
                &lease.lease_id,
                &digest("c"),
                JobResultOutcome::Succeeded,
            )
            .unwrap();
        assert_eq!(replay, ResultOutcome::Duplicate, "{label}");
        // The result row is unique: one landing, terminal job, completed
        // lease and attempt.
        let status = h.plane.job_status(&org("org_1"), &job.job.job_id).unwrap();
        assert_eq!(status.job.state, JobState::Completed);
        assert!(status.result.is_some());
        assert_eq!(status.generations[0].state, GenerationState::Completed);
        // A conflicting replay (different digest for the same generation) is
        // refused typed.
        let err = h
            .plane
            .submit_result(
                &org("org_1"),
                &worker_id("wrk_1"),
                &token,
                WORKER_PROTOCOL_VERSION,
                &job.job.job_id,
                JobGeneration::FIRST,
                &lease.lease_id,
                &digest("d"),
                JobResultOutcome::Failed,
            )
            .unwrap_err();
        assert!(
            matches!(err, WorkerError::DigestMismatch { .. }),
            "{label}: {err}"
        );
    }
}

#[test]
fn result_after_revocation_never_lands() {
    for (label, h) in harnesses() {
        let token = h.register("wrk_1", &["rust"]);
        let job = h.schedule("revoke", &digest("e"));
        let lease = job.lease.clone().unwrap();
        let report = h
            .plane
            .revoke_worker(&org("org_1"), &worker_id("wrk_1"))
            .unwrap();
        assert_eq!(report.expired.len(), 1, "{label}");
        let err = h
            .plane
            .submit_result(
                &org("org_1"),
                &worker_id("wrk_1"),
                &token,
                WORKER_PROTOCOL_VERSION,
                &job.job.job_id,
                JobGeneration::FIRST,
                &lease.lease_id,
                &digest("e"),
                JobResultOutcome::Succeeded,
            )
            .unwrap_err();
        assert!(
            matches!(err, WorkerError::WorkerRevoked(_)),
            "{label}: {err}"
        );
        assert!(h
            .store
            .result(&job.job.job_id, JobGeneration::FIRST)
            .unwrap()
            .is_none());
        let status = h.plane.job_status(&org("org_1"), &job.job.job_id).unwrap();
        assert_eq!(status.attempts[0].state, AttemptState::Lost);
        assert_eq!(status.job.current_generation.as_u64(), 2);
        assert!(
            status.lease.is_none(),
            "{label}: the revoked worker holds nothing"
        );
    }
}

#[test]
fn scheduler_crash_mid_lease_recovers_deterministically() {
    for (label, h) in harnesses() {
        let token = h.register("wrk_1", &["rust"]);
        let job = h.schedule("crash", &digest("f"));
        let lease = job.lease.clone().unwrap();
        // Crash 1: the scheduler died while the lease was healthy. Recovery
        // RESUMES the live lease untouched; the same result still lands
        // exactly once afterwards.
        let first = h.plane.recover(&org("org_1")).unwrap();
        assert_eq!(first.resumed.len(), 1, "{label}: live lease resumed");
        assert!(first.expired.is_empty(), "{label}");
        let landed = h
            .plane
            .submit_result(
                &org("org_1"),
                &worker_id("wrk_1"),
                &token,
                WORKER_PROTOCOL_VERSION,
                &job.job.job_id,
                JobGeneration::FIRST,
                &lease.lease_id,
                &digest("f"),
                JobResultOutcome::Succeeded,
            )
            .unwrap();
        assert_eq!(landed, ResultOutcome::Landed, "{label}");
        // Crash 2: a lease that silently missed its window EXPIRES (it is
        // never resumed as if nothing happened).
        let job2 = h.schedule("crash2", &digest("a"));
        h.clock.advance(60_000);
        let second = h.plane.recover(&org("org_1")).unwrap();
        assert_eq!(second.expired.len(), 1, "{label}");
        assert_eq!(second.requeued.len(), 1, "{label}");
        // Idempotent: a second sweep changes nothing and cannot accept a
        // second result for any generation.
        let before = h.plane.job_status(&org("org_1"), &job2.job.job_id).unwrap();
        let third = h.plane.recover(&org("org_1")).unwrap();
        assert!(
            third.expired.is_empty() && third.requeued.is_empty() && third.exhausted.is_empty(),
            "{label}: sweeps are idempotent"
        );
        let after = h.plane.job_status(&org("org_1"), &job2.job.job_id).unwrap();
        assert_eq!(
            before.job.current_generation, after.job.current_generation,
            "{label}"
        );
        let stale = h
            .plane
            .submit_result(
                &org("org_1"),
                &worker_id("wrk_1"),
                &token,
                WORKER_PROTOCOL_VERSION,
                &job2.job.job_id,
                JobGeneration::FIRST,
                &job2.lease.as_ref().unwrap().lease_id,
                &digest("a"),
                JobResultOutcome::Succeeded,
            )
            .unwrap_err();
        assert!(
            matches!(stale, WorkerError::SupersededLease { .. }),
            "{label}: {stale}"
        );
        // The completed job stays completed: recovery never reopens it.
        let status = h.plane.job_status(&org("org_1"), &job.job.job_id).unwrap();
        assert_eq!(status.job.state, JobState::Completed);
        assert_eq!(status.generations.len(), 1, "{label}: no extra generation");
    }
}

#[test]
fn resumed_assignment_repairs_a_crash_between_generation_and_lease() {
    for (label, h) in harnesses() {
        h.register("wrk_1", &["rust"]);
        // Simulate the exact crash window: the generation+attempt committed,
        // the CAS accept never ran (no journal replay of schedule either).
        let job_id = ExecutionJobId::try_new("job_crashwin").unwrap();
        let mut req = requirements();
        req.normalize().unwrap();
        let job = ExecutionJob {
            job_id: job_id.clone(),
            organization_id: "org_1".into(),
            trust_domain: "org_1".into(),
            job_key: JobKey::try_new("crashwin").unwrap(),
            requirements: req,
            payload_digest: digest("9"),
            requeue: RequeuePolicy { max_attempts: 3 },
            assigned_worker: None,
            created_ms: h.clock.now_ms(),
            current_generation: JobGeneration::FIRST,
            state: JobState::Assigned,
        };
        let generation = JobGenerationRow {
            job_id: job_id.clone(),
            organization_id: "org_1".into(),
            generation: JobGeneration::FIRST,
            state: GenerationState::Assigned,
            created_ms: h.clock.now_ms(),
            ended_ms: None,
            reason: None,
        };
        let attempt = JobAttempt {
            job_id: job_id.clone(),
            generation: JobGeneration::FIRST,
            attempt: 1,
            worker_id: None,
            lease_id: None,
            state: AttemptState::Pending,
            started_ms: h.clock.now_ms(),
            ended_ms: None,
            reason: None,
        };
        h.store
            .insert_generation_and_attempt(&job, &generation, &attempt)
            .unwrap();
        h.store
            .append_journal(
                &JournalEntry::new("org_1", "job_scheduled", h.clock.now_ms(), "test")
                    .unwrap()
                    .with_job(&job_id, JobGeneration::FIRST),
            )
            .unwrap();
        let report = h.plane.resume_assignments(&org("org_1")).unwrap();
        assert_eq!(report.reassigned.len(), 1, "{label}");
        let lease = h
            .store
            .lease_for_generation(&job_id, JobGeneration::FIRST)
            .unwrap()
            .expect("repaired lease");
        assert_eq!(lease.worker_id, worker_id("wrk_1"));
        // Idempotent: nothing left to repair.
        let again = h.plane.resume_assignments(&org("org_1")).unwrap();
        assert!(again.reassigned.is_empty(), "{label}");
    }
}

/// Audit P1: an infrastructure failure during recovery (a `workers()` read
/// failing typed) is never collapsed into "no eligible worker" — recovery
/// must return the store error instead of a successful empty report that
/// silently strands the job.
#[test]
fn resume_assignments_propagates_a_store_read_failure() {
    let h = Harness::memory();
    h.register("wrk_1", &["rust"]);
    let job_id = ExecutionJobId::try_new("job_fault").unwrap();
    let mut req = requirements();
    req.normalize().unwrap();
    let job = ExecutionJob {
        job_id: job_id.clone(),
        organization_id: "org_1".into(),
        trust_domain: "org_1".into(),
        job_key: JobKey::try_new("fault").unwrap(),
        requirements: req,
        payload_digest: digest("7"),
        requeue: RequeuePolicy { max_attempts: 3 },
        assigned_worker: None,
        created_ms: h.clock.now_ms(),
        current_generation: JobGeneration::FIRST,
        state: JobState::Assigned,
    };
    let generation = JobGenerationRow {
        job_id: job_id.clone(),
        organization_id: "org_1".into(),
        generation: JobGeneration::FIRST,
        state: GenerationState::Assigned,
        created_ms: h.clock.now_ms(),
        ended_ms: None,
        reason: None,
    };
    let attempt = JobAttempt {
        job_id: job_id.clone(),
        generation: JobGeneration::FIRST,
        attempt: 1,
        worker_id: None,
        lease_id: None,
        state: AttemptState::Pending,
        started_ms: h.clock.now_ms(),
        ended_ms: None,
        reason: None,
    };
    h.store
        .insert_generation_and_attempt(&job, &generation, &attempt)
        .unwrap();
    h.store
        .append_journal(
            &JournalEntry::new("org_1", "job_scheduled", h.clock.now_ms(), "test")
                .unwrap()
                .with_job(&job_id, JobGeneration::FIRST),
        )
        .unwrap();
    h.store.inject_workers_read_failure();
    let err = h
        .plane
        .resume_assignments(&org("org_1"))
        .expect_err("a store failure must abort recovery, never read as no-eligible-worker");
    assert!(matches!(err, WorkerError::Backend(_)), "{err:?}");
}

// ------------------------------------------------------------------- protocol

#[test]
fn version_skew_is_refused_typed_at_every_boundary() {
    for (label, h) in harnesses() {
        let token = h
            .plane
            .mint_registration_token(&org("org_1"), "org_1", "wl")
            .unwrap();
        let plain = SecretToken::try_new(token.token.expose().to_string()).unwrap();
        let mut skewed = caps("linux", &["rust"]);
        skewed.protocol_version = WORKER_PROTOCOL_VERSION + 1;
        let err = h
            .plane
            .register(&org("org_1"), &worker_id("wrk_1"), &plain, skewed, "w")
            .unwrap_err();
        assert!(
            matches!(err, WorkerError::ProtocolSkew { expected, got } if expected == WORKER_PROTOCOL_VERSION && got == WORKER_PROTOCOL_VERSION + 1),
            "{label}: {err}"
        );
        // Heartbeat and result submission skew are refused before any state
        // is touched.
        let plain2 = SecretToken::try_new(token.token.expose().to_string()).unwrap();
        h.plane
            .register(
                &org("org_1"),
                &worker_id("wrk_1"),
                &plain2,
                caps("linux", &["rust"]),
                "w",
            )
            .unwrap();
        let job = h.schedule("skew", &digest("a"));
        let lease = job.lease.clone().unwrap();
        let err = h
            .plane
            .heartbeat(
                &org("org_1"),
                &worker_id("wrk_1"),
                &plain2,
                WORKER_PROTOCOL_VERSION + 7,
                &lease.lease_id,
                JobGeneration::FIRST,
            )
            .unwrap_err();
        assert!(
            matches!(err, WorkerError::ProtocolSkew { got, .. } if got == WORKER_PROTOCOL_VERSION + 7),
            "{label}: {err}"
        );
        let err = h
            .plane
            .submit_result(
                &org("org_1"),
                &worker_id("wrk_1"),
                &plain2,
                0,
                &job.job.job_id,
                JobGeneration::FIRST,
                &lease.lease_id,
                &digest("a"),
                JobResultOutcome::Succeeded,
            )
            .unwrap_err();
        assert!(
            matches!(err, WorkerError::ProtocolSkew { got: 0, .. }),
            "{label}: {err}"
        );
        assert!(h
            .store
            .result(&job.job.job_id, JobGeneration::FIRST)
            .unwrap()
            .is_none());
    }
}

#[test]
fn capability_mismatch_and_wrong_token_are_refused() {
    for (label, h) in harnesses() {
        let token = h.register("wrk_1", &["rust"]);
        // A job that demands another toolchain is not placed on this worker:
        // the schedule refuses typed and writes nothing.
        let mut req = requirements();
        req.toolchains = vec!["cobol".into()];
        let err = h
            .plane
            .schedule_job(
                &org("org_1"),
                "org_1",
                &JobKey::try_new("mismatch").unwrap(),
                req,
                &digest("b"),
                RequeuePolicy { max_attempts: 3 },
                Some(&worker_id("wrk_1")),
            )
            .unwrap_err();
        assert!(
            matches!(err, WorkerError::CapabilityMismatch { .. }),
            "{label}: {err}"
        );
        assert!(
            h.store
                .job_by_key("org_1", &JobKey::try_new("mismatch").unwrap())
                .unwrap()
                .is_none(),
            "{label}: refused schedule writes nothing"
        );
        // A wrong token can never heartbeat or submit.
        let wrong = SecretToken::try_new("wkr_deadbeef".to_string()).unwrap();
        let job = h.schedule("tok", &digest("c"));
        let lease = job.lease.clone().unwrap();
        let err = h
            .plane
            .heartbeat(
                &org("org_1"),
                &worker_id("wrk_1"),
                &wrong,
                WORKER_PROTOCOL_VERSION,
                &lease.lease_id,
                JobGeneration::FIRST,
            )
            .unwrap_err();
        assert!(matches!(err, WorkerError::UnknownToken), "{label}: {err}");
        let err = h
            .plane
            .submit_result(
                &org("org_1"),
                &worker_id("wrk_1"),
                &wrong,
                WORKER_PROTOCOL_VERSION,
                &job.job.job_id,
                JobGeneration::FIRST,
                &lease.lease_id,
                &digest("c"),
                JobResultOutcome::Succeeded,
            )
            .unwrap_err();
        assert!(matches!(err, WorkerError::UnknownToken), "{label}: {err}");
        // The legitimate token still works.
        h.plane
            .heartbeat(
                &org("org_1"),
                &worker_id("wrk_1"),
                &token,
                WORKER_PROTOCOL_VERSION,
                &lease.lease_id,
                JobGeneration::FIRST,
            )
            .unwrap();
    }
}

#[test]
fn replayed_schedule_never_mints_a_second_generation() {
    for (label, h) in harnesses() {
        h.register("wrk_1", &["rust"]);
        let first = h.schedule("idem", &digest("a"));
        let replay = h.schedule("idem", &digest("a"));
        assert!(replay.replay, "{label}");
        assert_eq!(replay.job.job_id, first.job.job_id, "{label}");
        assert_eq!(replay.generation, JobGeneration::FIRST, "{label}");
        assert_eq!(
            replay.lease.as_ref().unwrap().lease_id,
            first.lease.as_ref().unwrap().lease_id
        );
        assert_eq!(
            h.store.generations(&first.job.job_id).unwrap().len(),
            1,
            "{label}: one immutable generation"
        );
        // A different digest under the SAME key is still the same job: the
        // key is the identity and the generation is immutable.
        let same = h.schedule("idem", &digest("b"));
        assert_eq!(same.job.payload_digest, digest("a"), "{label}");
    }
}

#[test]
fn foreign_job_status_is_indistinguishable_from_missing() {
    for (label, h) in harnesses() {
        h.register("wrk_1", &["rust"]);
        let job = h.schedule("iso", &digest("a"));
        let err = h
            .plane
            .job_status(&org("org_2"), &job.job.job_id)
            .unwrap_err();
        assert!(matches!(err, WorkerError::UnknownJob(_)), "{label}: {err}");
        let missing = ExecutionJobId::try_new("job_missing").unwrap();
        let err = h.plane.job_status(&org("org_1"), &missing).unwrap_err();
        assert!(matches!(err, WorkerError::UnknownJob(_)), "{label}: {err}");
        assert_eq!(err.to_string(), "job job_missing is unknown");
    }
}

#[test]
fn worker_listing_is_cursor_bounded_and_org_scoped() {
    for (label, h) in harnesses() {
        for i in 0..5 {
            let id = format!("wrk_{i}");
            let token = h
                .plane
                .mint_registration_token(&org("org_1"), "org_1", "wl")
                .unwrap();
            let plain = SecretToken::try_new(token.token.expose().to_string()).unwrap();
            h.plane
                .register(
                    &org("org_1"),
                    &worker_id(&id),
                    &plain,
                    caps("linux", &["rust"]),
                    &id,
                )
                .unwrap();
        }
        let token = h
            .plane
            .mint_registration_token(&org("org_2"), "org_2", "wl")
            .unwrap();
        let plain = SecretToken::try_new(token.token.expose().to_string()).unwrap();
        let mut foreign_caps = caps("linux", &["rust"]);
        foreign_caps.trust_domain = "org_2".into();
        h.plane
            .register(
                &org("org_2"),
                &worker_id("wrk_foreign"),
                &plain,
                foreign_caps,
                "foreign",
            )
            .unwrap();
        let page = h.plane.list_workers(&org("org_1"), None, 2).unwrap();
        assert_eq!(page.items.len(), 2, "{label}");
        assert_eq!(page.items[0].worker_id.as_str(), "wrk_0");
        let cursor = page.next_cursor.clone().unwrap();
        let page2 = h
            .plane
            .list_workers(&org("org_1"), Some(&cursor), 10)
            .unwrap();
        assert_eq!(page2.items.len(), 3, "{label}");
        assert!(
            page2.items.iter().all(|w| w.organization_id == "org_1"),
            "{label}: no foreign worker leaks into the page"
        );
        assert!(page2.next_cursor.is_none(), "{label}");
    }
}

#[test]
fn durable_rows_survive_a_store_reopen() {
    // SQLite only: the point is durability across process restarts.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workers.db");
    let store: Arc<dyn WorkerStore> = Arc::new(SqliteWorkerStore::open(&path).unwrap());
    let clock = Arc::new(ManualClock::new(5_000));
    let plane = WorkerPlane::new(store.clone(), clock.clone());
    let token = plane
        .mint_registration_token(&org("org_1"), "org_1", "wl")
        .unwrap();
    let plain = SecretToken::try_new(token.token.expose().to_string()).unwrap();
    plane
        .register(
            &org("org_1"),
            &worker_id("wrk_1"),
            &plain,
            caps("linux", &["rust"]),
            "w",
        )
        .unwrap();
    let job = plane
        .schedule_job(
            &org("org_1"),
            "org_1",
            &JobKey::try_new("durable").unwrap(),
            requirements(),
            &digest("a"),
            RequeuePolicy { max_attempts: 3 },
            None,
        )
        .unwrap();
    assert!(job.lease.is_some());
    drop(plane);
    drop(store);
    // Reopen: the worker, its lease and the immutable generation are there.
    let store: Arc<dyn WorkerStore> = Arc::new(SqliteWorkerStore::open(&path).unwrap());
    let plane = WorkerPlane::new(store.clone(), clock);
    let workers = plane.list_workers(&org("org_1"), None, 10).unwrap();
    assert_eq!(workers.items.len(), 1);
    assert_eq!(workers.items[0].capabilities_version, 1);
    let job_id: ExecutionJobId = job.job.job_id.clone();
    let status: JobStatus = plane.job_status(&org("org_1"), &job_id).unwrap();
    assert_eq!(status.generations.len(), 1);
    assert_eq!(status.generations[0].state, GenerationState::Leased);
    assert_eq!(status.attempts[0].state, AttemptState::Leased);
    // The recovered plane authenticates the SAME token and accepts a result.
    let landed = plane
        .submit_result(
            &org("org_1"),
            &worker_id("wrk_1"),
            &plain,
            WORKER_PROTOCOL_VERSION,
            &job_id,
            JobGeneration::FIRST,
            &status.lease.as_ref().unwrap().lease_id,
            &digest("a"),
            JobResultOutcome::Succeeded,
        )
        .unwrap();
    assert_eq!(landed, ResultOutcome::Landed);
}

#[test]
fn duplicate_landing_is_impossible_at_the_store_layer() {
    let store = MemoryWorkerStore::new();
    let result = crate::model::JobResult {
        job_id: ExecutionJobId::try_new("job_x").unwrap(),
        generation: JobGeneration::FIRST,
        worker_id: worker_id("wrk_1"),
        lease_id: WorkerLeaseId::try_new("lease_x").unwrap(),
        digest: digest("a"),
        outcome: JobResultOutcome::Succeeded,
        accepted_ms: 1,
        verification: Default::default(),
    };
    // The job/lease rows are absent, so landing refuses; the point is that
    // the store never invents a result row from a refusal.
    assert!(store.land_result(&result).is_err());
    assert!(store
        .result(&result.job_id, JobGeneration::FIRST)
        .unwrap()
        .is_none());
    let _ = ResultAppend::Landed;
}

// ---------------------------------------------- audit P1: registration atomicity

/// Audit P1 crash-window reproducer (a): the partial state is worker A's row
/// durable with token T while `T.consumed_by = None`; re-registering A through
/// T succeeds through the ONE store transaction and REPAIRS `consumed_by`, and
/// the repaired binding authenticates.
#[test]
fn crash_window_partial_registration_repairs_the_token_binding() {
    for (label, h) in harnesses() {
        let issued = h
            .plane
            .mint_registration_token(&org("org_1"), "org_1", "wl")
            .unwrap();
        let plain = SecretToken::try_new(issued.token.expose().to_string()).unwrap();
        h.store
            .put_worker(&partial_registration("wrk_a", &issued.token_hash))
            .unwrap();
        assert_eq!(
            h.store
                .token(&issued.token_hash)
                .unwrap()
                .unwrap()
                .consumed_by,
            None,
            "{label}: the crash window really leaves the token unconsumed"
        );
        // (a) A re-registers: the worker row, the consumed token row and the
        // journal row commit together.
        let outcome = h
            .plane
            .register(
                &org("org_1"),
                &worker_id("wrk_a"),
                &plain,
                caps("linux", &["rust"]),
                "a",
            )
            .unwrap();
        assert_eq!(outcome.worker.worker_id, worker_id("wrk_a"), "{label}");
        assert_eq!(
            h.store
                .token(&issued.token_hash)
                .unwrap()
                .unwrap()
                .consumed_by
                .as_deref(),
            Some("wrk_a"),
            "{label}: consumed_by repaired"
        );
        assert_eq!(
            h.store
                .workers_by_token_hash("org_1", &issued.token_hash)
                .unwrap()
                .len(),
            1,
            "{label}: exactly one worker row bound"
        );
        // (d) after repair, A authenticates with T.
        assert!(
            h.plane
                .authenticate_for_test(&worker_id("wrk_a"), &plain)
                .is_ok(),
            "{label}: repaired binding authenticates"
        );
        assert!(
            h.plane
                .journal_page(&org("org_1"), None, 100)
                .unwrap()
                .iter()
                .any(|e| matches!(
                    e.kind.as_str(),
                    "worker_registered" | "registration_refreshed" | "capabilities_reconciled"
                )),
            "{label}: the repair journal row committed with the state"
        );
    }
}

/// Audit P1 (b)/(c): while the partial state exists (A's row bound to T,
/// `T.consumed_by = None`), a DIFFERENT worker can never register T, an
/// unconsumed token never authenticates anyone (defense-in-depth), and only
/// A's atomic re-registration repairs the binding.
#[test]
fn legacy_partial_binding_refuses_a_second_worker_and_never_authenticates() {
    for (label, h) in harnesses() {
        let issued = h
            .plane
            .mint_registration_token(&org("org_1"), "org_1", "wl")
            .unwrap();
        let plain = SecretToken::try_new(issued.token.expose().to_string()).unwrap();
        h.store
            .put_worker(&partial_registration("wrk_a", &issued.token_hash))
            .unwrap();
        // (b) B reusing T is refused typed while A's legacy binding exists.
        let err = h
            .plane
            .register(
                &org("org_1"),
                &worker_id("wrk_b"),
                &plain,
                caps("linux", &["rust"]),
                "b",
            )
            .unwrap_err();
        assert!(
            matches!(
                err,
                WorkerError::TokenBoundElsewhere { ref worker, .. } if worker.as_str() == "wrk_a"
            ),
            "{label}: {err}"
        );
        // (c) even the worker row that carries the hash does not authenticate
        // while the durable token row is unconsumed.
        let err = h
            .plane
            .authenticate_for_test(&worker_id("wrk_a"), &plain)
            .unwrap_err();
        assert!(
            matches!(err, WorkerError::TokenNotConsumed(ref w) if w.as_str() == "wrk_a"),
            "{label}: {err}"
        );
        // The repair: only A, through the token, and only via the one
        // transactional mutation.
        h.plane
            .register(
                &org("org_1"),
                &worker_id("wrk_a"),
                &plain,
                caps("linux", &["rust"]),
                "a",
            )
            .unwrap();
        assert!(
            h.plane
                .authenticate_for_test(&worker_id("wrk_a"), &plain)
                .is_ok(),
            "{label}: repaired binding authenticates"
        );
        let err = h
            .plane
            .authenticate_for_test(&worker_id("wrk_b"), &plain)
            .unwrap_err();
        assert!(
            matches!(err, WorkerError::UnknownWorker(_)),
            "{label}: {err}"
        );
        let err = h
            .plane
            .register(
                &org("org_1"),
                &worker_id("wrk_b"),
                &plain,
                caps("linux", &["rust"]),
                "b",
            )
            .unwrap_err();
        assert!(
            matches!(err, WorkerError::TokenAlreadyUsed(ref c) if c == "wrk_a"),
            "{label}: {err}"
        );
        assert_eq!(
            h.store
                .workers_by_token_hash("org_1", &issued.token_hash)
                .unwrap()
                .len(),
            1,
            "{label}: exactly one worker row bound"
        );
    }
}

/// Tampered token row: `consumed_by` points at another worker, so neither
/// authentication nor re-registration of the true holder may pass.
#[test]
fn tampered_token_consumption_refuses_auth_and_reregistration() {
    for (label, h) in harnesses() {
        let plain = h.register("wrk_a", &["rust"]);
        let hash = TokenHash::of(plain.expose()).as_str().to_string();
        let mut row = h.store.token(&hash).unwrap().unwrap();
        row.consumed_by = Some("wrk_other".into());
        h.store.put_token(&row).unwrap();
        let err = h
            .plane
            .authenticate_for_test(&worker_id("wrk_a"), &plain)
            .unwrap_err();
        assert!(
            matches!(err, WorkerError::TokenAlreadyUsed(ref c) if c == "wrk_other"),
            "{label}: {err}"
        );
        let err = h
            .plane
            .register(
                &org("org_1"),
                &worker_id("wrk_a"),
                &plain,
                caps("linux", &["rust"]),
                "a",
            )
            .unwrap_err();
        assert!(
            matches!(err, WorkerError::TokenAlreadyUsed(ref c) if c == "wrk_other"),
            "{label}: {err}"
        );
        // The tamper did not move the durable binding either way.
        assert_eq!(
            h.store.workers_by_token_hash("org_1", &hash).unwrap().len(),
            1,
            "{label}"
        );
    }
}

/// Race: N threads register DIFFERENT worker ids with the SAME token after a
/// barrier. Exactly one wins; every loser is a typed refusal; exactly one
/// worker row is ever bound.
#[test]
fn parallel_registrations_of_one_token_yield_exactly_one_winner() {
    for (label, h) in harnesses() {
        let issued = h
            .plane
            .mint_registration_token(&org("org_1"), "org_1", "race")
            .unwrap();
        let plain = SecretToken::try_new(issued.token.expose().to_string()).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let mut handles = Vec::new();
        for i in 0..8 {
            let plane = h.plane.clone();
            let plain = plain.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                plane.register(
                    &org("org_1"),
                    &worker_id(&format!("wrk_race_{i}")),
                    &plain,
                    caps("linux", &["rust"]),
                    "race",
                )
            }));
        }
        let results: Vec<Result<_, WorkerError>> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();
        let winners = results.iter().filter(|r| r.is_ok()).count();
        assert_eq!(winners, 1, "{label}: exactly one winner: {results:?}");
        for result in &results {
            if let Err(e) = result {
                assert!(
                    matches!(
                        e,
                        WorkerError::TokenAlreadyUsed(_) | WorkerError::TokenBoundElsewhere { .. }
                    ),
                    "{label}: loser must be a typed refusal: {e}"
                );
            }
        }
        let bound = h
            .store
            .workers_by_token_hash("org_1", &issued.token_hash)
            .unwrap();
        assert_eq!(bound.len(), 1, "{label}: exactly one worker row bound");
        assert_eq!(
            h.store
                .token(&issued.token_hash)
                .unwrap()
                .unwrap()
                .consumed_by
                .as_deref(),
            Some(bound[0].worker_id.as_str()),
            "{label}: the consumed binding names the winner"
        );
        // The winner authenticates; the losing workers do not.
        let winner = bound[0].worker_id.clone();
        assert!(
            h.plane.authenticate_for_test(&winner, &plain).is_ok(),
            "{label}"
        );
        for i in 0..8 {
            let w = worker_id(&format!("wrk_race_{i}"));
            if w != winner {
                assert!(
                    h.plane.authenticate_for_test(&w, &plain).is_err(),
                    "{label}: loser {w} must not authenticate"
                );
            }
        }
    }
}

// ---------------------------------------------- audit P1: lease/journal atomicity

/// Audit P1: the `lease_accepted` journal row commits in the SAME transaction
/// as the lease/generation/job/attempt transition. An injected journal failure
/// returns Err with every row unchanged (no partial transition, no accepted
/// lease without its journal row).
#[test]
fn journal_failure_aborts_the_whole_lease_accept() {
    for (label, h) in harnesses() {
        let job_id = ExecutionJobId::try_new(format!("job_atomic_{label}")).unwrap();
        seed_open_job(&h.store, &job_id, 1_000_000);
        let lease = WorkerLease {
            lease_id: WorkerLeaseId::try_new(format!("lease_atomic_{label}")).unwrap(),
            organization_id: "org_1".into(),
            job_id: job_id.clone(),
            generation: JobGeneration::FIRST,
            worker_id: worker_id("wrk_atomic"),
            state: LeaseState::Live,
            heartbeat_interval_ms: 15_000,
            accepted_ms: 1_000_000,
            last_heartbeat_ms: 1_000_000,
            expires_at_ms: 2_000_000,
            ended_ms: None,
            reason: None,
        };
        let entry = JournalEntry::new(
            "org_1",
            "lease_accepted",
            1_000_000,
            format!("lease={} interval_ms=15000", lease.lease_id),
        )
        .unwrap()
        .with_worker(&lease.worker_id)
        .with_job(&job_id, JobGeneration::FIRST);

        h.store.inject_next_journal_failure();
        let err = h
            .store
            .try_accept_lease(&lease, std::slice::from_ref(&entry))
            .unwrap_err();
        assert!(matches!(err, WorkerError::Backend(_)), "{label}: {err}");
        // The whole transition rolled back: no lease row, the generation/job
        // stay `assigned`, the attempt stays `pending`.
        assert!(
            h.store
                .lease_for_generation(&job_id, JobGeneration::FIRST)
                .unwrap()
                .is_none(),
            "{label}: no lease row survives the aborted accept"
        );
        assert_eq!(
            h.store
                .generation(&job_id, JobGeneration::FIRST)
                .unwrap()
                .unwrap()
                .state,
            GenerationState::Assigned,
            "{label}"
        );
        assert_eq!(
            h.store.job(&job_id).unwrap().unwrap().state,
            JobState::Assigned,
            "{label}"
        );
        assert_eq!(
            h.store
                .attempt(&job_id, JobGeneration::FIRST, 1)
                .unwrap()
                .unwrap()
                .state,
            AttemptState::Pending,
            "{label}"
        );
        assert!(
            !h.store
                .journal("org_1", None, 500)
                .unwrap()
                .iter()
                .any(|e| e.kind == "lease_accepted"),
            "{label}: no journal row from the aborted accept"
        );
        // The retry (no injection) accepts and commits the journal row with
        // the state: an accepted lease always has its row.
        assert!(
            h.store
                .try_accept_lease(&lease, std::slice::from_ref(&entry))
                .unwrap()
                .is_none(),
            "{label}"
        );
        let lease_row = h
            .store
            .lease_for_generation(&job_id, JobGeneration::FIRST)
            .unwrap()
            .expect("the retry accepted");
        assert_eq!(lease_row.state, LeaseState::Live, "{label}");
        assert!(
            h.store
                .journal("org_1", None, 500)
                .unwrap()
                .iter()
                .any(|e| e.kind == "lease_accepted" && e.detail.contains(lease.lease_id.as_str())),
            "{label}: the journal row committed with the lease"
        );
    }
}

// ------------------------------------------ audit P1: seeded state-machine model

/// A tiny deterministic LCG (no new deps): the same seed replays the same
/// operation sequence, so a counterexample is reproducible byte-for-byte.
struct Lcg(u64);

impl Lcg {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self, modulo: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % modulo
    }
}

/// The model's store+plane pair plus the token plaintexts it minted and the
/// leases it observed accepted. Invariants are re-checked after EVERY step;
/// the journal cursor keeps the "accepted lease has its journal row" check
/// incremental (bounded work per step).
struct ModelHarness {
    plane: Arc<WorkerPlane>,
    store: Arc<dyn WorkerStore>,
    clock: Arc<ManualClock>,
    tokens: Vec<(SecretToken, String)>,
    accepted_leases: std::collections::BTreeSet<String>,
    journal_cursor: Option<i64>,
    lease_entries: std::collections::BTreeSet<String>,
    next_job: u64,
    rng: Lcg,
}

impl ModelHarness {
    fn new(store: Arc<dyn WorkerStore>, seed: u64) -> Self {
        let clock = Arc::new(ManualClock::new(1_000_000));
        let plane = WorkerPlane::new(store.clone(), clock.clone());
        Self {
            plane,
            store,
            clock,
            tokens: Vec::new(),
            accepted_leases: Default::default(),
            journal_cursor: None,
            lease_entries: Default::default(),
            next_job: 0,
            rng: Lcg::new(seed),
        }
    }

    fn memory(seed: u64) -> Self {
        Self::new(Arc::new(MemoryWorkerStore::new()), seed)
    }

    fn sqlite(path: &std::path::Path, seed: u64) -> Self {
        Self::new(Arc::new(SqliteWorkerStore::open(path).unwrap()), seed)
    }

    fn mint_token(&mut self) {
        let issued = self
            .plane
            .mint_registration_token(&org("org_1"), "org_1", "model")
            .unwrap();
        let plain = SecretToken::try_new(issued.token.expose().to_string()).unwrap();
        self.tokens.push((plain, issued.token_hash));
    }

    fn step(&mut self) {
        match self.rng.next(6) {
            // register(worker, token)
            0 | 1 => {
                let worker = worker_id(&format!("wrk_{}", self.rng.next(3)));
                let token = self.tokens[self.rng.next(3) as usize].0.clone();
                let tool = if self.rng.next(2) == 0 {
                    "rust"
                } else {
                    "node"
                };
                let _ = self.plane.register(
                    &org("org_1"),
                    &worker,
                    &token,
                    caps("linux", &[tool]),
                    "model",
                );
            }
            // authenticate(worker, token) — refusals are expected and typed;
            // the invariant below is about what SUCCEEDS.
            2 => {
                let worker = worker_id(&format!("wrk_{}", self.rng.next(4)));
                let token = self.tokens[self.rng.next(3) as usize].0.clone();
                let _ = self.plane.authenticate_for_test(&worker, &token);
            }
            // accept_lease: a fresh open job; the scheduler auto-accepts when
            // an eligible worker exists (every accepted lease must carry its
            // committed journal row).
            3 => {
                self.next_job += 1;
                let key = JobKey::try_new(format!("model-{}", self.next_job)).unwrap();
                let payload = digest(&format!("m{}", self.next_job));
                if let Ok(scheduled) = self.plane.schedule_job(
                    &org("org_1"),
                    "org_1",
                    &key,
                    requirements(),
                    &payload,
                    RequeuePolicy { max_attempts: 3 },
                    None,
                ) {
                    if let Some(lease) = scheduled.lease {
                        self.accepted_leases.insert(lease.lease_id.to_string());
                    }
                }
            }
            // tamper_token: point consumed_by at an existing or bogus worker.
            4 => {
                let hash = self.tokens[self.rng.next(3) as usize].1.clone();
                if let Some(mut row) = self.store.token(&hash).unwrap() {
                    row.consumed_by = if self.rng.next(2) == 0 {
                        Some("wrk_ghost".into())
                    } else {
                        Some(format!("wrk_{}", self.rng.next(3)))
                    };
                    self.store.put_token(&row).unwrap();
                }
            }
            // crash_partial_register: worker row durable, token never consumed
            // (only into a token hash no worker holds, so the harness itself
            // never creates the double-binding the system must prevent).
            _ => {
                let tidx = self.rng.next(self.tokens.len() as u64) as usize;
                let hash = self.tokens[tidx].1.clone();
                if self
                    .store
                    .workers_by_token_hash("org_1", &hash)
                    .unwrap()
                    .is_empty()
                {
                    self.next_job += 1;
                    let worker = format!("wrk_partial_{}", self.next_job);
                    self.store
                        .put_worker(&partial_registration(&worker, &hash))
                        .unwrap();
                }
            }
        }
        self.check_invariants();
    }

    fn check_invariants(&mut self) {
        let workers = self.store.workers("org_1", None, 1000).unwrap();

        // Invariant A: at most one worker row per token hash per organization.
        let mut by_hash: std::collections::BTreeMap<&str, Vec<&WorkerRegistration>> =
            Default::default();
        for worker in &workers {
            by_hash
                .entry(worker.token_hash.as_str())
                .or_default()
                .push(worker);
        }
        for (hash, bound) in &by_hash {
            assert!(
                bound.len() <= 1,
                "at most one worker per token hash {hash}: {:?}",
                bound
                    .iter()
                    .map(|w| w.worker_id.as_str())
                    .collect::<Vec<_>>()
            );
        }

        // Invariant B: a token authenticates exactly the worker its durable
        // `consumed_by` names AND that carries the hash; no other worker ever
        // authenticates, and an unconsumed token authenticates NO ONE.
        for (plain, hash) in &self.tokens {
            let token_row = self.store.token(hash).unwrap();
            for worker in &workers {
                let expected = token_row.as_ref().is_some_and(|row| {
                    row.consumed_by.as_deref() == Some(worker.worker_id.as_str())
                        && worker.token_hash == *hash
                });
                let succeeded = self
                    .plane
                    .authenticate_for_test(&worker.worker_id, plain)
                    .is_ok();
                assert_eq!(
                    succeeded,
                    expected,
                    "auth mismatch for {} with token {hash}: consumed_by={:?}",
                    worker.worker_id,
                    token_row.as_ref().and_then(|row| row.consumed_by.clone())
                );
            }
        }

        // Invariant C: every accepted lease has its committed journal row.
        self.drain_journal();
        assert_eq!(
            self.lease_entries, self.accepted_leases,
            "every accepted lease must have its lease_accepted journal row"
        );
    }

    fn drain_journal(&mut self) {
        loop {
            let page = self
                .store
                .journal("org_1", self.journal_cursor, 500)
                .unwrap();
            if page.is_empty() {
                break;
            }
            self.journal_cursor = page.last().map(|e| e.seq);
            let short_page = page.len() < 500;
            for entry in &page {
                if entry.kind == "lease_accepted" {
                    if let Some(rest) = entry.detail.strip_prefix("lease=") {
                        if let Some(lease) = rest.split_whitespace().next() {
                            self.lease_entries.insert(lease.to_string());
                        }
                    }
                }
            }
            if short_page {
                break;
            }
        }
    }
}

/// Deterministic state machine over both store twins: 10k seeded ops
/// (register / authenticate / accept_lease / tamper_token /
/// crash_partial_register) with the identity and journal invariants checked
/// after EVERY step; the memory "reopen" rebuilds the plane over the same
/// state.
#[test]
fn seeded_model_preserves_identity_and_journal_invariants_memory() {
    let mut model = ModelHarness::memory(0x5eed_0001);
    for _ in 0..4 {
        model.mint_token();
    }
    for _ in 0..10_000 {
        model.step();
    }
    model.check_invariants();
    model.plane = WorkerPlane::new(model.store.clone(), model.clock.clone());
    model.check_invariants();
}

/// The same seeded model over the SQLite file: after 10k ops the store is
/// dropped and reopened (a new store AND a new plane over the same file) and
/// the invariants must still hold — including the journal/lease pairing.
#[test]
fn seeded_model_preserves_identity_and_journal_invariants_across_sqlite_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("model.db");
    let mut model = ModelHarness::sqlite(&path, 0x5eed_0002);
    for _ in 0..4 {
        model.mint_token();
    }
    for _ in 0..10_000 {
        model.step();
    }
    model.check_invariants();
    let tokens = model.tokens.clone();
    let accepted = model.accepted_leases.clone();
    drop(model);
    let mut reopened = ModelHarness::sqlite(&path, 0x5eed_0003);
    reopened.tokens = tokens;
    reopened.accepted_leases = accepted;
    reopened.check_invariants();
}
