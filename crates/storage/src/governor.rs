//! The always-on storage governor.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use serde::{Deserialize, Serialize};

use crate::class::StorageClass;
use crate::config::{StorageGovernorConfig, DEFAULT_RESERVATION_LEASE_MS};
use crate::directory::{object_row, DirectoryBackend, SqliteBackend};
use crate::error::{GovernorError, GovernorResult, RefusalReason, StorageRefusal};
use crate::health::{ClassUsage, DegradedHealth, StorageMetrics};
use crate::ledger::{now_ms, Counters, Ledger, ObjectRow, UsageRow};
use crate::object::{EvictionOutcome, StorageBackend, StorageObject};

/// The governor's own durable directory name (never part of any budgeted
/// class: its ledger is bookkeeping, not governed payload).
pub const GOVERNOR_DIR_NAME: &str = "storage-governor";
pub const GOVERNOR_DB_NAME: &str = "governor.db";
pub const DEFAULT_UPDATE_REMNANT_STALE_MS: i64 = 60 * 60 * 1000;

/// Resolved roots for the classes that are plain directories.
#[derive(Debug, Clone)]
pub struct GovernorRoots {
    pub data_dir: PathBuf,
    pub browser_profiles: Vec<PathBuf>,
    pub terminal_agent_home: PathBuf,
    pub update_staging: Vec<PathBuf>,
    pub rebuildable_caches: Vec<PathBuf>,
}

impl GovernorRoots {
    pub fn under(data_dir: impl Into<PathBuf>) -> Self {
        let data_dir = data_dir.into();
        Self {
            browser_profiles: vec![
                data_dir.join("commerce").join("profiles"),
                data_dir.join("browser").join("profiles"),
            ],
            terminal_agent_home: data_dir.join("agent-home"),
            update_staging: vec![
                data_dir.join("install").join("staging"),
                data_dir.join("updates").join("staging"),
            ],
            rebuildable_caches: vec![
                data_dir.join("store").join("index_data").join("scratch"),
                data_dir
                    .join("store")
                    .join("index_data")
                    .join("generations"),
                data_dir.join("store").join("cache"),
                data_dir.join("cache"),
            ],
            data_dir,
        }
    }
}

struct Inner {
    root: PathBuf,
    data_dir: PathBuf,
    config: StorageGovernorConfig,
    ledger: Mutex<Ledger>,
    backends: RwLock<BTreeMap<StorageClass, Arc<dyn StorageBackend>>>,
    scan_errors: Mutex<BTreeMap<StorageClass, String>>,
    degraded: Mutex<Option<DegradedHealth>>,
    eviction_queries: AtomicU64,
    last_sqlite_maintenance_ms: AtomicI64,
}

/// One shared governor handle (clone is cheap).
#[derive(Clone)]
pub struct StorageGovernor {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for StorageGovernor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageGovernor")
            .field("root", &self.inner.root)
            .field("total_budget_bytes", &self.inner.config.total_budget_bytes)
            .finish()
    }
}

impl StorageGovernor {
    pub fn governor_dir(data_dir: &Path) -> PathBuf {
        data_dir.join(GOVERNOR_DIR_NAME)
    }

    pub fn database_path(data_dir: &Path) -> PathBuf {
        Self::governor_dir(data_dir).join(GOVERNOR_DB_NAME)
    }

    /// Open (creating if necessary) the governor for one data directory.
    /// The persisted configuration is authoritative; passing a different
    /// explicit config replaces it (an operator decision, validated).
    pub fn open(
        data_dir: impl Into<PathBuf>,
        config: StorageGovernorConfig,
    ) -> GovernorResult<Self> {
        let data_dir = data_dir.into();
        let config = config.validated().map_err(GovernorError::Malformed)?;
        let root = Self::governor_dir(&data_dir);
        std::fs::create_dir_all(&root)?;
        let ledger = Ledger::open(&root.join(GOVERNOR_DB_NAME))?;
        let stored = ledger.load_config()?;
        if stored.as_ref() != Some(&config) {
            ledger.store_config(&config)?;
        }
        Self::assemble(root, data_dir, config, ledger)
    }

    /// Open only when the governor database already exists (production CAS
    /// construction uses this so a bare test directory is not transformed
    /// into a governed installation).
    pub fn open_existing(data_dir: impl AsRef<Path>) -> Option<Self> {
        let data_dir = data_dir.as_ref();
        let root = Self::governor_dir(data_dir);
        let path = root.join(GOVERNOR_DB_NAME);
        if !path.is_file() {
            return None;
        }
        let ledger = Ledger::open(&path).ok()?;
        let config = ledger
            .load_config()
            .ok()
            .flatten()
            .unwrap_or_default()
            .validated()
            .ok()?;
        Self::assemble(root, data_dir.to_path_buf(), config, ledger).ok()
    }

