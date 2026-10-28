// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! faktor-fs — file service and watcher (spec §21 resource scopes).
//!
//! Every call carries its explicit workspace identity; paths are relative to
//! the workspace root and resolved traversal/symlink-safely. Reads are
//! bounded; writes are atomic (temp + fsync + rename). Workspaces own their
//! watcher; `close` unloads heavyweight resources after inactivity.

use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};

use faktor_core::error::{Error, ErrorKind};
use faktor_core::hash::FileHash;
use faktor_core::id::WorkspaceId;
use faktor_core::WorkspaceIdentity;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

pub mod atomic;
pub mod entry_state;
pub mod legacy;
mod platform;
pub mod rooted;
mod workspace_service;
pub use workspace_service::{WorkspaceFileService, WorkspaceHandle};
pub mod tree_manifest;

pub use legacy::{legacy_relative_path_within, workspace_relative_path_rejection};
pub use rooted::RootedDir;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsEventKind {
    Created,
    Modified,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsEvent {
    pub workspace_id: WorkspaceId,
    pub path: PathBuf,
    pub kind: FsEventKind,
}

#[derive(Debug, Clone)]
pub struct FileData {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
    pub size: usize,
    /// What the returned bytes' digest covers: the WHOLE file ([`ContentDigest::Full`])
    /// or only the bytes actually read ([`ContentDigest::Slice`]). The struct
    /// exposes no bare `hash`/`truncated` pair anymore (P0-50): a caller that
    /// needs whole-file identity must go through [`FileData::full_hash`] and
    /// handle the `Slice` case explicitly — a prefix hash can never silently
    /// masquerade as the file's identity.
    pub digest: ContentDigest,
}

impl FileData {
    /// The whole-file hash when this read covered the ENTIRE file content;
    /// `None` when the read was bounded and the digest only covers a
    /// prefix/slice — such a hash must never be compared against a
    /// whole-file identity (optimistic-concurrency expectations, CAS
    /// preimages, snapshots).
    pub fn full_hash(&self) -> Option<FileHash> {
        match self.digest {
            ContentDigest::Full(h) => Some(h),
            ContentDigest::Slice { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMeta {
    pub path: PathBuf,
    pub size: u64,
    pub modified_ms: i64,
}

const DEFAULT_READ_MAX: usize = 4 * 1024 * 1024;

/// Wall deadline for recursive watcher registration. `notify` walks the whole
/// tree inside its event-loop thread (one `inotify_add_watch` per directory on
/// Linux), so a pathological root — the filesystem root, a tree with hundreds
/// of thousands of directories — makes registration unbounded and wedges EVERY
/// later watcher message (and the caller that is attaching the workspace).
/// Registration therefore runs on a helper thread and `open` proceeds with an
/// explicit, loud degradation (no watcher: fingerprint reconciliation only)
/// once this deadline passes.
const DEFAULT_WATCH_REGISTRATION_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// Run `f` on a named helper thread and wait at most `deadline` for its
/// result. `None` means the deadline elapsed: the helper keeps running and its
/// result is discarded when it returns (callers must treat that as an
/// explicit degraded outcome, never as success). Kept for the deadline
/// semantics test; production watcher registration uses the bounded
/// singleton worker in `workspace_service`.
#[cfg(test)]
fn bounded_join<T: Send + 'static>(
    name: &str,
    deadline: std::time::Duration,
    f: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new()
        .name(name.to_string())
        .spawn(move || {
            let _ = tx.send(f());
        });
    if spawned.is_err() {
        return None;
    }
    rx.recv_timeout(deadline).ok()
}

/// Identity of a read: whether the digest covers the WHOLE file or only a
/// bounded prefix/slice (audit 48). A truncated hash must never be compared
/// with a whole-file identity: [`ContentDigest::Full`] values may be matched
/// against stored file hashes (e.g. snapshot `FileState`), while a
/// [`ContentDigest::Slice`] proves the bytes were cut short — the type
/// separation makes mistaking one for the other impossible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentDigest {
    /// Hash of the file's entire content at read time.
    Full(FileHash),
    /// Hash of the bytes in `[offset, offset+len)` of the file. Produced by
    /// bounded reads that hit the cap (the file has MORE content beyond
    /// `offset+len`) or by paging slice reads — any read whose hash does NOT
    /// prove whole-file coverage. A `Slice` must never be compared with a
    /// whole-file identity.
    Slice {
        hash: FileHash,
        offset: u64,
        len: u64,
    },
}

impl ContentDigest {
    pub fn hash(self) -> FileHash {
        match self {
            ContentDigest::Full(h) => h,
            ContentDigest::Slice { hash, .. } => hash,
        }
    }

    /// True when the digest proves it covers the entire file content.
    pub fn is_full(self) -> bool {
        matches!(self, ContentDigest::Full(_))
    }
}

/// Classified lock recovery for the workspace registry: the map is a
/// DERIVED cache of live watcher handles. A poisoned guard is recovered with
/// the poison flag cleared (reconciled against the durable workspace rows)
/// so one panicking caller can never wedge workspace resolution.
fn recover_lock<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(|poisoned| {
        lock.clear_poison();
        poisoned.into_inner()
    })
}

fn err_not_found(rel: &Path, e: std::io::Error) -> Error {
    if e.kind() == std::io::ErrorKind::NotFound {
        Error::not_found(format!("{}", rel.display()))
    } else {
        Error::new(ErrorKind::Internal, format!("{}: {e}", rel.display()))
    }
}

/// The shared bounded read over an ALREADY-OPEN file (the fd walk already
/// ran). The cap is ENFORCED by the read itself: at most `max_bytes + 1`
/// bytes are ever pulled into memory, so a concurrently growing file can
/// never outgrow the advertised bound. `fstat` only sizes the allocation;
/// the digest is `Full` only when the whole file was actually read within
/// the cap, `Slice` otherwise (exactly `max_bytes`, offset 0).
pub(crate) fn read_open_bounded(
    f: &mut fs::File,
    rel: &Path,
    max_bytes: usize,
) -> Result<(Vec<u8>, ContentDigest), Error> {
    use std::io::Read;
    let cap = max_bytes.saturating_add(1);
    let hint = f
        .metadata()
        .map_err(|e| Error::internal(format!("stat {rel:?}: {e}")))?
        .len()
        .min(cap as u64);
    let mut bytes = Vec::with_capacity(hint as usize);
    f.by_ref()
        .take(cap as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| Error::internal(format!("read {rel:?}: {e}")))?;
    let digest = if bytes.len() > max_bytes {
        bytes.truncate(max_bytes);
        ContentDigest::Slice {
            hash: FileHash::from(blake3::hash(&bytes).into()),
            offset: 0,
            len: bytes.len() as u64,
        }
    } else {
        ContentDigest::Full(FileHash::from(blake3::hash(&bytes).into()))
    };
    Ok((bytes, digest))
}

/// The Ok-payload type of [`WorkspaceHandle::resolve_fd`] on platforms
/// without `openat(2)` or a directory-relative NT open (wasm and friends):
/// an uninhabited marker proving the API can never succeed there.
#[cfg(not(any(unix, windows)))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnavailableFd(());

/// Open-after-resolve identity check (audit 47): resolution and open are two
/// syscalls; a swap between them redirects the open to a different file.
/// After opening we re-open the path WITHOUT following the final component
/// and compare identity — unix `(dev, ino)`, Windows (volume serial, 128-bit
/// file id). A swap — including a swap to a symlink/reparse point — or an
/// intermediate directory swapped after the walk passed it changes the
/// identity and is rejected loudly. On platforms without stable identity
/// metadata this is skipped (documented honest limit).
#[cfg(unix)]
fn opened_is_path(f: &fs::File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (f.metadata(), fs::symlink_metadata(path)) {
        (Ok(open), Ok(at_path)) => open.dev() == at_path.dev() && open.ino() == at_path.ino(),
        _ => false,
    }
}

#[cfg(windows)]
fn opened_is_path(f: &fs::File, path: &Path) -> bool {
    platform::opened_is_path(f, path)
}

#[cfg(not(any(unix, windows)))]
fn opened_is_path(_f: &fs::File, _path: &Path) -> bool {
    true
}

/// Test seam: called between the final open and the post-open identity check
/// in the read paths. Deterministic TOCTOU tests swap the target file (or
/// replace it with a symlink) inside this window.
#[cfg(test)]
type ReadSeam = Box<dyn Fn(&Path) + Send>;
#[cfg(test)]
static READ_RACE_SEAM: OnceLock<std::sync::Mutex<Option<ReadSeam>>> = OnceLock::new();
#[cfg(test)]
fn read_race_seam(rel: &Path) {
    if let Some(lock) = READ_RACE_SEAM.get() {
        if let Some(hook) = lock.lock().expect("seam poisoned").as_ref() {
            hook(rel);
        }
    }
}
#[cfg(not(test))]
fn read_race_seam(_rel: &Path) {}

fn resolve_within(root: &Path, path: &Path) -> Result<PathBuf, Error> {
    // Windows path hazards (extended/device prefixes, UNC escapes,
    // drive-relative forms, `..`, alternate data streams, NULs) are denied
    // BEFORE any canonicalization/open, so `resolve`-based writers cannot
    // be steered onto a stream or a namespace bypass either.
    //
    // The handle-relative walk IS the resolution on Windows
    // (`platform::windows::canonicalize_within`). `std::fs::canonicalize`
    // is unusable there: it fails for a path whose tail does not exist yet
    // (the atomic writers create the destination's parent directories only
    // AFTER resolving it), and it cannot follow a relative symlink whose
    // stored substitute name contains forward slashes — exactly how
    // `std::os::windows::fs::symlink_file` stores a relative target. The
    // walk reads substitute names itself, normalizes both separators,
    // follows only in-root reparse targets, and appends not-yet-existing
    // lexically validated components to the deepest existing ancestor.
    #[cfg(windows)]
    {
        platform::lexical_check(root, path)?;
        platform::canonicalize_within(root, path)
    }
    #[cfg(not(windows))]
    {
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.join(path)
        };
        for component in joined.components() {
            if let Component::ParentDir = component {
                return Err(Error::permission(format!(
                    "path traversal rejected: {path:?}"
                )));
            }
        }
        if let Ok(canon) = joined.canonicalize() {
            if canon.starts_with(root) {
                return Ok(canon);
            }
            return Err(Error::permission(format!(
                "path escapes workspace: {path:?}"
            )));
        }
        if let Ok(meta) = fs::symlink_metadata(&joined) {
            if meta.file_type().is_symlink() {
                return Err(Error::permission(format!(
                    "symlink escape rejected: {path:?}"
                )));
            }
        }
        let parent = joined
            .parent()
            .ok_or_else(|| Error::malformed("path has no parent"))?;
        let file_name = joined
            .file_name()
            .ok_or_else(|| Error::malformed("path has no file name"))?;
        let canon_parent = parent
            .canonicalize()
            .map_err(|_| Error::permission(format!("parent resolution failed: {path:?}")))?;
        let resolved = canon_parent.join(file_name);
        if resolved.starts_with(root) {
            Ok(resolved)
        } else {
            Err(Error::permission(format!(
                "path escapes workspace: {path:?}"
            )))
        }
    }
}

// ======================================================================
// Wave-13 controlled-merge primitives (audits 70/98/99) — additive.
//
// Bounded tree snapshot/copy + commit-time CAS merge applies over RAW
// paths (roots are canonicalized; every relative path is resolved with
// the same traversal/symlink discipline as [`WorkspaceHandle::resolve`]).
// Nothing is ever skipped silently: an un-copyable or unhashable file
// fails the whole operation loudly.
// ======================================================================

/// Hard depth bound of the bounded tree walks below (a hostile tree that
/// nests deeper fails loudly — never an unbounded recursion).
pub const MAX_TREE_WALK_DEPTH: usize = 256;
/// Per-file cap of one merge content apply. Merging streams the child's
/// file content through the wave-10 commit-time CAS helpers, which take the
/// whole payload; files beyond this bound fail with a typed [`ErrorKind::Oversized`]
/// error — the merge NEVER silently truncates a file.
pub const MAX_MERGE_FILE_BYTES: u64 = 256 * 1024 * 1024;

/// One file of a bounded tree snapshot: relative path, whole-content hash
/// and byte size. The hash always covers the WHOLE file (streamed, never
/// materialized) — a snapshot entry can never masquerade as a partial read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotEntry {
    pub path: PathBuf,
    pub hash: FileHash,
    pub size: u64,
}

