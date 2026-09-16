//! Canonical per-path entry state + commit-time CAS transitions.
//!
//! [`EntryState`] is the ONE per-path vocabulary of the runtime's tree
//! identity: absent, a regular file with its canonical git mode
//! (`100644`/`100755`) and whole-content BLAKE3 payload, or a symlink with
//! its LITERAL target bytes and the digest of those bytes. It is exactly the
//! per-entry projection of [`crate::tree_manifest`] (kind/mode/payload), so a
//! landing transaction can compare, apply and roll back one path against the
//! same identity the whole-tree `tm1:` digest folds — never bytes alone.
//!
//! Two CAS primitives perform every kind/mode transition atomically and never
//! follow a final symlink:
//!
//! * [`apply_tree_entry_cas`] — the expected state must still hold (or the
//!   candidate already holds, an idempotent replay); the path is moved to the
//!   candidate state;
//! * [`restore_tree_entry_cas`] — the CURRENT state must be the transaction's
//!   written state (a later user edit is preserved, never clobbered) and the
//!   path is moved back to the base state.
//!
//! The candidate/base payload bytes are supplied by the caller (the durable
//! CAS / the candidate root) and are re-verified against the state's digest
//! before any mutation, so a state/digest drift is a typed refusal. Regular
//! files are published through temp + fsync + rename (or an exclusive hard
//! link for a create); symlinks are created as literal links through temp +
//! rename; a mode-only transition goes through the atomic permission path
//! (open with `O_NOFOLLOW` + fchmod); `Absent` is a verified removal. On
//! unix, `rename(2)` over a symlink replaces the LINK itself — the target is
//! never touched.

use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use faktor_core::error::{Error, ErrorKind};
use faktor_core::hash::FileHash;

use crate::tree_manifest::{CanonicalMode, TreeEntry, TreeEntryKind, MAX_TREE_MANIFEST_LINK_BYTES};
use crate::MAX_MERGE_FILE_BYTES;

/// The canonical state of ONE path: absence, a regular file (canonical mode +
/// whole-content digest) or a symlink (LITERAL target bytes + their digest,
/// never resolved).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryState {
    Absent,
    Regular {
        mode: CanonicalMode,
        payload: FileHash,
    },
    Symlink {
        /// The literal target bytes (never followed, never resolved).
        target: Vec<u8>,
        /// `blake3(target)`.
        target_digest: FileHash,
    },
}

impl EntryState {
    pub const fn absent() -> Self {
        EntryState::Absent
    }

    pub const fn is_absent(&self) -> bool {
        matches!(self, EntryState::Absent)
    }

    pub const fn is_regular(&self) -> bool {
        matches!(self, EntryState::Regular { .. })
    }

    pub const fn is_symlink(&self) -> bool {
        matches!(self, EntryState::Symlink { .. })
    }

    /// The entry kind, or `None` for [`EntryState::Absent`].
    pub const fn kind(&self) -> Option<TreeEntryKind> {
        match self {
            EntryState::Absent => None,
            EntryState::Regular { .. } => Some(TreeEntryKind::Regular),
            EntryState::Symlink { .. } => Some(TreeEntryKind::Symlink),
        }
    }

    /// The canonical mode, or `None` for [`EntryState::Absent`].
    pub const fn mode(&self) -> Option<CanonicalMode> {
        match self {
            EntryState::Absent => None,
            EntryState::Regular { mode, .. } => Some(*mode),
            EntryState::Symlink { .. } => Some(CanonicalMode::Symlink),
        }
    }

    /// The payload digest (file content for regular, literal target for
    /// symlink), or `None` for [`EntryState::Absent`].
    pub const fn payload_digest(&self) -> Option<FileHash> {
        match self {
            EntryState::Absent => None,
            EntryState::Regular { payload, .. } => Some(*payload),
            EntryState::Symlink { target_digest, .. } => Some(*target_digest),
        }
    }

    /// The lowercase hex payload digest, or `None` for [`EntryState::Absent`].
    pub fn payload_digest_hex(&self) -> Option<String> {
        self.payload_digest().map(FileHash::to_hex)
    }

    /// The literal symlink target, or `None` for another state.
    pub fn target_bytes(&self) -> Option<&[u8]> {
        match self {
            EntryState::Symlink { target, .. } => Some(target),
            _ => None,
        }
    }

    /// A regular state; `Symlink` is not a regular mode and is refused.
    pub fn regular(mode: CanonicalMode, payload: FileHash) -> Result<Self, Error> {
        if mode == CanonicalMode::Symlink {
            return Err(Error::malformed(
                "a regular entry cannot carry the symlink canonical mode",
            ));
        }
        Ok(EntryState::Regular { mode, payload })
    }

    /// A symlink state from its LITERAL target bytes (bounded).
    pub fn symlink(target: Vec<u8>) -> Result<Self, Error> {
        if target.len() > MAX_TREE_MANIFEST_LINK_BYTES {
            return Err(Error::oversized(format!(
                "symlink target of {} bytes exceeds MAX_TREE_MANIFEST_LINK_BYTES ({MAX_TREE_MANIFEST_LINK_BYTES})",
                target.len()
            )));
        }
        let target_digest = FileHash::from(blake3::hash(&target).into());
        Ok(EntryState::Symlink {
            target,
            target_digest,
        })
    }

    /// The canonical state of one REGULAR manifest entry. A symlink manifest
    /// entry carries only the digest of its literal target — the target
    /// bytes themselves are not in the manifest — so it is a typed refusal:
    /// callers materialize symlink states from the file itself
    /// ([`state_of_path`]).
    pub fn from_tree_entry(entry: &TreeEntry) -> Result<Self, Error> {
        let digest = FileHash::from_hex(&entry.payload_digest).ok_or_else(|| {
            Error::malformed(format!(
                "manifest entry {:?} carries a non-canonical payload digest",
                entry.normalized_path
            ))
        })?;
        match entry.kind {
            TreeEntryKind::Regular => Self::regular(entry.mode, digest),
            TreeEntryKind::Symlink => Err(Error::malformed(format!(
                "manifest symlink entry {:?} carries only its target digest; the literal target must be materialized from the path",
                entry.normalized_path
            ))),
        }
    }

