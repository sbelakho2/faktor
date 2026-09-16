//! The additive commercial admission-gate hook (Wave 3 entitlements).
//!
//! The runtime consults ONE optional [`AdmissionGate`] at exactly three SAFE
//! BOUNDARIES — a new task admission, a new child spawn, and a new provider
//! attempt BEFORE dispatch — and nowhere else. Integration, rollback and
//! completion continuations are never gated: an entitlement change (an
//! expired subscription, an exhausted quota) therefore takes effect only on
//! the NEXT admission and can never interrupt an in-flight transaction.
//!
//! The hook is provider-neutral and billing-neutral: the gate receives a
//! bounded [`AdmissionRequest`] (boundary, run identity, observed counters)
//! and answers `Ok(())` or a typed [`AdmissionRefusal`] naming the exact
//! limit. Without a wired gate the runtime behaves byte-identically to the
//! pre-billing daemon (`None` = admit everything).

use serde::{Deserialize, Serialize};

/// One GATED boundary. Only these three exist: a continuation boundary is
/// not representable here, which is what makes "never interrupt an in-flight
/// transaction" structural rather than a policy flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionBoundary {
    NewTask,
    NewChildSpawn,
    NewProviderAttempt,
}

impl AdmissionBoundary {
    pub const fn as_str(self) -> &'static str {
        match self {
            AdmissionBoundary::NewTask => "new_task",
            AdmissionBoundary::NewChildSpawn => "new_child_spawn",
            AdmissionBoundary::NewProviderAttempt => "new_provider_attempt",
        }
    }
}

impl std::fmt::Display for AdmissionBoundary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The observed counters of one boundary (bounded; the gate never scans
/// durable state itself through this hook).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionObserved {
    /// Live (non-terminal) tasks of the owning session.
    pub active_tasks: u64,
    /// Live (non-terminal) children of the run so far.
    pub children_of_task: u64,
    /// Provider attempts (drive ops) already dispatched for this run.
    pub provider_attempts_of_task: u64,
    /// The estimated managed cost of the pending attempt, when known.
    pub estimated_provider_cost_micro: u64,
}

/// One bounded admission question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmissionRequest {
    pub boundary: AdmissionBoundary,
    /// The parent/owner session of the run.
    pub parent_session: u64,
    pub task_id: Option<u64>,
    pub run_id: String,
    /// The provider of a `NewProviderAttempt` boundary, when known.
    pub provider: Option<String>,
    pub model: Option<String>,
    pub observed: AdmissionObserved,
}

/// The typed refusal of one gate consultation: it ALWAYS names the exact
/// limit (or state cause) that refused, plus the boundary it refused at.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("admission refused at {boundary} (limit {limit}): {message}")]
pub struct AdmissionRefusal {
    pub boundary: AdmissionBoundary,
    pub limit: String,
    pub message: String,
}

impl AdmissionRefusal {
    pub fn new(
        boundary: AdmissionBoundary,
        limit: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            boundary,
            limit: limit.into(),
            message: message.into(),
        }
    }
}

/// The ONE admission authority the runtime consults. Implementations must be
/// thread-safe and must not block on unbounded work: the hook runs on the
/// executor's adoption path, before any durable row of the admitted unit.
pub trait AdmissionGate: Send + Sync {
    fn check(&self, request: &AdmissionRequest) -> Result<(), AdmissionRefusal>;
}

/// A gate that admits everything (the explicit pre-billing parity value;
/// the runtime's `None` gate is equivalent and is the default).
#[derive(Debug, Default)]
pub struct AllowAllAdmission;

impl AdmissionGate for AllowAllAdmission {
    fn check(&self, _request: &AdmissionRequest) -> Result<(), AdmissionRefusal> {
        Ok(())
    }
}
