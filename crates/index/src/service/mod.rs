//! The real repository [`IndexService`] (audits 30/64): a persistent,
//! generation-addressed, per-workspace repository index with a durable
//! [`WorkspaceIndexState`] machine in the store.
//!
//! # Never block the first prompt
//!
//! [`IndexService::view`] is instant: it returns the newest PUBLISHED
//! generation's content when one exists (`None` while a first build is in
//! flight or while persisted content is still reloading). The runtime keeps
//! the bounded evidence scan as the fallback until a `Ready` generation
//! exists, and [`IndexService::ensure_ready`] is the only blocking entry
//! point (tests / retry paths, always with an explicit deadline).
//!
//! # Cancellable scans and a hard shutdown bound (audit 20)
//!
//! Every scan walk is a sequence of ONE-directory-entry work units: the
//! cancellation token is observed between files/directories, between batch
//! files and between machine steps, so a worker shutdown on a deliberately
//! slow filesystem exits after at most the ONE syscall in flight instead of
//! after an entire scan invocation. The worker task JOINS its in-flight
//! blocking pass before reporting `Joined`, so
//! [`IndexService::shutdown_worker`] bounds the real exit; an abandoned
//! build publishes nothing and leaves the durable `Building { target }` row
//! for an immediate resume (never a wedge behind the build lease). The
//! per-service `slow_fs_ms` test seam holds that guarantee against a
//! paced filesystem.
//!
//! # Off-lock generation swap
//!
//! A build scans into a private in-memory index, then publishes in a fixed
//! order: ONE atomic publish through `faktor_fs::atomic` (temp + fsync +
//! rename + parent fsync) into `<data_root>/generations/<ws>/gen-<g>.json`
//! FIRST, followed by the durable CAS `Building{g} -> Ready{g}`. Readers
//! hold an immutable generation snapshot (`Arc`); a read at generation `g`
//! can never observe a partial `g+1` — the file only ever appears complete
//! and the in-memory content swaps only after the CAS. The ordering keeps
//! `Ready{g} => gen-g bytes visible` true at every instant, so a concurrent
//! reconcile can never mistake an in-flight writer for a torn publish.
//! Exactly one builder wins the publish CAS per generation; a loser's
//! staged snapshot is never named by the row (the next resume replaces it).
//!
//! # Restart and pruning
//!
//! `Ready{g}` restarts as `Ready{g}` (content reloaded from `gen-g.json`);
//! `Building`/`Dirty` resume building the SAME target; `Failed` waits for
//! the next change or an explicit retry. Publishing `N+1` prunes generation
//! files `<= N-1`, so at most [`KEEP_GENERATIONS`] generations are on disk.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::AtomicU64;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(test)]
use std::sync::OnceLock;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use faktor_core::cancellation::CancellationToken;
use faktor_core::id::WorkspaceId;
use faktor_fs::{FsEventKind, WorkspaceFileService, WorkspaceHandle};
use faktor_store::Store;
use faktor_terminal::ProcessSupervisor;

use crate::cold::ColdEvidenceProvider;
use crate::coverage::{
    fingerprint_shard, EvidenceFreshness, EvidencePackageMeta, FingerprintCoverage, IndexCoverage,
    IndexCoverageSnapshot, ScanCursor, ScanFrame, FINGERPRINT_SHARDS,
};
use crate::embedding::{
    apply_embeddings, EmbeddingIndex, EmbeddingModel, EmbeddingSource,
    MAX_EMBEDDING_SCAN_TEXT_BYTES,
};
use crate::generation::{FingerprintEntry, GenerationFile};
use crate::state::{
    PersistedIndexState, StateError, WorkspaceIndexState, JOURNAL_BUILDING, JOURNAL_CONTINUE,
    JOURNAL_CORRUPT, JOURNAL_DIRTY, JOURNAL_FAILED, JOURNAL_NOT_STARTED, JOURNAL_READY,
    JOURNAL_READY_PARTIAL, JOURNAL_RESUME, JOURNAL_TORN_READY, JOURNAL_VERIFIED,
    JOURNAL_VERIFY_DIRTY,
};
use crate::WorkspaceIndex;

/// Generations kept on disk: publishing `Ready{N}` prunes everything
/// `<= N - KEEP_GENERATIONS`, so `N-1` and `N` remain.
pub const KEEP_GENERATIONS: u64 = 2;

/// Directories never walked by a build or fingerprint (vcs metadata,
/// dependency trees).
pub const SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "target",
    ".venv",
    "dist",
];

// ------------------------------------------------------- shadow re-pointing
//
// P0-48: while a shadowed drive is live the service resolves a workspace's
// root through the SESSION crate's durable shadow registry, consumed here
// OPACELY (faktor-index cannot depend on faktor-session; see
// `workspace_live_shadow_root`) — every unrecognizable/ambiguous state
// degrades loudly to the stored root, never a guessed root.

/// Durable fact kind of one session's active-shadow row (mirror of
/// `faktor_session::SHADOW_ROW_KIND`).
const SHADOW_ROW_KIND: &str = "shadow_root";
/// Durable fact key of one session's active-shadow row.
const SHADOW_ROW_KEY: &str = "active";
/// Live shadow-row states (mirror of `ShadowRowState::is_live`): the row
/// still names the mutation target of a live or conflicted drive.
const SHADOW_LIVE_STATES: [&str; 2] = ["active", "integration_blocked"];

/// Parse ONE durable shadow-row value into its root when the row is LIVE.
/// `Ok(None)` for a well-formed row in a retired state; `Err` for a value
/// that is not a recognizable shadow row (hostile/corrupt) — callers
/// degrade to the stored root loudly.
fn parse_live_shadow_row(value: &str) -> Result<Option<PathBuf>, String> {
    let v: serde_json::Value =
        serde_json::from_str(value).map_err(|e| format!("shadow row decode: {e}"))?;
    let obj = v
        .as_object()
        .ok_or_else(|| "shadow row is not a JSON object".to_string())?;
    let state = obj
        .get("state")
        .and_then(|s| s.as_str())
        .ok_or_else(|| "shadow row carries no state".to_string())?;
    if !SHADOW_LIVE_STATES.contains(&state) {
        return Ok(None);
    }
    let root = obj
        .get("root")
        .and_then(|r| r.as_str())
        .filter(|r| !r.is_empty())
        .ok_or_else(|| format!("live shadow row carries no root (state {state})"))?;
    Ok(Some(PathBuf::from(root)))
}

/// Per-generation-batch content budgets (audit 5): a batch stops at the
/// first budget reached, persists its cursor + [`IndexCoverage`] in the
/// envelope, and indexing continues batch after batch until the walk
/// exhausted the tree. The budgets bound one BATCH, never the generation.
pub const BATCH_MAX_FILES: usize = 512;
pub const BATCH_MAX_DIRS: usize = 2_048;
pub const BATCH_MAX_BYTES: usize = 8 * 1024 * 1024;
/// Largest single file a batch will index (content, not stat size).
pub const SCAN_MAX_FILE_BYTES: u64 = 1_000_000;

/// Per-pass fingerprint budgets (audit 6): one shard walk pass is bounded;
/// the persisted [`FingerprintCoverage`] carries the round state and the
/// next pass continues/rotates. A capped pass is NEVER reported clean.
pub const FP_BATCH_FILES: usize = 4_096;
pub const FP_BATCH_DIRS: usize = 2_048;

/// Maximum generation-file bytes read back at load (bounded RAM).
const MAX_GENERATION_FILE_BYTES: u64 = 512 * 1024 * 1024;

/// Tuning knobs of one service (see [`IndexService::set_config`]).
#[derive(Debug, Clone, Copy)]
pub struct ServiceConfig {
    /// Poll cadence of the background reconciliation worker.
    pub poll: Duration,
    /// Minimum interval between full fingerprint reconciliations of an
    /// idle ready workspace (heals lost watcher events).
    pub fingerprint_interval: Duration,
    /// Lease after which an in-process build is assumed dead (crash) and
    /// reclaimable by another builder.
    pub build_lease: Duration,
    /// Content batch budget: maximum regular files one batch observes.
    pub batch_files: usize,
    /// Content batch budget: maximum directories one batch opens.
    pub batch_dirs: usize,
    /// Content batch budget: maximum bytes one batch reads.
    pub batch_bytes: usize,
    /// Fingerprint pass budget: maximum shard files one pass stats.
    pub fp_batch_files: usize,
    /// Fingerprint pass budget: maximum directories one pass opens.
    pub fp_batch_dirs: usize,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            poll: Duration::from_millis(200),
            fingerprint_interval: DEFAULT_FINGERPRINT_INTERVAL,
            build_lease: Duration::from_secs(10 * 60),
            batch_files: BATCH_MAX_FILES,
            batch_dirs: BATCH_MAX_DIRS,
            batch_bytes: BATCH_MAX_BYTES,
            fp_batch_files: FP_BATCH_FILES,
            fp_batch_dirs: FP_BATCH_DIRS,
        }
    }
}

/// Typed service errors. Every error is advisory to the caller: the runtime
/// falls back to the bounded scan; the machine records durable failures.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("index store error: {0}")]
    Store(#[from] faktor_store::StoreError),
    #[error("workspace {0} is not a known workspace: {1}")]
    UnknownWorkspace(u64, String),
    #[error("index state machine error: {0}")]
    State(#[from] StateError),
    #[error("workspace {workspace} index not ready within deadline")]
    Deadline { workspace: u64 },
    #[error("index io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("index build for workspace {workspace} failed: {message}")]
    BuildFailed { workspace: u64, message: String },
    #[error("corrupt generation data for workspace {workspace}: {message}")]
    CorruptGeneration { workspace: u64, message: String },
    /// A durable row that cannot describe any legal machine state with the
    /// value it carries (today: a negative persisted generation). Refused
    /// typed and diagnosable; never normalized into a clean-looking value.
    #[error("corrupt persisted index state: {0}")]
    CorruptState(String),
    /// The process authority the index needs for cold git/rg children could
    /// not be initialized (standalone [`IndexService::open`] path only:
    /// daemon graphs inject the daemon's supervisor and never hit this).
    #[error("index process authority unavailable: {0}")]
    ProcessAuthority(String),
}

/// One immutable published-generation snapshot: the content is an `Arc`
/// that outlives any later swap, so reads never observe a partial `g+1`.
#[derive(Clone)]
pub struct IndexView {
    workspace: WorkspaceId,
    generation: u64,
    index: Arc<Mutex<WorkspaceIndex>>,
    coverage: IndexCoverage,
    fingerprint_coverage: FingerprintCoverage,
    freshness: EvidenceFreshness,
    content_identity: String,
    fingerprint_identity: String,
}

impl IndexView {
    pub fn workspace(&self) -> WorkspaceId {
        self.workspace
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The immutable content of this generation, in the shape the search
    /// layer consumes (`SearchService::new`).
    pub fn index(&self) -> Arc<Mutex<WorkspaceIndex>> {
        self.index.clone()
    }

    /// Durable coverage of this generation (audit 5): an incomplete record
    /// means a retrieval miss must consult the bounded direct filesystem/Git
    /// fallback before claiming "no match".
    pub fn coverage(&self) -> &IndexCoverage {
        &self.coverage
    }

    /// Durable fingerprint coverage of this generation (audit 6).
    pub fn fingerprint_coverage(&self) -> &FingerprintCoverage {
        &self.fingerprint_coverage
    }

    /// Typed freshness of this generation (audit 16).
    pub fn freshness(&self) -> EvidenceFreshness {
        self.freshness
    }

    /// Workspace content identity (per-generation digest).
    pub fn content_identity(&self) -> &str {
        &self.content_identity
    }

    /// Fingerprint identity (digest of the persisted fingerprint list).
    pub fn fingerprint_identity(&self) -> &str {
        &self.fingerprint_identity
    }

    /// The identity/freshness metadata every evidence package built from
    /// this view carries (audit 16).
    pub fn evidence_package_meta(&self) -> EvidencePackageMeta {
        EvidencePackageMeta {
            generation: Some(self.generation),
            content_identity: Some(self.content_identity.clone()),
            fingerprint_identity: Some(self.fingerprint_identity.clone()),
            freshness: self.freshness,
            coverage: self.coverage.clone(),
            fingerprint: self.fingerprint_coverage.clone(),
        }
    }

    /// A retrieval miss under an incomplete generation must use the bounded
    /// direct filesystem/Git fallback (audit 5).
    pub fn miss_needs_fallback(&self) -> bool {
        self.coverage.needs_fallback_on_miss()
    }
}

impl std::fmt::Debug for IndexView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexView")
            .field("workspace", &self.workspace)
            .field("generation", &self.generation)
            .field("coverage", &self.coverage)
            .field("freshness", &self.freshness)
            .finish_non_exhaustive()
    }
}

/// Per-workspace live record (mirror of the durable row + runtime state).
struct LiveWs {
    /// Mirror of the store row's state.
    state: WorkspaceIndexState,
    /// Mirror of the store row's numeric generation.
    row_generation: u64,
    /// Byte-exact `state_json` of the current store row (CAS equality).
    state_json: String,
    root: Option<PathBuf>,
    handle: Option<Arc<WorkspaceHandle>>,
    /// Published content (always for the newest published generation).
    content: Option<Arc<Mutex<WorkspaceIndex>>>,
    content_generation: u64,
    /// Fingerprint stored in the published generation file.
    last_fingerprint: Option<Vec<FingerprintEntry>>,
    /// Durable coverage of the published generation (audit 5). `None` while
    /// no generation content was loaded (treated as complete only when a
    /// published generation exists).
    coverage: Option<IndexCoverage>,
    /// Durable fingerprint coverage of the published generation (audit 6).
    fingerprint_coverage: Option<FingerprintCoverage>,
    /// Content identity of the published generation (audit 16).
    content_identity: String,
    /// Fingerprint identity of the published generation (audit 16).
    fingerprint_identity: String,
    /// A build is in flight IN THIS PROCESS (lease-guarded).
    building: bool,
    building_since: Option<Instant>,
    /// Events/retries arrived since the last build started (coalesced).
    pending: bool,
    /// A fingerprint pass proved the disk differs (or could not be read):
    /// the Ready generation must hop through `Dirty` into the next
    /// generation. Set by the build, consumed by [`IndexService::decide_next`].
    dirty_now: bool,
    /// The published generation's content is not loaded yet.
    pending_load: bool,
    last_fp_check: Instant,
}

