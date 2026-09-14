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
use std::path::{Path, PathBuf};

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
/// The root path itself is canonicalized (its identity is the caller's
/// choice); symlinks INSIDE the tree are never followed, so no entry can
/// resolve outside the root and loops cannot hang the walk. Regular files
/// are streamed (bounded RAM) with a unix TOCTOU identity check between
/// lstat and open.
pub fn tree_manifest(root: &Path, max_entries: usize) -> Result<TreeManifest, TreeManifestError> {
    if max_entries == 0 {
        return Err(TreeManifestError::Malformed(
            "tree manifest max_entries must be >= 1".into(),
        ));
    }
    let canonical = root
        .canonicalize()
        .map_err(|e| TreeManifestError::RootUnavailable(format!("{}: {e}", root.display())))?;
    if !canonical.is_dir() {
        return Err(TreeManifestError::RootUnavailable(format!(
            "{} is not a directory",
            root.display()
        )));
    }
    let mut manifest = TreeManifest::default();
    let mut count = 0usize;
    // Iterative DFS with an explicit stack: every frame is one directory
    // whose entries are sorted, so the traversal itself is deterministic.
    let mut stack: Vec<(PathBuf, String, usize)> = vec![(canonical.clone(), String::new(), 0)];
    while let Some((dir, prefix, depth)) = stack.pop() {
        if depth > MAX_TREE_MANIFEST_DEPTH {
            return Err(TreeManifestError::Oversized(format!(
                "tree manifest walk exceeded MAX_TREE_MANIFEST_DEPTH ({MAX_TREE_MANIFEST_DEPTH})"
            )));
        }
        let mut children: Vec<(std::ffi::OsString, PathBuf)> = Vec::new();
        let read = fs::read_dir(&dir)
            .map_err(|e| TreeManifestError::Io(format!("read_dir {}: {e}", dir.display())))?;
        for entry in read {
            let entry = entry
                .map_err(|e| TreeManifestError::Io(format!("read_dir {}: {e}", dir.display())))?;
            children.push((entry.file_name(), entry.path()));
        }
        children.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, path) in children {
            count += 1;
            if count > max_entries {
                return Err(TreeManifestError::Oversized(format!(
                    "tree manifest exceeds {max_entries} entries; refusing a partial manifest"
                )));
            }
            let name = name.into_string().map_err(|bad| {
                TreeManifestError::Malformed(format!(
                    "tree manifest entry name {bad:?} is not valid UTF-8; a lossy fold would collide distinct names"
                ))
            })?;
            let rel = join_relative(&prefix, &name);
            if rel.len() > MAX_TREE_MANIFEST_PATH_BYTES {
                return Err(TreeManifestError::Oversized(format!(
                    "tree manifest path of {} bytes exceeds MAX_TREE_MANIFEST_PATH_BYTES ({MAX_TREE_MANIFEST_PATH_BYTES})",
                    rel.len()
                )));
            }
            let meta = fs::symlink_metadata(&path)
                .map_err(|e| TreeManifestError::Io(format!("metadata {}: {e}", path.display())))?;
            let file_type = meta.file_type();
            if file_type.is_symlink() {
                let target = fs::read_link(&path).map_err(|e| {
                    TreeManifestError::Io(format!("read_link {}: {e}", path.display()))
                })?;
                let bytes = link_target_bytes(&target);
                if bytes.len() > MAX_TREE_MANIFEST_LINK_BYTES {
                    return Err(TreeManifestError::Oversized(format!(
                        "tree manifest symlink {rel:?} target of {} bytes exceeds MAX_TREE_MANIFEST_LINK_BYTES ({MAX_TREE_MANIFEST_LINK_BYTES})",
                        bytes.len()
                    )));
                }
                manifest.entries.push(TreeEntry {
                    normalized_path: rel,
                    kind: TreeEntryKind::Symlink,
                    mode: CanonicalMode::Symlink,
                    payload_digest: blake3::hash(&bytes).to_hex().to_string(),
                });
            } else if file_type.is_dir() {
                if TREE_MANIFEST_SKIP_DIRS.contains(&name.as_str()) {
                    continue;
                }
                stack.push((path, rel, depth + 1));
            } else if file_type.is_file() {
                let file = fs::File::open(&path)
                    .map_err(|e| TreeManifestError::Io(format!("open {}: {e}", path.display())))?;
                // unix TOCTOU hardening: the opened file must be the very
                // entry lstat classified (a swapped entry can never smuggle
                // bytes from a different file into this manifest entry).
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    let opened = file.metadata().map_err(|e| {
                        TreeManifestError::Io(format!("metadata {}: {e}", path.display()))
                    })?;
                    if meta.dev() != opened.dev() || meta.ino() != opened.ino() {
                        return Err(TreeManifestError::Malformed(format!(
                            "tree manifest entry {rel:?} changed identity between metadata and open (TOCTOU)"
                        )));
                    }
                }
                let payload_digest = hash_reader(file)
                    .map_err(|e| TreeManifestError::Io(format!("read {}: {e}", path.display())))?;
                manifest.entries.push(TreeEntry {
                    normalized_path: rel,
                    kind: TreeEntryKind::Regular,
                    mode: canonical_mode(&meta),
                    payload_digest,
                });
            } else {
                manifest.special_files.push(rel);
            }
        }
    }
    manifest
        .entries
        .sort_by(|a, b| a.normalized_path.cmp(&b.normalized_path));
    Ok(manifest)
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
/// Regular files are streamed through the durable atomic sequence (temp +
/// fsync + rename + parent fsync) with their permission bits preserved, so
/// the canonical mode of the copy is the source's. Symlinks are recreated
/// as LITERAL symlinks (never followed), so loops/escapes are copyable and
/// stay links. Special files are the typed
/// [`TreeManifestError::SpecialFile`] refusal. Internal atomic temporaries
/// (`.kp-tmp-*`) are skipped exactly like [`crate::copy_tree_skip`]. The
/// returned manifest covers the copied entries (sorted), so a caller can
/// record it without a second walk of the source.
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
    let mut manifest = TreeManifest::default();
    let mut count = 0usize;
    let mut total = 0u64;
    let mut stack: Vec<(PathBuf, PathBuf, String, usize)> =
        vec![(src.clone(), dst.clone(), String::new(), 0)];
    while let Some((src_dir, dst_dir, prefix, depth)) = stack.pop() {
        if depth > MAX_TREE_MANIFEST_DEPTH {
            return Err(TreeManifestError::Oversized(format!(
                "tree manifest copy exceeded MAX_TREE_MANIFEST_DEPTH ({MAX_TREE_MANIFEST_DEPTH})"
            )));
        }
        let mut children: Vec<(std::ffi::OsString, PathBuf)> = Vec::new();
        let read = fs::read_dir(&src_dir)
            .map_err(|e| TreeManifestError::Io(format!("read_dir {}: {e}", src_dir.display())))?;
        for entry in read {
            let entry = entry.map_err(|e| {
                TreeManifestError::Io(format!("read_dir {}: {e}", src_dir.display()))
            })?;
            children.push((entry.file_name(), entry.path()));
        }
        children.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, src_path) in children {
            count += 1;
            if count > max_entries {
                return Err(TreeManifestError::Oversized(format!(
                    "tree manifest copy exceeds {max_entries} entries"
                )));
            }
            let name = name.into_string().map_err(|bad| {
                TreeManifestError::Malformed(format!(
                    "tree manifest copy entry name {bad:?} is not valid UTF-8"
                ))
            })?;
            let rel = join_relative(&prefix, &name);
            if rel.len() > MAX_TREE_MANIFEST_PATH_BYTES {
                return Err(TreeManifestError::Oversized(format!(
                    "tree manifest copy path of {} bytes exceeds MAX_TREE_MANIFEST_PATH_BYTES ({MAX_TREE_MANIFEST_PATH_BYTES})",
                    rel.len()
                )));
            }
            if crate::atomic::is_internal_temp_name(&name) {
                // A concurrent in-flight daemon temp is never part of a
                // materialized copy (parity with `copy_tree_skip`).
                continue;
            }
            let dst_path = dst_dir.join(&name);
            let meta = fs::symlink_metadata(&src_path).map_err(|e| {
                TreeManifestError::Io(format!("metadata {}: {e}", src_path.display()))
            })?;
            let file_type = meta.file_type();
            if file_type.is_symlink() {
                let target = fs::read_link(&src_path).map_err(|e| {
                    TreeManifestError::Io(format!("read_link {}: {e}", src_path.display()))
                })?;
                let bytes = link_target_bytes(&target);
                if bytes.len() > MAX_TREE_MANIFEST_LINK_BYTES {
                    return Err(TreeManifestError::Oversized(format!(
                        "tree manifest copy symlink {rel:?} target of {} bytes exceeds MAX_TREE_MANIFEST_LINK_BYTES ({MAX_TREE_MANIFEST_LINK_BYTES})",
                        bytes.len()
                    )));
                }
                total = total.saturating_add(bytes.len() as u64);
                if total > max_total_bytes {
                    return Err(TreeManifestError::Oversized(format!(
                        "tree manifest copy of {rel:?} would exceed the {max_total_bytes}-byte total bound"
                    )));
                }
                create_symlink(&target, &dst_path)
                    .map_err(|e| TreeManifestError::Io(format!("symlink {rel:?}: {e}")))?;
                manifest.entries.push(TreeEntry {
                    normalized_path: rel,
                    kind: TreeEntryKind::Symlink,
                    mode: CanonicalMode::Symlink,
                    payload_digest: blake3::hash(&bytes).to_hex().to_string(),
                });
            } else if file_type.is_dir() {
                if skip_dirs.contains(&name.as_str()) {
                    continue;
                }
                fs::create_dir_all(&dst_path).map_err(|e| {
                    TreeManifestError::Io(format!("mkdir {}: {e}", dst_path.display()))
                })?;
                stack.push((src_path, dst_path, rel, depth + 1));
            } else if file_type.is_file() {
                let size = meta.len();
                total = total.saturating_add(size);
                if total > max_total_bytes {
                    return Err(TreeManifestError::Oversized(format!(
                        "tree manifest copy of {rel:?} would exceed the {max_total_bytes}-byte total bound"
                    )));
                }
                let file = fs::File::open(&src_path).map_err(|e| {
                    TreeManifestError::Io(format!("open {}: {e}", src_path.display()))
                })?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    let opened = file.metadata().map_err(|e| {
                        TreeManifestError::Io(format!("metadata {}: {e}", src_path.display()))
                    })?;
                    if meta.dev() != opened.dev() || meta.ino() != opened.ino() {
                        return Err(TreeManifestError::Malformed(format!(
                            "tree manifest copy entry {rel:?} changed identity between metadata and open (TOCTOU)"
                        )));
                    }
                }
                let (n, hash) = crate::copy_open_file(&file, &dst_path, Some(meta.permissions()))
                    .map_err(|e| TreeManifestError::Io(format!("copy {rel:?}: {e}")))?;
                if n != size {
                    return Err(TreeManifestError::Malformed(format!(
                        "tree manifest copy of {rel:?} changed size mid-copy ({n} of {size} bytes)"
                    )));
                }
                manifest.entries.push(TreeEntry {
                    normalized_path: rel,
                    kind: TreeEntryKind::Regular,
                    mode: canonical_mode(&meta),
                    payload_digest: hash.to_hex(),
                });
            } else {
                manifest.special_files.push(rel);
                return Err(TreeManifestError::SpecialFile {
                    paths: manifest.special_files,
                });
            }
        }
    }
    manifest
        .entries
        .sort_by(|a, b| a.normalized_path.cmp(&b.normalized_path));
    Ok(manifest)
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

fn join_relative(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

fn hash_reader(mut reader: impl Read) -> std::io::Result<String> {
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// The canonical mode of one regular file: the ANY-execute-bit projection
/// (`100755` when any of `0o111` is set, else `100644`).
fn canonical_mode(meta: &fs::Metadata) -> CanonicalMode {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 {
            CanonicalMode::ExecutableFile
        } else {
            CanonicalMode::RegularFile
        }
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        CanonicalMode::RegularFile
    }
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

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    // Windows must choose the link kind at creation time. The manifest
    // hashes the literal target either way; resolving here only PICKS the
    // API and a broken/looping link falls back to a file link.
    let resolved = if target.is_absolute() {
        target.to_path_buf()
    } else {
        link.parent().unwrap_or_else(|| Path::new(".")).join(target)
    };
    match fs::metadata(&resolved) {
        Ok(meta) if meta.is_dir() => std::os::windows::fs::symlink_dir(target, link),
        _ => std::os::windows::fs::symlink_file(target, link),
    }
}

#[cfg(not(any(unix, windows)))]
fn create_symlink(_target: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "symlink copy is unsupported on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn digest(root: &Path) -> String {
        tree_manifest_digest(root, MAX_TREE_MANIFEST_ENTRIES).unwrap()
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