    /// Bounded diagnostic spelling (never content bytes).
    pub fn describe(&self) -> String {
        match self {
            EntryState::Absent => "absent".to_string(),
            EntryState::Regular { mode, payload } => {
                format!("regular:{:o}:{}", mode.octal(), payload.to_hex())
            }
            EntryState::Symlink { target_digest, .. } => {
                format!("symlink:120000:{}", target_digest.to_hex())
            }
        }
    }

    /// True when `material` is the payload this state names: whole content
    /// for a regular file, the literal target bytes for a symlink.
    pub fn material_matches(&self, material: &[u8]) -> bool {
        match self {
            EntryState::Absent => false,
            EntryState::Regular { payload, .. } => {
                FileHash::from(blake3::hash(material).into()) == *payload
            }
            EntryState::Symlink { target, .. } => material == target.as_slice(),
        }
    }
}

impl serde::Serialize for EntryState {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        match self {
            EntryState::Absent => {
                let mut state = serializer.serialize_struct("EntryState", 1)?;
                state.serialize_field("state", "absent")?;
                state.end()
            }
            EntryState::Regular { mode, payload } => {
                let mut state = serializer.serialize_struct("EntryState", 3)?;
                state.serialize_field("state", "regular")?;
                state.serialize_field("mode", &mode.octal())?;
                state.serialize_field("payload", &payload.to_hex())?;
                state.end()
            }
            EntryState::Symlink {
                target,
                target_digest,
            } => {
                let mut state = serializer.serialize_struct("EntryState", 3)?;
                state.serialize_field("state", "symlink")?;
                state.serialize_field("target", target)?;
                state.serialize_field("target_digest", &target_digest.to_hex())?;
                state.end()
            }
        }
    }
}

impl<'de> serde::Deserialize<'de> for EntryState {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as DeError;
        #[derive(serde::Deserialize)]
        struct Wire {
            state: String,
            #[serde(default)]
            mode: u32,
            #[serde(default)]
            payload: Option<String>,
            #[serde(default)]
            target: Option<Vec<u8>>,
            #[serde(default)]
            target_digest: Option<String>,
        }
        let wire = Wire::deserialize(deserializer)?;
        let hex_field = |value: Option<String>, what: &str| -> Result<FileHash, D::Error> {
            let value = value.ok_or_else(|| {
                D::Error::custom(format!("entry state is missing its {what} digest"))
            })?;
            FileHash::from_hex(&value)
                .ok_or_else(|| D::Error::custom(format!("hostile {what} digest {value:?}")))
        };
        match wire.state.as_str() {
            "absent" => Ok(EntryState::Absent),
            "regular" => {
                let mode = parse_canonical_mode(wire.mode)
                    .ok_or_else(|| D::Error::custom(format!("hostile mode {:o}", wire.mode)))?;
                if mode == CanonicalMode::Symlink {
                    return Err(D::Error::custom(
                        "a regular entry cannot carry the symlink canonical mode",
                    ));
                }
                Ok(EntryState::Regular {
                    mode,
                    payload: hex_field(wire.payload, "payload")?,
                })
            }
            "symlink" => {
                if wire.mode != 0 && wire.mode != CanonicalMode::Symlink.octal() {
                    return Err(D::Error::custom(format!(
                        "hostile symlink mode {:o}",
                        wire.mode
                    )));
                }
                let target = wire.target.ok_or_else(|| {
                    D::Error::custom("symlink entry state is missing its literal target")
                })?;
                if target.len() > MAX_TREE_MANIFEST_LINK_BYTES {
                    return Err(D::Error::custom(
                        "symlink target exceeds the manifest bound",
                    ));
                }
                let recorded = hex_field(wire.target_digest, "target")?;
                let actual = FileHash::from(blake3::hash(&target).into());
                if recorded != actual {
                    return Err(D::Error::custom(
                        "symlink target digest does not match the literal target bytes",
                    ));
                }
                Ok(EntryState::Symlink {
                    target,
                    target_digest: recorded,
                })
            }
            other => Err(D::Error::custom(format!(
                "hostile entry state kind {other:?}"
            ))),
        }
    }
}

fn parse_canonical_mode(octal: u32) -> Option<CanonicalMode> {
    match octal {
        0o100_644 => Some(CanonicalMode::RegularFile),
        0o100_755 => Some(CanonicalMode::ExecutableFile),
        0o120_000 => Some(CanonicalMode::Symlink),
        _ => None,
    }
}

// ------------------------------------------------------------- observation

fn io_failure(what: &str, path: &Path, e: std::io::Error) -> Error {
    Error::internal(format!("{what} {}: {e}", path.display()))
}