struct Inner {
    store: Arc<Store>,
    data_root: PathBuf,
    fs: Arc<WorkspaceFileService>,
    /// The daemon's ONE process supervisor, injected at open time and
    /// carried into every cold git/rg runner this service builds. No global
    /// fallback: the index never constructs its own process authority.
    supervisor: Arc<ProcessSupervisor>,
    cfg: Mutex<ServiceConfig>,
    live: Mutex<HashMap<WorkspaceId, LiveWs>>,
    /// Last generation whose publication was LOGGED per workspace raw id:
    /// the ~30s reconciliation re-publishes and re-runs the publish path,
    /// and an unchanged generation must not re-log (the old line repeated
    /// every tick forever).
    published_logged: Mutex<HashMap<u64, i64>>,
    /// OPTIONAL build-time embedding source plus the operator-pinned model
    /// identity. `None` = the build carries persisted vectors forward but
    /// never calls out; search can still use them.
    embedding: Mutex<Option<(Arc<dyn EmbeddingSource>, EmbeddingModel)>>,
    notify: tokio::sync::Notify,
    worker_started: AtomicBool,
    /// Explicit lifecycle owner of the single reconciliation worker: the
    /// retained `JoinHandle` + its cancellation token + the health record.
    /// Owned by the service for the worker's whole life — never a detached
    /// task and never a dropped handle.
    worker: Mutex<WorkerSupervisor>,
    /// True while a blocking reconciliation pass is executing. A pass runs
    /// on the blocking pool, which cannot be force-killed: this flag makes
    /// the residual bound of [`IndexService::shutdown_worker`] observable
    /// (typed, via [`WorkerStatus::pass_in_flight`]) instead of implicit.
    pass_in_flight: AtomicBool,
    /// Test-only worker fault seam (per service instance, so worker tests
    /// never race each other through global state).
    #[cfg(test)]
    fault: WorkerFault,
    /// Test-only override of the shutdown join bound in ms (0 = the
    /// production 5s bound): drives the `Aborted` path without waiting.
    #[cfg(test)]
    shutdown_bound_ms: AtomicU64,
    /// Test-only artificial filesystem latency (ms) applied at every walk
    /// work-unit boundary (audit 20): a deliberately slow filesystem the
    /// cancellation/shutdown bound can be proven against. 0 = real FS.
    #[cfg(test)]
    slow_fs_ms: AtomicU64,
}

/// The repository index service: durable state machine + generation store +
/// watcher-driven reconciliation worker.
pub struct IndexService {
    inner: Arc<Inner>,
}

/// One regular file read by a content batch (bytes stay in RAM only for the
/// duration of the batch; the byte budget bounds the batch).
struct BatchFile {
    rel: String,
    bytes: Vec<u8>,
    modified_ms: i64,
}

/// Outcome of one bounded content batch (audit 5).
struct ContentBatch {
    files: Vec<BatchFile>,
    /// Regular files the walker observed (indexed or skipped).
    files_seen: u64,
    /// Files actually read and indexed.
    indexed: u64,
    /// Bytes read and indexed.
    bytes: u64,
    /// Resume position after the batch.
    cursor: ScanCursor,
    /// True when the walk exhausted the tree.
    complete: bool,
    truncated_reason: Option<String>,
    /// The cancellation token fired during the walk (audit 20): the batch
    /// is NOT indexed and the build is abandoned, never published.
    cancelled: bool,
}

/// Outcome of one bounded fingerprint shard pass (audit 6).
struct FingerprintPass {
    entries: Vec<FingerprintEntry>,
    shard_completed: bool,
    cursor: ScanCursor,
    reason: Option<String>,
    /// The cancellation token fired during the pass (audit 20): nothing is
    /// merged and the build is abandoned, never published.
    cancelled: bool,
}

/// Walker control returned by the per-file visitor.
enum WalkControl {
    Continue,
    Stop(&'static str),
}

/// Outcome of one bounded walk (audit 20): the resume cursor, whether the
/// tree was exhausted, the budget reason when it stopped early, and whether
/// the caller's cancellation token fired at a work-unit boundary.
struct WalkOutcome {
    cursor: ScanCursor,
    complete: bool,
    reason: Option<String>,
    cancelled: bool,
}

impl WalkOutcome {
    fn done(cursor: ScanCursor, complete: bool, reason: Option<String>) -> Self {
        Self {
            cursor,
            complete,
            reason,
            cancelled: false,
        }
    }

