//! Durable worker-plane state: the [`WorkerStore`] seam plus its in-memory
//! and SQLite implementations.
//!
//! The store owns the STRUCTURAL guarantees the plane depends on:
//!
//! - `wp_lease` carries `UNIQUE(job_id, generation)`: two workers accepting
//!   the same immutable generation can never both win — the CAS accept is
//!   decided by the database, not by a read-then-write race;
//! - `wp_result` carries `PRIMARY KEY(job_id, generation)`: a result for one
//!   generation lands at most once, ever (a duplicate replay is a typed
//!   `Duplicate`, a conflicting replay a typed conflict);
//! - `wp_generation` rows are append-only content: only the state/end fields
//!   move, guarded by an expected source state (a CAS), so a lease that was
//!   already superseded can never be re-opened;
//! - every state transition that must be joint (lease + generation + job +
//!   attempt) runs in ONE transaction through the ONE writer lock.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::error::WorkerError;
use crate::ids::{ExecutionJobId, JobGeneration, JobKey, WorkerId, WorkerLeaseId};
use crate::model::{
    AttemptState, ExecutionJob, GenerationState, JobAttempt, JobGenerationRow, JobResult,
    JournalEntry, LeaseState, WorkerLease, WorkerRegistration, WorkerTokenRow,
};