/// Outcome of one CAS merge operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CasMergeResult {
    /// The file content/absence was written now.
    Applied,
    /// The destination already holds the merge's end state (an earlier,
    /// crashed run applied it — or the state converged); treated as merged.
    AlreadyCurrent,
}

/// Bounded snapshot of every file under `root` as (relative path -> hash).
/// The walk is traversal-safe (symlinks must resolve inside `root`), reads
/// are whole-file streaming (bounded RAM), never skips an unreadable file
/// and refuses trees beyond `max_entries` with a typed Oversized error.
/// Sorted by relative path (deterministic replay order).
pub fn snapshot_tree(root: &Path, max_entries: usize) -> Result<Vec<SnapshotEntry>, Error> {
    if max_entries == 0 {
        return Err(Error::malformed("snapshot max_entries must be >= 1"));
    }
    let root = root
        .canonicalize()
        .map_err(|e| Error::not_found(format!("snapshot root {}: {e}", root.display())))?;
    if !root.is_dir() {
        return Err(Error::not_found(format!(
            "snapshot root {} is not a directory",
            root.display()
        )));
    }
    let mut out = Vec::new();
    let mut count = 0usize;
    walk_files(
        &root,
        &root,
        Path::new(""),
        0,
        max_entries,
        &mut count,
        &mut |rel, abs, f| {
            let size = f
                .metadata()
                .map_err(|e| Error::internal(format!("metadata {}: {e}", abs.display())))?
                .len();
            let (_, hash) = hash_open_file(f)?;
            out.push(SnapshotEntry {
                path: rel.to_path_buf(),
                hash,
                size,
            });
            Ok(())
        },
    )?;
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Bounded copy of every file under `src_root` into `dst_root` (which must
/// already exist as a directory, distinct from `src_root`). Each file is
/// copied with the durable atomic sequence (unique temp + fsync + rename +
/// parent fsync), so a crash leaves whole files, never torn ones. Every
/// source file is opened exactly once and read whole; an atomic-rename
/// writer on the source therefore yields either the pre- or the post-state
/// of each file — never a mix. Errors fail the WHOLE copy loudly (nothing
/// is skipped); caps (entries / total bytes) are typed Oversized errors.
/// Returns the copied manifest (relative path, hash, size), sorted.
pub fn copy_tree(
    src_root: &Path,
    dst_root: &Path,
    max_entries: usize,
    max_total_bytes: u64,
) -> Result<Vec<SnapshotEntry>, Error> {
    copy_tree_skip(src_root, dst_root, max_entries, max_total_bytes, &[])
}

/// [`copy_tree`] with named directories skipped entirely: directory entries
/// whose file name is listed in `skip_dirs` (at ANY depth) are never walked
/// and never copied — their whole subtree is absent from the copy and from
/// the returned manifest. Used for daemon-owned shadow copies of user
/// checkouts, where `.git` plumbing is bookkeeping, never content. Nothing
/// else differs from [`copy_tree`]: caps are typed Oversized errors, every
/// file is copied with the durable atomic sequence, and a mid-copy source
/// drift fails loudly.
pub fn copy_tree_skip(
    src_root: &Path,
    dst_root: &Path,
    max_entries: usize,
    max_total_bytes: u64,
    skip_dirs: &[&str],
) -> Result<Vec<SnapshotEntry>, Error> {
    if max_entries == 0 || max_total_bytes == 0 {
        return Err(Error::malformed("copy caps must be >= 1"));
    }
    let src = src_root
        .canonicalize()
        .map_err(|e| Error::not_found(format!("copy source {}: {e}", src_root.display())))?;
    let dst = dst_root
        .canonicalize()
        .map_err(|e| Error::not_found(format!("copy destination {}: {e}", dst_root.display())))?;
    if !src.is_dir() || !dst.is_dir() {
        return Err(Error::not_found(
            "copy source and destination must be directories",
        ));
    }
    if src == dst {
        return Err(Error::conflict(
            "copy source and destination are the same tree",
        ));
    }
    for name in skip_dirs {
        if name.is_empty()
            || name.contains('/')
            || name.contains('\\')
            || *name == "."
            || *name == ".."
        {
            return Err(Error::malformed(format!(
                "skip directory name {name:?} must be a bare file name"
            )));
        }
    }
    let mut out = Vec::new();
    let mut count = 0usize;
    let mut total = 0u64;
    let out_ref = &mut out;
    let total_ref = &mut total;
    walk_files_skip(
        &src,
        &src,
        Path::new(""),
        0,
        max_entries,
        &mut count,
        &mut |rel, abs, f| {
            // Internal atomic-write temporaries (a concurrent CAS writer's
            // in-flight `.faktor-tmp-*` files, or legacy `.kp-tmp-*` crash
            // residue) are never part of a snapshot: materialized roots must
            // contain exactly the manifest entries.
            let fname = rel
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if atomic::is_internal_temp_name(&fname) {
                return Ok(());
            }
            let meta = f
                .metadata()
                .map_err(|e| Error::internal(format!("metadata {}: {e}", abs.display())))?;
            let size = meta.len();
            if total_ref.saturating_add(size) > max_total_bytes {
                return Err(Error::oversized(format!(
                    "copy of {abs:?} would exceed the {max_total_bytes}-byte total bound"
                )));
            }
            *total_ref += size;
            let target = dst.join(rel);
            let parent = target
                .parent()
                .ok_or_else(|| Error::malformed(format!("copy target {target:?} has no parent")))?;
            fs::create_dir_all(parent)
                .map_err(|e| Error::internal(format!("mkdir {}: {e}", parent.display())))?;
            let (n, hash) = copy_open_file(f, &target, Some(meta.permissions()))?;
            if n != size {
                // The file changed size mid-copy (an in-place, non-atomic
                // writer): loud — never a silently torn copy.
                return Err(Error::conflict(format!(
                    "{rel:?} changed size while being copied ({n} of {size} bytes)"
                )));
            }
            out_ref.push(SnapshotEntry {
                path: rel.to_path_buf(),
                hash,
                size,
            });
            Ok(())
        },
        skip_dirs,
    )?;
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Apply ONE approved merge: overwrite `dst_root/dst_rel` with the whole
/// content of `src_root/src_rel`, guarded by commit-time CAS on BOTH sides.
///
/// - `expected_src_hash`: the staged digest of the source (the change-set
///   `child_hash`). If the source drifted since staging, the apply fails
///   with a Conflict — a changed candidate is never merged.
/// - `expected_dst`: the base-snapshot hash of the destination path
///   (`None` = the parent had no such file at the base snapshot, so the
///   destination must not exist; the write is an exclusive create).
///   If the destination moved on since the base snapshot, the apply fails
///   with a Conflict and the destination stays byte-identical — never
///   overwritten.
///
/// Replay semantics: when the destination already equals the source digest
/// (a crashed run applied it, or the state converged), the apply is
/// [`CasMergeResult::AlreadyCurrent`] — idempotent, never an error. The
/// destination write goes through the guarded
/// `atomic::atomic_replace_cas_guarded` discipline (a create is a
/// CAS-ABSENT publish) with the parent chain re-verified handle-relative
/// immediately before the rename, so a parent swapped for an outside
/// symlink since resolution refuses the write typed; a crash at any point
/// leaves either the old or the new whole file.
pub fn merge_apply_content(
    dst_root: &Path,
    dst_rel: &Path,
    src_root: &Path,
    src_rel: &Path,
    expected_src_hash: FileHash,
    expected_dst: Option<FileHash>,
) -> Result<CasMergeResult, Error> {
    let dst_root = dst_root.canonicalize().map_err(|e| {
        Error::not_found(format!(
            "merge destination root {}: {e}",
            dst_root.display()
        ))
    })?;
    let src_root = src_root
        .canonicalize()
        .map_err(|e| Error::not_found(format!("merge source root {}: {e}", src_root.display())))?;
    // An EXCLUSIVE create may need to materialize parent directories that
    // the parent tree never had (the child created the file inside its own
    // tree); every intermediate directory is created and verified inside
    // the canonical root — a symlink escape fails the file loudly. For an
    // existing-file apply the parent directory must already exist (its
    // disappearance is a per-file conflict, surfaced below).
    let dst = if expected_dst.is_none() {
        resolve_or_create_within(&dst_root, dst_rel)?
    } else {
        resolve_within(&dst_root, dst_rel)?
    };
    let src = resolve_within(&src_root, src_rel)?;
    // Classify the destination BEFORE reading the source (cheap reject).
    let dst_state = atomic::FileState::now_with_digest(&dst).map_err(|e| {
        Error::new(
            e.kind,
            format!("destination recheck {}: {}", dst.display(), e.message),
        )
    })?;
    match (dst_state.exists, expected_dst, dst_state.digest) {
        (false, Some(_), _) => {
            return Err(Error::conflict(format!(
                "{} vanished after the base snapshot; refusing to recreate it from a stale patch",
                dst.display()
            )));
        }
        (true, None, Some(cur)) if cur == expected_src_hash => {
            // The destination already holds exactly the child's content: a
            // crashed earlier apply (or convergent state) — idempotent.
            return Ok(CasMergeResult::AlreadyCurrent);
        }
        (true, None, _) => {
            return Err(Error::conflict(format!(
                "{} exists although the base snapshot had no such file (parent-side creation conflicts with the merge)",
                dst.display()
            )));
        }
        (true, Some(_base), Some(cur)) if cur == expected_src_hash => {
            // The destination already holds exactly the child's content: a
            // crashed earlier apply (or convergent state) — idempotent.
            return Ok(CasMergeResult::AlreadyCurrent);
        }
        (true, Some(base), Some(cur)) if cur != base => {
            return Err(Error::conflict(format!(
                "{} changed since the base snapshot (expected {}, found {}); the parent file was not touched",
                dst.display(),
                base.to_hex(),
                cur.to_hex()
            )));
        }
        (true, Some(_), None) => {
            return Err(Error::conflict(format!(
                "{} is not a readable file; refusing to overwrite it",
                dst.display()
            )));
        }
        _ => {}
    }
    // Read the source ONCE into the wave-10 CAS payload. The metadata length
    // is only an early refusal/allocation hint: the read itself is capped at
    // MAX_MERGE_FILE_BYTES + 1, so a file that grows after the stat can
    // never be materialized unbounded (it becomes a loud Oversized refusal).
    let meta = fs::metadata(&src).map_err(|e| err_not_found(src_rel, e))?;
    if meta.len() > MAX_MERGE_FILE_BYTES {
        return Err(Error::oversized(format!(
            "merge file {src_rel:?} has {} bytes (cap {MAX_MERGE_FILE_BYTES}); refusing to merge it whole",
            meta.len()
        )));
    }
    let mut f = fs::File::open(&src).map_err(|e| err_not_found(src_rel, e))?;
    if !opened_is_path(&f, &src) {
        return Err(Error::permission(format!(
            "{src_rel:?} changed identity between resolution and open (TOCTOU)"
        )));
    }
    use std::io::Read;
    let mut bytes = Vec::with_capacity(meta.len().min(MAX_MERGE_FILE_BYTES + 1) as usize);
    f.by_ref()
        .take(MAX_MERGE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| Error::internal(format!("read {src_rel:?}: {e}")))?;
    if bytes.len() as u64 > MAX_MERGE_FILE_BYTES {
        return Err(Error::oversized(format!(
            "merge file {src_rel:?} grew past the {MAX_MERGE_FILE_BYTES}-byte cap while being read; refusing to merge it whole"
        )));
    }
    let hash = FileHash::from(blake3::hash(&bytes).into());
    if hash != expected_src_hash {
        return Err(Error::conflict(format!(
            "{} drifted since it was staged (expected {}, found {}); nothing was merged",
            src.display(),
            expected_src_hash.to_hex(),
            hash.to_hex()
        )));
    }
    match expected_dst {
        Some(base) => {
            let expected = atomic::FileState {
                exists: true,
                size: None,
                digest: Some(base),
                modified_ms: None,
            };
            atomic::atomic_replace_cas_guarded(
                &dst,
                &expected,
                &bytes,
                &WorkspaceHandle::verify_parent_under_root(&dst_root),
            )
            .map_err(|e| {
                if e.kind == ErrorKind::Conflict {
                    Error::conflict(format!(
                        "{}: {}",
                        dst.display(),
                        e.message
                            .strip_prefix("cas mismatch; ")
                            .unwrap_or(&e.message)
                    ))
                } else {
                    e
                }
            })?;
        }
        None => {
            // Exclusive create as a CAS-ABSENT publish: absent at the
            // commit-time recheck (a concurrent creator is a typed Conflict)
            // and the parent re-verified immediately before the rename, so a
            // create can neither bypass the CAS guard nor be redirected.
            atomic::atomic_replace_cas_guarded(
                &dst,
                &atomic::FileState::absent(),
                &bytes,
                &WorkspaceHandle::verify_parent_under_root(&dst_root),
            )
            .map_err(|e| {
                if e.kind == ErrorKind::Conflict {
                    Error::conflict(format!(
                        "{} appeared since the base snapshot; the merge did not overwrite it",
                        dst.display()
                    ))
                } else {
                    e
                }
            })?;
        }
    }
    Ok(CasMergeResult::Applied)
}

/// Apply ONE approved DELETION: remove `dst_root/dst_rel` only when its
/// current content digest still equals the base-snapshot hash `expected`
/// (the parent must be unchanged since the base snapshot). A destination
/// that moved on fails with a Conflict and stays untouched; an already
/// absent destination is [`CasMergeResult::AlreadyCurrent`] (the merge's
/// end state — absent — already holds). POSIX offers no atomic
/// compare-and-unlink: the digest recheck happens immediately before the
/// unlink, the same recheck-rename window wave-10 documents for CAS
/// content writes. Immediately before the unlink the destination's parent
/// chain is re-verified handle-relative, so a parent swapped for an outside
/// symlink since resolution refuses the deletion instead of unlinking an
/// outside file.
pub fn merge_delete(
    dst_root: &Path,
    dst_rel: &Path,
    expected: FileHash,
) -> Result<CasMergeResult, Error> {
    let dst_root = dst_root.canonicalize().map_err(|e| {
        Error::not_found(format!(
            "merge destination root {}: {e}",
            dst_root.display()
        ))
    })?;
    let dst = resolve_within(&dst_root, dst_rel)?;
    let state = atomic::FileState::now_with_digest(&dst).map_err(|e| {
        Error::new(
            e.kind,
            format!("delete recheck {}: {}", dst.display(), e.message),
        )
    })?;
    if !state.exists {
        return Ok(CasMergeResult::AlreadyCurrent);
    }
    match state.digest {
        Some(cur) if cur == expected => {
            WorkspaceHandle::verify_parent_under_root(&dst_root)(&dst)?;
            fs::remove_file(&dst)
                .map_err(|e| Error::internal(format!("remove {}: {e}", dst.display())))?;
            if let Some(parent) = dst.parent() {
                atomic::fsync_parent(parent);
            }
            Ok(CasMergeResult::Applied)
        }
        Some(cur) => Err(Error::conflict(format!(
            "{} changed since the base snapshot (expected {}, found {}); the deletion was refused",
            dst.display(),
            expected.to_hex(),
            cur.to_hex()
        ))),
        None => Err(Error::conflict(format!(
            "{} is not a readable file; the deletion was refused",
            dst.display()
        ))),
    }
}

/// Commit-time CAS write of RAW bytes at `dst_root/dst_rel` (the landing /
/// rollback primitive beside [`merge_apply_content`] and [`merge_delete`]).
/// `expected` is the destination's previous whole-content digest:
///
/// - `Some(base)`: the destination must still hold `base` (or already hold
///   the new digest — an idempotent replay → [`CasMergeResult::AlreadyCurrent`]);
///   anything else is a typed Conflict and the destination is untouched;
/// - `None`: the destination must not exist (a CAS-ABSENT publish); a
///   concurrent creation is a typed Conflict.
///
/// The write goes through the shared atomic commit discipline with the
/// destination's parent chain re-verified handle-relative immediately
/// before the rename, so a parent swapped for an outside symlink since
/// resolution refuses the write typed; a crash leaves either the old or the
/// new whole file.
pub fn cas_write_content(
    dst_root: &Path,
    dst_rel: &Path,
    bytes: &[u8],
    expected: Option<FileHash>,
) -> Result<CasMergeResult, Error> {
    let dst_root = dst_root.canonicalize().map_err(|e| {
        Error::not_found(format!(
            "cas-write destination root {}: {e}",
            dst_root.display()
        ))
    })?;
    let dst = if expected.is_none() {
        resolve_or_create_within(&dst_root, dst_rel)?
    } else {
        resolve_within(&dst_root, dst_rel)?
    };
    let new_hash = FileHash::from(blake3::hash(bytes).into());
    let state = atomic::FileState::now_with_digest(&dst).map_err(|e| {
        Error::new(
            e.kind,
            format!("cas-write recheck {}: {}", dst.display(), e.message),
        )
    })?;
    match (state.exists, expected, state.digest) {
        (true, _, Some(cur)) if cur == new_hash => Ok(CasMergeResult::AlreadyCurrent),
        (false, Some(_), _) => Err(Error::conflict(format!(
            "{} vanished after the base snapshot; refusing to recreate it from a stale decision",
            dst.display()
        ))),
        (true, None, _) => Err(Error::conflict(format!(
            "{} exists although the base snapshot had no such file; refusing to overwrite it",
            dst.display()
        ))),
        (true, Some(base), Some(cur)) if cur != base => Err(Error::conflict(format!(
            "{} changed since the base snapshot (expected {}, found {}); not overwritten",
            dst.display(),
            base.to_hex(),
            cur.to_hex()
        ))),
        (true, Some(_), None) => Err(Error::conflict(format!(
            "{} is not a readable file; refusing to overwrite it",
            dst.display()
        ))),
        (false, None, _) => {
            // Exclusive create as a CAS-ABSENT publish (parent re-verified
            // immediately before the rename, absent digest rechecked at
            // commit time); a concurrent creator is a typed Conflict.
            atomic::atomic_replace_cas_guarded(
                &dst,
                &atomic::FileState::absent(),
                bytes,
                &WorkspaceHandle::verify_parent_under_root(&dst_root),
            )
            .map_err(|e| {
                if e.kind == ErrorKind::Conflict {
                    Error::conflict(format!(
                        "{} appeared during the landing; refusing to overwrite it",
                        dst.display()
                    ))
                } else {
                    e
                }
            })?;
            Ok(CasMergeResult::Applied)
        }
        (true, Some(base), Some(_)) => {
            let expected = atomic::FileState {
                exists: true,
                size: None,
                digest: Some(base),
                modified_ms: None,
            };
            atomic::atomic_replace_cas_guarded(
                &dst,
                &expected,
                bytes,
                &WorkspaceHandle::verify_parent_under_root(&dst_root),
            )
            .map_err(|e| {
                if e.kind == ErrorKind::Conflict {
                    Error::conflict(format!(
                        "{}: {}",
                        dst.display(),
                        e.message
                            .strip_prefix("cas mismatch; ")
                            .unwrap_or(&e.message)
                    ))
                } else {
                    e
                }
            })?;
            Ok(CasMergeResult::Applied)
        }
    }
}

/// Bounded recursive walk over the canonical tree `root` (already
/// canonicalized, non-symlink). Yields every FILE as (relative path,
/// absolute path, open verified handle) to `visit`. Directory symlinks are
/// followed only when their canonical target stays inside `root`; symlink
/// escapes, unreadable entries and non-file specials fail loudly. The walk
/// never silently skips anything and caps the entry count at `max_entries`.
/// Outcome of processing ONE directory entry: either fully handled or
/// confirmed-gone (it vanished mid-walk because an atomic writer renamed it
/// away — the file does not exist in the tree's current state, so skipping
/// it is honest, never a silent skip of an unreadable file).
enum EntryHandled {
    Done,
    Gone,
}

/// How many times a listed entry may race an atomic writer before the walk
/// gives up loudly (an entry that keeps vanishing/reappearing is hostile).
const WALK_ENTRY_ATTEMPTS: usize = 64;

/// Tree walks ([`snapshot_tree`], [`copy_tree`]) may name directories that
/// are skipped entirely (their whole subtree is never walked): e.g. a
/// source tree's `.git` plumbing is daemon bookkeeping, never content the
/// walk reports or copies. An empty slice skips nothing.
type WalkSkip<'a> = &'a [&'a str];

fn walk_files<F>(
    root: &Path,
    dir: &Path,
    rel: &Path,
    depth: usize,
    max_entries: usize,
    count: &mut usize,
    visit: &mut F,
) -> Result<(), Error>
where
    F: FnMut(&Path, &Path, &fs::File) -> Result<(), Error>,
{
    walk_files_skip(root, dir, rel, depth, max_entries, count, visit, &[])
}

#[allow(clippy::too_many_arguments)]
fn walk_files_skip<F>(
    root: &Path,
    dir: &Path,
    rel: &Path,
    depth: usize,
    max_entries: usize,
    count: &mut usize,
    visit: &mut F,
    skip_dirs: WalkSkip<'_>,
) -> Result<(), Error>
where
    F: FnMut(&Path, &Path, &fs::File) -> Result<(), Error>,
{
    if depth > MAX_TREE_WALK_DEPTH {
        return Err(Error::oversized(format!(
            "tree walk exceeded depth {MAX_TREE_WALK_DEPTH} at {rel:?}"
        )));
    }
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()), // dir vanished
        Err(e) => {
            return Err(Error::internal(format!("read_dir {}: {e}", dir.display())));
        }
    };
    for entry in rd {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                return Err(Error::internal(format!("read_dir {}: {e}", dir.display())));
            }
        };
        let name = entry.file_name();
        let full = dir.join(&name);
        let rel_child = rel.join(&name);
        // Retry window: a listed entry may be renamed away (atomic writer)
        // or replaced between the listing and its open. The entry is
        // handled when it is processed while present; an entry that is
        // confirmed-absent at metadata time is skipped (it no longer is
        // part of the tree); an entry that keeps racing for
        // WALK_ENTRY_ATTEMPTS is hostile and fails loudly.
        let mut handled = EntryHandled::Gone;
        for attempt in 0..WALK_ENTRY_ATTEMPTS {
            let count_before = *count;
            match try_walk_entry(
                root,
                &full,
                &rel_child,
                depth,
                max_entries,
                count,
                visit,
                skip_dirs,
            ) {
                Ok(h) => {
                    handled = h;
                    break;
                }
                Err(e) if attempt + 1 < WALK_ENTRY_ATTEMPTS => {
                    *count = count_before; // a raced attempt must not count
                                           // Transient race (vanished/replaced mid-open): retry;
                                           // the next attempt re-metadatas and either processes a
                                           // stable image or confirms the entry gone.
                    let _ = e;
                    continue;
                }
                Err(e) => {
                    *count = count_before;
                    return Err(e);
                }
            }
        }
        match handled {
            EntryHandled::Done => {}
            EntryHandled::Gone => {}
        }
    }
    Ok(())
}

