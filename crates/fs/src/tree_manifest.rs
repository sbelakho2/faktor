//! Canonical tree manifest: the ONE definition of "the same tree" for every
//! root snapshot in the runtime (run bases, candidates, verification
//! records, integration transactions, completion-step guards and final
//! completion).
//!
//! One manifest entry is one non-directory entry under the root:
//!
//! * `kind` — [`TreeEntryKind::Regular`] or [`TreeEntryKind::Symlink`].
//!   Directories are implicit (they exist only as path prefixes of entries)
//!   and are never entries themselves.
//! * `mode` — the canonical git-style mode: `100644` regular, `100755`
//!   executable regular, `120000` symlink. For regular files the executable
//!   bit is the ONLY permission bit that participates in tree identity
//!   (`chmod 0644 -> 0755` changes the digest; `chmod 0644 -> 0600` does
//!   not), matching what git records as `100644`/`100755`. Ownership,
//!   timestamps and setuid/setgid/sticky bits are NOT part of the manifest.
//!   On platforms without an executable bit (Windows) every regular file is
//!   `100644`.
//! * `payload_digest` — BLAKE3 (lowercase hex) of the file's WHOLE content
//!   (streamed) for regular files, or of the LITERAL symlink target bytes
//!   for symlinks. Symlinks are NEVER followed: a link to a file/dir, a
//!   broken link, a link to outside the root and a link loop are all hashed
//!   as their literal target string, so the walk can never traverse outside
//!   the root.
//! * `normalized_path` — the relative path with exactly one `/` between
//!   components, no leading/trailing separator and no `.`/`..` component.
//!   Paths are stored as their exact UTF-8 encoding: unicode is NOT
//!   normalized (an NFC and an NFD spelling are two different entries) and a
//!   non-UTF-8 OS name is a typed [`TreeManifestError::Malformed`] refusal
//!   rather than a lossy fold. NUL cannot occur in an OS path name, so the
//!   length-prefixed framing below is unambiguous either way.
//!
//! Special files (FIFOs, sockets, block/character devices) cannot be
//! represented by the canonical modes and are NOT silently folded away: the
//! walk records them as completeness notes ([`TreeManifest::special_files`])
//! and every digest/equality ([`TreeManifest::digest`],
//! [`tree_manifest_digest`]) returns the typed
//! [`TreeManifestError::SpecialFile`] refusal. Equality is never silently
//! degraded by an unrepresentable entry.
//!
//! # Canonical digest (format `tm1`)
//!
//! Entries are sorted by `normalized_path` ascending (UTF-8 byte order) and
//! serialized WITHOUT ambiguity before hashing:
//!
//! ```text
//! "faktor-tree-manifest\0" "tm1:" u64_be(entry_count)
//! for each entry:
//!   u8   kind (0 = regular, 1 = symlink)
//!   u32be mode octal (0o100644 | 0o100755 | 0o120000)
//!   u32be path_len, path bytes
//!   u32be payload_len, payload hex bytes
//! ```
//!
//! The digest is `"tm1:" + hex(BLAKE3(serialization))`. The version prefix
//! makes a canonical digest distinguishable from the historical
//! content-only 64-char hex digest it replaces: a record written by the old
//! definition can never silently compare equal to a canonical digest — it
//! fails equality loudly and must be re-verified.
//!
//! # Bounds
//!
//! `max_entries` bounds EVERY filesystem entry the walk inspects
//! (directories and skipped VCS directories included),
//! [`MAX_TREE_MANIFEST_DEPTH`] bounds the recursion; beyond either the walk
//! is a typed [`TreeManifestError::Oversized`] refusal — never a partial
//! manifest. Paths beyond [`MAX_TREE_MANIFEST_PATH_BYTES`] and link targets
//! beyond [`MAX_TREE_MANIFEST_LINK_BYTES`] are typed refusals too.
//!
//! Directories named in `skip_dirs` ([`TREE_MANIFEST_SKIP_DIRS`] for the
//! runtime's snapshots) are never walked at any depth: VCS bookkeeping the
//! completion steps legitimately mutate (commits) is never working content.

use std::fs;
use std::io::Read;
use std::path::Path;

use faktor_core::error::ErrorKind;

use crate::rooted::{RootedDir, RootedEntry, RootedEntryKind, WalkBudget, WalkStep};

/// Version prefix of the canonical digest (`tm1:<64-hex>`). Old content-only
/// digests are bare 64-char hex and can never equal a canonical one.
pub const TREE_MANIFEST_DIGEST_PREFIX: &str = "tm1:";

/// Hard bound on one bounded manifest walk: trees beyond it are typed
/// `Oversized` refusals, never a partial manifest.
pub const MAX_TREE_MANIFEST_ENTRIES: usize = 100_000;

/// Hard depth bound of the manifest walk (a hostile nested tree fails loudly
/// instead of recursing without bound).
pub const MAX_TREE_MANIFEST_DEPTH: usize = 64;

/// Directory names skipped at ANY depth by the canonical manifest: VCS
/// bookkeeping the completion steps legitimately mutate (commits) and that
/// is never working content. Everything else is manifested.
pub const TREE_MANIFEST_SKIP_DIRS: &[&str] = &[".git", ".hg", ".svn"];

/// Hard bound on one normalized entry path (UTF-8 bytes).
pub const MAX_TREE_MANIFEST_PATH_BYTES: usize = 64 * 1024;

/// Hard bound on one literal symlink target (bytes hashed into the manifest,
/// never resolved).
pub const MAX_TREE_MANIFEST_LINK_BYTES: usize = 1024 * 1024;

/// The entry kind of the canonical manifest. Dirs are implicit; special
/// files are excluded and surface as a typed refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeEntryKind {
    Regular,
    Symlink,
}

/// The canonical (git-style) mode of one manifest entry: `100644` regular,
/// `100755` executable regular, `120000` symlink.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalMode {
    RegularFile,
    ExecutableFile,
    Symlink,
}

impl CanonicalMode {
    /// The octal value recorded by git (`100644` / `100755` / `120000`).
    pub const fn octal(self) -> u32 {
        match self {
            CanonicalMode::RegularFile => 0o100_644,
            CanonicalMode::ExecutableFile => 0o100_755,
            CanonicalMode::Symlink => 0o120_000,
        }
    }
}

/// One entry of the canonical tree manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    /// Relative path, `/`-separated, exact UTF-8 (no unicode normalization).
    pub normalized_path: String,
    pub kind: TreeEntryKind,
    pub mode: CanonicalMode,
    /// BLAKE3 hex of the whole content (regular) or the LITERAL target
    /// bytes (symlink, never followed).
    pub payload_digest: String,
}

/// A canonical manifest: the sorted entries plus the completeness notes of
/// every special file the walk refused to fold away.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TreeManifest {
    entries: Vec<TreeEntry>,
    special_files: Vec<String>,
}