    /// Discover a governor from any path whose ancestors contain a
    /// `storage-governor/governor.db` marker (the CAS/store attach path).
    pub fn discover(path: &Path) -> Option<Self> {
        let start = if path.is_dir() {
            path.to_path_buf()
        } else {
            path.parent()?.to_path_buf()
        };
        for ancestor in start.ancestors().take(6) {
            let marker = Self::database_path(ancestor);
            if marker.is_file() {
                return Self::open_existing(ancestor);
            }
        }
        None
    }

    fn assemble(
        root: PathBuf,
        data_dir: PathBuf,
        config: StorageGovernorConfig,
        ledger: Ledger,
    ) -> GovernorResult<Self> {
        let persisted_degraded = ledger
            .meta("degraded")
            .ok()
            .flatten()
            .filter(|raw| !raw.is_empty())
            .and_then(|raw| serde_json::from_str::<DegradedHealth>(&raw).ok());
        let governor = Self {
            inner: Arc::new(Inner {
                root,
                data_dir: data_dir.clone(),
                config,
                ledger: Mutex::new(ledger),
                backends: RwLock::new(BTreeMap::new()),
                scan_errors: Mutex::new(BTreeMap::new()),
                degraded: Mutex::new(persisted_degraded),
                eviction_queries: AtomicU64::new(0),
                last_sqlite_maintenance_ms: AtomicI64::new(0),
            }),
        };
        governor.install_default_backends(&GovernorRoots::under(data_dir));
        Ok(governor)
    }

    fn install_default_backends(&self, roots: &GovernorRoots) {
        // Registration is unconditional: the directory may be created AFTER
        // the governor (browser profiles, caches, updater staging). A
        // missing root scans as empty and starts counting as soon as it
        // appears; never silently ungoverned.
        let mut browser_roots: Vec<PathBuf> = roots.browser_profiles.clone();
        browser_roots.push(roots.terminal_agent_home.clone());
        let _ = self.register_backend(Arc::new(DirectoryBackend::with_roots(
            StorageClass::BrowserStorage,
            browser_roots,
        )));
        let _ = self.register_backend(Arc::new(
            DirectoryBackend::with_roots(StorageClass::UpdateRemnant, roots.update_staging.clone())
                .with_min_age(DEFAULT_UPDATE_REMNANT_STALE_MS),
        ));
        let _ = self.register_backend(Arc::new(DirectoryBackend::with_roots(
            StorageClass::RebuildableCache,
            roots.rebuildable_caches.clone(),
        )));
        let _ = self.register_backend(Arc::new(SqliteBackend::new(roots.data_dir.clone())));
    }

    pub fn root(&self) -> &Path {
        &self.inner.root
    }

    pub fn data_dir(&self) -> &Path {
        &self.inner.data_dir
    }

    pub fn config(&self) -> &StorageGovernorConfig {
        &self.inner.config
    }

    pub fn register_backend(&self, backend: Arc<dyn StorageBackend>) -> GovernorResult<()> {
        self.inner
            .backends
            .write()
            .unwrap()
            .insert(backend.class(), backend);
        Ok(())
    }

    pub fn backend(&self, class: StorageClass) -> Option<Arc<dyn StorageBackend>> {
        self.inner.backends.read().unwrap().get(&class).cloned()
    }

    /// Durable object classification of a CAS digest (used by CAS backends
    /// to divide one physical root into logical classes).
    pub fn object_class_of_digest(&self, digest: &str) -> GovernorResult<Option<StorageClass>> {
        self.inner
            .ledger
            .lock()
            .unwrap()
            .object_class_of_digest(digest)
    }

    /// Digest ownership inside one physical root family (`main` or
    /// `evidence`). The same content in two roots is two governed blobs.
    pub fn object_class_of_digest_in_family(
        &self,
        digest: &str,
        family: &str,
    ) -> GovernorResult<Option<StorageClass>> {
        self.inner
            .ledger
            .lock()
            .unwrap()
            .object_class_of_digest_in_family(digest, family)
    }

    pub fn object_last_used(&self, class: StorageClass, key: &str) -> GovernorResult<Option<i64>> {
        self.inner
            .ledger
            .lock()
            .unwrap()
            .object_last_used(class, key)
    }

    pub fn register_object(
        &self,
        class: StorageClass,
        key: &str,
        bytes: u64,
        digest: Option<&str>,
    ) -> GovernorResult<()> {
        let inserted = self.inner.ledger.lock().unwrap().upsert_object(
            class,
            digest.unwrap_or(key),
            bytes,
            digest,
            now_ms(),
            false,
        )?;
        if !inserted {
            // The digest already belongs to another class in the same
            // physical root (dedup across logical classes): touch the
            // existing owner instead of double counting.
            if let Some(digest) = digest {
                if let Some(owner) =
                    self.object_class_of_digest_in_family(digest, class.family())?
                {
                    let _ = self
                        .inner
                        .ledger
                        .lock()
                        .unwrap()
                        .touch(owner, digest, now_ms());
                }
            }
        }
        Ok(())
    }

