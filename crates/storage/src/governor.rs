//! The always-on storage governor.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use serde::{Deserialize, Serialize};

use crate::capacity::{
    volume_admission_deficit, volume_reserve_bytes, CapacityProbe, CapacitySnapshot,
    SystemCapacityProbe,
};
use crate::class::StorageClass;
use crate::config::StorageGovernorConfig;
use crate::directory::{object_row, DirectoryBackend, SqliteBackend};
use crate::error::{GovernorError, GovernorResult, RefusalReason, StorageRefusal};
use crate::health::{ClassUsage, DegradedHealth, StorageMetrics};
use crate::ledger::{
    now_ms, Counters, DirectoryRootsMeta, HistoryPruneReport, Ledger, ObjectRow, UsageRow,
    VolumeDenial, VolumeRule, HISTORY_DETAIL_KEEP,
};
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

/// Bounded metadata/temp growth charged to the destination volume's
/// free-space check for ONE non-SQLite write: the reservation insert and the
/// commit upsert travel through the governor ledger's SQLite WAL (a frame per
/// statement plus row bytes), and directory publishes carry dirent/page
/// overhead while their temp file is staged. 16 KiB — four default SQLite
/// frames — is comfortably above the measured per-admission ledger growth and
/// stays negligible next to any payload worth admitting. SQLite/WAL class
/// appends use [`sqlite_growth_bound`] instead (the payload itself can land
/// twice on disk: WAL + database).
pub const RESERVATION_METADATA_ALLOWANCE_BYTES: u64 = 16 * 1024;

/// The free-space write bound for one admitted write of `bytes` on `class`:
/// payload plus bounded metadata/WAL/temp growth. This is what a destination
/// volume must still hold above its reserve before the write may start.
fn free_space_write_bound(class: StorageClass, bytes: u64) -> u64 {
    if class == StorageClass::SqliteWal {
        sqlite_growth_bound(bytes)
    } else {
        bytes.saturating_add(RESERVATION_METADATA_ALLOWANCE_BYTES)
    }
}

/// Resolved roots for the classes that are plain directories.
#[derive(Debug, Clone)]
pub struct GovernorRoots {
    pub data_dir: PathBuf,
    pub browser_profiles: Vec<PathBuf>,
    pub terminal_agent_home: PathBuf,
    pub update_staging: Vec<PathBuf>,
    pub rebuildable_caches: Vec<PathBuf>,
    /// Daemon-owned shadow mutation workspaces (`<data-dir>/shadows`): one
    /// governed directory class covering every `<session>/<shadow-id>`.
    pub shadows_root: PathBuf,
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
            shadows_root: data_dir.join("shadows"),
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
    capacity_probe: Arc<dyn CapacityProbe>,
    /// Extra roots (CAS roots register themselves) measured in addition to
    /// every directory backend root and the data directory.
    capacity_roots: RwLock<std::collections::BTreeSet<PathBuf>>,
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

/// WHY bytes are being admitted: which aggregate-band ceiling the admission
/// competes under. See [`crate::config`] for the documented three-band
/// split (normal ≤85%, emergency/critical metadata ≤90%, recovery-only
/// above). No OS quota is enforced for any band — none is portable
/// cross-platform; the bands are an admission policy over measured
/// physical usage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionPurpose {
    /// Ordinary class admissions (CAS publishes, caches, logs, evidence,
    /// shadow copies, normal task mutations).
    Normal,
    /// Growth the daemon itself must record to stay recoverable:
    /// SQLite/WAL growth of the governed ledgers.
    EmergencyMetadata,
    /// Explicit recovery-path writes (rollback/finalize/repair): the only
    /// admissions allowed into the 90–100% recovery reserve.
    Recovery,
}