    fn cancelled(cursor: ScanCursor) -> Self {
        Self {
            cursor,
            complete: false,
            reason: Some("cancelled".to_string()),
            cancelled: true,
        }
    }
}

/// The slow-filesystem test seam (audit 20): sleeps for the configured
/// artificial latency of ONE walk work unit. `None` — every production
/// path — is a no-op.
fn pace_walk_unit(pace: Option<Duration>) {
    if let Some(delay) = pace {
        std::thread::sleep(delay);
    }
}

/// One directory frame of the deterministic batch walk. Entries are sorted
/// by name; `idx` points at the next entry (resume skips via `after`).
struct WalkFrame {
    dir: PathBuf,
    rel: String,
    entries: Vec<(String, WalkKind)>,
    idx: usize,
    /// Resume high-water recorded for this directory when `idx == 0`.
    after: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WalkKind {
    Dir,
    File,
    Other,
}

/// Build/crash seam hook for adversarial tests, fired at named points of
/// the publish pipeline. A hook may panic to simulate a builder killed
/// mid-build (the durable state stays `Building`; content is never
/// published; no torn reads are possible).
#[cfg(test)]
type SeamHook = Box<dyn Fn(u64, u64, &'static str) + Send>;
#[cfg(test)]
static SEAM: OnceLock<Mutex<Option<SeamHook>>> = OnceLock::new();

#[cfg(test)]
fn fire_seam(ws: u64, generation: u64, point: &'static str) {
    if let Some(lock) = SEAM.get() {
        // Take the hook OUT of the mutex before invoking: a hook that
        // panics (the crash simulation) must never poison the seam lock.
        // A hook that RETURNS normally stays armed for later seam points of
        // the same pipeline — a test targets one point by name and the
        // first point must not consume it; tests clear it explicitly.
        let hook = lock_recover(lock).take();
        if let Some(hook) = hook {
            hook(ws, generation, point);
            *lock_recover(lock) = Some(hook);
        }
    }
}

#[cfg(not(test))]
fn fire_seam(_ws: u64, _generation: u64, _point: &'static str) {}

#[cfg(test)]
pub(crate) fn install_seam(hook: SeamHook) {
    let lock = SEAM.get_or_init(|| Mutex::new(None));
    *lock_recover(lock) = Some(hook);
}

#[cfg(test)]
pub(crate) fn clear_seam() {
    if let Some(lock) = SEAM.get() {
        *lock_recover(lock) = None;
    }
}

impl IndexService {
    /// Open (creating when needed) an index service rooted at `data_root`,
    /// persisted through `store`, watching workspaces via `fs`.
    ///
    /// Standalone form: the cold git/rg authority comes from
    /// [`ProcessSupervisor::try_shared`], and a failure to initialize it is
    /// a typed [`IndexError::ProcessAuthority`] — never a panic and never an
    /// unenforced child. Daemon graphs must call
    /// [`IndexService::open_with_supervisor`] with the daemon's ONE
    /// supervisor instead.
    pub fn open(
        store: Arc<Store>,
        data_root: PathBuf,
        fs: Arc<WorkspaceFileService>,
    ) -> Result<Arc<Self>, IndexError> {
        let supervisor = ProcessSupervisor::try_shared()
            .map_err(|e| IndexError::ProcessAuthority(e.to_string()))?;
        Self::open_with_supervisor(store, data_root, fs, supervisor)
    }

    /// [`IndexService::open`] over an INJECTED process supervisor: every
    /// cold git/rg child this service builds runs under the daemon's single
    /// `ProcessSupervisor` (bounded registry, daemon-shutdown scope). No
    /// global fallback exists on this path.
    pub fn open_with_supervisor(
        store: Arc<Store>,
        data_root: PathBuf,
        fs: Arc<WorkspaceFileService>,
        supervisor: Arc<ProcessSupervisor>,
    ) -> Result<Arc<Self>, IndexError> {
        fs::create_dir_all(data_root.join("generations"))?;
        fs::create_dir_all(data_root.join("scratch"))?;
        // Crash residue from a killed builder must not accumulate: sweep
        // stale atomic/staging temps across every workspace generation dir
        // (bounded; regular files only, age-gated).
        let sweep = sweep_all_stale_temp_orphans(&data_root);
        log_sweep_summary("open", 0, sweep);
        Ok(Arc::new(Self {
            inner: Arc::new(Inner {
                store,
                data_root,
                fs,
                supervisor,
                cfg: Mutex::new(ServiceConfig::default()),
                live: Mutex::new(HashMap::new()),
                published_logged: Mutex::new(HashMap::new()),
                embedding: Mutex::new(None),
                notify: tokio::sync::Notify::new(),
                worker_started: AtomicBool::new(false),
                worker: Mutex::new(WorkerSupervisor::idle()),
                pass_in_flight: AtomicBool::new(false),
                #[cfg(test)]
                fault: WorkerFault::default(),
                #[cfg(test)]
                shutdown_bound_ms: AtomicU64::new(0),
                #[cfg(test)]
                slow_fs_ms: AtomicU64::new(0),
            }),
        }))
    }

    /// The artificial walk-unit latency of the slow-filesystem test seam
    /// (audit 20). Always `None` in production builds: the seam exists only
    /// so an adversarial test can hold a scan in flight long enough to prove
    /// cancellation is observed between files/directories and shutdown
    /// stays inside its hard bound.
    fn scan_pace(&self) -> Option<Duration> {
        #[cfg(test)]
        {
            let ms = self.inner.slow_fs_ms.load(Ordering::Acquire);
            (ms > 0).then(|| Duration::from_millis(ms))
        }
        #[cfg(not(test))]
        {
            None
        }
    }

    /// Tune the reconciliation knobs (operator/test surface).
    pub fn set_config(&self, cfg: ServiceConfig) {
        *lock_recover(&self.inner.cfg) = cfg;
    }

    fn cfg_of(inner: &Inner) -> ServiceConfig {
        *lock_recover(&inner.cfg)
    }

    /// Configure (or clear) the build-time embedding source and the
    /// operator-pinned model identity. Idempotent and cheap; it takes effect
    /// on the NEXT build (an in-flight build keeps the source it started
    /// with). `None` keeps the durable vectors and never calls out.
    ///
    /// A configured source makes every build carry matching persisted
    /// vectors forward (keyed by content hash + model + revision +
    /// dimension), embedding only chunks that changed, are new, or belong to
    /// a new model/revision — an unchanged corpus is never re-embedded.
    pub fn set_embedding_source(
        &self,
        source: Option<Arc<dyn EmbeddingSource>>,
        model: EmbeddingModel,
    ) {
        *lock_recover(&self.inner.embedding) = source.map(|source| (source, model));
    }

    /// The source + identity a build must use, snapshotted under the lock.
    fn embedding_config(&self) -> Option<(Arc<dyn EmbeddingSource>, EmbeddingModel)> {
        lock_recover(&self.inner.embedding).clone()
    }

    /// Carry matching prior vectors and embed only the batch's missing
    /// chunks (see [`apply_embeddings`]); logs the typed outcome. The
    /// accumulation's OWN vectors are the prior on resumed batches, so a
    /// batch never re-embeds earlier batches' work.
    fn embed_accumulated(
        &self,
        workspace: WorkspaceId,
        ws_raw: u64,
        target: u64,
        index: &mut WorkspaceIndex,
        batch_chunks: &[(String, Vec<String>)],
        source_config: Option<(Arc<dyn EmbeddingSource>, EmbeddingModel)>,
    ) {
        match source_config {
            Some((source, model)) => {
                let prior = index
                    .embedding_index(workspace)
                    .cloned()
                    .or_else(|| self.prior_embeddings(workspace));
                let stats = apply_embeddings(
                    index,
                    workspace,
                    prior.as_ref(),
                    &model,
                    batch_chunks,
                    Some(source.as_ref()),
                );
                if stats.degraded {
                    tracing::warn!(
                        workspace = ws_raw,
                        generation = target,
                        carried = stats.carried,
                        "index embeddings degraded for this build (lexical/symbol retrieval is unaffected)"
                    );
                } else if stats.carried > 0 || stats.embedded > 0 {
                    tracing::info!(
                        workspace = ws_raw,
                        generation = target,
                        carried = stats.carried,
                        embedded = stats.embedded,
                        calls = stats.calls,
                        "index embeddings settled"
                    );
                }
            }
            None => {
                if index.embedding_index(workspace).is_none() {
                    if let Some(prior) = self.prior_embeddings(workspace) {
                        index.replace_embeddings(workspace, prior);
                    }
                }
            }
        }
    }

    /// The previously published embedding index for `workspace`: the
    /// in-memory published generation when it is loaded, else the newest
    /// generation file on disk (a Dirty-at-restart build resumes before the
    /// Ready content is materialized). Hostile/missing files degrade to
    /// `None` (a full re-embed, never a wrong reuse).
    fn prior_embeddings(&self, workspace: WorkspaceId) -> Option<EmbeddingIndex> {
        {
            let live = lock_recover(&self.inner.live);
            if let Some(l) = live.get(&workspace) {
                if let Some(content) = &l.content {
                    if let Ok(index) = content.lock() {
                        if let Some(embeddings) = index.embedding_index(workspace) {
                            return Some(embeddings.clone().sanitize());
                        }
                    }
                    return Some(EmbeddingIndex::default());
                }
            }
        }
        newest_generation_embeddings(&self.inner.data_root, workspace)
    }

    /// Start the single background reconciliation worker of this service
    /// (idempotent). Requires a tokio runtime context; without one the
    /// service keeps working synchronously through
    /// [`IndexService::ensure_ready`] / [`IndexService::reconcile_now`].
    ///
    /// The task is explicitly OWNED: its `JoinHandle` is retained by the
    /// service (see [`WorkerSupervisor`]), [`IndexService::shutdown_worker`]
    /// cancels and joins it, and [`IndexService::worker_status`] reports its
    /// state. A panicking pass stops the worker loudly ([`WorkerState::Failed`],
    /// started flag cleared) — never an immediate respawn; a later explicit
    /// call starts a new generation and bumps the restart counter.
    pub fn spawn_worker(&self) -> bool {
        let inner = self.inner.clone();
        let mut reconciled_dead = false;
        loop {
            if inner
                .worker_started
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break;
            }
            if reconciled_dead {
                // A concurrent spawner won the flag after we reconciled the
                // dead generation: it is a live worker, idempotent success.
                return true;
            }
            // The flag is set: a live worker (idempotent success) or a task
            // body that died without recording a terminal state (e.g. a
            // panic in the async body itself). Reconcile the latter so the
            // retry can start a fresh generation instead of wedging on a
            // dead one.
            let mut worker = lock_worker_supervisor(&inner);
            if !reconcile_dead_worker(&mut worker, &inner.worker_started) {
                return true;
            }
            reconciled_dead = true;
        }
        if tokio::runtime::Handle::try_current().is_err() {
            inner.worker_started.store(false, Ordering::Release);
            return false;
        }
        let weak = Arc::downgrade(&inner);
        let mut worker = lock_worker_supervisor(&inner);
        if worker.handle.is_some() || worker.state != WorkerState::NotStarted {
            worker.restarts = worker.restarts.saturating_add(1);
        }
        worker.generation = worker.generation.wrapping_add(1);
        let generation = worker.generation;
        let cancel = CancellationToken::new();
        worker.cancel = cancel.clone();
        worker.state = WorkerState::Running;
        worker.handle = Some(tokio::spawn(worker_loop(weak, cancel, generation)));
        true
    }

    /// Stop the OWNED reconciliation worker and JOIN it, bounded. Cancellation
    /// is signalled first; the worker task observes it and JOINS its in-flight
    /// `spawn_blocking` pass before returning (audit 20) instead of leaving it
    /// detached on the blocking pool. The pass itself observes the token
    /// between work units: scan walks check between files/directories, batch
    /// indexing between files, and the machine between steps — so a
    /// deliberately slow filesystem exits after at most the ONE syscall in
    /// flight.
    ///
    /// HONEST RESIDUAL BOUND: only a genuinely uncancellable unit (one slow
    /// syscall or embedding call already executing) can outlive the bound. If
    /// that happens the worker task is aborted and the still-running pass is
    /// reported TYPED via [`WorkerStatus::pass_in_flight`] (logged here too),
    /// so the real exit bound is never implicit.
    pub async fn shutdown_worker(&self) -> WorkerShutdown {
        /// Bounded join window of the owned worker task.
        const SHUTDOWN_BOUND: Duration = Duration::from_secs(5);
        let shutdown_bound = {
            #[cfg(test)]
            {
                let ms = self.inner.shutdown_bound_ms.load(Ordering::Acquire);
                if ms == 0 {
                    SHUTDOWN_BOUND
                } else {
                    Duration::from_millis(ms)
                }
            }
            #[cfg(not(test))]
            {
                SHUTDOWN_BOUND
            }
        };
        let handle = {
            let mut worker = lock_worker_supervisor(&self.inner);
            worker.cancel.cancel();
            self.inner.worker_started.store(false, Ordering::Release);
            if worker.state == WorkerState::Running {
                worker.state = WorkerState::Stopped;
            }
            worker.handle.take()
        };
        let Some(mut handle) = handle else {
            return WorkerShutdown::NotRunning;
        };
        match tokio::time::timeout(shutdown_bound, &mut handle).await {
            Ok(Ok(())) => WorkerShutdown::Joined,
            Ok(Err(join_err)) => {
                // The owned task ended by panic/abort while we waited: it is
                // still joined (nothing detached) and the failure is
                // recorded, never discarded.
                let message = format!("worker task ended with {join_err}");
                tracing::error!(error = %message, "index reconciliation worker joined with a failure");
                let mut worker = lock_worker_supervisor(&self.inner);
                if worker.last_error.is_none() {
                    worker.last_error = Some(message);
                }
                worker.state = WorkerState::Failed;
                WorkerShutdown::Joined
            }
            Err(_) => {
                handle.abort();
                let _ = handle.await;
                let pass_in_flight = self.inner.pass_in_flight.load(Ordering::Acquire);
                tracing::error!(
                    bound_ms = shutdown_bound.as_millis() as u64,
                    pass_in_flight,
                    "index reconciliation worker did not stop within the bound; its async task \
                     was aborted and reaped{}",
                    if pass_in_flight {
                        " — a blocking pass is STILL RUNNING and sets the real exit bound \
                         (reported typed via WorkerStatus::pass_in_flight)"
                    } else {
                        ""
                    }
                );
                let mut worker = lock_worker_supervisor(&self.inner);
                worker.state = WorkerState::Stopped;
                WorkerShutdown::Aborted
            }
        }
    }

    /// Health snapshot of the background reconciliation worker (lifecycle
    /// state, last error, restart count) for health/doctor surfaces. A task
    /// body that died without recording a terminal state (a panic outside
    /// the blocking pass) is reconciled here — a dead worker is never
    /// reported as `Running`.
    pub fn worker_status(&self) -> WorkerStatus {
        let mut worker = lock_worker_supervisor(&self.inner);
        reconcile_dead_worker(&mut worker, &self.inner.worker_started);
        worker.status(self.inner.pass_in_flight.load(Ordering::Acquire))
    }

    /// The workspace's LIVE shadow root, when a shadowed single-agent drive
    /// currently re-points it (P0-48 root re-pointing): `Some(root)`
    /// exactly when ONE session of this workspace carries a durable live
    /// shadow row (state `active` / `integration_blocked`). `Ok(None)` with
    /// no live shadow — the stored workspace root stays authoritative — and
    /// also (loudly, logged) for ambiguous or corrupt registry state: the
    /// index resolves from the stored root and NEVER guesses a shadow.
    ///
    /// The registry is the session crate's durable fact space, consumed
    /// here OPACELY (faktor-index must not depend on faktor-session): the
    /// row value is parsed tolerantly; an unrecognizable shape degrades to
    /// the stored root with a loud log, exactly like a hostile row must.
    fn workspace_live_shadow_root(
        &self,
        workspace: WorkspaceId,
    ) -> Result<Option<PathBuf>, IndexError> {
        let sessions = self.inner.store.list_sessions(Some(workspace))?;
        let mut found: Option<PathBuf> = None;
        for srow in sessions {
            let facts = self.inner.store.memory_facts(srow.id)?;
            for (kind, key, value) in facts {
                if kind != SHADOW_ROW_KIND || key != SHADOW_ROW_KEY {
                    continue;
                }
                match parse_live_shadow_row(&value) {
                    Ok(Some(root)) => {
                        if found.is_some() {
                            tracing::warn!(
                                workspace = workspace.raw(),
                                "more than one live shadow on this workspace; the index resolves from the stored workspace root (never a guess)"
                            );
                            return Ok(None);
                        }
                        found = Some(root);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::error!(
                            workspace = workspace.raw(),
                            error = %e,
                            "corrupt shadow registry row; the index resolves from the stored workspace root (never a guess)"
                        );
                        return Ok(None);
                    }
                }
            }
        }
        Ok(found)
    }

    /// Register a workspace: mirror its durable state row, open its watcher
    /// handle, and kick the reconciliation worker. Idempotent and cheap;
    /// NEVER blocks on a build. Failures (unknown/unresolvable workspace)
    /// are errors the caller turns into fallback behavior.
    pub fn attach(&self, workspace: WorkspaceId) -> Result<(), IndexError> {
        {
            let live = lock_recover(&self.inner.live);
            if live.contains_key(&workspace) {
                drop(live);
                self.inner.notify.notify_one();
                self.spawn_worker_if_possible();
                return Ok(());
            }
        }
        // Crash-residue sweep (fresh attach only): stale atomic/staging
        // temps in this workspace's generation/scratch dirs are
        // bounded-cleaned before the machine resumes. Age-gated and
        // regular-file-only: live temps, symlinks and real generation files
        // survive.
        let sweep = sweep_stale_temp_orphans(&self.inner.data_root, workspace);
        log_sweep_summary("attach", workspace.raw(), sweep);
        // Fresh workspace: resolve root + watcher handle. P0-48: while
        // exactly ONE session carries a LIVE shadow row, the index resolves
        // from the SHADOW root (evidence/watch reflect the mutated world).
        // Zero shadows keep the stored root; ambiguity or a corrupt registry
        // degrades to the stored root loudly (never a guessed root).
        let root = match self.workspace_live_shadow_root(workspace)? {
            Some(shadow) => shadow,
            None => {
                PathBuf::from(self.inner.store.workspace_root(workspace)?.ok_or_else(|| {
                    IndexError::UnknownWorkspace(workspace.raw(), "no row".into())
                })?)
            }
        };
        let handle = match self.inner.fs.open(workspace, root.clone()) {
            Ok(h) => Some(Arc::new(h)),
            Err(e) => {
                tracing::warn!(
                    workspace = workspace.raw(),
                    "index attach: watcher open failed ({}); fingerprint reconciliation only",
                    e
                );
                None
            }
        };
        // Durable row mirror; corrupt payloads fail OPEN as a durable
        // Failed row (never a silent NotStarted default).
        let (mut state, mut row_generation, mut state_json) =
            match self.inner.store.index_state_get(workspace)? {
                Some(row) => {
                    // A negative persisted generation is typed corruption: it
                    // can never be "cleaned" by normalizing to 0 (that would
                    // destroy the forensic value and make corruption look like
                    // a clean initial generation).
                    let row_generation = persisted_generation(workspace, row.generation)?;
                    match PersistedIndexState::parse(row.state_json.clone(), row.generation) {
                        Ok(p) => (p.state, row_generation, p.state_json),
                        Err(e) => {
                            let msg = format!("corrupt persisted index state: {e}");
                            tracing::error!(workspace = workspace.raw(), "{msg}");
                            let failed = WorkspaceIndexState::Failed {
                                message: truncate(&msg, 256),
                            };
                            let failed_json = failed.to_row_json();
                            self.inner.store.index_state_put(
                                workspace,
                                &failed_json,
                                row.generation,
                                JOURNAL_CORRUPT,
                            )?;
                            (failed, row_generation, failed_json)
                        }
                    }
                }
                None => {
                    // Seed the durable row so CAS transitions have a target.
                    let ns = WorkspaceIndexState::NotStarted;
                    self.inner.store.index_state_put(
                        workspace,
                        &ns.to_row_json(),
                        0,
                        JOURNAL_NOT_STARTED,
                    )?;
                    (ns.clone(), 0, ns.to_row_json())
                }
            };
        // Torn-publish recovery: the row says Ready{g} but the generation
        // file is absent (crash between the publish CAS and the rename):
        // heal durably through the machine (Ready -> Dirty -> rebuild).
        if let WorkspaceIndexState::Ready { generation } = &state {
            let gen = *generation;
            if !generation_file_path(&self.inner.data_root, workspace, gen).exists() {
                tracing::warn!(
                    workspace = workspace.raw(),
                    generation = gen,
                    "torn publish heal: ready generation file missing"
                );
                let dirty = WorkspaceIndexState::Dirty { generation: gen };
                state.check_transition(row_generation, &dirty, gen)?;
                let ok = self.inner.store.index_state_cas(
                    workspace,
                    &state_json,
                    row_generation as i64,
                    &dirty.to_row_json(),
                    gen as i64,
                    JOURNAL_TORN_READY,
                )?;
                if ok {
                    state = dirty;
                    state_json = state.to_row_json();
                    row_generation = gen;
                }
            }
        }
        let pending = !matches!(
            state,
            WorkspaceIndexState::Ready { .. } | WorkspaceIndexState::Failed { .. }
        );
        let pending_load = matches!(
            state,
            WorkspaceIndexState::Ready { .. } | WorkspaceIndexState::Dirty { .. }
        );
        let cfg = Self::cfg_of(&self.inner);
        {
            let mut live = lock_recover(&self.inner.live);
            live.insert(
                workspace,
                LiveWs {
                    state,
                    row_generation,
                    state_json,
                    root: Some(root),
                    handle,
                    content: None,
                    content_generation: 0,
                    last_fingerprint: None,
                    coverage: None,
                    fingerprint_coverage: None,
                    content_identity: String::new(),
                    fingerprint_identity: String::new(),
                    building: false,
                    building_since: None,
                    pending,
                    dirty_now: false,
                    pending_load,
                    last_fp_check: Instant::now() - cfg.fingerprint_interval,
                },
            );
        }
        self.inner.notify.notify_one();
        self.spawn_worker_if_possible();
        Ok(())
    }

    fn spawn_worker_if_possible(&self) {
        let _ = self.spawn_worker();
    }

    /// Immediate view of the newest PUBLISHED generation of `workspace`.
    /// `None` while the first build is in flight (no Ready generation yet),
    /// or while the persisted Ready content is still being reloaded. Never
    /// blocks and never triggers work.
    pub fn view(&self, workspace: WorkspaceId) -> Option<IndexView> {
        let live = lock_recover(&self.inner.live);
        let l = live.get(&workspace)?;
        if matches!(l.state, WorkspaceIndexState::NotStarted) || l.pending_load {
            return None;
        }
        let index = l.content.as_ref()?;
        let coverage = l
            .coverage
            .clone()
            .unwrap_or_else(IndexCoverage::legacy_unknown);
        let fingerprint_coverage = l
            .fingerprint_coverage
            .clone()
            .unwrap_or_else(FingerprintCoverage::legacy_unknown);
        // Freshness (audit 16 + legacy migration): pre-coverage generation
        // LEGACY_UNKNOWN, incomplete coverage PARTIAL, complete + rebuild in
        // flight STALE_WHILE_REBUILDING, otherwise CURRENT.
        let freshness = EvidenceFreshness::classify(
            &coverage,
            &fingerprint_coverage,
            l.pending
                || l.building
                || matches!(
                    l.state,
                    WorkspaceIndexState::Dirty { .. } | WorkspaceIndexState::Building { .. }
                ),
        );
        Some(IndexView {
            workspace,
            generation: l.content_generation,
            index: index.clone(),
            coverage,
            fingerprint_coverage,
            freshness,
            content_identity: l.content_identity.clone(),
            fingerprint_identity: l.fingerprint_identity.clone(),
        })
    }

    /// Probe the mirrored durable state (tests/observability).
    pub fn state(&self, workspace: WorkspaceId) -> Option<(WorkspaceIndexState, u64)> {
        let live = lock_recover(&self.inner.live);
        live.get(&workspace)
            .map(|l| (l.state.clone(), l.row_generation))
    }

    /// Durable content coverage of one attached workspace (audit 5). `None`
    /// when unattached; an incomplete record — including the legacy-unknown
    /// marker of a pre-coverage generation — means batches continue.
    pub fn coverage(&self, workspace: WorkspaceId) -> Option<IndexCoverage> {
        let live = lock_recover(&self.inner.live);
        let l = live.get(&workspace)?;
        Some(
            l.coverage
                .clone()
                .unwrap_or_else(IndexCoverage::legacy_unknown),
        )
    }

    /// Durable fingerprint coverage of one attached workspace (audit 6): an
    /// absent record is legacy-unknown, never fully clean.
    pub fn fingerprint_coverage(&self, workspace: WorkspaceId) -> Option<FingerprintCoverage> {
        let live = lock_recover(&self.inner.live);
        let l = live.get(&workspace)?;
        Some(
            l.fingerprint_coverage
                .clone()
                .unwrap_or_else(FingerprintCoverage::legacy_unknown),
        )
    }

    /// Typed coverage/freshness diagnostics of one attached workspace (the
    /// `/native` coverage route shape, audit 5/6): a pure mirror read.
    pub fn coverage_snapshot(&self, workspace: WorkspaceId) -> Option<IndexCoverageSnapshot> {
        let live = lock_recover(&self.inner.live);
        let l = live.get(&workspace)?;
        let coverage = l
            .coverage
            .clone()
            .unwrap_or_else(IndexCoverage::legacy_unknown);
        let fingerprint = l
            .fingerprint_coverage
            .clone()
            .unwrap_or_else(FingerprintCoverage::legacy_unknown);
        let published = l.content.as_ref().map(|_| l.content_generation);
        // Wire-safe: legacy-unknown -> partial (strict IDE-panel contract).
        let freshness = EvidenceFreshness::classify(
            &coverage,
            &fingerprint,
            l.pending
                || l.building
                || matches!(
                    l.state,
                    WorkspaceIndexState::Dirty { .. } | WorkspaceIndexState::Building { .. }
                ),
        )
        .wire();
        Some(IndexCoverageSnapshot {
            workspace: workspace.raw(),
            state: index_state_label(&l.state).to_string(),
            generation: l.row_generation,
            published_generation: published,
            coverage,
            fingerprint,
            freshness,
            serving: l.content.is_some(),
        })
    }

    /// The cheap pre-Ready evidence provider of one attached workspace
    /// (P0-30): while no Ready view exists, the runtime serves cold evidence
    /// from this provider — persisted OLD generations first, then targeted
    /// reads — instead of the legacy scan. `None` when unattached.
    pub fn cold_provider(&self, workspace: WorkspaceId) -> Option<ColdEvidenceProvider> {
        let live = lock_recover(&self.inner.live);
        let l = live.get(&workspace)?;
        let root = l.root.clone()?;
        Some(ColdEvidenceProvider::new(
            root,
            workspace,
            self.inner.data_root.join("generations"),
            self.inner.supervisor.clone(),
        ))
    }

    /// Explicit retry (e.g. after [`WorkspaceIndexState::Failed`]) or event
    /// kick: marks the workspace pending and wakes the worker. Never blocks.
    pub fn request_build(&self, workspace: WorkspaceId) -> Result<(), IndexError> {
        self.attach(workspace)?;
        {
            let mut live = lock_recover(&self.inner.live);
            if let Some(l) = live.get_mut(&workspace) {
                l.pending = true;
            }
        }
        self.inner.notify.notify_one();
        self.spawn_worker_if_possible();
        Ok(())
    }

    /// Blocking readiness: waits until the workspace's machine is at rest on
    /// a PUBLISHED generation (`Ready`, no pending rebuild) and returns that
    /// view — driving builds synchronously when no worker is running (tests,
    /// retry paths). A stale-but-readable generation is served only after the
    /// deadline passes. The runtime NEVER calls this on the first-prompt path
    /// (it uses [`IndexService::view`] + attach only).
    pub fn ensure_ready(
        &self,
        workspace: WorkspaceId,
        deadline: Instant,
    ) -> Result<IndexView, IndexError> {
        self.attach(workspace)?;
        let mut stale_view: Option<IndexView> = None;
        loop {
            if self.machine_at_rest(workspace) {
                if let Some(view) = self.view(workspace) {
                    return Ok(view);
                }
            } else if stale_view.is_none() {
                stale_view = self.view(workspace);
            }
            if Instant::now() >= deadline {
                if let Some(view) = stale_view.or_else(|| self.view(workspace)) {
                    return Ok(view); // serve the newest published snapshot
                }
                return Err(IndexError::Deadline {
                    workspace: workspace.raw(),
                });
            }
            // One reconcile pass may claim and run a full build inline.
            let _ = self.reconcile_now(workspace);
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    /// True when the workspace has no pending rebuild: durable state is
    /// Ready (or Failed with no retry pending) and the published content
    /// for that state is loaded.
    fn machine_at_rest(&self, workspace: WorkspaceId) -> bool {
        let live = lock_recover(&self.inner.live);
        match live.get(&workspace) {
            Some(l) => {
                let ready_state = match &l.state {
                    WorkspaceIndexState::Ready { .. } => true,
                    WorkspaceIndexState::Failed { .. } => !l.pending,
                    _ => false,
                };
                // An ABSENT coverage record is legacy-unknown, never
                // complete: the workspace is not at rest, its rebuild is due.
                let coverage_complete = l.coverage.as_ref().map(|c| c.complete).unwrap_or(false);
                let fingerprint_complete = l
                    .fingerprint_coverage
                    .as_ref()
                    .map(|c| c.complete)
                    .unwrap_or(false);
                ready_state
                    && !l.pending
                    && !l.dirty_now
                    && !l.pending_load
                    && !l.building
                    && coverage_complete
                    && fingerprint_complete
                    && l.content.is_some()
                    && l.content_generation == l.state.generation().unwrap_or(u64::MAX)
            }
            None => false,
        }
    }

    /// One synchronous reconciliation pass over one workspace: drain
    /// watcher events, mark dirty durably, load published content, claim
    /// and run at most one build. The worker calls this on the blocking
    /// pool; synchronous callers (ensure_ready) call it directly.
    pub fn reconcile_now(&self, workspace: WorkspaceId) -> Result<(), IndexError> {
        self.reconcile_now_cancellable(workspace, None)
    }

    /// [`IndexService::reconcile_now`] with an optional cancellation check at
    /// every machine-step boundary. The worker's pass passes its generation
    /// token: a shutdown never starts the next step (or the expensive build)
    /// after the signal, while a build already entered still runs to its own
    /// bounded end (blocking code cannot be interrupted).
    fn reconcile_now_cancellable(
        &self,
        workspace: WorkspaceId,
        cancel: Option<&CancellationToken>,
    ) -> Result<(), IndexError> {
        // 1. Drain this workspace's watcher channel (lossy by design; all
        // events coalesce into ONE pending flag — a 1000-event storm is one
        // dirty mark and one rebuild).
        {
            let mut live = lock_recover(&self.inner.live);
            if let Some(l) = live.get_mut(&workspace) {
                if let Some(handle) = &l.handle {
                    let mut rx = lock_recover(handle.events());
                    let mut saw = false;
                    while let Ok(ev) = rx.try_recv() {
                        if ev.workspace_id == workspace
                            && matches!(
                                ev.kind,
                                FsEventKind::Created | FsEventKind::Modified | FsEventKind::Removed
                            )
                        {
                            saw = true;
                        }
                    }
                    if saw {
                        l.pending = true;
                    }
                }
            }
        }
        // 2. Machine steps (bounded loop; at most one build per call).
        for _ in 0..8 {
            if cancel.is_some_and(CancellationToken::is_cancelled) {
                return Ok(());
            }
            let next = self.decide_next(workspace)?;
            match next {
                Next::Idle => return Ok(()),
                Next::Load => self.load_content(workspace)?,
                Next::MarkDirty => self.mark_dirty(workspace)?,
                Next::Claim { target, kind } => {
                    if cancel.is_some_and(CancellationToken::is_cancelled) {
                        // Never START the build after the signal: the claim
                        // stays durable (`Building`) and the next pass or the
                        // build lease resumes it.
                        return Ok(());
                    }
                    if self.claim_build(workspace, target, kind)? {
                        // One claim per reconcile call: run the build
                        // inline (blocking by design; the worker wraps the
                        // whole pass in spawn_blocking). One build runs ONE
                        // bounded batch (content or fingerprint); an
                        // incomplete generation continues on the next pass.
                        // `cancel` is observed INSIDE the scan/serialize
                        // work units (audit 20): a cancelled build abandons
                        // without publishing and leaves the resume durable.
                        let _ = self.run_build(workspace, target, kind, cancel)?;
                        return Ok(());
                    }
                    // A concurrent builder claimed (or a restart advanced)
                    // the row: re-read the durable truth before deciding.
                    self.refresh_mirror(workspace)?;
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Decide the next machine step from the mirrored state.
    fn decide_next(&self, workspace: WorkspaceId) -> Result<Next, IndexError> {
        let cfg = Self::cfg_of(&self.inner);
        let mut live = lock_recover(&self.inner.live);
        let Some(l) = live.get_mut(&workspace) else {
            return Ok(Next::Idle);
        };
        let state = l.state.clone();
        let row_generation = l.row_generation;
        let claimable = {
            let active = l.building
                && l.building_since
                    .map(|t| t.elapsed() < cfg.build_lease)
                    .unwrap_or(false);
            !active
        };
        let now = Instant::now();
        match &state {
            WorkspaceIndexState::NotStarted => {
                if claimable {
                    Ok(Next::Claim {
                        target: 1,
                        kind: JOURNAL_BUILDING,
                    })
                } else {
                    Ok(Next::Idle)
                }
            }
            WorkspaceIndexState::Building { .. } => {
                if claimable {
                    // Durable Building with no live builder: crash residue,
                    // resume the SAME generation the row names.
                    Ok(Next::Claim {
                        target: row_generation,
                        kind: JOURNAL_RESUME,
                    })
                } else {
                    Ok(Next::Idle)
                }
            }
            WorkspaceIndexState::Dirty { .. } => {
                let target = state.next_build_target(row_generation)?;
                if claimable {
                    Ok(Next::Claim {
                        target,
                        kind: JOURNAL_BUILDING,
                    })
                } else {
                    Ok(Next::Idle)
                }
            }
            WorkspaceIndexState::Failed { .. } => {
                if l.pending && claimable {
                    let target = state.next_build_target(row_generation)?;
                    Ok(Next::Claim {
                        target,
                        kind: JOURNAL_BUILDING,
                    })
                } else {
                    Ok(Next::Idle)
                }
            }
            WorkspaceIndexState::Ready { generation } => {
                let gen = *generation;
                // Absent coverage is legacy-unknown (incomplete) and alone
                // schedules the resumable full rebuild of this generation —
                // no unrelated dirtiness event is required.
                let content_complete = l.coverage.as_ref().map(|c| c.complete).unwrap_or(false);
                let fingerprint_complete = l
                    .fingerprint_coverage
                    .as_ref()
                    .map(|c| c.complete)
                    .unwrap_or(false);
                let fp_in_progress = !fingerprint_complete;
                if l.pending_load || l.content_generation != gen || l.content.is_none() {
                    l.pending_load = true;
                    Ok(Next::Load)
                } else if l.dirty_now && claimable {
                    // A fingerprint pass proved the disk differs (or could
                    // not be read): the published generation is superseded
                    // through the Dirty hop. The rebuild either lands the
                    // change or surfaces the unreadable root as a durable
                    // failure — never a silent clean.
                    l.dirty_now = false;
                    l.pending = true;
                    Ok(Next::MarkDirty)
                } else if (!content_complete || fp_in_progress) && claimable {
                    // Batch continuation (audit 5/6): the SAME generation
                    // keeps extending — content batches first, then
                    // fingerprint shard passes — until its coverage is
                    // complete. A capped/partial scan is NEVER at rest.
                    Ok(Next::Claim {
                        target: gen,
                        kind: JOURNAL_CONTINUE,
                    })
                } else if claimable
                    && (l.pending
                        || now.duration_since(l.last_fp_check) >= cfg.fingerprint_interval)
                {
                    // Every rebuild decision is fingerprint-VERIFIED: a
                    // pending watcher event (or the periodic reconciliation
                    // that heals events the lossy fs channel dropped) starts
                    // a ROTATED fingerprint round. `run_build` runs the
                    // bounded shard passes and either supersedes the
                    // generation (durable Dirty) or completes the round —
                    // and only a COMPLETE round can drop the event as stale.
                    l.last_fp_check = now;
                    let previous = l.fingerprint_coverage.clone().unwrap_or_default();
                    l.fingerprint_coverage = Some(FingerprintCoverage::round_at_epoch(
                        previous.round_start,
                        previous.epoch.wrapping_add(1),
                    ));
                    l.dirty_now = false;
                    Ok(Next::Claim {
                        target: gen,
                        kind: JOURNAL_CONTINUE,
                    })
                } else {
                    Ok(Next::Idle)
                }
            }
        }
    }

    /// Re-read the durable row into the mirror (a CAS lost the race, or an
    /// external writer advanced the row).
    fn refresh_mirror(&self, workspace: WorkspaceId) -> Result<(), IndexError> {
        let row = self.inner.store.index_state_get(workspace)?;
        // Same typed corruption gate as `attach`: a negative persisted
        // generation is refused with its value, never normalized to 0.
        let row = match row {
            Some(row) => Some((persisted_generation(workspace, row.generation)?, row)),
            None => None,
        };
        let mut live = lock_recover(&self.inner.live);
        let Some(l) = live.get_mut(&workspace) else {
            return Ok(());
        };
        match row {
            Some((row_generation, row)) => {
                match PersistedIndexState::parse(row.state_json.clone(), row.generation) {
                    Ok(p) => refresh_mirror_into(l, p),
                    Err(e) => {
                        // Corrupt row from an external writer: fail open loudly —
                        // and never discard the rewrite of the durable Failed
                        // marker: the caller retries the reconcile pass with the
                        // propagated store error (the corrupt row stays the
                        // durable retry trigger until the rewrite lands).
                        let message = format!("corrupt persisted index state: {e}");
                        tracing::error!(
                            workspace = workspace.raw(),
                            generation = row.generation,
                            "refresh_mirror: {message}; rewriting the durable row as Failed"
                        );
                        let failed = WorkspaceIndexState::Failed {
                            message: truncate(&message, 256),
                        };
                        let failed_json = failed.to_row_json();
                        self.inner.store.index_state_put(
                            workspace,
                            &failed_json,
                            row.generation,
                            JOURNAL_CORRUPT,
                        )?;
                        refresh_mirror_into(
                            l,
                            PersistedIndexState {
                                state: failed,
                                row_generation,
                                state_json: failed_json,
                            },
                        );
                    }
                }
            }
            None => {
                l.state = WorkspaceIndexState::NotStarted;
                l.row_generation = 0;
                l.state_json = l.state.to_row_json();
                l.pending = true;
                l.pending_load = false;
                l.content = None;
                l.last_fingerprint = None;
                l.coverage = None;
                l.fingerprint_coverage = None;
                l.content_identity.clear();
                l.fingerprint_identity.clear();
            }
        }
        Ok(())
    }

    /// Load the published generation's content from its durable file.
    fn load_content(&self, workspace: WorkspaceId) -> Result<(), IndexError> {
        let (state, state_json, row_generation) = {
            let live = lock_recover(&self.inner.live);
            // Ephemeral map: a detached workspace is a no-op, never a panic.
            let Some(l) = live.get(&workspace) else {
                return Ok(());
            };
            (l.state.clone(), l.state_json.clone(), l.row_generation)
        };
        let gen = match &state {
            WorkspaceIndexState::Ready { generation }
            | WorkspaceIndexState::Dirty { generation } => *generation,
            _ => return Ok(()),
        };
        let path = generation_file_path(&self.inner.data_root, workspace, gen);
        let loaded = match read_generation_file(&path) {
            Ok(file) => {
                if file.workspace != workspace.raw() || file.generation != gen {
                    Err(IndexError::CorruptGeneration {
                        workspace: workspace.raw(),
                        message: format!(
                            "envelope claims workspace {}/gen {}, expected {}/{gen}",
                            file.workspace, file.generation, workspace
                        ),
                    })
                } else {
                    let coverage = file.coverage();
                    let fingerprint_coverage = file.fingerprint_coverage();
                    let content_identity = file.identity();
                    let fingerprint_identity = file.fingerprint_identity();
                    file.materialize()
                        .map(|idx| {
                            (
                                idx,
                                file.fingerprint,
                                coverage,
                                fingerprint_coverage,
                                content_identity,
                                fingerprint_identity,
                            )
                        })
                        .map_err(|e| IndexError::CorruptGeneration {
                            workspace: workspace.raw(),
                            message: e,
                        })
                }
            }
            Err(e) => Err(e),
        };
        let mut live = lock_recover(&self.inner.live);
        let Some(l) = live.get_mut(&workspace) else {
            return Ok(());
        };
        match loaded {
            Ok((
                idx,
                fingerprint,
                coverage,
                fingerprint_coverage,
                content_identity,
                fp_identity,
            )) => {
                l.content = Some(Arc::new(Mutex::new(idx)));
                l.content_generation = gen;
                l.last_fingerprint = Some(fingerprint);
                l.coverage = Some(coverage);
                l.fingerprint_coverage = Some(fingerprint_coverage);
                l.content_identity = content_identity;
                l.fingerprint_identity = fp_identity;
                l.pending_load = false;
                drop(live);
                prune_generations(&self.inner.data_root, workspace, gen);
                Ok(())
            }
            Err(e) => {
                // Missing/corrupt file under a published generation: heal
                // through the machine — Ready -> Dirty -> rebuild. The
                // journal names the tear loudly; the row never silently
                // falls back to "no index".
                tracing::error!(
                    workspace = workspace.raw(),
                    generation = gen,
                    "published generation unreadable: {e}"
                );
                let dirty = WorkspaceIndexState::Dirty { generation: gen };
                let ok = self.inner.store.index_state_cas(
                    workspace,
                    &state_json,
                    row_generation as i64,
                    &dirty.to_row_json(),
                    gen as i64,
                    JOURNAL_TORN_READY,
                )?;
                if ok {
                    l.state = dirty;
                    l.state_json = l.state.to_row_json();
                    l.pending = true;
                    l.pending_load = false;
                    l.last_fingerprint = None;
                    l.coverage = None;
                    l.fingerprint_coverage = None;
                    l.content_identity.clear();
                    l.fingerprint_identity.clear();
                }
                Ok(())
            }
        }
    }

    /// Durable `Ready { g } -> Dirty { g }` (the watcher-event hop).
    fn mark_dirty(&self, workspace: WorkspaceId) -> Result<(), IndexError> {
        let (state, state_json, row_generation) = {
            let live = lock_recover(&self.inner.live);
            // Ephemeral map: a detached workspace is a no-op, never a panic.
            let Some(l) = live.get(&workspace) else {
                return Ok(());
            };
            (l.state.clone(), l.state_json.clone(), l.row_generation)
        };
        let WorkspaceIndexState::Ready { generation } = state else {
            return Ok(());
        };
        let dirty = WorkspaceIndexState::Dirty { generation };
        state.check_transition(row_generation, &dirty, generation)?;
        let ok = self.inner.store.index_state_cas(
            workspace,
            &state_json,
            row_generation as i64,
            &dirty.to_row_json(),
            generation as i64,
            JOURNAL_DIRTY,
        )?;
        let mut live = lock_recover(&self.inner.live);
        let Some(l) = live.get_mut(&workspace) else {
            return Ok(());
        };
        if ok {
            l.state = dirty;
            l.state_json = l.state.to_row_json();
        } else {
            // Lost to a concurrent writer: re-read before deciding again.
            l.pending = true;
        }
        Ok(())
    }

    /// CAS into `Building { target }` (the claim). Exactly one claimant
    /// wins per generation; losers refresh their mirror and wait.
    fn claim_build(
        &self,
        workspace: WorkspaceId,
        target: u64,
        kind: &'static str,
    ) -> Result<bool, IndexError> {
        let cfg = Self::cfg_of(&self.inner);
        let (state, state_json, row_generation, continuation_covered) = {
            let mut live = lock_recover(&self.inner.live);
            let stale = live
                .get(&workspace)
                .map(|l| {
                    l.building
                        && l.building_since
                            .map(|t| t.elapsed() >= cfg.build_lease)
                            .unwrap_or(false)
                })
                .unwrap_or(false);
            if stale {
                let Some(l) = live.get_mut(&workspace) else {
                    return Ok(false);
                };
                l.building = false; // stale lease: crash reclaim
                l.building_since = None;
            }
            let Some(l) = live.get(&workspace) else {
                return Ok(false);
            };
            if l.building {
                return Ok(false);
            }
            // A CONTINUATION claim is legal only while the published
            // generation's coverage (content or fingerprint) is incomplete
            // (an absent record is legacy-unknown: incomplete); anything
            // else must go through the Dirty hop.
            let covered = !l.coverage.as_ref().map(|c| c.complete).unwrap_or(false)
                || !l
                    .fingerprint_coverage
                    .as_ref()
                    .map(|c| c.complete)
                    .unwrap_or(false);
            (
                l.state.clone(),
                l.state_json.clone(),
                l.row_generation,
                covered,
            )
        };
        if kind == JOURNAL_CONTINUE
            && !(matches!(state, WorkspaceIndexState::Ready { generation } if generation == target)
                && continuation_covered)
        {
            tracing::debug!(
                workspace = workspace.raw(),
                target,
                "continuation claim refused: the published generation is complete"
            );
            return Ok(false);
        }
        let building = WorkspaceIndexState::Building { generation: target };
        let resume = matches!(state, WorkspaceIndexState::Building { .. });
        let journal_kind = if resume || kind == JOURNAL_RESUME {
            JOURNAL_RESUME
        } else {
            kind
        };
        state.check_transition(row_generation, &building, target)?;
        let ok = self.inner.store.index_state_cas(
            workspace,
            &state_json,
            row_generation as i64,
            &building.to_row_json(),
            target as i64,
            journal_kind,
        )?;
        if ok {
            let mut live = lock_recover(&self.inner.live);
            if let Some(l) = live.get_mut(&workspace) {
                l.state = building;
                l.state_json = l.state.to_row_json();
                l.row_generation = target;
                l.building = true;
                l.building_since = Some(Instant::now());
                l.pending = false;
            }
        }
        Ok(ok)
    }

    /// Run the whole build pipeline for a claimed generation: scan, ONE
    /// atomic publish through `faktor_fs::atomic` (temp + fsync + rename +
    /// parent fsync), publish CAS, in-memory swap, prune.
    ///
    /// Failure modes:
    /// - genuine build error -> durable `Building -> Failed` (message);
    /// - panic/unwind mid-pipeline (crash) -> durable state stays
    ///   `Building { target }`; the next attach/reclaim resumes the target
    ///   and no reader ever saw a torn generation (a crash after the rename
    ///   but before the CAS leaves a complete, unreferenced file the resume
    ///   overwrites);
    /// - publish CAS lost -> another builder won; the loser's staged whole
    ///   snapshot is never named Ready, so it stays invisible;
    /// - cancellation (audit 20) -> the scan/serialize work units observe
    ///   `cancel` between files/directories/machine steps and the pipeline
    ///   ABANDONS without publishing; the durable row stays
    ///   `Building { target }` and the in-process lease is released for a
    ///   later resume.
    fn run_build(
        &self,
        workspace: WorkspaceId,
        target: u64,
        kind: &'static str,
        cancel: Option<&CancellationToken>,
    ) -> Result<BuildOutcome, IndexError> {
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            return Ok(self.abort_build(workspace));
        }
        let continuation = kind == JOURNAL_CONTINUE;
        let ws_raw = workspace.raw();
        let (expected_json, root) = {
            let live = lock_recover(&self.inner.live);
            // Ephemeral map: a detached workspace is a no-op, never a panic.
            let Some(l) = live.get(&workspace) else {
                return Ok(BuildOutcome::Ran);
            };
            (l.state_json.clone(), l.root.clone())
        };
        let Some(root) = root else {
            self.fail_build(
                workspace,
                target,
                &expected_json,
                "no workspace root".into(),
            )?;
            return Ok(BuildOutcome::Ran);
        };
        // Build-time embedding is configured per service (additive API); the
        // source snapshot is read once so an in-flight build is stable.
        let source_config = self.embedding_config();
        let cfg = Self::cfg_of(&self.inner);
        let pace = self.scan_pace();
        let gen_path = generation_file_path(&self.inner.data_root, workspace, target);

        // LIVE seed: a fingerprint-only continuation's in-progress round
        // exists only in the mirror until the next publish (the published
        // file's fingerprint coverage is still complete). Content state
        // always comes from the durable partial envelope.
        let live_fp_round = {
            let live = lock_recover(&self.inner.live);
            live.get(&workspace).and_then(|l| {
                let cov = l.fingerprint_coverage.clone()?;
                (!cov.complete).then_some(cov)
            })
        };

        // Resume the SAME generation's durable accumulation when the file
        // belongs to this target and still has incomplete work (or the claim
        // is an explicit continuation, e.g. a fingerprint round).
        let resumed = match read_generation_file(&gen_path) {
            Ok(file)
                if file.workspace == ws_raw
                    && file.generation == target
                    && (!file.coverage().complete
                        || !file.fingerprint_coverage().complete
                        || continuation) =>
            {
                match file.materialize() {
                    Ok(index) => Some((
                        index,
                        file.coverage(),
                        file.cursor.clone().unwrap_or_default(),
                        file.fingerprint
                            .iter()
                            .cloned()
                            .map(|e| (e.path.clone(), e))
                            .collect::<BTreeMap<String, FingerprintEntry>>(),
                        file.fingerprint_coverage(),
                        file.fingerprint_next
                            .iter()
                            .cloned()
                            .map(|e| (e.path.clone(), e))
                            .collect::<BTreeMap<String, FingerprintEntry>>(),
                        file.fingerprint_next_epoch,
                    )),
                    Err(e) => {
                        self.fail_build(
                            workspace,
                            target,
                            &expected_json,
                            format!("resume generation {target}: {e}"),
                        )?;
                        return Ok(BuildOutcome::Ran);
                    }
                }
            }
            Ok(_) => None,
            Err(e) if gen_path.exists() => {
                // A corrupt partial envelope is loud, never a silent fresh
                // restart over adversary-controlled bytes.
                tracing::warn!(
                    workspace = ws_raw,
                    generation = target,
                    error = %e,
                    "unreadable generation envelope; restarting the build"
                );
                None
            }
            Err(_) => None,
        };
        let (
            mut index,
            mut coverage,
            mut cursor,
            mut fp_stored,
            file_fp_cov,
            mut fp_next,
            file_next_epoch,
        ) = match resumed {
            Some(parts) => parts,
            None => (
                WorkspaceIndex::new(),
                IndexCoverage::empty(),
                ScanCursor::default(),
                BTreeMap::new(),
                // A fresh generation first establishes its fingerprint
                // baseline (shard 0); later rounds rotate via the persisted
                // `round_start` and VERIFY against the baseline.
                FingerprintCoverage::baseline(),
                BTreeMap::new(),
                None,
            ),
        };
        // A continuation with an in-memory round in progress continues THAT
        // round (its shard/cursor state); otherwise the durable file state
        // rules.
        let mut fp_cov = match (continuation, live_fp_round) {
            (true, Some(round)) => round,
            _ => file_fp_cov,
        };
        // The in-progress map belongs to the round that wrote it; a new
        // round starts from an empty map.
        if file_next_epoch != Some(fp_cov.epoch) {
            fp_next.clear();
        }
        // A pre-coverage fingerprint list is not a trustworthy baseline:
        // re-establish the fingerprint from the fresh scan instead of
        // merging unverifiable entries into it.
        if fp_cov.is_legacy_unknown() {
            fp_stored.clear();
        }
        // Diagnostics honesty: while this build runs, concurrent readers of
        // `coverage`/`coverage_snapshot` must see the IN-PROGRESS coverage
        // (partial), never the stale complete value from before the claim.
        {
            let mut live = lock_recover(&self.inner.live);
            if let Some(l) = live.get_mut(&workspace) {
                l.coverage = Some(coverage.clone());
                l.fingerprint_coverage = Some(fp_cov.clone());
            }
        }
        let mut dirty_now = false;

        // ---- bounded CONTENT batch (audit 5) ----
        let mut batch_chunks: Vec<(String, Vec<String>)> = Vec::new();
        let mut chunk_text_bytes = 0usize;
        if !coverage.complete {
            let batch = match scan_content_batch(&root, &cursor, &cfg, pace, cancel) {
                Ok(batch) => batch,
                Err(e) => {
                    self.fail_build(workspace, target, &expected_json, e)?;
                    return Ok(BuildOutcome::Ran);
                }
            };
            if batch.cancelled {
                return Ok(self.abort_build(workspace));
            }
            for file in &batch.files {
                // Cancellation is observed between indexed files too: the
                // in-memory accumulation is discarded by the abandonment
                // (nothing was published), never handed to the machine.
                if cancel.is_some_and(CancellationToken::is_cancelled) {
                    return Ok(self.abort_build(workspace));
                }
                index
                    .index_file(
                        workspace,
                        Path::new(&file.rel),
                        &file.bytes,
                        file.modified_ms,
                    )
                    .map_err(|e| IndexError::BuildFailed {
                        workspace: ws_raw,
                        message: format!("index {}: {e}", file.rel),
                    })?;
                if source_config.is_some() {
                    // Chunk texts for build-time embedding: bounded per file
                    // (chunk caps) and in total, so a hostile repo cannot
                    // inflate the batch's memory.
                    let texts: Vec<String> =
                        crate::embedding::chunk_text(&String::from_utf8_lossy(&file.bytes))
                            .into_iter()
                            .map(str::to_string)
                            .collect();
                    let bytes_here: usize = texts.iter().map(|t| t.len()).sum();
                    if !texts.is_empty()
                        && chunk_text_bytes.saturating_add(bytes_here)
                            <= MAX_EMBEDDING_SCAN_TEXT_BYTES
                    {
                        chunk_text_bytes += bytes_here;
                        batch_chunks.push((file.rel.clone(), texts));
                    }
                }
            }
            coverage.files_seen = coverage.files_seen.saturating_add(batch.files_seen);
            coverage.files_indexed = coverage.files_indexed.saturating_add(batch.indexed);
            coverage.bytes_indexed = coverage.bytes_indexed.saturating_add(batch.bytes);
            coverage.complete = batch.complete;
            coverage.truncated_reason = batch.truncated_reason.clone();
            cursor = batch.cursor.clone();
            if coverage.complete {
                cursor = ScanCursor::default();
            }
        }

        // ---- bounded FINGERPRINT shard passes (audit 6) ----
        // Only meaningful once the content walk completed; while content is
        // incomplete the generation keeps extending and can never be claimed
        // clean. A capped round stays incomplete (never "fully clean") and
        // the next batch continues it.
        if coverage.complete {
            for _ in 0..FINGERPRINT_SHARDS {
                if fp_cov.complete {
                    break;
                }
                if cancel.is_some_and(CancellationToken::is_cancelled) {
                    return Ok(self.abort_build(workspace));
                }
                let shard = fp_cov.shard;
                let pass = match fingerprint_pass(
                    &root,
                    shard,
                    &fp_cov.cursor_for(shard),
                    cfg.fp_batch_files,
                    cfg.fp_batch_dirs,
                    pace,
                    cancel,
                ) {
                    Ok(pass) => {
                        if pass.cancelled {
                            return Ok(self.abort_build(workspace));
                        }
                        pass
                    }
                    Err(e) => {
                        // A fingerprint that cannot be read can never be
                        // declared clean: the generation is superseded
                        // through the Dirty hop, and the rebuild will surface
                        // the unreadable root as a durable failure.
                        tracing::warn!(
                            workspace = ws_raw,
                            generation = target,
                            error = %e,
                            "fingerprint pass failed; the generation is superseded (never declared clean)"
                        );
                        fp_cov.truncated_reason = Some(format!("fingerprint_error: {e}"));
                        fp_cov.complete = false;
                        dirty_now = true;
                        break;
                    }
                };
                for entry in pass.entries {
                    match fp_stored.get(&entry.path) {
                        Some(previous) if *previous == entry => {}
                        // A baseline round RECORDS the fingerprint; only a
                        // verification round can call a difference a change.
                        _ => {
                            if fp_cov.verify {
                                dirty_now = true;
                            }
                        }
                    }
                    fp_next.insert(entry.path.clone(), entry);
                }
                if pass.shard_completed {
                    fp_cov.finish_shard(shard);
                    if fp_cov.complete {
                        // The round finished. A baseline round MERGES the
                        // observed map into the accumulation it has been
                        // publishing all along (nothing may be lost). A
                        // verification round must have seen every stored
                        // entry or the difference (a deletion) supersedes the
                        // generation; the observed map then becomes the new
                        // baseline.
                        if fp_cov.verify {
                            if fp_stored.len() != fp_next.len() {
                                dirty_now = true;
                            }
                            fp_stored = std::mem::take(&mut fp_next);
                        } else {
                            for (path, entry) in std::mem::take(&mut fp_next) {
                                fp_stored.insert(path, entry);
                            }
                        }
                    }
                } else {
                    fp_cov.set_cursor(shard, pass.cursor);
                    fp_cov.truncated_reason = Some(
                        pass.reason
                            .unwrap_or_else(|| "fingerprint_batch".to_string()),
                    );
                }
                fp_cov.scanned = if fp_cov.verify && !fp_cov.complete {
                    fp_next.len() as u64
                } else {
                    fp_stored.len() as u64
                };
                if dirty_now {
                    break;
                }
            }
        }

        // A BASELINE round accumulates directly into the published baseline
        // (comparisons are off); a VERIFICATION round keeps the published
        // baseline intact and carries its observed map separately until the
        // round completes.
        if !fp_cov.verify {
            for (path, entry) in std::mem::take(&mut fp_next) {
                fp_stored.insert(path, entry);
            }
        }

        // Cancellation before the embedding carry/embed work unit: nothing
        // is published and the durable `Building { target }` row resumes.
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            return Ok(self.abort_build(workspace));
        }

        // Persisted vectors survive a generation swap regardless of whether
        // a source is configured: with one, matching vectors are carried and
        // only missing chunks embed; without one, the prior store rides
        // along verbatim for search. The accumulation's OWN vectors are the
        // prior on resumed batches (earlier batches' work is never re-embedded).
        //
        // A fingerprint-only continuation carries NO new chunks: re-running
        // the pass would rebuild an EMPTY identity-scoped store and drop the
        // persisted vectors, so it keeps the materialized store verbatim.
        if batch_chunks.is_empty() {
            if index.embedding_index(workspace).is_none() {
                if let Some(prior) = self.prior_embeddings(workspace) {
                    index.replace_embeddings(workspace, prior);
                }
            }
        } else {
            self.embed_accumulated(
                workspace,
                ws_raw,
                target,
                &mut index,
                &batch_chunks,
                source_config,
            );
        }
        // Publish invariant (typed embedding status): the exact entry
        // captured into the envelope is the one the swap installs as the
        // published content, so view and durable generation spell the SAME
        // typed status. A source-less, prior-less build MUST publish the
        // explicit `Unconfigured` record (never an absent entry): otherwise
        // callers read `embedding_index(ws) == None` as "unconfigured"
        // while doctor reads `Unconfigured` from the file. An absent entry
        // therefore means exactly "no published generation at all".
        if index.embedding_index(workspace).is_none() {
            index.replace_embeddings(workspace, EmbeddingIndex::default());
        }
        let (fingerprint_next, fingerprint_next_epoch) = if fp_cov.verify && !fp_cov.complete {
            (
                fp_next.values().cloned().collect::<Vec<FingerprintEntry>>(),
                Some(fp_cov.epoch),
            )
        } else {
            (Vec::new(), None)
        };
        fp_cov.scanned = if fingerprint_next_epoch.is_some() {
            fp_next.len() as u64
        } else {
            fp_stored.len() as u64
        };
        // Cancellation before the serializer work unit: the generation is
        // abandoned (nothing staged, nothing published).
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            return Ok(self.abort_build(workspace));
        }
        let fingerprint: Vec<FingerprintEntry> = fp_stored.values().cloned().collect();
        let envelope = GenerationFile::capture_with_coverage(
            ws_raw,
            target,
            &index,
            fingerprint,
            coverage.clone(),
            (!coverage.complete).then_some(cursor.clone()),
            fp_cov.clone(),
            fingerprint_next,
            fingerprint_next_epoch,
        );
        // A clean verification round republishes the SAME identity: its
        // bytes are already visible, so the atomic rewrite is skipped while
        // the durable verified fact is still recorded. Any real change (or
        // an incomplete batch) writes normally.
        let identity = envelope.identity();
        let prior_identity = {
            let live = lock_recover(&self.inner.live);
            live.get(&workspace)
                .map(|l| l.content_identity.clone())
                .unwrap_or_default()
        };
        let verified_clean = envelope.coverage().complete
            && envelope.fingerprint_coverage().complete
            && !dirty_now
            && !prior_identity.is_empty()
            && identity == prior_identity;
        let journal_kind = if !envelope.coverage().complete {
            // An incomplete batch publish journals `ready_partial` so the
            // complete publish remains the one `ready` fact of the
            // generation.
            JOURNAL_READY_PARTIAL
        } else if dirty_now {
            // The round proved a difference: the same-generation bytes stay
            // visible first, then the Dirty hop rebuilds.
            JOURNAL_VERIFY_DIRTY
        } else if verified_clean {
            JOURNAL_VERIFIED
        } else {
            JOURNAL_READY
        };
        if !verified_clean {
            let bytes = envelope.to_bytes().map_err(|e| IndexError::BuildFailed {
                workspace: ws_raw,
                message: e,
            })?;
            // Crash seam: a test hook may kill this builder right here, before
            // any byte of the generation was staged and before the publish CAS.
            // The durable state is still Building{target}, so the next build
            // resumes the SAME target and no reader can observe a torn
            // generation.
            fire_seam(ws_raw, target, "before_publish");
            // The publish directory must exist before the atomic writer.
            fs::create_dir_all(generation_dir(&self.inner.data_root, workspace))?;
            // Filesystem visibility FIRST, durable state SECOND: the atomic
            // writer (temp + fsync + rename + parent fsync) stages the file
            // BEFORE the `Building -> Ready` CAS. `Ready{target}` must never
            // name invisible bytes, or a concurrent reconcile races the
            // writer, fails to read the file, and heals Ready -> Dirty
            // (a phantom generation on a clean workspace). A crash before
            // the rename leaves the row Building; after the rename it leaves
            // a complete, unreferenced file the resume overwrites. CAS
            // losers may stage whole bytes for the same generation: every
            // rename is atomic and the CAS names exactly one published one.
            let gen_path = generation_file_path(&self.inner.data_root, workspace, target);
            let published = faktor_fs::atomic::atomic_replace_guarded(&gen_path, &bytes, &|_| {
                // Adversarial seam: the bytes are written + fsynced under the
                // writer's temp name but the rename to the visible generation
                // has not happened; a test hook may kill the builder here.
                fire_seam(ws_raw, target, "before_visibility");
                Ok(())
            });
            if let Err(e) = published {
                // No CAS was attempted: the durable state is still
                // Building{target}, so clearing the in-process lease lets the
                // machine retry/resume the same target (a real IO failure is not
                // a Failed build — never renumber, never skip).
                let mut live = lock_recover(&self.inner.live);
                if let Some(l) = live.get_mut(&workspace) {
                    l.building = false;
                    l.building_since = None;
                }
                return Err(IndexError::Io(std::io::Error::other(e.message)));
            }
        }
        // Publish CAS: only ONE builder wins per generation. The visible
        // bytes are already durable at the moment the row first says Ready.
        let ready = WorkspaceIndexState::Ready { generation: target };
        let ok = self.inner.store.index_state_cas(
            workspace,
            &expected_json,
            target as i64,
            &ready.to_row_json(),
            target as i64,
            journal_kind,
        )?;
        if !ok {
            // Another builder published this generation (crash-resume race):
            // the file written above is a complete snapshot for `target`;
            // the winner's CAS → Ready owns the live swap. Clear the lease
            // and refresh the mirror on the next reconcile.
            let mut live = lock_recover(&self.inner.live);
            if let Some(l) = live.get_mut(&workspace) {
                l.building = false;
                l.building_since = None;
            }
            return Ok(BuildOutcome::Ran);
        }
        {
            let mut live = lock_recover(&self.inner.live);
            if let Some(l) = live.get_mut(&workspace) {
                l.state = ready;
                l.state_json = l.state.to_row_json();
                l.row_generation = target;
                l.content = Some(Arc::new(Mutex::new(index)));
                l.content_generation = target;
                l.last_fingerprint = Some(envelope.fingerprint.clone());
                l.coverage = Some(envelope.coverage());
                l.fingerprint_coverage = Some(envelope.fingerprint_coverage());
                l.content_identity = envelope.identity();
                l.fingerprint_identity = envelope.fingerprint_identity();
                l.dirty_now = dirty_now;
                l.pending_load = false;
                l.building = false;
                l.building_since = None;
                // An incomplete batch keeps the workspace pending: the next
                // reconcile continues the SAME generation (content batches
                // and/or fingerprint shard passes) until it is complete.
                if !envelope.coverage().complete || !envelope.fingerprint_coverage().complete {
                    l.pending = true;
                }
            }
        }
        prune_generations(&self.inner.data_root, workspace, target);
        let first_publication = {
            let mut logged = self
                .inner
                .published_logged
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let target_marker = target as i64;
            if logged.get(&ws_raw).copied() == Some(target_marker) {
                false
            } else {
                if logged.len() > 1024 {
                    logged.clear();
                }
                logged.insert(ws_raw, target_marker);
                true
            }
        };
        if first_publication {
            tracing::info!(
                workspace = ws_raw,
                generation = target,
                "index generation published"
            );
        }
        Ok(BuildOutcome::Ran)
    }

    /// Abandon a claim whose build observed cancellation (audit 20): release
    /// the IN-PROCESS lease only. The durable row deliberately stays
    /// `Building { target }` (the machine resumes the SAME generation), and
    /// the workspace stays pending so any later pass (or an explicit
    /// `ensure_ready`/`request_build`) reclaims it immediately instead of
    /// waiting out the build lease. Nothing was published.
    fn abort_build(&self, workspace: WorkspaceId) -> BuildOutcome {
        let mut live = lock_recover(&self.inner.live);
        if let Some(l) = live.get_mut(&workspace) {
            l.building = false;
            l.building_since = None;
            l.pending = true;
        }
        BuildOutcome::Cancelled
    }

    /// Journal a genuine build failure: `Building { target } -> Failed`.
    /// The message is durable; retry happens on the next change or on an
    /// explicit [`IndexService::request_build`] — never on a timer.
    fn fail_build(
        &self,
        workspace: WorkspaceId,
        target: u64,
        expected_json: &str,
        message: String,
    ) -> Result<(), IndexError> {
        tracing::error!(
            workspace = workspace.raw(),
            generation = target,
            "index build failed: {message}"
        );
        let failed = WorkspaceIndexState::Failed {
            message: truncate(&message, 512),
        };
        let ok = self.inner.store.index_state_cas(
            workspace,
            expected_json,
            target as i64,
            &failed.to_row_json(),
            target as i64,
            JOURNAL_FAILED,
        )?;
        let mut live = lock_recover(&self.inner.live);
        if let Some(l) = live.get_mut(&workspace) {
            if ok {
                l.state = failed;
                l.state_json = l.state.to_row_json();
                l.row_generation = target;
            }
            l.building = false;
            l.building_since = None;
        }
        Ok(())
    }
}

/// Terminal outcome of one [`IndexService::run_build`] pipeline.
enum BuildOutcome {
    /// The pipeline ran to its end: published (or lost the publish CAS, or
    /// journaled a durable failure).
    Ran,
    /// The cancellation token fired at a work-unit boundary (audit 20):
    /// nothing was published. The durable row stays `Building { target }`
    /// for the resume and the in-process lease is cleared so a later
    /// reconcile can claim the SAME generation immediately.
    Cancelled,
}

/// What the reconciler should do next for one workspace.
enum Next {
    Idle,
    /// Load the published generation's content from its durable file.
    Load,
    /// Mark the published generation dirty durably (`Ready -> Dirty`).
    MarkDirty,
    /// Claim a build into `Building { target }` and run it.
    Claim {
        target: u64,
        kind: &'static str,
    },
}

fn refresh_mirror_into(l: &mut LiveWs, p: PersistedIndexState) {
    let keep_content = match (&p.state, l.content_generation) {
        (
            WorkspaceIndexState::Ready { generation } | WorkspaceIndexState::Dirty { generation },
            cg,
        ) => *generation == cg && l.content.is_some(),
        _ => false,
    };
    if !keep_content {
        l.content = None;
        l.last_fingerprint = None;
        l.coverage = None;
        l.fingerprint_coverage = None;
        l.content_identity.clear();
        l.fingerprint_identity.clear();
    }
    l.state = p.state;
    l.state_json = p.state_json;
    l.row_generation = p.row_generation;
    l.building = false;
    l.building_since = None;
    l.dirty_now = false;
    l.pending_load = matches!(l.state, WorkspaceIndexState::Ready { .. }) && !keep_content;
    l.pending = matches!(
        l.state,
        WorkspaceIndexState::NotStarted
            | WorkspaceIndexState::Building { .. }
            | WorkspaceIndexState::Dirty { .. }
    );
    l.last_fp_check = Instant::now() - DEFAULT_FINGERPRINT_INTERVAL;
}

/// Default fingerprint reconciliation interval (see [`ServiceConfig`]).
const DEFAULT_FINGERPRINT_INTERVAL: Duration = Duration::from_secs(30);

/// Stable machine spelling of a durable index state (diagnostics surfaces;
/// mirrors the doctor's `doctor_index_state_label`).
fn index_state_label(state: &WorkspaceIndexState) -> &'static str {
    match state {
        WorkspaceIndexState::NotStarted => "not_started",
        WorkspaceIndexState::Building { .. } => "building",
        WorkspaceIndexState::Dirty { .. } => "dirty",
        WorkspaceIndexState::Ready { .. } => "ready",
        WorkspaceIndexState::Failed { .. } => "failed",
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &s[..end])
    }
}

// ------------------------------------------------------------------ worker

/// Lifecycle state of the service's single background reconciliation worker
/// (see [`IndexService::worker_status`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerState {
    /// No worker has ever been spawned (or spawning was refused: no tokio
    /// runtime in the caller).
    NotStarted,
    /// The owned task is alive.
    Running,
    /// The owned task exited cleanly (cancellation / shutdown).
    Stopped,
    /// The owned task terminated with a panic; reconciliation no longer runs
    /// until an explicit kick (attach / [`IndexService::spawn_worker`])
    /// starts a new generation.
    Failed,
}

/// Health snapshot of the background reconciliation worker, for
/// health/doctor surfaces: lifecycle state, the last failure message, and
/// how many worker generations were started after the first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerStatus {
    /// Lifecycle state.
    pub state: WorkerState,
    /// Last failure (panic) message, when the worker ever failed.
    pub last_error: Option<String>,
    /// Worker generations started AFTER the first one. Automatic restarts do
    /// not exist; every increment is an explicit `spawn_worker` call on a
    /// terminal service (attach/kick respawn), so the count is bounded by
    /// external calls, never by an internal loop.
    pub restarts: u64,
    /// True while a blocking reconciliation pass is STILL RUNNING. The pass
    /// observes cancellation between work units (scan files/directories,
    /// batch files, machine steps — audit 20), so after a cancellation this
    /// flag clears once the ONE syscall/unit already in flight returns. If
    /// [`IndexService::shutdown_worker`] reports
    /// [`WorkerShutdown::Aborted`], this flag tells the operator that a
    /// genuinely uncancellable unit is still running and sets the real exit
    /// bound, not the shutdown bound. False for every at-rest state.
    pub pass_in_flight: bool,
}

/// Outcome of [`IndexService::shutdown_worker`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerShutdown {
    /// No owned task was alive (never spawned, or already stopped/failed).
    NotRunning,
    /// The owned task was cancelled and joined within the bound; nothing
    /// remains detached.
    Joined,
    /// The task did not exit within the bound and was aborted; its retained
    /// handle was still awaited, so the owner reports a terminal state. This
    /// only happens when a genuinely uncancellable work unit (one slow
    /// syscall/embedding call already executing) outlives the bound — check
    /// [`WorkerStatus::pass_in_flight`] for that residual.
    Aborted,
}

/// Explicit lifecycle owner of the single background reconciliation worker:
/// the `JoinHandle` is retained for the task's whole life (never dropped
/// unattached), cancellation is signalled through [`WorkerSupervisor::cancel`],
/// and the last outcome is recorded for health/doctor.
struct WorkerSupervisor {
    /// Worker generation: a worker records its failure only onto the
    /// generation it was spawned for, so an explicit respawn's fresh state
    /// can never be clobbered by the dying predecessor.
    generation: u64,
    /// Cancellation signal of the CURRENT generation.
    cancel: CancellationToken,
    /// Retained handle of the CURRENT generation.
    handle: Option<tokio::task::JoinHandle<()>>,
    last_error: Option<String>,
    state: WorkerState,
    restarts: u64,
}

impl WorkerSupervisor {
    fn idle() -> Self {
        Self {
            generation: 0,
            cancel: CancellationToken::new(),
            handle: None,
            last_error: None,
            state: WorkerState::NotStarted,
            restarts: 0,
        }
    }

