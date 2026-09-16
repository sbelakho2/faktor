//! The ONE worker-plane service: registration/reconciliation, immutable
//! generation scheduling, CAS lease acceptance, heartbeat renewal with
//! bounded staleness, generation-checked result landing, revocation and
//! deterministic crash recovery.
//!
//! Semantics, exactly:
//!
//! - **assignment** mints one immutable generation for a job and (when an
//!   eligible worker exists) CAS-accepts a lease for it. A replayed schedule
//!   with the same `(organization, job_key)` returns the SAME job and
//!   generation;
//! - **heartbeats** renew only a live lease of the exact generation; the
//!   expiry can only move forward. Missing heartbeats past `expires_at_ms`
//!   make the lease `expired`, the attempt terminal (`lost`) and the job
//!   requeued under its bounded policy (or terminally failed when the
//!   attempts are exhausted);
//! - **results** are accepted only while the lease is live AND the
//!   generation is the job's current generation. Anything else is the typed
//!   [`WorkerError::SupersededLease`] (or `LeaseExpired`/`LeaseNotLive`),
//!   journaled and never landed. The durable result row is unique per
//!   `(job, generation)`, so a duplicate replay is a `Duplicate` outcome,
//!   never a second landing;
//! - **trust domain** is checked on registration (capability advertisement
//!   must match the token's domain) and on every lease accept (the job's
//!   organization and trust domain must equal the worker's);
//! - **protocol skew** is refused typed at register, heartbeat and result.

use std::sync::Arc;

use faktor_cloud::{Clock, OrganizationId, SecretToken, SystemClock, TokenHash};
use serde::{Deserialize, Serialize};

use crate::error::{WorkerError, WORKER_PROTOCOL_VERSION};
use crate::ids::{JobGeneration, JobKey, WorkerId, WorkerLeaseId};
use crate::model::{
    clamp_heartbeat_interval, AttemptState, ExecutionJob, GenerationState, JobAttempt,
    JobGenerationRow, JobRequirements, JobResult, JobResultOutcome, JobState, JobStatus,
    JobVerificationClaim, JournalEntry, LeaseState, RequeuePolicy, WorkerCapabilities, WorkerLease,
    WorkerPage, WorkerRegistration, WorkerTokenRow, WorkerView, DEFAULT_MAX_ATTEMPTS,
    HEARTBEAT_DEFAULT_INTERVAL_MS, MAX_LIVE_LEASES_PER_WORKER, MAX_WORKER_NAME_BYTES,
    RESULT_DIGEST_BYTES,
};
use crate::store::{ResultAppend, WorkerStore};

/// Bound on one worker listing page.
pub const MAX_WORKER_PAGE: usize = 200;
/// Bound on one journal page.
pub const MAX_JOURNAL_PAGE: usize = 500;
/// Bound on the jobs one `claim_next` long-poll scan examines.
pub const MAX_CLAIM_SCAN_JOBS: usize = 512;

/// One minted registration token: the plaintext is exposed exactly once and
/// never stored (only its SHA-256 hash is durable).
#[derive(Debug)]
pub struct IssuedWorkerToken {
    pub token: SecretToken,
    pub token_hash: String,
    pub organization_id: String,
    pub trust_domain: String,
    pub label: String,
}

/// The outcome of one registration (initial or re-registration).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistrationOutcome {
    pub worker: WorkerView,
    /// `true` when the advertisement changed and the version was bumped.
    pub capabilities_reconciled: bool,
}

/// The outcome of one job schedule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduledJob {
    pub job: ExecutionJob,
    pub generation: JobGeneration,
    /// `Some` when a lease was CAS-accepted for the generation.
    #[serde(default)]
    pub lease: Option<WorkerLease>,
    /// `true` when this call replayed an existing job (nothing new minted).
    pub replay: bool,
}

/// The outcome of one heartbeat.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeartbeatOutcome {
    pub lease_id: WorkerLeaseId,
    pub generation: JobGeneration,
    pub heartbeat_interval_ms: i64,
    pub expires_at_ms: i64,
}

/// The outcome of one result submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResultOutcome {
    /// The first result for the generation landed.
    Landed,
    /// The identical result was already landed (idempotent replay).
    Duplicate,
}

/// One claimed job: the immutable job root plus the live lease this worker
/// CAS-accepted for its current generation. Returned by
/// [`WorkerPlane::claim_next`] — the worker-side runtime's transport
/// long-polls/claims through the same method the wire adapter calls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimedLease {
    pub job: ExecutionJob,
    pub lease: WorkerLease,
}

/// A deterministic recovery sweep report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryReport {
    /// Live leases that are still within their heartbeat window (resumed
    /// untouched).
    pub resumed: Vec<String>,
    /// Jobs whose un-leased assigned generation was re-attempted.
    pub reassigned: Vec<String>,
    /// Leases expired by this sweep.
    pub expired: Vec<String>,
    /// Attempts requeued into a new generation (`job -> new generation`).
    pub requeued: Vec<(String, u64)>,
    /// Jobs that exhausted their bounded requeue policy.
    pub exhausted: Vec<String>,
}

impl RecoveryReport {
    pub fn is_empty(&self) -> bool {
        self.resumed.is_empty()
            && self.reassigned.is_empty()
            && self.expired.is_empty()
            && self.requeued.is_empty()
            && self.exhausted.is_empty()
    }
}

/// The worker plane: the ONE authority over worker registrations, job
/// generations, leases and landed results.
pub struct WorkerPlane {
    store: Arc<dyn WorkerStore>,
    clock: Arc<dyn Clock>,
}

impl WorkerPlane {
    pub fn new(store: Arc<dyn WorkerStore>, clock: Arc<dyn Clock>) -> Arc<Self> {
        Arc::new(Self { store, clock })
    }

    /// The production construction path (system clock).
    pub fn with_system_clock(store: Arc<dyn WorkerStore>) -> Arc<Self> {
        Self::new(store, Arc::new(SystemClock))
    }

    pub fn store(&self) -> &Arc<dyn WorkerStore> {
        &self.store
    }

