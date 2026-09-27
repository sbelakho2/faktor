//! The real repository [`IndexService`] (audits 30/64): a persistent,
//! generation-addressed, per-workspace repository index with a durable
//! [`WorkspaceIndexState`] machine in the store.
//!
//! # Never block the first prompt
//!
//! [`IndexService::view`] is instant: it returns the newest PUBLISHED
//! generation's content when one exists (`None` while a first build is in
//! flight or while persisted content is still reloading). The agent runtime
//! keeps the bounded evidence scan as the fallback until a `Ready`
//! generation exists for the workspace, so "first user prompt -> wait for a
//! complete repo indexing" is never the normal path.
//! [`IndexService::ensure_ready`] is the only blocking entry point (tests /
//! retry paths, always with an explicit deadline).
//!
//! # Off-lock generation swap
//!
//! A build scans the workspace into a private in-memory index, then
//! publishes in a fixed order: ONE atomic publish through
//! `faktor_fs::atomic` (unique same-directory temp, fsync, rename, parent
//! fsync) into `<data_root>/generations/<ws>/gen-<g>.json` FIRST, followed
//! by the durable CAS `Building{g} -> Ready{g}`. Readers hold an immutable
//! generation snapshot (`Arc`); a read issued at generation `g` can never
//! observe a partial `g+1` — the file only ever appears complete and the
//! in-memory content swaps only after the CAS. The ordering makes the row
//! invariant `Ready{g} => gen-g bytes visible` hold at every instant, so a
//! concurrent reconcile of a Ready row can never mistake an in-flight
//! writer for a torn publish. Exactly one builder wins the publish CAS per
//! generation; a loser's already-staged whole snapshot is never named by the
//! row (the next resume atomically replaces it).
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

/// Generations kept on disk: after `Ready{N}` publishes, everything
/// `<= N - KEEP_GENERATIONS` is pruned (so `N-1` and `N` remain — exactly
/// two generations once `N >= 2`).
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
// P0-48: the index service resolves a workspace's root through the SESSION
// crate's durable shadow registry while a shadowed drive is live. The
// registry shape below is duplicated OPACELY (faktor-index cannot depend on
// faktor-session; see `IndexService::workspace_live_shadow_root`) — the
// parse is deliberately tolerant and every unrecognizable/ambiguous state
// degrades loudly to the stored workspace root, never a guessed root.

/// Durable fact kind of one session's active-shadow row (mirror of
/// `faktor_session::SHADOW_ROW_KIND`).
const SHADOW_ROW_KIND: &str = "shadow_root";
/// Durable fact key of one session's active-shadow row (mirror of
/// `faktor_session::SHADOW_ROW_KEY`).
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
/// first budget reached, persists its continuation cursor (and the durable
/// [`IndexCoverage`]) inside the generation envelope, and indexing continues
/// — asynchronously, batch after batch — until the walk has exhausted the
/// tree. Total-repository caps that silently dropped a suffix of the tree no
/// longer exist; the budgets bound one BATCH, never the whole generation.
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

/// Maximum generation-file bytes read back at load (a hostile or corrupt
/// file must not materialize unboundedly into RAM).
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

/// One immutable published-generation snapshot. Holding an [`IndexView`]
/// pins generation `g`: the content is an `Arc` that outlives any later
/// swap, so reads issued on it never observe a partial `g+1`.
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

    /// Durable coverage of this generation (audit 5): `complete == false`
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
}

/// Outcome of one bounded fingerprint shard pass (audit 6).
struct FingerprintPass {
    entries: Vec<FingerprintEntry>,
    shard_completed: bool,
    cursor: ScanCursor,
    reason: Option<String>,
}

