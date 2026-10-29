//! Typed governor failures and refusals.

use serde::{Deserialize, Serialize};

use crate::class::StorageClass;

/// Why one admission was refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalReason {
    /// The object alone is larger than its class quota.
    ClassQuota,
    /// The object alone is larger than the aggregate budget.
    AggregateBudget,
    /// Eviction could not free enough bytes inside the class.
    NoEvictableObjects,
    /// Pinned/required bytes leave no admissible room.
    RequiredBytes,
    /// The class has no registered eviction backend.
    NoBackend,
    /// Admitting the bytes would consume the always-preserved free-space
    /// reserve (measured free space, not only the nominal quota).
    FreeSpaceReserve,
    /// No backing volume (or no measurable destination volume) could be
    /// probed: free space and the reserve cannot be verified, so new
    /// material storage is refused until capacity measurement succeeds and
    /// health is degraded `capacity_unmeasured`.
    CapacityUnmeasured,
    /// A backend refused the eviction (integrity failure, active file, ...).
    BackendRefused,
    /// The request itself is malformed (zero length, invalid key, ...).
    InvalidRequest,
}

impl RefusalReason {
    pub const fn as_str(&self) -> &'static str {
        match self {
            RefusalReason::ClassQuota => "class_quota",
            RefusalReason::AggregateBudget => "aggregate_budget",
            RefusalReason::NoEvictableObjects => "no_evictable_objects",
            RefusalReason::RequiredBytes => "required_bytes",
            RefusalReason::NoBackend => "no_backend",
            RefusalReason::FreeSpaceReserve => "free_space_reserve",
            RefusalReason::CapacityUnmeasured => "capacity_unmeasured",
            RefusalReason::BackendRefused => "backend_refused",
            RefusalReason::InvalidRequest => "invalid_request",
        }
    }
}

/// One typed admission refusal. `available_bytes` is the free room after the
/// governor's best eviction attempt; a refusal is recorded durably.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageRefusal {
    pub class: StorageClass,
    pub requested_bytes: u64,
    pub available_bytes: u64,
    pub reason: RefusalReason,
    pub detail: String,
}

impl std::fmt::Display for StorageRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "storage admission refused for {}: requested {} bytes, {} bytes available ({}, {})",
            self.class.as_str(),
            self.requested_bytes,
            self.available_bytes,
            self.reason.as_str(),
            self.detail
        )
    }
}

#[derive(Debug, thiserror::Error)]
pub enum GovernorError {
    #[error("storage governor io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("storage governor sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("storage admission refused: {0}")]
    Refused(StorageRefusal),
    #[error("storage governor unavailable: {0}")]
    Unavailable(String),
    #[error("storage governor state is corrupt: {0}")]
    Corrupt(String),
    #[error("storage governor request malformed: {0}")]
    Malformed(String),
    #[error("storage backend failed: {0}")]
    Backend(String),
}

impl GovernorError {
    pub fn is_refusal(&self) -> bool {
        matches!(self, GovernorError::Refused(_))
    }

    pub fn refusal(&self) -> Option<&StorageRefusal> {
        match self {
            GovernorError::Refused(refusal) => Some(refusal),
            _ => None,
        }
    }
}

pub type GovernorResult<T> = Result<T, GovernorError>;