    /// Install a scanned inventory in one transaction (used by reconciliation
    /// and by the scale probe). Rows are `(key, bytes, last_used_ms)`.
    pub fn replace_inventory(
        &self,
        class: StorageClass,
        objects: &[(String, u64, i64)],
    ) -> GovernorResult<()> {
        let rows: Vec<ObjectRow> = objects
            .iter()
            .map(|(key, bytes, last_used)| ObjectRow {
                class,
                key: key.clone(),
                bytes: *bytes,
                digest: None,
                last_used_ms: *last_used,
                required: false,
            })
            .collect();
        let mut ledger = self.inner.ledger.lock().unwrap();
        ledger.set_inventory(class, &rows)?;
        Ok(())
    }

    pub fn touch(&self, class: StorageClass, key: &str) -> GovernorResult<()> {
        self.inner
            .ledger
            .lock()
            .unwrap()
            .touch(class, key, now_ms())
    }

    pub fn pin(&self, class: StorageClass, key: &str, reason: &str) -> GovernorResult<()> {
        let mut ledger = self.inner.ledger.lock().unwrap();
        ledger.pin(class, key, reason)
    }

    pub fn unpin(&self, class: StorageClass, key: &str) -> GovernorResult<()> {
        let mut ledger = self.inner.ledger.lock().unwrap();
        ledger.unpin(class, key)
    }

    pub fn is_pinned(&self, class: StorageClass, key: &str) -> GovernorResult<bool> {
        self.inner.ledger.lock().unwrap().is_pinned(class, key)
    }

    /// Reserve `bytes` BEFORE publishing. Performs eviction by class/age when
    /// the class quota or the aggregate budget would be exceeded, and refuses
    /// typed (recording the refusal) when the budget cannot be met.
    pub fn reserve(
        &self,
        class: StorageClass,
        key: &str,
        bytes: u64,
    ) -> GovernorResult<Reservation> {
        if key.is_empty() {
            return Err(GovernorError::Malformed(
                "storage reservation requires a non-empty key".into(),
            ));
        }
        let quota = self.inner.config.quota(class);
        let total = self.inner.config.total_budget_bytes;
        if bytes > quota {
            return self.refuse(
                class,
                key,
                bytes,
                0,
                RefusalReason::ClassQuota,
                format!("object alone exceeds the {} byte class quota", quota),
            );
        }
        if bytes > total {
            return self.refuse(
                class,
                key,
                bytes,
                0,
                RefusalReason::AggregateBudget,
                format!("object alone exceeds the {total} byte aggregate budget"),
            );
        }
        let mut last_available = 0u64;
        let mut last_reason = RefusalReason::NoEvictableObjects;
        for _attempt in 0..4 {
            match self.try_reserve(class, key, bytes)? {
                Admit::Reserved(reservation) => {
                    self.clear_degraded_if_healthy()?;
                    return Ok(reservation);
                }
                Admit::Need {
                    deficit,
                    available,
                    reason,
                } => {
                    last_available = available;
                    last_reason = reason;
                    let freed = self.evict_for(class, deficit)?;
                    if freed == 0 {
                        break;
                    }
                }
            }
        }
        self.refuse(
            class,
            key,
            bytes,
            last_available,
            last_reason,
            "eviction could not free enough bytes; the hard budget is not met".into(),
        )
    }

    fn try_reserve(&self, class: StorageClass, key: &str, bytes: u64) -> GovernorResult<Admit> {
        let now = now_ms();
        let mut ledger = self.inner.ledger.lock().unwrap();
        let _ = ledger.expire_reservations(now)?;
        let usage = ledger.usage()?;
        let pending = ledger.pending_by_class()?;
        let class_used = usage.get(&class).map(|row| row.bytes).unwrap_or(0)
            + pending.get(&class).copied().unwrap_or(0);
        let total_used =
            usage.values().map(|row| row.bytes).sum::<u64>() + pending.values().sum::<u64>();
        let quota = self.inner.config.quota(class);
        let total = self.inner.config.total_budget_bytes;
        let class_room = quota.saturating_sub(class_used);
        let total_room = total.saturating_sub(total_used);
        let available = class_room.min(total_room);
        if bytes <= available {
            let id = uuid::Uuid::new_v4().simple().to_string();
            let expires = now.saturating_add(self.inner.config.reservation_lease_ms.max(
                if self.inner.config.reservation_lease_ms == 0 {
                    DEFAULT_RESERVATION_LEASE_MS
                } else {
                    self.inner.config.reservation_lease_ms
                },
            ));
            ledger.insert_reservation(&id, class, key, bytes, now, expires)?;
            return Ok(Admit::Reserved(Reservation {
                governor: self.clone(),
                id,
                class,
                key: key.to_string(),
                bytes,
                active: true,
            }));
        }
        let pinned = usage.get(&class).map(|row| row.pinned_bytes).unwrap_or(0);
        let reason = if pinned >= quota || (total_used >= total && pinned >= quota) {
            RefusalReason::RequiredBytes
        } else if self.backend(class).is_none() {
            RefusalReason::NoBackend
        } else {
            RefusalReason::NoEvictableObjects
        };
        let deficit = class_used
            .saturating_add(bytes)
            .saturating_sub(quota)
            .max(total_used.saturating_add(bytes).saturating_sub(total));
        Ok(Admit::Need {
            deficit,
            available,
            reason,
        })
    }