    fn status(&self, pass_in_flight: bool) -> WorkerStatus {
        WorkerStatus {
            state: self.state,
            last_error: self.last_error.clone(),
            restarts: self.restarts,
            pass_in_flight,
        }
    }
}

/// Reconcile a worker whose async task body already finished while the
/// started flag still claims a live worker (e.g. the task body panicked
/// outside the blocking pass, so no `run_pass` `JoinError` handler ran).
/// The generation is marked [`WorkerState::Failed`], the failure is named,
/// and the started flag is cleared so an explicit kick can start a fresh
/// generation. Returns true when a dead-running worker was reconciled.
fn reconcile_dead_worker(worker: &mut WorkerSupervisor, started: &AtomicBool) -> bool {
    if worker.state != WorkerState::Running {
        return false;
    }
    let dead = worker
        .handle
        .as_ref()
        .is_some_and(|handle| handle.is_finished());
    if !dead {
        return false;
    }
    worker.state = WorkerState::Failed;
    worker.last_error.get_or_insert_with(|| {
        "worker task terminated without recording a terminal state".to_string()
    });
    started.store(false, Ordering::Release);
    true
}

/// Lock an INDEX-INTERNAL mutex with poison recovery (audit item 8).
///
/// Every mutex this helper serves guards process-local, rebuildable state:
/// operator config, the live workspace map, the optional embedding source,
/// watcher event queues and the test seam. The durable truth lives in the
/// `Store` rows and the generation files on disk, so a panic in one holder
/// cannot make continuing unsafe — the lock is recovered with
/// `into_inner()` (Rust mutex payloads are never torn) instead of wedging
/// every later request behind `PoisonError`. Durable authorities (the store
/// writer service) fail typed; this module has none.
fn lock_recover<'a, T>(m: &'a Mutex<T>) -> MutexGuard<'a, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Lock the worker supervisor with classified POISON recovery. The shutdown
/// and health paths run during teardown — exactly where a writer panic is
/// most likely — and must never panic themselves (`expect` would abort the
/// daemon shutdown). The poisoned guard's inner value is sound (Rust mutexes
/// never tear their payload), so it is recovered with the poison flag
/// cleared, the failure is logged, and the supervisor records the failure
/// (`Failed`, `last_error`) with the started flag released so an explicit
/// kick can start a fresh generation.
fn lock_worker_supervisor(inner: &Inner) -> std::sync::MutexGuard<'_, WorkerSupervisor> {
    match inner.worker.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            inner.worker.clear_poison();
            let mut guard = poisoned.into_inner();
            let message = "worker supervisor lock poisoned by a panicking writer".to_string();
            tracing::error!(
                error = %message,
                "index reconciliation worker supervisor lock was poisoned; recovering the inner supervisor state"
            );
            if guard.last_error.is_none() {
                guard.last_error = Some(message);
            }
            if guard.state == WorkerState::Running {
                guard.state = WorkerState::Failed;
            }
            inner.worker_started.store(false, Ordering::Release);
            guard
        }
    }
}

