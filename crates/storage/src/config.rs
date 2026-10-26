//! Budget and quota configuration.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::class::StorageClass;

/// Default aggregate budget: 32 GiB. Large enough for a workstation install,
/// small enough that a never-evicted class can never fill a disk.
pub const DEFAULT_TOTAL_BUDGET_BYTES: u64 = 32 * 1024 * 1024 * 1024;
/// Default admission watermark (90%): above it the governor evicts before
/// admitting new bytes.
pub const DEFAULT_WATERMARK_PERMILLE: u16 = 900;
/// A reservation that is neither committed nor aborted within this window is
/// an orphan of a crashed publish and is expired by reconciliation.
pub const DEFAULT_RESERVATION_LEASE_MS: i64 = 15 * 60 * 1000;

/// The one hard aggregate budget plus per-class quotas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageGovernorConfig {
    pub total_budget_bytes: u64,
    pub watermark_permille: u16,
    pub reservation_lease_ms: i64,
    pub quotas: BTreeMap<StorageClass, u64>,
}

impl Default for StorageGovernorConfig {
    fn default() -> Self {
        let total = DEFAULT_TOTAL_BUDGET_BYTES;
        let quotas = StorageClass::ALL
            .into_iter()
            .map(|class| {
                (
                    class,
                    total.saturating_mul(class.default_share_permille()) / 1000,
                )
            })
            .collect();
        Self {
            total_budget_bytes: total,
            watermark_permille: DEFAULT_WATERMARK_PERMILLE,
            reservation_lease_ms: DEFAULT_RESERVATION_LEASE_MS,
            quotas,
        }
    }
}

impl StorageGovernorConfig {
    pub fn validated(mut self) -> Result<Self, String> {
        if self.total_budget_bytes == 0 {
            return Err("storage.total_budget_bytes must be > 0".into());
        }
        if self.watermark_permille == 0 || self.watermark_permille > 1000 {
            return Err("storage.watermark_permille must be 1..=1000".into());
        }
        if self.reservation_lease_ms <= 0 {
            return Err("storage.reservation_lease_ms must be > 0".into());
        }
        for class in StorageClass::ALL {
            let quota = self
                .quotas
                .get(&class)
                .copied()
                .unwrap_or_else(|| self.default_quota(class));
            if quota == 0 {
                return Err(format!(
                    "storage quota for {} must be > 0 (a zero quota refuses every write)",
                    class.as_str()
                ));
            }
            if quota > self.total_budget_bytes {
                return Err(format!(
                    "storage quota for {} ({quota}) exceeds the aggregate budget ({})",
                    class.as_str(),
                    self.total_budget_bytes
                ));
            }
            self.quotas.insert(class, quota);
        }
        Ok(self)
    }

    pub fn quota(&self, class: StorageClass) -> u64 {
        self.quotas
            .get(&class)
            .copied()
            .unwrap_or_else(|| self.default_quota(class))
    }

    fn default_quota(&self, class: StorageClass) -> u64 {
        self.total_budget_bytes
            .saturating_mul(class.default_share_permille())
            / 1000
    }

    /// A budget/cli-level constructor with optional per-class overrides.
    pub fn with_quotas(
        total_budget_bytes: u64,
        watermark_permille: u16,
        overrides: impl IntoIterator<Item = (StorageClass, u64)>,
    ) -> Result<Self, String> {
        let mut config = Self {
            total_budget_bytes,
            watermark_permille,
            reservation_lease_ms: DEFAULT_RESERVATION_LEASE_MS,
            quotas: BTreeMap::new(),
        };
        for (class, bytes) in overrides {
            config.quotas.insert(class, bytes);
        }
        config.validated()
    }
}