    pub fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }

    // ------------------------------------------------------------- registration

    /// Mint one org-scoped registration token. The plaintext rides the
    /// return value exactly once; the durable row carries only the hash.
    pub fn mint_registration_token(
        &self,
        organization: &OrganizationId,
        trust_domain: &str,
        label: &str,
    ) -> Result<IssuedWorkerToken, WorkerError> {
        if label.is_empty() || label.len() > MAX_WORKER_NAME_BYTES {
            return Err(WorkerError::Malformed(format!(
                "token label must be 1..={MAX_WORKER_NAME_BYTES} bytes"
            )));
        }
        let trust_domain = trust_domain.trim().to_ascii_lowercase();
        if trust_domain.is_empty() || trust_domain.len() > crate::model::MAX_CAPABILITY_BYTES {
            return Err(WorkerError::Malformed(format!(
                "trust domain must be 1..={} bytes",
                crate::model::MAX_CAPABILITY_BYTES
            )));
        }
        let token = new_token()?;
        let hash = TokenHash::of(token.expose());
        let row = WorkerTokenRow {
            token_hash: hash.as_str().to_string(),
            organization_id: organization.as_str().to_string(),
            trust_domain: trust_domain.clone(),
            label: label.to_string(),
            created_ms: self.now_ms(),
            revoked: false,
            consumed_by: None,
        };
        self.store.put_token(&row)?;
        Ok(IssuedWorkerToken {
            token,
            token_hash: hash.as_str().to_string(),
            organization_id: organization.as_str().to_string(),
            trust_domain,
            label: label.to_string(),
        })
    }

    /// The organization (and trust domain) one registration token belongs
    /// to — a token-independent READ for the wire adapters. Every mutation
    /// still authenticates the presented token.
    pub fn token_organization(
        &self,
        token: &SecretToken,
    ) -> Result<(OrganizationId, String), WorkerError> {
        let hash = TokenHash::of(token.expose());
        let Some(row) = self.store.token(hash.as_str())? else {
            return Err(WorkerError::UnknownToken);
        };
        if row.revoked {
            return Err(WorkerError::TokenRevoked);
        }
        let organization = OrganizationId::try_new(row.organization_id.clone())
            .map_err(|e| WorkerError::Malformed(e.to_string()))?;
        Ok((organization, row.trust_domain))
    }

    /// The worker one registration token is bound to (`consumed_by`), for
    /// the wire adapters: the caller names a token, never an identity. The
    /// full authentication still happens inside every mutation.
    pub fn worker_for_token(&self, token: &SecretToken) -> Result<WorkerId, WorkerError> {
        let hash = TokenHash::of(token.expose());
        let Some(row) = self.store.token(hash.as_str())? else {
            return Err(WorkerError::UnknownToken);
        };
        if row.revoked {
            return Err(WorkerError::TokenRevoked);
        }
        let Some(consumer) = row.consumed_by else {
            return Err(WorkerError::UnknownWorker(
                WorkerId::try_new("unregistered")
                    .map_err(|e| WorkerError::Malformed(e.to_string()))?,
            ));
        };
        WorkerId::try_new(consumer).map_err(|e| WorkerError::Malformed(e.to_string()))
    }

    /// The organization one registered worker belongs to (a read for the
    /// wire adapters; mutations still authenticate the token).
    pub fn worker_organization(&self, worker_id: &WorkerId) -> Result<OrganizationId, WorkerError> {
        let Some(worker) = self.store.worker(worker_id)? else {
            return Err(WorkerError::UnknownWorker(worker_id.clone()));
        };
        OrganizationId::try_new(worker.organization_id)
            .map_err(|e| WorkerError::Malformed(e.to_string()))
    }

    /// Revoke one registration token by hash (admin surface). A revoked
    /// token can neither register nor re-register.
    pub fn revoke_token(
        &self,
        organization: &OrganizationId,
        token_hash: &str,
    ) -> Result<(), WorkerError> {
        let Some(mut row) = self.store.token(token_hash)? else {
            return Err(WorkerError::UnknownToken);
        };
        if row.organization_id != organization.as_str() {
            return Err(WorkerError::UnknownToken);
        }
        if row.revoked {
            return Ok(());
        }
        row.revoked = true;
        self.store.put_token(&row)?;
        Ok(())
    }

    /// Register (or re-register) one worker with a registration token.
    ///
    /// Re-registration through the SAME token and worker id reconciles the
    /// capability advertisement: an unchanged advertisement keeps its
    /// version, a changed one bumps it by exactly one and is journaled. A
    /// token consumed by a DIFFERENT worker id is refused typed, and a
    /// revoked worker or token never re-registers.
    pub fn register(
        &self,
        organization: &OrganizationId,
        worker_id: &WorkerId,
        token: &SecretToken,
        mut capabilities: WorkerCapabilities,
        display_name: &str,
    ) -> Result<RegistrationOutcome, WorkerError> {
        capabilities.normalize()?;
        if display_name.is_empty() || display_name.len() > MAX_WORKER_NAME_BYTES {
            return Err(WorkerError::Malformed(format!(
                "worker display name must be 1..={MAX_WORKER_NAME_BYTES} bytes"
            )));
        }
        let now = self.now_ms();
        let hash = TokenHash::of(token.expose());
        let hash = hash.as_str().to_string();
        let Some(mut token_row) = self.store.token(&hash)? else {
            return Err(WorkerError::UnknownToken);
        };
        if token_row.revoked {
            return Err(WorkerError::TokenRevoked);
        }
        if token_row.organization_id != organization.as_str() {
            return Err(WorkerError::TokenOrgMismatch {
                owner: token_row.organization_id.clone(),
                requested: organization.as_str().to_string(),
            });
        }
        if capabilities.trust_domain != token_row.trust_domain {
            return Err(WorkerError::ForeignTrustDomain {
                requested: capabilities.trust_domain.clone(),
                actual: token_row.trust_domain.clone(),
            });
        }
        match &token_row.consumed_by {
            Some(consumer) if consumer != worker_id.as_str() => {
                return Err(WorkerError::TokenAlreadyUsed(consumer.clone()));
            }
            _ => {}
        }
        let existing = self.store.worker(worker_id)?;
        if let Some(existing) = &existing {
            if existing.revoked {
                return Err(WorkerError::WorkerRevoked(worker_id.clone()));
            }
            if existing.token_hash != hash {
                return Err(WorkerError::WorkerAlreadyBound {
                    worker: worker_id.clone(),
                    token_hash: existing.token_hash.clone(),
                });
            }
        }
        let (capabilities_version, reconciled) = match &existing {
            Some(prior) if prior.capabilities == capabilities => {
                (prior.capabilities_version, false)
            }
            Some(prior) => (prior.capabilities_version.saturating_add(1), true),
            None => (1, false),
        };
        let registration = WorkerRegistration {
            worker_id: worker_id.clone(),
            organization_id: organization.as_str().to_string(),
            trust_domain: token_row.trust_domain.clone(),
            display_name: display_name.to_string(),
            token_hash: hash.clone(),
            revoked: false,
            capabilities,
            capabilities_version,
            registered_ms: existing.as_ref().map(|p| p.registered_ms).unwrap_or(now),
            last_seen_ms: now,
            revoked_ms: None,
        };
        self.store.put_worker(&registration)?;
        token_row.consumed_by = Some(worker_id.as_str().to_string());
        self.store.put_token(&token_row)?;
        let kind = if existing.is_some() {
            if reconciled {
                "capabilities_reconciled"
            } else {
                "registration_refreshed"
            }
        } else {
            "worker_registered"
        };
        self.journal(
            organization.as_str(),
            kind,
            now,
            Some(worker_id),
            None,
            None,
            format!("capabilities_version={capabilities_version} reconciled={reconciled}"),
        )?;
        Ok(RegistrationOutcome {
            worker: WorkerView::from(&registration),
            capabilities_reconciled: reconciled,
        })
    }

    /// Revoke one worker: its token dies with it and every live lease of the
    /// worker is ended (`revoked`), its attempt terminal (`lost`) and its job
    /// requeued under the bounded policy.
    pub fn revoke_worker(
        &self,
        organization: &OrganizationId,
        worker_id: &WorkerId,
    ) -> Result<RecoveryReport, WorkerError> {
        let now = self.now_ms();
        let Some(mut worker) = self.store.worker(worker_id)? else {
            return Err(WorkerError::UnknownWorker(worker_id.clone()));
        };
        if worker.organization_id != organization.as_str() {
            // Tenant isolation: a foreign worker is indistinguishable from a
            // missing one.
            return Err(WorkerError::UnknownWorker(worker_id.clone()));
        }
        if worker.revoked {
            return Err(WorkerError::AlreadyRevoked(worker_id.clone()));
        }
        worker.revoked = true;
        worker.revoked_ms = Some(now);
        worker.last_seen_ms = now;
        self.store.put_worker(&worker)?;
        if let Some(mut token) = self.store.token(&worker.token_hash)? {
            token.revoked = true;
            self.store.put_token(&token)?;
        }
        let mut report = RecoveryReport::default();
        for lease in self.store.live_leases_of_worker(worker_id)? {
            self.end_lease_and_requeue(
                &lease,
                LeaseState::Revoked,
                "worker revoked",
                now,
                &mut report,
            )?;
        }
        self.journal(
            organization.as_str(),
            "worker_revoked",
            now,
            Some(worker_id),
            None,
            None,
            "registration and token revoked; live leases ended".to_string(),
        )?;
        Ok(report)
    }

    /// One org-scoped cursor page of workers.
    pub fn list_workers(
        &self,
        organization: &OrganizationId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<WorkerPage, WorkerError> {
        let limit = limit.clamp(1, MAX_WORKER_PAGE);
        let mut rows = self
            .store
            .workers(organization.as_str(), after, limit + 1)?;
        let next = if rows.len() > limit {
            rows.pop();
            rows.last().map(|w| w.worker_id.to_string())
        } else {
            None
        };
        Ok(WorkerPage {
            items: rows.iter().map(WorkerView::from).collect(),
            next_cursor: next,
        })
    }

    // -------------------------------------------------------------- scheduling

    /// The scheduler's assignment: mint (or replay) one immutable generation
    /// for `job_key` and, when an eligible worker exists, CAS-accept its
    /// lease. Replaying the same key returns the same job and generation.
    #[allow(clippy::too_many_arguments)]
    pub fn schedule_job(
        &self,
        organization: &OrganizationId,
        trust_domain: &str,
        job_key: &JobKey,
        mut requirements: JobRequirements,
        payload_digest: &str,
        requeue: RequeuePolicy,
        assigned_worker: Option<&WorkerId>,
    ) -> Result<ScheduledJob, WorkerError> {
        requeue.validate()?;
        requirements.normalize()?;
        validate_digest(payload_digest)?;
        if requirements.trust_domain != trust_domain.trim().to_ascii_lowercase() {
            return Err(WorkerError::ForeignTrustDomain {
                requested: requirements.trust_domain.clone(),
                actual: trust_domain.to_string(),
            });
        }
        let now = self.now_ms();
        if let Some(existing) = self.store.job_by_key(organization.as_str(), job_key)? {
            let lease = self
                .store
                .lease_for_generation(&existing.job_id, existing.current_generation)?;
            return Ok(ScheduledJob {
                generation: existing.current_generation,
                job: existing,
                lease,
                replay: true,
            });
        }
        let worker = match assigned_worker {
            Some(id) => {
                let worker =
                    self.require_eligible(id, organization, trust_domain, &requirements)?;
                Some(worker)
            }
            None => self.find_eligible(organization, trust_domain, &requirements)?,
        };
        let job_id = crate::ids::mint("job");
        let job_id = crate::ids::ExecutionJobId::try_new(job_id)?;
        let job = ExecutionJob {
            job_id: job_id.clone(),
            organization_id: organization.as_str().to_string(),
            trust_domain: trust_domain.to_string(),
            job_key: job_key.clone(),
            requirements,
            payload_digest: payload_digest.to_string(),
            requeue,
            assigned_worker: worker.as_ref().map(|w| w.worker_id.clone()),
            created_ms: now,
            current_generation: JobGeneration::FIRST,
            state: JobState::Assigned,
        };
        let generation = JobGenerationRow {
            job_id: job_id.clone(),
            organization_id: organization.as_str().to_string(),
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
        self.store
            .insert_generation_and_attempt(&job, &generation, &attempt)?;
        self.journal(
            organization.as_str(),
            "job_scheduled",
            now,
            job.assigned_worker.as_ref(),
            Some(&job_id),
            Some(JobGeneration::FIRST),
            format!("job_key={job_key} digest={payload_digest}"),
        )?;
        let lease = match worker {
            Some(worker) => Some(self.accept_lease_inner(
                &worker,
                organization,
                &job,
                JobGeneration::FIRST,
                now,
            )?),
            None => None,
        };
        Ok(ScheduledJob {
            job,
            generation: JobGeneration::FIRST,
            lease,
            replay: false,
        })
    }

    /// A worker accepts the lease of one immutable generation (the CAS step:
    /// exactly one worker wins; everyone else sees the winner's lease).
    pub fn accept_lease(
        &self,
        organization: &OrganizationId,
        worker_id: &WorkerId,
        token: &SecretToken,
        protocol_version: u32,
        job_id: &crate::ids::ExecutionJobId,
        generation: JobGeneration,
    ) -> Result<WorkerLease, WorkerError> {
        check_protocol(protocol_version)?;
        let worker = self.authenticate(worker_id, token)?;
        if worker.organization_id != organization.as_str() {
            return Err(WorkerError::UnknownWorker(worker_id.clone()));
        }
        let job = self.require_job(organization, job_id)?;
        let now = self.now_ms();
        self.accept_lease_inner(&worker, organization, &job, generation, now)
    }

    /// Claim the next eligible job of one worker: scan the organization's
    /// scheduled/requeued job index (the journal is the durable index of
    /// every job the scheduler minted) for an un-leased `assigned`
    /// generation this worker may take — its trust domain AND capability
    /// advertisement must satisfy the job's immutable requirements, and an
    /// assignment to another worker is skipped — then CAS-accept the lease
    /// through the SAME atomic path every other accept uses. Exactly one
    /// worker wins a generation; a lost CAS race skips to the next job and
    /// is never retried blindly. `Ok(None)` = no eligible work right now
    /// (the caller long-polls). Bounded: at most
    /// [`MAX_CLAIM_SCAN_JOBS`] jobs are examined per call.
    pub fn claim_next(
        &self,
        organization: &OrganizationId,
        worker_id: &WorkerId,
        token: &SecretToken,
        protocol_version: u32,
    ) -> Result<Option<ClaimedLease>, WorkerError> {
        check_protocol(protocol_version)?;
        let worker = self.authenticate(worker_id, token)?;
        if worker.organization_id != organization.as_str() {
            return Err(WorkerError::UnknownWorker(worker_id.clone()));
        }
        let now = self.now_ms();
        let mut after: Option<i64> = None;
        let mut scanned: usize = 0;
        let mut seen: std::collections::BTreeSet<String> = Default::default();
        loop {
            let page = self
                .store
                .journal(organization.as_str(), after, MAX_JOURNAL_PAGE)?;
            if page.is_empty() {
                return Ok(None);
            }
            after = page.last().map(|e| e.seq);
            let short_page = page.len() < MAX_JOURNAL_PAGE;
            for entry in &page {
                if entry.kind != "job_scheduled" && entry.kind != "job_requeued" {
                    continue;
                }
                let Some(job_id_raw) = &entry.job_id else {
                    continue;
                };
                if !seen.insert(job_id_raw.clone()) {
                    continue;
                }
                scanned += 1;
                if scanned > MAX_CLAIM_SCAN_JOBS {
                    return Ok(None);
                }
                let Ok(job_id) = crate::ids::ExecutionJobId::try_new(job_id_raw.clone()) else {
                    continue;
                };
                let Some(job) = self.store.job(&job_id)? else {
                    continue;
                };
                if job.organization_id != organization.as_str() || job.state.is_terminal() {
                    continue;
                }
                if worker.trust_domain != job.trust_domain {
                    continue;
                }
                // Capability matching: a worker only ever receives jobs whose
                // toolchains/sandbox/network profile it advertises. A
                // mismatch is a SILENT skip (the job is not for this worker),
                // never a refused claim that would surface as an error.
                if worker.capabilities.satisfies(&job.requirements).is_err() {
                    continue;
                }
                if let Some(assigned) = &job.assigned_worker {
                    if assigned != &worker.worker_id {
                        continue;
                    }
                }
                let generation = job.current_generation;
                if self
                    .store
                    .lease_for_generation(&job_id, generation)?
                    .is_some()
                {
                    continue;
                }
                let Some(generation_row) = self.store.generation(&job_id, generation)? else {
                    continue;
                };
                if generation_row.state != GenerationState::Assigned {
                    continue;
                }
                match self.accept_lease_inner(&worker, organization, &job, generation, now) {
                    Ok(lease) => {
                        self.journal(
                            organization.as_str(),
                            "job_claimed",
                            now,
                            Some(&worker.worker_id),
                            Some(&job.job_id),
                            Some(generation),
                            format!("lease={} claimed via long-poll", lease.lease_id),
                        )?;
                        return Ok(Some(ClaimedLease { job, lease }));
                    }
                    // Someone else moved first (a concurrent claim, a
                    // requeue, or this worker's own saturation): skip to the
                    // next candidate, never retry this one blindly.
                    Err(WorkerError::LeaseAlreadyTaken { .. })
                    | Err(WorkerError::NotLeasable { .. })
                    | Err(WorkerError::SupersededLease { .. })
                    | Err(WorkerError::NotAssignedToWorker { .. })
                    | Err(WorkerError::CapabilityMismatch { .. })
                    | Err(WorkerError::WorkerSaturated { .. }) => continue,
                    Err(e) => return Err(e),
                }
            }
            if short_page {
                return Ok(None);
            }
        }
    }

    /// Additive worker-side claim support: one LIVE lease this worker already
    /// holds for a non-terminal current generation. The scheduler's placement
    /// CAS may have accepted the lease on the worker's behalf (the
    /// orchestrator's placement adapter leases the generation it mints), so a
    /// long-polling worker adopts that lease here instead of sitting idle on a
    /// generation someone already holds for it. The regular heartbeat then
    /// renews the adopted lease and the result lands through the same
    /// generation-checked path. `Ok(None)` = nothing to adopt. Bounded: only
    /// this worker's own live leases are examined.
    pub fn adoptable_lease(
        &self,
        organization: &OrganizationId,
        worker_id: &WorkerId,
        token: &SecretToken,
        protocol_version: u32,
    ) -> Result<Option<ClaimedLease>, WorkerError> {
        check_protocol(protocol_version)?;
        let worker = self.authenticate(worker_id, token)?;
        if worker.organization_id != organization.as_str() {
            return Err(WorkerError::UnknownWorker(worker_id.clone()));
        }
        let now = self.now_ms();
        for lease in self.store.live_leases_of_worker(worker_id)? {
            if lease.organization_id != organization.as_str() {
                continue;
            }
            if lease.is_expired_at(now) {
                continue;
            }
            let Some(job) = self.store.job(&lease.job_id)? else {
                continue;
            };
            if job.state.is_terminal() || job.current_generation != lease.generation {
                continue;
            }
            if worker.trust_domain != job.trust_domain {
                continue;
            }
            if worker.capabilities.satisfies(&job.requirements).is_err() {
                continue;
            }
            self.journal(
                organization.as_str(),
                "job_adopted",
                now,
                Some(&worker.worker_id),
                Some(&job.job_id),
                Some(lease.generation),
                format!(
                    "lease={} adopted from the scheduler's placement",
                    lease.lease_id
                ),
            )?;
            return Ok(Some(ClaimedLease { job, lease }));
        }
        Ok(None)
    }

    fn accept_lease_inner(
        &self,
        worker: &WorkerRegistration,
        organization: &OrganizationId,
        job: &ExecutionJob,
        generation: JobGeneration,
        now: i64,
    ) -> Result<WorkerLease, WorkerError> {
        // Trust domain: a worker can only lease jobs of its own org/domain.
        if worker.organization_id != job.organization_id || worker.trust_domain != job.trust_domain
        {
            return Err(WorkerError::ForeignTrustDomain {
                requested: job.trust_domain.clone(),
                actual: worker.trust_domain.clone(),
            });
        }
        if let Some(assigned) = &job.assigned_worker {
            if assigned != &worker.worker_id {
                return Err(WorkerError::NotAssignedToWorker {
                    job: job.job_id.to_string(),
                    generation,
                    worker: worker.worker_id.clone(),
                    assigned: assigned.to_string(),
                });
            }
        }
        if generation != job.current_generation {
            self.journal(
                organization.as_str(),
                "stale_lease_rejected",
                now,
                Some(&worker.worker_id),
                Some(&job.job_id),
                Some(generation),
                format!("current generation is {}", job.current_generation),
            )?;
            return Err(WorkerError::SupersededLease {
                job: job.job_id.to_string(),
                generation,
                current: job.current_generation,
            });
        }
        if job.state.is_terminal() {
            return Err(WorkerError::JobTerminal {
                job: job.job_id.to_string(),
                state: job.state.as_str().to_string(),
            });
        }
        let generation_row = self
            .store
            .generation(&job.job_id, generation)?
            .ok_or_else(|| WorkerError::UnknownGeneration {
                job: job.job_id.to_string(),
                generation,
            })?;
        if let Some(existing) = self.store.lease_for_generation(&job.job_id, generation)? {
            self.journal(
                organization.as_str(),
                "lease_accept_lost",
                now,
                Some(&worker.worker_id),
                Some(&job.job_id),
                Some(generation),
                format!(
                    "lease {} already held by {}",
                    existing.lease_id, existing.worker_id
                ),
            )?;
            return Err(WorkerError::LeaseAlreadyTaken {
                job: job.job_id.to_string(),
                generation,
                worker: existing.worker_id.to_string(),
                lease: existing.lease_id.to_string(),
            });
        }
        if generation_row.state != GenerationState::Assigned {
            return Err(WorkerError::NotLeasable {
                job: job.job_id.to_string(),
                generation,
                state: generation_row.state.as_str().to_string(),
            });
        }
        worker
            .capabilities
            .satisfies(&job.requirements)
            .map_err(|detail| WorkerError::CapabilityMismatch {
                worker: worker.worker_id.clone(),
                job: job.job_id.to_string(),
                detail,
            })?;
        let live = self.store.live_leases_of_worker(&worker.worker_id)?;
        if live.len() >= MAX_LIVE_LEASES_PER_WORKER {
            return Err(WorkerError::WorkerSaturated {
                worker: worker.worker_id.clone(),
                limit: MAX_LIVE_LEASES_PER_WORKER,
            });
        }
        let interval = clamp_heartbeat_interval(HEARTBEAT_DEFAULT_INTERVAL_MS);
        let lease = WorkerLease {
            lease_id: WorkerLeaseId::try_new(crate::ids::mint("lease"))?,
            organization_id: organization.as_str().to_string(),
            job_id: job.job_id.clone(),
            generation,
            worker_id: worker.worker_id.clone(),
            state: LeaseState::Live,
            heartbeat_interval_ms: interval,
            accepted_ms: now,
            last_heartbeat_ms: now,
            expires_at_ms: now.saturating_add(interval),
            ended_ms: None,
            reason: None,
        };
        if let Some(existing) = self.store.try_accept_lease(&lease)? {
            // The CAS lost (a concurrent accept won between our read and the
            // atomic insert): report the winner, never a second lease.
            self.journal(
                organization.as_str(),
                "lease_accept_lost",
                now,
                Some(&worker.worker_id),
                Some(&job.job_id),
                Some(generation),
                format!(
                    "lease {} already held by {}",
                    existing.lease_id, existing.worker_id
                ),
            )?;
            return Err(WorkerError::LeaseAlreadyTaken {
                job: job.job_id.to_string(),
                generation,
                worker: existing.worker_id.to_string(),
                lease: existing.lease_id.to_string(),
            });
        }
        self.journal(
            organization.as_str(),
            "lease_accepted",
            now,
            Some(&worker.worker_id),
            Some(&job.job_id),
            Some(generation),
            format!("lease={} interval_ms={interval}", lease.lease_id),
        )?;
        Ok(lease)
    }

    // -------------------------------------------------------------- heartbeats

    /// Renew the lease of one immutable generation. The lease must be live,
    /// belong to the worker and carry the exact generation; the expiry can
    /// only move forward.
    pub fn heartbeat(
        &self,
        organization: &OrganizationId,
        worker_id: &WorkerId,
        token: &SecretToken,
        protocol_version: u32,
        lease_id: &WorkerLeaseId,
        generation: JobGeneration,
    ) -> Result<HeartbeatOutcome, WorkerError> {
        check_protocol(protocol_version)?;
        let worker = self.authenticate(worker_id, token)?;
        if worker.organization_id != organization.as_str() {
            return Err(WorkerError::UnknownWorker(worker_id.clone()));
        }
        let now = self.now_ms();
        let Some(lease) = self.store.lease(lease_id)? else {
            return Err(WorkerError::UnknownLease {
                lease: lease_id.clone(),
                worker: worker_id.clone(),
            });
        };
        if lease.worker_id != worker.worker_id {
            return Err(WorkerError::UnknownLease {
                lease: lease_id.clone(),
                worker: worker_id.clone(),
            });
        }
        if lease.generation != generation {
            self.journal(
                organization.as_str(),
                "stale_heartbeat_rejected",
                now,
                Some(&worker.worker_id),
                Some(&lease.job_id),
                Some(generation),
                format!("lease generation is {}", lease.generation),
            )?;
            return Err(WorkerError::SupersededLease {
                job: lease.job_id.to_string(),
                generation,
                current: self
                    .current_generation_of(&lease.job_id)?
                    .unwrap_or(lease.generation),
            });
        }
        match lease.state {
            LeaseState::Expired => {
                return Err(WorkerError::LeaseExpired {
                    lease: lease_id.clone(),
                    expired_at_ms: lease.expires_at_ms,
                })
            }
            LeaseState::Superseded | LeaseState::Revoked | LeaseState::Completed => {
                return Err(WorkerError::LeaseNotLive {
                    lease: lease_id.clone(),
                    state: lease.state.as_str().to_string(),
                })
            }
            LeaseState::Live => {}
        }
        if lease.is_expired_at(now) {
            // Bounded staleness: the lease is lost the moment its window
            // closes; the attempt becomes terminal and requeues.
            let mut report = RecoveryReport::default();
            self.end_lease_and_requeue(
                &lease,
                LeaseState::Expired,
                "heartbeat expired",
                now,
                &mut report,
            )?;
            return Err(WorkerError::LeaseExpired {
                lease: lease_id.clone(),
                expired_at_ms: lease.expires_at_ms,
            });
        }
        let expires_at = now.saturating_add(lease.heartbeat_interval_ms);
        let renewed = self
            .store
            .renew_lease(lease_id, generation, now, expires_at)?;
        if !renewed {
            let after = self.store.lease(lease_id)?;
            return match after {
                Some(after) if after.state != LeaseState::Live => Err(WorkerError::LeaseNotLive {
                    lease: lease_id.clone(),
                    state: after.state.as_str().to_string(),
                }),
                Some(after) => Err(WorkerError::SupersededLease {
                    job: after.job_id.to_string(),
                    generation,
                    current: after.generation,
                }),
                None => Err(WorkerError::UnknownLease {
                    lease: lease_id.clone(),
                    worker: worker_id.clone(),
                }),
            };
        }
        self.store.put_worker(&WorkerRegistration {
            last_seen_ms: now,
            ..worker.clone()
        })?;
        Ok(HeartbeatOutcome {
            lease_id: lease_id.clone(),
            generation,
            heartbeat_interval_ms: lease.heartbeat_interval_ms,
            expires_at_ms: expires_at,
        })
    }

    // ----------------------------------------------------------------- results

    /// Land one worker result. Accepted ONLY while the lease is live and its
    /// generation is the job's current generation: a superseded/stale result
    /// is the typed [`WorkerError::SupersededLease`], journaled and never
    /// landed.
    ///
    /// This is the legacy entry: it lands the result with the DEFAULT
    /// (fail-closed) verification claim — "not self-verified". Workers that
    /// ran their own deterministic verification report it through
    /// [`Self::submit_result_with_claim`].
    #[allow(clippy::too_many_arguments)]
    pub fn submit_result(
        &self,
        organization: &OrganizationId,
        worker_id: &WorkerId,
        token: &SecretToken,
        protocol_version: u32,
        job_id: &crate::ids::ExecutionJobId,
        generation: JobGeneration,
        lease_id: &WorkerLeaseId,
        digest: &str,
        outcome: JobResultOutcome,
    ) -> Result<ResultOutcome, WorkerError> {
        self.submit_result_with_claim(
            organization,
            worker_id,
            token,
            protocol_version,
            job_id,
            generation,
            lease_id,
            digest,
            outcome,
            JobVerificationClaim::default(),
        )
    }

    /// Land one worker result together with the worker's verification claim
    /// (whether the worker-side pipeline ran its own deterministic checks,
    /// and the digest of the produced tree when it mutated one). The claim is
    /// durable on the landed [`JobResult`]; the ORIGIN decides whether it is
    /// sufficient to settle the parent run (documented classification) or
    /// whether the attempt must be marked for origin verification.
    #[allow(clippy::too_many_arguments)]
    pub fn submit_result_with_claim(
        &self,
        organization: &OrganizationId,
        worker_id: &WorkerId,
        token: &SecretToken,
        protocol_version: u32,
        job_id: &crate::ids::ExecutionJobId,
        generation: JobGeneration,
        lease_id: &WorkerLeaseId,
        digest: &str,
        outcome: JobResultOutcome,
        verification: JobVerificationClaim,
    ) -> Result<ResultOutcome, WorkerError> {
        check_protocol(protocol_version)?;
        validate_digest(digest)?;
        let worker = self.authenticate(worker_id, token)?;
        if worker.organization_id != organization.as_str() {
            return Err(WorkerError::UnknownWorker(worker_id.clone()));
        }
        let now = self.now_ms();
        let job = self.require_job(organization, job_id)?;
        if job.current_generation != generation {
            self.journal(
                organization.as_str(),
                "stale_result_rejected",
                now,
                Some(&worker.worker_id),
                Some(&job.job_id),
                Some(generation),
                format!("current generation is {}", job.current_generation),
            )?;
            return Err(WorkerError::SupersededLease {
                job: job.job_id.to_string(),
                generation,
                current: job.current_generation,
            });
        }
        if digest != job.payload_digest {
            self.journal(
                organization.as_str(),
                "result_digest_rejected",
                now,
                Some(&worker.worker_id),
                Some(&job.job_id),
                Some(generation),
                format!("presented digest {digest}"),
            )?;
            return Err(WorkerError::DigestMismatch {
                expected: job.payload_digest.clone(),
                presented: digest.to_string(),
            });
        }
        // Idempotent replay: an identical result already landed for this
        // generation is reported as a Duplicate, never as a second landing
        // and never as a lease-state error.
        if let Some(existing) = self.store.result(job_id, generation)? {
            if existing.digest == digest
                && existing.worker_id == worker.worker_id
                && existing.lease_id == *lease_id
            {
                self.journal(
                    organization.as_str(),
                    "result_duplicate",
                    now,
                    Some(&worker.worker_id),
                    Some(&job.job_id),
                    Some(generation),
                    format!("lease={lease_id}"),
                )?;
                return Ok(ResultOutcome::Duplicate);
            }
            self.journal(
                organization.as_str(),
                "result_conflict_rejected",
                now,
                Some(&worker.worker_id),
                Some(&job.job_id),
                Some(generation),
                format!("lease={lease_id}"),
            )?;
            return Err(WorkerError::ResultConflict {
                job: job.job_id.to_string(),
                generation,
            });
        }
        let Some(lease) = self.store.lease(lease_id)? else {
            return Err(WorkerError::UnknownLease {
                lease: lease_id.clone(),
                worker: worker.worker_id.clone(),
            });
        };
        if lease.worker_id != worker.worker_id || lease.generation != generation {
            return Err(WorkerError::UnknownLease {
                lease: lease_id.clone(),
                worker: worker.worker_id.clone(),
            });
        }
        if lease.state != LeaseState::Live {
            self.journal(
                organization.as_str(),
                "result_lease_not_live",
                now,
                Some(&worker.worker_id),
                Some(&job.job_id),
                Some(generation),
                format!("lease state {}", lease.state.as_str()),
            )?;
            return match lease.state {
                LeaseState::Expired => Err(WorkerError::LeaseExpired {
                    lease: lease_id.clone(),
                    expired_at_ms: lease.expires_at_ms,
                }),
                state => Err(WorkerError::LeaseNotLive {
                    lease: lease_id.clone(),
                    state: state.as_str().to_string(),
                }),
            };
        }
        if lease.is_expired_at(now) {
            let mut report = RecoveryReport::default();
            self.end_lease_and_requeue(
                &lease,
                LeaseState::Expired,
                "result after expiry",
                now,
                &mut report,
            )?;
            self.journal(
                organization.as_str(),
                "stale_result_rejected",
                now,
                Some(&worker.worker_id),
                Some(&job.job_id),
                Some(generation),
                "lease had already expired".to_string(),
            )?;
            return Err(WorkerError::LeaseExpired {
                lease: lease_id.clone(),
                expired_at_ms: lease.expires_at_ms,
            });
        }
        let result = JobResult {
            job_id: job.job_id.clone(),
            generation,
            worker_id: worker.worker_id.clone(),
            lease_id: lease_id.clone(),
            digest: digest.to_string(),
            outcome,
            accepted_ms: now,
            verification,
        };
        let landed = self.store.land_result(&result)?;
        match landed {
            ResultAppend::Landed => {
                self.journal(
                    organization.as_str(),
                    "result_accepted",
                    now,
                    Some(&worker.worker_id),
                    Some(&job.job_id),
                    Some(generation),
                    format!("outcome={outcome:?} lease={lease_id}"),
                )?;
                Ok(ResultOutcome::Landed)
            }
            ResultAppend::Duplicate => {
                self.journal(
                    organization.as_str(),
                    "result_duplicate",
                    now,
                    Some(&worker.worker_id),
                    Some(&job.job_id),
                    Some(generation),
                    format!("lease={lease_id}"),
                )?;
                Ok(ResultOutcome::Duplicate)
            }
        }
    }

    // ------------------------------------------------------- status + recovery

    /// Org-scoped job status (root, generations, attempts, lease, result). A
    /// foreign organization is the SAME typed not-found as a missing job.
    pub fn job_status(
        &self,
        organization: &OrganizationId,
        job_id: &crate::ids::ExecutionJobId,
    ) -> Result<JobStatus, WorkerError> {
        let job = self.require_job(organization, job_id)?;
        Ok(JobStatus {
            generations: self.store.generations(job_id)?,
            attempts: self.store.attempts(job_id)?,
            lease: self
                .store
                .lease_for_generation(job_id, job.current_generation)?,
            result: self.store.result(job_id, job.current_generation)?,
            job,
        })
    }

    /// One org-scoped journal page (cursor = `seq`).
    pub fn journal_page(
        &self,
        organization: &OrganizationId,
        after_seq: Option<i64>,
        limit: usize,
    ) -> Result<Vec<JournalEntry>, WorkerError> {
        self.store.journal(
            organization.as_str(),
            after_seq,
            limit.clamp(1, MAX_JOURNAL_PAGE),
        )
    }

    /// The deterministic recovery sweep. Idempotent: a second immediate call
    /// is empty. Live leases inside their window are RESUMED untouched; live
    /// leases past their window EXPIRE (attempt lost + bounded requeue);
    /// assigned generations without a lease are re-assigned to an eligible
    /// worker when one exists. Result acceptance is never duplicated: the
    /// durable result row is unique per generation.
    pub fn recover(&self, organization: &OrganizationId) -> Result<RecoveryReport, WorkerError> {
        let now = self.now_ms();
        let mut report = RecoveryReport::default();
        for lease in self.store.live_leases(organization.as_str())? {
            if lease.is_expired_at(now) {
                self.end_lease_and_requeue(
                    &lease,
                    LeaseState::Expired,
                    "lease expired without heartbeats",
                    now,
                    &mut report,
                )?;
            } else {
                report.resumed.push(lease.lease_id.to_string());
            }
        }
        report.resumed.sort();
        Ok(report)
    }

    /// Complete the crash-recovery pass for a CRASHED SCHEDULER: re-attempt
    /// the assignment of every generation that is still `assigned` and has
    /// no lease, exactly like a fresh schedule would. Deterministic and
    /// idempotent; existing live leases are untouched.
    pub fn resume_assignments(
        &self,
        organization: &OrganizationId,
    ) -> Result<RecoveryReport, WorkerError> {
        let now = self.now_ms();
        let mut report = RecoveryReport::default();
        // Only jobs the scheduler minted are re-driven; the store has no
        // "list jobs" surface, so the journal's scheduled rows are the
        // durable index of every job of the organization.
        let mut after: Option<i64> = None;
        let mut seen: std::collections::BTreeSet<String> = Default::default();
        loop {
            let page = self
                .store
                .journal(organization.as_str(), after, MAX_JOURNAL_PAGE)?;
            if page.is_empty() {
                break;
            }
            after = page.last().map(|e| e.seq);
            for entry in &page {
                if entry.kind != "job_scheduled" && entry.kind != "job_requeued" {
                    continue;
                }
                let Some(job_id_raw) = &entry.job_id else {
                    continue;
                };
                if !seen.insert(job_id_raw.clone()) {
                    continue;
                }
                let Ok(job_id) = crate::ids::ExecutionJobId::try_new(job_id_raw.clone()) else {
                    continue;
                };
                let Some(job) = self.store.job(&job_id)? else {
                    continue;
                };
                if job.organization_id != organization.as_str() || job.state.is_terminal() {
                    continue;
                }
                if self
                    .store
                    .lease_for_generation(&job.job_id, job.current_generation)?
                    .is_some()
                {
                    continue;
                }
                let generation_row = self.store.generation(&job.job_id, job.current_generation)?;
                let Some(generation_row) = generation_row else {
                    continue;
                };
                if generation_row.state != GenerationState::Assigned {
                    continue;
                }
                let target = match &job.assigned_worker {
                    Some(id) => self
                        .require_eligible(id, organization, &job.trust_domain, &job.requirements)
                        .ok(),
                    None => self
                        .find_eligible(organization, &job.trust_domain, &job.requirements)
                        .ok()
                        .flatten(),
                };
                if let Some(worker) = target {
                    if self
                        .accept_lease_inner(
                            &worker,
                            organization,
                            &job,
                            job.current_generation,
                            now,
                        )
                        .is_ok()
                    {
                        report.reassigned.push(job.job_id.to_string());
                    }
                }
            }
            if page.len() < MAX_JOURNAL_PAGE {
                break;
            }
        }
        report.reassigned.sort();
        Ok(report)
    }

    /// The first eligible worker for one requirement set (used by the
    /// orchestrator's placement seam to decide remote vs local).
    pub fn find_eligible(
        &self,
        organization: &OrganizationId,
        trust_domain: &str,
        requirements: &JobRequirements,
    ) -> Result<Option<WorkerRegistration>, WorkerError> {
        let mut after: Option<String> = None;
        loop {
            let page =
                self.store
                    .workers(organization.as_str(), after.as_deref(), MAX_WORKER_PAGE)?;
            if page.is_empty() {
                return Ok(None);
            }
            after = page.last().map(|w| w.worker_id.to_string());
            let short_page = page.len() < MAX_WORKER_PAGE;
            for worker in page {
                if worker.revoked {
                    continue;
                }
                if worker.trust_domain != trust_domain {
                    continue;
                }
                if worker.capabilities.satisfies(requirements).is_err() {
                    continue;
                }
                if self.store.live_leases_of_worker(&worker.worker_id)?.len()
                    >= MAX_LIVE_LEASES_PER_WORKER
                {
                    continue;
                }
                return Ok(Some(worker));
            }
            if short_page {
                return Ok(None);
            }
        }
    }

    // ------------------------------------------------------------- internals

    fn current_generation_of(
        &self,
        job_id: &crate::ids::ExecutionJobId,
    ) -> Result<Option<JobGeneration>, WorkerError> {
        Ok(self.store.job(job_id)?.map(|j| j.current_generation))
    }

    fn require_job(
        &self,
        organization: &OrganizationId,
        job_id: &crate::ids::ExecutionJobId,
    ) -> Result<ExecutionJob, WorkerError> {
        let job = self
            .store
            .job(job_id)?
            .ok_or_else(|| WorkerError::UnknownJob(job_id.to_string()))?;
        if job.organization_id != organization.as_str() {
            return Err(WorkerError::UnknownJob(job_id.to_string()));
        }
        Ok(job)
    }

    fn require_eligible(
        &self,
        worker_id: &WorkerId,
        organization: &OrganizationId,
        trust_domain: &str,
        requirements: &JobRequirements,
    ) -> Result<WorkerRegistration, WorkerError> {
        let worker = self
            .store
            .worker(worker_id)?
            .ok_or_else(|| WorkerError::UnknownWorker(worker_id.clone()))?;
        if worker.organization_id != organization.as_str() {
            return Err(WorkerError::UnknownWorker(worker_id.clone()));
        }
        if worker.revoked {
            return Err(WorkerError::WorkerRevoked(worker_id.clone()));
        }
        if worker.trust_domain != trust_domain {
            return Err(WorkerError::ForeignTrustDomain {
                requested: trust_domain.to_string(),
                actual: worker.trust_domain.clone(),
            });
        }
        worker
            .capabilities
            .satisfies(requirements)
            .map_err(|detail| WorkerError::CapabilityMismatch {
                worker: worker_id.clone(),
                job: "(scheduled)".into(),
                detail,
            })?;
        Ok(worker)
    }

    /// Authenticate one worker with its registration token. A revoked worker
    /// or revoked token is refused typed.
    fn authenticate(
        &self,
        worker_id: &WorkerId,
        token: &SecretToken,
    ) -> Result<WorkerRegistration, WorkerError> {
        let worker = self
            .store
            .worker(worker_id)?
            .ok_or_else(|| WorkerError::UnknownWorker(worker_id.clone()))?;
        if worker.revoked {
            return Err(WorkerError::WorkerRevoked(worker_id.clone()));
        }
        let hash = TokenHash::of(token.expose());
        if worker.token_hash != hash.as_str() {
            return Err(WorkerError::UnknownToken);
        }
        match self.store.token(hash.as_str())? {
            Some(row) if row.revoked => Err(WorkerError::TokenRevoked),
            Some(_) => Ok(worker),
            None => Err(WorkerError::UnknownToken),
        }
    }

    /// End one lease (CAS) and derive the durable consequences: generation
    /// lost, attempt terminal, journal entry, then requeue-or-fail under the
    /// job's bounded policy. Idempotent: a lease someone else already moved
    /// yields no change.
    fn end_lease_and_requeue(
        &self,
        lease: &WorkerLease,
        state: LeaseState,
        reason: &str,
        now: i64,
        report: &mut RecoveryReport,
    ) -> Result<(), WorkerError> {
        if !self
            .store
            .end_lease(&lease.lease_id, lease.generation, state, now, reason)?
        {
            return Ok(());
        }
        report.expired.push(lease.lease_id.to_string());
        let _ = self.store.set_generation_state(
            &lease.job_id,
            lease.generation,
            GenerationState::Leased,
            GenerationState::Lost,
            Some(now),
            Some(reason),
        )?;
        let _ = self.store.set_attempt_state(
            &lease.job_id,
            lease.generation,
            attempt_number_for(&*self.store, &lease.job_id, lease.generation)?,
            AttemptState::Leased,
            AttemptState::Lost,
            Some(now),
            Some(&lease.worker_id),
            Some(&lease.lease_id),
            Some(reason),
        )?;
        self.journal(
            &lease.organization_id,
            "lease_expired",
            now,
            Some(&lease.worker_id),
            Some(&lease.job_id),
            Some(lease.generation),
            format!("lease={} reason={reason}", lease.lease_id),
        )?;
        // Requeue under the bounded policy.
        let Some(job) = self.store.job(&lease.job_id)? else {
            return Ok(());
        };
        if job.current_generation != lease.generation || job.state.is_terminal() {
            return Ok(());
        }
        let attempts = self.store.max_attempt_number(&job.job_id)?;
        if attempts >= job.requeue.max_attempts {
            self.store
                .set_job_state(&job.job_id, lease.generation, JobState::Failed)?;
            report.exhausted.push(job.job_id.to_string());
            self.journal(
                &job.organization_id,
                "attempts_exhausted",
                now,
                Some(&lease.worker_id),
                Some(&job.job_id),
                Some(lease.generation),
                format!("attempts={attempts} policy={}", job.requeue.max_attempts),
            )?;
            return Ok(());
        }
        let next_generation = match job.current_generation.next() {
            Ok(g) => g,
            Err(_) => {
                self.store
                    .set_job_state(&job.job_id, lease.generation, JobState::Failed)?;
                report.exhausted.push(job.job_id.to_string());
                return Ok(());
            }
        };
        let job_row = ExecutionJob {
            current_generation: next_generation,
            state: JobState::Assigned,
            ..job.clone()
        };
        let generation_row = JobGenerationRow {
            job_id: job.job_id.clone(),
            organization_id: job.organization_id.clone(),
            generation: next_generation,
            state: GenerationState::Assigned,
            created_ms: now,
            ended_ms: None,
            reason: Some(format!("requeued after {reason}")),
        };
        let attempt_row = JobAttempt {
            job_id: job.job_id.clone(),
            generation: next_generation,
            attempt: attempts.saturating_add(1),
            worker_id: None,
            lease_id: None,
            state: AttemptState::Pending,
            started_ms: now,
            ended_ms: None,
            reason: Some(format!("requeued after {reason}")),
        };
        let inserted =
            self.store
                .insert_generation_and_attempt(&job_row, &generation_row, &attempt_row)?;
        if !inserted {
            return Ok(());
        }
        report
            .requeued
            .push((job.job_id.to_string(), next_generation.as_u64()));
        self.journal(
            &job.organization_id,
            "job_requeued",
            now,
            Some(&lease.worker_id),
            Some(&job.job_id),
            Some(next_generation),
            format!("previous generation {} lost: {reason}", lease.generation),
        )?;
        // The requeued generation is open: assign it immediately when an
        // eligible worker exists (deterministic, bounded by the policy).
        let organization = match OrganizationId::try_new(job.organization_id.clone()) {
            Ok(org) => org,
            Err(_) => {
                return Err(WorkerError::Malformed(format!(
                    "job {} carries a malformed organization id",
                    job.job_id
                )))
            }
        };
        let target = match &job.assigned_worker {
            Some(id) => self
                .require_eligible(id, &organization, &job.trust_domain, &job.requirements)
                .ok(),
            None => self
                .find_eligible(&organization, &job.trust_domain, &job.requirements)
                .ok()
                .flatten(),
        };
        if let Some(worker) = target {
            let _ = self.accept_lease_inner(&worker, &organization, &job_row, next_generation, now);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn journal(
        &self,
        organization: &str,
        kind: &str,
        at_ms: i64,
        worker: Option<&WorkerId>,
        job: Option<&crate::ids::ExecutionJobId>,
        generation: Option<JobGeneration>,
        detail: String,
    ) -> Result<(), WorkerError> {
        let mut entry = JournalEntry::new(organization, kind, at_ms, detail)?;
        if let Some(job) = job {
            entry = entry.with_job(job, generation.unwrap_or(JobGeneration::FIRST));
        }
        if let Some(worker) = worker {
            entry = entry.with_worker(worker);
        }
        self.store.append_journal(&entry)
    }
}

fn attempt_number_for(
    store: &dyn WorkerStore,
    job_id: &crate::ids::ExecutionJobId,
    generation: JobGeneration,
) -> Result<u32, WorkerError> {
    Ok(store
        .attempt(job_id, generation, 1)?
        .map(|a| a.attempt)
        .unwrap_or(1))
}

fn check_protocol(presented: u32) -> Result<(), WorkerError> {
    if presented != WORKER_PROTOCOL_VERSION {
        return Err(WorkerError::ProtocolSkew {
            expected: WORKER_PROTOCOL_VERSION,
            got: presented,
        });
    }
    Ok(())
}

fn validate_digest(digest: &str) -> Result<(), WorkerError> {
    if digest.len() != RESULT_DIGEST_BYTES || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(WorkerError::Malformed(
            "payload digest must be 64 hex characters (sha256)".into(),
        ));
    }
    Ok(())
}

fn new_token() -> Result<SecretToken, WorkerError> {
    use rand::Rng;
    let mut rng = rand::rng();
    let bytes: [u8; 32] = rng.random();
    let mut token = String::with_capacity(67);
    token.push_str("wkr_");
    for byte in bytes {
        token.push_str(&format!("{byte:02x}"));
    }
    SecretToken::try_new(token).map_err(|e| WorkerError::Malformed(e.to_string()))
}

/// The default bounded requeue policy used when a caller does not supply one.
pub fn default_requeue() -> RequeuePolicy {
    RequeuePolicy {
        max_attempts: DEFAULT_MAX_ATTEMPTS,
    }
}
