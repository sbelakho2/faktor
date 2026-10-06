//! Workspace handle registry: open/close, rooted resolution, watcher and
//! event plumbing for [`WorkspaceFileService`]. Split out of `lib.rs` for the
//! source-size ceiling; the public surface is re-exported from the crate root.

use super::*;

/// First process-local identity handed to candidate/integration roots.
/// Durable workspace ids come from the store sequence (far below this
/// base), so a candidate id can never collide with a real workspace row.
pub(crate) const EPHEMERAL_WORKSPACE_ID_BASE: u64 = 1 << 63;

/// How many registrations may wait behind the singleton worker. Once full,
/// further registrations degrade immediately instead of allocating anything.
const WATCH_REGISTRATION_QUEUE_DEPTH: usize = 2;

/// Default preflight bound: entries examined before a root is handed to the
/// recursive watcher backend. Above this the service degrades to fingerprint
/// reconciliation instead of risking an unbounded recursive walk.
pub(crate) const DEFAULT_WATCH_PREFLIGHT_ENTRIES: usize = 50_000;
const WATCH_PREFLIGHT_MAX_DEPTH: usize = 64;

/// Bounded, symlink-safe preflight walk: returns false as soon as the entry
/// cap or depth cap is exceeded, so the caller never invokes the
/// uninterruptible backend on a pathological tree.
fn watch_preflight_bounded(root: &std::path::Path, max_entries: usize) -> bool {
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut seen = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        if depth > WATCH_PREFLIGHT_MAX_DEPTH {
            return false;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            seen += 1;
            if seen > max_entries {
                return false;
            }
            // `file_type` does not follow symlinks: a symlinked directory is
            // never traversed by the preflight (and `notify` defaults are
            // configured no-follow elsewhere).
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push((entry.path(), depth + 1));
            }
        }
    }
    true
}

/// Outcome of one bounded watcher registration (P2-FS).
enum WatchRegistrationOutcome {
    /// Watcher attached; the handle owns it.
    Registered(RecommendedWatcher),
    /// The backend refused registration (fatal, as before).
    Failed(String),
    /// Deadline elapsed / queue full / worker gone: open WITHOUT a watcher
    /// (fingerprint reconciliation only). The singleton worker may still be
    /// wedged on a pathological root, but no per-root thread can accumulate.
    Degraded,
}

struct WatchRegistrationRequest {
    watcher: RecommendedWatcher,
    root: PathBuf,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    reply: std::sync::mpsc::SyncSender<(RecommendedWatcher, notify::Result<()>)>,
}

/// The ONE watcher-registration worker (P2-FS). `notify`'s recursive watch
/// walks the tree inside the calling thread and offers no cancellation, so a
/// per-registration helper thread would accumulate one permanently blocked
/// thread per timed-out pathological root. Registrations are therefore
/// serialized through this bounded-queue singleton: the number of live
/// registration threads is at most one no matter how many roots time out.
static WATCH_REGISTRATION_QUEUE: std::sync::OnceLock<
    std::sync::mpsc::SyncSender<WatchRegistrationRequest>,
> = std::sync::OnceLock::new();

#[cfg(test)]
static WATCH_REGISTRATION_WORKER_SPAWNS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub(crate) fn watch_registration_worker_spawns() -> usize {
    WATCH_REGISTRATION_WORKER_SPAWNS.load(std::sync::atomic::Ordering::SeqCst)
}

fn watcher_registration_queue() -> &'static std::sync::mpsc::SyncSender<WatchRegistrationRequest> {
    WATCH_REGISTRATION_QUEUE.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::sync_channel::<WatchRegistrationRequest>(
            WATCH_REGISTRATION_QUEUE_DEPTH,
        );
        #[cfg(test)]
        WATCH_REGISTRATION_WORKER_SPAWNS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let _ = std::thread::Builder::new()
            .name("faktor-watch-registration".into())
            .spawn(move || {
                while let Ok(request) = rx.recv() {
                    if request.cancel.load(std::sync::atomic::Ordering::SeqCst) {
                        // The caller already degraded: drop its watcher now.
                        continue;
                    }
                    let mut watcher = request.watcher;
                    let result = watcher.watch(&request.root, RecursiveMode::Recursive);
                    // A timed-out caller is gone: the send fails and the
                    // watcher is dropped with the message.
                    let _ = request.reply.send((watcher, result));
                }
            });
        tx
    })
}