/// Process one directory entry. Ok(Done) = fully visited;
/// Ok(Gone) = confirmed absent (the file no longer exists); Err = real
/// failure (escape, unsupported type, cap) or a transient race the caller
/// retries.
#[allow(clippy::too_many_arguments)]
fn try_walk_entry<F>(
    root: &Path,
    full: &Path,
    rel_child: &Path,
    depth: usize,
    max_entries: usize,
    count: &mut usize,
    visit: &mut F,
    skip_dirs: WalkSkip<'_>,
) -> Result<EntryHandled, Error>
where
    F: FnMut(&Path, &Path, &fs::File) -> Result<(), Error>,
{
    let meta = match fs::symlink_metadata(full) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(EntryHandled::Gone),
        Err(e) => return Err(Error::internal(format!("metadata {}: {e}", full.display()))),
    };
    if meta.is_dir() {
        // A named skip directory is never walked (its whole subtree is
        // absent from the walk — e.g. `.git` plumbing of a copied repo).
        if let Some(name) = rel_child.file_name().and_then(|n| n.to_str()) {
            if skip_dirs.contains(&name) {
                return Ok(EntryHandled::Done);
            }
        }
        walk_files_skip(
            root,
            full,
            rel_child,
            depth + 1,
            max_entries,
            count,
            visit,
            skip_dirs,
        )?;
        return Ok(EntryHandled::Done);
    }
    if meta.is_file() {
        if *count >= max_entries {
            return Err(Error::oversized(format!(
                "tree at {root:?} exceeds the {max_entries}-entry snapshot cap"
            )));
        }
        *count += 1;
        let f = match fs::File::open(full) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                *count -= 1;
                return Ok(EntryHandled::Gone);
            }
            Err(e) => {
                return Err(Error::internal(format!("open {}: {e}", full.display())));
            }
        };
        if !opened_is_path(&f, full) {
            return Err(Error::permission(format!(
                "{rel_child:?} changed identity between resolution and open (TOCTOU)"
            )));
        }
        visit(rel_child, full, &f)?;
        return Ok(EntryHandled::Done);
    }
    if meta.file_type().is_symlink() {
        let target = fs::canonicalize(full).map_err(|e| {
            Error::permission(format!(
                "symlink {rel_child:?} cannot be resolved safely: {e}"
            ))
        })?;
        if !target.starts_with(root) {
            return Err(Error::permission(format!(
                "symlink escape rejected: {rel_child:?} -> {}",
                target.display()
            )));
        }
        let tmeta = match fs::symlink_metadata(&target) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(EntryHandled::Gone);
            }
            Err(e) => {
                return Err(Error::internal(format!(
                    "metadata {}: {e}",
                    target.display()
                )));
            }
        };
        if tmeta.is_dir() {
            if let Some(name) = rel_child.file_name().and_then(|n| n.to_str()) {
                if skip_dirs.contains(&name) {
                    return Ok(EntryHandled::Done);
                }
            }
            walk_files_skip(
                root,
                &target,
                rel_child,
                depth + 1,
                max_entries,
                count,
                visit,
                skip_dirs,
            )?;
            return Ok(EntryHandled::Done);
        }
        if tmeta.is_file() {
            if *count >= max_entries {
                return Err(Error::oversized(format!(
                    "tree at {root:?} exceeds the {max_entries}-entry snapshot cap"
                )));
            }
            *count += 1;
            let f = fs::File::open(&target)
                .map_err(|e| Error::internal(format!("open {}: {e}", target.display())))?;
            if !opened_is_path(&f, &target) {
                return Err(Error::permission(format!(
                    "{rel_child:?} changed identity between resolution and open (TOCTOU)"
                )));
            }
            visit(rel_child, &target, &f)?;
            return Ok(EntryHandled::Done);
        }
        return Err(Error::internal(format!(
            "symlink {rel_child:?} resolves to an unsupported file type"
        )));
    }
    Err(Error::internal(format!(
        "refusing to walk unsupported file type at {rel_child:?}"
    )))
}

