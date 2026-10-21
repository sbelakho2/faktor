use thiserror::Error;

use crate::digest::Digest;
use crate::graph::{EdgeKind, NodeId, NodeKind};
use crate::value::RevisionId;

#[derive(Debug, Clone, PartialEq, Error)]
pub enum CoreError {
    #[error("digest must be 64 lowercase/uppercase hex characters, got {value:?}")]
    InvalidDigest { value: String },
    #[error("field {field} must not be empty")]
    EmptyField { field: &'static str },
    #[error("field {field} exceeds {max} bytes")]
    FieldTooLong { field: &'static str, max: usize },
    #[error("revision mismatch: expected {expected}, found {found}")]
    RevisionMismatch {
        expected: RevisionId,
        found: RevisionId,
    },
    #[error("node {0} does not exist in this revision")]
    NodeNotFound(NodeId),
    #[error("edge endpoint mismatch: {edge:?} cannot connect {from:?} -> {to:?}")]
    EdgeKindNotAllowed {
        edge: EdgeKind,
        from: NodeKind,
        to: NodeKind,
    },
    #[error("graph limit exceeded: {what} (limit {limit})")]
    GraphLimit { what: &'static str, limit: usize },
    #[error("dependency cycle detected in the engineering design graph")]
    Cycle,
    #[error("decision node {0} has no EvidencedBy edge")]
    DecisionWithoutEvidence(NodeId),
    #[error("invalid engineering value: {reason}")]
    InvalidValue { reason: String },
    #[error("invalid receipt: {reason}")]
    InvalidReceipt { reason: String },
    #[error("serialization failed: {reason}")]
    Serialization { reason: String },
    #[error("units error: {0}")]
    Units(#[from] engineering_units::UnitError),
    #[error("digest {0} is not the canonical digest of the recorded results")]
    ResultDigestMismatch(Digest),
    #[error("gate contract violation in {gate}: {message}")]
    GateViolation {
        gate: crate::gate::GateId,
        message: String,
    },
}

pub type CoreResult<T> = Result<T, CoreError>;

pub(crate) fn require_text(field: &'static str, value: &str, max: usize) -> CoreResult<()> {
    if value.trim().is_empty() {
        return Err(CoreError::EmptyField { field });
    }
    if value.len() > max {
        return Err(CoreError::FieldTooLong { field, max });
    }
    Ok(())
}