/// Register one recursive watcher under a wall deadline through the bounded
/// singleton worker. Never blocks on a full queue and never spawns a thread
/// per call.
fn register_watch_bounded(
    watcher: RecommendedWatcher,
    root: PathBuf,
    deadline: std::time::Duration,
) -> WatchRegistrationOutcome {
    use std::sync::atomic::Ordering;
    let (reply, response) = std::sync::mpsc::sync_channel(1);
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let request = WatchRegistrationRequest {
        watcher,
        root,
        cancel: cancel.clone(),
        reply,
    };
    match watcher_registration_queue().try_send(request) {
        Ok(()) => {}
        Err(std::sync::mpsc::TrySendError::Full(_))
        | Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
            cancel.store(true, Ordering::SeqCst);
            return WatchRegistrationOutcome::Degraded;
        }
    }
    match response.recv_timeout(deadline) {
        Ok((watcher, Ok(()))) => WatchRegistrationOutcome::Registered(watcher),
        Ok((_watcher, Err(e))) => {
            WatchRegistrationOutcome::Failed(format!("watch registration failed: {e}"))
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            // Cooperative cancellation: if the worker has not started the
            // registration yet, it drops the watcher without walking.
            cancel.store(true, Ordering::SeqCst);
            WatchRegistrationOutcome::Degraded
        }
    }
}

/// Registry of open workspace ROOTS. A durable workspace id may be open at
/// several roots at once: its real root and the live shadow root of an
/// active shadow-mutation drive (the drive reads the world it mutates).
/// `open` is idempotent per (id, root); identity stays the id.
#[derive(Debug)]
pub struct WorkspaceFileService {
    pub(crate) workspaces: Mutex<HashMap<(WorkspaceId, PathBuf), WorkspaceHandle>>,
    /// Bounded recursive-watcher registration deadline (see
    /// [`DEFAULT_WATCH_REGISTRATION_DEADLINE`]).
    watch_registration_deadline: std::time::Duration,
    /// Bounded pre-registration tree examination cap (P1-7): a root with more
    /// entries than this is refused WITHOUT ever handing it to the
    /// uninterruptible `notify` backend, so one pathological workspace can
    /// never wedge the singleton worker for every other workspace.
    watch_preflight_entries: usize,
    /// Monotonic allocator for [`Self::open_ephemeral`] identities.
    next_ephemeral_id: AtomicU64,
}

impl Default for WorkspaceFileService {
    fn default() -> Self {
        Self {
            workspaces: Mutex::new(HashMap::new()),
            watch_registration_deadline: DEFAULT_WATCH_REGISTRATION_DEADLINE,
            watch_preflight_entries: DEFAULT_WATCH_PREFLIGHT_ENTRIES,
            next_ephemeral_id: AtomicU64::new(EPHEMERAL_WORKSPACE_ID_BASE),
        }
    }
}

