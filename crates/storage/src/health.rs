//! Typed health and metrics snapshots (doctor and native metrics consume the
//! same structure; no number is fabricated).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::capacity::FilesystemCapacity;
use crate::class::StorageClass;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClassUsage {
    pub class: StorageClass,
    pub bytes: u64,
    pub objects: u64,
    pub quota_bytes: u64,
    pub pinned_bytes: u64,
    pub pending_reservation_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageMetrics {
    pub total_bytes: u64,
    /// The EFFECTIVE hard budget (min of the operator cap and the measured
    /// capacity-relative cap); class quotas and watermarks use this number.
    pub total_budget_bytes: u64,
    /// The operator cap as configured (before capacity clamping).
    pub configured_budget_bytes: u64,
    /// Total capacity of the distinct backing filesystems (shared volumes
    /// counted once).
    pub capacity_bytes: u64,
    /// Free bytes available to an unprivileged writer right now.
    pub available_bytes: u64,
    /// Always-preserved free-space reserve: max(reserve_bytes, 5% capacity).
    pub reserve_bytes: u64,
    pub effective_budget_bytes: u64,
    /// Free bytes above the reserve (`available - reserve`).
    pub headroom_bytes: u64,
    /// False when capacity could not be measured; the operator cap alone
    /// applies and no capacity number is fabricated.
    pub capacity_measured: bool,
    pub filesystems: Vec<FilesystemCapacity>,
    pub total_objects: u64,
    /// Aggregate usage in permille of the hard budget (can exceed 1000 only
    /// when a class could not be evicted; health then names it degraded).
    pub used_permille_of_budget: u64,
    pub watermark_permille: u16,
    /// True when total usage is at or above the configured watermark.
    pub above_watermark: bool,
    pub per_class: BTreeMap<String, ClassUsage>,
    pub evictions: u64,
    pub refusals: u64,
    pub pending_reservations: u64,
    pub loss_markers: u64,
    /// Unix ms of the last maintenance pass (reconcile/enforce/WAL/remnant
    /// cleanup); `None` when the governor has never run one.
    pub last_maintenance_ms: Option<i64>,
    pub degraded: Option<DegradedHealth>,
}

/// WHY the budget could not be met. Typed, persisted, and surfaced by doctor;
/// never a silent truncation claim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DegradedHealth {
    pub code: String,
    pub detail: String,
    pub total_bytes: u64,
    pub total_budget_bytes: u64,
    pub required_bytes: u64,
    pub class: Option<StorageClass>,
    pub at_ms: i64,
}