impl TreeManifest {
    /// The manifest entries, sorted by `normalized_path` (UTF-8 byte order).
    pub fn entries(&self) -> &[TreeEntry] {
        &self.entries
    }

    /// Special files (FIFOs, sockets, devices) found under the root. A
    /// non-empty list means equality is UNPROVABLE and [`Self::digest`]
    /// refuses; the list is surfaced so a caller can name the blocking
    /// entries instead of silently comparing a degraded tree.
    pub fn special_files(&self) -> &[String] {
        &self.special_files
    }

    /// The canonical digest (`tm1:<64-hex>`), or the typed
    /// [`TreeManifestError::SpecialFile`] refusal while any special file
    /// exists under the root. The entries are re-sorted canonically here so
    /// the digest never depends on construction order.
    pub fn digest(&self) -> Result<String, TreeManifestError> {
        if !self.special_files.is_empty() {
            return Err(TreeManifestError::SpecialFile {
                paths: self.special_files.clone(),
            });
        }
        let mut entries: Vec<&TreeEntry> = self.entries.iter().collect();
        entries.sort_by(|a, b| a.normalized_path.cmp(&b.normalized_path));
        Ok(canonical_digest(&entries))
    }
}

/// Typed refusals of the canonical manifest walk/copy. `RootUnavailable`,
/// `Malformed`, `Oversized` and `SpecialFile` are semantic refusals;
/// `Io` is an underlying filesystem failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TreeManifestError {
    #[error("tree manifest root unavailable: {0}")]
    RootUnavailable(String),
    #[error("tree manifest malformed: {0}")]
    Malformed(String),
    #[error("tree manifest exceeds bound: {0}")]
    Oversized(String),
    #[error(
        "tree manifest refused: the tree contains special file(s) {paths:?}; the canonical \
         manifest cannot represent them, so tree equality is unprovable"
    )]
    SpecialFile { paths: Vec<String> },
    #[error("tree manifest i/o failure: {0}")]
    Io(String),
}