impl WorkspaceFileService {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A service with an explicit watcher-registration deadline: tests (and
    /// callers whose roots must never pay more than a short bound) can force
    /// the degraded path deterministically.
    pub fn with_watch_registration_deadline(deadline: std::time::Duration) -> Arc<Self> {
        Arc::new(Self {
            watch_registration_deadline: deadline,
            ..Self::default()
        })
    }

    /// A service with an explicit watcher preflight entry cap: tests prove a
    /// pathological root degrades WITHOUT wedging the singleton worker while
    /// an ordinary root still attaches a real watcher.
    pub fn with_watch_preflight_entries(entries: usize) -> Arc<Self> {
        Arc::new(Self {
            watch_preflight_entries: entries,
            ..Self::default()
        })
    }

    pub fn open(&self, workspace_id: WorkspaceId, root: PathBuf) -> Result<WorkspaceHandle, Error> {
        // Canonicalize first so idempotency compares equal paths.
        let root = root
            .canonicalize()
            .map_err(|e| Error::not_found(format!("workspace root {}: {e}", root.display())))?;
        {
            let map = recover_lock(&self.workspaces);
            if let Some(h) = map.get(&(workspace_id, root.clone())) {
                return Ok(h.clone());
            }
        }
        self.build_and_register(workspace_id, root)
    }

    /// Open a process-local, NON-durable root (a run's candidate/integration
    /// directory). The id is allocated from a reserved high range and is
    /// never persisted; the handle is NOT cached, so dropping it releases
    /// the watcher (a candidate root must not accumulate registrations
    /// across runs). Durable ids are unaffected: `open` keeps refusing an
    /// id/root mismatch.
    pub fn open_ephemeral(&self, root: PathBuf) -> Result<WorkspaceHandle, Error> {
        let root = root
            .canonicalize()
            .map_err(|e| Error::not_found(format!("workspace root {}: {e}", root.display())))?;
        if !root.is_dir() {
            return Err(Error::not_found(format!(
                "workspace root {} is not a directory",
                root.display()
            )));
        }
        let id = WorkspaceId::new(self.next_ephemeral_id.fetch_add(1, Ordering::SeqCst));
        self.build_handle(id, root)
    }

    fn build_and_register(
        &self,
        workspace_id: WorkspaceId,
        root: PathBuf,
    ) -> Result<WorkspaceHandle, Error> {
        if !root.is_dir() {
            return Err(Error::not_found(format!(
                "workspace root {} is not a directory",
                root.display()
            )));
        }
        let handle = self.build_handle(workspace_id, root.clone())?;
        recover_lock(&self.workspaces).insert((workspace_id, root), handle.clone());
        Ok(handle)
    }

    fn build_handle(
        &self,
        workspace_id: WorkspaceId,
        root: PathBuf,
    ) -> Result<WorkspaceHandle, Error> {
        let (tx, rx) = mpsc::channel(1024);
        let watcher = RecommendedWatcher::new(
            move |res: notify::Result<notify::Event>| {
                if let Ok(ev) = res {
                    for p in ev.paths {
                        let kind = match ev.kind {
                            notify::EventKind::Create(_) => FsEventKind::Created,
                            notify::EventKind::Modify(_) => FsEventKind::Modified,
                            notify::EventKind::Remove(_) => FsEventKind::Removed,
                            _ => continue,
                        };
                        // Lossy by design: the watcher callback must NEVER
                        // block the filesystem pipeline (a full channel would
                        // deadlock writes). Dropped events are recovered by
                        // index rescan (incremental indexing rebuilds from
                        // disk state).
                        let _ = tx.try_send(FsEvent {
                            workspace_id,
                            path: p,
                            kind,
                        });
                    }
                }
            },
            notify::Config::default(),
        )
        .map_err(|e| Error::internal(format!("watcher: {e}")))?;
        // The filesystem root can never be walked to completion: refuse the
        // recursive registration up front (explicit degradation) instead of
        // paying the deadline and leaking a walker thread over `/proc`,
        // `/sys`, mount points, and every other unbounded tree below it.
        let watcher = if root.parent().is_none() {
            tracing::warn!(
                workspace = workspace_id.raw(),
                root = %root.display(),
                "workspace root is the filesystem root: recursive watch registration is unbounded; opening WITHOUT a watcher (fingerprint reconciliation only)"
            );
            None
        } else if !watch_preflight_bounded(&root, self.watch_preflight_entries) {
            // P1-7: a root beyond the preflight bound is refused HERE, before
            // the uninterruptible recursive backend is ever asked to walk it:
            // one pathological workspace degrades itself and cannot starve
            // every later registration behind a wedged singleton worker.
            tracing::warn!(
                workspace = workspace_id.raw(),
                root = %root.display(),
                entries = self.watch_preflight_entries,
                "workspace exceeds the watcher preflight entry bound; opening WITHOUT a watcher (fingerprint reconciliation only)"
            );
            None
        } else {
            // P2-FS: registration runs through the bounded singleton worker
            // (never a thread per timed-out root).
            let registration =
                register_watch_bounded(watcher, root.clone(), self.watch_registration_deadline);
            match registration {
                WatchRegistrationOutcome::Registered(watcher) => Some(watcher),
                WatchRegistrationOutcome::Failed(reason) => {
                    return Err(Error::internal(format!(
                        "watch {}: {reason}",
                        root.display()
                    )));
                }
                WatchRegistrationOutcome::Degraded => {
                    tracing::warn!(
                        workspace = workspace_id.raw(),
                        root = %root.display(),
                        deadline_ms = self.watch_registration_deadline.as_millis() as u64,
                        "watcher registration exceeded its wall deadline; opening WITHOUT a watcher (fingerprint reconciliation only)"
                    );
                    None
                }
            }
        };
        let rooted = Arc::new(RootedDir::open(&root)?);
        Ok(WorkspaceHandle {
            workspace_id,
            root,
            rooted,
            _watcher: watcher.map(|w| Arc::new(Mutex::new(w))),
            events: Arc::new(Mutex::new(rx)),
        })
    }

    /// Idle unload (spec §21): drops the watcher and every cached root of
    /// this workspace id (real and shadow).
    pub fn close(&self, workspace_id: WorkspaceId) {
        recover_lock(&self.workspaces).retain(|(id, _), _| *id != workspace_id);
    }

    pub fn open_count(&self) -> usize {
        recover_lock(&self.workspaces).len()
    }

    /// The handle registered for `workspace_id`, if open (idle unload
    /// removes it). A READ-ONLY registry probe: unlike [`Self::open`] it
    /// never re-registers a workspace or creates a watcher, so callers that
    /// must act only on service-owned workspaces can refuse a foreign or
    /// unloaded handle without side effects.
    pub fn registered_handle(&self, workspace_id: WorkspaceId) -> Option<WorkspaceHandle> {
        recover_lock(&self.workspaces)
            .iter()
            .find(|((id, _), _)| *id == workspace_id)
            .map(|(_, handle)| handle.clone())
    }
}

#[derive(Clone)]
pub struct WorkspaceHandle {
    workspace_id: WorkspaceId,
    root: PathBuf,
    /// The workspace's anchored directory authority, opened ONCE at
    /// [`WorkspaceFileService::open`] and retained for the handle's whole
    /// lifetime. Every handle-relative traversal (enumeration, budgeted
    /// walks, deletes) runs through this handle instead of re-opening the
    /// root path per operation: a root directory entry swapped after the
    /// open cannot redirect those operations.
    rooted: Arc<RootedDir>,
    /// Retained filesystem-event watcher. `None` on scoped handles
    /// ([`WorkspaceHandle::open_scoped`]), which create no watcher and emit
    /// no events: their whole lifetime is one caller operation.
    _watcher: Option<Arc<Mutex<RecommendedWatcher>>>,
    events: Arc<Mutex<mpsc::Receiver<FsEvent>>>,
}

impl std::fmt::Debug for WorkspaceHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceHandle")
            .field("workspace_id", &self.workspace_id)
            .field("root", &self.root)
            .finish()
    }
}