/// Test-only fault seam of the worker body (per service instance, so worker
/// tests never race each other through global state).
#[cfg(test)]
#[derive(Default)]
struct WorkerFault {
    /// Count of `run_pass` invocations (the bounded/no-hot-loop assertion).
    pass_invocations: AtomicU64,
    /// One-shot: the next worker ASYNC BODY panics before its first pass
    /// (the dead-generation fault; distinct from a panicking blocking pass).
    async_body_panic: AtomicBool,
    /// One-shot: the next pass panics (the `JoinError` fault).
    panic_next: AtomicBool,
    /// One-shot: the next pass blocks this many ms on the blocking thread
    /// (the "in-flight pass outlives the shutdown bound" fault).
    block_next_ms: AtomicU64,
}

/// Clears [`Inner::pass_in_flight`] on every exit path, panics included.
struct PassInFlightGuard<'a> {
    flag: &'a AtomicBool,
}

impl Drop for PassInFlightGuard<'_> {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::Release);
    }
}

/// The background reconciliation worker: ONE owned task per service. Runs
/// bounded synchronous passes on the blocking pool; waits for kicks or the
/// poll cadence between passes.
///
/// Failure contract (ownerless-task/panic-loop fix): a panic in a pass is
/// LOUD and TERMINAL. The old shape
/// (`spawn_blocking(...).await.unwrap_or(true)`) mapped `JoinError` to
/// "work remains" and immediately re-ran the pass — a deterministic panic
/// became an unbounded CPU-burning retry loop whose failure was discarded.
/// Now the join result is matched explicitly: the panic is logged and
/// recorded (`state = Failed`, `last_error`, started flag cleared), and the
/// loop RETURNS. Automatic restart is deliberately absent; a later explicit
/// kick (`spawn_worker`) starts a new generation.
async fn worker_loop(weak: std::sync::Weak<Inner>, cancel: CancellationToken, generation: u64) {
    loop {
        if cancel.is_cancelled() {
            return;
        }
        let Some(inner) = weak.upgrade() else {
            return;
        };
        #[cfg(test)]
        if inner.fault.async_body_panic.swap(false, Ordering::SeqCst) {
            panic!("[test] deliberate index worker async-body panic");
        }
        let cfg = IndexService::cfg_of(&inner);
        let service = IndexService {
            inner: inner.clone(),
        };
        let pass_cancel = cancel.clone();
        let mut pass = tokio::task::spawn_blocking(move || service.run_pass(&pass_cancel));
        // Cancellation stops waiting on the pass, but never DETACHES it: the
        // pass observes the token between work units (scan files/
        // directories, batch files, machine steps — audit 20) and exits
        // promptly, so this task joins it before returning. The worker's
        // join therefore bounds the REAL exit including the blocking work
        // (see `shutdown_worker`); only a genuinely uncancellable unit can
        // still outlive the bound and is then reported typed.
        let joined = tokio::select! {
            joined = &mut pass => Some(joined),
            _ = cancel.cancelled() => None,
        };
        let joined = match joined {
            Some(joined) => joined,
            None => pass.await,
        };
        let again = match joined {
            Ok(again) => again,
            Err(join_err) => {
                let message = format!("reconciliation pass panicked: {join_err}");
                tracing::error!(
                    error = %message,
                    generation,
                    "index reconciliation worker STOPPED (panicking pass); no automatic restart — a new generation needs an explicit kick"
                );
                inner.worker_started.store(false, Ordering::Release);
                if let Ok(mut worker) = inner.worker.lock() {
                    if worker.generation == generation {
                        worker.last_error = Some(message);
                        worker.state = WorkerState::Failed;
                    }
                }
                return;
            }
        };
        if cancel.is_cancelled() {
            return;
        }
        if again {
            // Work is immediately claimable (a claimed build landed; another
            // workspace is pending) — continue the same loop. Every FAILURE
            // path above returns, so this can never spin on a failed pass.
            continue;
        }
        tokio::select! {
            _ = inner.notify.notified() => {}
            _ = tokio::time::sleep(cfg.poll) => {}
            _ = cancel.cancelled() => return,
        }
    }
}

