//! Always-on storage governance for every durable class Faktor owns.
//!
//! One hard aggregate budget plus per-class quotas; admission is a durable
//! reservation written BEFORE bytes are published; eviction is by class
//! (LRU/age), never touches active/pinned evidence, and verifies CAS
//! integrity before deleting a blob. Compaction archives additionally leave
//! a durable loss marker. SQLite WALs are checkpoint-truncated and growth is
//! refused before an append that would breach the budget. When the budget
//! cannot be met the health is typed degraded — never a silent success.

pub mod class;
pub mod config;
pub mod directory;
pub mod error;
pub mod governor;
pub mod health;
pub mod ledger;
pub mod object;

#[cfg(test)]
mod tests;

pub use class::StorageClass;
pub use config::{
    StorageGovernorConfig, DEFAULT_RESERVATION_LEASE_MS, DEFAULT_TOTAL_BUDGET_BYTES,
    DEFAULT_WATERMARK_PERMILLE,
};
pub use directory::{DirectoryBackend, SqliteBackend};
pub use error::{GovernorError, GovernorResult, RefusalReason, StorageRefusal};
pub use governor::{
    GovernorRoots, ReconcileReport, Reservation, StorageGovernor, GOVERNOR_DB_NAME,
    GOVERNOR_DIR_NAME,
};
pub use health::{ClassUsage, DegradedHealth, StorageMetrics};
pub use object::{EvictionOutcome, StorageBackend, StorageObject};
