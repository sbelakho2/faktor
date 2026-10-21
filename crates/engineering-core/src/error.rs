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
    #[error("receipt carries no Ed25519 attestation")]
    MissingReceiptAttestation,
    #[error("receipt attestation is invalid: {reason}")]
    InvalidReceiptAttestation { reason: String },
    #[error("receipt signature does not verify against the canonical digest")]
    ReceiptSignatureMismatch,
    #[error("receipt was signed by untrusted key {key_id}")]
    UntrustedReceiptKey { key_id: String },
    #[error(
        "receipt signed at {signed_at_ms} is outside key {key_id}'s validity \
         (not before {not_before_ms}, retired at {retired_at_ms:?})"
    )]
    ReceiptKeyOutsideValidity {
        key_id: String,
        signed_at_ms: u64,
        not_before_ms: u64,
        retired_at_ms: Option<u64>,
    },
    #[error("signing key store {path}: {reason}")]
    KeyStore { path: String, reason: String },
    #[error("signing key store {path} refused: {reason}")]
    KeyStoreRefused { path: String, reason: String },
    #[error(
        "signing key store ownership/permission checks are unavailable on this \
         platform ({platform}); refusing to trust the key"
    )]
    KeyStorePlatformUnsupported { platform: &'static str },
    #[error("signing key {key_id} was retired at {retired_at_ms} and cannot sign new receipts")]
    RetiredSigningKey { key_id: String, retired_at_ms: u64 },
    #[error("signing key {key_id} is not valid before {not_before_ms}")]
    SigningKeyNotYetValid { key_id: String, not_before_ms: u64 },
    #[error("applicability selection is invalid: {reason}")]
    InvalidSelection { reason: String },
    #[error("gate contract violation in {gate}: {message}")]
    GateViolation {
        gate: crate::gate::GateId,
        message: String,
    },
    /// A human approval is required for this consequential change.
    #[error(transparent)]
    Approval(#[from] faktor_approval::ApprovalError),
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
