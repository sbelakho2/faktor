//! Budget and quota configuration.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::class::StorageClass;

/// Default operator cap: no fixed cap, so the capacity-relative budget is
/// authoritative. An operator may still set `total_budget_bytes` explicitly.
pub const DEFAULT_TOTAL_BUDGET_BYTES: u64 = u64::MAX;
/// Default admission watermark (90%): above it the governor evicts before
/// admitting new bytes.
pub const DEFAULT_WATERMARK_PERMILLE: u16 = 900;
/// A reservation that is neither committed nor aborted within this window is
/// an orphan of a crashed publish and is expired by reconciliation.
pub const DEFAULT_RESERVATION_LEASE_MS: i64 = 15 * 60 * 1000;

/// Default capacity-relative hard cap: 5% of the backing filesystem.
pub const DEFAULT_CAPACITY_PERMILLE: u16 = 50;
/// Safe clamp for the capacity-relative cap (0.1%..=50%).
pub const MIN_CAPACITY_PERMILLE: u16 = 1;
pub const MAX_CAPACITY_PERMILLE: u16 = 500;
/// Default free-space reserve: 5% of capacity (floor `DEFAULT_RESERVE_BYTES`).
pub const DEFAULT_RESERVE_PERMILLE: u16 = 50;
/// Safe clamp for the reserve share (1%..=50%).
pub const MIN_RESERVE_PERMILLE: u16 = 10;
pub const MAX_RESERVE_PERMILLE: u16 = 500;
/// Absolute floor of the always-preserved free-space reserve: 2 GiB.
pub const DEFAULT_RESERVE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const MIN_RESERVE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_RESERVE_BYTES: u64 = 256 * 1024 * 1024 * 1024;
/// A small installation still needs room for its own data even when 5% of a
/// tiny volume is less than that.
pub const DEFAULT_MIN_INSTALL_ALLOWANCE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const MIN_INSTALL_ALLOWANCE_BYTES: u64 = 32 * 1024 * 1024;
pub const MAX_INSTALL_ALLOWANCE_BYTES: u64 = 256 * 1024 * 1024 * 1024;

/// Recovery-only reserve bands over the EFFECTIVE aggregate budget.
///
/// The aggregate budget is split into three admission bands (documented
/// defaults, permille):
///
/// - **normal** `0..=850‰`: ordinary class admissions (CAS publishes,
///   caches, logs, evidence, shadow copies). Normal admission is refused
///   typed once aggregate usage (durable + pending) would cross this
///   ceiling, so it can never consume the bytes below.
/// - **emergency/critical metadata** `851‰..=900‰`: growth the daemon
///   itself must be able to record to stay recoverable (SQLite/WAL growth
///   of the governor's own and the session ledgers).
/// - **RECOVERY-ONLY reserve** `901‰..=1000‰`: rollback/finalize/repair
///   bytes only, admitted through the explicit recovery-purpose API. No
///   OS-level quota is enforced (none is portable cross-platform); the
///   reserve is an admission policy, held by refusing normal and emergency
///   admission before it and by measuring physical usage at every
///   admission.
///
/// Safe clamps: normal `500..=950‰`, emergency pulled into
/// `normal+1..=1000‰` so the bands can never invert or vanish.
pub const DEFAULT_NORMAL_ADMISSION_CEILING_PERMILLE: u16 = 850;
pub const MIN_NORMAL_ADMISSION_CEILING_PERMILLE: u16 = 500;
pub const MAX_NORMAL_ADMISSION_CEILING_PERMILLE: u16 = 950;
pub const DEFAULT_EMERGENCY_CEILING_PERMILLE: u16 = 900;
pub const MIN_EMERGENCY_CEILING_PERMILLE: u16 = 100;
pub const MAX_EMERGENCY_CEILING_PERMILLE: u16 = 1000;

/// Default per-shadow hard ceiling: one shadow directory (its base copy
/// PLUS every physical byte a run produces inside it) may never hold more
/// than this, independent of the aggregate shadow class quota. Matches the
/// shadow base-copy byte cap so a single run can never pin the whole
/// shadow-class share.
pub const DEFAULT_SHADOW_MAX_BYTES_PER_SHADOW: u64 = 1024 * 1024 * 1024;
pub const MIN_SHADOW_MAX_BYTES_PER_SHADOW: u64 = 1024 * 1024;
pub const MAX_SHADOW_MAX_BYTES_PER_SHADOW: u64 = 64 * 1024 * 1024 * 1024;