/// True when `value` is a canonical canonical-manifest digest
/// (`tm1:` + exactly 64 lowercase-or-uppercase hex chars).
pub fn is_tree_manifest_digest(value: &str) -> bool {
    value
        .strip_prefix(TREE_MANIFEST_DIGEST_PREFIX)
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// Build the canonical manifest of every non-directory entry under `root`.
///
/// The root's identity is the caller's choice (it is canonicalized once);
/// every entry below it is reached through the rooted-directory authority:
/// directories are opened handle-relative (`openat`/NT relative open),
/// children are inspected with no-follow semantics, and symlinks INSIDE the
/// tree are never followed — they are hashed as their literal target, so no
/// entry can resolve outside the root and loops cannot hang the walk.
/// Regular files are streamed (bounded RAM) through the open handle.
pub fn tree_manifest(root: &Path, max_entries: usize) -> Result<TreeManifest, TreeManifestError> {
    if max_entries == 0 {
        return Err(TreeManifestError::Malformed(
            "tree manifest max_entries must be >= 1".into(),
        ));
    }
    // The canonical manifest's historical whole-operation bound is the entry
    // cap (plus the depth cap): entry/depth-exhaustion is a typed Oversized
    // refusal and the read budget is unbounded because the manifest's job is
    // a whole-tree digest (streamed, never materialized).
    // max_entries + 1 directories: with the depth cap a tree can never hold
    // more dirs than entries + 1, so this directory bound can never refuse
    // before the historical entry bound does (parity).
    let mut budget = WalkBudget::new(
        max_entries,
        max_entries.saturating_add(1),
        MAX_TREE_MANIFEST_DEPTH,
        u64::MAX,
        u64::MAX,
    );
    tree_manifest_budgeted(root, &mut budget)
}

/// [`tree_manifest`] with an explicit whole-operation [`WalkBudget`]. The
/// caller owns every cap (entries, directories, depth, total file bytes,
/// total bytes read); exhaustion is the typed
/// [`TreeManifestError::Oversized`] refusal — never a partial manifest.
pub fn tree_manifest_budgeted(
    root: &Path,
    budget: &mut WalkBudget,
) -> Result<TreeManifest, TreeManifestError> {
    let canonical = root
        .canonicalize()
        .map_err(|e| TreeManifestError::RootUnavailable(format!("{}: {e}", root.display())))?;
    if !canonical.is_dir() {
        return Err(TreeManifestError::RootUnavailable(format!(
            "{} is not a directory",
            root.display()
        )));
    }
    let dir = RootedDir::open(&canonical)
        .map_err(|e| TreeManifestError::RootUnavailable(format!("{}: {e}", root.display())))?;
    let mut manifest = TreeManifest::default();
    let mut failure: Option<TreeManifestError> = None;
    {
        let mut visit = |entry: &RootedEntry,
                         _depth: usize,
                         budget: &mut WalkBudget|
         -> Result<WalkStep, crate::Error> {
            if failure.is_some() {
                return Ok(WalkStep::Stop);
            }
            match manifest_entry(&dir, &mut manifest, entry, budget) {
                Ok(step) => Ok(step),
                Err(e) => {
                    failure = Some(e);
                    Ok(WalkStep::Stop)
                }
            }
        };
        if let Err(e) = dir.walk_bounded(Path::new(""), budget, TREE_MANIFEST_SKIP_DIRS, &mut visit)
        {
            return Err(manifest_walk_error(e));
        }
    }
    if let Some(e) = failure {
        return Err(e);
    }
    manifest
        .entries
        .sort_by(|a, b| a.normalized_path.cmp(&b.normalized_path));
    Ok(manifest)
}

/// One entry of a budgeted manifest walk.
fn manifest_entry(
    dir: &RootedDir,
    manifest: &mut TreeManifest,
    entry: &RootedEntry,
    budget: &mut WalkBudget,
) -> Result<WalkStep, TreeManifestError> {
    let name = entry.name.to_str().ok_or_else(|| {
        TreeManifestError::Malformed(format!(
            "tree manifest entry name {:?} is not valid UTF-8; a lossy fold would collide distinct names",
            entry.name
        ))
    })?;
    // Internal atomic-write temporaries (an in-flight or crashed CAS
    // writer's `.{name}.kp-tmp-*`) are daemon bookkeeping, never working
    // content: they are invisible to the canonical manifest exactly like
    // `.git` is (a materialized root must digest the same with or without a
    // crash residue temp).
    if crate::atomic::is_internal_temp_name(name) {
        // Internal atomic-write temporaries are daemon bookkeeping, never
        // content: a temp-named DIRECTORY is not descended either.
        return Ok(if entry.kind == RootedEntryKind::Directory {
            WalkStep::SkipDir
        } else {
            WalkStep::Continue
        });
    }
    let rel = entry.rel.to_str().ok_or_else(|| {
        TreeManifestError::Malformed(format!(
            "tree manifest path {:?} is not valid UTF-8; a lossy fold would collide distinct paths",
            entry.rel
        ))
    })?;
    if rel.len() > MAX_TREE_MANIFEST_PATH_BYTES {
        return Err(TreeManifestError::Oversized(format!(
            "tree manifest path of {} bytes exceeds MAX_TREE_MANIFEST_PATH_BYTES ({MAX_TREE_MANIFEST_PATH_BYTES})",
            rel.len()
        )));
    }
    match entry.kind {
        RootedEntryKind::Directory => Ok(WalkStep::Continue),
        RootedEntryKind::Symlink => {
            let target = dir.read_link(&entry.rel).map_err(manifest_io)?;
            let bytes = link_target_bytes(&target);
            if bytes.len() > MAX_TREE_MANIFEST_LINK_BYTES {
                return Err(TreeManifestError::Oversized(format!(
                    "tree manifest symlink {rel:?} target of {} bytes exceeds MAX_TREE_MANIFEST_LINK_BYTES ({MAX_TREE_MANIFEST_LINK_BYTES})",
                    bytes.len()
                )));
            }
            budget
                .charge_read_bytes(bytes.len() as u64)
                .map_err(manifest_walk_error)?;
            manifest.entries.push(TreeEntry {
                normalized_path: rel.to_string(),
                kind: TreeEntryKind::Symlink,
                mode: CanonicalMode::Symlink,
                payload_digest: blake3::hash(&bytes).to_hex().to_string(),
            });
            Ok(WalkStep::Continue)
        }
        RootedEntryKind::File => {
            let mut file = dir.open_read(&entry.rel).map_err(manifest_io)?;
            let payload_digest = hash_reader_charged(&mut file, budget).map_err(manifest_io)?;
            manifest.entries.push(TreeEntry {
                normalized_path: rel.to_string(),
                kind: TreeEntryKind::Regular,
                mode: canonical_mode_for(entry.executable()),
                payload_digest,
            });
            Ok(WalkStep::Continue)
        }
        RootedEntryKind::Other => {
            manifest.special_files.push(rel.to_string());
            Ok(WalkStep::Continue)
        }
    }
}

/// The canonical digest of `root` (`tm1:<64-hex>`), refusing with the typed
/// [`TreeManifestError::SpecialFile`] while any special file exists under
/// the root (equality is unprovable then, never silently degraded).
pub fn tree_manifest_digest(root: &Path, max_entries: usize) -> Result<String, TreeManifestError> {
    tree_manifest(root, max_entries)?.digest()
}

/// Manifest-faithful bounded copy of every entry under `src_root` into the
/// existing `dst_root`.
///
/// The copy walks the SOURCE through the rooted-directory authority (every
/// entry is reached handle-relative; symlinks are recreated as LITERAL
/// symlinks and never followed, so loops/escapes are copyable and stay
/// links) and writes each destination through the same authority (temp entry
/// created with `open_create_new` + fsync + anchored publish). Regular files
/// stream through the durable atomic sequence with their permission bits
/// preserved, so the canonical mode of the copy is the source's. Special
/// files are the typed [`TreeManifestError::SpecialFile`] refusal. Internal
/// atomic temporaries (`.kp-tmp-*`) are skipped exactly like
/// [`crate::copy_tree_skip`]. The returned manifest covers the copied entries
/// (sorted), so a caller can record it without a second walk of the source.
pub fn copy_tree_manifest(
    src_root: &Path,
    dst_root: &Path,
    max_entries: usize,
    max_total_bytes: u64,
    skip_dirs: &[&str],
) -> Result<TreeManifest, TreeManifestError> {
    if max_entries == 0 || max_total_bytes == 0 {
        return Err(TreeManifestError::Malformed(
            "copy caps must be >= 1".into(),
        ));
    }
    // The copy's whole-operation budget: entries/dirs/depth as for the
    // manifest, and BOTH byte budgets equal the historical total bound (the
    // copy reads exactly the bytes it writes; symlink targets count toward
    // the total exactly as before).
    let mut budget = WalkBudget::new(
        max_entries,
        max_entries.saturating_add(1),
        MAX_TREE_MANIFEST_DEPTH,
        max_total_bytes,
        max_total_bytes,
    );
    copy_tree_manifest_budgeted(src_root, dst_root, skip_dirs, &mut budget)
}

/// [`copy_tree_manifest`] with an explicit whole-operation [`WalkBudget`].
pub fn copy_tree_manifest_budgeted(
    src_root: &Path,
    dst_root: &Path,
    skip_dirs: &[&str],
    budget: &mut WalkBudget,
) -> Result<TreeManifest, TreeManifestError> {
    for name in skip_dirs {
        if name.is_empty()
            || name.contains('/')
            || name.contains('\\')
            || *name == "."
            || *name == ".."
        {
            return Err(TreeManifestError::Malformed(format!(
                "skip directory name {name:?} must be a bare file name"
            )));
        }
    }
    let src = src_root.canonicalize().map_err(|e| {
        TreeManifestError::RootUnavailable(format!("copy source {}: {e}", src_root.display()))
    })?;
    let dst = dst_root.canonicalize().map_err(|e| {
        TreeManifestError::RootUnavailable(format!("copy destination {}: {e}", dst_root.display()))
    })?;
    if !src.is_dir() || !dst.is_dir() {
        return Err(TreeManifestError::RootUnavailable(
            "copy source and destination must be directories".into(),
        ));
    }
    if src == dst {
        return Err(TreeManifestError::Malformed(
            "copy source and destination are the same tree".into(),
        ));
    }
    let src_dir = RootedDir::open(&src).map_err(|e| {
        TreeManifestError::RootUnavailable(format!("copy source {}: {e}", src.display()))
    })?;
    let dst_dir = RootedDir::open(&dst).map_err(|e| {
        TreeManifestError::RootUnavailable(format!("copy destination {}: {e}", dst.display()))
    })?;
    let mut manifest = TreeManifest::default();
    let mut failure: Option<TreeManifestError> = None;
    {
        let mut visit = |entry: &RootedEntry,
                         _depth: usize,
                         budget: &mut WalkBudget|
         -> Result<WalkStep, crate::Error> {
            if failure.is_some() {
                return Ok(WalkStep::Stop);
            }
            match copy_entry(&src_dir, &dst_dir, &mut manifest, entry, budget) {
                Ok(step) => Ok(step),
                Err(e) => {
                    failure = Some(e);
                    Ok(WalkStep::Stop)
                }
            }
        };
        if let Err(e) = src_dir.walk_bounded(Path::new(""), budget, skip_dirs, &mut visit) {
            if failure.is_none() {
                failure = Some(manifest_walk_error(e));
            }
        }
    }
    if let Some(e) = failure {
        return Err(e);
    }
    manifest
        .entries
        .sort_by(|a, b| a.normalized_path.cmp(&b.normalized_path));
    Ok(manifest)
}

/// Copy one source entry into the destination authority.
fn copy_entry(
    src: &RootedDir,
    dst: &RootedDir,
    manifest: &mut TreeManifest,
    entry: &RootedEntry,
    budget: &mut WalkBudget,
) -> Result<WalkStep, TreeManifestError> {
    let name = entry.name.to_str().ok_or_else(|| {
        TreeManifestError::Malformed(format!(
            "tree manifest copy entry name {:?} is not valid UTF-8",
            entry.name
        ))
    })?;
    let rel = entry.rel.to_str().ok_or_else(|| {
        TreeManifestError::Malformed(format!(
            "tree manifest copy path {:?} is not valid UTF-8",
            entry.rel
        ))
    })?;
    if rel.len() > MAX_TREE_MANIFEST_PATH_BYTES {
        return Err(TreeManifestError::Oversized(format!(
            "tree manifest copy path of {} bytes exceeds MAX_TREE_MANIFEST_PATH_BYTES ({MAX_TREE_MANIFEST_PATH_BYTES})",
            rel.len()
        )));
    }
    if crate::atomic::is_internal_temp_name(name) {
        // A concurrent in-flight daemon temp is never part of a
        // materialized copy (parity with `copy_tree_skip`); a temp-named
        // DIRECTORY is not descended either.
        return Ok(if entry.kind == RootedEntryKind::Directory {
            WalkStep::SkipDir
        } else {
            WalkStep::Continue
        });
    }
    match entry.kind {
        RootedEntryKind::Directory => {
            dst.create_dir_all(&entry.rel).map_err(manifest_io)?;
            Ok(WalkStep::Continue)
        }
        RootedEntryKind::Symlink => {
            let target = src.read_link(&entry.rel).map_err(manifest_io)?;
            let bytes = link_target_bytes(&target);
            if bytes.len() > MAX_TREE_MANIFEST_LINK_BYTES {
                return Err(TreeManifestError::Oversized(format!(
                    "tree manifest copy symlink {rel:?} target of {} bytes exceeds MAX_TREE_MANIFEST_LINK_BYTES ({MAX_TREE_MANIFEST_LINK_BYTES})",
                    bytes.len()
                )));
            }
            budget
                .charge_file_bytes(bytes.len() as u64)
                .map_err(manifest_walk_error)?;
            dst.create_symlink(&target, &entry.rel)
                .map_err(manifest_io)?;
            manifest.entries.push(TreeEntry {
                normalized_path: rel.to_string(),
                kind: TreeEntryKind::Symlink,
                mode: CanonicalMode::Symlink,
                payload_digest: blake3::hash(&bytes).to_hex().to_string(),
            });
            Ok(WalkStep::Continue)
        }
        RootedEntryKind::File => {
            let size = entry.size;
            let mut file = src.open_read(&entry.rel).map_err(manifest_io)?;
            let permissions = file.metadata().map(|m| m.permissions()).ok();
            let (n, hash) = copy_open_file_rooted(dst, &entry.rel, &mut file, permissions, budget)
                .map_err(manifest_io)?;
            if n != size {
                return Err(TreeManifestError::Malformed(format!(
                    "tree manifest copy of {rel:?} changed size mid-copy ({n} of {size} bytes)"
                )));
            }
            manifest.entries.push(TreeEntry {
                normalized_path: rel.to_string(),
                kind: TreeEntryKind::Regular,
                mode: canonical_mode_for(entry.executable()),
                payload_digest: hash.to_hex(),
            });
            Ok(WalkStep::Continue)
        }
        RootedEntryKind::Other => {
            manifest.special_files.push(rel.to_string());
            Err(TreeManifestError::SpecialFile {
                paths: manifest.special_files.clone(),
            })
        }
    }
}

/// Stream one open source file into a fresh destination temp via the
/// anchored authority (exclusive create -> write -> fsync -> anchored
/// publish) and return (bytes copied, hash). The budget is charged for every
/// byte read; a failure removes the temp entry.
fn copy_open_file_rooted(
    dst: &RootedDir,
    dst_rel: &Path,
    src: &mut fs::File,
    permissions: Option<fs::Permissions>,
    budget: &mut WalkBudget,
) -> Result<(u64, crate::FileHash), crate::Error> {
    use std::io::Write as _;
    let name = dst_rel
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let parent = dst_rel.parent().unwrap_or_else(|| Path::new(""));
    let tmp_rel = parent.join(format!(
        ".{}.kp-tmp-{}-{}",
        name,
        std::process::id(),
        uuid::Uuid::new_v4()
    ));
    let result = (|| -> Result<(u64, crate::FileHash), crate::Error> {
        let mut out = dst.open_create_new(&tmp_rel)?;
        let mut hasher = blake3::Hasher::new();
        let mut buf = [0u8; 64 * 1024];
        let mut copied = 0u64;
        loop {
            let n = src
                .read(&mut buf)
                .map_err(|e| crate::Error::internal(format!("read {dst_rel:?}: {e}")))?;
            if n == 0 {
                break;
            }
            budget.charge_read_bytes(n as u64)?;
            out.write_all(&buf[..n])
                .map_err(|e| crate::Error::internal(format!("write {}: {e}", tmp_rel.display())))?;
            hasher.update(&buf[..n]);
            copied += n as u64;
        }
        if let Some(perms) = permissions {
            out.set_permissions(perms).map_err(|e| {
                crate::Error::internal(format!("set permissions {}: {e}", tmp_rel.display()))
            })?;
        }
        out.flush()
            .map_err(|e| crate::Error::internal(format!("flush {}: {e}", tmp_rel.display())))?;
        out.sync_all()
            .map_err(|e| crate::Error::internal(format!("fsync {}: {e}", tmp_rel.display())))?;
        drop(out);
        dst.atomic_publish(&tmp_rel, dst_rel)?;
        Ok((copied, crate::FileHash::from(hasher.finalize().into())))
    })();
    if result.is_err() {
        let _ = dst.remove_file(&tmp_rel);
    }
    result
}

/// The canonical byte serialization folded into the digest. Length prefixes
/// make the framing unambiguous even though an OS path can contain any byte
/// except NUL (and a symlink target payload is hashed, never embedded raw).
fn canonical_digest(entries: &[&TreeEntry]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"faktor-tree-manifest\0");
    hasher.update(TREE_MANIFEST_DIGEST_PREFIX.as_bytes());
    hasher.update(&(entries.len() as u64).to_be_bytes());
    for entry in entries {
        hasher.update(&[match entry.kind {
            TreeEntryKind::Regular => 0u8,
            TreeEntryKind::Symlink => 1u8,
        }]);
        hasher.update(&entry.mode.octal().to_be_bytes());
        hasher.update(&(entry.normalized_path.len() as u32).to_be_bytes());
        hasher.update(entry.normalized_path.as_bytes());
        hasher.update(&(entry.payload_digest.len() as u32).to_be_bytes());
        hasher.update(entry.payload_digest.as_bytes());
    }
    format!(
        "{TREE_MANIFEST_DIGEST_PREFIX}{}",
        hasher.finalize().to_hex()
    )
}

