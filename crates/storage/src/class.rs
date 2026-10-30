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
    /// Daemon-owned shadow mutation workspaces
    /// (`<data-dir>/shadows/<session>/<shadow-id>`): bounded base copies plus
    /// every PHYSICAL byte a shadowed run produces inside them (build
    /// outputs such as `target/`/`node_modules` are excluded from source
    /// manifests but always counted here). Never LRU-evicted — an active
    /// shadow is a live workspace, so class pressure is a typed refusal, not
    /// silent data destruction.
    ShadowWorkspace,
}

impl StorageClass {
    pub const ALL: [StorageClass; 10] = [
        StorageClass::MainCas,
        StorageClass::EvidenceCas,
        StorageClass::CompactionArchive,
        StorageClass::TerminalCapture,
        StorageClass::BrowserStorage,
        StorageClass::Checkpoint,
        StorageClass::SqliteWal,
        StorageClass::UpdateRemnant,
        StorageClass::RebuildableCache,
        StorageClass::ShadowWorkspace,
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
            StorageClass::ShadowWorkspace => "shadow_workspace",
        }
    }

    pub fn from_label(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|class| class.as_str() == raw)
    }

    /// Default share of the aggregate budget, in permille (sums to 1000).
    /// `ShadowWorkspace` takes 80‰ from the main CAS share: a shadow is a
    /// bounded copy of one checkout (capped by the per-shadow ceiling) plus
    /// its build outputs, and it must never crowd out durable evidence,
    /// archives or checkpoints.
    pub const fn default_share_permille(self) -> u64 {
        match self {
            StorageClass::MainCas => 270,
            StorageClass::EvidenceCas => 150,
            StorageClass::CompactionArchive => 100,
            StorageClass::TerminalCapture => 100,
            StorageClass::BrowserStorage => 50,
            StorageClass::Checkpoint => 100,
            StorageClass::SqliteWal => 100,
            StorageClass::UpdateRemnant => 20,
            StorageClass::RebuildableCache => 30,
            StorageClass::ShadowWorkspace => 80,
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

    /// Whether LRU eviction may ever delete bytes of this class to make
    /// budget room. Shadow workspaces are LIVE run workspaces: deleting
    /// files from under an in-flight drive would silently corrupt the run,
    /// so shadow class pressure resolves to a typed refusal (and a degraded
    /// health naming the class) — never silent destruction.
    pub const fn is_evictable(self) -> bool {
        !matches!(self, StorageClass::ShadowWorkspace)
    }

    /// Retention strength of one logical class sharing a physical CAS root.
    /// A deduplicated blob is owned by the STRONGEST class that ever
    /// referenced it, so an archive/checkpoint/capture reference cannot lose
    /// its class (and its loss marker) merely because identical bytes were
    /// first stored as general main-CAS content.
    pub const fn retention_rank(self) -> u8 {
        match self {
            StorageClass::CompactionArchive => 3,
            StorageClass::Checkpoint => 2,
            StorageClass::TerminalCapture => 1,
            StorageClass::MainCas => 0,
            _ => 0,
        }
    }
}
