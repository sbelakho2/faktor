//! Objects and the backend contract the governor evicts through.

use crate::class::StorageClass;
use crate::error::GovernorResult;
use crate::governor::StorageGovernor;

/// One evictable object as a backend reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageObject {
    /// Stable key inside the class (CAS: the 64-hex digest; directories: the
    /// `/`-joined relative path).
    pub key: String,
    pub bytes: u64,
    /// Last-use/admission time in Unix milliseconds; the LRU/age key.
    pub last_used_ms: i64,
    /// Set when the object has a content digest (CAS classes).
    pub digest: Option<String>,
    /// Pinned or active objects are never candidates.
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvictionOutcome {
    Evicted {
        bytes: u64,
        /// The backend proved integrity before deleting (CAS classes).
        verified: bool,
        digest: Option<String>,
    },
    /// The object was already gone (idempotent replay).
    Absent,
    /// The backend refused; the object stays and the governor records a
    /// refusal.
    Refused { detail: String },
}

/// One physical storage surface the governor can enumerate and evict from.
pub trait StorageBackend: Send + Sync {
    fn class(&self) -> StorageClass;

    /// Enumerate the objects of THIS class. CAS backends filter the shared
    /// root through the durable object index so a blob belongs to exactly
    /// one class; directory backends walk their root (bounded, no symlinks).
    fn scan(&self, governor: &StorageGovernor) -> GovernorResult<Vec<StorageObject>>;

    /// Verify (CAS classes) and delete one object. Must never delete bytes it
    /// could not verify, and must return `Absent` for an idempotent replay.
    fn evict(
        &self,
        governor: &StorageGovernor,
        object: &StorageObject,
    ) -> GovernorResult<EvictionOutcome>;
}