/// Stream-hash an open file through the budget: every byte read is charged
/// ([`WalkBudget::charge_read_bytes`]) so whole-operation read budgets are
/// honest.
fn hash_reader_charged(
    mut reader: impl Read,
    budget: &mut WalkBudget,
) -> Result<String, crate::Error> {
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| crate::Error::internal(format!("read: {e}")))?;
        if n == 0 {
            break;
        }
        budget.charge_read_bytes(n as u64)?;
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// The canonical mode of one regular file: the ANY-execute-bit projection
/// (`100755` when any of `0o111` is set, else `100644`).
fn canonical_mode_for(executable: bool) -> CanonicalMode {
    if executable {
        CanonicalMode::ExecutableFile
    } else {
        CanonicalMode::RegularFile
    }
}

/// Map a handle-walk failure onto the manifest's typed refusal space. A
/// containment refusal (a link where the walk required a real entry, a
/// bogus path) is a [`TreeManifestError::Malformed`] refusal: the tree no
/// longer is what it claimed to be.
fn manifest_walk_error(e: crate::Error) -> TreeManifestError {
    match e.kind {
        ErrorKind::Oversized => TreeManifestError::Oversized(e.message),
        ErrorKind::Malformed => TreeManifestError::Malformed(e.message),
        ErrorKind::Permission => TreeManifestError::Malformed(format!(
            "containment refusal during the tree walk: {}",
            e.message
        )),
        ErrorKind::NotFound => TreeManifestError::RootUnavailable(e.message),
        _ => TreeManifestError::Io(e.message),
    }
}