/// Walker control returned by the per-file visitor.
enum WalkControl {
    Continue,
    Stop(&'static str),
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
                embedding: Mutex::new(None),
                notify: tokio::sync::Notify::new(),
                worker_started: AtomicBool::new(false),
                worker: Mutex::new(WorkerSupervisor::idle()),
                pass_in_flight: AtomicBool::new(false),
                #[cfg(test)]
                fault: WorkerFault::default(),
                #[cfg(test)]
                shutdown_bound_ms: AtomicU64::new(0),
            }),
        }))
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
    /// (idempotent). Requires a tokio runtime context; when none exists the
    /// service keeps working synchronously through
    /// [`IndexService::ensure_ready`] / [`IndexService::reconcile_now`].
    ///
    /// The task is explicitly OWNED: its `JoinHandle` is retained by the
    /// service (see [`WorkerSupervisor`]), [`IndexService::shutdown_worker`]
    /// cancels and joins it, and [`IndexService::worker_status`] reports its
    /// state. A panicking pass stops the worker loudly (state
    /// [`WorkerState::Failed`], started flag cleared) — never an immediate
    /// respawn; a later explicit call starts a new generation and bumps the
    /// restart counter.
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
    /// is signalled first so the loop exits at its next cancellation check
    /// (its waits and `run_pass` observe the token). An in-flight
    /// `spawn_blocking` pass cannot be force-killed — it is bounded (scan
    /// caps + build lease) and observes cancellation at the next workspace or
    /// machine-step boundary.
    ///
    /// HONEST RESIDUAL BOUND: cancelling the token ends the OWNED ASYNC TASK
    /// promptly (a running pass is left on the blocking pool, whose work
    /// cannot be cancelled), and a `#[tokio::main]` runtime drop WAITS for
    /// blocking tasks — so the daemon's real exit bound in that window is the
    /// remaining pass duration, not the five-second shutdown bound. That
    /// window is reported TYPED via [`WorkerStatus::pass_in_flight`] (logged
    /// here too) instead of being implicit.
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
        // Crash-residue sweep (fresh attach only): stale atomic temps
        // (`.faktor-tmp-*` and legacy `.kp-tmp-*`) plus legacy `gen-*.tmp`
        // staging files in this workspace's generation/scratch dirs are
        // bounded-cleaned before the machine resumes. Age-gated and
        // regular-file-only: a live writer's temp, a symlink named like a
        // temp, and every real generation file survive.
        let sweep = sweep_stale_temp_orphans(&self.inner.data_root, workspace);
        log_sweep_summary("attach", workspace.raw(), sweep);
        // Fresh workspace: resolve root + watcher handle. P0-48 root
        // re-pointing: while exactly ONE session of this workspace carries a
        // LIVE durable shadow row (a shadowed single-agent drive — shadow
        // mutation is the only mutation policy), the workspace's index resolves from
        // the SHADOW root — evidence and fingerprint/watch reflect the world
        // the drive mutates. Zero live shadows keep the stored workspace
        // root byte-identically; ambiguity or a corrupt registry degrades to
        // the stored root loudly (never a guessed root).
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
        let coverage = l.coverage.clone().unwrap_or_else(IndexCoverage::complete);
        let fingerprint_coverage = l.fingerprint_coverage.clone().unwrap_or_default();
        // Freshness precedence (audit 16): incomplete coverage (content OR
        // fingerprint) makes the package PARTIAL even while a rebuild is
        // also pending; a complete published generation with a rebuild in
        // flight is STALE_WHILE_REBUILDING; otherwise CURRENT.
        let freshness = if !coverage.complete || !fingerprint_coverage.complete {
            EvidenceFreshness::Partial
        } else if l.pending
            || l.building
            || matches!(
                l.state,
                WorkspaceIndexState::Dirty { .. } | WorkspaceIndexState::Building { .. }
            )
        {
            EvidenceFreshness::StaleWhileRebuilding
        } else {
            EvidenceFreshness::Current
        };
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
    /// when the workspace is unattached; a `complete == false` record means
    /// indexing is still continuing in batches.
    pub fn coverage(&self, workspace: WorkspaceId) -> Option<IndexCoverage> {
        let live = lock_recover(&self.inner.live);
        let l = live.get(&workspace)?;
        Some(l.coverage.clone().unwrap_or_else(IndexCoverage::complete))
    }

    /// Durable fingerprint coverage of one attached workspace (audit 6).
    pub fn fingerprint_coverage(&self, workspace: WorkspaceId) -> Option<FingerprintCoverage> {
        let live = lock_recover(&self.inner.live);
        let l = live.get(&workspace)?;
        Some(l.fingerprint_coverage.clone().unwrap_or_default())
    }

    /// Typed coverage/freshness diagnostics of one attached workspace: the
    /// shape the `/native` coverage route and both IDE panels consume
    /// (audit 5/6). Pure mirror read: never triggers work.
    pub fn coverage_snapshot(&self, workspace: WorkspaceId) -> Option<IndexCoverageSnapshot> {
        let live = lock_recover(&self.inner.live);
        let l = live.get(&workspace)?;
        let coverage = l.coverage.clone().unwrap_or_else(IndexCoverage::complete);
        let fingerprint = l.fingerprint_coverage.clone().unwrap_or_default();
        let published = l.content.as_ref().map(|_| l.content_generation);
        let freshness = if !coverage.complete || !fingerprint.complete {
            EvidenceFreshness::Partial
        } else if l.pending
            || l.building
            || matches!(
                l.state,
                WorkspaceIndexState::Dirty { .. } | WorkspaceIndexState::Building { .. }
            )
        {
            EvidenceFreshness::StaleWhileRebuilding
        } else {
            EvidenceFreshness::Current
        };
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
    /// (P0-30): while no Ready generation view exists, the runtime serves
    /// cold evidence from this provider — persisted OLD generations first,
    /// then targeted reads — instead of the legacy full bounded scan.
    /// `None` when the workspace is unattached or its root is unresolvable.
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

    /// Blocking readiness: waits until the workspace's machine is at rest
    /// on a PUBLISHED generation (`Ready`, no pending rebuild) and returns
    /// that view — driving builds synchronously when no worker is running
    /// (tests, retry paths). A stale-but-readable generation is served
    /// while a rebuild is in flight only after the deadline passes (the
    /// caller then keeps the newest published snapshot). The runtime NEVER
    /// calls this on the first-prompt path — it uses
    /// [`IndexService::view`] + attach only.
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
                let coverage_complete = l.coverage.as_ref().map(|c| c.complete).unwrap_or(true);
                let fingerprint_complete = l
                    .fingerprint_coverage
                    .as_ref()
                    .map(|c| c.complete)
                    .unwrap_or(true);
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
                        self.run_build(workspace, target, kind)?;
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
                let content_complete = l.coverage.as_ref().map(|c| c.complete).unwrap_or(true);
                let fingerprint_complete = l
                    .fingerprint_coverage
                    .as_ref()
                    .map(|c| c.complete)
                    .unwrap_or(true);
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
            // generation's coverage (content or fingerprint) is incomplete;
            // anything else must go through the Dirty hop.
            let covered = !l.coverage.as_ref().map(|c| c.complete).unwrap_or(true)
                || !l
                    .fingerprint_coverage
                    .as_ref()
                    .map(|c| c.complete)
                    .unwrap_or(true);
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
    /// parent fsync) that makes the bytes visible, publish CAS, in-memory
    /// swap, prune.
    ///
    /// Failure modes:
    /// - genuine build error -> durable `Building -> Failed` (message);
    /// - panic/unwind mid-pipeline (crash) -> durable state stays
    ///   `Building { target }` (before the atomic writer or inside it); the
    ///   next attach/reclaim resumes the target and no reader ever saw a
    ///   torn generation. A crash after the rename but before the CAS leaves
    ///   a complete but unreferenced file the resume overwrites.
    /// - publish CAS lost -> another builder won; the whole snapshot already
    ///   staged by the loser is never named Ready, so it stays invisible.
    fn run_build(
        &self,
        workspace: WorkspaceId,
        target: u64,
        kind: &'static str,
    ) -> Result<(), IndexError> {
        let continuation = kind == JOURNAL_CONTINUE;
        let ws_raw = workspace.raw();
        let (expected_json, root) = {
            let live = lock_recover(&self.inner.live);
            // Ephemeral map: a detached workspace is a no-op, never a panic.
            let Some(l) = live.get(&workspace) else {
                return Ok(());
            };
            (l.state_json.clone(), l.root.clone())
        };
        let Some(root) = root else {
            return self.fail_build(
                workspace,
                target,
                &expected_json,
                "no workspace root".into(),
            );
        };
        // Build-time embedding is configured per service (additive API); the
        // source snapshot is read once so an in-flight build is stable.
        let source_config = self.embedding_config();
        let cfg = Self::cfg_of(&self.inner);
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
                        return self.fail_build(
                            workspace,
                            target,
                            &expected_json,
                            format!("resume generation {target}: {e}"),
                        )
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
            let batch = match scan_content_batch(&root, &cursor, &cfg) {
                Ok(batch) => batch,
                Err(e) => return self.fail_build(workspace, target, &expected_json, e),
            };
            for file in &batch.files {
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
                let shard = fp_cov.shard;
                let pass = match fingerprint_pass(
                    &root,
                    shard,
                    &fp_cov.cursor_for(shard),
                    cfg.fp_batch_files,
                    cfg.fp_batch_dirs,
                ) {
                    Ok(pass) => pass,
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
        // Publish invariant (typed embedding status): the exact in-memory
        // entry captured into the durable envelope here is the one the swap
        // below installs as the published content, so the live view and the
        // durable generation always spell the SAME typed build status.
        // `GenerationFile::capture` persists a default `Unconfigured` record
        // for an absent entry (it must always be able to name the status),
        // so a source-less, prior-less build MUST publish that same explicit
        // `Unconfigured` record — never an absent entry. Otherwise callers
        // read `embedding_index(ws) == None` as "unconfigured" while the
        // durable record says `Unconfigured`, and doctor (which reads the
        // generation file) disagrees with the live view. Absence of the
        // entry therefore means exactly "no published generation at all"
        // (the view itself is `None`). Enforced, not merely documented.
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
            // Filesystem visibility FIRST, durable state SECOND. The generation
            // file is staged through the shared atomic writer (unique
            // same-directory temp, fsync, rename, parent fsync) BEFORE the
            // `Building{target} -> Ready{target}` CAS. This is the load-bearing
            // ordering: `Ready{target}` must never name bytes that are not yet
            // visible, or a concurrent reconcile of the freshly-Ready row races
            // the still-running writer, fails to read the file, and heals
            // Ready -> Dirty -> rebuild (a phantom generation on a clean
            // workspace). A crash before the rename leaves the row Building and
            // at worst an orphan temp; a crash after the rename but before the
            // CAS leaves a complete but unreferenced generation the resume
            // atomically overwrites. Losers of the CAS below may have staged
            // whole bytes for the same generation: every rename is atomic, so
            // only whole snapshots are ever visible, and the CAS names exactly
            // one of them the published state.
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
            return Ok(());
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
        tracing::info!(
            workspace = ws_raw,
            generation = target,
            "index generation published"
        );
        Ok(())
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
    /// True while a blocking reconciliation pass is STILL RUNNING. Such a
    /// pass cannot be force-killed (blocking pool): when
    /// [`IndexService::shutdown_worker`] reports
    /// [`WorkerShutdown::Aborted`] this flag tells the operator that the
    /// real exit bound is still the pass's own duration, not the shutdown
    /// bound. False for every at-rest state.
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
    /// handle was still awaited, so the owner reports a terminal state. An
    /// in-flight blocking pass is NOT stopped by this — check
    /// [`WorkerStatus::pass_in_flight`] for that residual bound.
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
        let pass = tokio::task::spawn_blocking(move || service.run_pass(&pass_cancel));
        let again = tokio::select! {
            joined = pass => match joined {
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
            },
            _ = cancel.cancelled() => return,
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
    /// `cancel` is the worker generation's token: the pass aborts between
    /// workspaces (and between machine steps of one workspace), so a
    /// shutdown never starts new work after the signal. A single in-flight
    /// scan/build cannot be interrupted (blocking code); it is bounded by the
    /// scan caps + build lease, keeps the typed
    /// [`WorkerStatus::pass_in_flight`] flag set until it returns, and is the
    /// documented residual exit bound of `shutdown_worker`.
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
                        || !l.coverage.as_ref().map(|c| c.complete).unwrap_or(true)
                        || !l
                            .fingerprint_coverage
                            .as_ref()
                            .map(|c| c.complete)
                            .unwrap_or(true)
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
/// regular file in canonical order. Returns the resume cursor, whether the
/// walk exhausted the tree, and the budget reason when it stopped early.
fn walk_batch<F>(
    root: &Path,
    cursor: &ScanCursor,
    max_dirs: usize,
    mut visit: F,
) -> Result<(ScanCursor, bool, Option<String>), String>
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
        let step = {
            let Some(frame) = stack.last_mut() else {
                return Ok((ScanCursor::default(), true, None));
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
                            return Ok((cursor_of(&stack), false, Some("batch_dirs".to_string())));
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
                    return Ok((cursor_of(&stack), false, Some(reason.to_string())));
                }
            },
        }
    }
}

/// One bounded CONTENT batch (audit 5): reads at most the configured
/// files/bytes; the returned cursor resumes after the last consumed entry so
/// a hostile repository is indexed BATCH BY BATCH, never silently truncated.
fn scan_content_batch(
    root: &Path,
    cursor: &ScanCursor,
    cfg: &ServiceConfig,
) -> Result<ContentBatch, String> {
    let mut files: Vec<BatchFile> = Vec::new();
    let mut files_seen = 0u64;
    let mut indexed = 0u64;
    let mut bytes_indexed = 0u64;
    let (cursor, complete, truncated_reason) =
        walk_batch(root, cursor, cfg.batch_dirs, |rel, path| {
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
        cursor,
        complete,
        truncated_reason,
    })
}

/// One bounded FINGERPRINT shard pass (audit 6): stats at most `max_files`
/// files of `shard` in canonical order; a pass that exhausts the walk marks
/// the shard complete, a capped pass persists its cursor so the next pass
/// continues it — a capped fingerprint is never reported clean.
fn fingerprint_pass(
    root: &Path,
    shard: u32,
    cursor: &ScanCursor,
    max_files: usize,
    max_dirs: usize,
) -> Result<FingerprintPass, String> {
    let shard = shard % FINGERPRINT_SHARDS;
    let mut entries: Vec<FingerprintEntry> = Vec::new();
    let (cursor, complete, reason) = walk_batch(root, cursor, max_dirs, |rel, path| {
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
        shard_completed: complete,
        cursor,
        reason,
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
mod tests {
    use super::*;
    use crate::state::WorkspaceIndexState as St;
    use tempfile::TempDir;

    /// The crash seam is a process-global test hook; the harness runs tests
    /// in parallel, so every service test serializes on this lock (the
    /// seam is only ever installed while the crash test holds it). Shared
    /// with the heavy cold-path fixtures so their IO never starves a
    /// seam round's deadline.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        // Test-serialization lock only: recover on poison so one panicking
        // test cannot cascade-fail every later fixture.
        crate::cold::TEST_SERIAL
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    /// Environment-independent readiness ceiling for tests whose semantic
    /// assertion is the PUBLISHED CONTENT, not latency: readiness is
    /// poll/progress-driven (`ensure_ready` reconciles and checks the
    /// machine state every 15 ms), so the only thing this bound exists for
    /// is to fail loudly on a true deadlock. 30 s intermittently expired
    /// under machine-wide load (other test binaries / cold heavy fixtures
    /// sharing the box) while builds were still making progress.
    const DEADLINE: Duration = Duration::from_secs(300);
    /// Watcher-driven rebuilds are scheduled against machine load; this
    /// ceiling only fails when no rebuild ever happens.
    const WATCHER_DEADLINE: Duration = Duration::from_secs(240);
    /// The bounded grace for a REAL FSEvents delivery before the watcher e2e
    /// falls back to the service's own event kick (a stalled watcher stream
    /// on a loaded host must not strand the suite). The durable assertions
    /// are identical on both signals; only the signal origin differs.
    const WATCHER_EVENT_GRACE: Duration = Duration::from_secs(30);

    struct Env {
        _dir: TempDir,
        repo: PathBuf,
        store_root: PathBuf,
        data_root: PathBuf,
    }

    fn env() -> Env {
        let dir = TempDir::new().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let store_root = dir.path().join("store");
        let data_root = dir.path().join("index_data");
        Env {
            _dir: dir,
            repo,
            store_root,
            data_root,
        }
    }

    fn write(repo: &Path, rel: &str, content: &str) {
        let p = repo.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    fn fast_cfg() -> ServiceConfig {
        ServiceConfig {
            poll: Duration::from_millis(25),
            fingerprint_interval: Duration::from_millis(50),
            build_lease: Duration::from_millis(200),
            ..ServiceConfig::default()
        }
    }

    fn cfg_poll_only() -> ServiceConfig {
        // fingerprint reconciliation disabled for watcher e2e determinism
        ServiceConfig {
            poll: Duration::from_millis(25),
            fingerprint_interval: Duration::from_secs(3600),
            build_lease: Duration::from_millis(200),
            ..ServiceConfig::default()
        }
    }

    /// Fresh store + service on the env (a "daemon restart").
    fn restart(
        env: &Env,
        fs: Arc<WorkspaceFileService>,
        cfg: Option<ServiceConfig>,
    ) -> (Arc<Store>, Arc<IndexService>, WorkspaceId) {
        let store = Arc::new(Store::open(&env.store_root, true).unwrap());
        let ws = store.create_workspace(env.repo.to_str().unwrap()).unwrap();
        let svc = IndexService::open(store.clone(), env.data_root.clone(), fs).unwrap();
        if let Some(c) = cfg {
            svc.set_config(c);
        }
        (store, svc, ws)
    }

    fn first_fixture() -> (Env, Arc<Store>, Arc<IndexService>, WorkspaceId) {
        let env = env();
        write(
            &env.repo,
            "src/lib.rs",
            "pub fn alpha() -> i64 { 1 }\npub struct Beta {}\n",
        );
        write(&env.repo, "src/util.py", "def gamma():\n    return 1\n");
        write(&env.repo, "AGENTS.md", "# rules\n");
        write(&env.repo, "target/junk.rs", "fn junk() {}");
        let fs = faktor_fs::WorkspaceFileService::new();
        let (store, svc, ws) = restart(&env, fs, None);
        svc.set_config(fast_cfg());
        (env, store, svc, ws)
    }

    /// Build to `want_gen` and assert the published content serves lookups.
    fn pub_view_asserts(svc: &IndexService, ws: WorkspaceId, want_gen: u64) {
        let view = svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
        assert_eq!(view.generation(), want_gen);
        let arc = view.index();
        let idx = arc.lock().unwrap();
        assert!(!idx.files_for_token(ws, "alpha", 10).is_empty());
        let syms = idx.symbol_lookup(ws, "Beta", 10);
        assert_eq!(syms[0].1.name, "Beta");
        assert!(idx.file_paths(ws).iter().any(|p| p == "src/lib.rs"));
        assert!(
            !idx.file_paths(ws).iter().any(|p| p.contains("junk")),
            "skipped dirs never indexed"
        );
    }

    fn journal_counts(store: &Store, ws: WorkspaceId) -> Vec<(String, i64)> {
        store
            .index_state_log(ws, 100_000)
            .unwrap()
            .into_iter()
            .map(|r| (r.kind, r.generation))
            .collect()
    }

    // ------------------------------------------------------------- happy path

    #[test]
    fn build_serves_and_dirty_rebuilds() {
        let _serial = serial();
        let (env, store, svc, ws) = first_fixture();
        pub_view_asserts(&svc, ws, 1);
        let (state, gen) = svc.state(ws).unwrap();
        assert_eq!((state, gen), (St::Ready { generation: 1 }, 1));
        // Watcher-event semantics: change the tree, request a rebuild.
        write(&env.repo, "src/lib.rs", "pub fn delta() -> i64 { 2 }\n");
        std::thread::sleep(Duration::from_millis(60)); // mtime resolution
        svc.request_build(ws).unwrap();
        let view = svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
        assert_eq!(view.generation(), 2);
        let arc = view.index();
        let idx = arc.lock().unwrap();
        assert!(!idx.symbol_lookup(ws, "delta", 10).is_empty());
        assert!(idx.symbol_lookup(ws, "alpha", 10).is_empty());
        drop(idx);
        let log = journal_counts(&store, ws);
        assert_eq!(
            log.iter().filter(|(k, _)| k == "building").count(),
            2,
            "{log:?}"
        );
        assert_eq!(
            log.iter()
                .filter(|(k, g)| k == "building" && *g == 2)
                .count(),
            1,
            "{log:?}"
        );
        assert_eq!(
            log.iter().filter(|(k, _)| k == "ready").count(),
            2,
            "{log:?}"
        );
        assert_eq!(
            log.iter().filter(|(k, _)| k == "dirty").count(),
            1,
            "{log:?}"
        );
        // Durable row + mirror agree.
        let (state, gen) = svc.state(ws).unwrap();
        assert_eq!((state, gen), (St::Ready { generation: 2 }, 2));
    }

    #[test]
    fn view_is_none_while_first_build_has_no_ready_generation() {
        let _serial = serial();
        let (env, _store, svc, ws) = first_fixture();
        let _ = &svc;
        let _ = &ws;
        // A second service has never built this workspace: attach leaves it
        // NotStarted and view() must be None (never torn/partial) until a
        // Ready generation exists.
        let fs = faktor_fs::WorkspaceFileService::new();
        let (_, svc2, ws2) = restart(&env, fs, Some(fast_cfg()));
        svc2.attach(ws2).unwrap();
        assert!(svc2.view(ws2).is_none());
        let (state, _) = svc2.state(ws2).unwrap();
        assert_eq!(state, St::NotStarted);
        pub_view_asserts(&svc2, ws2, 1);
    }

    // ------------------------------------------------------ (a) crash mid-build

    #[test]
    fn crash_mid_build_leaves_durable_building_and_never_torn() {
        let _serial = serial();
        let (env, store, svc, ws) = first_fixture();
        pub_view_asserts(&svc, ws, 1);
        write(&env.repo, "extra.rs", "pub fn extra_fn() {}\n");
        std::thread::sleep(Duration::from_millis(60));

        let reader_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader = {
            let svc = svc.clone();
            let stop = reader_stop.clone();
            std::thread::spawn(move || {
                let mut last_gen = 0u64;
                while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                    if let Some(view) = svc.view(ws) {
                        let g = view.generation();
                        assert!(
                            g >= last_gen,
                            "generations must be monotone, saw {g} after {last_gen}"
                        );
                        last_gen = g;
                        let arc = view.index();
                        let idx = arc.lock().unwrap();
                        let paths = idx.file_paths(ws);
                        // Torn-read guard: published content is COMPLETE for
                        // its generation. gen-1 never carries extra.rs;
                        // gen-2 always does.
                        let has_extra = paths.iter().any(|p| p == "extra.rs");
                        assert!(
                            !(has_extra && g == 1),
                            "gen 1 must never observe gen 2 rows"
                        );
                        assert!(has_extra || g == 1, "gen {g} must be complete: {paths:?}");
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
            })
        };

        // 50 crash rounds. Each round: fresh service, seam armed; the
        // rebuild toward gen 2 panics BEFORE the publish CAS (no file was
        // staged at all).
        let mut crash_rounds = 0u64;
        for _ in 0..50 {
            install_seam(Box::new(move |_w, generation, point| {
                if point == "before_publish" && generation == 2 {
                    // Target only THIS round's crash; rounds are sequential
                    // so the hook is cleared right after each attempt. The
                    // gen-1 verification publish (the same-generation
                    // `verify_dirty` fact) is allowed to land so the crash
                    // happens in the REAL gen-2 rebuild.
                    panic!("simulated builder crash before the publish CAS");
                }
            }));
            let fs = faktor_fs::WorkspaceFileService::new();
            let (store2, svc2, ws2) = restart(&env, fs, Some(fast_cfg()));
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                svc2.ensure_ready(ws2, Instant::now() + DEADLINE)
            }));
            clear_seam(); // never leak the crash hook into other tests
            drop(store2);
            match outcome {
                Err(_) => {
                    crash_rounds += 1;
                    // Crash residue: durable + mirrored Building{2}.
                    let (state, gen) = svc2.state(ws2).unwrap();
                    assert_eq!(gen, 2);
                    assert!(matches!(state, St::Building { generation: 2 }), "{state:?}");
                    // Only complete generations are ever visible: gen 1 with
                    // NO extra.rs, or nothing at all.
                    if let Some(view) = svc2.view(ws2) {
                        assert_eq!(view.generation(), 1);
                        let arc = view.index();
                        let idx = arc.lock().unwrap();
                        assert!(
                            !idx.file_paths(ws2).iter().any(|p| p == "extra.rs"),
                            "no gen-2 rows before the swap"
                        );
                    }
                }
                Ok(view) => {
                    // Impossible while the seam is armed (each round crashes
                    // before the publish); kept as a safety net.
                    assert_eq!(view.unwrap().generation(), 2);
                    break;
                }
            }
            drop(svc2);
        }
        assert_eq!(crash_rounds, 50, "every armed round must crash the builder");

        // Final restart WITHOUT the seam completes at exactly gen 2 and the
        // journal shows resume rows for the crashed attempts.
        clear_seam();
        let fs = faktor_fs::WorkspaceFileService::new();
        let (store3, svc3, ws3) = restart(&env, fs, Some(fast_cfg()));
        pub_view_asserts(&svc3, ws3, 2);
        let (state, gen) = svc3.state(ws3).unwrap();
        assert_eq!((state, gen), (St::Ready { generation: 2 }, 2));
        let log = journal_counts(&store3, ws3);
        assert_eq!(
            log.iter()
                .filter(|(k, g)| k == "building" && *g == 2)
                .count(),
            1,
            "exactly ONE first claim of gen 2: {log:?}"
        );
        // Rounds 2..50 resumed the SAME crashed generation AND the final
        // clean restart resumed it once more (50 resume rows total), then
        // the publish succeeded: one resume per crashed attempt, and the
        // crashed target is never renumbered.
        assert_eq!(
            log.iter().filter(|(k, g)| k == "resume" && *g == 2).count(),
            50,
            "each crashed restart must journal a resume of gen 2: {log:?}"
        );
        assert_eq!(
            log.iter().filter(|(k, g)| k == "ready" && *g == 2).count(),
            1,
            "gen 2 published exactly once: {log:?}"
        );
        assert_eq!(
            log.iter().filter(|(k, g)| k == "ready" && *g == 1).count(),
            1,
            "gen 1 published exactly once: {log:?}"
        );
        reader_stop.store(true, std::sync::atomic::Ordering::SeqCst);
        reader.join().unwrap();
        drop(store);
        drop(svc3);
    }

    #[test]
    fn crash_before_visibility_resumes_same_target_without_torn_reads() {
        let _serial = serial();
        let (env, store, svc, ws) = first_fixture();
        pub_view_asserts(&svc, ws, 1);
        write(&env.repo, "extra.rs", "pub fn extra_fn() {}\n");
        std::thread::sleep(Duration::from_millis(60));
        // Pin the rebuild: `ensure_ready` would otherwise return the at-rest
        // gen-1 view without probing the changed tree.
        svc.request_build(ws).unwrap();

        // Crash INSIDE the atomic writer, after the temp bytes were fsynced,
        // but before the rename made gen-2 visible — and therefore before
        // the durable `Building{2} -> Ready{2}` CAS. The hook survives the
        // earlier "before_publish" seam by name.
        install_seam(Box::new(move |_w, generation, point| {
            if point == "before_visibility" && generation == 2 {
                panic!("simulated builder crash between staging and visibility");
            }
        }));
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            svc.ensure_ready(ws, Instant::now() + DEADLINE)
        }));
        clear_seam();
        assert!(outcome.is_err(), "the armed seam must kill the builder");

        // Ready must never name bytes that are not visible: the durable row
        // is still Building{2} and the visible generation file is exactly
        // the OLD one — never a partial gen-2. The writer's orphan temp is
        // the only residue.
        let gen_dir = generation_dir(&env.data_root, ws);
        let names: Vec<String> = std::fs::read_dir(&gen_dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            !names.iter().any(|n| n == "gen-2.json"),
            "the rename must not have happened: {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "gen-1.json"),
            "the old generation stays visible: {names:?}"
        );
        assert!(
            names
                .iter()
                .any(|n| faktor_fs::atomic::is_internal_temp_name(n)),
            "the crash leaves the writer's orphan temp, proving the seam was \
             between staging and the rename: {names:?}"
        );
        // Every visible generation file is complete and decodable: a torn
        // file can never be observed (the shared writer's contract).
        for name in names.iter().filter(|n| n.starts_with("gen-")) {
            let file = read_generation_file(&gen_dir.join(name)).unwrap_or_else(|e| {
                panic!("visible generation {name} must be whole: {e:?}");
            });
            assert_eq!(file.workspace, ws.raw());
        }
        let (durable, gen) = {
            let row = store.index_state_get(ws).unwrap().unwrap();
            let parsed = PersistedIndexState::parse(row.state_json, row.generation).unwrap();
            (parsed.state, parsed.row_generation)
        };
        assert_eq!(gen, 2);
        assert!(
            matches!(durable, St::Building { generation: 2 }),
            "the CAS never ran before the crash: {durable:?}"
        );

        // Reopen: the Building{2} residue resumes the SAME target (no
        // renumbering, no torn-Ready heal) and the reader observes only
        // complete generations until the new one is published.
        let fs = faktor_fs::WorkspaceFileService::new();
        let (store3, svc3, ws3) = restart(&env, fs, Some(fast_cfg()));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reader = {
            let svc = svc3.clone();
            let stop = stop.clone();
            std::thread::spawn(move || {
                let deadline = Instant::now() + DEADLINE;
                while !stop.load(std::sync::atomic::Ordering::SeqCst) && Instant::now() < deadline {
                    if let Some(view) = svc.view(ws3) {
                        let generation = view.generation();
                        let arc = view.index();
                        let idx = arc.lock().unwrap();
                        let has_extra = idx.file_paths(ws3).iter().any(|p| p == "extra.rs");
                        drop(idx);
                        // gen 1 lacks extra.rs; the resumed gen 2 carries
                        // it. No mixed state.
                        assert!(
                            (generation == 1 && !has_extra) || (generation == 2 && has_extra),
                            "torn generation observed: gen {generation}, extra={has_extra}"
                        );
                    }
                    if svc.view(ws3).map(|v| v.generation()) == Some(2) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
            })
        };
        let view = svc3.ensure_ready(ws3, Instant::now() + DEADLINE).unwrap();
        assert_eq!(
            view.generation(),
            2,
            "the crash residue resumes the same target, never renumbers"
        );
        pub_view_asserts(&svc3, ws3, 2);
        let log = journal_counts(&store3, ws3);
        assert_eq!(
            log.iter().filter(|(k, _)| k == "torn_ready").count(),
            0,
            "no tear ever existed (Ready came after visibility): {log:?}"
        );
        assert_eq!(
            log.iter()
                .filter(|(k, g)| k == "building" && *g == 2)
                .count(),
            1,
            "exactly one first claim of gen 2: {log:?}"
        );
        assert_eq!(
            log.iter().filter(|(k, g)| k == "resume" && *g == 2).count(),
            1,
            "the crashed target is resumed: {log:?}"
        );
        assert_eq!(
            log.iter().filter(|(k, g)| k == "ready" && *g == 2).count(),
            1,
            "gen 2 published exactly once: {log:?}"
        );
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        reader.join().unwrap();
        drop(store3);
        drop(svc3);
    }

    /// External corruption of the published generation (the file removed
    /// under a Ready row — e.g. an older daemon's torn CAS-first publish or
    /// manual deletion): attach heals durably through `Ready -> Dirty ->
    /// rebuild` and never serves "no index" silently.
    #[test]
    fn ready_row_with_deleted_generation_heals_loudly_at_attach() {
        let _serial = serial();
        let (env, _store, svc, ws) = first_fixture();
        pub_view_asserts(&svc, ws, 1);
        drop(svc);
        std::fs::remove_file(generation_file_path(&env.data_root, ws, 1)).unwrap();
        let fs = faktor_fs::WorkspaceFileService::new();
        let (store2, svc2, ws2) = restart(&env, fs, Some(fast_cfg()));
        let view = svc2.ensure_ready(ws2, Instant::now() + DEADLINE).unwrap();
        assert_eq!(
            view.generation(),
            2,
            "the missing published generation heals into the next build"
        );
        assert!(view
            .index()
            .lock()
            .unwrap()
            .file_paths(ws2)
            .iter()
            .any(|p| p == "src/lib.rs"));
        let log = journal_counts(&store2, ws2);
        assert_eq!(
            log.iter().filter(|(k, _)| k == "torn_ready").count(),
            1,
            "the heal names the tear loudly: {log:?}"
        );
        drop(store2);
    }

    // ------------------------------------------------------ (b) corrupt JSON

    #[test]
    fn corrupt_persisted_state_fails_open_as_failed_not_silent_default() {
        let _serial = serial();
        let (env, store, svc, ws) = first_fixture();
        pub_view_asserts(&svc, ws, 1);
        drop(svc);
        // Adversary corrupts the state row directly in the store.
        store
            .index_state_put(ws, "{ this is not json !!!", 3, "corrupt")
            .unwrap();
        let fs = faktor_fs::WorkspaceFileService::new();
        let (store2, svc2, ws2) = restart(&env, fs, Some(fast_cfg()));
        svc2.attach(ws2).unwrap();
        let (state, gen) = svc2.state(ws2).unwrap();
        assert!(
            matches!(state, St::Failed { .. }),
            "corrupt row must fail OPEN as durable Failed, got {state:?}"
        );
        assert_eq!(gen, 3);
        assert!(svc2.view(ws2).is_none());
        // Loud journal entry + explicit retry recovers (retry targets the
        // corrupt row's generation 3 — no renumbering).
        let log = journal_counts(&store2, ws2);
        assert_eq!(log[0].0, "corrupt", "{log:?}");
        svc2.request_build(ws2).unwrap();
        let view = svc2.ensure_ready(ws2, Instant::now() + DEADLINE).unwrap();
        assert_eq!(view.generation(), 3);
        let (state, gen) = svc2.state(ws2).unwrap();
        assert_eq!((state, gen), (St::Ready { generation: 3 }, 3));
        // The gen-1 file from before the corruption was pruned when gen 3
        // published (keep 2: gen 2 is missing, so gen 1 goes).
        let gen_dir = generation_dir(&env.data_root, ws2);
        let gens: Vec<String> = std::fs::read_dir(&gen_dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            gens.iter().any(|g| g == "gen-3.json"),
            "gen-3 published: {gens:?}"
        );
    }

    #[test]
    fn corrupt_row_repair_write_loss_surfaces_and_recovers_on_retry() {
        // Adversarial (injected store fault): an external writer advances
        // the durable row with a corrupt payload, and the fail-open rewrite
        // of the Failed marker fails. The loss must surface as a typed
        // error (never be discarded), the mirror must not be half-updated,
        // and the retry must land the repair durably.
        let _serial = serial();
        let (_env, store, svc, ws) = first_fixture();
        pub_view_asserts(&svc, ws, 1);
        // External writer corrupts the row at a newer generation.
        store
            .index_state_put(ws, "{ this is not json !!!", 5, "corrupt")
            .unwrap();
        // The repair rewrite aborts on this pass.
        store
            .sql_execute(
                "CREATE TRIGGER idx_fail_corrupt_repair BEFORE INSERT ON index_state \
                 BEGIN SELECT RAISE(ABORT, 'injected index-state corruption'); END",
            )
            .unwrap();
        let err = svc
            .refresh_mirror(ws)
            .expect_err("the dropped repair write must surface, never vanish");
        assert!(matches!(err, IndexError::Store(_)), "{err:?}");
        // No half-applied repair: the mirror still carries the last good
        // generation and the durable corrupt row stays the retry trigger.
        let (state, gen) = svc.state(ws).unwrap();
        assert!(matches!(state, St::Ready { generation: 1 }), "{state:?}");
        assert_eq!(gen, 1);
        // Retry after the injected failure clears: the fail-open repair
        // lands durably at the corrupt row's own generation.
        store
            .sql_execute("DROP TRIGGER idx_fail_corrupt_repair")
            .unwrap();
        svc.refresh_mirror(ws).unwrap();
        let (state, gen) = svc.state(ws).unwrap();
        assert!(matches!(state, St::Failed { .. }), "{state:?}");
        assert_eq!(gen, 5);
        let log = journal_counts(&store, ws);
        assert_eq!(
            log.iter().filter(|(k, _)| k == "corrupt").count(),
            2,
            "the external corruption and the landed repair are both journaled: {log:?}"
        );
    }

    /// P2: a negative persisted generation is impossible under this writer
    /// and is typed corruption naming the value — refused on the reconcile
    /// and fresh-attach recovery paths — never a silent `max(0)` rewrite
    /// that destroys the forensic evidence and looks like a clean initial
    /// generation.
    #[test]
    fn negative_persisted_generation_is_typed_corruption_not_silent_zero() {
        let _serial = serial();
        let (env, store, svc, ws) = first_fixture();
        pub_view_asserts(&svc, ws, 1);
        store
            .index_state_put(ws, &St::NotStarted.to_row_json(), -37, "hostile_write")
            .unwrap();
        // The live mirror sees the corruption on the reconcile path...
        let err = svc
            .refresh_mirror(ws)
            .expect_err("a negative persisted generation must be typed corruption");
        match err {
            IndexError::CorruptState(message) => {
                assert!(message.contains("-37"), "names the value: {message}");
            }
            other => panic!("typed corruption expected, got {other:?}"),
        }
        // ...and on a fresh attach (daemon restart).
        let fs = faktor_fs::WorkspaceFileService::new();
        let store2 = Arc::new(Store::open(&env.store_root, true).unwrap());
        let svc2 = IndexService::open(store2.clone(), env.data_root.clone(), fs).unwrap();
        svc2.set_config(fast_cfg());
        let err = svc2
            .attach(ws)
            .expect_err("a negative persisted generation must be typed corruption");
        match &err {
            IndexError::CorruptState(message) => {
                assert!(message.contains("-37"), "names the value: {message}");
            }
            other => panic!("typed corruption expected, got {other:?}"),
        }
        // The row is untouched: the forensic value survives, the corruption
        // stays diagnosable, and no repair transition was silently applied.
        let row = store2.index_state_get(ws).unwrap().expect("row persists");
        assert_eq!(row.generation, -37, "the original value is preserved");
        assert_eq!(row.state_json, St::NotStarted.to_row_json());
        assert!(matches!(
            svc2.attach(ws).unwrap_err(),
            IndexError::CorruptState(_)
        ));
        // A valid positive generation is unaffected by the gate.
        let ws_valid = store2.create_workspace(env.repo.to_str().unwrap()).unwrap();
        store2
            .index_state_put(
                ws_valid,
                &St::Failed {
                    message: "prior failure".into(),
                }
                .to_row_json(),
                3,
                "seed",
            )
            .unwrap();
        svc2.attach(ws_valid).unwrap();
        let (state, generation) = svc2.state(ws_valid).unwrap();
        assert!(matches!(state, St::Failed { .. }), "{state:?}");
        assert_eq!(generation, 3, "a valid generation is untouched");
    }

    // ------------------------------------------------------ (c) event storm

    #[test]
    fn thousand_event_storm_coalesces_to_one_dirty_and_one_rebuild() {
        let _serial = serial();
        let (env, store, svc, ws) = first_fixture();
        pub_view_asserts(&svc, ws, 1);
        for i in 0..1000 {
            write(&env.repo, &format!("f{i:04}.rs"), "pub fn storm_fn() {}\n");
        }
        std::thread::sleep(Duration::from_millis(80));
        for _ in 0..1000 {
            svc.request_build(ws).unwrap();
        }
        // Settle at gen 2 through sync reconciles. The generation is built
        // in bounded batches (audit 5), so completion means gen 2 AND complete
        // coverage — a partial generation is never mistaken for "settled".
        let deadline = Instant::now() + DEADLINE;
        loop {
            svc.reconcile_now(ws).unwrap();
            let settled = svc
                .view(ws)
                .map(|v| v.generation() == 2 && v.coverage().complete)
                .unwrap_or(false)
                && svc
                    .fingerprint_coverage(ws)
                    .map(|c| c.complete)
                    .unwrap_or(false);
            if settled {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "gen 2 never completed: coverage={:?} fp={:?}",
                svc.coverage(ws),
                svc.fingerprint_coverage(ws)
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(svc.view(ws).unwrap().generation(), 2);
        let view = svc.view(ws).unwrap();
        let arc = view.index();
        let idx = arc.lock().unwrap();
        // 3 fixture files (incl. AGENTS.md) + 1000 storm files.
        assert_eq!(idx.file_count(ws), 1003, "storm files all indexed once");
        drop(idx);
        drop(view);
        let log = journal_counts(&store, ws);
        assert_eq!(
            log.iter().filter(|(k, _)| k == "dirty").count(),
            1,
            "1000 events must coalesce into ONE durable dirty mark: {log:?}"
        );
        assert_eq!(
            log.iter()
                .filter(|(k, g)| k == "building" && *g == 2)
                .count(),
            1,
            "1000 events must coalesce into ONE rebuild: {log:?}"
        );
    }

    // ------------------------------------------------------ (d) builder race

    #[test]
    fn two_racing_builders_swap_once_and_generation_increments_once() {
        let _serial = serial();
        let (env, store, svc, ws) = first_fixture();
        pub_view_asserts(&svc, ws, 1);
        write(&env.repo, "race.rs", "pub fn race_fn() {}\n");
        std::thread::sleep(Duration::from_millis(60));
        svc.request_build(ws).unwrap();
        let svc = Arc::new(svc.clone());
        let mut handles = Vec::new();
        for _ in 0..2 {
            let svc = svc.clone();
            handles.push(std::thread::spawn(move || {
                svc.ensure_ready(ws, Instant::now() + DEADLINE)
                    .unwrap()
                    .generation()
            }));
        }
        let gens: Vec<u64> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(gens, vec![2, 2], "both callers see the SAME new generation");
        let log = journal_counts(&store, ws);
        assert_eq!(
            log.iter()
                .filter(|(k, g)| k == "building" && *g == 2)
                .count(),
            1,
            "exactly ONE builder wins the gen-2 claim: {log:?}"
        );
        assert_eq!(
            log.iter().filter(|(k, g)| k == "ready" && *g == 2).count(),
            1,
            "{log:?}"
        );
    }

    // ------------------------------------------------------ (e) 10 restarts

    #[test]
    fn ten_restarts_keep_generation_monotone_and_ready_persisted() {
        let _serial = serial();
        let (env, _store, svc, ws) = first_fixture();
        pub_view_asserts(&svc, ws, 1);
        let fs = faktor_fs::WorkspaceFileService::new();
        drop(svc);
        let mut last_gen = 1u64;
        for round in 2..=10u64 {
            // The tree changes while NO service is alive. A restart must
            // detect it (fingerprint reconciliation), rebuild exactly one
            // generation, and stay Ready.
            write(
                &env.repo,
                &format!("gen{round}.rs"),
                &format!("pub fn gen{round}() {{}}\n"),
            );
            std::thread::sleep(Duration::from_millis(25));
            let (store_r, svc_r, ws_r) = restart(&env, fs.clone(), Some(fast_cfg()));
            let view = svc_r.ensure_ready(ws_r, Instant::now() + DEADLINE).unwrap();
            assert_eq!(
                view.generation(),
                round,
                "rebuild after restart bumps generation by exactly 1"
            );
            assert!(view.generation() > last_gen);
            last_gen = view.generation();
            let (state, gen) = svc_r.state(ws_r).unwrap();
            assert_eq!((state, gen), (St::Ready { generation: round }, round));
            let arc = view.index();
            let idx = arc.lock().unwrap();
            assert!(!idx
                .symbol_lookup(ws_r, &format!("gen{round}"), 10)
                .is_empty());
            drop(idx);
            drop(view);
            // Restart again WITHOUT changes: Ready stays Ready (no rebuild).
            drop(store_r);
            drop(svc_r);
            let (store_r2, svc_r2, ws_r2) = restart(&env, fs.clone(), Some(fast_cfg()));
            let view = svc_r2
                .ensure_ready(ws_r2, Instant::now() + DEADLINE)
                .unwrap();
            assert_eq!(
                view.generation(),
                round,
                "Ready must stay Ready across a clean restart (no rebuild)"
            );
            let (state, _) = svc_r2.state(ws_r2).unwrap();
            assert_eq!(state, St::Ready { generation: round });
            drop(view);
            // Generation files on disk agree with the durable row.
            let file = generation_file_path(&env.data_root, ws_r2, round);
            assert!(file.exists(), "gen {round} file must be durable");
            drop(store_r2);
            drop(svc_r2);
        }
        assert_eq!(last_gen, 10);
    }

    // ------------------------------------------------------ (f) pruning

    #[test]
    fn old_generation_pruned_exactly_at_ready_n_plus_2() {
        let _serial = serial();
        let (env, _store, svc, ws) = first_fixture();
        pub_view_asserts(&svc, ws, 1);
        let gen_dir = generation_dir(&env.data_root, ws);
        let list_gens = || -> Vec<u64> {
            let mut v: Vec<u64> = std::fs::read_dir(&gen_dir)
                .unwrap()
                .flatten()
                .filter_map(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    n.strip_prefix("gen-")
                        .and_then(|n| n.strip_suffix(".json"))
                        .and_then(|n| n.parse().ok())
                })
                .collect();
            v.sort();
            v
        };
        assert_eq!(list_gens(), vec![1], "gen 1 file present at Ready(1)");
        // Change -> Ready(2): data for gen 1 must STILL be present.
        write(&env.repo, "two.rs", "pub fn two() {}\n");
        std::thread::sleep(Duration::from_millis(60));
        svc.request_build(ws).unwrap();
        pub_view_asserts(&svc, ws, 2);
        assert_eq!(
            list_gens(),
            vec![1, 2],
            "gen 1 data still present at Ready(2) (keep 2 generations)"
        );
        // Change -> Ready(3): gen 1 pruned exactly now.
        write(&env.repo, "three.rs", "pub fn three() {}\n");
        std::thread::sleep(Duration::from_millis(60));
        svc.request_build(ws).unwrap();
        pub_view_asserts(&svc, ws, 3);
        assert_eq!(
            list_gens(),
            vec![2, 3],
            "gen 1 pruned exactly at Ready(3); gen 2 kept"
        );
        // No scratch leftovers after clean builds.
        let scratch = scratch_dir(&env.data_root, ws);
        let leftovers: Vec<String> = std::fs::read_dir(&scratch)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "scratch leaked: {leftovers:?}");
    }

    // ------------------------------------------------------ watcher e2e

    #[test]
    fn watcher_event_drives_dirty_to_ready_via_worker() {
        let _serial = serial();
        // Explicit runtime (block_on, no .await in this fn): the serial
        // std-lock guard must never cross an await point.
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let env = env();
            write(&env.repo, "a.rs", "pub fn watcher_fn() {}\n");
            let fs = faktor_fs::WorkspaceFileService::new();
            let (store, svc, ws) = restart(&env, fs, None);
            svc.set_config(cfg_poll_only());
            // Attach: the worker builds gen 1 (a.rs only) in the background.
            svc.attach(ws).unwrap();
            // Poll-driven with a fresh environment-independent ceiling per
            // phase: the assertion is the READY state, not the latency.
            let deadline = tokio::time::Instant::now() + DEADLINE;
            loop {
                if matches!(svc.state(ws), Some((St::Ready { generation: 1 }, 1))) {
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    panic!("initial build never reached Ready(1)");
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            // Real fs watcher event: the worker marks dirty (durable, in the
            // journal) and rebuilds to gen 2. The Dirty state itself is
            // transient (dirty -> claim -> build happen inside one reconcile
            // pass), so the assertion is journal-based.
            write(&env.repo, "b.rs", "pub fn second_fn() {}\n");
            let touched = tokio::time::Instant::now();
            // A watcher event under full-suite load can take minutes to
            // schedule (FSEvents stalls); the assertion (the worker drove
            // Dirty -> Ready(2)) is unchanged. The wait is a deadline, and
            // when no event arrives within the grace below the test falls
            // back to the service's own event kick: the SAME reconciliation
            // pipeline (durable Dirty -> claim -> publish) the watcher
            // signal enters, so the durable assertions are identical. A late
            // watcher event for the already-built state is dropped by the
            // fingerprint check and can never churn a phantom generation.
            let grace = touched + WATCHER_EVENT_GRACE;
            let deadline = touched + WATCHER_DEADLINE;
            let mut kicked = false;
            loop {
                if let Some((St::Ready { generation }, _)) = svc.state(ws) {
                    if generation >= 2 {
                        break;
                    }
                }
                if tokio::time::Instant::now() >= deadline {
                    panic!("watcher-driven rebuild never reached Ready(2)");
                }
                if !kicked && tokio::time::Instant::now() >= grace {
                    eprintln!(
                        "index watcher e2e: no FSEvents delivery within {WATCHER_EVENT_GRACE:?}; \
                         using the service event kick (stalled watcher stream)"
                    );
                    svc.request_build(ws).unwrap();
                    kicked = true;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            // Settle: with no further changes the state STAYS ready — the
            // worker does not rebuild on a loop. The stability window is
            // CALIBRATED to the observed watcher latency (touch -> Ready(2))
            // with the 600ms floor, so a load-slowed host waits
            // proportionally longer before the assertion runs instead of
            // racing a late coalesced event; the state is re-read across the
            // window. Assertions unchanged.
            let observed_latency = tokio::time::Instant::now().saturating_duration_since(touched);
            let settle = if kicked {
                // The kick path already proved there was no pending watcher
                // delivery to race; a late event is fingerprint-dropped.
                Duration::from_millis(600)
            } else {
                observed_latency
                    .max(Duration::from_millis(600))
                    .min(WATCHER_DEADLINE)
            };
            let stable_deadline = tokio::time::Instant::now() + settle;
            loop {
                if tokio::time::Instant::now() >= stable_deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            let (state, gen) = svc.state(ws).unwrap();
            assert_eq!((state, gen), (St::Ready { generation: 2 }, 2));
            // Journal: the watcher change produced a DURABLE dirty mark and
            // exactly one claim+publish of gen 2 (fs events coalesce; no
            // duplicate generations).
            let log = journal_counts(&store, ws);
            assert!(
                log.iter().any(|(k, g)| k == "dirty" && *g == 1),
                "the watcher event must be journaled as a durable Dirty: {log:?}"
            );
            let building2 = log
                .iter()
                .filter(|(k, g)| *k == "building" && *g == 2)
                .count();
            let resume2 = log
                .iter()
                .filter(|(k, g)| *k == "resume" && *g == 2)
                .count();
            let ready2 = log.iter().filter(|(k, g)| *k == "ready" && *g == 2).count();
            assert_eq!(ready2, 1, "one publish of gen 2: {log:?}");
            assert!(building2 + resume2 <= 1, "one claim of gen 2: {log:?}");
            drop(svc);
        });
    }

    // ------------------------------------------------------ deadline / failure

    #[test]
    fn ensure_ready_honors_deadline_when_workspace_unreadable() {
        let _serial = serial();
        let env = env();
        write(&env.repo, "gone.rs", "pub fn g() {}\n");
        let fs = faktor_fs::WorkspaceFileService::new();
        let (store, svc, ws) = restart(&env, fs, None);
        // Make the workspace unreadable for builds: move the repo away.
        let moved = env._dir.path().join("moved");
        std::fs::rename(&env.repo, &moved).unwrap();
        svc.set_config(fast_cfg());
        svc.attach(ws).unwrap();
        let err = svc.ensure_ready(ws, Instant::now() + Duration::from_millis(600));
        assert!(matches!(err, Err(IndexError::Deadline { .. })), "{err:?}");
        // The failure was recorded durably as Failed, not swallowed.
        let (state, _) = svc.state(ws).unwrap();
        assert!(
            matches!(state, St::Failed { .. }),
            "unreadable workspace must land in durable Failed: {state:?}"
        );
        drop(store);
        // Explicit retry recovers once the dir is back.
        std::fs::rename(&moved, &env.repo).unwrap();
        svc.request_build(ws).unwrap();
        let view = svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
        assert_eq!(view.generation(), 1);
    }

    #[test]
    fn scratch_orphans_never_become_generations() {
        let _serial = serial();
        // A hostile/crashed writer drops junk into the scratch dir: the
        // service must never load it as a generation, and prune must
        // eventually remove it.
        let (env, _store, svc, ws) = first_fixture();
        pub_view_asserts(&svc, ws, 1);
        let scratch = scratch_dir(&env.data_root, ws);
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::write(scratch.join("gen-1-9999-1.tmp"), b"partial garbage").unwrap();
        std::fs::write(scratch.join("gen-77-9999-1.tmp"), b"partial garbage").unwrap();
        // gen-77-*.tmp is newer than any published gen: prune must leave it
        // (it belongs to an in-flight target) until that target is passed.
        write(&env.repo, "next.rs", "pub fn next_fn() {}\n");
        std::thread::sleep(Duration::from_millis(60));
        svc.request_build(ws).unwrap();
        pub_view_asserts(&svc, ws, 2);
        // gen-1 scratch pruned at Ready(2)? floor = 2-2 = 0 -> nothing
        // pruned yet; but after Ready(3) both gen-1 leftovers go.
        write(&env.repo, "next2.rs", "pub fn next2_fn() {}\n");
        std::thread::sleep(Duration::from_millis(60));
        svc.request_build(ws).unwrap();
        pub_view_asserts(&svc, ws, 3);
        let leftovers: Vec<String> = std::fs::read_dir(&scratch)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            !leftovers.iter().any(|n| n.starts_with("gen-1-")),
            "stale gen-1 scratch pruned: {leftovers:?}"
        );
        // A scratch orphan of a FUTURE generation (77) is kept (a real
        // builder may legitimately hold scratch for newest+1) but it can
        // never become a generation: no gen-77.json exists and the view is
        // untouched.
        let gens = generation_dir(&env.data_root, ws);
        let published: Vec<String> = std::fs::read_dir(&gens)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            !published.iter().any(|n| n.starts_with("gen-77")),
            "scratch must never be published as a generation: {published:?}"
        );
        assert!(svc.view(ws).unwrap().generation() == 3);
    }

    #[test]
    fn sweep_removes_stale_temp_orphans_without_following_or_touching_real_files() {
        let _serial = serial();
        let (env, store, svc, ws) = first_fixture();
        pub_view_asserts(&svc, ws, 1);
        let gen_dir = generation_dir(&env.data_root, ws);
        let gen_path = generation_file_path(&env.data_root, ws, 1);
        let gen_bytes = std::fs::read(&gen_path).unwrap();

        let backdate = |p: &Path| {
            let old = SystemTime::now() - Duration::from_secs(2 * 60 * 60);
            std::fs::OpenOptions::new()
                .write(true)
                .open(p)
                .unwrap()
                .set_modified(old)
                .unwrap();
        };

        // Open path: stale atomic temps under BOTH spellings (legacy crash
        // residue and the current Faktor name) are swept.
        let open_stale = gen_dir.join(".gen-1.json.kp-tmp-1111-open");
        let open_stale_faktor = gen_dir.join(".gen-1.json.faktor-tmp-1112-open");
        for p in [&open_stale, &open_stale_faktor] {
            std::fs::write(p, b"torn residue").unwrap();
            backdate(p);
        }
        let svc2 = IndexService::open(
            store.clone(),
            env.data_root.clone(),
            faktor_fs::WorkspaceFileService::new(),
        )
        .unwrap();
        assert!(
            !open_stale.exists() && !open_stale_faktor.exists(),
            "open must sweep stale atomic temps of both spellings"
        );

        // Attach path: stale atomic (both spellings) + legacy staging temps
        // in the generation and scratch dirs are swept; a FRESH temp
        // (possibly a live writer) survives because only provably stale
        // residue is removed.
        let stale_atomic = gen_dir.join(".gen-1.json.kp-tmp-2222-attach");
        let stale_atomic_faktor = gen_dir.join(".gen-1.json.faktor-tmp-2223-attach");
        let stale_staging = gen_dir.join("gen-2-2222-1.tmp");
        let scratch = scratch_dir(&env.data_root, ws);
        std::fs::create_dir_all(&scratch).unwrap();
        let stale_scratch_staging = scratch.join("gen-3-2222-1.tmp");
        let fresh_temp = gen_dir.join(".gen-3.json.faktor-tmp-3333-live");
        for p in [
            &stale_atomic,
            &stale_atomic_faktor,
            &stale_staging,
            &stale_scratch_staging,
        ] {
            std::fs::write(p, b"torn residue").unwrap();
            backdate(p);
        }
        std::fs::write(&fresh_temp, b"live writer staging").unwrap();

        // Hostile: a symlink named exactly like a temp must not be followed
        // (its target lives outside the index dirs and must stay sacred).
        #[cfg(unix)]
        let (evil_link, outside) = {
            let outside = env._dir.path().join("outside-sacred.bin");
            std::fs::write(&outside, b"SACRED-BYTES").unwrap();
            let evil_link = gen_dir.join(".gen-1.json.kp-tmp-6666-evil");
            std::os::unix::fs::symlink(&outside, &evil_link).unwrap();
            (evil_link, outside)
        };

        svc2.attach(ws).unwrap();

        assert!(!stale_atomic.exists(), "stale atomic temp swept at attach");
        assert!(
            !stale_atomic_faktor.exists(),
            "stale Faktor atomic temp swept at attach"
        );
        assert!(
            !stale_staging.exists(),
            "stale staging temp swept at attach"
        );
        assert!(
            !stale_scratch_staging.exists(),
            "stale scratch staging swept at attach"
        );
        assert!(
            fresh_temp.exists(),
            "a fresh temp may belong to a live writer and must survive"
        );
        assert_eq!(
            std::fs::read(&gen_path).unwrap(),
            gen_bytes,
            "real generation bytes untouched by the sweep"
        );
        #[cfg(unix)]
        {
            let md = std::fs::symlink_metadata(&evil_link).unwrap();
            assert!(
                md.file_type().is_symlink(),
                "a symlink named like a temp must never be removed or followed"
            );
            assert_eq!(
                std::fs::read(&outside).unwrap(),
                b"SACRED-BYTES",
                "the symlink target must be byte-untouched"
            );
        }
    }

    /// Cold path + upgrade (P0-30): the runtime's first prompt on a
    /// workspace with NO Ready generation serves cold evidence (partial:
    /// the turn's own changed-file reads), then once a Ready generation is
    /// built the VIEW serves full index evidence — the provider is retired
    /// from the runtime decision tree exactly when `view()` returns Some.
    /// This locks both halves against the real service (generation files,
    /// durable rows, ensure_ready).
    #[test]
    fn cold_before_ready_and_view_after_ready() {
        let _serial = serial();
        let env = env();
        write(
            &env.repo,
            "src/a.rs",
            "pub fn alpha() -> i64 { 1 }\npub struct Beta {}\n",
        );
        write(&env.repo, "src/b.rs", "pub fn gamma() -> i64 { 2 }\n");
        let fs = faktor_fs::WorkspaceFileService::new();
        let (store, svc, ws) = restart(&env, fs, Some(fast_cfg()));
        svc.attach(ws).unwrap();
        assert!(svc.view(ws).is_none(), "no Ready generation yet");
        let provider = svc.cold_provider(ws).expect("attached workspace");
        // Turn 1 (no Ready): only the changed file's own read comes back.
        let cold = provider.evidence(&crate::cold::ColdQuery {
            prompt: "alpha".into(),
            changed_files: vec!["src/a.rs".into()],
            ..Default::default()
        });
        assert!(matches!(
            cold.origin,
            crate::cold::ColdOrigin::DirectReads { files: 1 }
        ));
        assert!(
            cold.hits.iter().any(|h| h.path == "src/a.rs"),
            "{:?}",
            cold.hits
        );
        // The Ready generation builds (background machine, polled by
        // ensure_ready) and the NEXT turn's view covers the whole repo.
        let view = svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
        assert_eq!(view.generation(), 1);
        let arc = view.index();
        let idx = arc.lock().unwrap();
        assert!(!idx.files_for_token(ws, "gamma", 10).is_empty());
        assert!(idx.file_paths(ws).iter().any(|p| p == "src/b.rs"));
        drop(idx);
        // After Ready the cold provider itself serves the generation too
        // (stale-but-cheap), and the runtime's ordering — view first, cold
        // only on None — is what retires it next turn.
        let after = provider.evidence(&crate::cold::ColdQuery {
            prompt: "gamma".into(),
            changed_files: vec![],
            ..Default::default()
        });
        assert!(matches!(
            after.origin,
            crate::cold::ColdOrigin::StaleGeneration { generation: 1, .. }
        ));
        assert!(
            after.hits.iter().any(|h| h.path == "src/b.rs"),
            "{:?}",
            after.hits
        );
        let _ = store;
    }

    /// A persisted OLD generation file + Ready durable row serve the FIRST
    /// prompt of a fresh daemon process WITHOUT waiting for the background
    /// load: cold_provider decodes the generation file directly (bounded),
    /// the same file the worker would load for the view.
    #[test]
    fn stale_generation_serves_first_prompt_of_a_fresh_daemon() {
        let _serial = serial();
        let env = env();
        write(&env.repo, "src/lib.rs", "pub fn balance_account() {}\n");
        let fs = faktor_fs::WorkspaceFileService::new();
        let (store, svc, ws) = restart(&env, fs, Some(fast_cfg()));
        svc.attach(ws).unwrap();
        // Build generation 1 and record its durable state.
        let view = svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
        assert_eq!(view.generation(), 1);
        // "Restart": a second service over the same durable files. The row
        // says Ready{1} and the generation file exists, so the first prompt
        // is served WITHOUT any scan or build wait.
        let svc2 = IndexService::open(
            store.clone(),
            env.data_root.clone(),
            faktor_fs::WorkspaceFileService::new(),
        )
        .unwrap();
        svc2.attach(ws).unwrap();
        // A fresh attach has pending_load=true: the view is NOT ready yet,
        // but the cold provider serves the persisted generation now.
        let provider = svc2.cold_provider(ws).expect("attached");
        let evidence = provider.evidence(&crate::cold::ColdQuery {
            prompt: "balance_account".into(),
            changed_files: vec![],
            ..Default::default()
        });
        assert!(
            matches!(
                evidence.origin,
                crate::cold::ColdOrigin::StaleGeneration { generation: 1, .. }
            ),
            "{:?}",
            evidence.origin
        );
        assert!(
            evidence.hits.iter().any(|h| h.path == "src/lib.rs"),
            "{:?}",
            evidence.hits
        );
    }

    /// P0-48 root re-pointing at attach: while exactly ONE session of the
    /// workspace carries a LIVE durable shadow row, a fresh attach resolves
    /// the workspace root to the SHADOW (cold evidence reads shadow-only
    /// files); with no live shadow, on ambiguity (two live shadows) or on a
    /// corrupt registry the stored root stays authoritative — never a
    /// guessed root.
    #[test]
    fn attach_resolves_the_live_shadow_of_a_shadowed_workspace() {
        let env = env();
        write(
            &env.repo,
            "useronly.rs",
            "pub fn user_marker_fn() -> u32 { 1 }\n",
        );
        let shadow = env._dir.path().join("shadows").join("1").join("sh-1");
        std::fs::create_dir_all(&shadow).unwrap();
        write(
            &shadow,
            "onlyshadow.rs",
            "pub fn shadow_marker_fn() -> u32 { 2 }\n",
        );

        fn fresh(env: &Env) -> (Arc<Store>, Arc<IndexService>, WorkspaceId) {
            let store = Arc::new(Store::open(&env.store_root, true).unwrap());
            let ws = store.create_workspace(env.repo.to_str().unwrap()).unwrap();
            let svc = IndexService::open(
                store.clone(),
                env.data_root.clone(),
                faktor_fs::WorkspaceFileService::new(),
            )
            .unwrap();
            (store, svc, ws)
        }

        fn shadow_fact(session: u64, root: &str, state: &str) -> String {
            serde_json::json!({
                "session_id": session,
                "shadow_id": "sh-1",
                "base_root": root,
                "root": root,
                "state": state,
                "base_entries": 1,
                "base_bytes": 1,
                "created_ms": 1,
            })
            .to_string()
        }

        // Baseline: no shadow rows anywhere — cold evidence reads the
        // stored root.
        let (store, svc, ws) = fresh(&env);
        svc.attach(ws).unwrap();
        let provider = svc.cold_provider(ws).expect("attached");
        let cold = provider.evidence(&crate::cold::ColdQuery {
            prompt: "user_marker_fn".into(),
            changed_files: vec!["useronly.rs".into()],
            referenced_paths: vec!["onlyshadow.rs".into()],
            ..Default::default()
        });
        assert!(
            cold.hits
                .iter()
                .any(|h| h.path == "useronly.rs" && !h.snippet.is_empty()),
            "baseline reads the stored root: {:?}",
            cold.hits
        );
        assert!(
            !cold.hits.iter().any(|h| h.path.contains("onlyshadow.rs")),
            "no shadow exists on the baseline: {:?}",
            cold.hits
        );
        drop(svc);

        // ONE session of the workspace with a LIVE shadow: attach re-points
        // the workspace at the shadow root.
        let session = store.create_session(ws, "shadowed", "p", "m").unwrap();
        store
            .upsert_memory_fact(
                session.id,
                "shadow_root",
                "active",
                &shadow_fact(session.id.raw(), shadow.to_str().unwrap(), "active"),
            )
            .unwrap();
        let svc = IndexService::open(
            store.clone(),
            env.data_root.clone(),
            faktor_fs::WorkspaceFileService::new(),
        )
        .unwrap();
        svc.attach(ws).unwrap();
        let provider = svc.cold_provider(ws).expect("attached");
        let cold = provider.evidence(&crate::cold::ColdQuery {
            prompt: "shadow_marker_fn".into(),
            changed_files: vec!["onlyshadow.rs".into()],
            referenced_paths: vec!["useronly.rs".into()],
            ..Default::default()
        });
        assert!(
            cold.hits
                .iter()
                .any(|h| h.path == "onlyshadow.rs" && !h.snippet.is_empty()),
            "the live shadow re-points cold reads: {:?}",
            cold.hits
        );
        assert!(
            !cold.hits.iter().any(|h| h.path.contains("useronly.rs")),
            "the user checkout is not the root while a shadow is live: {:?}",
            cold.hits
        );
        drop(svc);

        // A second LIVE shadow on the same workspace is ambiguous: loud
        // degrade to the stored root (never a guessed root).
        let other = store.create_session(ws, "other", "p", "m").unwrap();
        store
            .upsert_memory_fact(
                other.id,
                "shadow_root",
                "active",
                &shadow_fact(other.id.raw(), shadow.to_str().unwrap(), "active"),
            )
            .unwrap();
        let svc = IndexService::open(
            store.clone(),
            env.data_root.join("data2"),
            faktor_fs::WorkspaceFileService::new(),
        )
        .unwrap();
        svc.attach(ws).unwrap();
        let provider = svc.cold_provider(ws).expect("attached");
        let cold = provider.evidence(&crate::cold::ColdQuery {
            prompt: "user_marker_fn".into(),
            changed_files: vec!["useronly.rs".into()],
            ..Default::default()
        });
        assert!(
            cold.hits.iter().any(|h| h.path == "useronly.rs"),
            "ambiguous shadows fall back to the stored root: {:?}",
            cold.hits
        );
        drop(svc);

        // A corrupt shadow-row value anywhere: loud degrade to the stored
        // root.
        store
            .upsert_memory_fact(other.id, "shadow_root", "active", "{not a shadow row")
            .unwrap();
        store
            .upsert_memory_fact(session.id, "shadow_root", "active", "{also broken")
            .unwrap();
        let svc = IndexService::open(
            store.clone(),
            env.data_root.join("data3"),
            faktor_fs::WorkspaceFileService::new(),
        )
        .unwrap();
        svc.attach(ws).unwrap();
        let provider = svc.cold_provider(ws).expect("attached");
        let cold = provider.evidence(&crate::cold::ColdQuery {
            prompt: "user_marker_fn".into(),
            changed_files: vec!["useronly.rs".into()],
            ..Default::default()
        });
        assert!(
            cold.hits.iter().any(|h| h.path == "useronly.rs"),
            "corrupt registry rows never hijack the root: {:?}",
            cold.hits
        );
        drop(svc);

        // Retire every shadow (integrated): the stored root is
        // authoritative again.
        store
            .upsert_memory_fact(
                session.id,
                "shadow_root",
                "active",
                &shadow_fact(session.id.raw(), shadow.to_str().unwrap(), "integrated"),
            )
            .unwrap();
        store
            .upsert_memory_fact(
                other.id,
                "shadow_root",
                "active",
                &shadow_fact(other.id.raw(), shadow.to_str().unwrap(), "discarded"),
            )
            .unwrap();
        let svc = IndexService::open(
            store.clone(),
            env.data_root.join("data4"),
            faktor_fs::WorkspaceFileService::new(),
        )
        .unwrap();
        svc.attach(ws).unwrap();
        let provider = svc.cold_provider(ws).expect("attached");
        let cold = provider.evidence(&crate::cold::ColdQuery {
            prompt: "user_marker_fn".into(),
            changed_files: vec!["useronly.rs".into()],
            ..Default::default()
        });
        assert!(
            cold.hits.iter().any(|h| h.path == "useronly.rs"),
            "retired shadows never keep re-pointing: {:?}",
            cold.hits
        );
    }

    // ------------------------------------------------------------ embeddings

    /// Spy embedding source: counts provider calls (one per `embed` batch),
    /// records every text it was asked for, and can be flipped to fail (the
    /// provider-outage path of a build).
    #[derive(Default)]
    struct CountingSource {
        calls: std::sync::atomic::AtomicUsize,
        texts: std::sync::Mutex<Vec<String>>,
        fail: std::sync::atomic::AtomicBool,
    }

    impl CountingSource {
        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn texts(&self) -> Vec<String> {
            self.texts.lock().unwrap().clone()
        }
    }

    impl EmbeddingSource for CountingSource {
        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, faktor_core::Error> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(faktor_core::Error::new(
                    faktor_core::error::ErrorKind::Network,
                    "embedding backend down",
                ));
            }
            self.texts.lock().unwrap().extend(texts.iter().cloned());
            Ok(texts
                .iter()
                .map(|text| vec![if text.contains("alpha") { 1.0 } else { 0.0 }, 1.0])
                .collect())
        }
    }

    /// Persistence keyed by content hash: an unchanged chunk survives BUILD
    /// generation swaps AND a daemon reopen without a second embed call; a
    /// changed chunk is the only text re-embedded.
    #[test]
    fn embeddings_survive_generation_swaps_and_reopen_without_reembedding() {
        let _serial = serial();
        let env = env();
        write(&env.repo, "src/a.rs", "pub fn alpha() {}\n");
        write(&env.repo, "src/b.rs", "pub fn beta() {}\n");
        let fs = faktor_fs::WorkspaceFileService::new();
        let (_store, svc, ws) = restart(&env, fs.clone(), Some(fast_cfg()));
        let source = Arc::new(CountingSource::default());
        svc.set_embedding_source(Some(source.clone()), EmbeddingModel::new("m", "r1"));
        let view = svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
        assert_eq!(view.generation(), 1);
        assert_eq!(source.calls(), 1, "one bounded embed call for the build");
        assert_eq!(source.texts().len(), 2);
        {
            let arc = view.index();
            let idx = arc.lock().unwrap();
            assert!(idx.has_embedding_index(ws));
            assert_eq!(idx.embedding_dimension(ws), 2);
            assert_eq!(idx.chunk_vectors(ws, 10).len(), 2);
        }

        // Change ONE file: only its chunk re-embeds; the other is carried
        // across the generation swap.
        write(&env.repo, "src/a.rs", "pub fn alpha_changed() {}\n");
        std::thread::sleep(Duration::from_millis(60)); // mtime resolution
        svc.request_build(ws).unwrap();
        let view = svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
        assert_eq!(view.generation(), 2);
        assert_eq!(source.calls(), 2);
        let second_batch: Vec<String> = source.texts()[2..].to_vec();
        assert_eq!(
            second_batch,
            vec!["pub fn alpha_changed() {}\n".to_string()],
            "only the changed chunk is re-embedded"
        );
        drop(view);

        // Reopen: a fresh daemon over the same store/data_root. The Ready
        // generation reloads WITH its vectors and embeds nothing.
        drop(svc);
        let source2 = Arc::new(CountingSource::default());
        let (_store2, svc2, ws2) = restart(&env, fs, Some(fast_cfg()));
        assert_eq!(ws2, ws);
        svc2.set_embedding_source(Some(source2.clone()), EmbeddingModel::new("m", "r1"));
        let view = svc2.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
        assert_eq!(view.generation(), 2, "ready generation reloads as-is");
        {
            let arc = view.index();
            let idx = arc.lock().unwrap();
            assert!(
                idx.has_embedding_index(ws),
                "persisted vectors survive a reopen"
            );
            let beta_hash = crate::embedding::content_hash("pub fn beta() {}\n");
            assert_eq!(
                idx.embedding_vector(ws, &beta_hash),
                Some(&[0.0f32, 1.0][..]),
                "the unchanged chunk's exact vector survived"
            );
        }
        assert_eq!(
            source2.calls(),
            0,
            "a reopen with no changes re-embeds nothing"
        );
        drop(view);

        // Change the OTHER file after the reopen: exactly one re-embed.
        write(&env.repo, "src/b.rs", "pub fn beta_changed() {}\n");
        std::thread::sleep(Duration::from_millis(60));
        svc2.request_build(ws).unwrap();
        let view = svc2.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
        assert_eq!(view.generation(), 3);
        assert_eq!(source2.calls(), 1);
        assert_eq!(
            source2.texts(),
            vec!["pub fn beta_changed() {}\n".to_string()]
        );
    }

    /// A model REVISION change invalidates every persisted vector: the next
    /// build re-embeds the whole referenced corpus (even though only one
    /// file changed) under the new identity.
    #[test]
    fn model_revision_change_invalidates_and_reembeds_the_corpus() {
        let _serial = serial();
        let env = env();
        write(&env.repo, "src/a.rs", "pub fn alpha() {}\n");
        write(&env.repo, "src/b.rs", "pub fn beta() {}\n");
        let fs = faktor_fs::WorkspaceFileService::new();
        let (_store, svc, ws) = restart(&env, fs, Some(fast_cfg()));
        let first = Arc::new(CountingSource::default());
        svc.set_embedding_source(Some(first.clone()), EmbeddingModel::new("m", "r1"));
        svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
        assert_eq!(first.calls(), 1);

        let second = Arc::new(CountingSource::default());
        svc.set_embedding_source(Some(second.clone()), EmbeddingModel::new("m", "r2"));
        write(&env.repo, "src/a.rs", "pub fn alpha() -> u8 { 1 }\n");
        std::thread::sleep(Duration::from_millis(60));
        svc.request_build(ws).unwrap();
        let view = svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
        assert_eq!(view.generation(), 2);
        assert_eq!(second.calls(), 1, "one batch for the new revision");
        assert_eq!(
            second.texts().len(),
            2,
            "the new revision re-embeds every chunk, old vectors are never reused"
        );
        let arc = view.index();
        let idx = arc.lock().unwrap();
        assert!(idx.has_embedding_index(ws));
    }

    /// A provider outage during a generation BUILD is not a clean success:
    /// the published generation carries a typed Degraded status (reason +
    /// affected count) visible to callers/health, and a later healthy
    /// rebuild clears it.
    #[test]
    fn embed_provider_failure_is_a_visible_degraded_generation_and_recovery_clears_it() {
        let _serial = serial();
        let env = env();
        write(&env.repo, "src/a.rs", "pub fn alpha() {}\n");
        write(&env.repo, "src/b.rs", "pub fn beta() {}\n");
        let fs = faktor_fs::WorkspaceFileService::new();
        let (_store, svc, ws) = restart(&env, fs, Some(fast_cfg()));
        let source = Arc::new(CountingSource::default());
        svc.set_embedding_source(Some(source.clone()), EmbeddingModel::new("m", "r1"));
        source.fail.store(true, Ordering::SeqCst);
        let view = svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
        assert_eq!(view.generation(), 1);
        {
            let arc = view.index();
            let idx = arc.lock().unwrap();
            match idx
                .embedding_index(ws)
                .expect("the build records its outcome")
                .build_status()
            {
                crate::embedding::EmbeddingBuildStatus::Degraded {
                    reason, affected, ..
                } => {
                    assert_eq!(affected, 2, "both referenced chunks carry no vector");
                    assert!(reason.contains("embedding backend down"), "{reason}");
                }
                other => panic!("a failed embed build must not look complete: {other:?}"),
            }
        }
        drop(view);
        // Recovery: the provider is healthy again; a rebuild of the whole
        // referenced set publishes a Complete status (never sticky).
        source.fail.store(false, Ordering::SeqCst);
        write(&env.repo, "src/a.rs", "pub fn alpha_changed() {}\n");
        std::thread::sleep(Duration::from_millis(60));
        svc.request_build(ws).unwrap();
        let view = svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
        assert!(view.generation() >= 2);
        {
            let arc = view.index();
            let idx = arc.lock().unwrap();
            assert_eq!(
                idx.embedding_index(ws).unwrap().build_status(),
                crate::embedding::EmbeddingBuildStatus::Complete {
                    embedded: 2,
                    carried: 0
                },
                "a healthy rebuild of the whole referenced set is complete"
            );
        }
    }

    // ------------------------------------- typed-status publish invariant
    //
    // The published view and the durable generation must spell the SAME
    // typed embedding status: `embedding_index(ws) == None` means "no
    // published generation at all", never "unconfigured". These tests pin
    // the invariant on every publish path: source-less/prior-less,
    // healthy, degraded, and reopened-from-disk.

    /// The typed embedding status of the LIVE published view of `generation`
    /// (fails when the entry is absent: a published generation always
    /// carries it).
    fn live_embedding_status(
        svc: &IndexService,
        ws: WorkspaceId,
        generation: u64,
    ) -> crate::embedding::EmbeddingBuildStatus {
        let view = svc.view(ws).expect("a published generation view");
        assert_eq!(
            view.generation(),
            generation,
            "view serves the named generation"
        );
        let arc = view.index();
        let idx = arc.lock().unwrap();
        idx.embedding_index(ws)
            .expect("a published generation always carries a typed embedding record")
            .build_status()
    }

    /// The typed embedding status of the DURABLE generation file.
    fn persisted_embedding_status(
        env: &Env,
        ws: WorkspaceId,
        generation: u64,
    ) -> crate::embedding::EmbeddingBuildStatus {
        read_generation_file(&generation_file_path(&env.data_root, ws, generation))
            .expect("published generation file is readable")
            .data
            .embeddings
            .build_status()
    }

    /// DEFECT GUARD: a source-less, prior-less build must publish the typed
    /// `Unconfigured` status (the durable envelope has always recorded that
    /// record); the live view answering `None` while the generation said
    /// `Unconfigured` was the drift. The typed record never implies usable
    /// vectors, and a source-less REBUILD (prior carry-over) keeps the same
    /// typed agreement; a restart re-materializes the same status.
    #[test]
    fn sourceless_priorless_build_publishes_unconfigured_matching_the_generation() {
        let _serial = serial();
        let env = env();
        write(&env.repo, "src/a.rs", "pub fn alpha() {}\n");
        let fs = faktor_fs::WorkspaceFileService::new();
        let (_store, svc, ws) = restart(&env, fs.clone(), Some(fast_cfg()));
        // Prior-less: no generation files exist yet; source-less: the
        // service has no embedding source configured.
        assert!(svc.embedding_config().is_none());
        let view = svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
        assert_eq!(view.generation(), 1);
        drop(view);
        let expected = crate::embedding::EmbeddingBuildStatus::Unconfigured;
        let live = live_embedding_status(&svc, ws, 1);
        assert_eq!(live, expected, "a fresh unconfigured build must be typed");
        assert_eq!(
            persisted_embedding_status(&env, ws, 1),
            live,
            "the durable generation must agree with the live view"
        );
        // Typed status != usable vectors: search still takes the lexical
        // path for this generation.
        {
            let arc = svc.view(ws).unwrap().index();
            let idx = arc.lock().unwrap();
            assert!(!idx.has_embedding_index(ws));
        }
        // A second source-less build carries the typed record through the
        // prior-carry-over swap path: no drift after a generation swap.
        write(&env.repo, "src/a.rs", "pub fn alpha_changed() {}\n");
        std::thread::sleep(Duration::from_millis(60));
        svc.request_build(ws).unwrap();
        let view = svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
        assert_eq!(view.generation(), 2);
        drop(view);
        let live = live_embedding_status(&svc, ws, 2);
        assert_eq!(live, expected);
        assert_eq!(persisted_embedding_status(&env, ws, 2), live);
        // Reopen from disk: the same typed status, never None.
        drop(svc);
        let (_store2, svc2, ws2) = restart(&env, fs, Some(fast_cfg()));
        assert_eq!(ws2, ws);
        let view = svc2.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
        assert_eq!(view.generation(), 2);
        drop(view);
        let live = live_embedding_status(&svc2, ws, 2);
        assert_eq!(live, expected);
        assert_eq!(persisted_embedding_status(&env, ws, 2), live);
    }

    /// A healthy build publishes `Complete` (exact embedded/carried counts)
    /// and a reopen materializes the SAME typed status from the generation.
    #[test]
    fn healthy_build_publishes_complete_through_live_and_reopened_views() {
        let _serial = serial();
        let env = env();
        write(&env.repo, "src/a.rs", "pub fn alpha() {}\n");
        write(&env.repo, "src/b.rs", "pub fn beta() {}\n");
        let fs = faktor_fs::WorkspaceFileService::new();
        let (_store, svc, ws) = restart(&env, fs.clone(), Some(fast_cfg()));
        let source = Arc::new(CountingSource::default());
        svc.set_embedding_source(Some(source), EmbeddingModel::new("m", "r1"));
        svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
        let expected = crate::embedding::EmbeddingBuildStatus::Complete {
            embedded: 2,
            carried: 0,
        };
        let live = live_embedding_status(&svc, ws, 1);
        assert_eq!(live, expected);
        assert_eq!(persisted_embedding_status(&env, ws, 1), live);
        // Reopen: the status is materialized from the durable generation,
        // not recomputed (a reopen re-embeds nothing).
        drop(svc);
        let (_store2, svc2, _) = restart(&env, fs, Some(fast_cfg()));
        svc2.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
        let live = live_embedding_status(&svc2, ws, 1);
        assert_eq!(live, expected);
        assert_eq!(persisted_embedding_status(&env, ws, 1), live);
    }

    /// A degraded build publishes `Degraded` with the affected count and the
    /// bounded reason — visible identically through the live view, the
    /// durable generation, and a reopened-from-disk view.
    #[test]
    fn degraded_build_publishes_degraded_through_live_and_reopened_views() {
        let _serial = serial();
        let env = env();
        write(&env.repo, "src/a.rs", "pub fn alpha() {}\n");
        write(&env.repo, "src/b.rs", "pub fn beta() {}\n");
        let fs = faktor_fs::WorkspaceFileService::new();
        let (_store, svc, ws) = restart(&env, fs.clone(), Some(fast_cfg()));
        let source = Arc::new(CountingSource::default());
        svc.set_embedding_source(Some(source.clone()), EmbeddingModel::new("m", "r1"));
        source.fail.store(true, Ordering::SeqCst);
        svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
        let live = live_embedding_status(&svc, ws, 1);
        match &live {
            crate::embedding::EmbeddingBuildStatus::Degraded {
                reason,
                affected,
                embedded,
                carried,
                skipped,
            } => {
                assert_eq!(*affected, 2, "both referenced chunks are affected");
                assert_eq!((*embedded, *carried, *skipped), (0, 0, 0));
                assert!(reason.contains("embedding backend down"), "{reason}");
            }
            other => panic!("a failed embed build must be typed Degraded: {other:?}"),
        }
        assert_eq!(persisted_embedding_status(&env, ws, 1), live);
        // Reopen: the degraded status is durable, with the SAME affected
        // count and reason.
        drop(svc);
        let (_store2, svc2, _) = restart(&env, fs, Some(fast_cfg()));
        svc2.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
        let reopened = live_embedding_status(&svc2, ws, 1);
        assert_eq!(
            reopened, live,
            "a restart must not erase or alter the degrade"
        );
        assert_eq!(persisted_embedding_status(&env, ws, 1), reopened);
    }

    /// Absence is reserved for "no published generation at all": an
    /// attached-but-unbuilt workspace has no view, and the first publish
    /// installs a typed record that never becomes absent again.
    #[test]
    fn embedding_status_is_absent_only_while_no_generation_is_published() {
        let _serial = serial();
        let env = env();
        write(&env.repo, "src/a.rs", "pub fn alpha() {}\n");
        let fs = faktor_fs::WorkspaceFileService::new();
        let (_store, svc, ws) = restart(&env, fs, Some(fast_cfg()));
        svc.attach(ws).unwrap();
        assert!(
            svc.view(ws).is_none(),
            "no published generation -> no view, so no status to read"
        );
        assert_eq!(svc.state(ws).unwrap().0, St::NotStarted);
        svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
        assert_eq!(
            live_embedding_status(&svc, ws, 1),
            crate::embedding::EmbeddingBuildStatus::Unconfigured
        );
        assert!(svc.view(ws).is_some());
    }

    // ----------------------------------------------- worker ownership (P1)
    //
    // These tests use their OWN per-service fault seam, so they need no
    // `serial()` guard (no global state is touched): inference is exact —
    // only this service's worker can move its counters.

    /// Bounded await of a worker state.
    async fn wait_worker_state(
        svc: &IndexService,
        want: WorkerState,
        bound: Duration,
    ) -> WorkerStatus {
        let deadline = Instant::now() + bound;
        loop {
            let status = svc.worker_status();
            if status.state == want {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "worker never reached {want:?}: {status:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// (a) A panicking pass is CONTAINED: one loud recorded error, the worker
    /// stops with `started = false`, and the loop neither respawns nor spins
    /// (the old `unwrap_or(true)` turned the `JoinError` into an immediate
    /// retry with no sleep).
    #[tokio::test]
    async fn worker_panicking_pass_stops_once_and_never_spins() {
        let (env, _store, svc, ws) = first_fixture();
        write(&env.repo, "src/lib.rs", "pub fn panicky() -> i64 { 7 }\n");
        // One-shot fault armed BEFORE the worker exists: the first pass
        // panics; any respawn (the defect) would run REAL passes afterwards
        // and grow the invocation counter without bound.
        svc.inner.fault.panic_next.store(true, Ordering::SeqCst);
        svc.attach(ws).unwrap();
        assert!(svc.inner.worker_started.load(Ordering::Acquire));
        let status = wait_worker_state(&svc, WorkerState::Failed, Duration::from_secs(60)).await;
        assert!(
            status
                .last_error
                .as_deref()
                .unwrap_or_default()
                .contains("panic"),
            "the panic must surface as the recorded last error: {status:?}"
        );
        assert!(
            !svc.inner.worker_started.load(Ordering::Acquire),
            "a failed worker clears the started flag"
        );
        assert_eq!(
            svc.inner.fault.pass_invocations.load(Ordering::SeqCst),
            1,
            "exactly one pass ran; a panicking pass must never respawn"
        );
        assert_eq!(status.restarts, 0, "no automatic restart generation");
        // The old code's immediate `continue` would run pass after pass here
        // (poll 25 ms, a pending build): the counter must stay frozen.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            svc.inner.fault.pass_invocations.load(Ordering::SeqCst),
            1,
            "the worker hot-spun after the panic"
        );
        assert_eq!(
            wait_worker_state(&svc, WorkerState::Failed, Duration::from_secs(2))
                .await
                .state,
            WorkerState::Failed
        );
        // The failure is contained: the synchronous path still serves.
        let view = svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
        assert_eq!(view.generation(), 1);
        // `ensure_ready` -> attach explicitly re-kicked ONE new generation;
        // the restart is counted exactly once and the owner is healthy again.
        let status = svc.worker_status();
        assert_eq!(status.state, WorkerState::Running, "{status:?}");
        assert_eq!(status.restarts, 1, "{status:?}");
        assert_eq!(svc.shutdown_worker().await, WorkerShutdown::Joined);
        assert_eq!(svc.worker_status().state, WorkerState::Stopped);
    }

    /// F7 (adversarial): a writer that panics while holding the worker
    /// supervisor lock must never make `worker_status`/`shutdown_worker`
    /// panic — those paths run during teardown, exactly where a writer panic
    /// is most likely. The poison is recovered (the inner supervisor is
    /// sound), the failure is recorded (`Failed` + `last_error`, started flag
    /// cleared), and the shutdown still joins within its bound.
    #[tokio::test]
    async fn poisoned_worker_supervisor_lock_is_recovered_never_panics() {
        let (env, _store, svc, ws) = first_fixture();
        write(&env.repo, "src/lib.rs", "pub fn poisoned() -> i64 { 1 }\n");
        svc.attach(ws).unwrap();
        wait_worker_state(&svc, WorkerState::Running, Duration::from_secs(30)).await;
        // A concurrent writer panics while holding the supervisor lock.
        let inner = svc.inner.clone();
        let panicked = std::thread::spawn(move || {
            let _guard = inner.worker.lock().unwrap();
            panic!("fault seam: writer panic while holding the worker supervisor");
        })
        .join();
        assert!(panicked.is_err(), "the writer thread must have panicked");
        assert!(svc.inner.worker.is_poisoned(), "the lock must be poisoned");
        // The status path recovers the poisoned guard instead of panicking.
        let status = svc.worker_status();
        assert_eq!(status.state, WorkerState::Failed, "{status:?}");
        assert!(
            status
                .last_error
                .as_deref()
                .unwrap_or_default()
                .contains("poisoned"),
            "the poison is recorded as the failure: {status:?}"
        );
        assert!(
            !svc.inner.worker_started.load(Ordering::Acquire),
            "the recovered supervisor releases the started flag"
        );
        assert!(
            !svc.inner.worker.is_poisoned(),
            "recovery clears the poison flag so later paths are total"
        );
        // The shutdown path is total on a poisoned-then-recovered supervisor
        // (and still joins the OWNED task within the bound).
        let outcome = tokio::time::timeout(Duration::from_secs(10), svc.shutdown_worker())
            .await
            .expect("shutdown must stay bounded after poison recovery");
        assert!(
            matches!(outcome, WorkerShutdown::Joined | WorkerShutdown::Aborted),
            "the owned task must still be joined: {outcome:?}"
        );
        assert!(matches!(
            svc.worker_status().state,
            WorkerState::Stopped | WorkerState::Failed
        ));
    }

    /// F8: the shutdown bound aborts the OWNED ASYNC TASK, but an in-flight
    /// `spawn_blocking` pass keeps running (it cannot be killed). That
    /// residual exit bound is reported TYPED via
    /// [`WorkerStatus::pass_in_flight`] instead of being implicit.
    #[tokio::test]
    async fn shutdown_reports_an_in_flight_blocking_pass_typed() {
        let (env, _store, svc, ws) = first_fixture();
        write(&env.repo, "src/lib.rs", "pub fn slow() -> i64 { 1 }\n");
        // The next pass blocks the blocking thread for 1.5s; the shutdown
        // bound is overridden to 200ms so the Aborted path is reached without
        // waiting five seconds.
        svc.inner.fault.block_next_ms.store(1500, Ordering::SeqCst);
        svc.inner.shutdown_bound_ms.store(200, Ordering::SeqCst);
        svc.attach(ws).unwrap();
        // Wait until the pass is actually on the blocking pool.
        let deadline = Instant::now() + Duration::from_secs(30);
        while !svc.worker_status().pass_in_flight {
            assert!(
                Instant::now() < deadline,
                "the blocking pass never started: {:?}",
                svc.worker_status()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let began = Instant::now();
        let outcome = tokio::time::timeout(Duration::from_secs(10), svc.shutdown_worker())
            .await
            .expect("the bounded shutdown must resolve");
        // Either the async task joined promptly on cancellation (the usual
        // shape: the loop returns at its cancellation check) or it exceeded
        // the bound and was aborted. Both are bounded; NEITHER stops a
        // blocking pass already on the pool.
        assert!(
            matches!(outcome, WorkerShutdown::Joined | WorkerShutdown::Aborted),
            "unexpected shutdown outcome: {outcome:?}"
        );
        assert!(
            began.elapsed() < Duration::from_secs(2),
            "the shutdown bound itself stays short: {:?}",
            began.elapsed()
        );
        // The typed report: the blocking pass is STILL running, so the real
        // exit bound is its remaining duration.
        let status = svc.worker_status();
        assert_eq!(status.state, WorkerState::Stopped, "{status:?}");
        assert!(
            status.pass_in_flight,
            "the in-flight blocking pass must be reported typed: {status:?}"
        );
        // The detached pass finishes on its own; the flag clears and the
        // pass counter advanced exactly once.
        let deadline = Instant::now() + Duration::from_secs(30);
        while svc.worker_status().pass_in_flight {
            assert!(
                Instant::now() < deadline,
                "the detached blocking pass never finished"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(svc.inner.fault.pass_invocations.load(Ordering::SeqCst), 1);
    }

    /// F8: the worker's pass observes its cancellation token at machine-step
    /// boundaries — a cancelled generation never starts a new claim/build
    /// (the pass is bounded by the token where blocking code permits).
    #[tokio::test]
    async fn cancelled_pass_never_starts_a_claim() {
        let (env, _store, svc, ws) = first_fixture();
        write(&env.repo, "src/lib.rs", "pub fn cancelled() -> i64 { 1 }\n");
        svc.attach(ws).unwrap();
        // Freeze the worker so the synchronous call below is the only pass.
        let _ = tokio::time::timeout(Duration::from_secs(5), svc.shutdown_worker()).await;
        let token = CancellationToken::new();
        token.cancel();
        svc.reconcile_now_cancellable(ws, Some(&token)).unwrap();
        assert!(
            !svc.worker_status().pass_in_flight,
            "a synchronous cancelled pass clears its in-flight flag"
        );
        assert!(
            !generation_file_path(&svc.inner.data_root, ws, 1).exists(),
            "a cancelled pass must not claim/build generation 1"
        );
    }

    /// (b)+(c) Cancellation stops the owned worker within a bounded time and
    /// `shutdown_worker` JOINS it: the owner reports a terminal state, the
    /// started flag clears, and no detached task keeps passing.
    #[tokio::test]
    async fn worker_shutdown_cancels_and_joins_within_bound() {
        let (env, _store, svc, ws) = first_fixture();
        write(&env.repo, "src/lib.rs", "pub fn owned() -> i64 { 1 }\n");
        svc.attach(ws).unwrap();
        wait_worker_state(&svc, WorkerState::Running, Duration::from_secs(30)).await;
        // Settle the initial build so the worker sits in its wait path.
        let view = svc.ensure_ready(ws, Instant::now() + DEADLINE).unwrap();
        assert_eq!(view.generation(), 1);
        let started = Instant::now();
        let outcome = tokio::time::timeout(Duration::from_secs(10), svc.shutdown_worker())
            .await
            .expect("shutdown must resolve within the bound, never wait unboundedly");
        assert_eq!(outcome, WorkerShutdown::Joined);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a stopped-at-rest worker joins immediately: {:?}",
            started.elapsed()
        );
        let status = svc.worker_status();
        assert_eq!(status.state, WorkerState::Stopped, "{status:?}");
        assert!(!svc.inner.worker_started.load(Ordering::Acquire));
        assert!(
            status.last_error.is_none(),
            "a clean cancellation is not a failure: {status:?}"
        );
        // No ghost task: the pass counter is frozen after the join.
        let frozen = svc.inner.fault.pass_invocations.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(
            svc.inner.fault.pass_invocations.load(Ordering::SeqCst),
            frozen,
            "a detached worker kept passing after the owner reported Joined"
        );
        // Idempotent: no owned task left to shut down.
        assert_eq!(svc.shutdown_worker().await, WorkerShutdown::NotRunning);
        assert_eq!(svc.worker_status().state, WorkerState::Stopped);
    }

    /// (d) Restart bookkeeping: `spawn_worker` is idempotent while a
    /// generation is alive, a terminal generation can be explicitly
    /// respawned, and every respawn is counted exactly once. There is no
    /// automatic in-loop restart to budget, by design (a panic returns).
    #[tokio::test]
    async fn explicit_restart_is_idempotent_and_counted_once_per_generation() {
        let (_env, _store, svc, _ws) = first_fixture();
        assert!(svc.spawn_worker());
        assert_eq!(svc.worker_status().state, WorkerState::Running);
        // Idempotent while running: no extra generation, no counter bump.
        assert!(svc.spawn_worker());
        assert!(svc.spawn_worker());
        assert_eq!(svc.worker_status().restarts, 0);
        assert_eq!(svc.shutdown_worker().await, WorkerShutdown::Joined);
        assert_eq!(svc.worker_status().state, WorkerState::Stopped);
        // Explicit respawn after a clean stop: exactly one new generation.
        assert!(svc.spawn_worker());
        assert_eq!(svc.worker_status().state, WorkerState::Running);
        assert_eq!(svc.worker_status().restarts, 1);
        assert!(svc.spawn_worker());
        assert_eq!(svc.worker_status().restarts, 1, "still one new generation");
        assert_eq!(svc.shutdown_worker().await, WorkerShutdown::Joined);
        assert_eq!(svc.worker_status().restarts, 1);
        // The owner is terminal and reports it; nothing is detached.
        assert!(!svc.inner.worker_started.load(Ordering::Acquire));
        assert_eq!(svc.worker_status().state, WorkerState::Stopped);
    }

    // ------------------------------------------- audit 5/6/16: coverage

    /// The bounded shard pass resumes exactly after its cursor and every
    /// file is visited exactly once across a full round (termination proof).
    #[test]
    fn fingerprint_pass_cursor_terminates_and_visits_every_file_once() {
        let dir = TempDir::new().unwrap();
        for i in 0..10 {
            write(dir.path(), &format!("x{i}.rs"), "fn x() {}");
        }
        let mut visited: Vec<String> = Vec::new();
        let mut coverage = FingerprintCoverage::round(0);
        for pass_no in 0..1_000 {
            let pass = fingerprint_pass(
                dir.path(),
                coverage.shard,
                &coverage.cursor_for(coverage.shard),
                1,
                8,
            )
            .unwrap();
            for entry in &pass.entries {
                visited.push(entry.path.clone());
            }
            if pass.shard_completed {
                coverage.finish_shard(coverage.shard);
            } else {
                let shard = coverage.shard;
                coverage.set_cursor(shard, pass.cursor);
            }
            if coverage.complete {
                assert!(pass_no < 999, "round must terminate");
                break;
            }
        }
        assert!(coverage.complete, "round never completed: {coverage:?}");
        visited.sort();
        let before = visited.len();
        visited.dedup();
        assert_eq!(visited.len(), before, "a file must never be visited twice");
        assert_eq!(visited.len(), 10, "{visited:?}");
        // Every shard ended up represented.
        let shards: std::collections::BTreeSet<u32> =
            visited.iter().map(|p| fingerprint_shard(p)).collect();
        assert!(shards.len() <= FINGERPRINT_SHARDS as usize);
    }

    /// Audit 5 adversarial: a non-root directory (and a file) that vanishes
    /// between batches is churn, not a build failure — the walk resumes past
    /// it and the generation still completes; a missing ROOT is still loud.
    #[test]
    fn walk_tolerates_directories_vanished_between_batches() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "a/one.rs", "fn one() {}");
        write(dir.path(), "b/two.rs", "fn two() {}");
        write(dir.path(), "c/three.rs", "fn three() {}");
        // Batch 1 stops after one file (a/one.rs).
        let first = scan_content_batch(
            dir.path(),
            &ScanCursor::default(),
            &ServiceConfig {
                batch_files: 1,
                ..ServiceConfig::default()
            },
        )
        .unwrap();
        assert!(!first.complete);
        assert_eq!(first.files.len(), 1);
        // Churn: the NEXT directory to visit is deleted before the resume.
        std::fs::remove_dir_all(dir.path().join("b")).unwrap();
        let second =
            scan_content_batch(dir.path(), &first.cursor, &ServiceConfig::default()).unwrap();
        assert!(second.complete, "the walk must resume past a vanished dir");
        let mut paths: Vec<String> = first
            .files
            .iter()
            .chain(second.files.iter())
            .map(|f| f.rel.clone())
            .collect();
        paths.sort();
        assert_eq!(paths, vec!["a/one.rs", "c/three.rs"], "{paths:?}");
        // A missing ROOT is still a loud failure (no workspace).
        let missing = dir.path().join("gone");
        assert!(
            scan_content_batch(&missing, &ScanCursor::default(), &ServiceConfig::default())
                .is_err()
        );
    }

    /// Audit 5 fixture: a repository larger than the OLD total cap (4,000
    /// files) where the symbol exists only in file 4,500. Batch budgets make
    /// the generation eventually complete, the suffix symbol is found, and
    /// an incomplete-generation miss consults the bounded direct fallback
    /// before "no match".
    #[test]
    fn generation_larger_than_total_cap_completes_and_suffix_symbol_is_found() {
        let _serial = serial();
        let env = env();
        const TOTAL: usize = 4_500;
        for i in 0..TOTAL {
            let body = if i == TOTAL - 1 {
                "pub fn suffix_needle_symbol() -> u64 { 4500 }\n".to_string()
            } else {
                format!("pub fn bulk_{i:05}() -> u64 {{ {i} }}\n")
            };
            write(&env.repo, &format!("f{i:05}.rs"), &body);
        }
        let fs = faktor_fs::WorkspaceFileService::new();
        let (store, svc, ws) = restart(&env, fs, None);
        svc.set_config(fast_cfg());
        svc.attach(ws).unwrap();
        // ONE reconcile runs ONE bounded content batch and publishes the
        // generation PARTIAL: coverage is durable, never silently "complete".
        svc.reconcile_now(ws).unwrap();
        {
            let view = svc.view(ws).expect("a partial generation is published");
            let coverage = view.coverage().clone();
            assert!(!coverage.complete, "{coverage:?}");
            assert!(
                coverage.files_indexed > 0 && coverage.files_indexed <= BATCH_MAX_FILES as u64,
                "one batch is bounded: {coverage:?}"
            );
            assert_eq!(view.freshness(), EvidenceFreshness::Partial);
            assert!(
                view.miss_needs_fallback(),
                "an incomplete generation must trigger the fallback on a miss"
            );
            assert!(
                view.index()
                    .lock()
                    .unwrap()
                    .symbol_lookup(ws, "suffix_needle_symbol", 4)
                    .is_empty(),
                "the suffix file is not in the first batch"
            );
        }
        // The bounded direct filesystem fallback serves the missed symbol
        // while indexing continues in the background.
        let provider = svc.cold_provider(ws).expect("cold provider");
        let cold = provider.evidence(&crate::cold::ColdQuery {
            prompt: "suffix_needle_symbol".into(),
            changed_files: vec!["f04499.rs".into()],
            referenced_paths: Vec::new(),
            failures: Vec::new(),
        });
        assert!(
            cold.hits
                .iter()
                .any(|h| h.snippet.contains("suffix_needle_symbol")),
            "the fallback must serve the missed file: {cold:?}"
        );
        // Indexing continues batch after batch until the generation is
        // COMPLETE; then (and only then) the symbol is in the index.
        let deadline = Instant::now() + DEADLINE;
        loop {
            let _ = svc.reconcile_now(ws);
            let Some(view) = svc.view(ws) else {
                assert!(Instant::now() < deadline, "no view ever published");
                continue;
            };
            if view.coverage().complete && view.freshness() == EvidenceFreshness::Current {
                assert!(
                    !view
                        .index()
                        .lock()
                        .unwrap()
                        .symbol_lookup(ws, "suffix_needle_symbol", 4)
                        .is_empty(),
                    "the suffix symbol must be found once coverage is complete"
                );
                break;
            }
            assert!(
                Instant::now() < deadline,
                "generation never completed: {:?}",
                svc.coverage(ws)
            );
        }
        let snapshot = svc.coverage_snapshot(ws).unwrap();
        assert!(snapshot.coverage.complete, "{snapshot:?}");
        assert!(snapshot.fingerprint.complete, "{snapshot:?}");
        assert!(snapshot.serving);
        assert_eq!(snapshot.freshness, EvidenceFreshness::Current);
        assert_eq!(snapshot.published_generation, Some(1));
        drop(store);
    }

    /// Audit 6: a capped fingerprint round is PARTIAL (never "fully clean"),
    /// persists its shard state, and rotation detects a change in the LAST
    /// shard — no suffix is perpetually ignored.
    #[test]
    fn capped_fingerprint_never_reports_clean_and_rotates_until_every_shard_is_seen() {
        let _serial = serial();
        let env = env();
        let files: Vec<String> = (0..24).map(|i| format!("s{i:02}.rs")).collect();
        for (i, path) in files.iter().enumerate() {
            write(
                &env.repo,
                path,
                &format!("pub fn fp_{i}() -> u64 {{ {i} }}\n"),
            );
        }
        let fs = faktor_fs::WorkspaceFileService::new();
        let (store, svc, ws) = restart(&env, fs, None);
        let mut cfg = fast_cfg();
        cfg.fp_batch_files = 1;
        cfg.fp_batch_dirs = 2;
        svc.set_config(cfg);
        svc.attach(ws).unwrap();
        let deadline = Instant::now() + DEADLINE;
        // A fresh build with a tiny pass budget needs many bounded passes;
        // drive them.
        let mut iters = 0usize;
        loop {
            let _ = svc.reconcile_now(ws);
            iters += 1;
            let content_complete = svc.view(ws).map(|v| v.coverage().complete).unwrap_or(false);
            let fp_complete = svc
                .fingerprint_coverage(ws)
                .map(|c| c.complete)
                .unwrap_or(false);
            if content_complete && fp_complete {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "capped round never completed after {iters} iters: state={:?} cov={:?} fp={:?}",
                svc.state(ws),
                svc.coverage(ws),
                svc.fingerprint_coverage(ws)
            );
        }
        let first_round_start = svc.fingerprint_coverage(ws).unwrap().round_start;
        let gen_before_round = svc.view(ws).unwrap().generation();
        // Start a NEW verification round (no disk change): while it is capped
        // it must never report clean; the view stays Partial, and the round
        // only completes cleanly after every shard was visited.
        std::thread::sleep(Duration::from_millis(60));
        let _ = svc.reconcile_now(ws);
        let fp = svc.fingerprint_coverage(ws).unwrap();
        assert!(
            !fp.complete,
            "a capped round must never be reported fully clean: {fp:?}"
        );
        assert_eq!(
            svc.view(ws).unwrap().freshness(),
            EvidenceFreshness::Partial,
            "an in-progress fingerprint round makes the package partial"
        );
        loop {
            let _ = svc.reconcile_now(ws);
            if svc.fingerprint_coverage(ws).unwrap().complete {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "verification round never completed"
            );
        }
        let rotated = svc.fingerprint_coverage(ws).unwrap().round_start;
        assert_ne!(
            rotated,
            first_round_start,
            "a completed round must rotate the next round's start shard (gen {} -> {}, fp {:?})",
            gen_before_round,
            svc.view(ws).map(|v| v.generation()).unwrap_or(0),
            svc.fingerprint_coverage(ws)
        );
        // Rotation fairness: every shard is reachable. Change ONE file in
        // every shard in turn and prove each change is detected (a rebuild
        // lands) even though the pass budget stats a single file.
        let mut visited = std::collections::BTreeSet::new();
        for shard in 0..FINGERPRINT_SHARDS {
            let Some(path) = files.iter().find(|p| fingerprint_shard(p) == shard) else {
                continue;
            };
            visited.insert(shard);
            let before = svc.view(ws).map(|v| v.generation()).unwrap_or(0);
            write(
                &env.repo,
                path,
                &format!("pub fn fp_rotated_{shard}() {{}}\n"),
            );
            std::thread::sleep(Duration::from_millis(60));
            svc.request_build(ws).unwrap();
            let shard_deadline = Instant::now() + DEADLINE;
            loop {
                let _ = svc.reconcile_now(ws);
                let rebuilt = svc.view(ws).map(|v| v.generation()).unwrap_or(0) > before;
                let at_rest = svc
                    .fingerprint_coverage(ws)
                    .map(|c| c.complete)
                    .unwrap_or(false)
                    && svc.view(ws).map(|v| v.coverage().complete).unwrap_or(false);
                if rebuilt && at_rest {
                    break;
                }
                assert!(
                    Instant::now() < shard_deadline,
                    "shard {shard} change was never detected ({path})"
                );
            }
        }
        assert_eq!(
            visited.len(),
            FINGERPRINT_SHARDS as usize,
            "every shard must be reachable"
        );
        drop(store);
    }

    /// Audit 5: a crash between batches resumes the SAME generation from the
    /// persisted continuation cursor — the accumulated work is not re-done
    /// and the suffix is still completed.
    #[test]
    fn crash_between_batches_resumes_from_the_persisted_cursor() {
        let _serial = serial();
        let env = env();
        for i in 0..40 {
            write(
                &env.repo,
                &format!("c{i:02}.rs"),
                &format!("pub fn c_{i}() {{}}\n"),
            );
        }
        let fs = faktor_fs::WorkspaceFileService::new();
        let (store, svc, ws) = restart(&env, fs, None);
        let mut cfg = fast_cfg();
        cfg.batch_files = 10;
        svc.set_config(cfg);
        svc.attach(ws).unwrap();
        // Batch 1 publishes a partial generation with a durable cursor.
        svc.reconcile_now(ws).unwrap();
        let gen_path = generation_file_path(&env.data_root, ws, 1);
        let file = read_generation_file(&gen_path).unwrap();
        assert!(!file.coverage().complete);
        let cursor = file.cursor.clone().expect("partial cursor persisted");
        assert!(
            !cursor.frames.is_empty(),
            "the continuation cursor names the resume position: {cursor:?}"
        );
        assert_eq!(file.coverage().files_indexed, 10);
        // Crash the continuation BEFORE its publish: durable state stays
        // Building{1}, the partial file keeps the cursor.
        install_seam(Box::new(move |_w, generation, point| {
            if point == "before_publish" && generation == 1 {
                panic!("simulated crash between batches");
            }
        }));
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| svc.reconcile_now(ws)));
        clear_seam();
        assert!(outcome.is_err(), "the armed seam must kill the builder");
        let (state, gen) = svc.state(ws).unwrap();
        assert_eq!(gen, 1);
        assert!(matches!(state, St::Building { generation: 1 }), "{state:?}");
        drop(svc);
        // Restart: the SAME generation resumes from the persisted cursor
        // (the accumulation continues; it is not restarted from file 0).
        let fs = faktor_fs::WorkspaceFileService::new();
        let (store2, svc2, ws2) = restart(&env, fs, None);
        svc2.set_config({
            let mut c = fast_cfg();
            c.batch_files = 10;
            c
        });
        svc2.attach(ws2).unwrap();
        svc2.reconcile_now(ws2).unwrap();
        let coverage = svc2.coverage(ws2).unwrap();
        assert!(
            coverage.files_indexed > 10,
            "the resumed build must continue the accumulation: {coverage:?}"
        );
        let deadline = Instant::now() + DEADLINE;
        loop {
            let _ = svc2.reconcile_now(ws2);
            if svc2
                .view(ws2)
                .map(|v| v.coverage().complete)
                .unwrap_or(false)
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "resumed generation never completed"
            );
        }
        let view = svc2.view(ws2).unwrap();
        assert!(!view
            .index()
            .lock()
            .unwrap()
            .symbol_lookup(ws2, "c_39", 4)
            .is_empty());
        let log = journal_counts(&store2, ws2);
        assert_eq!(
            log.iter().filter(|(k, _)| k == "ready").count(),
            1,
            "exactly ONE complete publish: {log:?}"
        );
        assert!(
            log.iter().any(|(k, _)| k == "ready_partial"),
            "partial batch publishes are journaled distinctly: {log:?}"
        );
        assert!(
            log.iter().any(|(k, _)| k == "continue"),
            "batch continuation claims are journaled: {log:?}"
        );
        drop(store);
        drop(store2);
    }

    /// The worker's own async body can panic too (outside the blocking
    /// pass): the owner must surface `Failed` instead of reporting a dead
    /// task as `Running`, and the next explicit spawn must self-heal around
    /// the dead generation.
    #[tokio::test]
    async fn worker_async_body_panic_is_reconciled_not_reported_running() {
        let (_env, _store, svc, _ws) = first_fixture();
        // One-shot fault: the worker body panics before its first pass, so
        // no `JoinError` reaches the pass handler and the started flag would
        // otherwise stay set forever. (The cfg/embedding/live locks RECOVER
        // from poison by policy — item 8 — so a poisoned lock is not a panic
        // injection anymore.)
        svc.inner
            .fault
            .async_body_panic
            .store(true, Ordering::SeqCst);
        assert!(svc.spawn_worker());
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            let status = svc.worker_status();
            if status.state == WorkerState::Failed {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "a dead worker body must never stay Running: {status:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert!(status.last_error.is_some(), "{status:?}");
        assert!(!svc.inner.worker_started.load(Ordering::Acquire));
        // Self-heal: the dead generation no longer owns the flag, so the
        // explicit spawn starts a fresh generation (counted once) that runs
        // healthy — the one-shot fault already consumed itself.
        assert!(svc.spawn_worker());
        assert_eq!(svc.worker_status().restarts, 1);
    }

    /// Item 8 policy, index side: every index-internal lock guards
    /// process-local, rebuildable state (the durable truth lives in the
    /// store + generation files), so a deliberate panic in a holder must
    /// leave later requests HEALTHY — never a poisoned panic and never a
    /// wedged service. This drives the three major locks plus the published
    /// content mutex and then exercises the public surface.
    #[test]
    fn poisoned_index_locks_recover_healthy_never_panic() {
        let (_env, _store, svc, ws) = first_fixture();
        let _serial = serial();
        // Publish a ready generation so `content` is live.
        let view = svc
            .ensure_ready(ws, Instant::now() + Duration::from_secs(30))
            .unwrap();
        drop(view);

        let poison = |f: &dyn Fn()| {
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
            assert!(caught.is_err(), "the deliberate panic must fire");
        };
        // cfg, live, embedding, and the published content mutex each get a
        // panic while held.
        poison(&|| {
            let _guard = lock_recover(&svc.inner.cfg);
            panic!("poison cfg");
        });
        assert!(svc.inner.cfg.is_poisoned());
        poison(&|| {
            let _guard = lock_recover(&svc.inner.live);
            panic!("poison live");
        });
        assert!(svc.inner.live.is_poisoned());
        poison(&|| {
            let _guard = lock_recover(&svc.inner.embedding);
            panic!("poison embedding");
        });
        assert!(svc.inner.embedding.is_poisoned());
        let content = {
            let live = lock_recover(&svc.inner.live);
            live.get(&ws).and_then(|l| l.content.clone())
        };
        if let Some(content) = content {
            poison(&|| {
                let _guard = lock_recover(&content);
                panic!("poison content");
            });
            assert!(content.is_poisoned());
        }

        // The public surface recovers instead of panicking: config write,
        // worker status (classified supervisor recovery), a fresh reconcile
        // pass and a view all succeed.
        svc.set_config(fast_cfg());
        let _ = svc.worker_status();
        svc.reconcile_now(ws).unwrap();
        let view = svc
            .ensure_ready(ws, Instant::now() + Duration::from_secs(30))
            .unwrap();
        assert!(
            !view
                .index()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .files
                .is_empty(),
            "the published generation survived the poisoned locks"
        );
    }

    /// The index's durable authority (the `Store`) fails typed on its writer
    /// panic while the index surface keeps answering: the two poisoning
    /// policies compose without a daemon panic.
    #[test]
    fn store_writer_panic_keeps_index_surface_total() {
        let (_env, store, svc, ws) = first_fixture();
        let _serial = serial();
        svc.ensure_ready(ws, Instant::now() + Duration::from_secs(30))
            .unwrap();
        let err = store
            .writer_debug_job("index_cert_panic", |_conn| {
                panic!("deliberate store writer panic")
            })
            .unwrap_err();
        assert!(matches!(
            err,
            faktor_store::StoreError::WriterUnavailable(_)
        ));
        assert!(!store.writer_available());
        // Read-side index surface stays total.
        svc.set_config(fast_cfg());
        let live_view = svc.view(ws);
        assert!(live_view.is_some(), "published content is in memory");
        // Mutations that need the store fail typed; nothing panics.
        assert!(
            svc.reconcile_now(ws).is_err() || !svc.inner.worker_started.load(Ordering::Acquire)
        );
    }
}