fn default_capacity_permille() -> u16 {
    DEFAULT_CAPACITY_PERMILLE
}
fn default_reserve_permille() -> u16 {
    DEFAULT_RESERVE_PERMILLE
}
fn default_reserve_bytes() -> u64 {
    DEFAULT_RESERVE_BYTES
}
fn default_min_install_allowance_bytes() -> u64 {
    DEFAULT_MIN_INSTALL_ALLOWANCE_BYTES
}
fn default_normal_admission_ceiling_permille() -> u16 {
    DEFAULT_NORMAL_ADMISSION_CEILING_PERMILLE
}
fn default_emergency_ceiling_permille() -> u16 {
    DEFAULT_EMERGENCY_CEILING_PERMILLE
}
fn default_shadow_max_bytes_per_shadow() -> u64 {
    DEFAULT_SHADOW_MAX_BYTES_PER_SHADOW
}

/// The one hard aggregate budget plus per-class quotas.
///
/// `total_budget_bytes` is the operator cap; the governor applies the smaller
/// of it and the capacity-relative cap measured at open/reserve time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageGovernorConfig {
    pub total_budget_bytes: u64,
    pub watermark_permille: u16,
    pub reservation_lease_ms: i64,
    #[serde(default = "default_capacity_permille")]
    pub capacity_permille: u16,
    #[serde(default = "default_reserve_permille")]
    pub reserve_permille: u16,
    #[serde(default = "default_reserve_bytes")]
    pub reserve_bytes: u64,
    #[serde(default = "default_min_install_allowance_bytes")]
    pub min_install_allowance_bytes: u64,
    /// Upper bound (permille of the effective budget) of the NORMAL
    /// admission band; see the recovery-band constants for the documented
    /// three-band split.
    #[serde(default = "default_normal_admission_ceiling_permille")]
    pub normal_admission_ceiling_permille: u16,
    /// Upper bound (permille) of the emergency/critical-metadata band; the
    /// remaining band above it is RECOVERY-ONLY.
    #[serde(default = "default_emergency_ceiling_permille")]
    pub emergency_ceiling_permille: u16,
    /// Hard ceiling of ONE shadow directory (base copy plus physical
    /// growth), enforced by the governor independently of the aggregate
    /// shadow class quota.
    #[serde(default = "default_shadow_max_bytes_per_shadow")]
    pub shadow_max_bytes_per_shadow: u64,
    /// Explicit per-class quotas. Classes absent here use their documented
    /// share of the EFFECTIVE (capacity-aware) budget, so defaults shrink
    /// with the disk instead of staying pinned to a fixed total.
    pub quotas: BTreeMap<StorageClass, u64>,
}

