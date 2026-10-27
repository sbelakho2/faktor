//! Objects and the backend contract the governor evicts through.

use std::path::Path;

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
    /// The object still exists but SHRANK (a WAL checkpoint truncation).
    /// `remaining_bytes` stays indexed and counted; deleting the row would
    /// fabricate room for bytes that are still on disk.
    Reclaimed {
        bytes: u64,
        remaining_bytes: u64,
        verified: bool,
    },
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

    /// The CANONICAL key this backend assigns to a file at `root/rel`, or
    /// `None` when `root` is not one of this backend's registered roots.
    ///
    /// Writers that publish into a governed directory MUST reserve and
    /// commit under this key: a multi-root backend prefixes keys with
    /// `<root-index>:`, so a bare root-relative path names a *different*
    /// object than the one `scan` later reports and the durable index row is
    /// orphaned (unevictable bytes).
    fn canonical_directory_key(&self, _root: &Path, _rel: &Path) -> Option<String> {
        None
    }

    /// Verify (CAS classes) and delete one object. Must never delete bytes it
    /// could not verify, and must return `Absent` for an idempotent replay.
    fn evict(
        &self,
        governor: &StorageGovernor,
        object: &StorageObject,
    ) -> GovernorResult<EvictionOutcome>;

    /// Physical roots of a directory backend, exposed so a second
    /// registration of the same class MERGES roots instead of silently
    /// dropping the first root from governance (default: not a directory
    /// backend).
    fn directory_roots(&self) -> Option<(&[std::path::PathBuf], i64)> {
        None
    }
}
