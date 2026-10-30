// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! Always-on storage governance for every durable class Faktor owns.
//!
//! One hard aggregate budget plus per-class quotas; admission is a durable
//! reservation written BEFORE bytes are published; eviction is by class
//! (LRU/age), never touches active/pinned evidence, and verifies CAS
//! integrity before deleting a blob. Compaction archives additionally leave
//! a durable loss marker. SQLite WALs are checkpoint-truncated and growth is
//! refused before an append that would breach the budget. When the budget
//! cannot be met the health is typed degraded — never a silent success.

pub mod capacity;
pub mod class;
pub mod config;
pub mod directory;
pub mod error;
pub mod governor;
pub mod health;
pub mod ledger;
pub mod object;

#[cfg(test)]
#[path = "band_tests.rs"]
mod band_tests;
#[cfg(test)]
#[path = "history_tests.rs"]
mod history_tests;
#[cfg(test)]
mod tests;

pub use capacity::{
    CapacityProbe, CapacitySnapshot, FilesystemCapacity, FilesystemSpace, SystemCapacityProbe,
};
pub use class::StorageClass;
pub use config::{
    StorageGovernorConfig, DEFAULT_CAPACITY_PERMILLE, DEFAULT_EMERGENCY_CEILING_PERMILLE,
    DEFAULT_MIN_INSTALL_ALLOWANCE_BYTES, DEFAULT_NORMAL_ADMISSION_CEILING_PERMILLE,
    DEFAULT_RESERVATION_LEASE_MS, DEFAULT_RESERVE_BYTES, DEFAULT_RESERVE_PERMILLE,
    DEFAULT_SHADOW_MAX_BYTES_PER_SHADOW, DEFAULT_TOTAL_BUDGET_BYTES, DEFAULT_WATERMARK_PERMILLE,
    MAX_CAPACITY_PERMILLE, MAX_INSTALL_ALLOWANCE_BYTES, MAX_RESERVE_BYTES, MAX_RESERVE_PERMILLE,
    MAX_SHADOW_MAX_BYTES_PER_SHADOW, MIN_CAPACITY_PERMILLE, MIN_INSTALL_ALLOWANCE_BYTES,
    MIN_RESERVE_BYTES, MIN_RESERVE_PERMILLE, MIN_SHADOW_MAX_BYTES_PER_SHADOW,
};
pub use directory::{DirectoryBackend, SqliteBackend};
pub use error::{GovernorError, GovernorResult, RefusalReason, StorageRefusal};
pub use governor::{
    AdmissionPurpose, GovernorRoots, ReconcileReport, Reservation, ShadowGrowth, SqliteAdmission,
    StorageGovernor, GOVERNOR_DB_NAME, GOVERNOR_DIR_NAME,
};
pub use health::{ClassUsage, DegradedHealth, StorageMetrics};
pub use ledger::{
    HistoryAggregateRow, HistoryPruneReport, HISTORY_AGGREGATE_BUCKET_MS, HISTORY_DETAIL_KEEP,
};
pub use object::{EvictionOutcome, StorageBackend, StorageObject};