impl IndexService {
    /// One blocking pass over every attached workspace, executed by the
    /// worker under `spawn_blocking` (and reusable synchronously). Returns
    /// true when more work is immediately claimable.
    ///
    /// `cancel` is the worker generation's token and is observed at every
    /// work-unit boundary (audit 20): between workspaces, between machine
    /// steps of one workspace, between indexed files of a content batch, and
    /// between files/directories of a scan walk. A shutdown never starts new
    /// work after the signal and a scan in flight exits after at most ONE
    /// work unit (the syscall already running), keeping the typed
    /// [`WorkerStatus::pass_in_flight`] flag truthful for that residual
    /// window only.
    fn run_pass(&self, cancel: &CancellationToken) -> bool {
        self.inner.pass_in_flight.store(true, Ordering::Release);
        let _in_flight = PassInFlightGuard {
            flag: &self.inner.pass_in_flight,
        };
        #[cfg(test)]
        {
            self.inner
                .fault
                .pass_invocations
                .fetch_add(1, Ordering::SeqCst);
            if self.inner.fault.panic_next.swap(false, Ordering::SeqCst) {
                panic!("fault seam: injected reconciliation-pass panic");
            }
            let block_ms = self.inner.fault.block_next_ms.swap(0, Ordering::SeqCst);
            if block_ms > 0 {
                // The in-flight-pass fault: this thread is the blocking pool;
                // a shutdown cannot kill it and must report it.
                std::thread::sleep(Duration::from_millis(block_ms));
            }
        }
        if cancel.is_cancelled() {
            return false;
        }
        let workspaces: Vec<WorkspaceId> = lock_recover(&self.inner.live).keys().copied().collect();
        let mut work_remains = false;
        for ws in workspaces {
            if cancel.is_cancelled() {
                return false;
            }
            if let Err(e) = self.reconcile_now_cancellable(ws, Some(cancel)) {
                tracing::warn!(workspace = ws.raw(), "reconcile pass error: {e}");
                continue;
            }
            let live = lock_recover(&self.inner.live);
            let needs_more = live
                .get(&ws)
                .map(|l| {
                    l.pending
                        || l.pending_load
                        || l.dirty_now
                        || !l.coverage.as_ref().map(|c| c.complete).unwrap_or(false)
                        || !l
                            .fingerprint_coverage
                            .as_ref()
                            .map(|c| c.complete)
                            .unwrap_or(false)
                })
                .unwrap_or(false);
            if needs_more {
                work_remains = true;
            }
        }
        work_remains
    }
}