impl AdmissionPurpose {
    /// The aggregate-band ceiling in bytes this purpose may reach.
    pub fn ceiling_bytes(self, config: &StorageGovernorConfig, effective_budget: u64) -> u64 {
        match self {
            AdmissionPurpose::Normal => config.normal_admission_ceiling_bytes(effective_budget),
            AdmissionPurpose::EmergencyMetadata => config.emergency_ceiling_bytes(effective_budget),
            AdmissionPurpose::Recovery => effective_budget,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            AdmissionPurpose::Normal => "normal",
            AdmissionPurpose::EmergencyMetadata => "emergency_metadata",
            AdmissionPurpose::Recovery => "recovery",
        }
    }
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
        Self::open_with_capacity_probe(data_dir, config, Arc::new(SystemCapacityProbe))
    }

    /// Open with an injected capacity probe (deterministic low-capacity /
    /// low-free tests, and hosts that already know their backing volume).
    pub fn open_with_capacity_probe(
        data_dir: impl Into<PathBuf>,
        config: StorageGovernorConfig,
        capacity_probe: Arc<dyn CapacityProbe>,
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
        Self::assemble(root, data_dir, config, ledger, capacity_probe)
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
        Self::assemble(
            root,
            data_dir.to_path_buf(),
            config,
            ledger,
            Arc::new(SystemCapacityProbe),
        )
        .map(Some)
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
        capacity_probe: Arc<dyn CapacityProbe>,
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
                capacity_probe,
                capacity_roots: RwLock::new(std::collections::BTreeSet::new()),
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
        // Shadow workspaces are governed from their FIRST byte: the scan
        // counts every physical file under the shadow root (build outputs
        // included — they are excluded from source manifests, never from the
        // physical budget). The class is non-evictable (live run workspaces;
        // see [`StorageClass::is_evictable`]).
        let _ = self.register_backend(Arc::new(DirectoryBackend::with_roots(
            StorageClass::ShadowWorkspace,
            vec![roots.shadows_root.clone()],
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

    /// Register another backing root (a CAS root outside `data_dir`) whose
    /// filesystem must be measured. Roots on the same volume are deduplicated
    /// by filesystem identity, never double counted.
    pub fn register_capacity_root(&self, root: impl Into<PathBuf>) {
        self.inner
            .capacity_roots
            .write()
            .unwrap()
            .insert(root.into());
    }

    /// The current capacity-aware budget. Free space is measured on every
    /// call so admission follows actual disk headroom, not only the nominal
    /// quota.
    pub fn capacity_snapshot(&self) -> CapacitySnapshot {
        let mut paths: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
        paths.insert(self.inner.data_dir.clone());
        paths.insert(self.inner.root.clone());
        for root in self.inner.capacity_roots.read().unwrap().iter() {
            paths.insert(root.clone());
        }
        {
            let backends = self.inner.backends.read().unwrap();
            for backend in backends.values() {
                if let Some((roots, _)) = backend.directory_roots() {
                    for root in roots {
                        paths.insert(root.clone());
                    }
                }
            }
        }
        let spaces = paths
            .iter()
            .filter_map(|path| self.inner.capacity_probe.space(path));
        CapacitySnapshot::evaluate(&self.inner.config, spaces)
    }

    fn effective_budget(&self) -> u64 {
        self.capacity_snapshot().effective_budget_bytes
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
            StorageClass::ShadowWorkspace,
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

    /// Reserve `bytes` BEFORE publishing. The admission checks the
    /// capacity-aware hard budget (operator cap vs measured capacity),
    /// per-class quota, and the DESTINATION volume's own free-space reserve
    /// against measured free space minus unmaterialized reservations. When
    /// headroom is short, noncritical classes are evicted; if the reserve
    /// still cannot be preserved the admission is refused typed and health
    /// degrades — the disk is never filled. When no volume can be measured
    /// the admission is refused typed `capacity_unmeasured` (never an
    /// unbounded fallback to the operator cap).
    ///
    /// The destination volume is resolved from the class/key (see
    /// [`Self::destination_paths`]); writers that know their exact target
    /// path should use [`Self::reserve_into`] instead.
    pub fn reserve(
        &self,
        class: StorageClass,
        key: &str,
        bytes: u64,
    ) -> GovernorResult<Reservation> {
        self.reserve_inner(class, key, bytes, None, AdmissionPurpose::Normal)
    }

    /// Reserve `bytes` for a write that will land at `destination` (an
    /// absolute/relative path the writer owns). Admission enforces exactly
    /// THAT path's volume — its own reserve plus the aggregate budget — so a
    /// nearly-full destination can never borrow headroom from another
    /// filesystem, and a write targeting a healthy volume is never refused
    /// because a different volume is full.
    pub fn reserve_into(
        &self,
        class: StorageClass,
        key: &str,
        bytes: u64,
        destination: impl AsRef<Path>,
    ) -> GovernorResult<Reservation> {
        self.reserve_inner(
            class,
            key,
            bytes,
            Some(destination.as_ref()),
            AdmissionPurpose::Normal,
        )
    }

    /// RECOVERY-PATH admission: the same atomic checks as [`Self::reserve`]
    /// but under the recovery band, i.e. against the FULL effective budget
    /// instead of the normal ceiling. This is the only way rollback,
    /// finalize and repair bytes may enter the 90–100% reserve; ordinary
    /// callers never reach it, and it still enforces class quotas and the
    /// destination volume's free-space reserve.
    pub fn reserve_recovery(
        &self,
        class: StorageClass,
        key: &str,
        bytes: u64,
    ) -> GovernorResult<Reservation> {
        self.reserve_inner(class, key, bytes, None, AdmissionPurpose::Recovery)
    }

    /// [`Self::reserve_recovery`] with an explicit destination path (same
    /// volume rule as [`Self::reserve_into`]).
    pub fn reserve_recovery_into(
        &self,
        class: StorageClass,
        key: &str,
        bytes: u64,
        destination: impl AsRef<Path>,
    ) -> GovernorResult<Reservation> {
        self.reserve_inner(
            class,
            key,
            bytes,
            Some(destination.as_ref()),
            AdmissionPurpose::Recovery,
        )
    }

    fn reserve_inner(
        &self,
        class: StorageClass,
        key: &str,
        bytes: u64,
        destination: Option<&Path>,
        purpose: AdmissionPurpose,
    ) -> GovernorResult<Reservation> {
        if key.is_empty() {
            return Err(GovernorError::Malformed(
                "storage reservation requires a non-empty key".into(),
            ));
        }
        // Always-on tick: directory classes (profiles/caches/agent homes) grow
        // outside reservations, so they are re-indexed and enforced on a
        // bounded cadence. Non-blocking when another thread is already
        // maintaining.
        let _ = self.maintain_if_due(300_000, 30_000);
        let destinations = self.destination_paths(class, key, destination);
        let write_bound = free_space_write_bound(class, bytes);
        let mut last_available = 0u64;
        let mut last_reason = RefusalReason::NoEvictableObjects;
        let mut last_detail: Option<String> = None;
        // At most four admission rounds: each round either admits, or makes
        // one bounded eviction pass. The destination volume's free space is
        // re-measured every round (inside the admission transaction), so
        // eviction only counts when the filesystem really freed it.
        for _attempt in 0..4 {
            let capacity = self.capacity_snapshot();
            if !capacity.measured {
                // Counterexample 1: measurement failure must never widen the
                // budget to the (possibly u64::MAX) operator cap. No volume
                // means no verified headroom: refuse typed until a probe
                // succeeds; health degrades `capacity_unmeasured`.
                return self.refuse(
                    class,
                    key,
                    bytes,
                    0,
                    RefusalReason::CapacityUnmeasured,
                    format!(
                        "no backing volume could be measured ({} registered capacity paths); \
                         new material storage is refused until capacity measurement succeeds",
                        self.capacity_path_count()
                    ),
                );
            }
            let total = capacity.effective_budget_bytes;
            let admission_ceiling = purpose.ceiling_bytes(&self.inner.config, total).min(total);
            let quota = self.inner.config.quota_for(class, total);
            if bytes > quota {
                return self.refuse(
                    class,
                    key,
                    bytes,
                    0,
                    RefusalReason::ClassQuota,
                    format!("object alone exceeds the {quota} byte class quota"),
                );
            }
            if bytes > total {
                return self.refuse(
                    class,
                    key,
                    bytes,
                    0,
                    RefusalReason::AggregateBudget,
                    format!("object alone exceeds the {total} byte effective aggregate budget"),
                );
            }
            match self.try_reserve(
                class,
                key,
                bytes,
                quota,
                total,
                admission_ceiling,
                &destinations,
                write_bound,
            )? {
                Admit::Reserved(reservation) => {
                    self.maintain_watermark()?;
                    self.clear_degraded_if_healthy()?;
                    return Ok(reservation);
                }
                Admit::Need(need) => {
                    if let Some(detail) = need.volume_unmeasured.clone() {
                        // No eviction can restore measurement: refuse typed
                        // with the exact degraded state surfaced.
                        return self.refuse(
                            class,
                            key,
                            bytes,
                            0,
                            RefusalReason::CapacityUnmeasured,
                            detail,
                        );
                    }
                    last_available = need.available;
                    last_reason = need.reason();
                    last_detail = need.volume_detail.clone();
                    let mut freed = 0u64;
                    if need.class_deficit > 0 {
                        // Class pressure is satisfied ONLY from that class:
                        // evicting a different class cannot make the class
                        // quota true and destroys unrelated cold data.
                        freed = self.evict_class_for(class, need.class_deficit)?;
                    }
                    if need.class_deficit == 0 || freed >= need.class_deficit {
                        let remaining = need.total_deficit.saturating_sub(freed);
                        let volume_remaining = need.volume_deficit.saturating_sub(freed);
                        let remaining = remaining.max(volume_remaining);
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
            last_detail.unwrap_or_else(|| {
                "eviction could not free enough bytes; the hard budget or free-space reserve \
                 is not met"
                    .into()
            }),
        )
    }

    /// How many registered paths a capacity probe would visit (degraded-state
    /// detail; never used to fabricate a measurement).
    fn capacity_path_count(&self) -> usize {
        let mut paths: std::collections::BTreeSet<PathBuf> = std::collections::BTreeSet::new();
        paths.insert(self.inner.data_dir.clone());
        paths.insert(self.inner.root.clone());
        paths.extend(self.inner.capacity_roots.read().unwrap().iter().cloned());
        for backend in self.inner.backends.read().unwrap().values() {
            if let Some((roots, _)) = backend.directory_roots() {
                paths.extend(roots.iter().cloned());
            }
        }
        paths.len()
    }

    /// Candidate destination paths for a pending write under `class`/`key`,
    /// in resolution order:
    ///
    /// 1. the explicit destination passed by the writer (authoritative),
    /// 2. the backend's derived path for the key ([`StorageBackend::admission_path`]),
    /// 3. every directory root of the class when the key names no single root,
    /// 4. otherwise the conservative candidate set: the data directory plus
    ///    every registered capacity root. When the true destination cannot be
    ///    narrowed to one path, EVERY distinct candidate volume must keep its
    ///    own reserve — the check can then never under-admit the real target,
    ///    only (deliberately) over-refuse.
    fn destination_paths(
        &self,
        class: StorageClass,
        key: &str,
        explicit: Option<&Path>,
    ) -> Vec<PathBuf> {
        if let Some(destination) = explicit {
            return vec![destination.to_path_buf()];
        }
        if let Some(backend) = self.backend(class) {
            if let Some(path) = backend.admission_path(key) {
                return vec![path];
            }
            if let Some((roots, _)) = backend.directory_roots() {
                return roots.to_vec();
            }
        }
        let mut candidates = vec![self.inner.data_dir.clone()];
        candidates.extend(self.inner.capacity_roots.read().unwrap().iter().cloned());
        candidates
    }

    #[allow(clippy::too_many_arguments)]
    fn try_reserve(
        &self,
        class: StorageClass,
        key: &str,
        bytes: u64,
        quota: u64,
        total: u64,
        admission_ceiling: u64,
        destinations: &[PathBuf],
        write_bound: u64,
    ) -> GovernorResult<Admit> {
        let now = now_ms();
        let lease = self.inner.config.reservation_lease_ms.max(1);
        // The ledger volume (its SQLite/WAL lives under the data dir) is
        // identified before the transaction; filesystem ids are stable, while
        // the destination volumes themselves are re-measured INSIDE the
        // transaction together with the pending rows.
        let ledger_volume_id = self
            .inner
            .capacity_probe
            .space(&self.inner.data_dir)
            .map(|space| space.filesystem_id);
        let rule = VolumeRule {
            probe: self.inner.capacity_probe.as_ref(),
            paths: destinations.to_vec(),
            write_bound,
            ledger_volume_id,
            unsettled_inflight: self.inner.wal_debt.inflight.load(Ordering::Acquire),
            admission_ceiling,
            config: &self.inner.config,
        };
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
                rule,
            )?
        };
        let Some(id) = row.reservation_id else {
            let room = quota
                .saturating_sub(row.class_used)
                .min(total.saturating_sub(row.total_used));
            let available = match &row.volume_denial {
                // The volume's headroom is the honest "available" when the
                // destination reserve is what refused the write.
                Some(VolumeDenial::Reserve { available, .. }) => room.min(*available),
                _ => room,
            };
            return Ok(Admit::Need(Need {
                class_deficit: row.class_deficit,
                total_deficit: row.total_deficit,
                budget_deficit: row.budget_deficit,
                volume_deficit: match &row.volume_denial {
                    Some(VolumeDenial::Reserve { deficit, .. }) => *deficit,
                    _ => 0,
                },
                volume_unmeasured: match &row.volume_denial {
                    Some(VolumeDenial::Unmeasured { detail }) => Some(detail.clone()),
                    _ => None,
                },
                volume_detail: match &row.volume_denial {
                    Some(VolumeDenial::Reserve { detail, .. }) => Some(detail.clone()),
                    _ => None,
                },
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
        let effective_budget = self.effective_budget();
        let health = DegradedHealth {
            code: format!("storage_{}", reason.as_str()),
            detail: detail.clone(),
            total_bytes,
            total_budget_bytes: effective_budget,
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
        // Live run workspaces (shadow roots) are counted but never deleted
        // by LRU pressure: class pressure resolves to a typed refusal and a
        // degraded health naming the class, never silent destruction of an
        // in-flight run's files. The cursor in the eviction loops still
        // advances past these rows, so the pass stays bounded.
        if !object.class.is_evictable() {
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

    /// Enforce every class quota, the capacity-aware aggregate budget, and
    /// the always-preserved free-space reserve. Returns the bytes freed.
    /// When the reserve still cannot be preserved after evicting noncritical
    /// classes, health is degraded typed.
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
        let capacity = self.capacity_snapshot();
        let effective_budget = capacity.effective_budget_bytes;
        let mut freed = 0u64;
        let usage = self.inner.ledger.lock().unwrap().usage()?;
        for class in StorageClass::ALL {
            let used = usage.get(&class).map(|row| row.bytes).unwrap_or(0);
            let quota = self.inner.config.quota_for(class, effective_budget);
            if used > quota {
                freed += self.evict_class_for(class, used - quota)?;
            }
        }
        let total_used = self.inner.ledger.lock().unwrap().sum_usage()?;
        if total_used > effective_budget {
            freed += self.evict_global_for(total_used - effective_budget)?;
        }
        // Actual disk headroom: if free space has fallen into the reserve,
        // reclaim noncritical classes until the reserve is whole again.
        for _attempt in 0..3 {
            let capacity = self.capacity_snapshot();
            let deficit = capacity.reserve_deficit();
            if deficit == 0 {
                break;
            }
            let reclaimed = self.evict_global_for(deficit)?;
            if reclaimed == 0 {
                break;
            }
            freed = freed.saturating_add(reclaimed);
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
        let quota = self
            .inner
            .config
            .quota_for(class, self.capacity_snapshot().effective_budget_bytes);
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
        // History retention on the heavy tick: the append-only loss/eviction/
        // refusal tables keep their newest HISTORY_DETAIL_KEEP detail rows;
        // older rows are compacted into hourly aggregates + durable loss
        // tombstones and their pages reclaimed. Bounded and non-blocking to
        // admissions (this is the reconcile path, never the hot write path).
        {
            let mut ledger = self.inner.ledger.lock().unwrap();
            let _ = ledger.prune_history(HISTORY_DETAIL_KEEP);
        }
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
        let capacity = self.capacity_snapshot();
        let effective_budget = capacity.effective_budget_bytes;
        let mut per_class = BTreeMap::new();
        for class in StorageClass::ALL {
            let usage = usage.get(&class).cloned().unwrap_or_else(UsageRow::default);
            per_class.insert(
                class.as_str().to_string(),
                ClassUsage {
                    class,
                    bytes: usage.bytes,
                    objects: usage.objects,
                    quota_bytes: self.inner.config.quota_for(class, effective_budget),
                    pinned_bytes: usage.pinned_bytes,
                    pending_reservation_bytes: pending.get(&class).copied().unwrap_or(0),
                },
            );
        }
        let watermark_floor =
            effective_budget.saturating_mul(self.inner.config.watermark_permille as u64) / 1000;
        let normal_ceiling = self
            .inner
            .config
            .normal_admission_ceiling_bytes(effective_budget);
        let emergency_ceiling = self.inner.config.emergency_ceiling_bytes(effective_budget);
        let pending_total = pending
            .values()
            .copied()
            .fold(0u64, |sum, bytes| sum.saturating_add(bytes));
        let band_usage = total.saturating_add(pending_total);
        let admission_band = if total > effective_budget {
            "over_budget"
        } else if band_usage > effective_budget {
            // Durable usage is inside the budget but live reservations push
            // the admitted view past it.
            "over_budget"
        } else if band_usage > emergency_ceiling {
            "recovery_reserve"
        } else if band_usage > normal_ceiling {
            "emergency"
        } else {
            "normal"
        };
        Ok(StorageMetrics {
            total_bytes: total,
            total_budget_bytes: effective_budget,
            configured_budget_bytes: self.inner.config.total_budget_bytes,
            capacity_bytes: capacity.capacity_bytes,
            available_bytes: capacity.available_bytes,
            reserve_bytes: capacity.reserve_bytes,
            effective_budget_bytes: effective_budget,
            headroom_bytes: capacity.headroom_bytes,
            capacity_measured: capacity.measured,
            filesystems: capacity.filesystems,
            total_objects: objects,
            used_permille_of_budget: total
                .saturating_mul(1000)
                .checked_div(effective_budget)
                .unwrap_or(0),
            watermark_permille: self.inner.config.watermark_permille,
            above_watermark: total >= watermark_floor,
            normal_admission_ceiling_bytes: normal_ceiling,
            emergency_ceiling_bytes: emergency_ceiling,
            recovery_reserve_bytes: effective_budget.saturating_sub(emergency_ceiling),
            admission_band: admission_band.to_string(),
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
        let effective_budget = self.effective_budget();
        let floor =
            effective_budget.saturating_mul(self.inner.config.watermark_permille as u64) / 1000;
        if total > floor {
            let low = floor.saturating_sub(
                effective_budget.saturating_mul(WATERMARK_HYSTERESIS_PERMILLE) / 1000,
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
        let capacity = self.capacity_snapshot();
        let effective_budget = capacity.effective_budget_bytes;
        // Counterexample 1, surfaced state: no measurable volume degrades
        // health FIRST (above every capacity-relative judgement), naming the
        // exact state instead of silently trusting the operator cap.
        let measurement = (!capacity.measured).then(|| DegradedHealth {
            code: "capacity_unmeasured".into(),
            detail: format!(
                "no backing volume could be measured ({} registered capacity paths); the \
                 capacity-relative budget, the free-space reserve and new-material-storage \
                 admission are unavailable until a probe succeeds",
                self.capacity_path_count()
            ),
            total_bytes: total,
            total_budget_bytes: effective_budget,
            required_bytes: pinned,
            class: None,
            at_ms: now_ms(),
        });
        // Counterexample 2 on the maintenance side: the reserve must be whole
        // on EVERY volume; pooled free space can no longer mask one nearly-full
        // volume behind another's emptiness.
        let reserve_pressure = capacity.measured.then(|| {
            let worst = capacity
                .filesystems
                .iter()
                .max_by_key(|fs| fs.reserve_bytes.saturating_sub(fs.available_bytes));
            worst.and_then(|fs| {
                let deficit = fs.reserve_bytes.saturating_sub(fs.available_bytes);
                (deficit > 0).then(|| DegradedHealth {
                    code: "free_space_reserve_unmet".into(),
                    detail: format!(
                        "volume {}: {} bytes free but its {} byte reserve must be preserved; no evictable noncritical object remains",
                        fs.filesystem_id, fs.available_bytes, fs.reserve_bytes
                    ),
                    total_bytes: total,
                    total_budget_bytes: effective_budget,
                    required_bytes: pinned.saturating_add(deficit),
                    class: None,
                    at_ms: now_ms(),
                })
            })
        });
        let structural = measurement
            .or(reserve_pressure.flatten())
            .or_else(|| {
            if total > effective_budget {
                Some(DegradedHealth {
                    code: "aggregate_budget_exceeded".into(),
                    detail: format!(
                        "{} bytes exceed the hard {} byte effective aggregate budget and no evictable object remains",
                        total, effective_budget
                    ),
                    total_bytes: total,
                    total_budget_bytes: effective_budget,
                    required_bytes: pinned,
                    class: None,
                    at_ms: now_ms(),
                })
            } else {
                usage
                    .iter()
                    .find_map(|(class, row)| {
                        let quota = self.inner.config.quota_for(*class, effective_budget);
                        (row.bytes > quota).then(|| DegradedHealth {
                            code: "class_quota_exceeded".into(),
                            detail: format!(
                                "{} holds {} bytes over its {} byte quota and no evictable object remains",
                                class.as_str(),
                                row.bytes - quota,
                                quota
                            ),
                            total_bytes: total,
                            total_budget_bytes: effective_budget,
                            required_bytes: row.pinned_bytes,
                            class: Some(*class),
                            at_ms: now_ms(),
                        })
                    })
                    .or_else(|| {
                        (pinned > effective_budget).then(|| DegradedHealth {
                            code: "required_bytes_exceed_budget".into(),
                            detail: format!(
                                "{} required bytes exceed the hard {} byte aggregate budget",
                                pinned, effective_budget
                            ),
                            total_bytes: total,
                            total_budget_bytes: effective_budget,
                            required_bytes: pinned,
                            class: None,
                            at_ms: now_ms(),
                        })
                    })
            }
        });
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
                    total_budget_bytes: effective_budget,
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
        let capacity = self.capacity_snapshot();
        let effective_budget = capacity.effective_budget_bytes;
        // An unmeasured governor is NEVER "healthy again": the
        // `capacity_unmeasured` degradation stands until a probe succeeds.
        if capacity.measured
            && total <= effective_budget
            && usage.iter().all(|(class, row)| {
                row.bytes <= self.inner.config.quota_for(*class, effective_budget)
            })
            && capacity.reserve_deficit() == 0
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
        let quota = self.inner.config.quota_for(
            StorageClass::SqliteWal,
            self.capacity_snapshot().effective_budget_bytes,
        );
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
    /// Takes the already-held ledger lock so the budget room, the destination
    /// volume rule and the debt increment can be checked atomically.
    fn sqlite_room(
        &self,
        ledger: &Ledger,
        incoming_bytes: u64,
        quota: u64,
        total: u64,
    ) -> GovernorResult<Option<(u64, u64, u64)>> {
        let bound = sqlite_growth_bound(incoming_bytes);
        let now = now_ms();
        let snapshot = ledger.budget_snapshot(StorageClass::SqliteWal, now)?;
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

    /// The destination-volume room for one SQLite/WAL growth admission: the
    /// data directory's volume must keep its own reserve after live
    /// reservations on that volume (including unknown-volume rows) and the
    /// unsettled in-flight WAL debt are charged against measured free space.
    /// Evaluated under the held ledger lock together with the budget room, so
    /// synchronized admissions cannot both pass one free-space reading.
    fn sqlite_volume_room(&self, ledger: &Ledger, bound: u64) -> GovernorResult<SqliteVolumeRoom> {
        let Some(space) = self.inner.capacity_probe.space(&self.inner.data_dir) else {
            return Ok(SqliteVolumeRoom::Unmeasured {
                detail: format!(
                    "capacity measurement failed for the SQLite data directory {}; SQLite/WAL \
                     growth is refused until that volume can be measured",
                    self.inner.data_dir.display()
                ),
            });
        };
        let reserve_bytes = volume_reserve_bytes(&self.inner.config, space.capacity_bytes);
        let pending = ledger.pending_on_volume(space.filesystem_id, now_ms())?;
        let inflight = self.inner.wal_debt.inflight.load(Ordering::Acquire);
        let deficit = volume_admission_deficit(
            space.available_bytes,
            reserve_bytes,
            pending.saturating_add(inflight),
            bound,
        );
        if deficit > 0 {
            return Ok(SqliteVolumeRoom::Reserve {
                deficit,
                available: space.available_bytes.saturating_sub(reserve_bytes),
                detail: format!(
                    "data-dir volume {}: {} bytes free, {reserve_bytes} byte reserve, {pending} \
                     bytes of unmaterialized reservations, {inflight} bytes of in-flight SQLite \
                     growth, {bound} byte write bound; {deficit} bytes short of preserving the \
                     reserve",
                    space.filesystem_id, space.available_bytes
                ),
            });
        }
        Ok(SqliteVolumeRoom::Ok)
    }

    fn charge_sqlite_growth(
        &self,
        incoming_bytes: u64,
        tracked: bool,
    ) -> GovernorResult<Option<SqliteAdmission>> {
        let bound = sqlite_growth_bound(incoming_bytes);
        let capacity = self.capacity_snapshot();
        if !capacity.measured {
            // Measurement failure never admits growth: the caller's refusal
            // path reports the typed `capacity_unmeasured` state.
            return Ok(None);
        }
        let budget = capacity.effective_budget_bytes;
        let quota = self.inner.config.quota_for(StorageClass::SqliteWal, budget);
        // Emergency/critical-metadata band: ledger/WAL growth may consume
        // up to the emergency ceiling (default 90% of the effective
        // budget); the RECOVERY-ONLY reserve above it is never spent by
        // ongoing metadata growth — only explicit recovery admissions.
        let total = self
            .inner
            .config
            .emergency_ceiling_bytes(budget)
            .min(budget);
        loop {
            let mut stop = false;
            let mut admitted: Option<SqliteAdmission> = None;
            {
                // One ledger lock spans the budget room, the destination
                // volume's free-space rule and the debt increment: a
                // concurrent admission can never slip between check and
                // charge on one stale free-space reading.
                let ledger = self.inner.ledger.lock().unwrap();
                let room = self.sqlite_room(&ledger, incoming_bytes, quota, total)?;
                let volume_ok = matches!(
                    self.sqlite_volume_room(&ledger, bound)?,
                    SqliteVolumeRoom::Ok
                );
                if room.is_none() || !volume_ok {
                    // Budget room spent, or the destination volume cannot
                    // keep its reserve: the caller evicts or refuses.
                    stop = true;
                } else {
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
                        admitted = Some(SqliteAdmission {
                            governor: self.clone(),
                            charged: if tracked { bound } else { 0 },
                            active: tracked,
                        });
                    } else if tracked {
                        settle_inflight(&self.inner.wal_debt.inflight, bound);
                    }
                }
            }
            if let Some(admission) = admitted {
                return Ok(Some(admission));
            }
            if stop {
                break;
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
        // Destination volume: reclaim noncritical classes before refusing an
        // append that would consume that volume's free-space reserve.
        let volume_deficit = {
            let ledger = self.inner.ledger.lock().unwrap();
            match self.sqlite_volume_room(&ledger, sqlite_growth_bound(incoming_bytes))? {
                SqliteVolumeRoom::Reserve { deficit, .. } => deficit,
                _ => 0,
            }
        };
        if volume_deficit > 0 {
            let _ = self.evict_global_for(volume_deficit);
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
        let capacity = self.capacity_snapshot();
        let budget = capacity.effective_budget_bytes;
        let quota = self.inner.config.quota_for(StorageClass::SqliteWal, budget);
        // The admission comparison runs against the emergency band; the
        // health report below still names the FULL effective budget.
        let total = self
            .inner
            .config
            .emergency_ceiling_bytes(budget)
            .min(budget);
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
        let volume_room = {
            let ledger = self.inner.ledger.lock().unwrap();
            self.sqlite_volume_room(&ledger, bound)?
        };
        let volume_headroom = match &volume_room {
            SqliteVolumeRoom::Reserve { available, .. } => *available,
            SqliteVolumeRoom::Ok => capacity.headroom_bytes,
            SqliteVolumeRoom::Unmeasured { .. } => 0,
        };
        // Counterexample 1 on the SQLite path: measurement failure must
        // refuse typed, never fall back to the operator cap as if headroom
        // were known.
        let reason =
            if !capacity.measured || matches!(volume_room, SqliteVolumeRoom::Unmeasured { .. }) {
                RefusalReason::CapacityUnmeasured
            } else if matches!(volume_room, SqliteVolumeRoom::Reserve { .. }) {
                RefusalReason::FreeSpaceReserve
            } else if class_used.saturating_add(bound) > quota {
                RefusalReason::ClassQuota
            } else {
                RefusalReason::AggregateBudget
            };
        let detail = match &volume_room {
            SqliteVolumeRoom::Unmeasured { detail } => detail.clone(),
            SqliteVolumeRoom::Reserve { detail, .. } => detail.clone(),
            SqliteVolumeRoom::Ok => format!(
                "SQLite/WAL growth refused before the append ({class_used} used of {quota} \
                 quota, emergency ceiling {total} of {budget} byte budget)"
            ),
        };
        let available = quota
            .saturating_sub(class_used)
            .min(total.saturating_sub(total_used))
            .min(volume_headroom);
        {
            let mut ledger = self.inner.ledger.lock().unwrap();
            ledger.record_refusal(
                StorageClass::SqliteWal,
                incoming_bytes,
                available,
                reason.as_str(),
                &detail,
            )?;
        }
        let health = DegradedHealth {
            code: format!("storage_{}", reason.as_str()),
            detail: format!(
                "SQLite growth of {incoming_bytes} bytes refused ({detail}; {class_used} used of \
                 {quota} quota, emergency ceiling {total}, pinned {pinned})"
            ),
            total_bytes: total_used,
            total_budget_bytes: budget,
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
            detail,
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

    // ------------------------------------------------------------- shadows

    /// Physical bytes under one directory: every regular file, bounded by
    /// the same walk limit as directory scans. Unlike the canonical tree
    /// manifest this NEVER skips build outputs (`target/`, `node_modules`,
    /// caches): they are excluded from source identity but always count
    /// against the physical budget.
    pub fn measure_directory_bytes(root: &Path) -> GovernorResult<u64> {
        use crate::directory::MAX_WALK_ENTRIES;
        let mut stack = vec![root.to_path_buf()];
        let mut total = 0u64;
        let mut visited = 0usize;
        while let Some(dir) = stack.pop() {
            let read = match std::fs::read_dir(&dir) {
                Ok(read) => read,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(GovernorError::Backend(format!(
                        "measure {}: {e}",
                        dir.display()
                    )));
                }
            };
            for entry in read {
                visited += 1;
                if visited > MAX_WALK_ENTRIES {
                    return Err(GovernorError::Backend(format!(
                        "directory measurement exceeded the {MAX_WALK_ENTRIES}-entry bound under {}",
                        root.display()
                    )));
                }
                let entry = entry.map_err(|e| {
                    GovernorError::Backend(format!("measure entry under {}: {e}", dir.display()))
                })?;
                let file_type = entry.file_type().map_err(|e| {
                    GovernorError::Backend(format!(
                        "measure entry type under {}: {e}",
                        dir.display()
                    ))
                })?;
                if file_type.is_symlink() {
                    continue;
                }
                let path = entry.path();
                if file_type.is_dir() {
                    stack.push(path);
                    continue;
                }
                if !file_type.is_file() {
                    continue;
                }
                let meta = match std::fs::metadata(&path) {
                    Ok(meta) => meta,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(e) => {
                        return Err(GovernorError::Backend(format!(
                            "measure metadata {}: {e}",
                            path.display()
                        )));
                    }
                };
                total = total.saturating_add(meta.len());
            }
        }
        Ok(total)
    }

    /// Shadow admission BEFORE any copy byte is written: the per-shadow
    /// hard ceiling is checked first (typed [`RefusalReason::ShadowCeiling`]),
    /// then the base-copy bytes are reserved against the ACTUAL shadow
    /// destination filesystem — class quota, admission band and that
    /// volume's free-space reserve, inside the atomic admission transaction.
    /// The caller commits with the copied bytes once the copy is durable, so
    /// the shadow's accounting is charged from its first instant and can
    /// never break the destination volume's reserve.
    pub fn shadow_admission(
        &self,
        shadow_key: &str,
        preflight_bytes: u64,
        destination: impl AsRef<Path>,
    ) -> GovernorResult<Reservation> {
        let ceiling = self.inner.config.shadow_max_bytes_per_shadow;
        if preflight_bytes > ceiling {
            return self.refuse(
                StorageClass::ShadowWorkspace,
                shadow_key,
                preflight_bytes,
                0,
                RefusalReason::ShadowCeiling,
                format!(
                    "shadow {shadow_key} base copy of {preflight_bytes} bytes exceeds the {ceiling} \
                     byte per-shadow hard ceiling (base copy plus physical growth must fit it)"
                ),
            );
        }
        self.reserve_into(
            StorageClass::ShadowWorkspace,
            shadow_key,
            preflight_bytes,
            destination,
        )
    }

    /// Re-scan one class's backend and replace its durable inventory with
    /// disk truth (the scan-start guard preserves rows committed after the
    /// scan began). Used at the shadow begin/retire boundaries so shadow
    /// accounting is exact immediately — not one maintenance tick later.
    /// Returns the class's durable bytes after the refresh.
    pub fn refresh_class_inventory(&self, class: StorageClass) -> GovernorResult<u64> {
        let Some(backend) = self.backend(class) else {
            return Err(GovernorError::Unavailable(format!(
                "no backend registered for {}",
                class.as_str()
            )));
        };
        let scan_started_ms = now_ms();
        let objects = backend.scan(self)?;
        let rows: Vec<ObjectRow> = objects
            .iter()
            .map(|object| object_row(class, object))
            .collect();
        let mut ledger = self.inner.ledger.lock().unwrap();
        ledger.set_inventory_scanned_at(class, &rows, scan_started_ms)?;
        let bytes = ledger
            .usage()?
            .get(&class)
            .map(|row| row.bytes)
            .unwrap_or(0);
        Ok(bytes)
    }

    /// Durable usage of ONE shadow directory (its key plus everything under
    /// `<key>/` in the shadow class inventory).
    pub fn shadow_usage(&self, shadow_key: &str) -> GovernorResult<u64> {
        self.inner
            .ledger
            .lock()
            .unwrap()
            .usage_prefix(StorageClass::ShadowWorkspace, shadow_key)
    }

    /// Post-execution growth accounting at the shadow task boundary: refresh
    /// the class inventory from disk truth (build outputs included), then
    /// check the per-shadow hard ceiling, the shadow class quota and the
    /// aggregate budget. On breach a typed refusal is recorded durably,
    /// health degrades naming the shadow class, and the caller must kill or
    /// refuse the run per policy — the bytes are already physical, so the
    /// refusal gates every NEW admission instead of pretending the write
    /// never happened. No OS quota is enforced (none is portable
    /// cross-platform); this measurement is the enforcement.
    pub fn admit_shadow_growth(
        &self,
        shadow_key: &str,
        root: &Path,
    ) -> GovernorResult<ShadowGrowth> {
        let measured = Self::measure_directory_bytes(root)?;
        let class_bytes = self.refresh_class_inventory(StorageClass::ShadowWorkspace)?;
        let shadow_bytes = self.shadow_usage(shadow_key)?;
        let capacity = self.capacity_snapshot();
        let total = self.inner.ledger.lock().unwrap().sum_usage()?;
        let effective_budget = capacity.effective_budget_bytes;
        let per_shadow_ceiling = self.inner.config.shadow_max_bytes_per_shadow;
        let class_quota = self
            .inner
            .config
            .quota_for(StorageClass::ShadowWorkspace, effective_budget);
        let growth = ShadowGrowth {
            shadow_bytes,
            class_bytes,
            total_bytes: total,
            per_shadow_ceiling_bytes: per_shadow_ceiling,
        };
        if shadow_bytes > per_shadow_ceiling {
            return self.refuse(
                StorageClass::ShadowWorkspace,
                shadow_key,
                shadow_bytes.saturating_sub(per_shadow_ceiling),
                0,
                RefusalReason::ShadowCeiling,
                format!(
                    "shadow {shadow_key} holds {shadow_bytes} physical bytes (measured {measured} \
                     at the task boundary) over its {per_shadow_ceiling} byte hard ceiling; no OS \
                     quota is enforced cross-platform, so the breach refuses every new admission \
                     and the run must be killed/refused per policy"
                ),
            );
        }
        if class_bytes > class_quota {
            return self.refuse(
                StorageClass::ShadowWorkspace,
                shadow_key,
                class_bytes.saturating_sub(class_quota),
                0,
                RefusalReason::ClassQuota,
                format!(
                    "shadow class holds {class_bytes} physical bytes over its {class_quota} byte \
                     quota; shadow workspaces are never LRU-evicted (live run state), so class \
                     pressure is a typed refusal and new admissions stop"
                ),
            );
        }
        if total > effective_budget {
            return self.refuse(
                StorageClass::ShadowWorkspace,
                shadow_key,
                total.saturating_sub(effective_budget),
                0,
                RefusalReason::AggregateBudget,
                format!(
                    "total governed usage {total} exceeds the {effective_budget} byte hard \
                     aggregate budget with shadow {shadow_key} at {shadow_bytes} bytes"
                ),
            );
        }
        Ok(growth)
    }

    /// Release the durable accounting of one REMOVED shadow directory (its
    /// key plus its whole prefix) in one transaction: retire/discard and
    /// the crash sweep free budget room immediately instead of waiting for
    /// the next full scan. Returns the released bytes.
    pub fn release_shadow(&self, shadow_key: &str) -> GovernorResult<u64> {
        self.inner
            .ledger
            .lock()
            .unwrap()
            .remove_objects_with_prefix(StorageClass::ShadowWorkspace, shadow_key)
    }

    /// Explicit history-retention pass over the append-only tables (keep the
    /// newest `detail_keep` rows, compact the rest into hourly aggregates +
    /// durable loss tombstones, reclaim pages incrementally). Runs on every
    /// full reconcile with [`HISTORY_DETAIL_KEEP`]; operators and tests may
    /// force it here.
    pub fn prune_history(&self, detail_keep: u64) -> GovernorResult<HistoryPruneReport> {
        self.inner.ledger.lock().unwrap().prune_history(detail_keep)
    }

    /// The documented default retention pass ([`HISTORY_DETAIL_KEEP`]).
    pub fn prune_history_default(&self) -> GovernorResult<HistoryPruneReport> {
        self.prune_history(HISTORY_DETAIL_KEEP)
    }

    /// The compacted hourly aggregate rows of one history kind
    /// (`loss_marker`/`eviction`/`refusal`): the durable, bounded view an
    /// operator reads after retention moved detail rows out.
    pub fn history_aggregates(
        &self,
        kind: &str,
    ) -> GovernorResult<Vec<crate::HistoryAggregateRow>> {
        self.inner.ledger.lock().unwrap().history_aggregates(kind)
    }

    /// Durable per-key archive-loss tombstones kept by retention (each is
    /// one compacted loss that stays visible and deduplicated).
    pub fn loss_tombstone_count(&self) -> GovernorResult<u64> {
        self.inner.ledger.lock().unwrap().loss_tombstone_count()
    }
}

enum Admit {
    Reserved(Reservation),
    Need(Need),
}

/// Outcome of one destination-volume room check on the SQLite path.
enum SqliteVolumeRoom {
    /// The volume keeps its reserve after unmaterialized charges.
    Ok,
    /// The volume's reserve could not be preserved.
    Reserve {
        deficit: u64,
        available: u64,
        detail: String,
    },
    /// The volume could not be measured at all.
    Unmeasured { detail: String },
}

struct Need {
    class_deficit: u64,
    total_deficit: u64,
    /// Deficit against the FULL effective budget. Zero with a positive
    /// `total_deficit` means the write fits the hard budget but not its
    /// admission band (the recovery reserve held).
    budget_deficit: u64,
    /// Bytes the destination volume is short of preserving its own reserve,
    /// once unmaterialized reservations and the write bound are charged.
    volume_deficit: u64,
    /// Set when a destination volume could not be measured (typed refusal;
    /// eviction cannot help, the governor must stop and report the degraded
    /// state).
    volume_unmeasured: Option<String>,
    /// Human-readable destination-volume denial carried into the final
    /// refusal detail.
    volume_detail: Option<String>,
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
        if self.volume_unmeasured.is_some() {
            return RefusalReason::CapacityUnmeasured;
        }
        if self.class_deficit > 0 && self.class_pinned.saturating_add(self.class_bytes) > self.quota
        {
            return RefusalReason::RequiredBytes;
        }
        if self.total_deficit > 0 && self.total_pinned.saturating_add(self.class_bytes) > self.total
        {
            return RefusalReason::RequiredBytes;
        }
        if self.volume_deficit > 0 {
            return RefusalReason::FreeSpaceReserve;
        }
        if self.total_deficit > 0 && self.budget_deficit == 0 {
            // The write fits the hard aggregate budget but not its admission
            // band: the recovery reserve (or the emergency band for a normal
            // admission) held. Typed distinctly from a true budget refusal.
            return RefusalReason::NormalAdmissionCeiling;
        }
        if self.budget_deficit > 0 {
            // The write exceeds the hard aggregate budget itself (100%): a
            // true budget exhaustion, never a band artifact.
            return RefusalReason::AggregateBudget;
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
            let capacity = self.governor.capacity_snapshot();
            let effective_budget = capacity.effective_budget_bytes;
            if total > effective_budget {
                let pinned = self.governor.inner.ledger.lock().unwrap().sum_pinned()?;
                self.governor.record_degraded(DegradedHealth {
                    code: "reservation_lease_expired_over_budget".into(),
                    detail: format!(
                        "publish of {} bytes committed after its reservation expired and took total usage to {total} (hard {} bytes)",
                        actual_bytes, effective_budget
                    ),
                    total_bytes: total,
                    total_budget_bytes: effective_budget,
                    required_bytes: pinned,
                    class: Some(self.class),
                    at_ms: now_ms(),
                })?;
                let _ = self.governor.enforce();
            } else if capacity.reserve_deficit() > 0 {
                let _ = self.governor.enforce();
            }
            return Ok(());
        }
        let total = self.governor.inner.ledger.lock().unwrap().sum_usage()?;
        let capacity = self.governor.capacity_snapshot();
        let effective_budget = capacity.effective_budget_bytes;
        if total > effective_budget {
            let pinned = self.governor.inner.ledger.lock().unwrap().sum_pinned()?;
            self.governor.record_degraded(DegradedHealth {
                code: "aggregate_budget_exceeded".into(),
                detail: format!(
                    "commit of {} bytes made total usage {total} exceed the hard {} byte budget",
                    actual_bytes, effective_budget
                ),
                total_bytes: total,
                total_budget_bytes: effective_budget,
                required_bytes: pinned,
                class: Some(self.class),
                at_ms: now_ms(),
            })?;
        }
        if capacity.reserve_deficit() > 0 {
            // Another process may have consumed the reserve while this
            // reservation was in flight; reclaim before reporting success.
            let _ = self.governor.enforce();
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

/// One shadow boundary measurement after a class-inventory refresh: the
/// shadow's durable bytes, the whole shadow class and the aggregate total,
/// against which the per-shadow ceiling / class quota / hard budget were
/// checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShadowGrowth {
    pub shadow_bytes: u64,
    pub class_bytes: u64,
    pub total_bytes: u64,
    pub per_shadow_ceiling_bytes: u64,
}