/// The handle type returned by [`WorkspaceHandle::resolve_fd`]: an owned
/// unix fd, an owned Windows handle, or the uninhabited marker on platforms
/// with neither traversal primitive. On every supported platform the value
/// converts into a [`fs::File`].
#[cfg(unix)]
pub type ResolvedHandle = std::os::unix::io::OwnedFd;
#[cfg(windows)]
pub type ResolvedHandle = std::os::windows::io::OwnedHandle;
#[cfg(not(any(unix, windows)))]
pub type ResolvedHandle = UnavailableFd;

impl WorkspaceHandle {
    pub fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// True when a recursive filesystem watcher was registered at open. An
    /// explicit degraded open (filesystem root, or registration that exceeded
    /// its wall deadline) returns `false`: the handle is fully usable, but
    /// change detection relies on fingerprint reconciliation, never silently.
    pub fn watcher_attached(&self) -> bool {
        self._watcher.is_some()
    }

    /// Open a one-shot, watcher-less handle whose whole lifetime is ONE
    /// caller operation (recovery verification): every read runs through the
    /// same handle-relative anchored walk and post-open identity net as a
    /// service-owned handle, but no recursive watcher is created and the
    /// handle is never registered in a [`WorkspaceFileService`]. The root is
    /// canonicalized and validated exactly like [`WorkspaceFileService::open`]
    /// (missing/non-directory roots fail typed). No file is opened here.
    pub fn open_scoped(workspace_id: WorkspaceId, root: PathBuf) -> Result<Self, Error> {
        let root = root
            .canonicalize()
            .map_err(|e| Error::not_found(format!("workspace root {}: {e}", root.display())))?;
        if !root.is_dir() {
            return Err(Error::not_found(format!(
                "workspace root {} is not a directory",
                root.display()
            )));
        }
        let rooted = Arc::new(RootedDir::open(&root)?);
        // No producer: a scoped handle emits no filesystem events, and no
        // consumer outside its one caller can ever observe the channel.
        let (tx, rx) = mpsc::channel(1);
        drop(tx);
        Ok(Self {
            workspace_id,
            root,
            rooted,
            _watcher: None,
            events: Arc::new(Mutex::new(rx)),
        })
    }

    /// Traversal/symlink-safe resolution of a relative path under the root.
    pub fn resolve(&self, rel: &Path) -> Result<PathBuf, Error> {
        resolve_within(&self.root, rel)
    }