impl Default for StorageGovernorConfig {
    fn default() -> Self {
        Self {
            total_budget_bytes: DEFAULT_TOTAL_BUDGET_BYTES,
            watermark_permille: DEFAULT_WATERMARK_PERMILLE,
            reservation_lease_ms: DEFAULT_RESERVATION_LEASE_MS,
            capacity_permille: DEFAULT_CAPACITY_PERMILLE,
            reserve_permille: DEFAULT_RESERVE_PERMILLE,
            reserve_bytes: DEFAULT_RESERVE_BYTES,
            min_install_allowance_bytes: DEFAULT_MIN_INSTALL_ALLOWANCE_BYTES,
            normal_admission_ceiling_permille: DEFAULT_NORMAL_ADMISSION_CEILING_PERMILLE,
            emergency_ceiling_permille: DEFAULT_EMERGENCY_CEILING_PERMILLE,
            shadow_max_bytes_per_shadow: DEFAULT_SHADOW_MAX_BYTES_PER_SHADOW,
            quotas: BTreeMap::new(),
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
        // Safe clamps: a bad number can never disable the capacity guard or
        // the reserve; it is pulled into the documented safe range.
        self.capacity_permille = self
            .capacity_permille
            .clamp(MIN_CAPACITY_PERMILLE, MAX_CAPACITY_PERMILLE);
        self.reserve_permille = self
            .reserve_permille
            .clamp(MIN_RESERVE_PERMILLE, MAX_RESERVE_PERMILLE);
        self.reserve_bytes = self
            .reserve_bytes
            .clamp(MIN_RESERVE_BYTES, MAX_RESERVE_BYTES);
        self.min_install_allowance_bytes = self
            .min_install_allowance_bytes
            .clamp(MIN_INSTALL_ALLOWANCE_BYTES, MAX_INSTALL_ALLOWANCE_BYTES);
        // Band clamps: a bad number can never invert the three-band split or
        // zero the recovery reserve; both bands are pulled into the
        // documented safe range with emergency strictly above normal.
        self.normal_admission_ceiling_permille = self.normal_admission_ceiling_permille.clamp(
            MIN_NORMAL_ADMISSION_CEILING_PERMILLE,
            MAX_NORMAL_ADMISSION_CEILING_PERMILLE,
        );
        let emergency_floor = self.normal_admission_ceiling_permille.saturating_add(1);
        self.emergency_ceiling_permille = self.emergency_ceiling_permille.clamp(
            emergency_floor.min(MAX_EMERGENCY_CEILING_PERMILLE),
            MAX_EMERGENCY_CEILING_PERMILLE,
        );
        self.shadow_max_bytes_per_shadow = self.shadow_max_bytes_per_shadow.clamp(
            MIN_SHADOW_MAX_BYTES_PER_SHADOW,
            MAX_SHADOW_MAX_BYTES_PER_SHADOW,
        );
        // Migrate a persisted pre-capacity configuration: quotas that are
        // exactly the documented shares of the old fixed total are DEFAULT
        // shares, so let them follow the effective budget instead of pinning
        // a stale 32 GiB-derived number.
        let derived_defaults = !self.quotas.is_empty()
            && StorageClass::ALL.iter().all(|class| {
                self.quotas.get(class).copied()
                    == Some(
                        self.total_budget_bytes
                            .saturating_mul(class.default_share_permille())
                            / 1000,
                    )
            });
        if derived_defaults {
            self.quotas.clear();
        }
        for (class, quota) in &self.quotas {
            if *quota == 0 {
                return Err(format!(
                    "storage quota for {} must be > 0 (a zero quota refuses every write)",
                    class.as_str()
                ));
            }
            if *quota > self.total_budget_bytes {
                return Err(format!(
                    "storage quota for {} ({quota}) exceeds the aggregate budget ({})",
                    class.as_str(),
                    self.total_budget_bytes
                ));
            }
        }
        Ok(self)
    }

    /// The explicit quota when the operator configured one, otherwise this
    /// class's share of `effective_budget` (never above it).
    pub fn quota_for(&self, class: StorageClass, effective_budget: u64) -> u64 {
        match self.quotas.get(&class) {
            Some(quota) => (*quota).min(effective_budget),
            None => effective_budget.saturating_mul(class.default_share_permille()) / 1000,
        }
    }

    /// Legacy accessor: an explicit quota, otherwise the share of the
    /// configured operator cap. Governor paths use [`Self::quota_for`] with
    /// the capacity-aware effective budget.
    pub fn quota(&self, class: StorageClass) -> u64 {
        self.quota_for(class, self.total_budget_bytes)
    }

    /// The NORMAL admission band ceiling in bytes over one effective budget
    /// (default 85%): normal admissions may never push durable+pending
    /// aggregate usage above it.
    pub fn normal_admission_ceiling_bytes(&self, effective_budget: u64) -> u64 {
        effective_budget.saturating_mul(self.normal_admission_ceiling_permille as u64) / 1000
    }

    /// The EMERGENCY/critical-metadata band ceiling in bytes (default 90%):
    /// ledger/self-metadata growth may proceed up to here; everything above
    /// is the RECOVERY-ONLY reserve.
    pub fn emergency_ceiling_bytes(&self, effective_budget: u64) -> u64 {
        effective_budget.saturating_mul(self.emergency_ceiling_permille as u64) / 1000
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
            ..Self::default()
        };
        for (class, bytes) in overrides {
            config.quotas.insert(class, bytes);
        }
        config.validated()
    }
}
