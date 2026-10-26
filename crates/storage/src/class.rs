//! The storage classes one process-wide budget governs.

use serde::{Deserialize, Serialize};

/// One governed storage class. The class is a logical label over physical
/// bytes: the main CAS root carries the `MainCas`, `CompactionArchive`,
/// `TerminalCapture` and `Checkpoint` classes (a blob belongs to exactly ONE
/// of them through the durable object index), while the evidence CAS, browser
/// profiles, SQLite databases, updater remnants and rebuildable caches have
/// their own roots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageClass {
    /// Tool artifacts and general content-addressed blobs.
    MainCas,
    /// The evidence authority's backing CAS.
    EvidenceCas,
    /// Compaction archive chunks and their JSON manifests (never evicted
    /// silently: eviction leaves a durable loss marker).
    CompactionArchive,
    /// Captured terminal output artifacts.
    TerminalCapture,
    /// Browser profiles, scratch and captured downloads.
    BrowserStorage,
    /// Snapshot/checkpoint blobs referenced by checkpoint rows.
    Checkpoint,
    /// SQLite databases, WAL and SHM sidecars.
    SqliteWal,
    /// Updater staging/download remnants.
    UpdateRemnant,
    /// Rebuildable index/search/embedding caches.
    RebuildableCache,
}

impl StorageClass {
    pub const ALL: [StorageClass; 9] = [
        StorageClass::MainCas,
        StorageClass::EvidenceCas,
        StorageClass::CompactionArchive,
        StorageClass::TerminalCapture,
        StorageClass::BrowserStorage,
        StorageClass::Checkpoint,
        StorageClass::SqliteWal,
        StorageClass::UpdateRemnant,
        StorageClass::RebuildableCache,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            StorageClass::MainCas => "main_cas",
            StorageClass::EvidenceCas => "evidence_cas",
            StorageClass::CompactionArchive => "compaction_archive",
            StorageClass::TerminalCapture => "terminal_capture",
            StorageClass::BrowserStorage => "browser_storage",
            StorageClass::Checkpoint => "checkpoint",
            StorageClass::SqliteWal => "sqlite_wal",
            StorageClass::UpdateRemnant => "update_remnant",
            StorageClass::RebuildableCache => "rebuildable_cache",
        }
    }

    pub fn from_label(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|class| class.as_str() == raw)
    }

    /// Default share of the aggregate budget, in permille (sums to 1000).
    pub const fn default_share_permille(self) -> u64 {
        match self {
            StorageClass::MainCas => 350,
            StorageClass::EvidenceCas => 150,
            StorageClass::CompactionArchive => 100,
            StorageClass::TerminalCapture => 100,
            StorageClass::BrowserStorage => 50,
            StorageClass::Checkpoint => 100,
            StorageClass::SqliteWal => 100,
            StorageClass::UpdateRemnant => 20,
            StorageClass::RebuildableCache => 30,
        }
    }

    /// Physical root family. Logical classes that share one CAS root
    /// (`main_cas`, `compaction_archive`, `terminal_capture`, `checkpoint`)
    /// share a digest namespace; the evidence CAS is a separate physical
    /// root, so identical content is two distinct governed blobs.
    pub const fn family(self) -> &'static str {
        match self {
            StorageClass::EvidenceCas => "evidence",
            _ => "main",
        }
    }

    /// CAS-backed classes are indexed by digest and must verify integrity
    /// before their blob is deleted.
    pub const fn is_cas(self) -> bool {
        matches!(
            self,
            StorageClass::MainCas
                | StorageClass::EvidenceCas
                | StorageClass::CompactionArchive
                | StorageClass::TerminalCapture
                | StorageClass::Checkpoint
        )
    }

    /// Archives may be evicted, but never silently: every eviction leaves a
    /// durable loss marker naming the object, its digest and its bytes.
    pub const fn requires_loss_marker(self) -> bool {
        matches!(self, StorageClass::CompactionArchive)
    }
}