    fn refuse<T>(
        &self,
        class: StorageClass,
        _key: &str,
        bytes: u64,
        available: u64,
        reason: RefusalReason,
        detail: String,
    ) -> GovernorResult<T> {
        {
            let mut ledger = self.inner.ledger.lock().unwrap();
            ledger.record_refusal(class, bytes, available, reason.as_str(), &detail)?;
        }
        let (total_bytes, required_bytes) = {
            let ledger = self.inner.ledger.lock().unwrap();
            (ledger.sum_usage()?, ledger.sum_pinned()?)
        };
        let health = DegradedHealth {
            code: format!("storage_{}", reason.as_str()),
            detail: detail.clone(),
            total_bytes,
            total_budget_bytes: self.inner.config.total_budget_bytes,
            required_bytes,
            class: Some(class),
            at_ms: now_ms(),
        };
        self.record_degraded(health)?;
        Err(GovernorError::Refused(StorageRefusal {
            class,
            requested_bytes: bytes,
            available_bytes: available,
            reason,
            detail,
        }))
    }

    fn evict_for(&self, class: StorageClass, target: u64) -> GovernorResult<u64> {
        if target == 0 {
            return Ok(0);
        }
        let mut freed = 0u64;
        let mut attempted: std::collections::BTreeSet<(StorageClass, String)> =
            std::collections::BTreeSet::new();
        let mut steps = 0usize;
        while freed < target && steps < 1_000_000 {
            let batch = {
                let ledger = self.inner.ledger.lock().unwrap();
                self.inner.eviction_queries.fetch_add(1, Ordering::Relaxed);
                let class_page = ledger.candidates_page(class, (i64::MIN, ""), 64)?;
                if class_page.is_empty() {
                    ledger.global_candidates_page((i64::MIN, "", ""), 64)?
                } else {
                    class_page
                }
            };
            let mut progressed = false;
            for object in batch {
                steps += 1;
                if freed >= target {
                    break;
                }
                let identity = (object.class, object.key.clone());
                if !attempted.insert(identity) {
                    continue;
                }
                progressed = true;
                freed = freed.saturating_add(self.evict_row(&object)?);
            }
            if !progressed {
                break;
            }
        }
        Ok(freed)
    }

    fn evict_row(&self, object: &ObjectRow) -> GovernorResult<u64> {
        if object.required || self.is_pinned(object.class, &object.key)? {
            return Ok(0);
        }
        let Some(backend) = self.backend(object.class) else {
            return Ok(0);
        };
        let outcome = backend.evict(
            self,
            &StorageObject {
                key: object.key.clone(),
                bytes: object.bytes,
                last_used_ms: object.last_used_ms,
                digest: object.digest.clone(),
                required: object.required,
            },
        )?;
        match outcome {
            EvictionOutcome::Evicted {
                bytes,
                verified,
                digest,
            } => {
                let mut ledger = self.inner.ledger.lock().unwrap();
                ledger.remove_object(object.class, &object.key)?;
                ledger.record_eviction(
                    object.class,
                    &object.key,
                    digest.as_deref().or(object.digest.as_deref()),
                    bytes,
                    verified,
                    "governed_eviction",
                )?;
                if object.class.requires_loss_marker() {
                    ledger.record_loss_marker(
                        object.class,
                        &object.key,
                        digest.as_deref().or(object.digest.as_deref()),
                        bytes,
                        "archive_evicted",
                    )?;
                }
                Ok(bytes)
            }
            EvictionOutcome::Absent => {
                let mut ledger = self.inner.ledger.lock().unwrap();
                ledger.remove_object(object.class, &object.key)?;
                Ok(0)
            }
            EvictionOutcome::Refused { detail } => {
                let mut ledger = self.inner.ledger.lock().unwrap();
                ledger.record_refusal(
                    object.class,
                    object.bytes,
                    0,
                    RefusalReason::BackendRefused.as_str(),
                    &detail,
                )?;
                Ok(0)
            }
        }
    }

