//! The always-on storage governor configuration (`[storage]`).
//!
//! One hard aggregate budget plus optional per-class quotas. Absent class
//! quotas use the governor's documented shares. The section is always
//! honored (the governor is always on); there is no "disabled" mode that
//! could silently reintroduce an unbounded class.

use std::path::Path;
use std::sync::Arc;

use faktor_storage::{
    StorageClass, StorageGovernor, StorageGovernorConfig, DEFAULT_RESERVATION_LEASE_MS,
    DEFAULT_TOTAL_BUDGET_BYTES, DEFAULT_WATERMARK_PERMILLE,
};

/// Default age after which updater staging files are removed as remnants.
pub const DEFAULT_UPDATE_REMNANT_STALE_MS: i64 = 60 * 60 * 1000;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageCfg {
    pub total_budget_bytes: u64,
    pub watermark_permille: u16,
    pub reservation_lease_ms: i64,
    pub main_cas_bytes: Option<u64>,
    pub evidence_cas_bytes: Option<u64>,
    pub compaction_archive_bytes: Option<u64>,
    pub terminal_capture_bytes: Option<u64>,
    pub browser_storage_bytes: Option<u64>,
    pub checkpoint_bytes: Option<u64>,
    pub sqlite_wal_bytes: Option<u64>,
    pub update_remnant_bytes: Option<u64>,
    pub rebuildable_cache_bytes: Option<u64>,
    /// Age bound for updater staging remnants. Fresh (in-flight) files are
    /// preserved.
    pub update_remnant_stale_ms: i64,
}

impl Default for StorageCfg {
    fn default() -> Self {
        Self {
            total_budget_bytes: DEFAULT_TOTAL_BUDGET_BYTES,
            watermark_permille: DEFAULT_WATERMARK_PERMILLE,
            reservation_lease_ms: DEFAULT_RESERVATION_LEASE_MS,
            main_cas_bytes: None,
            evidence_cas_bytes: None,
            compaction_archive_bytes: None,
            terminal_capture_bytes: None,
            browser_storage_bytes: None,
            checkpoint_bytes: None,
            sqlite_wal_bytes: None,
            update_remnant_bytes: None,
            rebuildable_cache_bytes: None,
            update_remnant_stale_ms: DEFAULT_UPDATE_REMNANT_STALE_MS,
        }
    }
}

impl StorageCfg {
    fn overrides(&self) -> Vec<(StorageClass, u64)> {
        [
            (StorageClass::MainCas, self.main_cas_bytes),
            (StorageClass::EvidenceCas, self.evidence_cas_bytes),
            (
                StorageClass::CompactionArchive,
                self.compaction_archive_bytes,
            ),
            (StorageClass::TerminalCapture, self.terminal_capture_bytes),
            (StorageClass::BrowserStorage, self.browser_storage_bytes),
            (StorageClass::Checkpoint, self.checkpoint_bytes),
            (StorageClass::SqliteWal, self.sqlite_wal_bytes),
            (StorageClass::UpdateRemnant, self.update_remnant_bytes),
            (StorageClass::RebuildableCache, self.rebuildable_cache_bytes),
        ]
        .into_iter()
        .filter_map(|(class, bytes)| bytes.map(|bytes| (class, bytes)))
        .collect()
    }

    pub fn governor_config(&self) -> Result<StorageGovernorConfig, String> {
        let mut config = StorageGovernorConfig {
            total_budget_bytes: self.total_budget_bytes,
            watermark_permille: self.watermark_permille,
            reservation_lease_ms: self.reservation_lease_ms,
            quotas: std::collections::BTreeMap::new(),
        };
        for (class, bytes) in self.overrides() {
            config.quotas.insert(class, bytes);
        }
        config.validated()
    }

    pub fn validate(&self) -> Result<(), String> {
        self.governor_config()?;
        if self.update_remnant_stale_ms <= 0
            || self.update_remnant_stale_ms > 30 * 24 * 60 * 60 * 1000
        {
            return Err("storage.update_remnant_stale_ms must be 1..=2592000000 (30 days)".into());
        }
        Ok(())
    }

    /// Open (creating) the always-on governor for this data directory and
    /// reconcile/evict once against disk truth.
    pub fn open_governor(&self, data_dir: &Path) -> Result<Arc<StorageGovernor>, String> {
        let governor = StorageGovernor::open(data_dir, self.governor_config()?)
            .map_err(|e| format!("storage governor: {e}"))?;
        governor
            .reconcile()
            .map_err(|e| format!("storage governor reconcile: {e}"))?;
        let _ = governor.enforce();
        let _ = governor.cleanup_update_remnants(self.update_remnant_stale_ms);
        Ok(Arc::new(governor))
    }

    /// Open the EXISTING governor read-only (doctor). Never creates state.
    pub fn existing_governor(data_dir: &Path) -> Option<Arc<StorageGovernor>> {
        StorageGovernor::open_existing(data_dir).map(Arc::new)
    }
}