/// Stream-hash an already-open file handle. Returns (bytes read, hash).
fn hash_open_file(f: &fs::File) -> Result<(u64, FileHash), Error> {
    use std::io::Read;
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    let mut reader = std::io::BufReader::new(f);
    let mut hashed = 0u64;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| Error::internal(format!("read: {e}")))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        hashed += n as u64;
    }
    Ok((hashed, FileHash::from(hasher.finalize().into())))
}

/// Stream-copy an already-open source handle into `target` with the durable
/// atomic sequence: unique temp + (optional permission bits) + fsync +
/// rename + parent fsync. Returns (bytes copied, hash of the copied
/// content). Never skips, never tears. `permissions` is `Some` for the
/// canonical manifest copy and for the stable tree copies (which must
/// preserve the executable bit the tree identity hashes); `None` keeps the
/// caller-created default bits.
pub(crate) fn copy_open_file(
    f: &fs::File,
    target: &Path,
    permissions: Option<fs::Permissions>,
) -> Result<(u64, FileHash), Error> {
    let parent = target
        .parent()
        .ok_or_else(|| Error::malformed(format!("{target:?} has no parent")))?;
    fs::create_dir_all(parent)
        .map_err(|e| Error::internal(format!("mkdir {}: {e}", parent.display())))?;
    let tmp = parent.join(atomic::temp_name(
        &target
            .file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default(),
    ));
    let (n, hash) = {
        let mut out = fs::File::create(&tmp)
            .map_err(|e| Error::internal(format!("create {}: {e}", tmp.display())))?;
        let mut hasher = blake3::Hasher::new();
        use std::io::{Read, Write};
        let mut reader = std::io::BufReader::new(f);
        let mut buf = [0u8; 64 * 1024];
        let mut copied = 0u64;
        loop {
            let r = reader
                .read(&mut buf)
                .map_err(|e| Error::internal(format!("read: {e}")))?;
            if r == 0 {
                break;
            }
            out.write_all(&buf[..r])
                .map_err(|e| Error::internal(format!("write {}: {e}", tmp.display())))?;
            hasher.update(&buf[..r]);
            copied += r as u64;
        }
        if let Some(perms) = permissions {
            out.set_permissions(perms)
                .map_err(|e| Error::internal(format!("set permissions {}: {e}", tmp.display())))?;
        }
        out.flush()
            .map_err(|e| Error::internal(format!("flush {}: {e}", tmp.display())))?;
        out.sync_all()
            .map_err(|e| Error::internal(format!("fsync {}: {e}", tmp.display())))?;
        (copied, FileHash::from(hasher.finalize().into()))
    };
    fs::rename(&tmp, target).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        Error::internal(format!(
            "rename {} -> {}: {e}",
            tmp.display(),
            target.display()
        ))
    })?;
    atomic::fsync_parent(parent);
    Ok((n, hash))
}

