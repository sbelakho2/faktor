//! Filesystem capacity measurement and the capacity-aware budget.
//!
//! The nominal governor budget is never the whole story: a disk can already
//! be nearly full when Faktor starts. Every admission therefore combines a
//! hard budget (the smaller of the operator cap and a capacity-relative cap)
//! with an always-preserved free-space reserve. Capacity and free space are
//! measured per backing filesystem and deduplicated by filesystem identity,
//! so several governed roots on one volume are counted once.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::StorageGovernorConfig;

/// One measured backing filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FilesystemSpace {
    /// Stable identity of the backing filesystem/volume. Two paths that
    /// report the same id share one capacity, so shared volumes are counted
    /// exactly once.
    pub filesystem_id: u64,
    pub capacity_bytes: u64,
    /// Bytes available to an unprivileged writer (`f_bavail`, not `f_bfree`).
    pub available_bytes: u64,
}

/// Measurement seam. Production uses [`SystemCapacityProbe`]; tests inject
/// simulated filesystems so low-capacity/low-free behaviour is deterministic.
pub trait CapacityProbe: Send + Sync + std::panic::RefUnwindSafe {
    fn space(&self, path: &Path) -> Option<FilesystemSpace>;
}

/// Reads real filesystem capacity/free space.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemCapacityProbe;

impl CapacityProbe for SystemCapacityProbe {
    fn space(&self, path: &Path) -> Option<FilesystemSpace> {
        let existing = nearest_existing(path)?;
        let filesystem_id = filesystem_id(&existing)?;
        let (capacity_bytes, available_bytes) = system_space(&existing)?;
        Some(FilesystemSpace {
            filesystem_id,
            capacity_bytes,
            available_bytes,
        })
    }
}

/// One deduplicated filesystem entry in the metrics snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilesystemCapacity {
    pub capacity_bytes: u64,
    pub available_bytes: u64,
}

/// The effective capacity-aware budget at one instant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapacitySnapshot {
    /// False when no backing path could be measured; the configured user cap
    /// then remains the only limit (no fabricated capacity number).
    pub measured: bool,
    pub capacity_bytes: u64,
    pub available_bytes: u64,
    pub reserve_bytes: u64,
    /// Capacity-relative cap before the operator cap is applied.
    pub capacity_cap_bytes: u64,
    /// `min(user cap, capacity cap, capacity - reserve)`: the hard budget.
    pub effective_budget_bytes: u64,
    /// Free bytes above the reserve: what may still be admitted.
    pub headroom_bytes: u64,
    pub filesystems: Vec<FilesystemCapacity>,
}

impl Default for CapacitySnapshot {
    fn default() -> Self {
        Self {
            measured: false,
            capacity_bytes: 0,
            available_bytes: 0,
            reserve_bytes: 0,
            capacity_cap_bytes: u64::MAX,
            effective_budget_bytes: u64::MAX,
            headroom_bytes: u64::MAX,
            filesystems: Vec::new(),
        }
    }
}

impl CapacitySnapshot {
    /// Combine measured filesystems with the configured policy.
    ///
    /// `reserve = max(reserve_bytes, reserve_permille * capacity)`;
    /// `capacity_cap = max(capacity_permille * capacity, min_install_allowance)`;
    /// `effective = min(user_cap, capacity_cap, capacity - reserve)`.
    pub fn evaluate(
        config: &StorageGovernorConfig,
        spaces: impl IntoIterator<Item = FilesystemSpace>,
    ) -> Self {
        let mut by_id: BTreeMap<u64, FilesystemCapacity> = BTreeMap::new();
        for space in spaces {
            by_id
                .entry(space.filesystem_id)
                .and_modify(|entry| {
                    // The same volume measured through several roots shares
                    // one capacity; free space is the conservative minimum.
                    entry.capacity_bytes = entry.capacity_bytes.max(space.capacity_bytes);
                    entry.available_bytes = entry.available_bytes.min(space.available_bytes);
                })
                .or_insert(FilesystemCapacity {
                    capacity_bytes: space.capacity_bytes,
                    available_bytes: space.available_bytes,
                });
        }
        if by_id.is_empty() {
            return Self {
                measured: false,
                capacity_cap_bytes: config.total_budget_bytes,
                effective_budget_bytes: config.total_budget_bytes,
                ..Self::default()
            };
        }
        let filesystems: Vec<FilesystemCapacity> = by_id.into_values().collect();
        let capacity_bytes = filesystems
            .iter()
            .fold(0u64, |sum, fs| sum.saturating_add(fs.capacity_bytes));
        let available_bytes = filesystems
            .iter()
            .fold(0u64, |sum, fs| sum.saturating_add(fs.available_bytes));
        let reserve_bytes = config
            .reserve_bytes
            .max(capacity_bytes.saturating_mul(config.reserve_permille as u64) / 1000);
        let relative_cap = capacity_bytes.saturating_mul(config.capacity_permille as u64) / 1000;
        let capacity_cap_bytes = relative_cap
            .max(config.min_install_allowance_bytes)
            .min(capacity_bytes.saturating_sub(reserve_bytes));
        let effective_budget_bytes = config.total_budget_bytes.min(capacity_cap_bytes);
        Self {
            measured: true,
            capacity_bytes,
            available_bytes,
            reserve_bytes,
            capacity_cap_bytes,
            effective_budget_bytes,
            headroom_bytes: available_bytes.saturating_sub(reserve_bytes),
            filesystems,
        }
    }

    /// Bytes that must be reclaimed before `incoming_bytes` may be admitted
    /// without eating into the reserve.
    pub fn admission_deficit(&self, incoming_bytes: u64) -> u64 {
        if !self.measured {
            return 0;
        }
        incoming_bytes
            .saturating_add(self.reserve_bytes)
            .saturating_sub(self.available_bytes)
    }

    /// Bytes currently missing from the always-preserved reserve.
    pub fn reserve_deficit(&self) -> u64 {
        if !self.measured {
            return 0;
        }
        self.reserve_bytes.saturating_sub(self.available_bytes)
    }
}

fn nearest_existing(path: &Path) -> Option<PathBuf> {
    let mut current = Some(path);
    while let Some(candidate) = current {
        if candidate.exists() {
            return Some(candidate.to_path_buf());
        }
        current = candidate.parent();
    }
    None
}

#[cfg(unix)]
fn filesystem_id(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(std::fs::metadata(path).ok()?.dev())
}

#[cfg(windows)]
fn filesystem_id(path: &Path) -> Option<u64> {
    use std::os::windows::fs::MetadataExt;
    Some(
        std::fs::metadata(path)
            .ok()?
            .volume_serial_number()
            .map(u64::from)
            .unwrap_or(0),
    )
}

#[cfg(not(any(unix, windows)))]
fn filesystem_id(_path: &Path) -> Option<u64> {
    None
}

#[cfg(unix)]
fn system_space(path: &Path) -> Option<(u64, u64)> {
    let stat = rustix::fs::statvfs(path).ok()?;
    let unit = if stat.f_frsize != 0 {
        stat.f_frsize
    } else {
        stat.f_bsize
    };
    Some((
        stat.f_blocks.saturating_mul(unit),
        stat.f_bavail.saturating_mul(unit),
    ))
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn system_space(path: &Path) -> Option<(u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let mut available = 0u64;
    let mut total = 0u64;
    let mut total_free = 0u64;
    let ok =
        unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, &mut total, &mut total_free) };
    (ok != 0).then_some((total, available))
}

#[cfg(not(any(unix, windows)))]
fn system_space(_path: &Path) -> Option<(u64, u64)> {
    None
}