/// Map a per-entry io failure (open/read/stat) onto the manifest space.
fn manifest_io(e: crate::Error) -> TreeManifestError {
    manifest_walk_error(e)
}

/// The LITERAL symlink target bytes hashed into the manifest. Unix hashes
/// the raw OS bytes (a target need not be UTF-8); other platforms hash the
/// UTF-8 encoding of the target path.
#[cfg(unix)]
fn link_target_bytes(target: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    target.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn link_target_bytes(target: &Path) -> Vec<u8> {
    target.to_string_lossy().into_owned().into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn digest(root: &Path) -> String {
        tree_manifest_digest(root, MAX_TREE_MANIFEST_ENTRIES).unwrap()
    }

    /// Parity vector: the canonical digest and every per-entry digest of a
    /// clean fixture tree, captured with the PRE-rewrite path-based walker,
    /// must stay byte-identical after the rooted-handle rewrite. A drift
    /// means the traversal or the canonical serialization changed.
    #[cfg(unix)]
    #[test]
    fn clean_tree_digest_parity_vector() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        write(&root, "a/x.txt", b"x");
        write(&root, "a/y/z.txt", b"z");
        write(&root, "b.txt", b"b");
        write(&root, "tool.sh", b"#!/bin/sh\n");
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root.join("tool.sh"), fs::Permissions::from_mode(0o755)).unwrap();
            std::os::unix::fs::symlink("../a/x.txt", root.join("link")).unwrap();
        }
        assert_eq!(
            digest(&root),
            "tm1:ea537450a1fbb6ebc3ce15d477e7095b74854e18465a0e07ea714920bed45020"
        );
        let manifest = tree_manifest(&root, 100).unwrap();
        let rows: Vec<(String, u32, String)> = manifest
            .entries()
            .iter()
            .map(|e| {
                (
                    e.normalized_path.clone(),
                    e.mode.octal(),
                    e.payload_digest.clone(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                (
                    "a/x.txt".to_string(),
                    0o100_644,
                    "3ae7d805f6789a6402acb70ad4096a85a56bf6804eaf25c0493ac697548d30b5".to_string()
                ),
                (
                    "a/y/z.txt".to_string(),
                    0o100_644,
                    "1104908ab930e671002c7cd7f3fc921570b1bf64ecfa12fe363585c630eaca6b".to_string()
                ),
                (
                    "b.txt".to_string(),
                    0o100_644,
                    "10e5cf3d3c8a4f9f3468c8cc58eea84892a22fdadbc1acb22410190044c1d553".to_string()
                ),
                (
                    "link".to_string(),
                    0o120_000,
                    "88971e633741657ff005b75df6fb88081045add7e7708cc00babb7fbc74e2dd1".to_string()
                ),
                (
                    "tool.sh".to_string(),
                    0o100_755,
                    "bc1f407a11c9377c8b9b13f956b279c8462775105eb958fc9ae3c40de87cc96e".to_string()
                ),
            ]
        );
    }

    struct SeamClear;
    impl Drop for SeamClear {
        fn drop(&mut self) {
            crate::rooted::clear_entry_seam();
        }
    }

    /// Install the (process-global) entry seam; the shared lock serializes
    /// every seam-using test in the binary.
    fn install_seam(
        f: impl Fn(&Path) + Send + 'static,
    ) -> (std::sync::MutexGuard<'static, ()>, SeamClear) {
        let guard = crate::rooted::ENTRY_SEAM_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::rooted::install_entry_seam(Box::new(f));
        (guard, SeamClear)
    }

    /// A file enumerated as a regular file, swapped for an outside symlink in
    /// the enumeration -> open window, is a typed refusal: the outside bytes
    /// are never hashed into any manifest entry.
    #[cfg(unix)]
    #[test]
    fn manifest_refuses_a_file_swapped_for_an_outside_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        write(&outside, "secret.txt", b"outside-manifest-secret");
        let root = dir.path().join("root");
        write(&root, "seam-swap.txt", b"inside");
        let swap_root = root.clone();
        let swap_target = outside.join("secret.txt");
        let (_lock, _clear) = install_seam(move |rel| {
            if rel == Path::new("seam-swap.txt") {
                let _ = fs::remove_file(swap_root.join("seam-swap.txt"));
                let _ = std::os::unix::fs::symlink(&swap_target, swap_root.join("seam-swap.txt"));
            }
        });
        let err = tree_manifest(&root, MAX_TREE_MANIFEST_ENTRIES).unwrap_err();
        assert!(
            matches!(err, TreeManifestError::Malformed(_)),
            "expected a loud containment refusal, got {err:?}"
        );
        assert_eq!(
            fs::read(outside.join("secret.txt")).unwrap(),
            b"outside-manifest-secret"
        );
    }

    /// A directory swapped for an outside symlink before descent is refused;
    /// the outside tree is never entered and the moved-away original stays
    /// intact (the walk never follows the replacement).
    #[cfg(unix)]
    #[test]
    fn manifest_refuses_a_dir_swapped_for_an_outside_symlink_before_descent() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        write(&outside, "marker.txt", b"outside-dir-secret");
        let root = dir.path().join("root");
        write(&root, "a-seam/keep.txt", b"inside");
        let swap_root = root.clone();
        let swap_outside = outside.clone();
        let (_lock, _clear) = install_seam(move |rel| {
            if rel == Path::new("a-seam") {
                // libc rename: the static-authority scan forbids the std
                // rename spelling outside the sanctioned atomic module, and
                // this is test scaffolding, not a publish.
                let from =
                    std::ffi::CString::new(swap_root.join("a-seam").to_str().unwrap()).unwrap();
                let to = std::ffi::CString::new(swap_root.join("a-seam-moved").to_str().unwrap())
                    .unwrap();
                // SAFETY: both CStrings are NUL-terminated valid paths.
                unsafe {
                    libc::rename(from.as_ptr(), to.as_ptr());
                }
                let _ = std::os::unix::fs::symlink(&swap_outside, swap_root.join("a-seam"));
            }
        });
        let err = tree_manifest(&root, MAX_TREE_MANIFEST_ENTRIES).unwrap_err();
        assert!(
            matches!(err, TreeManifestError::Malformed(_)),
            "expected a loud containment refusal, got {err:?}"
        );
        assert!(outside.join("marker.txt").exists());
        assert!(!outside.join("keep.txt").exists());
        assert!(root.join("a-seam-moved/keep.txt").exists());
    }

    /// The manifest copy refuses the same swap and leaves no partial or temp
    /// entry in the destination.
    #[cfg(unix)]
    #[test]
    fn copy_refuses_a_source_file_swapped_for_an_outside_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        write(&outside, "secret.txt", b"outside-copy-secret");
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        write(&src, "copy-seam.txt", b"inside");
        fs::create_dir_all(&dst).unwrap();
        let swap_src = src.clone();
        let swap_target = outside.join("secret.txt");
        let (_lock, _clear) = install_seam(move |rel| {
            if rel == Path::new("copy-seam.txt") {
                let _ = fs::remove_file(swap_src.join("copy-seam.txt"));
                let _ = std::os::unix::fs::symlink(&swap_target, swap_src.join("copy-seam.txt"));
            }
        });
        let err = copy_tree_manifest(&src, &dst, MAX_TREE_MANIFEST_ENTRIES, 1024 * 1024, &[])
            .unwrap_err();
        assert!(
            matches!(err, TreeManifestError::Malformed(_)),
            "expected a loud containment refusal, got {err:?}"
        );
        assert!(!dst.join("copy-seam.txt").exists());
        for entry in fs::read_dir(&dst).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            assert!(!name.contains("kp-tmp-"), "temp leaked: {name}");
        }
        assert_eq!(
            fs::read(outside.join("secret.txt")).unwrap(),
            b"outside-copy-secret"
        );
    }

    /// VCS bookkeeping and internal atomic-temp directories are invisible to
    /// BOTH the manifest and the copy (old `copy_tree_skip` parity): they are
    /// charged to the walk budget but never visited, descended or
    /// materialized.
    #[test]
    fn skipped_and_temp_dirs_are_never_walked_or_materialized() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        write(&src, "keep.txt", b"keep");
        write(&src, ".git/objects/blob", b"plumbing");
        write(&src, "x.kp-tmp-crash/sub/file.txt", b"residue");
        fs::create_dir_all(&dst).unwrap();
        let manifest = copy_tree_manifest(
            &src,
            &dst,
            MAX_TREE_MANIFEST_ENTRIES,
            1024 * 1024,
            TREE_MANIFEST_SKIP_DIRS,
        )
        .unwrap();
        let paths: Vec<&str> = manifest
            .entries()
            .iter()
            .map(|e| e.normalized_path.as_str())
            .collect();
        assert_eq!(paths, vec!["keep.txt"]);
        assert!(
            !dst.join(".git").exists(),
            "a skipped VCS dir must not be materialized"
        );
        assert!(
            !dst.join("x.kp-tmp-crash").exists(),
            "a temp-named dir must not be materialized"
        );
        assert_eq!(fs::read(dst.join("keep.txt")).unwrap(), b"keep");
    }

    /// Budget exhaustion through the explicit whole-operation budgets is a
    /// typed Oversized refusal for both the manifest walk and the copy.
    #[test]
    fn budget_exhaustion_is_typed_oversized() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        write(&src, "a.txt", b"a");
        write(&src, "b.txt", b"bb");
        write(&src, "sub/c.txt", b"ccc");
        fs::create_dir_all(&dst).unwrap();

        let mut budget = WalkBudget::new(1, 100, 64, u64::MAX, u64::MAX);
        assert!(matches!(
            tree_manifest_budgeted(&src, &mut budget),
            Err(TreeManifestError::Oversized(_))
        ));

        let mut budget = WalkBudget::new(100, 100, 64, 1, 1);
        assert!(matches!(
            copy_tree_manifest_budgeted(&src, &dst, &[], &mut budget),
            Err(TreeManifestError::Oversized(_))
        ));

        let mut budget = WalkBudget::new(100, 100, 0, u64::MAX, u64::MAX);
        assert!(matches!(
            tree_manifest_budgeted(&src, &mut budget),
            Err(TreeManifestError::Oversized(_))
        ));
    }

    fn write(root: &Path, rel: &str, bytes: &[u8]) -> PathBuf {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        path
    }

    fn entry<'a>(manifest: &'a TreeManifest, path: &str) -> &'a TreeEntry {
        manifest
            .entries()
            .iter()
            .find(|e| e.normalized_path == path)
            .unwrap_or_else(|| panic!("entry {path:?} missing from {:?}", manifest.entries()))
    }

    #[test]
    fn digest_is_deterministic_across_creation_order_and_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        let entries = [
            ("b.txt", b"b".as_slice()),
            ("a/x.txt", b"x".as_slice()),
            ("a/y/z.txt", b"z".as_slice()),
            ("c.txt", b"c".as_slice()),
        ];
        for (rel, bytes) in entries {
            write(&a, rel, bytes);
        }
        for (rel, bytes) in entries.iter().rev() {
            write(&b, rel, bytes);
        }
        let da = digest(&a);
        assert_eq!(da, digest(&b), "creation order is not part of identity");
        assert!(da.starts_with(TREE_MANIFEST_DIGEST_PREFIX));
        assert!(is_tree_manifest_digest(&da));
        let manifest = tree_manifest(&a, MAX_TREE_MANIFEST_ENTRIES).unwrap();
        let names: Vec<&str> = manifest
            .entries()
            .iter()
            .map(|e| e.normalized_path.as_str())
            .collect();
        assert_eq!(names, vec!["a/x.txt", "a/y/z.txt", "b.txt", "c.txt"]);
    }

    #[test]
    fn content_change_changes_the_digest_and_old_shape_is_never_matched() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        write(&root, "f.txt", b"one");
        let first = digest(&root);
        write(&root, "f.txt", b"two");
        let second = digest(&root);
        assert_ne!(first, second);
        // A legacy content-only digest is bare 64-char hex: never a
        // canonical digest and never equal to one.
        let legacy = "0".repeat(64);
        assert!(!is_tree_manifest_digest(&legacy));
        assert_ne!(first, legacy);
        assert!(!is_tree_manifest_digest("tm1:nothex"));
        assert!(!is_tree_manifest_digest(&format!("tm1:{}", "0".repeat(63))));
    }

    #[cfg(unix)]
    #[test]
    fn executable_bit_changes_the_digest_other_permission_bits_do_not() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let f = write(&root, "tool.sh", b"#!/bin/sh\n");
        fs::set_permissions(&f, fs::Permissions::from_mode(0o644)).unwrap();
        let regular = digest(&root);
        assert_eq!(
            entry(&tree_manifest(&root, 100).unwrap(), "tool.sh").mode,
            CanonicalMode::RegularFile
        );
        fs::set_permissions(&f, fs::Permissions::from_mode(0o754)).unwrap();
        let executable = digest(&root);
        assert_ne!(regular, executable, "chmod 0644 -> 0755 must be visible");
        assert_eq!(
            entry(&tree_manifest(&root, 100).unwrap(), "tool.sh").mode,
            CanonicalMode::ExecutableFile
        );
        // Read/write/setuid permission bits are NOT part of the canonical
        // mode projection (git records only the exec bit for regular files).
        fs::set_permissions(&f, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(digest(&root), regular);
        fs::set_permissions(&f, fs::Permissions::from_mode(0o4755)).unwrap();
        assert_eq!(digest(&root), executable);
    }

    #[cfg(unix)]
    #[test]
    fn regular_to_symlink_with_same_resolved_bytes_changes_the_digest() {
        let dir = tempfile::tempdir().unwrap();
        let regular = dir.path().join("regular");
        write(&regular, "f", b"same");
        let linked = dir.path().join("linked");
        write(&linked, "target.txt", b"same");
        std::os::unix::fs::symlink("target.txt", linked.join("f")).unwrap();
        let dr = digest(&regular);
        let dl = digest(&linked);
        assert_ne!(
            dr, dl,
            "regular(100644) and symlink(120000) over the same resolved bytes are different trees"
        );
        assert_eq!(
            entry(&tree_manifest(&regular, 100).unwrap(), "f").mode,
            CanonicalMode::RegularFile
        );
        assert_eq!(
            entry(&tree_manifest(&linked, 100).unwrap(), "f").mode,
            CanonicalMode::Symlink
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_target_change_with_identical_resolved_bytes_changes_the_digest() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        write(&root, "data.txt", b"same");
        write(&root, "other.txt", b"same");
        let link = root.join("link");
        std::os::unix::fs::symlink("data.txt", &link).unwrap();
        let first = digest(&root);
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink("other.txt", &link).unwrap();
        let second = digest(&root);
        assert_ne!(first, second);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_hashed_literally_and_never_followed_outside_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        write(&outside, "secret.txt", b"v1");
        let root = dir.path().join("root");
        write(&root, "real.txt", b"real");
        std::os::unix::fs::symlink("../outside/secret.txt", root.join("escape")).unwrap();
        std::os::unix::fs::symlink("../outside", root.join("dir_escape")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", root.join("absolute")).unwrap();
        std::os::unix::fs::symlink("loop", root.join("loop")).unwrap();
        let first = digest(&root);
        // Changing what the links RESOLVE to must not move the digest: only
        // the literal target strings are content.
        write(&outside, "secret.txt", b"v2");
        assert_eq!(digest(&root), first);
        let manifest = tree_manifest(&root, 100).unwrap();
        for name in ["escape", "dir_escape", "absolute", "loop"] {
            let e = entry(&manifest, name);
            assert_eq!(e.kind, TreeEntryKind::Symlink);
            assert_eq!(e.mode, CanonicalMode::Symlink);
        }
        assert_eq!(
            entry(&manifest, "escape").payload_digest,
            blake3::hash(b"../outside/secret.txt").to_hex().to_string()
        );
        // No traversal: the outside secret and the loop target never appear
        // as manifest paths.
        let paths: Vec<&str> = manifest
            .entries()
            .iter()
            .map(|e| e.normalized_path.as_str())
            .collect();
        assert_eq!(
            paths,
            vec!["absolute", "dir_escape", "escape", "loop", "real.txt"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn directory_symlink_is_not_traversed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        write(&root, "real/f.txt", b"content");
        std::os::unix::fs::symlink("real", root.join("link")).unwrap();
        let manifest = tree_manifest(&root, 100).unwrap();
        let paths: Vec<&str> = manifest
            .entries()
            .iter()
            .map(|e| e.normalized_path.as_str())
            .collect();
        assert_eq!(paths, vec!["link", "real/f.txt"]);
        // The same content reached through the link is NOT a second entry.
        assert!(manifest
            .entries()
            .iter()
            .all(|e| e.normalized_path != "link/f.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn fifo_is_a_typed_refusal_with_a_completeness_note() {
        use std::ffi::CString;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        write(&root, "ok.txt", b"ok");
        let fifo = root.join("pipe");
        let c = CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path.
        let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o644) };
        assert_eq!(rc, 0, "mkfifo: {}", std::io::Error::last_os_error());
        let manifest = tree_manifest(&root, 100).unwrap();
        assert_eq!(manifest.special_files().to_vec(), vec!["pipe".to_string()]);
        assert!(manifest
            .entries()
            .iter()
            .all(|e| e.normalized_path != "pipe"));
        match manifest.digest() {
            Err(TreeManifestError::SpecialFile { paths }) => assert_eq!(paths, vec!["pipe"]),
            other => panic!("expected a typed special-file refusal, got {other:?}"),
        }
        match tree_manifest_digest(&root, 100) {
            Err(TreeManifestError::SpecialFile { .. }) => {}
            other => panic!("expected a typed special-file refusal, got {other:?}"),
        }
    }

    #[test]
    fn entry_cap_and_depth_are_typed_refusals_never_partial() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        write(&root, "a.txt", b"a");
        write(&root, "b.txt", b"b");
        assert!(matches!(
            tree_manifest(&root, 1),
            Err(TreeManifestError::Oversized(_))
        ));
        assert!(matches!(
            tree_manifest(&root, 0),
            Err(TreeManifestError::Malformed(_))
        ));
        // A deep (hostile) tree fails on the depth bound.
        let mut deep = root.clone();
        for _ in 0..(MAX_TREE_MANIFEST_DEPTH + 2) {
            deep = deep.join("d");
        }
        fs::create_dir_all(&deep).unwrap();
        assert!(matches!(
            tree_manifest(&root, MAX_TREE_MANIFEST_ENTRIES),
            Err(TreeManifestError::Oversized(_))
        ));
    }

    #[test]
    fn vcs_bookkeeping_dirs_are_skipped_at_any_depth() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        write(&root, "src/main.rs", b"fn main() {}");
        let before = digest(&root);
        write(&root, ".git/objects/blob", b"commit state");
        write(&root, "src/.svn/entries", b"svn");
        assert_eq!(digest(&root), before, "VCS bookkeeping is not content");
        assert!(tree_manifest(&root, 100)
            .unwrap()
            .entries()
            .iter()
            .all(|e| !e.normalized_path.contains(".git")));
    }

    #[test]
    fn unicode_names_are_exact_and_never_normalized() {
        let nfc = TreeManifest {
            entries: vec![TreeEntry {
                normalized_path: "caf\u{e9}.txt".into(),
                kind: TreeEntryKind::Regular,
                mode: CanonicalMode::RegularFile,
                payload_digest: blake3::hash(b"x").to_hex().to_string(),
            }],
            special_files: Vec::new(),
        };
        let nfd = TreeManifest {
            entries: vec![TreeEntry {
                normalized_path: "cafe\u{301}.txt".into(),
                kind: TreeEntryKind::Regular,
                mode: CanonicalMode::RegularFile,
                payload_digest: blake3::hash(b"x").to_hex().to_string(),
            }],
            special_files: Vec::new(),
        };
        assert_ne!(
            nfc.digest().unwrap(),
            nfd.digest().unwrap(),
            "no unicode normalization: distinct spellings are distinct tree entries"
        );
        // NUL cannot occur in an OS path, and the length-prefixed framing
        // keeps a (synthetic) embedded NUL unambiguous.
        let embedded = TreeManifest {
            entries: vec![TreeEntry {
                normalized_path: "a\0b".into(),
                kind: TreeEntryKind::Regular,
                mode: CanonicalMode::RegularFile,
                payload_digest: blake3::hash(b"x").to_hex().to_string(),
            }],
            special_files: Vec::new(),
        };
        let split = TreeManifest {
            entries: vec![
                TreeEntry {
                    normalized_path: "a".into(),
                    kind: TreeEntryKind::Regular,
                    mode: CanonicalMode::RegularFile,
                    payload_digest: blake3::hash(b"x").to_hex().to_string(),
                },
                TreeEntry {
                    normalized_path: "b".into(),
                    kind: TreeEntryKind::Regular,
                    mode: CanonicalMode::RegularFile,
                    payload_digest: blake3::hash(b"x").to_hex().to_string(),
                },
            ],
            special_files: Vec::new(),
        };
        assert_ne!(embedded.digest().unwrap(), split.digest().unwrap());
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        write(&root, "\u{65e5}\u{672c}\u{8a9e}.txt", b"ok");
        let manifest = tree_manifest(&root, 100).unwrap();
        assert_eq!(
            manifest.entries()[0].normalized_path,
            "\u{65e5}\u{672c}\u{8a9e}.txt"
        );
    }

    // APFS refuses to store invalid-UTF-8 names (EILSEQ at create), so this
    // hostile-name probe is linux-only: it locks the walk's typed refusal.
    #[cfg(target_os = "linux")]
    #[test]
    fn non_utf8_name_is_a_typed_malformed_refusal() {
        use std::os::unix::ffi::OsStringExt;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir_all(&root).unwrap();
        let name = std::ffi::OsString::from_vec(vec![0x66, 0x80, 0x6f]);
        fs::write(root.join(&name), b"x").unwrap();
        match tree_manifest(&root, 100) {
            Err(TreeManifestError::Malformed(message)) => {
                assert!(message.contains("UTF-8"), "{message}")
            }
            other => panic!("expected a typed malformed refusal, got {other:?}"),
        }
    }

    #[test]
    fn missing_or_non_directory_root_is_a_typed_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        assert!(matches!(
            tree_manifest(&missing, 100),
            Err(TreeManifestError::RootUnavailable(_))
        ));
        let file = write(dir.path(), "file.txt", b"x");
        assert!(matches!(
            tree_manifest(&file, 100),
            Err(TreeManifestError::RootUnavailable(_))
        ));
    }

    #[test]
    fn empty_roots_have_one_stable_digest() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        assert_eq!(digest(&a), digest(&b));
        assert!(tree_manifest(&a, 1).unwrap().entries().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn copy_is_manifest_faithful_for_modes_and_symlinks() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        let tool = write(&src, "bin/tool.sh", b"#!/bin/sh\necho hi\n");
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        write(&src, "bin/readme.md", b"readme");
        std::os::unix::fs::symlink("readme.md", src.join("bin/readme.link")).unwrap();
        std::os::unix::fs::symlink("../..", src.join("bin/escape")).unwrap();
        fs::create_dir_all(&dst).unwrap();
        let copied = copy_tree_manifest(
            &src,
            &dst,
            MAX_TREE_MANIFEST_ENTRIES,
            1024 * 1024,
            TREE_MANIFEST_SKIP_DIRS,
        )
        .unwrap();
        assert_eq!(copied.digest().unwrap(), digest(&src));
        assert_eq!(digest(&dst), digest(&src), "the copy is the SAME tree");
        assert!(fs::symlink_metadata(dst.join("bin/readme.link"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read_link(dst.join("bin/escape")).unwrap(),
            PathBuf::from("../..")
        );
        let mode = fs::metadata(dst.join("bin/tool.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_ne!(mode & 0o111, 0, "executable bit preserved");
        let readme_mode = fs::metadata(dst.join("bin/readme.md"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            readme_mode & 0o111,
            0,
            "non-executable stays non-executable"
        );
    }

    #[cfg(unix)]
    #[test]
    fn copy_refuses_special_files_and_respects_caps() {
        use std::ffi::CString;
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        write(&src, "a.txt", b"a");
        write(&src, "b.txt", b"bb");
        fs::create_dir_all(&dst).unwrap();
        assert!(matches!(
            copy_tree_manifest(&src, &dst, 1, 1024, &[]),
            Err(TreeManifestError::Oversized(_))
        ));
        assert!(matches!(
            copy_tree_manifest(&src, &dst, 100, 2, &[]),
            Err(TreeManifestError::Oversized(_))
        ));
        let fifo = src.join("pipe");
        let c = CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path.
        let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o644) };
        assert_eq!(rc, 0, "mkfifo: {}", std::io::Error::last_os_error());
        assert!(matches!(
            copy_tree_manifest(&src, &dst, 100, 1024, &[]),
            Err(TreeManifestError::SpecialFile { .. })
        ));
    }
}