    /// Evict one named object (used by explicit retention and tests). Pinned
    /// objects are never touched. Archive eviction leaves a durable marker.
    pub fn evict_object(&self, class: StorageClass, key: &str) -> GovernorResult<u64> {
        let Some(object) = self.inner.ledger.lock().unwrap().object_row(class, key)? else {
            return Ok(0);
        };
        self.evict_row(&object)
    }

    /// Enforce every class quota and the aggregate budget. Returns the bytes
    /// freed; when the budget still cannot be met health is degraded typed.
    pub fn enforce(&self) -> GovernorResult<u64> {
        self.record_maintenance()?;
        let mut freed = 0u64;
        let usage = self.inner.ledger.lock().unwrap().usage()?;
        for class in StorageClass::ALL {
            let used = usage.get(&class).map(|row| row.bytes).unwrap_or(0);
            let quota = self.inner.config.quota(class);
            if used > quota {
                freed += self.evict_for(class, used - quota)?;
            }
        }
        let total_used = self.inner.ledger.lock().unwrap().sum_usage()?;
        if total_used > self.inner.config.total_budget_bytes {
            freed += self.evict_for(
                StorageClass::RebuildableCache,
                total_used - self.inner.config.total_budget_bytes,
            )?;
        }
        self.clear_degraded_if_healthy()?;
        self.evaluate_degraded()?;
        Ok(freed)
    }

    /// Full reconciliation: expire crashed reservations, scan every registered
    /// backend, rebuild the object index/usage from durable disk state, and
    /// record archive objects that vanished without an explicit eviction.
    pub fn reconcile(&self) -> GovernorResult<ReconcileReport> {
        let backends: Vec<Arc<dyn StorageBackend>> = self
            .inner
            .backends
            .read()
            .unwrap()
            .values()
            .cloned()
            .collect();
        let mut by_class: BTreeMap<StorageClass, Vec<ObjectRow>> = BTreeMap::new();
        let mut scan_errors = BTreeMap::new();
        for backend in backends {
            match backend.scan(self) {
                Ok(objects) => {
                    let class = backend.class();
                    let entry = by_class.entry(class).or_default();
                    entry.extend(objects.iter().map(|object| object_row(class, object)));
                }
                Err(error) => {
                    scan_errors.insert(backend.class(), error.to_string());
                }
            }
        }
        let mut report = ReconcileReport::default();
        {
            let mut ledger = self.inner.ledger.lock().unwrap();
            report.expired_reservations = ledger.expire_reservations(now_ms())?;
            for (class, objects) in by_class {
                let change = ledger.set_inventory(class, &objects)?;
                report.scanned += change.scanned;
                report.indexed += change.inserted;
                report.removed += change.removed;
                report.missing_loss_markers += change.missing_markers;
            }
            report.total_bytes = ledger.sum_usage()?;
        }
        *self.inner.scan_errors.lock().unwrap() = scan_errors;
        self.record_maintenance()?;
        self.clear_degraded_if_healthy()?;
        self.evaluate_degraded()?;
        Ok(report)
    }

    pub fn metrics(&self) -> GovernorResult<StorageMetrics> {
        let (usage, pending, counters, total, objects, total_pending) = {
            let ledger = self.inner.ledger.lock().unwrap();
            (
                ledger.usage()?,
                ledger.pending_by_class()?,
                ledger.counters()?,
                ledger.sum_usage()?,
                ledger.sum_objects()?,
                ledger.pending_count()?,
            )
        };
        let mut per_class = BTreeMap::new();
        for class in StorageClass::ALL {
            let usage = usage.get(&class).cloned().unwrap_or_else(UsageRow::default);
            per_class.insert(
                class.as_str().to_string(),
                ClassUsage {
                    class,
                    bytes: usage.bytes,
                    objects: usage.objects,
                    quota_bytes: self.inner.config.quota(class),
                    pinned_bytes: usage.pinned_bytes,
                    pending_reservation_bytes: pending.get(&class).copied().unwrap_or(0),
                },
            );
        }
        let watermark_floor = self
            .inner
            .config
            .total_budget_bytes
            .saturating_mul(self.inner.config.watermark_permille as u64)
            / 1000;
        Ok(StorageMetrics {
            total_bytes: total,
            total_budget_bytes: self.inner.config.total_budget_bytes,
            total_objects: objects,
            used_permille_of_budget: total
                .saturating_mul(1000)
                .checked_div(self.inner.config.total_budget_bytes)
                .unwrap_or(0),
            watermark_permille: self.inner.config.watermark_permille,
            above_watermark: total >= watermark_floor,
            per_class,
            evictions: counters.evictions,
            refusals: counters.refusals,
            pending_reservations: total_pending,
            loss_markers: counters.loss_markers,
            last_maintenance_ms: self.last_maintenance_ms()?,
            degraded: self.evaluate_degraded()?,
        })
    }

