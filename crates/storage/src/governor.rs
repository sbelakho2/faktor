//! The always-on storage governor.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use serde::{Deserialize, Serialize};

use crate::class::StorageClass;
use crate::config::StorageGovernorConfig;
use crate::directory::{object_row, DirectoryBackend, SqliteBackend};
use crate::error::{GovernorError, GovernorResult, RefusalReason, StorageRefusal};
use crate::health::{ClassUsage, DegradedHealth, StorageMetrics};
use crate::ledger::{now_ms, Counters, DirectoryRootsMeta, Ledger, ObjectRow, UsageRow};
use crate::object::{EvictionOutcome, StorageBackend, StorageObject};

/// The governor's own durable directory name (never part of any budgeted
/// class: its ledger is bookkeeping, not governed payload).
pub const GOVERNOR_DIR_NAME: &str = "storage-governor";
pub const GOVERNOR_DB_NAME: &str = "governor.db";
pub const DEFAULT_UPDATE_REMNANT_STALE_MS: i64 = 60 * 60 * 1000;

/// Proactive watermark eviction leaves this much headroom below the watermark
/// (5% of the aggregate budget). Without it every write above the watermark
/// would evict exactly its own size — eviction thrash.
const WATERMARK_HYSTERESIS_PERMILLE: u64 = 50;

/// Conservative on-disk growth charged for one SQLite append: the payload can
/// land in the WAL AND extend the database file, and WAL frames carry page
/// overhead. Charging a bound (never less than the payload) keeps the hard
/// budget true even when the caller's estimate is optimistic.
fn sqlite_growth_bound(incoming_bytes: u64) -> u64 {
    incoming_bytes.saturating_mul(2).saturating_add(16 * 1024)
}

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
                // The native browser surface roots its manager at
                // `<data>/store/browser`, so profiles and downloads live one
                // level deeper than the historical commerce root.
                data_dir
                    .join("store")
                    .join("browser")
                    .join("commerce")
                    .join("profiles"),
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
    last_reconcile_ms: AtomicI64,
    last_enforce_ms: AtomicI64,
    /// Bytes admitted for SQLite/WAL growth since the last inventory scan,
    /// SHARED by every governor instance in this process that opens the same
    /// ledger path (one atomic per append, never a scan). Maintenance
    /// reconciles it with durable disk truth; `inflight` marks admissions
    /// whose write may still be between admission and commit, so a scan can
    /// never discount bytes that are not on disk yet.
    wal_debt: Arc<SharedWalDebt>,
    wal_scan_gate: Mutex<()>,
    maintenance_gate: Mutex<()>,
    sqlite_maintenance_runs: AtomicU64,
    family_index_loads: AtomicU64,
}

/// SQLite/WAL admission debt shared by all governor instances of one ledger.
#[derive(Default)]
struct SharedWalDebt {
    admitted: AtomicU64,
    inflight: AtomicU64,
}

fn shared_wal_debt(database: &Path) -> Arc<SharedWalDebt> {
    static REGISTRY: OnceLock<Mutex<BTreeMap<PathBuf, Arc<SharedWalDebt>>>> = OnceLock::new();
    let registry = REGISTRY.get_or_init(|| Mutex::new(BTreeMap::new()));
    registry
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .entry(database.to_path_buf())
        .or_insert_with(|| Arc::new(SharedWalDebt::default()))
        .clone()
}

/// One consistent `(admitted, inflight)` snapshot taken BEFORE a scan.
#[derive(Debug, Clone, Copy)]
struct WalDebtSnapshot {
    admitted: u64,
    inflight: u64,
}

/// One tracked SQLite growth admission. Dropping settles the in-flight half;
/// the admitted half stays until an inventory scan reconciles disk truth.
pub struct SqliteAdmission {
    governor: StorageGovernor,
    charged: u64,
    active: bool,
}

impl std::fmt::Debug for SqliteAdmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteAdmission")
            .field("charged", &self.charged)
            .finish()
    }
}

impl Drop for SqliteAdmission {
    fn drop(&mut self) {
        if self.active {
            settle_inflight(&self.governor.inner.wal_debt.inflight, self.charged);
        }
    }
}

