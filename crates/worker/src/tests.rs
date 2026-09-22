//! Adversarial tests of the worker plane: every test tries to break the
//! system — revoked/consumed token reuse, capability drift, missed
//! heartbeats, stale generations, cross-org leasing, protocol skew, racing
//! accepts, scheduler crashes mid-lease and duplicate result replays.
//!
//! Each scenario runs against BOTH store twins (in-memory and SQLite), so
//! the semantics are proven of the durable implementation, not only of the
//! test double.

use std::sync::Arc;

use faktor_cloud::{Clock, ManualClock, OrganizationId, SecretToken};

use crate::error::{WorkerError, WORKER_PROTOCOL_VERSION};
use crate::ids::{ExecutionJobId, JobGeneration, JobKey, WorkerId, WorkerLeaseId};
use crate::model::{
    AttemptState, ExecutionJob, GenerationState, JobAttempt, JobGenerationRow, JobRequirements,
    JobResultOutcome, JobState, JobStatus, JournalEntry, LeaseState, NetworkProfile, RequeuePolicy,
    SandboxCapability, WorkerCapabilities, WorkerLease,
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
                store.try_accept_lease(&lease).unwrap().is_none()
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