/// Resolve `rel` under the CANONICAL root, creating every missing
/// INTERMEDIATE directory along the way (used for exclusive merge creates
/// whose parent directories the parent tree never had). Every step is
/// verified: an existing prefix must canonicalize to a directory INSIDE the
/// root (symlink escapes and swapped prefixes fail loudly), and a created
/// prefix is re-verified immediately after `mkdir` (a racer cannot smuggle
/// a symlink past the check). The final component may name an absent file.
fn resolve_or_create_within(root: &Path, rel: &Path) -> Result<PathBuf, Error> {
    // Same Windows hazard pre-screen as `resolve_within`: nothing may be
    // created through a namespace bypass or an alternate data stream.
    #[cfg(windows)]
    platform::lexical_check(root, rel)?;
    let comps: Vec<Component> = rel.components().collect();
    if comps.is_empty() {
        return Err(Error::malformed(format!("{rel:?} is empty")));
    }
    let mut prefix = root.to_path_buf();
    for (i, comp) in comps.iter().enumerate() {
        let Component::Normal(name) = comp else {
            return Err(Error::permission(format!(
                "path traversal rejected: {rel:?}"
            )));
        };
        prefix.push(name);
        let last = i + 1 == comps.len();
        match fs::symlink_metadata(&prefix) {
            Ok(meta) => {
                let canon = prefix.canonicalize().map_err(|_| {
                    Error::permission(format!("parent resolution failed: {rel:?} ({name:?})"))
                })?;
                if !canon.starts_with(root) {
                    return Err(Error::permission(format!(
                        "path escapes workspace: {rel:?}"
                    )));
                }
                if !last && !meta.is_dir() {
                    return Err(Error::conflict(format!(
                        "{:?} exists and is not a directory; {:?} cannot be merged",
                        canon.display(),
                        rel
                    )));
                }
                prefix = canon;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if last {
                    // The final file simply does not exist yet.
                    return Ok(prefix);
                }
                fs::create_dir(&prefix).map_err(|ce| {
                    if ce.kind() == std::io::ErrorKind::AlreadyExists {
                        Error::conflict(format!(
                            "{:?} appeared while {:?} was being staged",
                            prefix.display(),
                            rel
                        ))
                    } else {
                        Error::internal(format!("mkdir {}: {ce}", prefix.display()))
                    }
                })?;
                let canon = prefix.canonicalize().map_err(|_| {
                    Error::permission(format!(
                        "parent resolution failed after mkdir: {rel:?} ({name:?})"
                    ))
                })?;
                if !canon.starts_with(root) {
                    return Err(Error::permission(format!(
                        "path escapes workspace: {rel:?}"
                    )));
                }
                prefix = canon;
            }
            Err(e) => {
                return Err(Error::internal(format!(
                    "metadata {}: {e}",
                    prefix.display()
                )));
            }
        }
    }
    Ok(prefix)
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;

/// Report row 6: cache eviction is use-aware. A held (in-use) durable root
/// is never the eviction victim while unused shadow roots exist, so the hot
/// root keeps its watcher across 3x the cache bound of shadow churn.
#[test]
fn in_use_durable_root_is_never_evicted_by_shadow_churn() {
    let dir = tempfile::tempdir().unwrap();
    // No watcher registrations: the preflight cap of zero forces the
    // deterministic degraded path, so this cache probe cannot occupy the
    // singleton registration worker (and cannot slow the parallel watcher
    // tests).
    let service = WorkspaceFileService::with_watch_preflight_entries(0);
    let id = faktor_core::id::WorkspaceId::new(7);
    let durable_root = dir.path().join("durable");
    std::fs::create_dir_all(&durable_root).unwrap();
    let durable = service.open(id, durable_root.clone()).unwrap();
    let canonical = durable_root.canonicalize().unwrap();
    for i in 0..(crate::workspace_service::MAX_CACHED_ROOTS * 3) {
        let root = dir.path().join(format!("shadow-{i}"));
        std::fs::create_dir_all(&root).unwrap();
        let _ = service.open(id, root).unwrap();
    }
    let cached = service.cached_roots();
    assert!(
        cached
            .iter()
            .any(|(cached_id, root)| *cached_id == id && *root == canonical),
        "the in-use durable root must stay cached: {cached:?}"
    );
    assert!(
        cached.len() <= crate::workspace_service::MAX_CACHED_ROOTS,
        "unused shadow roots are evicted, not the held durable root: {}",
        cached.len()
    );
    // The held handle is still fully usable.
    std::fs::write(canonical.join("still-hot.txt"), b"ok").unwrap();
    assert!(durable
        .read_default(std::path::Path::new("still-hot.txt"))
        .is_ok());
}

/// Adversarial bound: when EVERY cached root is still held there is no
/// legal eviction victim. The registry must stay at the bound and serve the
/// new root uncached; the historical code inserted anyway, so held-root
/// churn grew the cache without limit (one watcher per turn).
#[test]
fn held_roots_never_grow_the_registry_past_the_bound() {
    let dir = tempfile::tempdir().unwrap();
    let service = WorkspaceFileService::with_watch_preflight_entries(0);
    let id = faktor_core::id::WorkspaceId::new(9);
    let overflow = 16;
    let mut held = Vec::new();
    for i in 0..(crate::workspace_service::MAX_CACHED_ROOTS + overflow) {
        let root = dir.path().join(format!("held-{i}"));
        std::fs::create_dir_all(&root).unwrap();
        held.push(service.open(id, root).unwrap());
    }
    assert_eq!(
        service.open_count(),
        crate::workspace_service::MAX_CACHED_ROOTS,
        "held roots must not push the registry past its hard bound"
    );
    // Every handle, cached or not, remains a usable root.
    for handle in held {
        std::fs::write(handle.root().join("probe.txt"), b"ok").unwrap();
        assert!(handle
            .read_default(std::path::Path::new("probe.txt"))
            .is_ok());
    }
}

/// Eviction order is the insertion index, not a hash-map scan: the oldest
/// unused root is the victim and the newest root is retained.
#[test]
fn eviction_takes_the_oldest_unused_root_in_insertion_order() {
    let dir = tempfile::tempdir().unwrap();
    let service = WorkspaceFileService::with_watch_preflight_entries(0);
    let id = faktor_core::id::WorkspaceId::new(10);
    for i in 0..crate::workspace_service::MAX_CACHED_ROOTS {
        let root = dir.path().join(format!("ordered-{i:03}"));
        std::fs::create_dir_all(&root).unwrap();
        drop(service.open(id, root).unwrap());
    }
    let newest = dir.path().join("newest");
    std::fs::create_dir_all(&newest).unwrap();
    let newest_handle = service.open(id, newest.clone()).unwrap();
    let cached = service.cached_roots();
    assert!(
        !cached.iter().any(|(_, root)| root.ends_with("ordered-000")),
        "the oldest unused root must be evicted first: {cached:?}"
    );
    assert!(
        cached
            .iter()
            .any(|(_, root)| *root == newest.canonicalize().unwrap()),
        "the newest root must be retained: {cached:?}"
    );
    drop(newest_handle);
}

/// Scale probe regression: every shadowed turn opens a NEW root under the
/// session's durable workspace id. The registry must stay bounded (one
/// watcher thread per cached root), while still allowing the durable root and
/// the live shadow root to coexist.
#[test]
fn cached_workspace_roots_are_hard_bounded() {
    let dir = tempfile::tempdir().unwrap();
    // A zero-entry preflight forces the deterministic degraded (no-watcher)
    // path WITHOUT enqueueing registrations on the singleton worker: the
    // test is about the REGISTRY bound, not the platform watcher backend's
    // latency, and a 192-deep registration flood would starve the parallel
    // watcher tests.
    let service = WorkspaceFileService::with_watch_preflight_entries(0);
    let id = faktor_core::id::WorkspaceId::new(7);
    for i in 0..(crate::workspace_service::MAX_CACHED_ROOTS * 3) {
        let root = dir.path().join(format!("shadow-{i}"));
        std::fs::create_dir_all(&root).unwrap();
        let handle = service.open(id, root).unwrap();
        assert_eq!(handle.workspace_id(), id);
    }
    assert!(
        service.open_count() <= crate::workspace_service::MAX_CACHED_ROOTS,
        "cached roots must not grow with turn count: {}",
        service.open_count()
    );
}

/// Instrumented eviction cost: with ONE held durable root and the rest
/// unused, each insert at capacity examines the oldest ordered entry (the
/// historical full-map `min_by_key` scanned all 64 every time). The probe
/// counts candidate examinations, not wall time, so it is CI-stable.
#[test]
fn eviction_examines_the_oldest_index_entry_not_the_whole_map() {
    let dir = tempfile::tempdir().unwrap();
    let service = WorkspaceFileService::with_watch_preflight_entries(0);
    let id = faktor_core::id::WorkspaceId::new(11);
    let durable_root = dir.path().join("durable");
    std::fs::create_dir_all(&durable_root).unwrap();
    let _held = service.open(id, durable_root).unwrap();
    for i in 0..crate::workspace_service::MAX_CACHED_ROOTS {
        let root = dir.path().join(format!("churn-{i:03}"));
        std::fs::create_dir_all(&root).unwrap();
        drop(service.open(id, root).unwrap());
    }
    crate::workspace_service::EVICTION_CANDIDATES.with(|count| count.set(0));
    const ROUNDS: usize = 32;
    for i in 0..ROUNDS {
        let root = dir.path().join(format!("probe-{i:03}"));
        std::fs::create_dir_all(&root).unwrap();
        drop(service.open(id, root).unwrap());
    }
    let probes = crate::workspace_service::EVICTION_CANDIDATES.with(|count| count.get());
    assert!(
        probes <= ROUNDS * 2,
        "eviction scanned {probes} candidates over {ROUNDS} inserts (ordered index should probe ~1)"
    );
}