fn settle_inflight(inflight: &AtomicU64, charged: u64) {
    let _ = inflight.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
        Some(current.saturating_sub(charged))
    });
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
        let mut ledger = Ledger::open(&root.join(GOVERNOR_DB_NAME))?;
        let stored = ledger.load_config()?;
        if stored.as_ref() != Some(&config) {
            ledger.store_config(&config)?;
        }
        // A new PROCESS never inherits a live reservation: a crash between
        // reserve and publish is recovered immediately instead of blocking
        // the budget for the whole lease. Same-process instances share the
        // boot id and keep each other's reservations.
        let _ = ledger.expire_foreign_reservations(crate::ledger::process_boot_id());
        Self::assemble(root, data_dir, config, ledger)
    }

    /// Open only when the governor database already exists (production CAS
    /// construction uses this so a bare test directory is not transformed
    /// into a governed installation).
    pub fn open_existing(data_dir: impl AsRef<Path>) -> Option<Self> {
        Self::open_existing_result(data_dir).ok().flatten()
    }

    /// Fallible form of [`Self::open_existing`]: a present-but-corrupt
    /// governor is a loud error, never silently "not initialized" (which
    /// would let a host run ungoverned or make doctor lie).
    pub fn open_existing_result(data_dir: impl AsRef<Path>) -> GovernorResult<Option<Self>> {
        let data_dir = data_dir.as_ref();
        let root = Self::governor_dir(data_dir);
        let path = root.join(GOVERNOR_DB_NAME);
        if !path.is_file() {
            return Ok(None);
        }
        let ledger = Ledger::open(&path)?;
        let config = ledger
            .load_config()?
            .unwrap_or_default()
            .validated()
            .map_err(GovernorError::Corrupt)?;
        Self::assemble(root, data_dir.to_path_buf(), config, ledger).map(Some)
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
        let last_reconcile_ms = ledger
            .meta("last_reconcile_ms")
            .ok()
            .flatten()
            .and_then(|raw| raw.parse::<i64>().ok())
            .unwrap_or_else(now_ms);
        let last_enforce_ms = ledger
            .meta("last_enforce_ms")
            .ok()
            .flatten()
            .and_then(|raw| raw.parse::<i64>().ok())
            .unwrap_or_else(now_ms);
        let wal_debt = shared_wal_debt(&root.join(GOVERNOR_DB_NAME));
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
                last_reconcile_ms: AtomicI64::new(last_reconcile_ms),
                last_enforce_ms: AtomicI64::new(last_enforce_ms),
                wal_debt,
                wal_scan_gate: Mutex::new(()),
                maintenance_gate: Mutex::new(()),
                sqlite_maintenance_runs: AtomicU64::new(0),
                family_index_loads: AtomicU64::new(0),
            }),
        };
        governor.install_default_backends(&GovernorRoots::under(data_dir));
        governor.sync_directory_roots()?;
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

    /// Add another directory root to an existing class (merge, never replace).
    /// The daemon uses this for an operator-configured updater install root so
    /// the default staging roots stay governed too.
    pub fn add_directory_root(
        &self,
        class: StorageClass,
        root: impl Into<PathBuf>,
        min_age_ms: i64,
    ) -> GovernorResult<()> {
        let root = root.into();
        let mut backends = self.inner.backends.write().unwrap();
        let mut roots = Vec::new();
        let mut age = min_age_ms;
        if let Some(existing) = backends.get(&class) {
            if let Some((old_roots, old_age)) = existing.directory_roots() {
                roots.extend_from_slice(old_roots);
                age = age.max(old_age);
            }
        }
        {
            let ledger = self.inner.ledger.lock().unwrap();
            if let Some(meta) = ledger.directory_roots(class)? {
                for persisted in meta.roots {
                    if !roots.contains(&persisted) {
                        roots.push(persisted);
                    }
                }
                age = age.max(meta.min_age_ms);
            }
        }
        if !roots.contains(&root) {
            roots.push(root);
        }
        backends.insert(
            class,
            Arc::new(DirectoryBackend::with_roots(class, roots.clone()).with_min_age(age)),
        );
        self.inner.ledger.lock().unwrap().store_directory_roots(
            class,
            &DirectoryRootsMeta {
                roots,
                min_age_ms: age,
            },
        )?;
        Ok(())
    }

    /// The CANONICAL key the backend registered for `class` assigns to a
    /// file at `root/rel`, or `None` when no backend of this class owns
    /// `root`.
    ///
    /// A writer that publishes into a governed directory must use this key
    /// for its reservation and commit: on a multi-root backend the key is
    /// `<root-index>:<rel>`, so a bare root-relative path would index an
    /// object that `scan` never reports (orphaned row, unevictable bytes).
    pub fn directory_key(&self, class: StorageClass, root: &Path, rel: &Path) -> Option<String> {
        let backends = self.inner.backends.read().ok()?;
        let backend = backends.get(&class)?;
        backend.canonical_directory_key(root, rel)
    }

    /// Merge every instance's registered directory roots from the durable
    /// ledger into this instance's backends. The ROOTS of a class are shared
    /// state: a download manager rooted at `<data>/store/browser/...` and the
    /// daemon's default roots must both scan the union, or one instance's
    /// reconcile would erase inventory indexed by the other.
    pub fn sync_directory_roots(&self) -> GovernorResult<()> {
        let mut backends = self.inner.backends.write().unwrap();
        for class in [
            StorageClass::BrowserStorage,
            StorageClass::UpdateRemnant,
            StorageClass::RebuildableCache,
        ] {
            let mut roots = Vec::new();
            let mut age = 0i64;
            if let Some(existing) = backends.get(&class) {
                match existing.directory_roots() {
                    Some((old_roots, old_age)) => {
                        roots.extend_from_slice(old_roots);
                        age = age.max(old_age);
                    }
                    // A custom non-directory backend owns this class in this
                    // instance: never clobber it with the persisted roots.
                    None => continue,
                }
            }
            {
                let ledger = self.inner.ledger.lock().unwrap();
                if let Some(meta) = ledger.directory_roots(class)? {
                    for persisted in meta.roots {
                        if !roots.contains(&persisted) {
                            roots.push(persisted);
                        }
                    }
                    age = age.max(meta.min_age_ms);
                }
            }
            if roots.is_empty() {
                continue;
            }
            backends.insert(
                class,
                Arc::new(DirectoryBackend::with_roots(class, roots).with_min_age(age)),
            );
        }
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
    ) -> GovernorResult<bool> {
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
        Ok(inserted)
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
        // Always-on tick: directory classes (profiles/caches/agent homes) grow
        // outside reservations, so they are re-indexed and enforced on a
        // bounded cadence. Non-blocking when another thread is already
        // maintaining.
        let _ = self.maintain_if_due(300_000, 30_000);
        let mut last_available = 0u64;
        let mut last_reason = RefusalReason::NoEvictableObjects;
        // At most three admission rounds: each round either admits, or makes
        // one bounded eviction pass. The old four-round loop could evict
        // unrelated classes against a pure class-quota deficit and thrash.
        for _attempt in 0..3 {
            match self.try_reserve(class, key, bytes)? {
                Admit::Reserved(reservation) => {
                    self.maintain_watermark()?;
                    self.clear_degraded_if_healthy()?;
                    return Ok(reservation);
                }
                Admit::Need(need) => {
                    last_available = need.available;
                    last_reason = need.reason();
                    let mut freed = 0u64;
                    if need.class_deficit > 0 {
                        // Class pressure is satisfied ONLY from that class:
                        // evicting a different class cannot make the class
                        // quota true and destroys unrelated cold data.
                        freed = self.evict_class_for(class, need.class_deficit)?;
                    }
                    if need.class_deficit == 0 || freed >= need.class_deficit {
                        let remaining = need.total_deficit.saturating_sub(freed);
                        if remaining > 0 {
                            freed = freed.saturating_add(self.evict_global_for(remaining)?);
                        }
                    }
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
        let lease = self.inner.config.reservation_lease_ms.max(1);
        let quota = self.inner.config.quota(class);
        let total = self.inner.config.total_budget_bytes;
        let row = {
            let mut ledger = self.inner.ledger.lock().unwrap();
            ledger.try_reserve_atomic(
                class,
                key,
                bytes,
                now,
                now.saturating_add(lease),
                quota,
                total,
            )?
        };
        let Some(id) = row.reservation_id else {
            let available = quota
                .saturating_sub(row.class_used)
                .min(total.saturating_sub(row.total_used));
            return Ok(Admit::Need(Need {
                class_deficit: row.class_deficit,
                total_deficit: row.total_deficit,
                available,
                class_pinned: row.snapshot.class_pinned,
                total_pinned: row.snapshot.total_pinned,
                has_backend: self.backend(class).is_some(),
                class_bytes: bytes,
                quota,
                total,
            }));
        };
        Ok(Admit::Reserved(Reservation {
            governor: self.clone(),
            id,
            class,
            key: key.to_string(),
            bytes,
            active: true,
        }))
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

    /// Evict inside ONE class, oldest first. The keyset cursor advances past
    /// every inspected object (even one the backend refused) so a pass is
    /// O(candidates) and can never rescan the same head forever.
    fn evict_class_for(&self, class: StorageClass, target: u64) -> GovernorResult<u64> {
        if target == 0 {
            return Ok(0);
        }
        let mut cursor: Option<(i64, String)> = None;
        let mut freed = 0u64;
        while freed < target {
            let page = {
                let ledger = self.inner.ledger.lock().unwrap();
                self.inner.eviction_queries.fetch_add(1, Ordering::Relaxed);
                match &cursor {
                    None => ledger.candidates_page(class, (i64::MIN, ""), 64)?,
                    Some((last_used, key)) => {
                        ledger.candidates_page(class, (*last_used, key.as_str()), 64)?
                    }
                }
            };
            if page.is_empty() {
                break;
            }
            let page_len = page.len();
            for object in &page {
                if freed >= target {
                    break;
                }
                freed = freed.saturating_add(self.evict_row_soft(object)?);
                cursor = Some((object.last_used_ms, object.key.clone()));
            }
            if page_len < 64 {
                break;
            }
        }
        Ok(freed)
    }

    /// Evict globally (oldest first across classes) for aggregate pressure.
    fn evict_global_for(&self, target: u64) -> GovernorResult<u64> {
        if target == 0 {
            return Ok(0);
        }
        let mut cursor: Option<(i64, String, String)> = None;
        let mut freed = 0u64;
        while freed < target {
            let page = {
                let ledger = self.inner.ledger.lock().unwrap();
                self.inner.eviction_queries.fetch_add(1, Ordering::Relaxed);
                match &cursor {
                    None => ledger.global_candidates_page((i64::MIN, "", ""), 64)?,
                    Some((last_used, class, key)) => ledger
                        .global_candidates_page((*last_used, class.as_str(), key.as_str()), 64)?,
                }
            };
            if page.is_empty() {
                break;
            }
            let page_len = page.len();
            for object in &page {
                if freed >= target {
                    break;
                }
                freed = freed.saturating_add(self.evict_row_soft(object)?);
                cursor = Some((
                    object.last_used_ms,
                    object.class.as_str().to_string(),
                    object.key.clone(),
                ));
            }
            if page_len < 64 {
                break;
            }
        }
        Ok(freed)
    }

    /// Eviction-loop variant: one unverifiable/corrupt object must not abort
    /// the whole pass (which would leave the hard budget unmet and later
    /// objects unevicted). It is recorded as a typed backend refusal and the
    /// next candidate is tried. Explicit `evict_object` still returns the
    /// loud error to its caller.
    fn evict_row_soft(&self, object: &ObjectRow) -> GovernorResult<u64> {
        match self.evict_row(object) {
            Ok(freed) => Ok(freed),
            Err(error) => {
                self.inner.ledger.lock().unwrap().record_refusal(
                    object.class,
                    object.bytes,
                    0,
                    RefusalReason::BackendRefused.as_str(),
                    &format!("eviction of {} refused: {error}", object.key),
                )?;
                Ok(0)
            }
        }
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
                ledger.remove_object_and_record_eviction(
                    object.class,
                    &object.key,
                    digest.as_deref().or(object.digest.as_deref()),
                    bytes,
                    verified,
                    "governed_eviction",
                )?;
                Ok(bytes)
            }
            EvictionOutcome::Reclaimed {
                bytes,
                remaining_bytes,
                verified,
            } => {
                let mut ledger = self.inner.ledger.lock().unwrap();
                ledger.resize_object_and_record_eviction(
                    object.class,
                    &object.key,
                    object.digest.as_deref(),
                    bytes,
                    remaining_bytes,
                    verified,
                    "governed_truncation",
                )?;
                Ok(bytes)
            }
            EvictionOutcome::Absent => {
                let mut ledger = self.inner.ledger.lock().unwrap();
                let removed = ledger.remove_object(object.class, &object.key)?;
                // The accounting row was phantom (already absent on disk);
                // reclaiming it creates real admission room.
                Ok(removed.map(|row| row.bytes).unwrap_or(0))
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
        let now = now_ms();
        self.inner.last_enforce_ms.store(now, Ordering::Relaxed);
        let _ = self
            .inner
            .ledger
            .lock()
            .unwrap()
            .set_meta("last_enforce_ms", &now.to_string());
        let mut freed = 0u64;
        let usage = self.inner.ledger.lock().unwrap().usage()?;
        for class in StorageClass::ALL {
            let used = usage.get(&class).map(|row| row.bytes).unwrap_or(0);
            let quota = self.inner.config.quota(class);
            if used > quota {
                freed += self.evict_class_for(class, used - quota)?;
            }
        }
        let total_used = self.inner.ledger.lock().unwrap().sum_usage()?;
        if total_used > self.inner.config.total_budget_bytes {
            freed += self.evict_global_for(total_used - self.inner.config.total_budget_bytes)?;
        }
        self.clear_degraded_if_healthy()?;
        self.evaluate_degraded()?;
        Ok(freed)
    }

    /// Enforce ONE class quota without walking every class or the aggregate.
    /// Used on a dedup promotion: the bytes only move between class quotas,
    /// so charging a full `enforce()` per promoted CAS chunk would be O(n²)
    /// on an archive flood.
    pub fn enforce_class(&self, class: StorageClass) -> GovernorResult<u64> {
        let used = self
            .inner
            .ledger
            .lock()
            .unwrap()
            .usage()?
            .get(&class)
            .map(|row| row.bytes)
            .unwrap_or(0);
        let quota = self.inner.config.quota(class);
        if used > quota {
            let freed = self.evict_class_for(class, used - quota)?;
            self.clear_degraded_if_healthy()?;
            self.evaluate_degraded()?;
            Ok(freed)
        } else {
            Ok(0)
        }
    }

    /// Full reconciliation: expire crashed reservations, scan every registered
    /// backend, rebuild the object index/usage from durable disk state, and
    /// record archive objects that vanished without an explicit eviction.
    pub fn reconcile(&self) -> GovernorResult<ReconcileReport> {
        self.sync_directory_roots()?;
        // Serialize the WAL scan accounting: two reconciles must never both
        // discount the same admitted bytes against their own scan.
        let _wal_gate = self
            .inner
            .wal_scan_gate
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let scan_started_ms = now_ms();
        let debt = self.wal_debt_snapshot();
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
        let sqlite_scanned = !scan_errors.contains_key(&StorageClass::SqliteWal);
        let mut report = ReconcileReport::default();
        {
            let mut ledger = self.inner.ledger.lock().unwrap();
            report.expired_reservations = ledger.expire_reservations(now_ms())?;
            for (class, objects) in by_class {
                let change = ledger.set_inventory_scanned_at(class, &objects, scan_started_ms)?;
                report.scanned += change.scanned;
                report.indexed += change.inserted;
                report.removed += change.removed;
                report.missing_loss_markers += change.missing_markers;
            }
            report.total_bytes = ledger.sum_usage()?;
        }
        if sqlite_scanned {
            // Disk truth now includes every admission whose write completed
            // before this scan began. Admissions still in flight, and every
            // admission made after the scan start, stay charged.
            self.finish_wal_scan(debt);
        }
        *self.inner.scan_errors.lock().unwrap() = scan_errors;
        self.record_maintenance()?;
        let now = now_ms();
        self.inner.last_reconcile_ms.store(now, Ordering::Relaxed);
        let _ = self
            .inner
            .ledger
            .lock()
            .unwrap()
            .set_meta("last_reconcile_ms", &now.to_string());
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

    /// The whole digest index of one physical root family in one query (CAS
    /// reconciliation classifies in memory instead of N point queries).
    pub fn family_index(
        &self,
        family: &str,
    ) -> GovernorResult<BTreeMap<String, (StorageClass, i64)>> {
        self.inner
            .family_index_loads
            .fetch_add(1, Ordering::Relaxed);
        self.inner.ledger.lock().unwrap().family_index(family)
    }

    /// Reconcile only when the last full scan is older than `max_age_ms`.
    /// Startup uses this so several CAS roots attaching to one data dir do
    /// not each re-walk the whole disk after the daemon already reconciled.
    pub fn reconcile_if_stale(&self, max_age_ms: i64) -> GovernorResult<bool> {
        let now = now_ms();
        let last = self.inner.last_reconcile_ms.load(Ordering::Relaxed);
        if now.saturating_sub(last) < max_age_ms {
            return Ok(false);
        }
        self.reconcile()?;
        Ok(true)
    }

    /// Always-on maintenance tick: full reconcile at most every
    /// `reconcile_interval_ms`, cheap quota/budget enforcement at most every
    /// `enforce_interval_ms`. Non-blocking: a tick already running on another
    /// thread makes this a no-op, so user writes never queue behind a scan.
    pub fn maintain_if_due(
        &self,
        reconcile_interval_ms: i64,
        enforce_interval_ms: i64,
    ) -> GovernorResult<bool> {
        let now = now_ms();
        let reconcile_due = now
            .saturating_sub(self.inner.last_reconcile_ms.load(Ordering::Relaxed))
            >= reconcile_interval_ms;
        let enforce_due = now.saturating_sub(self.inner.last_enforce_ms.load(Ordering::Relaxed))
            >= enforce_interval_ms;
        if !reconcile_due && !enforce_due {
            return Ok(false);
        }
        let Ok(_guard) = self.inner.maintenance_gate.try_lock() else {
            return Ok(false);
        };
        let now = now_ms();
        if now.saturating_sub(self.inner.last_reconcile_ms.load(Ordering::Relaxed))
            >= reconcile_interval_ms
        {
            self.reconcile()?;
            self.enforce()?;
            return Ok(true);
        }
        if now.saturating_sub(self.inner.last_enforce_ms.load(Ordering::Relaxed))
            >= enforce_interval_ms
        {
            self.enforce()?;
            return Ok(true);
        }
        Ok(false)
    }

    /// Keep aggregate usage under the configured watermark by evicting the
    /// oldest evictable cold objects. The watermark is a documented proactive
    /// policy, not a refusal threshold: the hard budget remains the limit.
    fn maintain_watermark(&self) -> GovernorResult<()> {
        let total = self.inner.ledger.lock().unwrap().sum_usage()?;
        let floor = self
            .inner
            .config
            .total_budget_bytes
            .saturating_mul(self.inner.config.watermark_permille as u64)
            / 1000;
        if total > floor {
            let low = floor.saturating_sub(
                self.inner
                    .config
                    .total_budget_bytes
                    .saturating_mul(WATERMARK_HYSTERESIS_PERMILLE)
                    / 1000,
            );
            let _ = self.evict_global_for(total - low)?;
        }
        Ok(())
    }

    /// Bytes admitted for SQLite/WAL growth that durable inventory has not
    /// reconciled yet (shared by every governor instance of this ledger).
    /// Doctor/test seam; never a substitute for the per-class pending
    /// reservations, which cover CAS publish windows.
    pub fn sqlite_admitted_bytes(&self) -> u64 {
        self.inner.wal_debt.admitted.load(Ordering::Acquire)
    }

    /// Test/observability seam: how many full SQLite/WAL maintenance scans ran.
    pub fn sqlite_maintenance_runs(&self) -> u64 {
        self.inner.sqlite_maintenance_runs.load(Ordering::Relaxed)
    }

    /// Test/observability seam: how many family-index loads CAS reconciliation
    /// performed (one per scanned root, never one per blob).
    pub fn family_index_loads(&self) -> u64 {
        self.inner.family_index_loads.load(Ordering::Relaxed)
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
    /// class quota. Returns the bytes reclaimed. This is the explicit/heavy
    /// tick: the hot append path only reaches it under quota pressure or age,
    /// never once per write.
    pub fn sqlite_maintenance(&self) -> GovernorResult<u64> {
        self.record_maintenance()?;
        let _guard = self
            .inner
            .maintenance_gate
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let _wal_gate = self
            .inner
            .wal_scan_gate
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        self.inner
            .sqlite_maintenance_runs
            .fetch_add(1, Ordering::Relaxed);
        let Some(backend) = self.backend(StorageClass::SqliteWal) else {
            // No backend means no disk truth: keep the admitted debt rather
            // than pretending a scan happened.
            return Ok(0);
        };
        let scan_started_ms = now_ms();
        let debt = self.wal_debt_snapshot();
        let objects = backend.scan(self)?;
        let rows: Vec<ObjectRow> = objects
            .iter()
            .map(|object| object_row(StorageClass::SqliteWal, object))
            .collect();
        let before = {
            let mut ledger = self.inner.ledger.lock().unwrap();
            let before = ledger
                .usage()?
                .get(&StorageClass::SqliteWal)
                .map(|row| row.bytes)
                .unwrap_or(0);
            let _ =
                ledger.set_inventory_scanned_at(StorageClass::SqliteWal, &rows, scan_started_ms)?;
            before
        };
        // Truncate every WAL: this is non-blocking (busy_timeout 0) and a
        // reader pinning frames simply leaves the file for the next tick.
        // A checkpoint that cannot shrink is a REFUSAL, never a fake
        // eviction; a partial shrink stays indexed with its remaining bytes.
        for object in &rows {
            if object.key.ends_with(".db-wal") {
                let _ = self.evict_row(object);
            }
        }
        // A successful checkpoint folds WAL frames into the database file, so
        // the pre-checkpoint size snapshot is stale. Re-scan disk truth
        // before reporting usage or reclaimed bytes.
        let second_started_ms = now_ms();
        match backend.scan(self) {
            Ok(objects) => {
                let rows: Vec<ObjectRow> = objects
                    .iter()
                    .map(|object| object_row(StorageClass::SqliteWal, object))
                    .collect();
                let mut ledger = self.inner.ledger.lock().unwrap();
                ledger.set_inventory_scanned_at(
                    StorageClass::SqliteWal,
                    &rows,
                    second_started_ms,
                )?;
            }
            Err(error) => {
                self.inner
                    .scan_errors
                    .lock()
                    .unwrap()
                    .insert(StorageClass::SqliteWal, error.to_string());
                return Err(error);
            }
        }
        self.finish_wal_scan(debt);
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
            self.evict_class_for(StorageClass::SqliteWal, used - quota)?;
        }
        self.inner
            .last_sqlite_maintenance_ms
            .store(now_ms(), Ordering::Relaxed);
        self.clear_degraded_if_healthy()?;
        self.evaluate_degraded()?;
        let after = self
            .inner
            .ledger
            .lock()
            .unwrap()
            .usage()?
            .get(&StorageClass::SqliteWal)
            .map(|row| row.bytes)
            .unwrap_or(0);
        Ok(before.saturating_sub(after))
    }

    fn wal_debt_snapshot(&self) -> WalDebtSnapshot {
        WalDebtSnapshot {
            admitted: self.inner.wal_debt.admitted.load(Ordering::Acquire),
            inflight: self.inner.wal_debt.inflight.load(Ordering::Acquire),
        }
    }

    /// A scan that started after `snapshot` observed every admission whose
    /// write completed before it began. Keep the in-flight and post-start
    /// debt; never zero a counter and ignore a concurrent writer.
    fn finish_wal_scan(&self, snapshot: WalDebtSnapshot) {
        let completed = snapshot.admitted.saturating_sub(snapshot.inflight);
        if completed == 0 {
            return;
        }
        let _ = self.inner.wal_debt.admitted.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |current| Some(current.saturating_sub(completed)),
        );
    }

    /// Attempt one SQLite growth admission against durable usage, pending
    /// reservations and the bytes admitted since the last inventory scan.
    fn sqlite_room(
        &self,
        incoming_bytes: u64,
        quota: u64,
        total: u64,
    ) -> GovernorResult<Option<(u64, u64, u64)>> {
        let bound = sqlite_growth_bound(incoming_bytes);
        let now = now_ms();
        let snapshot = self
            .inner
            .ledger
            .lock()
            .unwrap()
            .budget_snapshot(StorageClass::SqliteWal, now)?;
        let debt = self.inner.wal_debt.admitted.load(Ordering::Acquire);
        let class_used = snapshot
            .class_bytes
            .saturating_add(snapshot.class_pending)
            .saturating_add(debt);
        let total_used = snapshot
            .total_bytes
            .saturating_add(snapshot.total_pending)
            .saturating_add(debt);
        if class_used.saturating_add(bound) <= quota && total_used.saturating_add(bound) <= total {
            return Ok(Some((class_used, total_used, snapshot.total_pinned)));
        }
        Ok(None)
    }

    fn charge_sqlite_growth(
        &self,
        incoming_bytes: u64,
        tracked: bool,
    ) -> GovernorResult<Option<SqliteAdmission>> {
        let bound = sqlite_growth_bound(incoming_bytes);
        let quota = self.inner.config.quota(StorageClass::SqliteWal);
        let total = self.inner.config.total_budget_bytes;
        loop {
            if self.sqlite_room(incoming_bytes, quota, total)?.is_none() {
                break;
            }
            let debt = self.inner.wal_debt.admitted.load(Ordering::Acquire);
            // Mark in-flight BEFORE the admitted CAS: a scan that snaps
            // between the two must not treat this admission as completed.
            if tracked {
                self.inner
                    .wal_debt
                    .inflight
                    .fetch_add(bound, Ordering::AcqRel);
            }
            if self
                .inner
                .wal_debt
                .admitted
                .compare_exchange(
                    debt,
                    debt.saturating_add(bound),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                return Ok(Some(SqliteAdmission {
                    governor: self.clone(),
                    charged: if tracked { bound } else { 0 },
                    active: tracked,
                }));
            }
            if tracked {
                settle_inflight(&self.inner.wal_debt.inflight, bound);
            }
        }
        Ok(None)
    }

    /// Growth refusal for SQLite: admit against durable usage plus the
    /// shared admitted-but-unsettled debt, run WAL maintenance only under
    /// pressure/age, and refuse the incoming append when the class quota or
    /// aggregate budget cannot hold it. The caller must not execute the write
    /// on `Err`.
    pub fn admit_sqlite_growth(&self, incoming_bytes: u64) -> GovernorResult<()> {
        self.admit_sqlite_growth_inner(incoming_bytes, false)
            .map(|_| ())
    }

    /// Tracked variant for the store writer: the returned guard keeps the
    /// admission marked in-flight for exactly the life of the write, so a
    /// concurrent maintenance scan can never discount bytes that are not on
    /// disk yet.
    pub fn admit_sqlite_growth_tracked(
        &self,
        incoming_bytes: u64,
    ) -> GovernorResult<SqliteAdmission> {
        self.admit_sqlite_growth_inner(incoming_bytes, true)
            .map(|admission| {
                admission.unwrap_or_else(|| SqliteAdmission {
                    governor: self.clone(),
                    charged: 0,
                    active: false,
                })
            })
    }

    fn admit_sqlite_growth_inner(
        &self,
        incoming_bytes: u64,
        tracked: bool,
    ) -> GovernorResult<Option<SqliteAdmission>> {
        if incoming_bytes == 0 {
            return Ok(if tracked {
                Some(SqliteAdmission {
                    governor: self.clone(),
                    charged: 0,
                    active: false,
                })
            } else {
                None
            });
        }
        // Run the bounded directory-class tick BEFORE charging this append,
        // so a reconcile reset of the unsettled counter cannot erase the
        // admission of a write that is already in flight.
        let _ = self.maintain_if_due(300_000, 30_000);
        if let Some(admission) = self.charge_sqlite_growth(incoming_bytes, tracked)? {
            return Ok(Some(admission));
        }
        // Pressure: at most one WAL maintenance per second; an explicit call
        // is never throttled. After a successful scan the debt is reset and
        // admission is retried exactly once.
        let now = now_ms();
        let last = self
            .inner
            .last_sqlite_maintenance_ms
            .load(Ordering::Relaxed);
        if now.saturating_sub(last) >= 1_000 {
            let _ = self.sqlite_maintenance();
            if let Some(admission) = self.charge_sqlite_growth(incoming_bytes, tracked)? {
                return Ok(Some(admission));
            }
        }
        let now = now_ms();
        let snapshot = self
            .inner
            .ledger
            .lock()
            .unwrap()
            .budget_snapshot(StorageClass::SqliteWal, now)?;
        let quota = self.inner.config.quota(StorageClass::SqliteWal);
        let total = self.inner.config.total_budget_bytes;
        let debt = self.inner.wal_debt.admitted.load(Ordering::Acquire);
        let class_used = snapshot
            .class_bytes
            .saturating_add(snapshot.class_pending)
            .saturating_add(debt);
        let total_used = snapshot
            .total_bytes
            .saturating_add(snapshot.total_pending)
            .saturating_add(debt);
        let pinned = snapshot.total_pinned;
        let bound = sqlite_growth_bound(incoming_bytes);
        let reason = if class_used.saturating_add(bound) > quota {
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
    Need(Need),
}

struct Need {
    class_deficit: u64,
    total_deficit: u64,
    available: u64,
    class_pinned: u64,
    total_pinned: u64,
    has_backend: bool,
    class_bytes: u64,
    quota: u64,
    total: u64,
}

impl Need {
    fn reason(&self) -> RefusalReason {
        if self.class_deficit > 0 && self.class_pinned.saturating_add(self.class_bytes) > self.quota
        {
            return RefusalReason::RequiredBytes;
        }
        if self.total_deficit > 0 && self.total_pinned.saturating_add(self.class_bytes) > self.total
        {
            return RefusalReason::RequiredBytes;
        }
        if self.class_deficit > 0 && !self.has_backend {
            return RefusalReason::NoBackend;
        }
        RefusalReason::NoEvictableObjects
    }
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
            // The lease expired while the publisher was still working (or a
            // concurrent reconcile removed the row). The bytes are already
            // durable, so accounting must adopt them instead of pretending
            // they do not exist. If that pushes usage over the hard budget,
            // health is degraded AND an eviction pass runs immediately.
            let key = digest.unwrap_or(&self.key);
            self.governor.inner.ledger.lock().unwrap().upsert_object(
                self.class,
                key,
                actual_bytes,
                digest,
                now_ms(),
                false,
            )?;
            let total = self.governor.inner.ledger.lock().unwrap().sum_usage()?;
            if total > self.governor.inner.config.total_budget_bytes {
                let pinned = self.governor.inner.ledger.lock().unwrap().sum_pinned()?;
                self.governor.record_degraded(DegradedHealth {
                    code: "reservation_lease_expired_over_budget".into(),
                    detail: format!(
                        "publish of {} bytes committed after its reservation expired and took total usage to {total} (hard {} bytes)",
                        actual_bytes, self.governor.inner.config.total_budget_bytes
                    ),
                    total_bytes: total,
                    total_budget_bytes: self.governor.inner.config.total_budget_bytes,
                    required_bytes: pinned,
                    class: Some(self.class),
                    at_ms: now_ms(),
                })?;
                let _ = self.governor.enforce();
            }
            return Ok(());
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
