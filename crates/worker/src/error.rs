//! The typed worker-plane refusal surface.
//!
//! Every refusal is a distinct variant: a stale-generation result is
//! [`WorkerError::SupersededLease`] (a structural refusal that is journaled,
//! never landed), a cross-organization lease is
//! [`WorkerError::ForeignTrustDomain`], and a protocol mismatch is
//! [`WorkerError::ProtocolSkew`] naming both versions.

use crate::ids::{JobGeneration, WorkerId, WorkerLeaseId};

/// The protocol version this runtime speaks. A worker presenting any other
/// value is refused typed and never registered, leashed or heard.
pub const WORKER_PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkerError {
    #[error("worker-plane store backend unavailable: {0}")]
    Backend(String),
    #[error("worker-plane store refused a malformed row: {0}")]
    Malformed(String),
    #[error("worker token is unknown")]
    UnknownToken,
    #[error("worker token was revoked")]
    TokenRevoked,
    #[error("worker token was already consumed by worker {0}")]
    TokenAlreadyUsed(String),
    #[error("worker token belongs to organization {owner}, not {requested}")]
    TokenOrgMismatch { owner: String, requested: String },
    #[error("worker {0} is unknown")]
    UnknownWorker(WorkerId),
    #[error("worker {0} was revoked")]
    WorkerRevoked(WorkerId),
    #[error("worker {0} is already revoked")]
    AlreadyRevoked(WorkerId),
    #[error("worker {worker} is already bound to registration token {token_hash}")]
    WorkerAlreadyBound {
        worker: WorkerId,
        token_hash: String,
    },
    #[error("worker {worker} already holds the bounded {limit} live lease(s)")]
    WorkerSaturated { worker: WorkerId, limit: usize },
    #[error("worker {0} is gone: its registration row vanished mid-operation")]
    WorkerVanished(WorkerId),
    #[error("protocol skew: this runtime speaks worker protocol v{expected}, the worker presented v{got}")]
    ProtocolSkew { expected: u32, got: u32 },
    #[error(
        "trust domain {requested} does not belong to this worker (its trust domain is {actual})"
    )]
    ForeignTrustDomain { requested: String, actual: String },
    #[error(
        "job {job} generation {generation} is superseded: the current generation is {current}"
    )]
    SupersededLease {
        job: String,
        generation: JobGeneration,
        current: JobGeneration,
    },
    #[error(
        "job {job} generation {generation} is already leased by worker {worker} (lease {lease})"
    )]
    LeaseAlreadyTaken {
        job: String,
        generation: JobGeneration,
        worker: String,
        lease: String,
    },
    #[error("lease {lease} was not found for worker {worker}")]
    UnknownLease {
        lease: WorkerLeaseId,
        worker: WorkerId,
    },
    #[error("lease {lease} is expired (it had to be renewed before {expired_at_ms} ms)")]
    LeaseExpired {
        lease: WorkerLeaseId,
        expired_at_ms: i64,
    },
    #[error("lease {lease} is no longer live ({state})")]
    LeaseNotLive { lease: WorkerLeaseId, state: String },
    #[error("job {job} generation {generation} is not assigned to worker {worker} (it is assigned to {assigned})")]
    NotAssignedToWorker {
        job: String,
        generation: JobGeneration,
        worker: WorkerId,
        assigned: String,
    },
    #[error("job {job} generation {generation} is not in a leasable state ({state})")]
    NotLeasable {
        job: String,
        generation: JobGeneration,
        state: String,
    },
    #[error("no eligible worker for job {job}: {reason}")]
    NoEligibleWorker { job: String, reason: String },
    #[error("worker {worker} cannot lease job {job}: capability mismatch ({detail})")]
    CapabilityMismatch {
        worker: WorkerId,
        job: String,
        detail: String,
    },
    #[error("attempts exhausted for job {job}: {attempts} attempt(s) of the bounded policy {max}")]
    AttemptsExhausted {
        job: String,
        attempts: u32,
        max: u32,
    },
    #[error(
        "result for job {job} generation {generation} was already accepted by worker {worker}"
    )]
    AlreadyAccepted {
        job: String,
        generation: JobGeneration,
        worker: String,
    },
    #[error("conflicting result for job {job} generation {generation} was already landed")]
    ResultConflict {
        job: String,
        generation: JobGeneration,
    },
    #[error("result digest {presented} does not match the scheduled job digest {expected}")]
    DigestMismatch { expected: String, presented: String },
    #[error("job {0} is unknown")]
    UnknownJob(String),
    #[error("job generation {generation} of job {job} is unknown")]
    UnknownGeneration {
        job: String,
        generation: JobGeneration,
    },
    #[error("the job {job} is terminal ({state}); nothing is leased")]
    JobTerminal { job: String, state: String },
}

impl From<WorkerError> for faktor_cloud::ControlPlaneError {
    fn from(e: WorkerError) -> Self {
        match e {
            WorkerError::Backend(m) => faktor_cloud::ControlPlaneError::Backend(m),
            WorkerError::Malformed(m) => faktor_cloud::ControlPlaneError::Malformed(m),
            WorkerError::UnknownToken
            | WorkerError::UnknownWorker(_)
            | WorkerError::UnknownJob(_)
            | WorkerError::UnknownGeneration { .. }
            | WorkerError::UnknownLease { .. } => {
                faktor_cloud::ControlPlaneError::NotFound(e.to_string())
            }
            WorkerError::TokenRevoked
            | WorkerError::TokenAlreadyUsed(_)
            | WorkerError::WorkerRevoked(_)
            | WorkerError::AlreadyRevoked(_) => {
                faktor_cloud::ControlPlaneError::Unauthorized(e.to_string())
            }
            _ => faktor_cloud::ControlPlaneError::Conflict(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_version_is_one() {
        assert_eq!(WORKER_PROTOCOL_VERSION, 1);
    }

    #[test]
    fn superseded_lease_names_both_generations() {
        let e = WorkerError::SupersededLease {
            job: "job_1".into(),
            generation: JobGeneration(1),
            current: JobGeneration(2),
        };
        let text = e.to_string();
        assert!(text.contains("generation 1"), "{text}");
        assert!(text.contains("current generation is 2"), "{text}");
    }
}