    /// Handle-relative, symlink/reparse-bounded resolution of `rel` (P0-49
    /// wave-11; audits 30/53/54 extended it to Windows).
    ///
    /// The returned handle is reached by a component-by-component walk
    /// anchored on the workspace root (unix: `openat(2)` directory fds, see
    /// `platform::unix`; Windows: `NtCreateFile` with
    /// `OBJECT_ATTRIBUTES.RootDirectory`, see `platform::windows`). The walk
    /// IS the resolution: no path string is re-resolved after it starts, so
    /// a hostile process swapping an intermediate directory for a
    /// symlink/reparse point cannot redirect the open. Symlinks and
    /// permitted reparse points (symlink + junction tags on Windows) are
    /// followed only by explicit, bounded re-anchoring (at most 8 hops per
    /// walk); targets that leave the workspace root are denied. `..`
    /// components, absolute paths outside the root and paths beyond 4096
    /// components are denied before any open. The final entry is opened
    /// without following (unix `O_NOFOLLOW`; Windows
    /// `FILE_OPEN_REPARSE_POINT`).
    ///
    /// Note: this low-level surface has no post-open identity net — the
    /// read/stat/hash methods add it. Use those unless the handle itself is
    /// the deliverable.
    #[cfg(any(unix, windows))]
    pub fn resolve_fd(&self, rel: &Path) -> Result<ResolvedHandle, Error> {
        platform::open_no_follow_walk(&self.root, rel, platform::OpenKind::Read)
    }

    /// Platforms with neither `openat(2)` nor a directory-relative NT open
    /// (wasm and friends): handle-relative walks are unsupported and this
    /// always fails typed; those platforms keep canonicalize-then-open with
    /// the post-open identity net.
    #[cfg(not(any(unix, windows)))]
    pub fn resolve_fd(&self, _rel: &Path) -> Result<ResolvedHandle, Error> {
        Err(Error::new(
            ErrorKind::Internal,
            format!(
                "resolve_fd is unsupported on {}: no directory-relative open",
                std::env::consts::OS
            ),
        ))
    }

    /// Read bounded by max_bytes (default 4MB); the digest says whether the
    /// returned bytes cover the whole file ([`ContentDigest::Full`]) or only
    /// a bounded prefix ([`ContentDigest::Slice`]). FileData exposes no
    /// `hash`/`truncated` pair: use [`FileData::full_hash`] for whole-file
    /// identity and handle the `Slice` case (oversized) explicitly.
    pub fn read(&self, rel: &Path, max_bytes: usize) -> Result<FileData, Error> {
        let (path, mut f) = self.open_resolved(rel)?;
        let (bytes, digest) = read_open_bounded(&mut f, rel, max_bytes)?;
        let size = bytes.len();
        Ok(FileData {
            path,
            bytes,
            size,
            digest,
        })
    }

    pub fn read_default(&self, rel: &Path) -> Result<FileData, Error> {
        self.read(rel, DEFAULT_READ_MAX)
    }

    /// Bounded read whose digest says what it covers: the whole file
    /// ([`ContentDigest::Full`]) or a capped prefix
    /// ([`ContentDigest::Slice`]). Snapshot probes and indexers use this so
    /// a truncated hash can never masquerade as the file's identity.
    pub fn read_hashed(
        &self,
        rel: &Path,
        max_bytes: usize,
    ) -> Result<(Vec<u8>, ContentDigest), Error> {
        let (_path, mut f) = self.open_resolved(rel)?;
        read_open_bounded(&mut f, rel, max_bytes)
    }

    /// Open `rel` for reading: the handle-relative anchored walk is the
    /// resolution on unix and Windows (never a pathname re-resolve);
    /// platforms without that primitive use canonicalize-then-open. Returns
    /// the canonical path string (for reporting) and the open file. The
    /// post-open identity net runs AFTER the open on every platform: a
    /// directory entry that was swapped — including an intermediate
    /// directory swapped for a symlink/reparse point after the walk passed
    /// it — is caught here and rejected loudly.
    #[cfg(any(unix, windows))]
    fn open_resolved(&self, rel: &Path) -> Result<(PathBuf, fs::File), Error> {
        let path = self.resolve(rel)?;
        let handle = platform::open_no_follow_walk(&self.root, rel, platform::OpenKind::Read)?;
        let f = fs::File::from(handle);
        read_race_seam(rel);
        if !opened_is_path(&f, &path) {
            return Err(Error::permission(format!(
                "{rel:?} changed identity between resolution and open (TOCTOU)"
            )));
        }
        Ok((path, f))
    }