// ------------------------------------------------------------------ layout

/// Durable published-generation file of one workspace.
fn generation_dir(data_root: &Path, ws: WorkspaceId) -> PathBuf {
    data_root.join("generations").join(ws.raw().to_string())
}

/// Scratch area (pre-atomic-publish builders wrote generation staging here).
/// No builder writes scratch anymore; the directory is kept only so
/// [`prune_generations`] can sweep crash residue left by an older daemon.
fn scratch_dir(data_root: &Path, ws: WorkspaceId) -> PathBuf {
    data_root.join("scratch").join(ws.raw().to_string())
}

fn generation_file_path(data_root: &Path, ws: WorkspaceId, generation: u64) -> PathBuf {
    generation_dir(data_root, ws).join(format!("gen-{generation}.json"))
}

/// Decode the persisted SIGNED `generation` column of an `index_state` row.
/// The column is written only by this service and only ever holds a `u64`
/// domain value, so a negative value cannot describe any legal machine
/// state: it is typed corruption evidence naming the value, never silently
/// normalized to a clean-looking generation 0.
fn persisted_generation(workspace: WorkspaceId, generation: i64) -> Result<u64, IndexError> {
    u64::try_from(generation).map_err(|_| {
        IndexError::CorruptState(format!(
            "workspace {} has negative persisted generation {generation}",
            workspace.raw()
        ))
    })
}

/// Read + decode a generation file (bounded read: oversized hostile files
/// are a loud corruption, never a RAM flood).
fn read_generation_file(path: &Path) -> Result<GenerationFile, IndexError> {
    let meta = fs::metadata(path).map_err(|e| IndexError::CorruptGeneration {
        workspace: 0,
        message: format!("generation file {} unreadable: {e}", path.display()),
    })?;
    if meta.len() > MAX_GENERATION_FILE_BYTES {
        return Err(IndexError::CorruptGeneration {
            workspace: 0,
            message: format!(
                "generation file {} is {} bytes (cap {MAX_GENERATION_FILE_BYTES})",
                path.display(),
                meta.len()
            ),
        });
    }
    let bytes = fs::read(path).map_err(|e| IndexError::CorruptGeneration {
        workspace: 0,
        message: format!("generation file {} unreadable: {e}", path.display()),
    })?;
    GenerationFile::from_bytes(&bytes).map_err(|e| IndexError::CorruptGeneration {
        workspace: 0,
        message: format!("{}: {e}", path.display()),
    })
}

/// Bound disk: after `newest` publishes, remove generation files (and stale
/// scratch files) whose generation is at least `KEEP_GENERATIONS` older.
/// Best effort — errors are logged, never fatal.
fn prune_generations(data_root: &Path, ws: WorkspaceId, newest: u64) {
    if newest < KEEP_GENERATIONS {
        return;
    }
    let floor = newest - KEEP_GENERATIONS;
    for dir in [generation_dir(data_root, ws), scratch_dir(data_root, ws)] {
        let Ok(rd) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in rd.flatten() {
            let Ok(name) = entry.file_name().into_string() else {
                continue;
            };
            // gen-<g>.json | gen-<g>-<pid>-<nonce>.tmp — the generation is
            // the number between "gen-" and the next "-" or ".".
            let g = name
                .strip_prefix("gen-")
                .and_then(|n| n.split(['-', '.']).next())
                .and_then(|n| n.parse::<u64>().ok());
            if let Some(g) = g {
                if g <= floor {
                    if let Err(e) = fs::remove_file(entry.path()) {
                        tracing::debug!("prune {} failed: {e}", entry.path().display());
                    }
                }
            }
        }
    }
}

// --------------------------------------------------------------- temp sweep

/// Bound on directory entries examined per sweep (enough for any plausible
/// crash residue; a hostile directory of temps cannot stall attach/open).
const SWEEP_MAX_ENTRIES: u32 = 4096;
/// Bound on generation/scratch workspace dirs examined by the open-time
/// all-workspaces sweep.
const SWEEP_MAX_DIRS: u32 = 256;
/// A temp is sweepable only after it outlived every builder lease: an
/// atomic-writer temp lives for milliseconds, so an hour-old one is
/// definitely crash residue, while a fresh one may belong to a live writer.
const SWEEP_MIN_AGE: Duration = Duration::from_secs(60 * 60);

/// One bounded-temp-sweep pass outcome (logged loudly at attach/open).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct SweepSummary {
    dirs: u32,
    entries: u32,
    removed: u32,
    kept: u32,
    failed: u32,
    truncated: bool,
}

impl SweepSummary {
    fn absorb(&mut self, other: Self) {
        self.dirs += other.dirs;
        self.entries += other.entries;
        self.removed += other.removed;
        self.kept += other.kept;
        self.failed += other.failed;
        self.truncated |= other.truncated;
    }
}

/// Names the sweep may ever remove: the shared atomic writer's
/// `.<name>.faktor-tmp-<pid>-<uuid>` temps, its pre-migration legacy
/// `.<name>.kp-tmp-<pid>-<uuid>` residue (accepted for compatibility, see
/// `faktor_fs::atomic::is_internal_temp_name`), and the legacy pre-atomic
/// staging `gen-<g>-<pid>-<nonce>.tmp` files. `gen-<g>.json` never matches.
fn is_sweepable_temp_name(name: &str) -> bool {
    faktor_fs::atomic::is_internal_temp_name(name)
        || (name.starts_with("gen-") && name.ends_with(".tmp"))
}