    /// Unix ms of the last maintenance pass (reconcile, enforce, WAL
    /// checkpoint/truncate, remnant cleanup); durable across restarts.
    pub fn last_maintenance_ms(&self) -> GovernorResult<Option<i64>> {
        Ok(self
            .inner
            .ledger
            .lock()
            .unwrap()
            .meta("last_maintenance_ms")?
            .and_then(|raw| raw.parse::<i64>().ok()))
    }

    fn record_maintenance(&self) -> GovernorResult<()> {
        self.inner
            .ledger
            .lock()
            .unwrap()
            .set_meta("last_maintenance_ms", &now_ms().to_string())
    }

    pub fn health(&self) -> GovernorResult<Option<DegradedHealth>> {
        self.evaluate_degraded()
    }

    fn evaluate_degraded(&self) -> GovernorResult<Option<DegradedHealth>> {
        let (usage, total, pinned) = {
            let ledger = self.inner.ledger.lock().unwrap();
            (ledger.usage()?, ledger.sum_usage()?, ledger.sum_pinned()?)
        };
        let structural = if total > self.inner.config.total_budget_bytes {
            Some(DegradedHealth {
                code: "aggregate_budget_exceeded".into(),
                detail: format!(
                    "{} bytes exceed the hard {} byte aggregate budget and no evictable object remains",
                    total, self.inner.config.total_budget_bytes
                ),
                total_bytes: total,
                total_budget_bytes: self.inner.config.total_budget_bytes,
                required_bytes: pinned,
                class: None,
                at_ms: now_ms(),
            })
        } else {
            usage
                .iter()
                .find_map(|(class, row)| {
                    let quota = self.inner.config.quota(*class);
                    (row.bytes > quota).then(|| DegradedHealth {
                        code: "class_quota_exceeded".into(),
                        detail: format!(
                            "{} holds {} bytes over its {} byte quota and no evictable object remains",
                            class.as_str(),
                            row.bytes - quota,
                            quota
                        ),
                        total_bytes: total,
                        total_budget_bytes: self.inner.config.total_budget_bytes,
                        required_bytes: row.pinned_bytes,
                        class: Some(*class),
                        at_ms: now_ms(),
                    })
                })
                .or_else(|| {
                    (pinned > self.inner.config.total_budget_bytes).then(|| DegradedHealth {
                        code: "required_bytes_exceed_budget".into(),
                        detail: format!(
                            "{} required bytes exceed the hard {} byte aggregate budget",
                            pinned, self.inner.config.total_budget_bytes
                        ),
                        total_bytes: total,
                        total_budget_bytes: self.inner.config.total_budget_bytes,
                        required_bytes: pinned,
                        class: None,
                        at_ms: now_ms(),
                    })
                })
        };
        let scan_error =
            self.inner
                .scan_errors
                .lock()
                .unwrap()
                .iter()
                .next()
                .map(|(class, detail)| DegradedHealth {
                    code: "storage_scan_failed".into(),
                    detail: format!("{} scan failed: {detail}", class.as_str()),
                    total_bytes: total,
                    total_budget_bytes: self.inner.config.total_budget_bytes,
                    required_bytes: pinned,
                    class: Some(*class),
                    at_ms: now_ms(),
                });
        let prior = self.inner.degraded.lock().unwrap().clone();
        let chosen = structural.or(scan_error).or(prior);
        *self.inner.degraded.lock().unwrap() = chosen.clone();
        if let Some(degraded) = &chosen {
            if let Ok(raw) = serde_json::to_string(degraded) {
                let _ = self.inner.ledger.lock().unwrap().set_meta("degraded", &raw);
            }
        }
        Ok(chosen)
    }

    fn record_degraded(&self, health: DegradedHealth) -> GovernorResult<()> {
        *self.inner.degraded.lock().unwrap() = Some(health.clone());
        if let Ok(raw) = serde_json::to_string(&health) {
            self.inner
                .ledger
                .lock()
                .unwrap()
                .set_meta("degraded", &raw)?;
        }
        Ok(())
    }

    fn clear_degraded_if_healthy(&self) -> GovernorResult<()> {
        let ledger = self.inner.ledger.lock().unwrap();
        let total = ledger.sum_usage()?;
        let usage = ledger.usage()?;
        drop(ledger);
        if total <= self.inner.config.total_budget_bytes
            && usage
                .iter()
                .all(|(class, row)| row.bytes <= self.inner.config.quota(*class))
        {
            *self.inner.degraded.lock().unwrap() = None;
            let _ = self.inner.ledger.lock().unwrap().set_meta("degraded", "");
        }
        Ok(())
    }