fn canonical_mode_of(meta: &fs::Metadata) -> CanonicalMode {
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

#[cfg(unix)]
fn literal_target_bytes(target: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    target.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn literal_target_bytes(target: &Path) -> Vec<u8> {
    target.to_string_lossy().into_owned().into_bytes()
}

#[cfg(unix)]
fn os_string_from_bytes(bytes: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStringExt;
    OsString::from_vec(bytes.to_vec())
}

#[cfg(not(unix))]
fn os_string_from_bytes(bytes: &[u8]) -> OsString {
    OsString::from(String::from_utf8_lossy(bytes).into_owned())
}

/// The canonical state of ONE path, NEVER following a final symlink: a link
/// is read with `read_link` and hashed as its LITERAL target; a missing path
/// is [`EntryState::Absent`]; a special file (directory/FIFO/socket/device)
/// is a typed refusal because the canonical manifest cannot represent it.
pub fn state_of_path(path: &Path) -> Result<EntryState, Error> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(EntryState::Absent),
        Err(e) => return Err(io_failure("metadata", path, e)),
    };
    let file_type = meta.file_type();
    if file_type.is_symlink() {
        let target = fs::read_link(path).map_err(|e| io_failure("read_link", path, e))?;
        let bytes = literal_target_bytes(&target);
        return EntryState::symlink(bytes);
    }
    if file_type.is_file() {
        let file = fs::File::open(path).map_err(|e| io_failure("open", path, e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let opened = file
                .metadata()
                .map_err(|e| io_failure("metadata", path, e))?;
            if meta.dev() != opened.dev() || meta.ino() != opened.ino() {
                return Err(Error::conflict(format!(
                    "{} changed identity between metadata and open (TOCTOU)",
                    path.display()
                )));
            }
        }
        let payload = hash_reader(file).map_err(|e| io_failure("read", path, e))?;
        return EntryState::regular(canonical_mode_of(&meta), payload);
    }
    Err(Error::conflict(format!(
        "{} is a special file the canonical state cannot represent; refusing to touch it",
        path.display()
    )))
}

/// Resolve `rel` under `root` without ever following the FINAL component:
/// the parent chain is created (when asked) and canonicalized INSIDE the
/// canonical root, then the final name is joined literally, so a symlink at
/// the destination is operated on as a LINK.
pub fn confined_path(root: &Path, rel: &Path, create_parents: bool) -> Result<PathBuf, Error> {
    for component in rel.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(Error::malformed(format!(
                "path {rel:?} is not a plain relative path"
            )));
        }
    }
    let name = rel
        .file_name()
        .ok_or_else(|| Error::malformed(format!("path {rel:?} has no file name")))?;
    let root = root
        .canonicalize()
        .map_err(|e| Error::not_found(format!("root {}: {e}", root.display())))?;
    let parent_rel = rel.parent().unwrap_or(Path::new(""));
    let parent = if parent_rel.as_os_str().is_empty() {
        root.clone()
    } else {
        root.join(parent_rel)
    };
    if create_parents {
        fs::create_dir_all(&parent).map_err(|e| io_failure("mkdir", &parent, e))?;
    }
    let canon_parent = parent
        .canonicalize()
        .map_err(|e| Error::not_found(format!("parent {}: {e}", parent.display())))?;
    if !canon_parent.starts_with(&root) {
        return Err(Error::permission(format!(
            "path {rel:?} escapes root {}",
            root.display()
        )));
    }
    Ok(canon_parent.join(name))
}

/// The canonical state of `root/rel` (a missing root/parent is
/// [`EntryState::Absent`], a special file is a typed refusal).
pub fn state_at(root: &Path, rel: &Path) -> Result<EntryState, Error> {
    match confined_path(root, rel, false) {
        Ok(path) => state_of_path(&path),
        Err(e) if e.kind == ErrorKind::NotFound => Ok(EntryState::Absent),
        Err(e) => Err(e),
    }
}

fn hash_reader(mut reader: impl Read) -> std::io::Result<FileHash> {
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(FileHash::from(hasher.finalize().into()))
}

// ------------------------------------------------------------- transitions

/// One durable step of a transition, reported to an internal seam (the
/// production seam is a no-op; the fault-injection tests abort at chosen
/// steps to model a crash).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransitionStep {
    /// The staged temp (file or symlink) exists; the destination is untouched.
    Staged,
    /// The destination carries the new state (rename/hard link/chmod/remove
    /// completed); directory fsync may not have run yet.
    Published,
}

type Seam<'a> = &'a mut dyn FnMut(TransitionStep) -> Result<(), Error>;

fn unique_temp_path(parent: &Path, name: &str) -> PathBuf {
    parent.join(format!(
        ".{name}.kp-tmp-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ))
}

#[cfg(unix)]
fn raw_symlink(target: &OsString, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn raw_symlink(target: &OsString, link: &Path) -> std::io::Result<()> {
    // Windows must choose the link kind at creation time; the literal target
    // is stored either way and the canonical manifest never resolves it, so a
    // broken/looping target falls back to a file link.
    let target_path = Path::new(target);
    let resolved = if target_path.is_absolute() {
        target_path.to_path_buf()
    } else {
        link.parent()
            .unwrap_or_else(|| Path::new("."))
            .join(target_path)
    };
    match fs::metadata(&resolved) {
        Ok(meta) if meta.is_dir() => std::os::windows::fs::symlink_dir(target, link),
        _ => std::os::windows::fs::symlink_file(target, link),
    }
}

#[cfg(not(any(unix, windows)))]
fn raw_symlink(_target: &OsString, link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!("symlink creation at {} is unsupported", link.display()),
    ))
}

fn create_literal_symlink(target: &OsString, link: &Path) -> Result<(), Error> {
    match raw_symlink(target, link) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(Error::conflict(format!(
            "{} appeared since the base state; the entry was not overwritten",
            link.display()
        ))),
        Err(e) => Err(io_failure("symlink", link, e)),
    }
}

fn set_executable_bits(path: &Path, mode: CanonicalMode) -> Result<(), Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        // Open with O_NOFOLLOW so a swapped-in symlink can never redirect the
        // permission change to its target.
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .map_err(|e| io_failure("open", path, e))?;
        let mut bits = file
            .metadata()
            .map_err(|e| io_failure("metadata", path, e))?
            .permissions()
            .mode()
            & !0o111;
        if mode == CanonicalMode::ExecutableFile {
            bits |= 0o111;
        }
        file.set_permissions(fs::Permissions::from_mode(bits))
            .map_err(|e| io_failure("chmod", path, e))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

fn write_staged_file(tmp: &Path, payload: &[u8], mode: CanonicalMode) -> Result<(), Error> {
    use std::io::Write;
    let mut file = fs::File::create(tmp).map_err(|e| io_failure("create", tmp, e))?;
    file.write_all(payload)
        .map_err(|e| io_failure("write", tmp, e))?;
    file.flush().map_err(|e| io_failure("flush", tmp, e))?;
    file.sync_all().map_err(|e| io_failure("fsync", tmp, e))?;
    drop(file);
    set_executable_bits(tmp, mode)
}