/// The SQL schema of the worker plane (migration v1 of its OWN database
/// file — the plane never shares the control-plane ladder, so enabling it
/// cannot perturb the commercial store's `user_version`).
pub const WORKER_SCHEMA_V1: &str = "
     CREATE TABLE IF NOT EXISTS wp_worker (
        id TEXT PRIMARY KEY,
        organization_id TEXT NOT NULL,
        trust_domain TEXT NOT NULL,
        revoked INTEGER NOT NULL,
        token_hash TEXT NOT NULL UNIQUE,
        payload TEXT NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_wp_worker_org ON wp_worker(organization_id, id);
     CREATE TABLE IF NOT EXISTS wp_token (
        token_hash TEXT PRIMARY KEY,
        organization_id TEXT NOT NULL,
        trust_domain TEXT NOT NULL,
        revoked INTEGER NOT NULL,
        consumed_by TEXT,
        payload TEXT NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_wp_token_org ON wp_token(organization_id, token_hash);
     CREATE TABLE IF NOT EXISTS wp_job (
        job_id TEXT PRIMARY KEY,
        organization_id TEXT NOT NULL,
        job_key TEXT NOT NULL,
        current_generation INTEGER NOT NULL,
        state TEXT NOT NULL,
        payload TEXT NOT NULL,
        UNIQUE (organization_id, job_key)
     );
     CREATE INDEX IF NOT EXISTS idx_wp_job_org ON wp_job(organization_id, job_id);
     CREATE TABLE IF NOT EXISTS wp_generation (
        job_id TEXT NOT NULL,
        generation INTEGER NOT NULL,
        organization_id TEXT NOT NULL,
        state TEXT NOT NULL,
        payload TEXT NOT NULL,
        PRIMARY KEY (job_id, generation)
     );
     CREATE INDEX IF NOT EXISTS idx_wp_generation_state
        ON wp_generation(organization_id, state, job_id);
     CREATE TABLE IF NOT EXISTS wp_lease (
        lease_id TEXT PRIMARY KEY,
        organization_id TEXT NOT NULL,
        job_id TEXT NOT NULL,
        generation INTEGER NOT NULL,
        worker_id TEXT NOT NULL,
        state TEXT NOT NULL,
        expires_at_ms INTEGER NOT NULL,
        payload TEXT NOT NULL,
        UNIQUE (job_id, generation)
     );
     CREATE INDEX IF NOT EXISTS idx_wp_lease_org_state
        ON wp_lease(organization_id, state, expires_at_ms);
     CREATE INDEX IF NOT EXISTS idx_wp_lease_worker ON wp_lease(worker_id, state);
     CREATE TABLE IF NOT EXISTS wp_attempt (
        job_id TEXT NOT NULL,
        generation INTEGER NOT NULL,
        attempt INTEGER NOT NULL,
        state TEXT NOT NULL,
        worker_id TEXT,
        lease_id TEXT,
        payload TEXT NOT NULL,
        PRIMARY KEY (job_id, generation, attempt)
     );
     CREATE INDEX IF NOT EXISTS idx_wp_attempt_job ON wp_attempt(job_id, generation);
     CREATE TABLE IF NOT EXISTS wp_result (
        job_id TEXT NOT NULL,
        generation INTEGER NOT NULL,
        organization_id TEXT NOT NULL,
        payload TEXT NOT NULL,
        PRIMARY KEY (job_id, generation)
     );
     CREATE INDEX IF NOT EXISTS idx_wp_result_org ON wp_result(organization_id, job_id);
     CREATE TABLE IF NOT EXISTS wp_journal (
        seq INTEGER PRIMARY KEY AUTOINCREMENT,
        organization_id TEXT NOT NULL,
        kind TEXT NOT NULL,
        job_id TEXT,
        worker_id TEXT,
        generation INTEGER,
        at_ms INTEGER NOT NULL,
        detail TEXT NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_wp_journal_org_seq ON wp_journal(organization_id, seq);
";

const WORKER_MIGRATIONS: &[&str] = &[
    WORKER_SCHEMA_V1,
    // v2 — the durability policy marker (P1 audit): the writer RECORDS the
    // acknowledged-durability policy (`synchronous = FULL`) so `doctor` can
    // observe it (SQLite pragmas are connection-scoped and invisible to a
    // separate probe connection). Owned by the durability stack
    // (`faktor_cloud::durability`), the same marker cloud records.
    faktor_cloud::durability::POLICY_SCHEMA_V5,
];

/// The outcome of one result landing attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultAppend {
    /// The result is durable and the lease/generation/attempt are completed.
    Landed,
    /// The identical result was already landed for this generation: nothing
    /// was written twice.
    Duplicate,
}

/// The durable worker-plane seam. Object-safe: the [`crate::service::WorkerPlane`]
/// holds one `Arc<dyn WorkerStore>`.
///
/// Compound transitions are implemented atomically; every CAS-style update
/// returns `false` when its guard (expected source state) no longer holds —
/// the caller must treat that as "someone else moved first", never retry
/// blindly.
pub trait WorkerStore: Send + Sync {
    fn put_worker(&self, worker: &WorkerRegistration) -> Result<(), WorkerError>;
    fn worker(&self, id: &WorkerId) -> Result<Option<WorkerRegistration>, WorkerError>;
    fn workers(
        &self,
        organization: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<WorkerRegistration>, WorkerError>;

    fn put_token(&self, token: &WorkerTokenRow) -> Result<(), WorkerError>;
    fn token(&self, token_hash: &str) -> Result<Option<WorkerTokenRow>, WorkerError>;

    fn put_job(&self, job: &ExecutionJob) -> Result<(), WorkerError>;
    fn job(&self, id: &ExecutionJobId) -> Result<Option<ExecutionJob>, WorkerError>;
    fn job_by_key(
        &self,
        organization: &str,
        key: &JobKey,
    ) -> Result<Option<ExecutionJob>, WorkerError>;

    fn put_generation(&self, row: &JobGenerationRow) -> Result<(), WorkerError>;
    fn generation(
        &self,
        job: &ExecutionJobId,
        generation: JobGeneration,
    ) -> Result<Option<JobGenerationRow>, WorkerError>;
    fn generations(&self, job: &ExecutionJobId) -> Result<Vec<JobGenerationRow>, WorkerError>;
    /// Atomically insert one generation + its attempt, updating the job root
    /// to point at the new generation. Returns `true` when the rows are new,
    /// `false` when byte-identical rows already exist (a replay).
    fn insert_generation_and_attempt(
        &self,
        job: &ExecutionJob,
        generation: &JobGenerationRow,
        attempt: &JobAttempt,
    ) -> Result<bool, WorkerError>;
    /// CAS transition of one generation's state. `from` must match the
    /// stored state or the call makes no change and returns `false`.
    fn set_generation_state(
        &self,
        job: &ExecutionJobId,
        generation: JobGeneration,
        from: GenerationState,
        to: GenerationState,
        ended_ms: Option<i64>,
        reason: Option<&str>,
    ) -> Result<bool, WorkerError>;
    /// CAS update of the job root state; the current generation must still
    /// match `expect_generation`.
    fn set_job_state(
        &self,
        job: &ExecutionJobId,
        expect_generation: JobGeneration,
        to: crate::model::JobState,
    ) -> Result<bool, WorkerError>;

    /// The atomic CAS accept: if a lease already exists for
    /// `(job_id, generation)` it is returned and nothing is written;
    /// otherwise the lease row is inserted, the generation moves
    /// `Assigned -> Leased`, the job root moves `Assigned -> Leased` and the
    /// `(job, generation)` attempt row moves to `Leased` with this worker —
    /// all in one transaction. `Ok(None)` means this caller won the accept.
    fn try_accept_lease(&self, lease: &WorkerLease) -> Result<Option<WorkerLease>, WorkerError>;
    fn lease(&self, id: &WorkerLeaseId) -> Result<Option<WorkerLease>, WorkerError>;
    fn lease_for_generation(
        &self,
        job: &ExecutionJobId,
        generation: JobGeneration,
    ) -> Result<Option<WorkerLease>, WorkerError>;
    /// Every lease of one organization still marked `live` (including ones
    /// already past their expiry — the caller decides the sweep).
    fn live_leases(&self, organization: &str) -> Result<Vec<WorkerLease>, WorkerError>;
    fn live_leases_of_worker(&self, worker: &WorkerId) -> Result<Vec<WorkerLease>, WorkerError>;
    /// CAS renew: only a `live` lease of `expect_generation` is renewed.
    fn renew_lease(
        &self,
        lease: &WorkerLeaseId,
        expect_generation: JobGeneration,
        last_heartbeat_ms: i64,
        expires_at_ms: i64,
    ) -> Result<bool, WorkerError>;
    /// CAS end: only a `live` lease of `expect_generation` is ended.
    fn end_lease(
        &self,
        lease: &WorkerLeaseId,
        expect_generation: JobGeneration,
        state: LeaseState,
        ended_ms: i64,
        reason: &str,
    ) -> Result<bool, WorkerError>;

    fn put_attempt(&self, attempt: &JobAttempt) -> Result<(), WorkerError>;
    fn attempt(
        &self,
        job: &ExecutionJobId,
        generation: JobGeneration,
        attempt: u32,
    ) -> Result<Option<JobAttempt>, WorkerError>;
    fn attempts(&self, job: &ExecutionJobId) -> Result<Vec<JobAttempt>, WorkerError>;
    /// Highest attempt number recorded for one job (`0` = none).
    fn max_attempt_number(&self, job: &ExecutionJobId) -> Result<u32, WorkerError>;
    /// CAS update of one attempt row (keyed by job/generation/attempt); the
    /// stored row must still be in `expect` state.
    #[allow(clippy::too_many_arguments)]
    fn set_attempt_state(
        &self,
        job: &ExecutionJobId,
        generation: JobGeneration,
        attempt: u32,
        expect: AttemptState,
        to: AttemptState,
        ended_ms: Option<i64>,
        worker: Option<&WorkerId>,
        lease: Option<&WorkerLeaseId>,
        reason: Option<&str>,
    ) -> Result<bool, WorkerError>;

    /// Atomically land a result: the lease must still be `live` and belong
    /// to the result's worker, the generation must still be current and the
    /// stored generation must still be `leased` (or `assigned`, for an
    /// immediate local path); then the result row is inserted (never
    /// twice), the lease completes, the generation completes and the attempt
    /// completes. A stale generation refuses typed and nothing is written.
    fn land_result(&self, result: &JobResult) -> Result<ResultAppend, WorkerError>;
    fn result(
        &self,
        job: &ExecutionJobId,
        generation: JobGeneration,
    ) -> Result<Option<JobResult>, WorkerError>;

    fn append_journal(&self, entry: &JournalEntry) -> Result<(), WorkerError>;
    fn journal(
        &self,
        organization: &str,
        after_seq: Option<i64>,
        limit: usize,
    ) -> Result<Vec<JournalEntry>, WorkerError>;
}

// ------------------------------------------------------------------- memory

#[derive(Default)]
struct MemoryState {
    workers: BTreeMap<String, WorkerRegistration>,
    tokens: BTreeMap<String, WorkerTokenRow>,
    jobs: BTreeMap<String, ExecutionJob>,
    generations: BTreeMap<(String, u64), JobGenerationRow>,
    leases: BTreeMap<String, WorkerLease>,
    attempts: BTreeMap<(String, u64, u32), JobAttempt>,
    results: BTreeMap<(String, u64), JobResult>,
    journal: Vec<JournalEntry>,
}

/// The in-memory twin (tests, ephemeral hosts). Same structural guarantees
/// as the SQLite store: unique lease per generation, unique result per
/// generation, CAS-guarded transitions, one mutex as the writer.
#[derive(Default)]
pub struct MemoryWorkerStore {
    state: Mutex<MemoryState>,
}

impl MemoryWorkerStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, MemoryState>, WorkerError> {
        self.state
            .lock()
            .map_err(|_| WorkerError::Backend("worker store lock is poisoned".into()))
    }
}

fn encode<T: Serialize>(value: &T) -> Result<String, WorkerError> {
    serde_json::to_string(value)
        .map_err(|e| WorkerError::Malformed(format!("worker row encode: {e}")))
}

/// A stable "content identity" of one stored value (used to detect a
/// conflicting replay through the SQLite payload column).
pub(crate) fn content_identity<T: Serialize>(value: &T) -> Result<String, WorkerError> {
    encode(value)
}

impl WorkerStore for MemoryWorkerStore {
    fn put_worker(&self, worker: &WorkerRegistration) -> Result<(), WorkerError> {
        let mut state = self.lock()?;
        state
            .workers
            .insert(worker.worker_id.to_string(), worker.clone());
        Ok(())
    }

    fn worker(&self, id: &WorkerId) -> Result<Option<WorkerRegistration>, WorkerError> {
        Ok(self.lock()?.workers.get(id.as_str()).cloned())
    }

    fn workers(
        &self,
        organization: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<WorkerRegistration>, WorkerError> {
        let state = self.lock()?;
        Ok(state
            .workers
            .values()
            .filter(|w| w.organization_id == organization)
            .filter(|w| after.is_none_or(|a| w.worker_id.as_str() > a))
            .take(limit)
            .cloned()
            .collect())
    }

    fn put_token(&self, token: &WorkerTokenRow) -> Result<(), WorkerError> {
        self.lock()?
            .tokens
            .insert(token.token_hash.clone(), token.clone());
        Ok(())
    }

    fn token(&self, token_hash: &str) -> Result<Option<WorkerTokenRow>, WorkerError> {
        Ok(self.lock()?.tokens.get(token_hash).cloned())
    }

    fn put_job(&self, job: &ExecutionJob) -> Result<(), WorkerError> {
        self.lock()?
            .jobs
            .insert(job.job_id.to_string(), job.clone());
        Ok(())
    }

    fn job(&self, id: &ExecutionJobId) -> Result<Option<ExecutionJob>, WorkerError> {
        Ok(self.lock()?.jobs.get(id.as_str()).cloned())
    }

    fn job_by_key(
        &self,
        organization: &str,
        key: &JobKey,
    ) -> Result<Option<ExecutionJob>, WorkerError> {
        Ok(self
            .lock()?
            .jobs
            .values()
            .find(|j| j.organization_id == organization && j.job_key == *key)
            .cloned())
    }

    fn put_generation(&self, row: &JobGenerationRow) -> Result<(), WorkerError> {
        self.lock()?.generations.insert(
            (row.job_id.to_string(), row.generation.as_u64()),
            row.clone(),
        );
        Ok(())
    }

    fn generation(
        &self,
        job: &ExecutionJobId,
        generation: JobGeneration,
    ) -> Result<Option<JobGenerationRow>, WorkerError> {
        Ok(self
            .lock()?
            .generations
            .get(&(job.to_string(), generation.as_u64()))
            .cloned())
    }

    fn generations(&self, job: &ExecutionJobId) -> Result<Vec<JobGenerationRow>, WorkerError> {
        let state = self.lock()?;
        let mut rows: Vec<JobGenerationRow> = state
            .generations
            .iter()
            .filter(|((j, _), _)| j == job.as_str())
            .map(|(_, row)| row.clone())
            .collect();
        rows.sort_by_key(|r| r.generation.as_u64());
        Ok(rows)
    }

    fn insert_generation_and_attempt(
        &self,
        job: &ExecutionJob,
        generation: &JobGenerationRow,
        attempt: &JobAttempt,
    ) -> Result<bool, WorkerError> {
        let mut state = self.lock()?;
        let key = (
            generation.job_id.to_string(),
            generation.generation.as_u64(),
        );
        let akey = (
            attempt.job_id.to_string(),
            attempt.generation.as_u64(),
            attempt.attempt,
        );
        if let Some(existing) = state.generations.get(&key) {
            let same = content_identity(existing)? == content_identity(generation)?
                && state
                    .attempts
                    .get(&akey)
                    .map(|a| content_identity(a).ok() == content_identity(attempt).ok())
                    .unwrap_or(false);
            if !same {
                return Err(WorkerError::Malformed(format!(
                    "conflicting generation insert for job {} generation {}",
                    generation.job_id, generation.generation
                )));
            }
            return Ok(false);
        }
        state.generations.insert(key, generation.clone());
        state.attempts.insert(akey, attempt.clone());
        state.jobs.insert(job.job_id.to_string(), job.clone());
        Ok(true)
    }

    fn set_generation_state(
        &self,
        job: &ExecutionJobId,
        generation: JobGeneration,
        from: GenerationState,
        to: GenerationState,
        ended_ms: Option<i64>,
        reason: Option<&str>,
    ) -> Result<bool, WorkerError> {
        let mut state = self.lock()?;
        let key = (job.to_string(), generation.as_u64());
        let Some(row) = state.generations.get_mut(&key) else {
            return Ok(false);
        };
        if row.state != from {
            return Ok(false);
        }
        row.state = to;
        if ended_ms.is_some() {
            row.ended_ms = ended_ms;
        }
        if reason.is_some() {
            row.reason = reason.map(|r| r.to_string());
        }
        Ok(true)
    }

    fn set_job_state(
        &self,
        job: &ExecutionJobId,
        expect_generation: JobGeneration,
        to: crate::model::JobState,
    ) -> Result<bool, WorkerError> {
        let mut state = self.lock()?;
        let Some(row) = state.jobs.get_mut(job.as_str()) else {
            return Ok(false);
        };
        if row.current_generation != expect_generation {
            return Ok(false);
        }
        row.state = to;
        Ok(true)
    }

    fn try_accept_lease(&self, lease: &WorkerLease) -> Result<Option<WorkerLease>, WorkerError> {
        let mut state = self.lock()?;
        if let Some(existing) = state
            .leases
            .values()
            .find(|l| l.job_id == lease.job_id && l.generation == lease.generation)
        {
            return Ok(Some(existing.clone()));
        }
        state
            .leases
            .insert(lease.lease_id.to_string(), lease.clone());
        let key = (lease.job_id.to_string(), lease.generation.as_u64());
        if let Some(row) = state.generations.get_mut(&key) {
            if row.state == GenerationState::Assigned {
                row.state = GenerationState::Leased;
            }
        }
        if let Some(job) = state.jobs.get_mut(lease.job_id.as_str()) {
            if job.current_generation == lease.generation
                && job.state == crate::model::JobState::Assigned
            {
                job.state = crate::model::JobState::Leased;
            }
        }
        let akey = (lease.job_id.to_string(), lease.generation.as_u64(), 1);
        if let Some(attempt) = state.attempts.get_mut(&akey) {
            if attempt.state == AttemptState::Pending {
                attempt.state = AttemptState::Leased;
                attempt.worker_id = Some(lease.worker_id.clone());
                attempt.lease_id = Some(lease.lease_id.clone());
            }
        }
        Ok(None)
    }

    fn lease(&self, id: &WorkerLeaseId) -> Result<Option<WorkerLease>, WorkerError> {
        Ok(self.lock()?.leases.get(id.as_str()).cloned())
    }

    fn lease_for_generation(
        &self,
        job: &ExecutionJobId,
        generation: JobGeneration,
    ) -> Result<Option<WorkerLease>, WorkerError> {
        Ok(self
            .lock()?
            .leases
            .values()
            .find(|l| l.job_id == *job && l.generation == generation)
            .cloned())
    }

    fn live_leases(&self, organization: &str) -> Result<Vec<WorkerLease>, WorkerError> {
        let state = self.lock()?;
        let mut rows: Vec<WorkerLease> = state
            .leases
            .values()
            .filter(|l| l.organization_id == organization && l.state == LeaseState::Live)
            .cloned()
            .collect();
        rows.sort_by(|a, b| a.lease_id.cmp(&b.lease_id));
        Ok(rows)
    }

    fn live_leases_of_worker(&self, worker: &WorkerId) -> Result<Vec<WorkerLease>, WorkerError> {
        let state = self.lock()?;
        let mut rows: Vec<WorkerLease> = state
            .leases
            .values()
            .filter(|l| l.worker_id == *worker && l.state == LeaseState::Live)
            .cloned()
            .collect();
        rows.sort_by(|a, b| a.lease_id.cmp(&b.lease_id));
        Ok(rows)
    }

    fn renew_lease(
        &self,
        lease: &WorkerLeaseId,
        expect_generation: JobGeneration,
        last_heartbeat_ms: i64,
        expires_at_ms: i64,
    ) -> Result<bool, WorkerError> {
        let mut state = self.lock()?;
        let Some(row) = state.leases.get_mut(lease.as_str()) else {
            return Ok(false);
        };
        if row.state != LeaseState::Live || row.generation != expect_generation {
            return Ok(false);
        }
        // Renewal can only move the expiry FORWARD: a stale/late heartbeat
        // can never shorten a lease.
        if expires_at_ms > row.expires_at_ms {
            row.expires_at_ms = expires_at_ms;
        }
        row.last_heartbeat_ms = row.last_heartbeat_ms.max(last_heartbeat_ms);
        Ok(true)
    }

    fn end_lease(
        &self,
        lease: &WorkerLeaseId,
        expect_generation: JobGeneration,
        state_to: LeaseState,
        ended_ms: i64,
        reason: &str,
    ) -> Result<bool, WorkerError> {
        let mut state = self.lock()?;
        let Some(row) = state.leases.get_mut(lease.as_str()) else {
            return Ok(false);
        };
        if row.state != LeaseState::Live || row.generation != expect_generation {
            return Ok(false);
        }
        row.state = state_to;
        row.ended_ms = Some(ended_ms);
        row.reason = Some(reason.to_string());
        Ok(true)
    }

    fn put_attempt(&self, attempt: &JobAttempt) -> Result<(), WorkerError> {
        self.lock()?.attempts.insert(
            (
                attempt.job_id.to_string(),
                attempt.generation.as_u64(),
                attempt.attempt,
            ),
            attempt.clone(),
        );
        Ok(())
    }

    fn attempt(
        &self,
        job: &ExecutionJobId,
        generation: JobGeneration,
        attempt: u32,
    ) -> Result<Option<JobAttempt>, WorkerError> {
        Ok(self
            .lock()?
            .attempts
            .get(&(job.to_string(), generation.as_u64(), attempt))
            .cloned())
    }

    fn attempts(&self, job: &ExecutionJobId) -> Result<Vec<JobAttempt>, WorkerError> {
        let state = self.lock()?;
        let mut rows: Vec<JobAttempt> = state
            .attempts
            .iter()
            .filter(|((j, _, _), _)| j == job.as_str())
            .map(|(_, a)| a.clone())
            .collect();
        rows.sort_by_key(|a| (a.generation.as_u64(), a.attempt));
        Ok(rows)
    }

    fn max_attempt_number(&self, job: &ExecutionJobId) -> Result<u32, WorkerError> {
        Ok(self
            .lock()?
            .attempts
            .keys()
            .filter(|(j, _, _)| j == job.as_str())
            .map(|(_, _, a)| *a)
            .max()
            .unwrap_or(0))
    }

    #[allow(clippy::too_many_arguments)]
    fn set_attempt_state(
        &self,
        job: &ExecutionJobId,
        generation: JobGeneration,
        attempt: u32,
        expect: AttemptState,
        to: AttemptState,
        ended_ms: Option<i64>,
        worker: Option<&WorkerId>,
        lease: Option<&WorkerLeaseId>,
        reason: Option<&str>,
    ) -> Result<bool, WorkerError> {
        let mut state = self.lock()?;
        let Some(row) = state
            .attempts
            .get_mut(&(job.to_string(), generation.as_u64(), attempt))
        else {
            return Ok(false);
        };
        if row.state != expect {
            return Ok(false);
        }
        row.state = to;
        if ended_ms.is_some() {
            row.ended_ms = ended_ms;
        }
        if worker.is_some() {
            row.worker_id = worker.cloned();
        }
        if lease.is_some() {
            row.lease_id = lease.cloned();
        }
        if reason.is_some() {
            row.reason = reason.map(|r| r.to_string());
        }
        Ok(true)
    }

    fn land_result(&self, result: &JobResult) -> Result<ResultAppend, WorkerError> {
        let mut state = self.lock()?;
        let key = (result.job_id.to_string(), result.generation.as_u64());
        if let Some(existing) = state.results.get(&key) {
            return if existing.digest == result.digest
                && existing.worker_id == result.worker_id
                && existing.lease_id == result.lease_id
            {
                Ok(ResultAppend::Duplicate)
            } else {
                Err(WorkerError::ResultConflict {
                    job: result.job_id.to_string(),
                    generation: result.generation,
                })
            };
        }
        let job = state
            .jobs
            .get(result.job_id.as_str())
            .cloned()
            .ok_or_else(|| WorkerError::UnknownJob(result.job_id.to_string()))?;
        if job.current_generation != result.generation {
            return Err(WorkerError::SupersededLease {
                job: result.job_id.to_string(),
                generation: result.generation,
                current: job.current_generation,
            });
        }
        let lease = state
            .leases
            .get(result.lease_id.as_str())
            .cloned()
            .ok_or_else(|| WorkerError::UnknownLease {
                lease: result.lease_id.clone(),
                worker: result.worker_id.clone(),
            })?;
        if lease.worker_id != result.worker_id || lease.generation != result.generation {
            return Err(WorkerError::UnknownLease {
                lease: result.lease_id.clone(),
                worker: result.worker_id.clone(),
            });
        }
        if lease.state != LeaseState::Live {
            return Err(WorkerError::LeaseNotLive {
                lease: result.lease_id.clone(),
                state: lease.state.as_str().to_string(),
            });
        }
        state.results.insert(key, result.clone());
        if let Some(row) = state.leases.get_mut(result.lease_id.as_str()) {
            row.state = LeaseState::Completed;
            row.ended_ms = Some(result.accepted_ms);
        }
        let gkey = (result.job_id.to_string(), result.generation.as_u64());
        if let Some(row) = state.generations.get_mut(&gkey) {
            if !row.state.is_terminal() {
                row.state = GenerationState::Completed;
                row.ended_ms = Some(result.accepted_ms);
            }
        }
        if let Some(root) = state.jobs.get_mut(result.job_id.as_str()) {
            root.state = crate::model::JobState::Completed;
        }
        let akey = (result.job_id.to_string(), result.generation.as_u64(), 1);
        if let Some(attempt) = state.attempts.get_mut(&akey) {
            if !attempt.state.is_terminal() {
                attempt.state = AttemptState::Completed;
                attempt.ended_ms = Some(result.accepted_ms);
            }
        }
        Ok(ResultAppend::Landed)
    }

    fn result(
        &self,
        job: &ExecutionJobId,
        generation: JobGeneration,
    ) -> Result<Option<JobResult>, WorkerError> {
        Ok(self
            .lock()?
            .results
            .get(&(job.to_string(), generation.as_u64()))
            .cloned())
    }

    fn append_journal(&self, entry: &JournalEntry) -> Result<(), WorkerError> {
        let mut state = self.lock()?;
        let mut entry = entry.clone();
        entry.seq = state.journal.len() as i64 + 1;
        state.journal.push(entry);
        Ok(())
    }

    fn journal(
        &self,
        organization: &str,
        after_seq: Option<i64>,
        limit: usize,
    ) -> Result<Vec<JournalEntry>, WorkerError> {
        let state = self.lock()?;
        Ok(state
            .journal
            .iter()
            .filter(|e| e.organization_id == organization)
            .filter(|e| after_seq.is_none_or(|a| e.seq > a))
            .take(limit)
            .cloned()
            .collect())
    }
}

// ------------------------------------------------------------------- sqlite

/// The durable [`WorkerStore`] over its own SQLite database file: WAL +
/// busy-timeout + an explicit migration cursor (`PRAGMA user_version`), and
/// one writer mutex so every compound transition is serialized.
pub struct SqliteWorkerStore {
    conn: Mutex<Connection>,
}

impl SqliteWorkerStore {
    /// Open (creating) the worker-plane database at `path`.
    ///
    /// Durability policy (P1 audit; the [`faktor_cloud::durability`] stack):
    /// the writer connection opens `synchronous = FULL`, file-backed opens
    /// record the policy marker, write a VERIFIED pre-migration restore point
    /// before any schema transition away from an existing version, and run
    /// the interval-gated rotating backup.
    pub fn open(path: &Path) -> Result<Self, WorkerError> {
        let conn = Connection::open(path).map_err(backend)?;
        Self::prepare(conn, Some(path))
    }

    /// Open an in-memory database (tests, ephemeral hosts).
    pub fn open_in_memory() -> Result<Self, WorkerError> {
        let conn = Connection::open_in_memory().map_err(backend)?;
        Self::prepare(conn, None)
    }

    fn prepare(conn: Connection, path: Option<&Path>) -> Result<Self, WorkerError> {
        // The acknowledged-durability policy (WAL + synchronous = FULL; see
        // `faktor_cloud::durability` for the documented choice) applies to
        // EVERY open, in-memory included, before any migration or query.
        faktor_cloud::durability::apply_policy(&conn).map_err(durability)?;
        let mut conn = conn;
        migrate(&mut conn, path)?;
        if let Some(path) = path {
            let now = faktor_cloud::durability::now_ms();
            // Record the writer's policy for `doctor` (best effort: a full
            // disk must not take the worker plane down; doctor then reports
            // the absent/stale marker loudly).
            if let Err(e) = faktor_cloud::durability::record_open_policy(&conn, now) {
                tracing::error!("worker durability marker not recorded: {e}");
            }
            // Interval-gated verified backup. Best effort, like the daemon's
            // startup backup.
            match faktor_cloud::durability::rotate_backup(&conn, path) {
                Ok(Some(dest)) => {
                    tracing::info!("worker backup written to {}", dest.display());
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("worker backup skipped: {e}"),
            }
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, WorkerError> {
        self.conn
            .lock()
            .map_err(|_| WorkerError::Backend("worker store lock is poisoned".into()))
    }
}

fn backend(e: rusqlite::Error) -> WorkerError {
    WorkerError::Backend(e.to_string())
}

fn durability(e: faktor_cloud::CloudStoreError) -> WorkerError {
    WorkerError::Backend(e.to_string())
}

fn migrate(conn: &mut Connection, db_path: Option<&Path>) -> Result<(), WorkerError> {
    let mut version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(backend)?;
    if version >= WORKER_MIGRATIONS.len() as i64 {
        return Ok(());
    }
    // A schema transition on an EXISTING database is irreversible structural
    // work: before the first pending migration runs, a verified pre-migration
    // restore point must be durable. If it cannot be written and
    // restore-verified, the migration is REFUSED (open fails).
    if version > 0 {
        if let Some(path) = db_path {
            faktor_cloud::durability::migration_backup(conn, path, version).map_err(|e| {
                WorkerError::Backend(format!(
                    "refusing migration without a verified pre-migration restore point: {e}"
                ))
            })?;
            #[cfg(test)]
            if take_injected_crash(path) {
                return Err(WorkerError::Backend(
                    "injected crash after the pre-migration restore point".into(),
                ));
            }
        }
    }
    for (i, sql) in WORKER_MIGRATIONS.iter().enumerate() {
        let target = (i + 1) as i64;
        if version >= target {
            continue;
        }
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(backend)?;
        tx.execute_batch(sql)
            .map_err(|e| WorkerError::Backend(format!("worker migration v{target}: {e}")))?;
        tx.execute_batch(&format!("PRAGMA user_version = {target}"))
            .map_err(|e| WorkerError::Backend(format!("worker migration v{target} cursor: {e}")))?;
        tx.commit().map_err(backend)?;
        version = target;
    }
    Ok(())
}

/// Test-only one-shot: make the NEXT migration of exactly `db_path` fail
/// AFTER the pre-migration restore point and BEFORE any migration SQL,
/// reproducing the crash-mid-migration durable state. Keyed by path so
/// concurrent tests never consume each other's injection.
#[cfg(test)]
static MIGRATION_CRASH: std::sync::Mutex<Option<std::path::PathBuf>> = std::sync::Mutex::new(None);

/// Arm [`MIGRATION_CRASH`] for `db_path` (one-shot; tests only).
#[cfg(test)]
fn inject_crash_before_migration(db_path: &Path) {
    *MIGRATION_CRASH.lock().unwrap() = Some(db_path.to_path_buf());
}

/// Consume the one-shot injection when it targets `db_path`.
#[cfg(test)]
fn take_injected_crash(db_path: &Path) -> bool {
    let mut armed = MIGRATION_CRASH.lock().unwrap();
    if armed.as_deref() == Some(db_path) {
        *armed = None;
        true
    } else {
        false
    }
}

fn parse<T: for<'de> serde::Deserialize<'de>>(payload: &str) -> Result<T, WorkerError> {
    serde_json::from_str(payload)
        .map_err(|e| WorkerError::Malformed(format!("worker row payload: {e}")))
}

fn payload_string(
    conn: &Connection,
    table: &str,
    key: &str,
    id: &str,
) -> Result<Option<String>, WorkerError> {
    let sql = format!("SELECT payload FROM {table} WHERE {key} = ?1");
    conn.query_row(&sql, params![id], |r| r.get(0))
        .optional()
        .map_err(backend)
}

impl WorkerStore for SqliteWorkerStore {
    fn put_worker(&self, worker: &WorkerRegistration) -> Result<(), WorkerError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO wp_worker (id, organization_id, trust_domain, revoked, token_hash, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
                organization_id = excluded.organization_id,
                trust_domain = excluded.trust_domain,
                revoked = excluded.revoked,
                payload = excluded.payload",
            params![
                worker.worker_id.as_str(),
                worker.organization_id,
                worker.trust_domain,
                worker.revoked as i64,
                worker.token_hash,
                encode(worker)?
            ],
        )
        .map_err(backend)?;
        Ok(())
    }

    fn worker(&self, id: &WorkerId) -> Result<Option<WorkerRegistration>, WorkerError> {
        let conn = self.lock()?;
        match payload_string(&conn, "wp_worker", "id", id.as_str())? {
            Some(payload) => Ok(Some(parse(&payload)?)),
            None => Ok(None),
        }
    }

    fn workers(
        &self,
        organization: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<WorkerRegistration>, WorkerError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT payload FROM wp_worker
                 WHERE organization_id = ?1 AND (?2 IS NULL OR id > ?2)
                 ORDER BY id LIMIT ?3",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map(params![organization, after, limit as i64], |r| {
                r.get::<_, String>(0)
            })
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        rows.iter().map(|p| parse(p)).collect()
    }

    fn put_token(&self, token: &WorkerTokenRow) -> Result<(), WorkerError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO wp_token (token_hash, organization_id, trust_domain, revoked, consumed_by, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(token_hash) DO UPDATE SET
                revoked = excluded.revoked,
                consumed_by = excluded.consumed_by,
                payload = excluded.payload",
            params![
                token.token_hash,
                token.organization_id,
                token.trust_domain,
                token.revoked as i64,
                token.consumed_by,
                encode(token)?
            ],
        )
        .map_err(backend)?;
        Ok(())
    }

    fn token(&self, token_hash: &str) -> Result<Option<WorkerTokenRow>, WorkerError> {
        let conn = self.lock()?;
        match payload_string(&conn, "wp_token", "token_hash", token_hash)? {
            Some(payload) => Ok(Some(parse(&payload)?)),
            None => Ok(None),
        }
    }

    fn put_job(&self, job: &ExecutionJob) -> Result<(), WorkerError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO wp_job (job_id, organization_id, job_key, current_generation, state, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(job_id) DO UPDATE SET
                current_generation = excluded.current_generation,
                state = excluded.state,
                payload = excluded.payload",
            params![
                job.job_id.as_str(),
                job.organization_id,
                job.job_key.as_str(),
                job.current_generation.as_u64() as i64,
                job.state.as_str(),
                encode(job)?
            ],
        )
        .map_err(backend)?;
        Ok(())
    }

    fn job(&self, id: &ExecutionJobId) -> Result<Option<ExecutionJob>, WorkerError> {
        let conn = self.lock()?;
        match payload_string(&conn, "wp_job", "job_id", id.as_str())? {
            Some(payload) => Ok(Some(parse(&payload)?)),
            None => Ok(None),
        }
    }

    fn job_by_key(
        &self,
        organization: &str,
        key: &JobKey,
    ) -> Result<Option<ExecutionJob>, WorkerError> {
        let conn = self.lock()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM wp_job WHERE organization_id = ?1 AND job_key = ?2",
                params![organization, key.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        match payload {
            Some(payload) => Ok(Some(parse(&payload)?)),
            None => Ok(None),
        }
    }

    fn put_generation(&self, row: &JobGenerationRow) -> Result<(), WorkerError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO wp_generation (job_id, generation, organization_id, state, payload)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(job_id, generation) DO UPDATE SET
                state = excluded.state,
                payload = excluded.payload",
            params![
                row.job_id.as_str(),
                row.generation.as_u64() as i64,
                row.organization_id,
                row.state.as_str(),
                encode(row)?
            ],
        )
        .map_err(backend)?;
        Ok(())
    }

    fn generation(
        &self,
        job: &ExecutionJobId,
        generation: JobGeneration,
    ) -> Result<Option<JobGenerationRow>, WorkerError> {
        let conn = self.lock()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM wp_generation WHERE job_id = ?1 AND generation = ?2",
                params![job.as_str(), generation.as_u64() as i64],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        match payload {
            Some(payload) => Ok(Some(parse(&payload)?)),
            None => Ok(None),
        }
    }

    fn generations(&self, job: &ExecutionJobId) -> Result<Vec<JobGenerationRow>, WorkerError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare("SELECT payload FROM wp_generation WHERE job_id = ?1 ORDER BY generation")
            .map_err(backend)?;
        let rows = stmt
            .query_map(params![job.as_str()], |r| r.get::<_, String>(0))
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        rows.iter().map(|p| parse(p)).collect()
    }

    fn insert_generation_and_attempt(
        &self,
        job: &ExecutionJob,
        generation: &JobGenerationRow,
        attempt: &JobAttempt,
    ) -> Result<bool, WorkerError> {
        let mut conn = self.lock()?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(backend)?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT payload FROM wp_generation WHERE job_id = ?1 AND generation = ?2",
                params![
                    generation.job_id.as_str(),
                    generation.generation.as_u64() as i64
                ],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        if let Some(existing) = existing {
            let same_generation = parse::<JobGenerationRow>(&existing)? == *generation;
            let existing_attempt: Option<String> = tx
                .query_row(
                    "SELECT payload FROM wp_attempt WHERE job_id = ?1 AND generation = ?2 AND attempt = ?3",
                    params![
                        attempt.job_id.as_str(),
                        attempt.generation.as_u64() as i64,
                        attempt.attempt as i64
                    ],
                    |r| r.get(0),
                )
                .optional()
                .map_err(backend)?;
            let same_attempt = existing_attempt
                .map(|p| parse::<JobAttempt>(&p).map(|a| a == *attempt))
                .transpose()?
                .unwrap_or(false);
            tx.commit().map_err(backend)?;
            if same_generation && same_attempt {
                return Ok(false);
            }
            return Err(WorkerError::Malformed(format!(
                "conflicting generation insert for job {} generation {}",
                generation.job_id, generation.generation
            )));
        }
        tx.execute(
            "INSERT INTO wp_generation (job_id, generation, organization_id, state, payload)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                generation.job_id.as_str(),
                generation.generation.as_u64() as i64,
                generation.organization_id,
                generation.state.as_str(),
                encode(generation)?
            ],
        )
        .map_err(backend)?;
        tx.execute(
            "INSERT INTO wp_attempt (job_id, generation, attempt, state, worker_id, lease_id, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                attempt.job_id.as_str(),
                attempt.generation.as_u64() as i64,
                attempt.attempt as i64,
                attempt.state.as_str(),
                attempt.worker_id.as_ref().map(|w| w.as_str()),
                attempt.lease_id.as_ref().map(|l| l.as_str()),
                encode(attempt)?
            ],
        )
        .map_err(backend)?;
        tx.execute(
            "INSERT INTO wp_job (job_id, organization_id, job_key, current_generation, state, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(job_id) DO UPDATE SET
                current_generation = excluded.current_generation,
                state = excluded.state,
                payload = excluded.payload",
            params![
                job.job_id.as_str(),
                job.organization_id,
                job.job_key.as_str(),
                job.current_generation.as_u64() as i64,
                job.state.as_str(),
                encode(job)?
            ],
        )
        .map_err(backend)?;
        tx.commit().map_err(backend)?;
        Ok(true)
    }

    fn set_generation_state(
        &self,
        job: &ExecutionJobId,
        generation: JobGeneration,
        from: GenerationState,
        to: GenerationState,
        ended_ms: Option<i64>,
        reason: Option<&str>,
    ) -> Result<bool, WorkerError> {
        let mut conn = self.lock()?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(backend)?;
        let payload: Option<String> = tx
            .query_row(
                "SELECT payload FROM wp_generation WHERE job_id = ?1 AND generation = ?2",
                params![job.as_str(), generation.as_u64() as i64],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        let Some(payload) = payload else {
            tx.commit().map_err(backend)?;
            return Ok(false);
        };
        let mut row: JobGenerationRow = parse(&payload)?;
        if row.state != from {
            tx.commit().map_err(backend)?;
            return Ok(false);
        }
        row.state = to;
        if ended_ms.is_some() {
            row.ended_ms = ended_ms;
        }
        if reason.is_some() {
            row.reason = reason.map(|r| r.to_string());
        }
        tx.execute(
            "UPDATE wp_generation SET state = ?3, payload = ?4 WHERE job_id = ?1 AND generation = ?2",
            params![
                job.as_str(),
                generation.as_u64() as i64,
                row.state.as_str(),
                encode(&row)?
            ],
        )
        .map_err(backend)?;
        tx.commit().map_err(backend)?;
        Ok(true)
    }

    fn set_job_state(
        &self,
        job: &ExecutionJobId,
        expect_generation: JobGeneration,
        to: crate::model::JobState,
    ) -> Result<bool, WorkerError> {
        let mut conn = self.lock()?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(backend)?;
        let payload: Option<String> = tx
            .query_row(
                "SELECT payload FROM wp_job WHERE job_id = ?1",
                params![job.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        let Some(payload) = payload else {
            tx.commit().map_err(backend)?;
            return Ok(false);
        };
        let mut root: ExecutionJob = parse(&payload)?;
        if root.current_generation != expect_generation {
            tx.commit().map_err(backend)?;
            return Ok(false);
        }
        root.state = to;
        tx.execute(
            "UPDATE wp_job SET state = ?2, payload = ?3 WHERE job_id = ?1",
            params![job.as_str(), root.state.as_str(), encode(&root)?],
        )
        .map_err(backend)?;
        tx.commit().map_err(backend)?;
        Ok(true)
    }

    fn try_accept_lease(&self, lease: &WorkerLease) -> Result<Option<WorkerLease>, WorkerError> {
        let mut conn = self.lock()?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(backend)?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT payload FROM wp_lease WHERE job_id = ?1 AND generation = ?2",
                params![lease.job_id.as_str(), lease.generation.as_u64() as i64],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        if let Some(existing) = existing {
            let existing: WorkerLease = parse(&existing)?;
            tx.commit().map_err(backend)?;
            return Ok(Some(existing));
        }
        tx.execute(
            "INSERT INTO wp_lease
                (lease_id, organization_id, job_id, generation, worker_id, state, expires_at_ms, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                lease.lease_id.as_str(),
                lease.organization_id,
                lease.job_id.as_str(),
                lease.generation.as_u64() as i64,
                lease.worker_id.as_str(),
                lease.state.as_str(),
                lease.expires_at_ms,
                encode(lease)?
            ],
        )
        .map_err(backend)?;
        // Generation Assigned -> Leased (CAS; a moved generation is not
        // re-opened — the lease row still exists and names its generation).
        let generation_payload: Option<String> = tx
            .query_row(
                "SELECT payload FROM wp_generation WHERE job_id = ?1 AND generation = ?2",
                params![lease.job_id.as_str(), lease.generation.as_u64() as i64],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        if let Some(payload) = generation_payload {
            let mut row: JobGenerationRow = parse(&payload)?;
            if row.state == GenerationState::Assigned {
                row.state = GenerationState::Leased;
                tx.execute(
                    "UPDATE wp_generation SET state = ?3, payload = ?4 WHERE job_id = ?1 AND generation = ?2",
                    params![
                        lease.job_id.as_str(),
                        lease.generation.as_u64() as i64,
                        row.state.as_str(),
                        encode(&row)?
                    ],
                )
                .map_err(backend)?;
            }
        }
        let job_payload: Option<String> = tx
            .query_row(
                "SELECT payload FROM wp_job WHERE job_id = ?1",
                params![lease.job_id.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        if let Some(payload) = job_payload {
            let mut root: ExecutionJob = parse(&payload)?;
            if root.current_generation == lease.generation
                && root.state == crate::model::JobState::Assigned
            {
                root.state = crate::model::JobState::Leased;
                tx.execute(
                    "UPDATE wp_job SET state = ?2, payload = ?3 WHERE job_id = ?1",
                    params![lease.job_id.as_str(), root.state.as_str(), encode(&root)?],
                )
                .map_err(backend)?;
            }
        }
        let attempt_payload: Option<String> = tx
            .query_row(
                "SELECT payload FROM wp_attempt
                 WHERE job_id = ?1 AND generation = ?2 ORDER BY attempt LIMIT 1",
                params![lease.job_id.as_str(), lease.generation.as_u64() as i64],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        if let Some(payload) = attempt_payload {
            let mut attempt: JobAttempt = parse(&payload)?;
            if attempt.state == AttemptState::Pending {
                attempt.state = AttemptState::Leased;
                attempt.worker_id = Some(lease.worker_id.clone());
                attempt.lease_id = Some(lease.lease_id.clone());
                tx.execute(
                    "UPDATE wp_attempt SET state = ?4, worker_id = ?5, lease_id = ?6, payload = ?7
                     WHERE job_id = ?1 AND generation = ?2 AND attempt = ?3",
                    params![
                        attempt.job_id.as_str(),
                        attempt.generation.as_u64() as i64,
                        attempt.attempt as i64,
                        attempt.state.as_str(),
                        attempt.worker_id.as_ref().map(|w| w.as_str()),
                        attempt.lease_id.as_ref().map(|l| l.as_str()),
                        encode(&attempt)?
                    ],
                )
                .map_err(backend)?;
            }
        }
        tx.commit().map_err(backend)?;
        Ok(None)
    }

    fn lease(&self, id: &WorkerLeaseId) -> Result<Option<WorkerLease>, WorkerError> {
        let conn = self.lock()?;
        match payload_string(&conn, "wp_lease", "lease_id", id.as_str())? {
            Some(payload) => Ok(Some(parse(&payload)?)),
            None => Ok(None),
        }
    }

    fn lease_for_generation(
        &self,
        job: &ExecutionJobId,
        generation: JobGeneration,
    ) -> Result<Option<WorkerLease>, WorkerError> {
        let conn = self.lock()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM wp_lease WHERE job_id = ?1 AND generation = ?2",
                params![job.as_str(), generation.as_u64() as i64],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        match payload {
            Some(payload) => Ok(Some(parse(&payload)?)),
            None => Ok(None),
        }
    }

    fn live_leases(&self, organization: &str) -> Result<Vec<WorkerLease>, WorkerError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT payload FROM wp_lease
                 WHERE organization_id = ?1 AND state = 'live' ORDER BY lease_id",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map(params![organization], |r| r.get::<_, String>(0))
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        rows.iter().map(|p| parse(p)).collect()
    }

    fn live_leases_of_worker(&self, worker: &WorkerId) -> Result<Vec<WorkerLease>, WorkerError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT payload FROM wp_lease
                 WHERE worker_id = ?1 AND state = 'live' ORDER BY lease_id",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map(params![worker.as_str()], |r| r.get::<_, String>(0))
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        rows.iter().map(|p| parse(p)).collect()
    }

    fn renew_lease(
        &self,
        lease: &WorkerLeaseId,
        expect_generation: JobGeneration,
        last_heartbeat_ms: i64,
        expires_at_ms: i64,
    ) -> Result<bool, WorkerError> {
        let mut conn = self.lock()?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(backend)?;
        let payload: Option<String> = tx
            .query_row(
                "SELECT payload FROM wp_lease WHERE lease_id = ?1",
                params![lease.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        let Some(payload) = payload else {
            tx.commit().map_err(backend)?;
            return Ok(false);
        };
        let mut row: WorkerLease = parse(&payload)?;
        if row.state != LeaseState::Live || row.generation != expect_generation {
            tx.commit().map_err(backend)?;
            return Ok(false);
        }
        if expires_at_ms > row.expires_at_ms {
            row.expires_at_ms = expires_at_ms;
        }
        row.last_heartbeat_ms = row.last_heartbeat_ms.max(last_heartbeat_ms);
        tx.execute(
            "UPDATE wp_lease SET expires_at_ms = ?2, payload = ?3 WHERE lease_id = ?1",
            params![lease.as_str(), row.expires_at_ms, encode(&row)?],
        )
        .map_err(backend)?;
        tx.commit().map_err(backend)?;
        Ok(true)
    }

    fn end_lease(
        &self,
        lease: &WorkerLeaseId,
        expect_generation: JobGeneration,
        state_to: LeaseState,
        ended_ms: i64,
        reason: &str,
    ) -> Result<bool, WorkerError> {
        let mut conn = self.lock()?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(backend)?;
        let payload: Option<String> = tx
            .query_row(
                "SELECT payload FROM wp_lease WHERE lease_id = ?1",
                params![lease.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        let Some(payload) = payload else {
            tx.commit().map_err(backend)?;
            return Ok(false);
        };
        let mut row: WorkerLease = parse(&payload)?;
        if row.state != LeaseState::Live || row.generation != expect_generation {
            tx.commit().map_err(backend)?;
            return Ok(false);
        }
        row.state = state_to;
        row.ended_ms = Some(ended_ms);
        row.reason = Some(reason.to_string());
        tx.execute(
            "UPDATE wp_lease SET state = ?2, payload = ?3 WHERE lease_id = ?1",
            params![lease.as_str(), row.state.as_str(), encode(&row)?],
        )
        .map_err(backend)?;
        tx.commit().map_err(backend)?;
        Ok(true)
    }

    fn put_attempt(&self, attempt: &JobAttempt) -> Result<(), WorkerError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO wp_attempt (job_id, generation, attempt, state, worker_id, lease_id, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(job_id, generation, attempt) DO UPDATE SET
                state = excluded.state,
                worker_id = excluded.worker_id,
                lease_id = excluded.lease_id,
                payload = excluded.payload",
            params![
                attempt.job_id.as_str(),
                attempt.generation.as_u64() as i64,
                attempt.attempt as i64,
                attempt.state.as_str(),
                attempt.worker_id.as_ref().map(|w| w.as_str()),
                attempt.lease_id.as_ref().map(|l| l.as_str()),
                encode(attempt)?
            ],
        )
        .map_err(backend)?;
        Ok(())
    }

    fn attempt(
        &self,
        job: &ExecutionJobId,
        generation: JobGeneration,
        attempt: u32,
    ) -> Result<Option<JobAttempt>, WorkerError> {
        let conn = self.lock()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM wp_attempt WHERE job_id = ?1 AND generation = ?2 AND attempt = ?3",
                params![job.as_str(), generation.as_u64() as i64, attempt as i64],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        match payload {
            Some(payload) => Ok(Some(parse(&payload)?)),
            None => Ok(None),
        }
    }

    fn attempts(&self, job: &ExecutionJobId) -> Result<Vec<JobAttempt>, WorkerError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT payload FROM wp_attempt WHERE job_id = ?1 ORDER BY generation, attempt",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map(params![job.as_str()], |r| r.get::<_, String>(0))
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        rows.iter().map(|p| parse(p)).collect()
    }

    fn max_attempt_number(&self, job: &ExecutionJobId) -> Result<u32, WorkerError> {
        let conn = self.lock()?;
        let max: Option<i64> = conn
            .query_row(
                "SELECT MAX(attempt) FROM wp_attempt WHERE job_id = ?1",
                params![job.as_str()],
                |r| r.get(0),
            )
            .map_err(backend)?;
        // The column only ever holds `u32` attempt numbers written by the
        // typed API (bit-cast into the signed column), so the read side casts
        // back through u64 and a value that does not fit is corruption:
        // refuse it by field name instead of silently wrapping with `as u32`.
        let max = max.unwrap_or(0);
        u32::try_from(max as u64).map_err(|_| {
            WorkerError::Malformed(format!(
                "wp_attempt max attempt number {max} for job {} is outside u32",
                job.as_str()
            ))
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn set_attempt_state(
        &self,
        job: &ExecutionJobId,
        generation: JobGeneration,
        attempt: u32,
        expect: AttemptState,
        to: AttemptState,
        ended_ms: Option<i64>,
        worker: Option<&WorkerId>,
        lease: Option<&WorkerLeaseId>,
        reason: Option<&str>,
    ) -> Result<bool, WorkerError> {
        let mut conn = self.lock()?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(backend)?;
        let payload: Option<String> = tx
            .query_row(
                "SELECT payload FROM wp_attempt WHERE job_id = ?1 AND generation = ?2 AND attempt = ?3",
                params![job.as_str(), generation.as_u64() as i64, attempt as i64],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        let Some(payload) = payload else {
            tx.commit().map_err(backend)?;
            return Ok(false);
        };
        let mut row: JobAttempt = parse(&payload)?;
        if row.state != expect {
            tx.commit().map_err(backend)?;
            return Ok(false);
        }
        row.state = to;
        if ended_ms.is_some() {
            row.ended_ms = ended_ms;
        }
        if worker.is_some() {
            row.worker_id = worker.cloned();
        }
        if lease.is_some() {
            row.lease_id = lease.cloned();
        }
        if reason.is_some() {
            row.reason = reason.map(|r| r.to_string());
        }
        tx.execute(
            "UPDATE wp_attempt SET state = ?4, worker_id = ?5, lease_id = ?6, payload = ?7
             WHERE job_id = ?1 AND generation = ?2 AND attempt = ?3",
            params![
                job.as_str(),
                generation.as_u64() as i64,
                attempt as i64,
                row.state.as_str(),
                row.worker_id.as_ref().map(|w| w.as_str()),
                row.lease_id.as_ref().map(|l| l.as_str()),
                encode(&row)?
            ],
        )
        .map_err(backend)?;
        tx.commit().map_err(backend)?;
        Ok(true)
    }

    fn land_result(&self, result: &JobResult) -> Result<ResultAppend, WorkerError> {
        let mut conn = self.lock()?;
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(backend)?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT payload FROM wp_result WHERE job_id = ?1 AND generation = ?2",
                params![result.job_id.as_str(), result.generation.as_u64() as i64],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        if let Some(existing) = existing {
            let existing: JobResult = parse(&existing)?;
            tx.commit().map_err(backend)?;
            return if existing.digest == result.digest
                && existing.worker_id == result.worker_id
                && existing.lease_id == result.lease_id
            {
                Ok(ResultAppend::Duplicate)
            } else {
                Err(WorkerError::ResultConflict {
                    job: result.job_id.to_string(),
                    generation: result.generation,
                })
            };
        }
        let job_payload: Option<String> = tx
            .query_row(
                "SELECT payload FROM wp_job WHERE job_id = ?1",
                params![result.job_id.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        let Some(job_payload) = job_payload else {
            tx.commit().map_err(backend)?;
            return Err(WorkerError::UnknownJob(result.job_id.to_string()));
        };
        let mut job: ExecutionJob = parse(&job_payload)?;
        if job.current_generation != result.generation {
            tx.commit().map_err(backend)?;
            return Err(WorkerError::SupersededLease {
                job: result.job_id.to_string(),
                generation: result.generation,
                current: job.current_generation,
            });
        }
        let lease_payload: Option<String> = tx
            .query_row(
                "SELECT payload FROM wp_lease WHERE lease_id = ?1",
                params![result.lease_id.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        let Some(lease_payload) = lease_payload else {
            tx.commit().map_err(backend)?;
            return Err(WorkerError::UnknownLease {
                lease: result.lease_id.clone(),
                worker: result.worker_id.clone(),
            });
        };
        let mut lease: WorkerLease = parse(&lease_payload)?;
        if lease.worker_id != result.worker_id || lease.generation != result.generation {
            tx.commit().map_err(backend)?;
            return Err(WorkerError::UnknownLease {
                lease: result.lease_id.clone(),
                worker: result.worker_id.clone(),
            });
        }
        if lease.state != LeaseState::Live {
            tx.commit().map_err(backend)?;
            return Err(WorkerError::LeaseNotLive {
                lease: result.lease_id.clone(),
                state: lease.state.as_str().to_string(),
            });
        }
        tx.execute(
            "INSERT INTO wp_result (job_id, generation, organization_id, payload)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                result.job_id.as_str(),
                result.generation.as_u64() as i64,
                job.organization_id,
                encode(result)?
            ],
        )
        .map_err(backend)?;
        lease.state = LeaseState::Completed;
        lease.ended_ms = Some(result.accepted_ms);
        tx.execute(
            "UPDATE wp_lease SET state = 'completed', payload = ?2 WHERE lease_id = ?1",
            params![lease.lease_id.as_str(), encode(&lease)?],
        )
        .map_err(backend)?;
        let generation_payload: Option<String> = tx
            .query_row(
                "SELECT payload FROM wp_generation WHERE job_id = ?1 AND generation = ?2",
                params![result.job_id.as_str(), result.generation.as_u64() as i64],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        if let Some(payload) = generation_payload {
            let mut row: JobGenerationRow = parse(&payload)?;
            if !row.state.is_terminal() {
                row.state = GenerationState::Completed;
                row.ended_ms = Some(result.accepted_ms);
            }
            tx.execute(
                "UPDATE wp_generation SET state = ?3, payload = ?4 WHERE job_id = ?1 AND generation = ?2",
                params![
                    result.job_id.as_str(),
                    result.generation.as_u64() as i64,
                    row.state.as_str(),
                    encode(&row)?
                ],
            )
            .map_err(backend)?;
        }
        job.state = crate::model::JobState::Completed;
        tx.execute(
            "UPDATE wp_job SET state = 'completed', payload = ?2 WHERE job_id = ?1",
            params![job.job_id.as_str(), encode(&job)?],
        )
        .map_err(backend)?;
        let attempt_payload: Option<String> = tx
            .query_row(
                "SELECT payload FROM wp_attempt WHERE job_id = ?1 AND generation = ?2 ORDER BY attempt LIMIT 1",
                params![result.job_id.as_str(), result.generation.as_u64() as i64],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        if let Some(payload) = attempt_payload {
            let mut attempt: JobAttempt = parse(&payload)?;
            if !attempt.state.is_terminal() {
                attempt.state = AttemptState::Completed;
                attempt.ended_ms = Some(result.accepted_ms);
                tx.execute(
                    "UPDATE wp_attempt SET state = 'completed', payload = ?4
                     WHERE job_id = ?1 AND generation = ?2 AND attempt = ?3",
                    params![
                        attempt.job_id.as_str(),
                        attempt.generation.as_u64() as i64,
                        attempt.attempt as i64,
                        encode(&attempt)?
                    ],
                )
                .map_err(backend)?;
            }
        }
        tx.commit().map_err(backend)?;
        Ok(ResultAppend::Landed)
    }

    fn result(
        &self,
        job: &ExecutionJobId,
        generation: JobGeneration,
    ) -> Result<Option<JobResult>, WorkerError> {
        let conn = self.lock()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM wp_result WHERE job_id = ?1 AND generation = ?2",
                params![job.as_str(), generation.as_u64() as i64],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        match payload {
            Some(payload) => Ok(Some(parse(&payload)?)),
            None => Ok(None),
        }
    }

    fn append_journal(&self, entry: &JournalEntry) -> Result<(), WorkerError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO wp_journal
                (organization_id, kind, job_id, worker_id, generation, at_ms, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                entry.organization_id,
                entry.kind,
                entry.job_id,
                entry.worker_id,
                entry.generation.map(|g| g.as_u64() as i64),
                entry.at_ms,
                entry.detail
            ],
        )
        .map_err(backend)?;
        Ok(())
    }

    fn journal(
        &self,
        organization: &str,
        after_seq: Option<i64>,
        limit: usize,
    ) -> Result<Vec<JournalEntry>, WorkerError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT seq, organization_id, kind, job_id, worker_id, generation, at_ms, detail
                 FROM wp_journal
                 WHERE organization_id = ?1 AND (?2 IS NULL OR seq > ?2)
                 ORDER BY seq LIMIT ?3",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map(params![organization, after_seq, limit as i64], |r| {
                Ok(JournalEntry {
                    seq: r.get(0)?,
                    organization_id: r.get(1)?,
                    kind: r.get(2)?,
                    job_id: r.get(3)?,
                    worker_id: r.get(4)?,
                    generation: r.get::<_, Option<i64>>(5)?.map(|g| JobGeneration(g as u64)),
                    at_ms: r.get(6)?,
                    detail: r.get(7)?,
                })
            })
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        Ok(rows)
    }
}

/// A tiny helper so tests and the service can read the schema version.
pub fn schema_version(store: &SqliteWorkerStore) -> Result<i64, WorkerError> {
    let conn = store.lock()?;
    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(backend)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::WorkerCapabilities;
    use crate::model::{JobRequirements, JobState, NetworkProfile, RequeuePolicy};

    /// P1 durability: the commercial worker-plane writer connection
    /// acknowledges a commit only after an fsync (`synchronous = FULL`), so a
    /// crash/power loss cannot roll back an acknowledged job/lease/result row.
    #[test]
    fn commercial_writer_connection_is_synchronous_full() {
        let store = SqliteWorkerStore::open_in_memory().unwrap();
        let conn = store.lock().unwrap();
        let sync: i64 = conn
            .query_row("PRAGMA synchronous", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sync, 2, "synchronous = FULL on the writer connection");
    }

    fn job(id: &str, org: &str) -> ExecutionJob {
        ExecutionJob {
            job_id: ExecutionJobId::try_new(id).unwrap(),
            organization_id: org.into(),
            trust_domain: org.into(),
            job_key: JobKey::try_new("key").unwrap(),
            requirements: JobRequirements {
                os: None,
                arch: None,
                toolchains: vec![],
                sandbox: vec![],
                network: None,
                min_cpu_cores: 0,
                min_memory_mb: 0,
                gpu: false,
                region: None,
                trust_domain: org.into(),
            },
            payload_digest: "a".repeat(64),
            requeue: RequeuePolicy::default(),
            assigned_worker: None,
            created_ms: 1,
            current_generation: JobGeneration::FIRST,
            state: JobState::Assigned,
        }
    }

    /// A file-backed open records the writer policy marker `doctor` reads and
    /// writes a first restore-verified rotating backup.
    #[test]
    fn file_backed_open_records_the_policy_marker_and_a_verified_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workers.db");
        drop(SqliteWorkerStore::open(&path).unwrap());
        let report = faktor_cloud::durability::doctor_probe(&path, false).unwrap();
        assert_eq!(report.journal_mode, "wal");
        assert!(report.integrity.is_empty(), "{:?}", report.integrity);
        let policy = report.policy.expect("the policy marker is recorded");
        for (key, value) in [
            ("synchronous", "FULL"),
            ("backup_policy", "rotating"),
            ("policy_version", "1"),
        ] {
            assert_eq!(
                policy
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| v.as_str()),
                Some(value),
                "marker key {key}"
            );
        }
        let (backup, _) = report
            .last_backup
            .expect("the first open writes a verified rotating backup");
        let expected = {
            let conn = Connection::open(&path).unwrap();
            faktor_cloud::durability::canonical_fingerprint(&conn).unwrap()
        };
        faktor_cloud::durability::restore_verify(&backup, &expected).unwrap();
    }

    /// Adversarial: more verified snapshots than the retention bound. The
    /// rotation must bound the kept set, and the newest must still verify.
    #[test]
    fn verified_backups_rotate_within_both_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workers.db");
        let store = SqliteWorkerStore::open(&path).unwrap();
        let conn = store.lock().unwrap();
        for _ in 0..(faktor_cloud::durability::MAX_BACKUP_FILES + 4) {
            let dest = faktor_cloud::durability::force_backup(&conn, &path).unwrap();
            let fp = faktor_cloud::durability::canonical_fingerprint(&conn).unwrap();
            faktor_cloud::durability::restore_verify(&dest, &fp).unwrap();
        }
        let kept = faktor_cloud::durability::list_rotating_backups(&path);
        assert_eq!(
            kept.len(),
            faktor_cloud::durability::MAX_BACKUP_FILES,
            "the rotating set is bounded"
        );
        let newest = kept.first().expect("a rotating backup is kept");
        let fp = faktor_cloud::durability::canonical_fingerprint(&conn).unwrap();
        faktor_cloud::durability::restore_verify(newest, &fp).unwrap();
        // Migration restore points are a separate class: never rotated as
        // rotating backups.
        faktor_cloud::durability::migration_backup(&conn, &path, 1).unwrap();
        assert_eq!(
            faktor_cloud::durability::list_rotating_backups(&path).len(),
            faktor_cloud::durability::MAX_BACKUP_FILES
        );
        assert!(faktor_cloud::durability::latest_migration_backup(&path).is_some());
    }

    /// Crash-mid-migration: the injected failure fires AFTER the
    /// pre-migration restore point and BEFORE any migration SQL. The database
    /// must stay at the old version, the restore point must exist and
    /// restore-verify, and `doctor` must report it.
    #[test]
    fn migration_crash_leaves_a_verified_restore_point_doctor_reports() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workers.db");
        drop(SqliteWorkerStore::open(&path).unwrap());
        // Roll the cursor back one version so the next open has a pending
        // migration (the tables are already there; the transition is the
        // crash point under test).
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA user_version = 1").unwrap();
        }
        inject_crash_before_migration(&path);
        let err = SqliteWorkerStore::open(&path)
            .err()
            .expect("the injected crash must fail the open");
        assert!(
            err.to_string().contains("injected crash"),
            "the failure is the injected crash: {err}"
        );
        {
            let conn = Connection::open(&path).unwrap();
            let version: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(version, 1, "no migration ran without its restore point");
        }
        let report = faktor_cloud::durability::doctor_probe(&path, false).unwrap();
        let (point, _) = report
            .migration_restore_point
            .expect("doctor reports the pre-migration restore point");
        assert!(
            point
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .contains("-pre-migration-v1-"),
            "the restore point names the version it protects: {}",
            point.display()
        );
        // The restore point is a real v1 database, not a partial copy.
        let backup =
            Connection::open_with_flags(&point, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        let version: i64 = backup
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 1);
        assert!(faktor_cloud::durability::integrity_check(&backup, true)
            .unwrap()
            .is_empty());
        // A clean re-open completes the migration and records the policy.
        drop(SqliteWorkerStore::open(&path).unwrap());
        let report = faktor_cloud::durability::doctor_probe(&path, false).unwrap();
        assert!(report.policy.is_some());
    }

    /// Adversarial: when the pre-migration restore point CANNOT be written,
    /// the migration is refused and the database stays at the old version —
    /// never a schema change without a way back.
    #[test]
    fn migration_is_refused_when_the_restore_point_cannot_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workers.db");
        drop(SqliteWorkerStore::open(&path).unwrap());
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA user_version = 1").unwrap();
        }
        // Block the backup directory: a regular file where the directory must
        // be makes every restore-point write impossible.
        let blocked = faktor_cloud::durability::backup_dir(&path);
        let _ = std::fs::remove_dir_all(&blocked);
        std::fs::write(&blocked, b"not a directory").unwrap();
        let err = SqliteWorkerStore::open(&path)
            .err()
            .expect("a migration without its restore point must be refused");
        assert!(
            err.to_string()
                .contains("refusing migration without a verified pre-migration restore point"),
            "the refusal is typed and names the gate: {err}"
        );
        {
            let conn = Connection::open(&path).unwrap();
            let version: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(version, 1, "the refused migration never ran");
        }
        // Unblock: the same open now migrates and records the policy.
        std::fs::remove_file(&blocked).unwrap();
        drop(SqliteWorkerStore::open(&path).unwrap());
        let report = faktor_cloud::durability::doctor_probe(&path, false).unwrap();
        assert!(report.policy.is_some());
    }

    /// The kill proof: the child opens the worker database, records an
    /// acknowledged worker registration, prints the ACK + writer pragma, and
    /// hangs; the parent SIGKILLs it and reopens. The acknowledged
    /// registration must be there.
    #[test]
    fn acknowledged_worker_commit_survives_sigkill_and_reopen() {
        const CHILD_ENV: &str = "FAKTOR_WORKER_DURABILITY_CHILD_DB";
        if let Ok(db_path) = std::env::var(CHILD_ENV) {
            child_acknowledged_worker_put(&db_path);
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workers.db");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("store::tests::acknowledged_worker_commit_survives_sigkill_and_reopen")
            .arg("--nocapture")
            .env(CHILD_ENV, &path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut lines = std::io::BufRead::lines(std::io::BufReader::new(stdout));
        let mut ack = None;
        while let Some(Ok(line)) = lines.next() {
            if let Some(rest) = line.strip_prefix("ACK ") {
                ack = Some(rest.to_string());
                break;
            }
        }
        child.kill().unwrap();
        let _ = child.wait();
        let ack = ack.expect("the child acknowledges its worker registration");
        assert!(
            ack.contains("SYNC=2"),
            "the writer connection was FULL at acknowledgement: {ack}"
        );
        let store = SqliteWorkerStore::open(&path).unwrap();
        let worker = store
            .worker(&WorkerId::try_new("wrk_kill").unwrap())
            .unwrap()
            .expect("the acknowledged registration survived the kill");
        assert_eq!(worker.organization_id, "org_kill");
    }

    /// The child body of the kill test. Never returns: the parent SIGKILLs it
    /// after the ACK (the bounded loop is only the fail-safe).
    fn child_acknowledged_worker_put(db_path: &str) -> ! {
        let store = SqliteWorkerStore::open(Path::new(db_path)).unwrap();
        let sync: i64 = {
            let conn = store.lock().unwrap();
            conn.query_row("PRAGMA synchronous", [], |r| r.get(0))
                .unwrap()
        };
        store
            .put_worker(&WorkerRegistration {
                worker_id: WorkerId::try_new("wrk_kill").unwrap(),
                organization_id: "org_kill".into(),
                trust_domain: "org_kill".into(),
                display_name: "kill-proof".into(),
                token_hash: "b".repeat(64),
                revoked: false,
                capabilities: WorkerCapabilities {
                    os: "linux".into(),
                    arch: "x86_64".into(),
                    toolchains: vec!["rust".into()],
                    sandbox: vec![],
                    network: NetworkProfile::None,
                    cpu_cores: 2,
                    memory_mb: 1024,
                    gpu: None,
                    region: "local".into(),
                    trust_domain: "org_kill".into(),
                    protocol_version: crate::error::WORKER_PROTOCOL_VERSION,
                },
                capabilities_version: 1,
                registered_ms: 7,
                last_seen_ms: 7,
                revoked_ms: None,
            })
            .unwrap();
        println!("ACK SYNC={sync}");
        loop {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// The schema is a real migration: opening twice is idempotent, and the
    /// cursor lands on v2 (v1 schema + the durability policy marker).
    #[test]
    fn sqlite_schema_version_is_two_and_reopen_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workers.db");
        let store = SqliteWorkerStore::open(&path).unwrap();
        assert_eq!(schema_version(&store).unwrap(), 2);
        drop(store);
        let store = SqliteWorkerStore::open(&path).unwrap();
        assert_eq!(schema_version(&store).unwrap(), 2);
    }

    /// Adversarial: corrupt the durable payload of one row and prove the
    /// read is a typed Malformed refusal, never a silent default.
    #[test]
    fn corrupt_payload_is_a_typed_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workers.db");
        let store = SqliteWorkerStore::open(&path).unwrap();
        let j = job("job_1", "org_1");
        store.put_job(&j).unwrap();
        {
            let conn = store.lock().unwrap();
            conn.execute("UPDATE wp_job SET payload = 'not json'", [])
                .unwrap();
        }
        let err = store.job(&j.job_id).unwrap_err();
        assert!(matches!(err, WorkerError::Malformed(_)), "{err}");
    }

    /// Adversarial: an out-of-range persisted attempt number is refused by
    /// field name, never narrowed with `as u32`; the largest valid `u32`
    /// value still round-trips exactly.
    #[test]
    fn oversize_persisted_attempt_number_is_a_typed_field_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workers.db");
        let store = SqliteWorkerStore::open(&path).unwrap();
        let j = job("job_1", "org_1");
        store.put_job(&j).unwrap();
        {
            let conn = store.lock().unwrap();
            conn.execute(
                "INSERT INTO wp_attempt(job_id, generation, attempt, state, worker_id, lease_id, payload)
                 VALUES (?1, 1, ?2, 'running', NULL, NULL, '{}')",
                params![j.job_id.as_str(), i64::from(u32::MAX) + 1],
            )
            .unwrap();
        }
        match store.max_attempt_number(&j.job_id).unwrap_err() {
            WorkerError::Malformed(msg) => {
                assert!(msg.contains("max attempt number"), "{msg}");
                assert!(msg.contains("outside u32"), "{msg}");
            }
            other => panic!("oversize attempt must be a typed Malformed refusal: {other}"),
        }
        {
            let conn = store.lock().unwrap();
            conn.execute("DELETE FROM wp_attempt", []).unwrap();
            conn.execute(
                "INSERT INTO wp_attempt(job_id, generation, attempt, state, worker_id, lease_id, payload)
                 VALUES (?1, 1, ?2, 'running', NULL, NULL, '{}')",
                params![j.job_id.as_str(), i64::from(u32::MAX)],
            )
            .unwrap();
        }
        assert_eq!(
            store.max_attempt_number(&j.job_id).unwrap(),
            u32::MAX,
            "u32::MAX must survive the i64 column round-trip exactly"
        );
    }

    /// The CAS accept is structural: two acceptors of one generation yield
    /// exactly one lease, and the loser sees the winner.
    #[test]
    fn two_accepts_yield_exactly_one_lease() {
        let store = MemoryWorkerStore::new();
        store.put_job(&job("job_1", "org_1")).unwrap();
        let lease = WorkerLease {
            lease_id: WorkerLeaseId::try_new("lease_a").unwrap(),
            organization_id: "org_1".into(),
            job_id: ExecutionJobId::try_new("job_1").unwrap(),
            generation: JobGeneration::FIRST,
            worker_id: WorkerId::try_new("wrk_a").unwrap(),
            state: LeaseState::Live,
            heartbeat_interval_ms: 1_000,
            accepted_ms: 10,
            last_heartbeat_ms: 10,
            expires_at_ms: 2_000,
            ended_ms: None,
            reason: None,
        };
        let mut other = lease.clone();
        other.lease_id = WorkerLeaseId::try_new("lease_b").unwrap();
        other.worker_id = WorkerId::try_new("wrk_b").unwrap();
        assert!(store.try_accept_lease(&lease).unwrap().is_none());
        let loser = store.try_accept_lease(&other).unwrap().unwrap();
        assert_eq!(loser.lease_id, lease.lease_id);
        assert_eq!(loser.worker_id, lease.worker_id);
    }
}