    #[cfg(not(any(unix, windows)))]
    fn open_resolved(&self, rel: &Path) -> Result<(PathBuf, fs::File), Error> {
        let path = self.resolve(rel)?;
        let f = fs::File::open(&path).map_err(|e| err_not_found(rel, e))?;
        read_race_seam(rel);
        if !opened_is_path(&f, &path) {
            return Err(Error::permission(format!(
                "{rel:?} changed identity between resolution and open (TOCTOU)"
            )));
        }
        Ok((path, f))
    }

    /// Stream-hash a file through a bounded 64 KiB buffer — the file is
    /// NEVER materialized in RAM (audit 49). Returns the number of bytes
    /// actually hashed and the BLAKE3 hash of those bytes.
    ///
    /// - `max_bytes: None` hashes the whole file (files of any size, but
    ///   streamingly — this is the snapshot probe path).
    /// - `max_bytes: Some(n)` caps the read at `min(file size, n)`; the
    ///   returned hash then covers only that prefix — callers that need
    ///   whole-file identity must pass `None` or check the byte count.
    pub fn hash_file_streaming(
        &self,
        rel: &Path,
        max_bytes: Option<u64>,
    ) -> Result<(u64, FileHash), Error> {
        let (_path, mut f) = self.open_resolved(rel)?;
        hash_open_bounded(&mut f, rel, max_bytes)
    }

    /// Slice read for paging big files (spec §23). The digest is always a
    /// [`ContentDigest::Slice`] over `[offset, offset+read)`: paging is
    /// partial by construction, so the returned hash NEVER claims whole-file
    /// identity ([`FileData::full_hash`] is always `None` here). End-of-file
    /// is `bytes.len() < len` (an empty `bytes` means `offset` is past EOF).
    pub fn read_slice(&self, rel: &Path, offset: u64, len: usize) -> Result<FileData, Error> {
        use std::io::{Read, Seek, SeekFrom};
        let (path, mut f) = self.open_resolved(rel)?;
        f.seek(SeekFrom::Start(offset))
            .map_err(|e| Error::internal(format!("seek {rel:?}: {e}")))?;
        let mut bytes = vec![0u8; len];
        let mut read = 0usize;
        while read < len {
            let n = f
                .read(&mut bytes[read..])
                .map_err(|e| Error::internal(format!("read {rel:?}: {e}")))?;
            if n == 0 {
                break;
            }
            read += n;
        }
        bytes.truncate(read);
        let hash = FileHash::from(blake3::hash(&bytes).into());
        let size = bytes.len();
        Ok(FileData {
            path,
            bytes,
            size,
            digest: ContentDigest::Slice {
                hash,
                offset,
                len: read as u64,
            },
        })
    }