    /// Checkpoint every discovered SQLite WAL (TRUNCATE) and enforce the
    /// class quota. Returns the bytes reclaimed.
    pub fn sqlite_maintenance(&self) -> GovernorResult<u64> {
        self.record_maintenance()?;
        let Some(backend) = self.backend(StorageClass::SqliteWal) else {
            return Ok(0);
        };
        let objects = backend.scan(self)?;
        let rows: Vec<ObjectRow> = objects
            .iter()
            .map(|object| object_row(StorageClass::SqliteWal, object))
            .collect();
        {
            let mut ledger = self.inner.ledger.lock().unwrap();
            let _ = ledger.set_inventory(StorageClass::SqliteWal, &rows)?;
        }
        let before = self
            .inner
            .ledger
            .lock()
            .unwrap()
            .usage()?
            .get(&StorageClass::SqliteWal)
            .map(|row| row.bytes)
            .unwrap_or(0);
        // Truncate every WAL: this is non-blocking (busy_timeout 0) and a
        // reader pinning frames simply leaves the file for the next tick.
        for object in &rows {
            if object.key.ends_with(".db-wal") {
                let _ = self.evict_row(object);
            }
        }
        let used = self
            .inner
            .ledger
            .lock()
            .unwrap()
            .usage()?
            .get(&StorageClass::SqliteWal)
            .map(|row| row.bytes)
            .unwrap_or(0);
        let quota = self.inner.config.quota(StorageClass::SqliteWal);
        if used > quota {
            // A live database cannot be deleted; only the WAL can shrink. If
            // the database alone exceeds the quota the budget cannot be met
            // and health is degraded typed.
            self.evict_for(StorageClass::SqliteWal, used - quota)?;
        }
        self.clear_degraded_if_healthy()?;
        self.evaluate_degraded()?;
        Ok(before.saturating_sub(
            self.inner
                .ledger
                .lock()
                .unwrap()
                .usage()?
                .get(&StorageClass::SqliteWal)
                .map(|row| row.bytes)
                .unwrap_or(0),
        ))
    }

    /// Growth refusal for SQLite: run WAL maintenance, then refuse the
    /// incoming append when the class quota or aggregate budget cannot hold
    /// it. The caller must not execute the write on `Err`.
    pub fn admit_sqlite_growth(&self, incoming_bytes: u64) -> GovernorResult<()> {
        // WAL maintenance is throttled on the hot append path; an explicit
        // `sqlite_maintenance` call is never throttled.
        let now = now_ms();
        let last = self
            .inner
            .last_sqlite_maintenance_ms
            .load(Ordering::Relaxed);
        if now.saturating_sub(last) >= 2_000 {
            let _ = self.sqlite_maintenance();
            self.inner
                .last_sqlite_maintenance_ms
                .store(now, Ordering::Relaxed);
        }
        let (class_used, total_used, pinned) = {
            let ledger = self.inner.ledger.lock().unwrap();
            let usage = ledger.usage()?;
            let pending = ledger.pending_by_class()?;
            let class_used = usage
                .get(&StorageClass::SqliteWal)
                .map(|row| row.bytes)
                .unwrap_or(0)
                + pending.get(&StorageClass::SqliteWal).copied().unwrap_or(0);
            let total_used =
                usage.values().map(|row| row.bytes).sum::<u64>() + pending.values().sum::<u64>();
            let pinned = ledger.sum_pinned()?;
            (class_used, total_used, pinned)
        };
        let quota = self.inner.config.quota(StorageClass::SqliteWal);
        let total = self.inner.config.total_budget_bytes;
        if class_used.saturating_add(incoming_bytes) <= quota
            && total_used.saturating_add(incoming_bytes) <= total
        {
            return Ok(());
        }
        let reason = if class_used.saturating_add(incoming_bytes) > quota {
            RefusalReason::ClassQuota
        } else {
            RefusalReason::AggregateBudget
        };
        let available = quota
            .saturating_sub(class_used)
            .min(total.saturating_sub(total_used));
        {
            let mut ledger = self.inner.ledger.lock().unwrap();
            ledger.record_refusal(
                StorageClass::SqliteWal,
                incoming_bytes,
                available,
                reason.as_str(),
                "SQLite/WAL growth refused before the append",
            )?;
        }
        let health = DegradedHealth {
            code: format!("storage_{}", reason.as_str()),
            detail: format!(
                "SQLite growth of {incoming_bytes} bytes refused ({} used of {} quota, pinned {pinned})",
                class_used, quota
            ),
            total_bytes: total_used,
            total_budget_bytes: total,
            required_bytes: pinned,
            class: Some(StorageClass::SqliteWal),
            at_ms: now_ms(),
        };
        self.record_degraded(health)?;
        Err(GovernorError::Refused(StorageRefusal {
            class: StorageClass::SqliteWal,
            requested_bytes: incoming_bytes,
            available_bytes: available,
            reason,
            detail: "SQLite/WAL growth refused before the append".into(),
        }))
    }