/// A temp is stale once its mtime is at least `min_age` in the past. An
/// unreadable/future mtime is NOT stale (never delete what cannot be proven
/// to be crash residue).
fn is_stale_temp(meta: &fs::Metadata, min_age: Duration) -> bool {
    meta.modified()
        .ok()
        .and_then(|m| SystemTime::now().duration_since(m).ok())
        .map(|age| age >= min_age)
        .unwrap_or(false)
}

/// Bounded sweep of ONE directory. Only regular files whose names look like
/// atomic/staging temps are candidates: symlinks and directories are never
/// followed and never removed, and no other name is ever touched.
fn sweep_temp_orphans_in_dir(dir: &Path, min_age: Duration, budget: &mut u32) -> SweepSummary {
    let Ok(rd) = fs::read_dir(dir) else {
        return SweepSummary::default();
    };
    let mut summary = SweepSummary {
        dirs: 1,
        ..SweepSummary::default()
    };
    for entry in rd.flatten() {
        if *budget == 0 {
            summary.truncated = true;
            break;
        }
        *budget -= 1;
        summary.entries += 1;
        let Ok(name) = entry.file_name().into_string() else {
            summary.kept += 1;
            continue;
        };
        if !is_sweepable_temp_name(&name) {
            summary.kept += 1;
            continue;
        }
        // `file_type()` never follows symlinks: a hostile link named like a
        // temp is kept (its target must not even be stat'ed as a file).
        let Ok(ft) = entry.file_type() else {
            summary.kept += 1;
            continue;
        };
        if !ft.is_file() {
            summary.kept += 1;
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(entry.path()) else {
            summary.kept += 1;
            continue;
        };
        if !is_stale_temp(&meta, min_age) {
            summary.kept += 1;
            continue;
        }
        match fs::remove_file(entry.path()) {
            Ok(()) => summary.removed += 1,
            Err(_) => summary.failed += 1,
        }
    }
    summary
}

/// Sweep one workspace's generation + scratch directories (attach path).
fn sweep_stale_temp_orphans(data_root: &Path, ws: WorkspaceId) -> SweepSummary {
    let mut budget = SWEEP_MAX_ENTRIES;
    let mut summary = SweepSummary::default();
    summary.absorb(sweep_temp_orphans_in_dir(
        &generation_dir(data_root, ws),
        SWEEP_MIN_AGE,
        &mut budget,
    ));
    summary.absorb(sweep_temp_orphans_in_dir(
        &scratch_dir(data_root, ws),
        SWEEP_MIN_AGE,
        &mut budget,
    ));
    summary
}

/// Sweep every workspace generation/scratch dir (open path), bounded in both
/// entries per dir and dirs per root.
fn sweep_all_stale_temp_orphans(data_root: &Path) -> SweepSummary {
    let mut budget = SWEEP_MAX_ENTRIES;
    let mut dir_budget = SWEEP_MAX_DIRS;
    let mut summary = SweepSummary::default();
    for base in ["generations", "scratch"] {
        let Ok(rd) = fs::read_dir(data_root.join(base)) else {
            continue;
        };
        for entry in rd.flatten() {
            if dir_budget == 0 {
                summary.truncated = true;
                break;
            }
            dir_budget -= 1;
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if !ft.is_dir() {
                continue;
            }
            summary.absorb(sweep_temp_orphans_in_dir(
                &entry.path(),
                SWEEP_MIN_AGE,
                &mut budget,
            ));
        }
    }
    summary
}

/// Loud-on-summary logging: an operator must be able to see what the sweep
/// examined and removed; a capped or failed sweep is a warning, never silent.
fn log_sweep_summary(origin: &'static str, workspace: u64, summary: SweepSummary) {
    if summary.failed > 0 || summary.truncated {
        tracing::warn!(
            origin,
            workspace,
            dirs = summary.dirs,
            entries = summary.entries,
            removed = summary.removed,
            kept = summary.kept,
            failed = summary.failed,
            truncated = summary.truncated,
            "index temp-orphan sweep incomplete (cap reached or delete failed)"
        );
    } else if summary.entries > 0 {
        tracing::info!(
            origin,
            workspace,
            dirs = summary.dirs,
            entries = summary.entries,
            removed = summary.removed,
            kept = summary.kept,
            "index temp-orphan sweep complete"
        );
    } else {
        tracing::debug!(
            origin,
            workspace,
            "index temp-orphan sweep: nothing to scan"
        );
    }
}

// ------------------------------------------------------------------ scanning

/// Filesystem modification time in epoch milliseconds (0 when unavailable).
fn modified_ms_of(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn join_rel(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

/// Open one walk frame: sorted entries plus the resume high-water. A
/// NON-ROOT directory that vanished between batches (churn during a
/// multi-batch generation) is exhausted, never a fatal build error; the root
/// itself missing is still a loud failure (the caller has no workspace).
fn open_frame(
    dir: PathBuf,
    rel: String,
    after: Option<&str>,
    tolerate_missing: bool,
) -> Result<WalkFrame, String> {
    let rd = match fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(e) if tolerate_missing && e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(WalkFrame {
                dir,
                rel,
                entries: Vec::new(),
                idx: 0,
                after: None,
            });
        }
        Err(e) => return Err(format!("read_dir {}: {e}", dir.display())),
    };
    let mut entries: Vec<(String, WalkKind)> = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let kind = match entry.file_type() {
            Ok(t) if t.is_dir() => WalkKind::Dir,
            Ok(t) if t.is_file() => WalkKind::File,
            // Symlinks and special files are never followed and never
            // indexed (they are consumed by the walk, nothing more).
            _ => WalkKind::Other,
        };
        entries.push((name, kind));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut idx = 0usize;
    if let Some(after) = after {
        while idx < entries.len() && entries[idx].0.as_str() <= after {
            idx += 1;
        }
    }
    Ok(WalkFrame {
        dir,
        rel,
        entries,
        idx,
        after: after.map(str::to_string),
    })
}

/// The resume cursor of the frames currently on the stack. A parent frame
/// still points at the directory entry whose child frame is on top: its
/// high-water is the entry BEFORE it, so a resume re-enters the child (which
/// carries its own high-water) instead of skipping it.
fn cursor_of(stack: &[WalkFrame]) -> ScanCursor {
    let frames = stack
        .iter()
        .filter(|f| f.idx > 0 || f.after.is_some() || !f.rel.is_empty())
        .map(|f| ScanFrame {
            dir: f.rel.clone(),
            after: if f.idx > 0 {
                f.entries[f.idx - 1].0.clone()
            } else {
                f.after.clone().unwrap_or_default()
            },
        })
        .collect();
    ScanCursor { frames }
}

/// Deterministic bounded batch walk (audit 5/6): sorted entries, [`SKIP_DIRS`]
/// skipped, `max_dirs` directories opened per call, `visit` invoked for every
/// regular file in canonical order.
///
/// Every loop iteration is ONE work unit (one directory entry: a file, a
/// directory, or a pop) and shares a single boundary where (a) the optional
/// slow-filesystem seam sleeps and (b) the optional cancellation token is
/// observed (audit 20). Cancelling therefore never requires an entire batch
/// or scan invocation to finish: the walk returns a [`WalkOutcome`] with
/// `cancelled = true` and the caller abandons the build without publishing.
fn walk_batch<F>(
    root: &Path,
    cursor: &ScanCursor,
    max_dirs: usize,
    pace: Option<Duration>,
    cancel: Option<&CancellationToken>,
    mut visit: F,
) -> Result<WalkOutcome, String>
where
    F: FnMut(&str, &Path) -> WalkControl,
{
    enum Step {
        Pop,
        EnterDir { dir: PathBuf, rel: String },
        VisitFile { rel: String, path: PathBuf },
    }
    let mut resume: HashMap<String, String> = cursor
        .frames
        .iter()
        .map(|f| (f.dir.clone(), f.after.clone()))
        .collect();
    let mut stack = vec![open_frame(
        root.to_path_buf(),
        String::new(),
        resume.remove("").as_deref(),
        false,
    )?];
    let mut dirs_opened = 1usize;
    loop {
        // ONE work unit: a deliberately slow filesystem sleeps here, then
        // cancellation is observed before any more work is attempted. An
        // already-cancelled walk still pays at most this one unit (the
        // in-flight syscall cannot be interrupted), which is the honest
        // residual of the hard shutdown bound.
        pace_walk_unit(pace);
        if cancel.is_some_and(CancellationToken::is_cancelled) {
            return Ok(WalkOutcome::cancelled(cursor_of(&stack)));
        }
        let step = {
            let Some(frame) = stack.last_mut() else {
                return Ok(WalkOutcome::done(ScanCursor::default(), true, None));
            };
            if frame.idx >= frame.entries.len() {
                Some(Step::Pop)
            } else {
                let (name, kind) = frame.entries[frame.idx].clone();
                match kind {
                    WalkKind::Other => {
                        frame.idx += 1;
                        None
                    }
                    WalkKind::Dir => {
                        if SKIP_DIRS.contains(&name.as_str()) {
                            frame.idx += 1;
                            None
                        } else if dirs_opened >= max_dirs {
                            return Ok(WalkOutcome::done(
                                cursor_of(&stack),
                                false,
                                Some("batch_dirs".to_string()),
                            ));
                        } else {
                            Some(Step::EnterDir {
                                dir: frame.dir.join(&name),
                                rel: join_rel(&frame.rel, &name),
                            })
                        }
                    }
                    WalkKind::File => Some(Step::VisitFile {
                        path: frame.dir.join(&name),
                        rel: join_rel(&frame.rel, &name),
                    }),
                }
            }
        };
        match step {
            None => {}
            Some(Step::Pop) => {
                stack.pop();
                // The parent's directory entry is consumed only now, when
                // its child frame is exhausted.
                if let Some(parent) = stack.last_mut() {
                    parent.idx += 1;
                }
            }
            Some(Step::EnterDir { dir, rel }) => {
                dirs_opened += 1;
                let after = resume.remove(rel.as_str());
                stack.push(open_frame(dir, rel, after.as_deref(), true)?);
            }
            Some(Step::VisitFile { rel, path }) => match visit(&rel, &path) {
                WalkControl::Continue => {
                    if let Some(frame) = stack.last_mut() {
                        frame.idx += 1;
                    }
                }
                WalkControl::Stop(reason) => {
                    return Ok(WalkOutcome::done(
                        cursor_of(&stack),
                        false,
                        Some(reason.to_string()),
                    ));
                }
            },
        }
    }
}

/// One bounded CONTENT batch (audit 5): reads at most the configured
/// files/bytes; the returned cursor resumes after the last consumed entry so
/// a hostile repository is indexed BATCH BY BATCH, never silently truncated.
/// `pace`/`cancel` are the slow-filesystem seam and the caller's token
/// (audit 20); a cancelled batch carries `cancelled = true` and must never
/// be indexed or published.
fn scan_content_batch(
    root: &Path,
    cursor: &ScanCursor,
    cfg: &ServiceConfig,
    pace: Option<Duration>,
    cancel: Option<&CancellationToken>,
) -> Result<ContentBatch, String> {
    let mut files: Vec<BatchFile> = Vec::new();
    let mut files_seen = 0u64;
    let mut indexed = 0u64;
    let mut bytes_indexed = 0u64;
    let walked = walk_batch(root, cursor, cfg.batch_dirs, pace, cancel, |rel, path| {
        if files_seen as usize >= cfg.batch_files {
            return WalkControl::Stop("batch_files");
        }
        if bytes_indexed as usize >= cfg.batch_bytes {
            return WalkControl::Stop("batch_bytes");
        }
        files_seen += 1;
        let Ok(meta) = fs::metadata(path) else {
            return WalkControl::Continue;
        };
        if meta.len() > SCAN_MAX_FILE_BYTES {
            return WalkControl::Continue;
        }
        let Ok(bytes) = fs::read(path) else {
            return WalkControl::Continue;
        };
        // Binary sniff: a NUL in the first 8 KiB means not text.
        if bytes.iter().take(8192).any(|b| *b == 0) {
            return WalkControl::Continue;
        }
        bytes_indexed = bytes_indexed.saturating_add(bytes.len() as u64);
        indexed += 1;
        files.push(BatchFile {
            rel: rel.to_string(),
            bytes,
            modified_ms: modified_ms_of(&meta),
        });
        WalkControl::Continue
    })?;
    Ok(ContentBatch {
        files,
        files_seen,
        indexed,
        bytes: bytes_indexed,
        cursor: walked.cursor,
        complete: walked.complete,
        truncated_reason: walked.reason,
        cancelled: walked.cancelled,
    })
}

/// One bounded FINGERPRINT shard pass (audit 6): stats at most `max_files`
/// files of `shard` in canonical order; a pass that exhausts the walk marks
/// the shard complete, a capped pass persists its cursor so the next pass
/// continues it — a capped fingerprint is never reported clean. `pace`/
/// `cancel` are the slow-filesystem seam and the caller's token (audit 20).
fn fingerprint_pass(
    root: &Path,
    shard: u32,
    cursor: &ScanCursor,
    max_files: usize,
    max_dirs: usize,
    pace: Option<Duration>,
    cancel: Option<&CancellationToken>,
) -> Result<FingerprintPass, String> {
    let shard = shard % FINGERPRINT_SHARDS;
    let mut entries: Vec<FingerprintEntry> = Vec::new();
    let walked = walk_batch(root, cursor, max_dirs, pace, cancel, |rel, path| {
        if fingerprint_shard(rel) != shard {
            return WalkControl::Continue;
        }
        if entries.len() >= max_files {
            return WalkControl::Stop("fingerprint_files");
        }
        let Ok(meta) = fs::metadata(path) else {
            return WalkControl::Continue;
        };
        entries.push(FingerprintEntry {
            path: rel.to_string(),
            size: meta.len(),
            modified_ms: modified_ms_of(&meta),
        });
        WalkControl::Continue
    })?;
    entries.sort();
    Ok(FingerprintPass {
        entries,
        shard_completed: walked.complete,
        cursor: walked.cursor,
        reason: walked.reason,
        cancelled: walked.cancelled,
    })
}

/// Embeddings of the NEWEST on-disk generation file of `workspace` (a
/// Dirty/Building-at-restart build resumes before the published content is
/// materialized, so the carry-over reads the durable file directly).
/// Bounded: at most [`SWEEP_MAX_ENTRIES`] directory entries examined, the
/// generation read itself capped by [`read_generation_file`]. A missing or
/// corrupt file degrades to `None` (a full re-embed, never a wrong reuse).
fn newest_generation_embeddings(data_root: &Path, ws: WorkspaceId) -> Option<EmbeddingIndex> {
    let mut budget = SWEEP_MAX_ENTRIES;
    let mut newest = 0u64;
    for entry in fs::read_dir(generation_dir(data_root, ws)).ok()?.flatten() {
        if budget == 0 {
            break;
        }
        budget -= 1;
        let Some(name) = entry.file_name().into_string().ok() else {
            continue;
        };
        let generation = name
            .strip_prefix("gen-")
            .and_then(|rest| rest.strip_suffix(".json"))
            .and_then(|n| n.parse::<u64>().ok());
        if let Some(generation) = generation {
            newest = newest.max(generation);
        }
    }
    if newest == 0 {
        return None;
    }
    let file = read_generation_file(&generation_file_path(data_root, ws, newest)).ok()?;
    Some(file.data.embeddings.sanitize())
}

#[cfg(test)]
mod legacy_tests;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