fn payload_for(state: &EntryState, material: &[u8]) -> Result<(), Error> {
    match state {
        EntryState::Absent => Ok(()),
        EntryState::Regular { payload, .. } => {
            if material.len() as u64 > MAX_MERGE_FILE_BYTES {
                return Err(Error::oversized(format!(
                    "entry payload of {} bytes exceeds MAX_MERGE_FILE_BYTES ({MAX_MERGE_FILE_BYTES})",
                    material.len()
                )));
            }
            let found = FileHash::from(blake3::hash(material).into());
            if found != *payload {
                return Err(Error::conflict(format!(
                    "entry material drifted from its recorded state (expected {}, found {})",
                    payload.to_hex(),
                    found.to_hex()
                )));
            }
            Ok(())
        }
        EntryState::Symlink { target, .. } => {
            if material != target.as_slice() {
                return Err(Error::conflict(
                    "symlink material is not the recorded literal target",
                ));
            }
            Ok(())
        }
    }
}

/// Perform the state change `from -> to` at `dst` (the caller has already
/// proved the live state equals `from`). `material` is the payload of `to`
/// (whole content / literal target). Never follows a final symlink.
fn transition(
    dst: &Path,
    from: &EntryState,
    to: &EntryState,
    material: &[u8],
    seam: Seam<'_>,
) -> Result<(), Error> {
    payload_for(to, material)?;
    let parent = dst
        .parent()
        .ok_or_else(|| Error::malformed(format!("{} has no parent", dst.display())))?;
    let name = dst
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match (from, to) {
        (_, EntryState::Absent) => {
            fs::remove_file(dst).map_err(|e| io_failure("remove", dst, e))?;
            seam(TransitionStep::Published)?;
            crate::atomic::fsync_parent(parent);
            Ok(())
        }
        (EntryState::Absent, EntryState::Regular { mode, .. }) => {
            // Exclusive create: the staged temp is hard-linked, and link(2)
            // fails atomically when the destination was taken meanwhile.
            let tmp = unique_temp_path(parent, &name);
            if let Err(e) = write_staged_file(&tmp, material, *mode) {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
            seam(TransitionStep::Staged)?;
            let linked = fs::hard_link(&tmp, dst);
            let _ = fs::remove_file(&tmp);
            match linked {
                Ok(()) => {
                    seam(TransitionStep::Published)?;
                    crate::atomic::fsync_parent(parent);
                    Ok(())
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    Err(Error::conflict(format!(
                        "{} appeared since the base state; the entry was not overwritten",
                        dst.display()
                    )))
                }
                Err(e) => Err(io_failure("link", dst, e)),
            }
        }
        (EntryState::Absent, EntryState::Symlink { target, .. }) => {
            let os_target = os_string_from_bytes(target);
            create_literal_symlink(&os_target, dst)?;
            seam(TransitionStep::Published)?;
            crate::atomic::fsync_parent(parent);
            Ok(())
        }
        (EntryState::Regular { .. }, EntryState::Regular { mode, payload })
            if from.payload_digest() == Some(*payload) =>
        {
            // Mode-only transition: the atomic permission path.
            set_executable_bits(dst, *mode)?;
            seam(TransitionStep::Published)?;
            Ok(())
        }
        (EntryState::Symlink { .. }, EntryState::Symlink { target, .. })
        | (EntryState::Regular { .. }, EntryState::Symlink { target, .. }) => {
            let tmp = unique_temp_path(parent, &name);
            let os_target = os_string_from_bytes(target);
            if let Err(e) = create_literal_symlink(&os_target, &tmp) {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
            seam(TransitionStep::Staged)?;
            // rename over a symlink replaces the LINK itself, never the target.
            fs::rename(&tmp, dst).map_err(|e| {
                let _ = fs::remove_file(&tmp);
                io_failure("rename", dst, e)
            })?;
            seam(TransitionStep::Published)?;
            crate::atomic::fsync_parent(parent);
            Ok(())
        }
        (
            EntryState::Symlink { .. } | EntryState::Regular { .. },
            EntryState::Regular { mode, .. },
        ) => {
            let tmp = unique_temp_path(parent, &name);
            if let Err(e) = write_staged_file(&tmp, material, *mode) {
                let _ = fs::remove_file(&tmp);
                return Err(e);
            }
            seam(TransitionStep::Staged)?;
            fs::rename(&tmp, dst).map_err(|e| {
                let _ = fs::remove_file(&tmp);
                io_failure("rename", dst, e)
            })?;
            seam(TransitionStep::Published)?;
            crate::atomic::fsync_parent(parent);
            Ok(())
        }
    }
}

fn verify_final(dst: &Path, to: &EntryState) -> Result<(), Error> {
    let live = state_of_path(dst)?;
    if live != *to {
        return Err(Error::internal(format!(
            "post-transition state of {} is {} but the target state is {}",
            dst.display(),
            live.describe(),
            to.describe()
        )));
    }
    Ok(())
}

/// Commit-time CAS apply of one canonical entry state:
///
/// * the live state must still equal `expected` (or already equal `next`, an
///   idempotent replay → [`crate::CasMergeResult::AlreadyCurrent`]);
/// * a kind change (regular <-> symlink), a symlink retarget and a content
///   change publish through temp + rename; a create is an exclusive hard
///   link/literal symlink; a mode-only change goes through the atomic
///   permission path; `Absent` is a verified removal;
/// * a final symlink is NEVER followed and the parent may not escape `root`.
///
/// `material` is `next`'s payload (whole content for a regular file, the
/// literal target for a symlink) and is re-verified against the state digest
/// before any mutation; it is unused when `next` is [`EntryState::Absent`].
pub fn apply_tree_entry_cas(
    root: &Path,
    rel: &Path,
    expected: &EntryState,
    next: &EntryState,
    material: &[u8],
) -> Result<crate::CasMergeResult, Error> {
    apply_tree_entry_cas_inner(root, rel, expected, next, material, &mut |_| Ok(()))
}

pub(crate) fn apply_tree_entry_cas_inner(
    root: &Path,
    rel: &Path,
    expected: &EntryState,
    next: &EntryState,
    material: &[u8],
    seam: Seam<'_>,
) -> Result<crate::CasMergeResult, Error> {
    let dst = match confined_path(root, rel, !next.is_absent()) {
        Ok(path) => path,
        // The whole parent chain is missing: the live state is Absent, so an
        // absent target already holds and a non-absent expectation conflicts.
        Err(e) if e.kind == ErrorKind::NotFound => {
            if next.is_absent() {
                return Ok(crate::CasMergeResult::AlreadyCurrent);
            }
            return Err(Error::conflict(format!(
                "cas mismatch at {rel:?}: the parent chain is missing but the target state is {}",
                next.describe()
            )));
        }
        Err(e) => return Err(e),
    };
    let current = state_of_path(&dst)?;
    if current == *next {
        return Ok(crate::CasMergeResult::AlreadyCurrent);
    }
    if current != *expected {
        return Err(Error::conflict(format!(
            "cas mismatch at {}: expected {}, found {}; nothing was applied",
            dst.display(),
            expected.describe(),
            current.describe()
        )));
    }
    transition(&dst, &current, next, material, seam)?;
    verify_final(&dst, next)?;
    Ok(crate::CasMergeResult::Applied)
}

/// Commit-time CAS restore of one transaction's written state back to its
/// base state:
///
/// * the live state must still equal `current` (the transaction's written
///   state) — a later user edit is a typed Conflict and is never clobbered;
/// * the live state already equal to `base` is an idempotent replay
///   ([`crate::CasMergeResult::AlreadyCurrent`]);
/// * otherwise the exact kind/mode/target transition is performed with the
///   base payload material (the durable rollback blob / literal target).
pub fn restore_tree_entry_cas(
    root: &Path,
    rel: &Path,
    current: &EntryState,
    base: &EntryState,
    material: &[u8],
) -> Result<crate::CasMergeResult, Error> {
    restore_tree_entry_cas_inner(root, rel, current, base, material, &mut |_| Ok(()))
}

pub(crate) fn restore_tree_entry_cas_inner(
    root: &Path,
    rel: &Path,
    current: &EntryState,
    base: &EntryState,
    material: &[u8],
    seam: Seam<'_>,
) -> Result<crate::CasMergeResult, Error> {
    let dst = match confined_path(root, rel, !base.is_absent()) {
        Ok(path) => path,
        Err(e) if e.kind == ErrorKind::NotFound => {
            if base.is_absent() {
                return Ok(crate::CasMergeResult::AlreadyCurrent);
            }
            return Err(Error::conflict(format!(
                "rollback refused at {rel:?}: the parent chain is missing but the base state is {}",
                base.describe()
            )));
        }
        Err(e) => return Err(e),
    };
    let live = state_of_path(&dst)?;
    if live == *base {
        return Ok(crate::CasMergeResult::AlreadyCurrent);
    }
    if live != *current {
        return Err(Error::conflict(format!(
            "rollback refused at {}: the live state {} is neither the transaction state {} nor the base {} (a later edit is preserved)",
            dst.display(),
            live.describe(),
            current.describe(),
            base.describe()
        )));
    }
    if current == base {
        return Ok(crate::CasMergeResult::AlreadyCurrent);
    }
    transition(&dst, current, base, material, seam)?;
    verify_final(&dst, base)?;
    Ok(crate::CasMergeResult::Applied)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn regular(bytes: &[u8], mode: CanonicalMode) -> EntryState {
        EntryState::regular(mode, FileHash::from(blake3::hash(bytes).into())).unwrap()
    }

    #[cfg(unix)]
    fn write_file(root: &Path, rel: &str, bytes: &[u8], mode: u32) -> PathBuf {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    fn live(root: &Path, rel: &str) -> EntryState {
        state_at(root, Path::new(rel)).unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn chmod_only_transition_0644_to_0755_and_back() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_file(root, "tool.sh", b"#!/bin/sh\n", 0o644);
        let plain = regular(b"#!/bin/sh\n", CanonicalMode::RegularFile);
        let exec = regular(b"#!/bin/sh\n", CanonicalMode::ExecutableFile);
        assert_eq!(live(root, "tool.sh"), plain);
        assert_eq!(
            apply_tree_entry_cas(root, Path::new("tool.sh"), &plain, &exec, b"#!/bin/sh\n")
                .unwrap(),
            crate::CasMergeResult::Applied
        );
        assert_eq!(live(root, "tool.sh"), exec);
        assert_ne!(
            fs::metadata(root.join("tool.sh"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0
        );
        assert_eq!(
            apply_tree_entry_cas(root, Path::new("tool.sh"), &exec, &plain, b"#!/bin/sh\n")
                .unwrap(),
            crate::CasMergeResult::Applied
        );
        assert_eq!(live(root, "tool.sh"), plain);
        // Read/write/setuid bits are not part of the canonical mode: 0600
        // still satisfies the 100644 state and no exec bit is added.
        fs::set_permissions(root.join("tool.sh"), fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            live(root, "tool.sh"),
            plain,
            "setuid/read/write bits do not move the canonical state"
        );
    }

    #[cfg(unix)]
    #[test]
    fn regular_to_symlink_and_back_with_identical_resolved_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_file(root, "data.txt", b"same", 0o644);
        write_file(root, "other.txt", b"same", 0o644);
        write_file(root, "f", b"same", 0o644);
        let file = regular(b"same", CanonicalMode::RegularFile);
        let link_a = EntryState::symlink(b"data.txt".to_vec()).unwrap();
        let link_b = EntryState::symlink(b"other.txt".to_vec()).unwrap();
        assert_ne!(
            link_a, link_b,
            "equal resolved bytes with different literal targets are DIFFERENT states"
        );
        // regular -> symlink: the file's bytes are replaced by a link.
        assert_eq!(
            apply_tree_entry_cas(root, Path::new("f"), &file, &link_a, b"data.txt").unwrap(),
            crate::CasMergeResult::Applied
        );
        assert!(fs::symlink_metadata(root.join("f"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read_link(root.join("f")).unwrap(),
            PathBuf::from("data.txt")
        );
        assert_eq!(fs::read(root.join("f")).unwrap(), b"same");
        // The target file itself was never touched.
        assert!(!fs::symlink_metadata(root.join("data.txt"))
            .unwrap()
            .file_type()
            .is_symlink());
        // symlink -> symlink retarget through temp + rename.
        assert_eq!(
            apply_tree_entry_cas(root, Path::new("f"), &link_a, &link_b, b"other.txt").unwrap(),
            crate::CasMergeResult::Applied
        );
        assert_eq!(
            fs::read_link(root.join("f")).unwrap(),
            PathBuf::from("other.txt")
        );
        // A stale expectation (link_a) now conflicts; nothing changes.
        let err = apply_tree_entry_cas(root, Path::new("f"), &link_a, &file, b"same").unwrap_err();
        assert_eq!(err.kind, ErrorKind::Conflict);
        assert!(err.message.contains("cas mismatch"), "{err:?}");
        assert_eq!(live(root, "f"), link_b);
        // symlink -> regular: rename over the link replaces the LINK.
        assert_eq!(
            apply_tree_entry_cas(root, Path::new("f"), &link_b, &file, b"same").unwrap(),
            crate::CasMergeResult::Applied
        );
        assert!(!fs::symlink_metadata(root.join("f"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(root.join("f")).unwrap(), b"same");
    }

    #[cfg(unix)]
    #[test]
    fn broken_and_looping_symlinks_are_states_not_errors() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let broken = EntryState::symlink(b"nowhere/at/all".to_vec()).unwrap();
        let looping = EntryState::symlink(b"loop".to_vec()).unwrap();
        assert_eq!(
            apply_tree_entry_cas(
                root,
                Path::new("dangling"),
                &EntryState::Absent,
                &broken,
                b"nowhere/at/all"
            )
            .unwrap(),
            crate::CasMergeResult::Applied
        );
        assert_eq!(live(root, "dangling"), broken);
        assert!(
            fs::read(root.join("dangling")).is_err(),
            "the link is broken"
        );
        assert_eq!(
            apply_tree_entry_cas(
                root,
                Path::new("loop"),
                &EntryState::Absent,
                &looping,
                b"loop"
            )
            .unwrap(),
            crate::CasMergeResult::Applied
        );
        assert_eq!(live(root, "loop"), looping);
        // Retargeting a broken link to a different broken target is still a
        // first-class transition.
        let other = EntryState::symlink(b"still/nowhere".to_vec()).unwrap();
        assert_eq!(
            apply_tree_entry_cas(
                root,
                Path::new("dangling"),
                &broken,
                &other,
                b"still/nowhere"
            )
            .unwrap(),
            crate::CasMergeResult::Applied
        );
        assert_eq!(live(root, "dangling"), other);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_never_followed_by_the_cas_primitives() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let outside = dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret"), b"outside-content").unwrap();
        // A symlink INSIDE the root pointing outside; applying over it must
        // replace the LINK, never write through it.
        std::os::unix::fs::symlink(outside.join("secret"), root.join("link")).unwrap();
        let link_state =
            EntryState::symlink(literal_target_bytes(&outside.join("secret"))).unwrap();
        let next = regular(b"inside", CanonicalMode::RegularFile);
        apply_tree_entry_cas(root, Path::new("link"), &link_state, &next, b"inside").unwrap();
        assert_eq!(
            fs::read(outside.join("secret")).unwrap(),
            b"outside-content"
        );
        assert_eq!(fs::read(root.join("link")).unwrap(), b"inside");
        // Removing a symlink removes the link, never the target.
        apply_tree_entry_cas(root, Path::new("link"), &next, &EntryState::Absent, b"").unwrap();
        assert!(!root.join("link").exists());
        assert_eq!(
            fs::read(outside.join("secret")).unwrap(),
            b"outside-content"
        );
    }

    #[test]
    fn absent_create_and_remove_are_exclusive_and_verified() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let entry = regular(b"payload", CanonicalMode::RegularFile);
        assert_eq!(
            apply_tree_entry_cas(
                root,
                Path::new("new.txt"),
                &EntryState::Absent,
                &entry,
                b"payload"
            )
            .unwrap(),
            crate::CasMergeResult::Applied
        );
        assert_eq!(live(root, "new.txt"), entry);
        // A second exclusive create conflicts, never clobbers.
        let err = apply_tree_entry_cas(
            root,
            Path::new("new.txt"),
            &EntryState::Absent,
            &regular(b"other", CanonicalMode::RegularFile),
            b"other",
        )
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Conflict);
        assert_eq!(fs::read(root.join("new.txt")).unwrap(), b"payload");
        // Remove requires the exact current state.
        let err = apply_tree_entry_cas(
            root,
            Path::new("new.txt"),
            &regular(b"drifted", CanonicalMode::RegularFile),
            &EntryState::Absent,
            b"",
        )
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Conflict);
        assert!(root.join("new.txt").exists());
        assert_eq!(
            apply_tree_entry_cas(root, Path::new("new.txt"), &entry, &EntryState::Absent, b"")
                .unwrap(),
            crate::CasMergeResult::Applied
        );
        assert_eq!(live(root, "new.txt"), EntryState::Absent);
        // Idempotent replay: the target state already holds.
        assert_eq!(
            apply_tree_entry_cas(root, Path::new("new.txt"), &entry, &EntryState::Absent, b"")
                .unwrap(),
            crate::CasMergeResult::AlreadyCurrent
        );
        // Creating nested parents works and is confined.
        assert_eq!(
            apply_tree_entry_cas(
                root,
                Path::new("a/b/c.txt"),
                &EntryState::Absent,
                &regular(b"deep", CanonicalMode::RegularFile),
                b"deep"
            )
            .unwrap(),
            crate::CasMergeResult::Applied
        );
        assert_eq!(
            live(root, "a/b/c.txt"),
            regular(b"deep", CanonicalMode::RegularFile)
        );
    }

    #[test]
    fn apply_and_restore_mismatches_are_typed_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("f"), b"base").unwrap();
        let base = regular(b"base", CanonicalMode::RegularFile);
        let next = regular(b"next", CanonicalMode::RegularFile);
        let wrong = regular(b"wrong", CanonicalMode::RegularFile);
        // expected=wrong while the live state is base and the target is next:
        // a typed CAS mismatch, nothing applied.
        let err = apply_tree_entry_cas(root, Path::new("f"), &wrong, &next, b"next").unwrap_err();
        assert_eq!(err.kind, ErrorKind::Conflict);
        assert!(err.message.contains("cas mismatch"), "{err:?}");
        assert_eq!(fs::read(root.join("f")).unwrap(), b"base");
        // restore with a live state that is neither the transaction state nor
        // the base state: typed refusal (a later edit is preserved).
        let candidate = regular(b"candidate", CanonicalMode::RegularFile);
        let other_base = regular(b"other-base", CanonicalMode::RegularFile);
        let err =
            restore_tree_entry_cas(root, Path::new("f"), &candidate, &other_base, b"other-base")
                .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Conflict);
        assert!(err.message.contains("rollback refused"), "{err:?}");
        assert_eq!(fs::read(root.join("f")).unwrap(), b"base");
        // Payload material that does not match the recorded state digest is a
        // typed refusal BEFORE any mutation.
        let err = apply_tree_entry_cas(root, Path::new("f"), &base, &next, b"forged-material")
            .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Conflict);
        assert_eq!(fs::read(root.join("f")).unwrap(), b"base");
        // Idempotent replay is NOT a conflict: the target already holds.
        assert_eq!(
            apply_tree_entry_cas(root, Path::new("f"), &wrong, &base, b"base").unwrap(),
            crate::CasMergeResult::AlreadyCurrent
        );
    }

    #[test]
    fn restore_never_clobbers_a_later_user_edit() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("f"), b"base").unwrap();
        let base = regular(b"base", CanonicalMode::RegularFile);
        let candidate = regular(b"candidate", CanonicalMode::RegularFile);
        apply_tree_entry_cas(root, Path::new("f"), &base, &candidate, b"candidate").unwrap();
        // The user edits the landed file.
        fs::write(root.join("f"), b"user-edit").unwrap();
        let err =
            restore_tree_entry_cas(root, Path::new("f"), &candidate, &base, b"base").unwrap_err();
        assert_eq!(err.kind, ErrorKind::Conflict);
        assert_eq!(fs::read(root.join("f")).unwrap(), b"user-edit");
        // The untouched case restores exactly.
        fs::write(root.join("f"), b"candidate").unwrap();
        assert_eq!(
            restore_tree_entry_cas(root, Path::new("f"), &candidate, &base, b"base").unwrap(),
            crate::CasMergeResult::Applied
        );
        assert_eq!(live(root, "f"), base);
    }

    #[cfg(unix)]
    #[test]
    fn restore_restores_exact_kind_mode_and_target() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let base = EntryState::symlink(b"target-a".to_vec()).unwrap();
        apply_tree_entry_cas(
            root,
            Path::new("l"),
            &EntryState::Absent,
            &base,
            b"target-a",
        )
        .unwrap();
        let landed = regular(b"content", CanonicalMode::ExecutableFile);
        apply_tree_entry_cas(root, Path::new("l"), &base, &landed, b"content").unwrap();
        assert_eq!(
            restore_tree_entry_cas(root, Path::new("l"), &landed, &base, b"target-a").unwrap(),
            crate::CasMergeResult::Applied
        );
        assert_eq!(live(root, "l"), base);
        assert_eq!(
            fs::read_link(root.join("l")).unwrap(),
            PathBuf::from("target-a")
        );
        // Mode-only rollback (executable -> regular) restores the exact mode.
        let exec = regular(b"same", CanonicalMode::ExecutableFile);
        fs::write(root.join("m"), b"same").unwrap();
        fs::set_permissions(root.join("m"), fs::Permissions::from_mode(0o755)).unwrap();
        let plain = regular(b"same", CanonicalMode::RegularFile);
        apply_tree_entry_cas(root, Path::new("m"), &exec, &plain, b"same").unwrap();
        assert_eq!(
            restore_tree_entry_cas(root, Path::new("m"), &plain, &exec, b"same").unwrap(),
            crate::CasMergeResult::Applied
        );
        assert_eq!(live(root, "m"), exec);
    }

    #[test]
    fn confinement_refuses_traversal_and_parent_escape() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir_all(&root).unwrap();
        let outside = dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret"), b"x").unwrap();
        let err = apply_tree_entry_cas(
            &root,
            Path::new("../outside/secret"),
            &EntryState::Absent,
            &regular(b"evil", CanonicalMode::RegularFile),
            b"evil",
        )
        .unwrap_err();
        assert_eq!(err.kind, ErrorKind::Malformed);
        assert_eq!(fs::read(outside.join("secret")).unwrap(), b"x");
        #[cfg(unix)]
        {
            // A parent symlink that escapes the root is refused.
            std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
            let err = apply_tree_entry_cas(
                &root,
                Path::new("escape/new"),
                &EntryState::Absent,
                &regular(b"evil", CanonicalMode::RegularFile),
                b"evil",
            )
            .unwrap_err();
            assert_eq!(err.kind, ErrorKind::Permission);
            assert!(!outside.join("new").exists());
        }
    }

    #[test]
    fn deserialization_refuses_hostile_states() {
        let bad_mode: Result<EntryState, _> =
            serde_json::from_str(r#"{"state":"regular","mode":420,"payload":"aa"}"#);
        assert!(bad_mode.is_err());
        let bad_hex: Result<EntryState, _> = serde_json::from_str(&format!(
            r#"{{"state":"regular","mode":33188,"payload":"{}"}}"#,
            "z".repeat(64)
        ));
        assert!(bad_hex.is_err());
        let mismatched: Result<EntryState, _> = serde_json::from_str(
            r#"{"state":"symlink","target":[116,97,114,103,101,116],"target_digest":"0000000000000000000000000000000000000000000000000000000000000000"}"#,
        );
        assert!(
            mismatched.is_err(),
            "a symlink digest that contradicts its literal target is refused"
        );
        let absent: EntryState = serde_json::from_str(r#"{"state":"absent"}"#).unwrap();
        assert_eq!(absent, EntryState::Absent);
        let good = EntryState::symlink(b"target".to_vec()).unwrap();
        let round: EntryState =
            serde_json::from_str(&serde_json::to_string(&good).unwrap()).unwrap();
        assert_eq!(round, good);
    }

    // ------------------------------------------------ crash-after-every-step
    //
    // The transition is a sequence of durable steps. A crash at ANY seam must
    // leave either the old whole state or the new whole state (never a torn
    // mix, never a surviving temp at the address), and re-running the SAME
    // CAS must converge (AlreadyCurrent when the new state already landed,
    // Applied when it did not) — the recovery contract of a crashed landing
    // transaction.

    fn crash_at(
        root: &Path,
        rel: &Path,
        expected: &EntryState,
        next: &EntryState,
        material: &[u8],
        at: TransitionStep,
    ) -> Result<crate::CasMergeResult, Error> {
        let mut aborted = false;
        let mut seam = |step: TransitionStep| {
            if step == at && !aborted {
                aborted = true;
                return Err(Error::internal("modeled crash"));
            }
            Ok(())
        };
        apply_tree_entry_cas_inner(root, rel, expected, next, material, &mut seam)
    }

    #[cfg(unix)]
    #[test]
    fn crash_at_every_transition_seam_recovers_without_torn_state() {
        let shapes: Vec<(&str, EntryState, EntryState, Vec<u8>)> = vec![
            (
                "create",
                EntryState::Absent,
                regular(b"one", CanonicalMode::RegularFile),
                b"one".to_vec(),
            ),
            (
                "replace",
                regular(b"base", CanonicalMode::RegularFile),
                regular(b"next", CanonicalMode::ExecutableFile),
                b"next".to_vec(),
            ),
            (
                "chmod-only",
                regular(b"base", CanonicalMode::RegularFile),
                regular(b"base", CanonicalMode::ExecutableFile),
                b"base".to_vec(),
            ),
            (
                "to-symlink",
                regular(b"base", CanonicalMode::RegularFile),
                EntryState::symlink(b"target-x".to_vec()).unwrap(),
                b"target-x".to_vec(),
            ),
            (
                "from-symlink",
                EntryState::symlink(b"target-x".to_vec()).unwrap(),
                regular(b"base", CanonicalMode::RegularFile),
                b"base".to_vec(),
            ),
            (
                "retarget",
                EntryState::symlink(b"target-x".to_vec()).unwrap(),
                EntryState::symlink(b"target-y".to_vec()).unwrap(),
                b"target-y".to_vec(),
            ),
            (
                "remove",
                regular(b"base", CanonicalMode::RegularFile),
                EntryState::Absent,
                Vec::new(),
            ),
        ];
        for at in [TransitionStep::Staged, TransitionStep::Published] {
            for (name, expected, next, material) in &shapes {
                let dir = tempfile::tempdir().unwrap();
                let root = dir.path();
                // Materialize the starting state at the address.
                match expected {
                    EntryState::Absent => {}
                    EntryState::Regular { mode, .. } => {
                        let path = root.join("f");
                        fs::write(&path, b"base").unwrap();
                        fs::set_permissions(
                            &path,
                            fs::Permissions::from_mode(if *mode == CanonicalMode::ExecutableFile {
                                0o755
                            } else {
                                0o644
                            }),
                        )
                        .unwrap();
                    }
                    EntryState::Symlink { target, .. } => {
                        std::os::unix::fs::symlink(
                            String::from_utf8(target.clone()).unwrap(),
                            root.join("f"),
                        )
                        .unwrap();
                    }
                }
                assert_eq!(&live(root, "f"), expected);
                let _ = crash_at(root, Path::new("f"), expected, next, material, at);
                // Whatever the seam aborted, the residue is a whole state: the
                // seam sits before its next step, so a `Staged` crash leaves
                // the OLD state and a `Published` crash leaves the NEW one.
                let residue = live(root, "f");
                assert!(
                    residue == *expected || residue == *next,
                    "{name} @ {at:?}: torn state {residue:?}"
                );
                // The seam aborts BEFORE its next step, so a `Published`
                // crash always leaves the NEW state on disk.
                if at == TransitionStep::Published {
                    assert_eq!(residue, *next, "{name} @ {at:?}: published residue");
                }
                // Recovery replay of the SAME CAS converges.
                let recovered =
                    apply_tree_entry_cas(root, Path::new("f"), expected, next, material).unwrap();
                assert!(
                    matches!(
                        recovered,
                        crate::CasMergeResult::Applied | crate::CasMergeResult::AlreadyCurrent
                    ),
                    "{name} @ {at:?}: replay must converge"
                );
                assert_eq!(live(root, "f"), *next, "{name} @ {at:?}: converged");
                // A genuine crash may leave an orphan `.kp-tmp-*` residue, but
                // it is INVISIBLE to the canonical manifest (daemon
                // bookkeeping, exactly like `.git`): the root digest must not
                // move, so a crashed landing's equality proof still holds.
                let digest_with_residue =
                    crate::tree_manifest::tree_manifest_digest(root, 10_000).unwrap();
                let mut swept = 0usize;
                for entry in fs::read_dir(root).unwrap().flatten() {
                    let n = entry.file_name().to_string_lossy().into_owned();
                    if n.contains(".kp-tmp-") {
                        fs::remove_file(entry.path()).unwrap();
                        swept += 1;
                    }
                }
                let digest_after_sweep =
                    crate::tree_manifest::tree_manifest_digest(root, 10_000).unwrap();
                assert_eq!(
                    digest_with_residue, digest_after_sweep,
                    "{name} @ {at:?}: orphan temps must not move the canonical digest ({swept} swept)"
                );
            }
        }
    }

    #[test]
    fn no_temp_is_left_behind_by_repeated_replaces() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("f"), b"v0").unwrap();
        let mut current = regular(b"v0", CanonicalMode::RegularFile);
        for i in 1..40u32 {
            let bytes = format!("v{i}").into_bytes();
            let next = regular(&bytes, CanonicalMode::RegularFile);
            apply_tree_entry_cas(root, Path::new("f"), &current, &next, &bytes).unwrap();
            current = next;
        }
        let names: Vec<String> = fs::read_dir(root)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["f".to_string()], "temp leaked: {names:?}");
    }
}