    /// Remove stale updater staging remnants. Fresh objects (an in-flight
    /// download) are preserved by the backend's minimum age.
    pub fn cleanup_update_remnants(&self, max_age_ms: i64) -> GovernorResult<u64> {
        self.record_maintenance()?;
        let Some(backend) = self.backend(StorageClass::UpdateRemnant) else {
            return Ok(0);
        };
        let objects = backend.scan(self)?;
        let now = now_ms();
        let mut removed = 0u64;
        for object in objects {
            if now.saturating_sub(object.last_used_ms) < max_age_ms {
                continue;
            }
            let row = object_row(StorageClass::UpdateRemnant, &object);
            removed += self.evict_row(&row)?;
        }
        Ok(removed)
    }

    pub fn loss_markers(
        &self,
        class: Option<StorageClass>,
    ) -> GovernorResult<Vec<(String, u64, String)>> {
        self.inner.ledger.lock().unwrap().loss_markers(class)
    }

    pub fn eviction_index(&self) -> GovernorResult<Vec<(String, String, u64, bool)>> {
        self.inner.ledger.lock().unwrap().eviction_index()
    }

    pub fn counters(&self) -> GovernorResult<Counters> {
        self.inner.ledger.lock().unwrap().counters()
    }

    /// Test/observability seam: number of bounded keyset candidate queries
    /// (constant per eviction batch, never one per object).
    pub fn eviction_query_count(&self) -> u64 {
        self.inner.eviction_queries.load(Ordering::Relaxed)
    }

    pub fn pending_reservations(&self) -> GovernorResult<u64> {
        self.inner.ledger.lock().unwrap().pending_count()
    }
}

enum Admit {
    Reserved(Reservation),
    Need {
        deficit: u64,
        available: u64,
        reason: RefusalReason,
    },
}

/// A durable reservation. Dropping without committing aborts it (best
/// effort); the ledger row is the crash-safe marker either way.
pub struct Reservation {
    governor: StorageGovernor,
    id: String,
    class: StorageClass,
    key: String,
    bytes: u64,
    active: bool,
}

impl std::fmt::Debug for Reservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reservation")
            .field("id", &self.id)
            .field("class", &self.class)
            .field("bytes", &self.bytes)
            .finish()
    }
}

impl Reservation {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn class(&self) -> StorageClass {
        self.class
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Commit the published bytes. `digest` is the CAS address when the
    /// object is content-addressed; the object index is keyed by it.
    pub fn commit(mut self, actual_bytes: u64, digest: Option<&str>) -> GovernorResult<()> {
        if actual_bytes > self.bytes {
            return Err(GovernorError::Malformed(format!(
                "reservation {} admitted {} bytes but {} were published; a caller may never \
                 exceed its own admission",
                self.id, self.bytes, actual_bytes
            )));
        }
        self.active = false;
        let committed = self
            .governor
            .inner
            .ledger
            .lock()
            .unwrap()
            .commit_reservation(&self.id, actual_bytes, digest, now_ms())?;
        if committed.is_none() {
            return Err(GovernorError::Corrupt(format!(
                "reservation {} vanished before commit",
                self.id
            )));
        }
        let total = self.governor.inner.ledger.lock().unwrap().sum_usage()?;
        if total > self.governor.inner.config.total_budget_bytes {
            let pinned = self.governor.inner.ledger.lock().unwrap().sum_pinned()?;
            self.governor.record_degraded(DegradedHealth {
                code: "aggregate_budget_exceeded".into(),
                detail: format!(
                    "commit of {} bytes made total usage {total} exceed the hard {} byte budget",
                    actual_bytes, self.governor.inner.config.total_budget_bytes
                ),
                total_bytes: total,
                total_budget_bytes: self.governor.inner.config.total_budget_bytes,
                required_bytes: pinned,
                class: Some(self.class),
                at_ms: now_ms(),
            })?;
        }
        Ok(())
    }

    pub fn abort(mut self) {
        self.active = false;
        let _ = self
            .governor
            .inner
            .ledger
            .lock()
            .unwrap()
            .delete_reservation(&self.id);
    }

    pub fn bytes_reserved(&self) -> u64 {
        self.bytes
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if self.active {
            if let Ok(mut ledger) = self.governor.inner.ledger.lock() {
                let _ = ledger.delete_reservation(&self.id);
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReconcileReport {
    pub scanned: u64,
    pub indexed: u64,
    pub removed: u64,
    pub expired_reservations: u64,
    pub missing_loss_markers: u64,
    pub total_bytes: u64,
}