    /// Atomic write: temp file in the same dir + fsync + rename + parent-dir
    /// fsync on unix. Delegates to the shared durable helper so every writer
    /// in the workspace follows the identical crash-safe sequence (audit
    /// 45/75).
    ///
    /// The rename still operates on the resolved path STRING (neither POSIX
    /// nor Win32 offers a rename-by-fd or a compare-and-swap rename), so
    /// immediately before the rename the destination's parent directory is
    /// re-verified with the handle-relative walk
    /// ([`Self::verify_parent_before_rename`]): a parent chain swapped to a
    /// symlink/reparse point outside the workspace since resolution fails
    /// the write instead of redirecting it. The window between that
    /// verification and the rename syscall is the single residual
    /// rename-by-path exposure (documented honest limit).
    pub fn write_atomic(&self, rel: &Path, bytes: &[u8]) -> Result<FileHash, Error> {
        let path = self.resolve(rel)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| Error::internal(format!("mkdir {}: {e}", parent.display())))?;
        }
        atomic::atomic_replace_guarded(&path, bytes, &self.verify_parent_before_rename())
    }

    /// Commit-time compare-and-swap write (audit 46): the destination's
    /// CURRENT content digest must equal `expected_hash` (the hash of what
    /// the caller read and validated) at the moment of replacement — a file
    /// that changed since the caller's read is never clobbered. The digest
    /// recheck happens immediately before the rename, under the shared
    /// per-path mutation lock, so cooperative writers serialize. The parent
    /// directory is fd-re-verified immediately before the rename as well
    /// (see [`Self::write_atomic`]).
    pub fn write_atomic_cas(
        &self,
        rel: &Path,
        expected_hash: FileHash,
        bytes: &[u8],
    ) -> Result<FileHash, Error> {
        let path = self.resolve(rel)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| Error::internal(format!("mkdir {}: {e}", parent.display())))?;
        }
        let expected = match atomic::FileState::now(&path)? {
            st if !st.exists => {
                return Err(Error::conflict(format!(
                    "{} disappeared before the commit-time check",
                    rel.display()
                )))
            }
            st => st,
        };
        let mut expected = expected;
        expected.digest = Some(expected_hash);
        atomic::atomic_replace_cas_guarded(
            &path,
            &expected,
            bytes,
            &self.verify_parent_before_rename(),
        )
    }

    /// Parent-directory verification closure for the guarded atomic writers
    /// (P0-49): re-walk the destination's parent with the handle-relative
    /// walk and require that every component is a genuine directory inside
    /// the workspace. A parent chain that became a symlink/reparse point —
    /// in particular one pointing outside the root — fails loudly, so the
    /// subsequent rename cannot be redirected outside the workspace.
    #[cfg(any(unix, windows))]
    fn verify_parent_before_rename(&self) -> impl Fn(&Path) -> Result<(), Error> + '_ {
        let root = &self.root;
        move |dest: &Path| {
            let parent = dest.parent().ok_or_else(|| {
                Error::malformed(format!("{} has no parent directory", dest.display()))
            })?;
            let parent_rel = parent.strip_prefix(root.as_path()).map_err(|_| {
                Error::permission(format!(
                    "write destination {} is outside the workspace root",
                    dest.display()
                ))
            })?;
            let _dir =
                platform::open_no_follow_walk(root, parent_rel, platform::OpenKind::Directory)?;
            Ok(())
        }
    }

    /// Platforms without a handle-relative walk: no parent re-verification
    /// before the rename; the CAS digest recheck remains the write-time
    /// guard there.
    #[cfg(not(any(unix, windows)))]
    fn verify_parent_before_rename(&self) -> impl Fn(&Path) -> Result<(), Error> + '_ {
        move |_dest: &Path| Ok(())
    }

    /// Free-function twin of the handle's `verify_parent_before_rename` for
    /// the CAS merge writers: immediately before the mutation the
    /// destination's parent chain is re-walked handle-relative under the
    /// canonical `root`, so a parent swapped for an outside symlink/reparse
    /// point since resolution fails the write typed instead of redirecting
    /// rename/link/unlink/chmod out of the workspace. The check-to-syscall
    /// gap is the documented rename-by-path residual (POSIX has no
    /// compare-and-swap rename).
    #[cfg(any(unix, windows))]
    pub(crate) fn verify_parent_under_root(
        root: &Path,
    ) -> impl Fn(&Path) -> Result<(), Error> + '_ {
        move |dest: &Path| {
            let parent = dest.parent().ok_or_else(|| {
                Error::malformed(format!("{} has no parent directory", dest.display()))
            })?;
            let parent_rel = parent.strip_prefix(root).map_err(|_| {
                Error::permission(format!(
                    "write destination {} is outside the workspace root",
                    dest.display()
                ))
            })?;
            platform::open_no_follow_walk(root, parent_rel, platform::OpenKind::Directory)?;
            Ok(())
        }
    }

    /// Platforms without a handle-relative walk keep the digest/CAS recheck
    /// as the only write-time guard.
    #[cfg(not(any(unix, windows)))]
    fn verify_parent_under_root(_root: &Path) -> impl Fn(&Path) -> Result<(), Error> + '_ {
        move |_dest: &Path| Ok(())
    }

    /// stat via the resolved handle (unix: fstat of the walked entry;
    /// Windows: `GetFileInformationByHandle`-backed metadata of the walked
    /// HANDLE — the file is never re-opened by path; the walk is the
    /// resolution). On platforms without a handle walk the
    /// canonicalize-then-stat flow is used.
    /// Note the honest trade: an entry that the process cannot open
    /// (no read permission) cannot be stat()ed through this API anymore,
    /// because a metadata-only open does not exist in the handle walk.
    pub fn stat(&self, rel: &Path) -> Result<FileMeta, Error> {
        let path = self.resolve(rel)?;
        #[cfg(any(unix, windows))]
        let meta = {
            let handle = platform::open_no_follow_walk(&self.root, rel, platform::OpenKind::Read)?;
            fs::File::from(handle)
                .metadata()
                .map_err(|e| Error::internal(format!("{}: {e}", rel.display())))?
        };
        #[cfg(not(any(unix, windows)))]
        let meta = fs::metadata(&path).map_err(|e| err_not_found(rel, e))?;
        let modified_ms = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        Ok(FileMeta {
            path,
            size: meta.len(),
            modified_ms,
        })
    }

    pub fn exists(&self, rel: &Path) -> bool {
        self.resolve(rel).map(|p| p.exists()).unwrap_or(false)
    }

    /// Legacy listing API: handle-relative now (the rooted-directory
    /// authority enumerates an open directory handle; no path is reopened),
    /// silently capped at `max_entries` for compatibility. Callers that must
    /// know about truncation use [`Self::list_entries`].
    pub fn list(&self, rel: &Path, max_entries: usize) -> Result<Vec<FileMeta>, Error> {
        let listing = self.list_entries(rel, max_entries)?;
        Ok(listing
            .entries
            .into_iter()
            .map(|entry| FileMeta {
                path: self.root.join(&entry.rel),
                size: entry.size,
                modified_ms: entry.modified_ms,
            })
            .collect())
    }

    /// Handle-relative enumeration through the workspace's RETAINED rooted
    /// authority (P0-52): the directory is reached by an `openat`/handle
    /// walk from the fd anchored at open time and its children are inspected
    /// relative to the open handle — never a resolve-then-`read_dir` by
    /// pathname. `truncated` reports that the directory held more than
    /// `max` entries.
    pub fn list_entries(&self, rel: &Path, max: usize) -> Result<crate::rooted::DirListing, Error> {
        self.rooted.list_entries(rel, max)
    }

    /// Budgeted handle-relative recursive walk (the shared traversal
    /// primitive for upper-layer walkers), anchored on the RETAINED rooted
    /// authority: every entry is charged to `budget`; the visitor may charge
    /// reads and stop early.
    pub fn walk_bounded<F>(
        &self,
        rel: &Path,
        budget: &mut crate::rooted::WalkBudget,
        skip_dirs: &[&str],
        visit: &mut F,
    ) -> Result<(), Error>
    where
        F: FnMut(
            &crate::rooted::RootedEntry,
            usize,
            &mut crate::rooted::WalkBudget,
        ) -> Result<crate::rooted::WalkStep, Error>,
    {
        self.rooted.walk_bounded(rel, budget, skip_dirs, visit)
    }

    /// Remove one workspace-relative file entry through the RETAINED rooted
    /// authority: the parent chain and the final entry are `openat`/
    /// unlinkat-walked from the fd anchored at open time, with strict
    /// no-follow semantics, so a directory swapped for a symlink/reparse
    /// point after any earlier resolution can never redirect the unlink
    /// outside the workspace. A missing entry is `Ok(())` (the goal state
    /// of a delete); a directory, link or out-of-root entry is a typed
    /// refusal.
    pub fn remove_file(&self, rel: &Path) -> Result<(), Error> {
        self.rooted.remove_file(rel)
    }

    pub fn events(&self) -> &Mutex<mpsc::Receiver<FsEvent>> {
        &self.events
    }

    /// Identity check helper: every tool call carries its identity; this
    /// verifies the workspace matches this handle (no cross-workspace use).
    pub fn verify_identity(&self, identity: &WorkspaceIdentity) -> Result<(), Error> {
        if identity.workspace_id != self.workspace_id {
            return Err(Error::permission(format!(
                "workspace identity mismatch: handle {}, identity {}",
                self.workspace_id, identity.workspace_id
            )));
        }
        Ok(())
    }
}

/// Stream-hash an ALREADY-OPEN file (the fd walk already ran; the cap is
/// decided from the fstat size at open time) through a bounded 64 KiB
/// buffer — the file is NEVER materialized in RAM.
fn hash_open_bounded(
    f: &mut fs::File,
    rel: &Path,
    max_bytes: Option<u64>,
) -> Result<(u64, FileHash), Error> {
    use std::io::Read;
    let meta = f
        .metadata()
        .map_err(|e| Error::internal(format!("stat {rel:?}: {e}")))?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    let mut remaining = match max_bytes {
        Some(max) => max.min(meta.len()),
        None => meta.len(),
    };
    let mut hashed = 0u64;
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        let n = f
            .read(&mut buf[..want])
            .map_err(|e| Error::internal(format!("read {rel:?}: {e}")))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        hashed += n as u64;
        remaining -= n as u64;
    }
    Ok((hashed, FileHash::from(hasher.finalize().into())))
}
