//! The immutable-version install layout, its stable bootstrap launcher and
//! the launch-time authentication of the activated release.
//!
//! ```text
//! <install_root>/
//!   launcher                                  stable bootstrap (small Faktor
//!                                             binary copied once; never
//!                                             replaced by an update)
//!   current                                   pointer: names
//!                                             versions/<release-id>/
//!   trusted-keys.json                         launch trust anchor (the
//!                                             operator key allowlist)
//!   versions/<release-id>/
//!     faktor                                  the exact release binary
//!     manifest                                the SIGNED `faktor-update/v1`
//!                                             manifest that authorized it
//!   artifacts/ + staging/                      the legacy content-addressed
//!                                             store (kept for the legacy
//!                                             apply flow)
//! ```
//!
//! The pointer is only a NAME. Authentication is entirely in the signed
//! manifest stored NEXT TO the binary: the bootstrap launcher reads the
//! pointer, loads `versions/<release-id>/manifest`, verifies the ed25519
//! signature against `trusted-keys.json` (an unsigned or unknown-key or
//! tampered manifest is refused), requires the pointer's digest to appear in
//! the signed manifest, re-hashes `versions/<release-id>/faktor`, refuses a
//! digest mismatch, and only then execs the exact bytes it verified. A
//! pointer swapped by anything other than the updater therefore cannot make
//! the launcher run an unauthenticated binary.
//!
//! A release directory is only ever VISIBLE complete: `materialize_release`
//! assembles the signed manifest plus the exact binary in a hidden temp
//! directory beside the final one and adopts the whole directory with one
//! rename, so a crash can never leave a manifest-less `versions/<id>/faktor`
//! that a pointer could name. `release_id_if_materialized` requires both
//! files (typed refusal otherwise), and `resolve_launch` refuses an
//! incomplete directory typed instead of treating it as a launchable
//! release.
//!
//! On non-unix the launcher proves readiness from the CHILD's own output
//! before detaching: the child's digest attestation line
//! (`faktor release digest=<64 hex>`) must equal the digest re-verified
//! immediately before the spawn AND the frozen startup line must arrive on
//! the same child pipe. A child that never proves both — or attests another
//! digest — is a failed launch: it is terminated and the launcher exits
//! non-zero with a typed refusal (never a silent `Ok(0)`).
//!
//! Honest limit: the digest re-hash and the exec are two syscalls; a writer
//! with write access to the version directory could race between them. The
//! install root is operator-owned (0700-style) state, and activation only
//! ever writes into content-addressed version directories, so this is the
//! documented race window, not a silent trust path.
//!
//! `verify_manifest_at_launch` deliberately does NOT re-check the validity
//! window or the channel pin: those govern update SELECTION at check time.
//! Re-checking them at launch would let an expired manifest brick an install
//! that is already running. Signature, key allowlist and digest binding are
//! always enforced.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::UpdateError;
use crate::install::{
    file_digest, InstallLayout, LAUNCHER_FILE_NAME, RELEASE_BINARY_NAME, RELEASE_MANIFEST_NAME,
    TRUST_FILE_NAME, VERSIONS_DIR_NAME,
};
use crate::keys::{TrustedKey, TrustedKeys};
use crate::manifest;

/// Schema tag of the install trust anchor file.
pub const TRUST_FILE_SCHEMA: &str = "faktor-install-trust/v1";
/// Environment variable carrying the activated release id into the launched
/// process (set by the bootstrap launcher AFTER verification).
pub const RELEASE_ID_ENV: &str = "FAKTOR_RELEASE_ID";
/// Environment variable carrying the activated release binary digest.
pub const RELEASE_DIGEST_ENV: &str = "FAKTOR_RELEASE_DIGEST";
/// Environment variable carrying the install root into the launched process.
pub const INSTALL_ROOT_ENV: &str = "FAKTOR_INSTALL_ROOT";
/// Environment override that forces bootstrap mode for one process (the
/// value is the install root). The VS Code launcher uses the `launcher`
/// file name instead; this is the explicit/test seam.
pub const LAUNCHER_ROOT_ENV: &str = "FAKTOR_LAUNCHER_INSTALL_ROOT";

/// The stable release id for one (version, binary digest) pair. This is the
/// single derivation both the packaging scripts and the updater use.
pub fn release_id_for(version: &str, digest: &str) -> String {
    let short = digest.get(..12).unwrap_or(digest);
    format!("{version}-{short}")
}

/// Validate one release id used as a directory name: bounded ASCII, no path
/// separators, no traversal, never empty or `.`/`..`.
pub fn validate_release_id(release_id: &str) -> Result<(), UpdateError> {
    let invalid = |detail: String| Err(UpdateError::Install(detail));
    if release_id.is_empty() || release_id.len() > 160 {
        return invalid("release id must be 1..=160 bytes".into());
    }
    if !release_id.is_ascii() {
        return invalid("release id must be ASCII".into());
    }
    if release_id.contains('/')
        || release_id.contains('\\')
        || release_id.contains("..")
        || release_id == "."
    {
        return invalid(format!(
            "release id {release_id:?} is not a plain directory name"
        ));
    }
    if !release_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-'))
    {
        return invalid(format!(
            "release id {release_id:?} contains a character outside [A-Za-z0-9._+-]"
        ));
    }
    Ok(())
}

/// One allowlisted operator identity as stored in the trust anchor file
/// (the same `{id, public_key}` shape the `[updater]` config section uses).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustEntry {
    pub id: String,
    pub public_key: String,
}

/// The launch trust anchor written by the updater next to the layout: the
/// operator key allowlist the bootstrap launcher verifies release manifests
/// against. An absent or empty anchor refuses every launch (there is no
/// implicit trust anchor), exactly like the update path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustFile {
    pub schema: String,
    pub keys: Vec<TrustEntry>,
}

impl TrustFile {
    pub fn from_keys(keys: &TrustedKeys) -> Self {
        TrustFile {
            schema: TRUST_FILE_SCHEMA.to_string(),
            keys: keys
                .identities()
                .into_iter()
                .filter_map(|identity| keys.get(identity))
                .map(|key| TrustEntry {
                    id: key.identity().to_string(),
                    public_key: key.public_key_base64(),
                })
                .collect(),
        }
    }

    pub fn to_keys(&self) -> Result<TrustedKeys, UpdateError> {
        if self.schema != TRUST_FILE_SCHEMA {
            return Err(UpdateError::Install(format!(
                "install trust anchor schema {:?} != {TRUST_FILE_SCHEMA:?}",
                self.schema
            )));
        }
        let mut keys = Vec::with_capacity(self.keys.len());
        for entry in &self.keys {
            let key = TrustedKey::from_base64(&entry.id, &entry.public_key)
                .map_err(|e| UpdateError::Install(format!("install trust anchor: {e}")))?;
            keys.push(key);
        }
        TrustedKeys::new(keys)
            .map_err(|e| UpdateError::Install(format!("install trust anchor: {e}")))
    }

    /// Read the anchor at `path`. A MISSING file is an empty allowlist (the
    /// launcher then refuses every release as an unknown key — never an
    /// implicit trust anchor); a present-but-malformed file is a typed
    /// install error, never silently ignored.
    pub fn read(path: &Path) -> Result<TrustedKeys, UpdateError> {
        match fs::read(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(TrustedKeys::empty()),
            Err(e) => Err(UpdateError::Install(format!(
                "read install trust anchor {}: {e}",
                path.display()
            ))),
            Ok(bytes) => {
                let parsed: TrustFile = serde_json::from_slice(&bytes).map_err(|e| {
                    UpdateError::Install(format!(
                        "install trust anchor {} is not valid: {e}",
                        path.display()
                    ))
                })?;
                parsed.to_keys()
            }
        }
    }
}

impl InstallLayout {
    /// The trust anchor path (the operator key allowlist for launches).
    pub fn trust_path(&self) -> PathBuf {
        self.root().join(TRUST_FILE_NAME)
    }

    /// The stable bootstrap launcher path.
    pub fn launcher_path(&self) -> PathBuf {
        self.root().join(LAUNCHER_FILE_NAME)
    }

    /// The immutable release directories (`versions/<release-id>/`).
    pub fn versions_dir(&self) -> PathBuf {
        self.root().join(VERSIONS_DIR_NAME)
    }

    pub fn release_dir(&self, release_id: &str) -> PathBuf {
        self.versions_dir().join(release_id)
    }

    /// The exact release binary of one release id.
    pub fn release_binary(&self, release_id: &str) -> PathBuf {
        self.release_dir(release_id).join(RELEASE_BINARY_NAME)
    }

    /// The signed release manifest stored next to the binary.
    pub fn release_manifest(&self, release_id: &str) -> PathBuf {
        self.release_dir(release_id).join(RELEASE_MANIFEST_NAME)
    }

    /// The release id derived from one (version, digest) pair WHEN the
    /// immutable directory is materialized COMPLETELY with exactly those
    /// bytes: `versions/<id>/faktor` hashes to `digest` AND
    /// `versions/<id>/manifest` is present and non-empty.
    ///
    /// `Ok(None)` = the legacy layout: the install root carries no
    /// `versions/` directory at all, so callers keep the legacy pointer shape
    /// untouched. Once the immutable layout exists the answer is exhaustive:
    /// a matching COMPLETE release, or a typed error naming the
    /// incomplete/corrupt directory. A digest/IO error is NEVER silently
    /// flattened into "not materialized" (that used to drop the release
    /// identity from `current` while the install still reported Applied), and
    /// a manifest-less crash residue is refused typed — never adopted, never
    /// silently degraded to the legacy pointer shape the bootstrap cannot
    /// launch.
    pub fn release_id_if_materialized(
        &self,
        version: &str,
        digest: &str,
    ) -> Result<Option<String>, UpdateError> {
        if digest.len() < 12 {
            return Ok(None);
        }
        let release_id = release_id_for(version, digest);
        if validate_release_id(&release_id).is_err() {
            return Ok(None);
        }
        let binary = self.release_binary(&release_id);
        let manifest = self.release_manifest(&release_id);
        let manifest_present = fs::symlink_metadata(&manifest)
            .map(|meta| meta.file_type().is_file() && meta.len() > 0)
            .unwrap_or(false);
        let binary_meta = match fs::symlink_metadata(&binary) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if manifest_present {
                    return Err(UpdateError::Install(format!(
                        "release {release_id} is incomplete: {} exists but the release binary is \
                         missing",
                        manifest.display()
                    )));
                }
                if self.versions_dir().is_dir() {
                    return Err(UpdateError::Install(format!(
                        "release {release_id} is not materialized under {}; the immutable layout \
                         exists, so a release identity may not silently fall back to the legacy \
                         pointer shape",
                        self.release_dir(&release_id).display()
                    )));
                }
                return Ok(None);
            }
            Err(e) => {
                return Err(UpdateError::Install(format!(
                    "release binary {} is unreadable: {e}",
                    binary.display()
                )))
            }
        };
        if !binary_meta.file_type().is_file() {
            return Err(UpdateError::Install(format!(
                "release binary {} is not a regular file (symlinks/directories are refused)",
                binary.display()
            )));
        }
        let actual = file_digest(&binary).map_err(|e| UpdateError::StagedArtifactUnusable {
            artifact: format!("{release_id}/{RELEASE_BINARY_NAME}"),
            detail: e.to_string(),
        })?;
        if actual != digest {
            return Err(UpdateError::StagedArtifactUnusable {
                artifact: format!("{release_id}/{RELEASE_BINARY_NAME}"),
                detail: format!(
                    "release binary digest {actual} != the requested digest {digest}; the immutable \
                     directory is corrupt or holds a different artifact"
                ),
            });
        }
        if !manifest_present {
            return Err(UpdateError::Install(format!(
                "release {release_id} is incomplete: the signed release manifest {} is missing or \
                 empty; refusing to name a release the bootstrap launcher cannot authenticate",
                manifest.display()
            )));
        }
        Ok(Some(release_id))
    }

    /// Publish one release immutably: `versions/<release-id>/faktor` from the
    /// content-addressed artifact plus `versions/<release-id>/manifest` (the
    /// SIGNED update manifest bytes that authorized those exact bytes). The
    /// artifact digest is re-checked here; a mismatch is refused before a
    /// version directory exists.
    ///
    /// Completeness is atomic: the release is assembled in a hidden temp
    /// directory beside the final directory (manifest first, then the exact
    /// binary bytes, every file fsynced before the directory rename), and only
    /// the whole directory is renamed into place — a crash at ANY point leaves
    /// either the previous state or the COMPLETE release, never a
    /// manifest-less `versions/<id>/faktor` that the bootstrap launcher would
    /// refuse. An existing INCOMPLETE/corrupt directory is rebuilt the same
    /// way (temp built first, then replaced), and stale atomic-temp residue
    /// of this release under either spelling (`.faktor-tmp-*` / legacy
    /// `.kp-tmp-*`) is cleared. Idempotent: an already-materialized release
    /// with the identical binary and manifest is kept; a complete directory
    /// whose signed manifest bytes changed is re-signed in place (atomic file
    /// replace — it is already complete).
    pub fn materialize_release(
        &self,
        release_id: &str,
        artifact_name: &str,
        digest: &str,
        manifest_bytes: &[u8],
    ) -> Result<PathBuf, UpdateError> {
        validate_release_id(release_id)?;
        if manifest_bytes.is_empty() {
            return Err(UpdateError::Install(
                "release manifest bytes are empty; refusing to materialize an unauthenticated \
                 release"
                    .into(),
            ));
        }
        let artifact = self.artifact_path(artifact_name, digest);
        let actual = file_digest(&artifact).map_err(|e| UpdateError::StagedArtifactUnusable {
            artifact: artifact_name.to_string(),
            detail: e.to_string(),
        })?;
        if actual != digest {
            return Err(UpdateError::StagedArtifactUnusable {
                artifact: artifact_name.to_string(),
                detail: format!("artifact content digest {actual} != {digest}"),
            });
        }
        let versions = self.versions_dir();
        fs::create_dir_all(&versions).map_err(|e| {
            UpdateError::Install(format!("create versions dir {}: {e}", versions.display()))
        })?;
        let dir = self.release_dir(release_id);
        let binary = self.release_binary(release_id);
        let manifest_path = self.release_manifest(release_id);
        if dir.is_dir() {
            let binary_regular = fs::symlink_metadata(&binary)
                .map(|meta| meta.file_type().is_file())
                .unwrap_or(false);
            let binary_current = binary_regular
                && file_digest(&binary).map_err(|e| UpdateError::StagedArtifactUnusable {
                    artifact: format!("{release_id}/{RELEASE_BINARY_NAME}"),
                    detail: e.to_string(),
                })? == digest;
            let manifest_current = fs::symlink_metadata(&manifest_path)
                .map(|meta| meta.file_type().is_file())
                .unwrap_or(false)
                && fs::read(&manifest_path)
                    .map(|existing| existing == manifest_bytes)
                    .unwrap_or(false);
            if binary_current && manifest_current {
                self.clear_stale_release_temps(release_id);
                return Ok(dir);
            }
            if binary_current {
                // The immutable binary is already exact and only the signed
                // manifest bytes differ (re-signed material): replace the file
                // atomically; the directory stays complete throughout.
                faktor_fs::atomic::atomic_replace(&manifest_path, manifest_bytes).map_err(|e| {
                    UpdateError::Install(format!(
                        "write release manifest {}: {e}",
                        manifest_path.display()
                    ))
                })?;
                faktor_fs::atomic::fsync_parent(&dir);
                self.clear_stale_release_temps(release_id);
                return Ok(dir);
            }
        }
        // Build the COMPLETE release in a hidden temp directory beside the
        // final one (same filesystem, so the adopt is one rename). The
        // manifest is written first; a crash leaves only temp residue, which
        // is never a release id callers can derive and is cleared below.
        let tmp = versions.join(format!(
            ".{release_id}.faktor-tmp-{}-{}",
            std::process::id(),
            unique_nonce()
        ));
        if let Err(e) = fs::create_dir_all(&tmp) {
            return Err(UpdateError::Install(format!(
                "create release staging dir {}: {e}",
                tmp.display()
            )));
        }
        let build = write_complete_release_dir(&tmp, &artifact, digest, manifest_bytes);
        if let Err(e) = build {
            return Err(match remove_temp_dir(&tmp) {
                None => e,
                Some(note) => UpdateError::Install(format!("{e}{note}")),
            });
        }
        // Adopt: a previously existing but incomplete/corrupt directory is
        // removed ONLY now that the replacement is fully built and durable, so
        // a crash leaves either the old directory or the new complete one.
        let adopt = (|| -> Result<(), UpdateError> {
            if dir.exists() {
                fs::remove_dir_all(&dir).map_err(|e| {
                    UpdateError::Install(format!(
                        "remove incomplete release dir {}: {e}",
                        dir.display()
                    ))
                })?;
            }
            faktor_fs::atomic::atomic_adopt_dir(&tmp, &dir).map_err(|e| {
                UpdateError::Install(format!(
                    "publish release dir {} -> {}: {e}",
                    tmp.display(),
                    dir.display()
                ))
            })
        })();
        if let Err(e) = adopt {
            return Err(match remove_temp_dir(&tmp) {
                None => e,
                Some(note) => UpdateError::Install(format!("{e}{note}")),
            });
        }
        self.clear_stale_release_temps(release_id);
        Ok(dir)
    }

    /// Remove stale atomic-temp build residue of one release (a crash during
    /// [`InstallLayout::materialize_release`] leaves only hidden temp
    /// directories). BOTH spellings are recognized — the current
    /// `.{id}.faktor-tmp-*` and legacy `.{id}.kp-tmp-*` crash residue — so an
    /// upgrade over an older installation never leaves unrecognized temp
    /// directories behind. A cleanup failure is logged, never silently
    /// discarded; it can never make the just-published release incomplete.
    fn clear_stale_release_temps(&self, release_id: &str) {
        let prefix = format!(".{release_id}.");
        let entries = match fs::read_dir(self.versions_dir()) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!(
                    "release materialization: could not scan {} for stale temp residue: {e}",
                    self.versions_dir().display()
                );
                return;
            }
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !name.starts_with(&prefix) || !faktor_fs::atomic::is_internal_temp_name(name) {
                continue;
            }
            if let Some(note) = remove_temp_dir(&entry.path()) {
                tracing::warn!("release materialization{note}");
            }
        }
    }

    /// Write (atomically) the launch trust anchor for the configured key
    /// allowlist. Written BEFORE a pointer can name a release, so a launched
    /// bootstrap always has its anchor in place.
    pub fn write_trust(&self, keys: &TrustedKeys) -> Result<(), UpdateError> {
        let anchor = TrustFile::from_keys(keys);
        let bytes = serde_json::to_vec_pretty(&anchor).map_err(|e| {
            UpdateError::Install(format!("install trust anchor serialization: {e}"))
        })?;
        faktor_fs::atomic::atomic_replace(&self.trust_path(), &bytes)
            .map(|_| ())
            .map_err(|e| {
                UpdateError::Install(format!(
                    "write install trust anchor {}: {e}",
                    self.trust_path().display()
                ))
            })
    }

    /// Install the STABLE bootstrap launcher from one source binary (the
    /// running CLI). Existing launchers are never replaced: the bootstrap
    /// must survive every later release, and a launcher that has to be
    /// updated by the update it launches is not a bootstrap. A missing
    /// launcher is copied and fsynced through the same atomic discipline.
    pub fn install_launcher(&self, source: &Path) -> Result<PathBuf, UpdateError> {
        let dest = self.launcher_path();
        if dest.is_file() {
            return Ok(dest);
        }
        let source_meta = fs::metadata(source).map_err(|e| {
            UpdateError::Install(format!("launcher source {}: {e}", source.display()))
        })?;
        if !source_meta.is_file() {
            return Err(UpdateError::Install(format!(
                "launcher source {} is not a regular file",
                source.display()
            )));
        }
        let tmp = self.root().join(format!(
            ".{LAUNCHER_FILE_NAME}.faktor-tmp-{}-{}",
            std::process::id(),
            unique_nonce()
        ));
        if let Err(e) = copy_file(source, &tmp) {
            let _ = fs::remove_file(&tmp);
            return Err(e);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if let Err(e) = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755)) {
                let _ = fs::remove_file(&tmp);
                return Err(UpdateError::Install(format!(
                    "chmod {}: {e}",
                    tmp.display()
                )));
            }
        }
        if let Err(e) = faktor_fs::atomic::atomic_adopt(&tmp, &dest) {
            let _ = fs::remove_file(&tmp);
            return Err(UpdateError::Install(format!(
                "publish bootstrap launcher {}: {e}",
                dest.display()
            )));
        }
        Ok(dest)
    }
}

/// Remove one temp release path (a directory, or a leftover file from an
/// older layout). A failure is returned as a note the caller attaches to the
/// primary error, never discarded.
fn remove_temp_dir(tmp: &Path) -> Option<String> {
    if fs::symlink_metadata(tmp).is_err() {
        return None;
    }
    match fs::remove_dir_all(tmp).or_else(|_| fs::remove_file(tmp)) {
        Ok(()) => None,
        Err(e) => Some(format!(
            "; additionally the temp path {} could not be removed: {e}",
            tmp.display()
        )),
    }
}

/// Stream-copy one file (bounded 64 KiB buffer, no unbounded RAM) into a
/// caller-chosen temp path; the caller adopts/fsyncs it.
fn copy_file(source: &Path, dest: &Path) -> Result<(), UpdateError> {
    use std::io::Write as _;
    let mut input = fs::File::open(source)
        .map_err(|e| UpdateError::Install(format!("open {}: {e}", source.display())))?;
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)
        .map_err(|e| UpdateError::Install(format!("create {}: {e}", dest.display())))?;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = std::io::Read::read(&mut input, &mut buffer)
            .map_err(|e| UpdateError::Install(format!("read {}: {e}", source.display())))?;
        if read == 0 {
            break;
        }
        output
            .write_all(&buffer[..read])
            .map_err(|e| UpdateError::Install(format!("write {}: {e}", dest.display())))?;
    }
    Ok(())
}

/// Write the COMPLETE content of one release into an already-created temp
/// directory: the signed manifest first, then the exact binary bytes with the
/// executable bit, each file fsynced, then the directory entry itself. The
/// caller adopts the whole directory with one rename, so the intermediate
/// states are unreachable as a release.
fn write_complete_release_dir(
    dir: &Path,
    artifact: &Path,
    digest: &str,
    manifest_bytes: &[u8],
) -> Result<(), UpdateError> {
    use std::io::Write as _;
    let manifest_path = dir.join(RELEASE_MANIFEST_NAME);
    let mut manifest = fs::File::create(&manifest_path)
        .map_err(|e| UpdateError::Install(format!("create {}: {e}", manifest_path.display())))?;
    manifest
        .write_all(manifest_bytes)
        .map_err(|e| UpdateError::Install(format!("write {}: {e}", manifest_path.display())))?;
    manifest
        .sync_all()
        .map_err(|e| UpdateError::Install(format!("fsync {}: {e}", manifest_path.display())))?;
    drop(manifest);

    let binary = dir.join(RELEASE_BINARY_NAME);
    copy_file(artifact, &binary)?;
    let actual = file_digest(&binary).map_err(|e| UpdateError::StagedArtifactUnusable {
        artifact: format!("{}/{}", dir.display(), RELEASE_BINARY_NAME),
        detail: e.to_string(),
    })?;
    if actual != digest {
        return Err(UpdateError::StagedArtifactUnusable {
            artifact: format!("{}/{}", dir.display(), RELEASE_BINARY_NAME),
            detail: format!("materialized binary digest {actual} != {digest}"),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))
            .map_err(|e| UpdateError::Install(format!("chmod {}: {e}", binary.display())))?;
    }
    fs::File::open(&binary)
        .and_then(|file| file.sync_all())
        .map_err(|e| UpdateError::Install(format!("fsync {}: {e}", binary.display())))?;
    faktor_fs::atomic::fsync_parent(dir);
    Ok(())
}

fn unique_nonce() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// One verified launch target: the exact binary path the bootstrap will
/// exec, bound to the authenticated pointer and signed manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReleaseTarget {
    pub release_id: String,
    pub version: String,
    pub channel: String,
    pub digest: String,
    pub binary: PathBuf,
}

/// The paths the bootstrap resolves from: the install root and the trust
/// anchor file (default `trusted-keys.json` inside the root).
#[derive(Debug, Clone)]
pub struct LaunchInputs {
    pub install_root: PathBuf,
    pub trust_path: PathBuf,
}

impl LaunchInputs {
    pub fn new(install_root: impl Into<PathBuf>) -> Self {
        let root = install_root.into();
        let trust_path = root.join(TRUST_FILE_NAME);
        LaunchInputs {
            install_root: root,
            trust_path,
        }
    }
}

/// Resolve (and authenticate) the release the pointer names:
///
/// 1. read the pointer; absent → refusal; no `release_id` → refusal (a
///    legacy pointer cannot be launched through the bootstrap);
/// 2. read the trust anchor; an empty allowlist refuses everything;
/// 3. refuse an INCOMPLETE release typed (a missing/empty manifest is crash
///    residue of a pre-atomic materialization, never a launchable release);
/// 4. load `versions/<release-id>/manifest` and verify the ed25519 signature
///    against the anchor (unsigned/unknown-key/key-mismatch/tampered are
///    distinct typed refusals);
/// 5. bind pointer ↔ manifest: version, channel and the pointer's digest
///    must all appear in the signed document;
/// 6. `faktor` must be a regular file whose content digest equals the
///    pointer's (and therefore the manifest's) digest.
pub fn resolve_launch(
    inputs: &LaunchInputs,
    keys: &TrustedKeys,
) -> Result<ReleaseTarget, UpdateError> {
    let layout = InstallLayout::open_readonly(inputs.install_root.clone());
    let pointer = layout
        .read_pointer()?
        .ok_or_else(|| UpdateError::LaunchRefused {
            detail: format!(
                "no install pointer at {} (nothing has been activated)",
                layout.pointer_path().display()
            ),
        })?;
    let release_id = pointer.release_id.clone().ok_or_else(|| UpdateError::LaunchRefused {
        detail: "the install pointer carries no release id (legacy pointer); the bootstrap launcher only runs authenticated releases".into(),
    })?;
    validate_release_id(&release_id)?;
    if keys.is_empty() {
        return Err(UpdateError::LaunchRefused {
            detail: format!(
                "the install trust anchor {} is absent or empty; refusing to launch an unauthenticated release",
                inputs.trust_path.display()
            ),
        });
    }
    // Completeness first: a release directory the updater never finished (a
    // binary with no signed manifest) is refused typed with the incompleteness
    // named, never mislabeled as a missing/tampered manifest.
    let manifest_path = layout.release_manifest(&release_id);
    let binary_path = layout.release_binary(&release_id);
    let manifest_meta =
        fs::symlink_metadata(&manifest_path).map_err(|e| UpdateError::LaunchRefused {
            detail: format!(
            "release {release_id} is incomplete: the signed manifest {} is missing ({e}); refusing \
             to launch",
            manifest_path.display()
        ),
        })?;
    if !manifest_meta.file_type().is_file() || manifest_meta.len() == 0 {
        return Err(UpdateError::LaunchRefused {
            detail: format!(
                "release {release_id} is incomplete: the signed manifest {} is empty or not a \
                 regular file; refusing to launch",
                manifest_path.display()
            ),
        });
    }
    let binary_meta =
        fs::symlink_metadata(&binary_path).map_err(|e| UpdateError::LaunchRefused {
            detail: format!(
            "release {release_id} is incomplete: the release binary {} is missing ({e}); refusing \
             to launch",
            binary_path.display()
        ),
        })?;
    if !binary_meta.file_type().is_file() {
        return Err(UpdateError::LaunchRefused {
            detail: format!(
                "release binary {} is not a regular file (symlinks/directories are refused)",
                binary_path.display()
            ),
        });
    }
    let manifest_bytes = fs::read(&manifest_path).map_err(|e| UpdateError::LaunchRefused {
        detail: format!(
            "the signed release manifest {} is unreadable: {e}",
            manifest_path.display()
        ),
    })?;
    let signed = manifest::verify_manifest_at_launch(&manifest_bytes, keys)?;
    if signed.version != pointer.version {
        return Err(UpdateError::LaunchRefused {
            detail: format!(
                "the signed manifest names version {} but the pointer names {}",
                signed.version, pointer.version
            ),
        });
    }
    let manifest_channel = signed.channel.to_string();
    if manifest_channel != pointer.channel {
        return Err(UpdateError::LaunchRefused {
            detail: format!(
                "the signed manifest names channel {manifest_channel} but the pointer names {}",
                pointer.channel
            ),
        });
    }
    if !signed
        .artifacts
        .iter()
        .any(|artifact| artifact.sha256 == pointer.digest)
    {
        return Err(UpdateError::LaunchRefused {
            detail: format!(
                "the pointer digest {} does not appear in the signed manifest for release {release_id}",
                pointer.digest
            ),
        });
    }
    let binary = layout.release_binary(&release_id);
    let actual = file_digest(&binary).map_err(|e| UpdateError::LaunchRefused {
        detail: format!("hash {}: {e}", binary.display()),
    })?;
    if actual != pointer.digest {
        return Err(UpdateError::LaunchRefused {
            detail: format!(
                "release digest mismatch: the pointer names {} but {RELEASE_BINARY_NAME} hashes to {actual}",
                pointer.digest
            ),
        });
    }
    Ok(ReleaseTarget {
        release_id,
        version: pointer.version,
        channel: pointer.channel,
        digest: pointer.digest,
        binary,
    })
}

/// Launch one resolved target: re-verify the binary digest immediately
/// before the exec (the documented TOCTOU window is the exec syscall
/// itself), export the verified release identity, and replace this process.
///
/// On unix the bootstrap IS replaced by the release binary (execve), so the
/// supervisor observes exactly one live process and the release's own exit
/// code is the daemon's. On non-unix the child is spawned with a
/// launcher-owned stdout handshake pipe and:
///
/// - the launcher's `faktor release started pid=<pid> digest=<hex>` line is
///   written first; if THAT write fails the launch is refused typed and the
///   unannounced release is terminated — a launch that cannot be observed is
///   never silently detached;
/// - readiness is proven only by the child's OWN attestation: the frozen
///   child digest line `faktor release digest=<64 hex>` (its
///   `FAKTOR_RELEASE_DIGEST`) plus the frozen `faktor server listening on
///   http://127.0.0.1:<port>` startup line, both read from the SAME child
///   pipe. The attested digest must equal the digest re-verified immediately
///   before the spawn; a wrong/missing attestation is never a proof;
/// - an early exit inside [`LAUNCH_FORWARD_WINDOW_MS`] is forwarded exactly;
/// - a release that never proves readiness inside the window is a FAILED
///   launch: it is terminated (the window expires only when the child never
///   proved digest + readiness, so it owns no confirmed daemon) and the
///   failure is returned typed (the bootstrap exits non-zero and prints it).
pub fn launch(
    target: &ReleaseTarget,
    install_root: &Path,
    args: &[OsString],
) -> Result<i32, UpdateError> {
    let actual = file_digest(&target.binary).map_err(|e| UpdateError::LaunchRefused {
        detail: format!("hash {} before exec: {e}", target.binary.display()),
    })?;
    if actual != target.digest {
        return Err(UpdateError::LaunchRefused {
            detail: format!(
                "release digest changed between verification and exec: expected {}, found {actual}",
                target.digest
            ),
        });
    }
    let mut command = std::process::Command::new(&target.binary);
    command
        .args(args)
        .env(RELEASE_ID_ENV, &target.release_id)
        .env(RELEASE_DIGEST_ENV, &target.digest)
        .env(INSTALL_ROOT_ENV, install_root);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        let error = command.exec();
        Err(UpdateError::LaunchRefused {
            detail: format!("exec {}: {error}", target.binary.display()),
        })
    }
    #[cfg(not(unix))]
    {
        // The startup/health handshake: the release's stdout rides a
        // launcher-owned pipe so the launcher can observe the child's own
        // digest attestation + frozen startup line while still forwarding
        // every line to the supervisor.
        let (handshake_reader, handshake_writer) =
            std::io::pipe().map_err(|e| UpdateError::LaunchRefused {
                detail: format!("create the launch handshake pipe: {e}"),
            })?;
        command.stdout(handshake_writer);
        let child = command.spawn().map_err(|e| UpdateError::LaunchRefused {
            detail: format!("spawn {}: {e}", target.binary.display()),
        })?;
        let mut process = ProcessChild { child };
        // Frozen supervisor handshake: the REAL release pid and the digest
        // verified immediately above, written before any release line is
        // forwarded so a supervisor can keep tracking the daemon after this
        // launcher exits at readiness. A vanished supervisor is a typed
        // refusal to detach, never a silent success: the release is
        // terminated so no unannounced process is left behind.
        announce_release_started(&mut process, &mut std::io::stdout().lock(), &target.digest)?;
        let mut readiness = StdoutReadiness::start_with(handshake_reader, forward_stdout_line);
        let clock = MonotonicClock::new();
        let decision = run_launch_window(
            &mut process,
            &mut readiness,
            &clock,
            &target.digest,
            Duration::from_millis(LAUNCH_FORWARD_WINDOW_MS),
        )?;
        if decision == LaunchDecision::Ready {
            tracing::info!(
                "release {} proved readiness with digest {}; the launcher detaches",
                target.binary.display(),
                target.digest
            );
        }
        conclude_launch(&mut process, decision, &target.digest)
    }
}

/// Write the supervisor handshake line and, when it cannot be written,
/// terminate the just-spawned release and return a typed refusal to detach.
/// A launch whose tracking line never reached the supervisor owns no
/// observable daemon; detaching it anyway would leak an unannounced process
/// and report a success the supervisor cannot see.
#[cfg(any(not(unix), test))]
fn announce_release_started<C: LaunchChild>(
    child: &mut C,
    out: &mut impl std::io::Write,
    digest: &str,
) -> Result<(), UpdateError> {
    match write_release_started_line(out, child.pid(), digest) {
        Ok(()) => Ok(()),
        Err(e) => {
            let terminated = match child.terminate() {
                Ok(()) => "the unannounced release process was terminated".to_string(),
                Err(terminate_error) => {
                    format!("terminating it FAILED ({terminate_error}); it may still be running")
                }
            };
            Err(UpdateError::LaunchRefused {
                detail: format!(
                    "the release-started handshake could not be written ({e}); refusing to detach \
                     the unannounced release (pid {}): {terminated}",
                    child.pid()
                ),
            })
        }
    }
}

/// Turn one finished launch window into the launcher's exit:
///
/// - an exit observed inside the window is forwarded exactly (its status is
///   the release's);
/// - a readiness proof is the ONLY success: exit 0 and detach (the release
///   keeps running);
/// - a window that elapsed without a proof is a FAILED launch: the release
///   never proved digest + readiness, so it is terminated (zero-orphans: the
///   launcher owns the unconfirmed process) and a typed failure is returned.
#[cfg(any(not(unix), test))]
fn conclude_launch<C: LaunchChild>(
    child: &mut C,
    decision: LaunchDecision,
    expected_digest: &str,
) -> Result<i32, UpdateError> {
    match decision {
        LaunchDecision::Exited(code) => Ok(code),
        LaunchDecision::Ready => Ok(0),
        LaunchDecision::WindowElapsed => {
            let terminated = match child.terminate() {
                Ok(()) => "the unconfirmed release process was terminated".to_string(),
                Err(e) => format!("terminating it FAILED ({e}); it may still be running"),
            };
            Err(UpdateError::LaunchRefused {
                detail: format!(
                    "release process {} did not prove readiness with digest {expected_digest} \
                     within {LAUNCH_FORWARD_WINDOW_MS} ms; {terminated}",
                    child.pid()
                ),
            })
        }
    }
}

/// The bounded launch window of the non-unix bootstrap launcher: how long it
/// waits for a started release to prove digest + readiness (or to exit).
///
/// The window is exactly **30_000 ms (30 s)**. A healthy release ends it
/// immediately through the child's digest attestation + frozen startup line
/// (typically milliseconds after spawn); a slow failed startup is reported
/// within this bound. On expiry the launch is a FAILED launch (see
/// [`conclude_launch`]): the child never proved digest + readiness, so the
/// launcher terminates it and refuses typed.
///
/// IDE COORDINATION: this constant is public so supervisor-side startup waits
/// reference it instead of copying a number. Any IDE-side startup timeout
/// MUST exceed this window (the previous VS Code 10 s / JetBrains 20 s waits
/// were SHORTER and would kill the wrapper before the launcher could report
/// its verdict). The window must never grow to the daemon's lifetime.
pub const LAUNCH_FORWARD_WINDOW_MS: u64 = 30_000;

/// The launch-window poll cadence: bounded work (one child `try_wait` plus
/// one non-blocking handshake check) per interval while the window is open.
pub const LAUNCH_POLL_INTERVAL_MS: u64 = 100;

/// The readiness proof a started release presents to the non-unix launcher:
/// the child announced the frozen daemon startup line (readiness) AND its own
/// digest attestation line, and `digest` is the digest the CHILD attested
/// (never the launcher's expected value). `run_launch_window` accepts the
/// proof only when it equals the digest re-verified immediately before the
/// spawn; a differing attestation is logged and never detaches the launcher.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchProof {
    pub digest: String,
}

/// The outcome of one bounded launch window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchDecision {
    /// The child proved digest + readiness inside the window: the launcher
    /// detaches immediately and exits 0 without waiting for the process (no
    /// forwarding loop).
    Ready,
    /// The child exited inside the window before proving readiness: its
    /// status is forwarded to the supervisor.
    Exited(i32),
    /// The window closed with the child still running but UNCONFIRMED: the
    /// child never proved digest + readiness, so this is a FAILED launch.
    /// The caller MUST terminate the child and report a typed failure
    /// ([`conclude_launch`]); it is never a success and the window itself
    /// performs no further polling.
    WindowElapsed,
}

/// The started release as the launch window observes it.
pub trait LaunchChild {
    /// The exit code once the release has exited; `Ok(None)` while it runs.
    fn try_exit_code(&mut self) -> Result<Option<i32>, UpdateError>;
    /// The child's pid (the release process a supervisor must track).
    fn pid(&self) -> u32;
    /// Terminate the child because the launch contract was not met (the
    /// launcher has not detached): kill and reap it, bounded. After `Ok(())`
    /// the child is no longer running; an `Err` means it may still be
    /// running and is named in the launcher's typed refusal.
    fn terminate(&mut self) -> Result<(), UpdateError>;
}

/// The readiness/digest handshake as the launch window observes it.
pub trait LaunchReadiness {
    /// `Ok(Some(proof))` once the release proved digest + readiness;
    /// `Ok(None)` while no proof has arrived.
    fn try_proof(&mut self) -> Result<Option<LaunchProof>, UpdateError>;
}

/// The monotonic clock of the launch window.
pub trait LaunchClock {
    /// Monotonic milliseconds since this clock was created.
    fn now_ms(&self) -> u64;
    /// Sleep one poll interval (the window's only waiting primitive).
    fn sleep_ms(&self, ms: u64);
}

/// The platform-neutral launch-window decision: forward an exit observed
/// inside the window, return [`LaunchDecision::Ready`] the moment the child
/// proves its digest + readiness, and return
/// [`LaunchDecision::WindowElapsed`] at the window bound. The window itself
/// never kills and never polls past the deadline; a caller that receives
/// `WindowElapsed` MUST treat it as a failed launch and terminate the child
/// (see [`conclude_launch`]), and never converts an observed exit or expiry
/// into a success report.
pub fn run_launch_window<C, R, K>(
    child: &mut C,
    readiness: &mut R,
    clock: &K,
    expected_digest: &str,
    window: Duration,
) -> Result<LaunchDecision, UpdateError>
where
    C: LaunchChild,
    R: LaunchReadiness,
    K: LaunchClock,
{
    let window_ms = u64::try_from(window.as_millis()).unwrap_or(u64::MAX);
    let deadline = clock.now_ms().saturating_add(window_ms);
    loop {
        // A process fact wins over a raced readiness proof: a release that
        // already exited is never reported as a healthy launch.
        if let Some(code) = child.try_exit_code()? {
            return Ok(LaunchDecision::Exited(code));
        }
        if let Some(proof) = readiness.try_proof()? {
            if proof.digest == expected_digest {
                return Ok(LaunchDecision::Ready);
            }
            tracing::warn!(
                "refusing a launch readiness proof attesting digest {} (the verified release is {})",
                proof.digest,
                expected_digest
            );
        }
        if clock.now_ms() >= deadline {
            return Ok(LaunchDecision::WindowElapsed);
        }
        clock.sleep_ms(LAUNCH_POLL_INTERVAL_MS);
    }
}

/// The frozen CHILD digest-attestation line the release prints on stdout
/// before (or alongside) its startup line: `faktor release digest=<64
/// lowercase hex>`. It is the child's OWN claim about the release digest its
/// bootstrap launcher exported via `FAKTOR_RELEASE_DIGEST`; the launcher
/// accepts a readiness proof only when BOTH this line and the frozen startup
/// line arrive on the SAME child stdout pipe and the attested digest equals
/// the digest re-verified before the spawn. A child that prints a different
/// digest is refused (the window then expires and the launch fails).
#[cfg(any(not(unix), test))]
const CHILD_DIGEST_PREFIX: &str = "faktor release digest=";

/// Parse one child digest attestation: exactly the frozen prefix plus 64
/// lowercase hex characters (anything else is not an attestation).
#[cfg(any(not(unix), test))]
fn parse_child_digest_attestation(line: &[u8]) -> Option<&str> {
    let text = std::str::from_utf8(line).ok()?;
    let digest = text
        .trim_end_matches(['\r', '\n'])
        .strip_prefix(CHILD_DIGEST_PREFIX)?;
    manifest::is_lower_hex(digest, 64).then_some(digest)
}

/// The frozen daemon startup line (the supervisor parses the same line):
/// `faktor server listening on http://127.0.0.1:<port>`. `faktor-server`
/// owns the writer, but this crate cannot depend on it (dependency
/// direction: the server depends on the updater), so the launcher mirrors
/// the prefix and requires a usable port.
#[cfg(any(not(unix), test))]
const STARTUP_LINE_PREFIX: &str = "faktor server listening on http://127.0.0.1:";

/// Is one handshake line the frozen daemon startup line with a usable port?
#[cfg(any(not(unix), test))]
fn is_daemon_startup_line(line: &[u8]) -> bool {
    let Ok(line) = std::str::from_utf8(line) else {
        return false;
    };
    match line
        .trim_end_matches(['\r', '\n'])
        .strip_prefix(STARTUP_LINE_PREFIX)
    {
        Some(port) => port.parse::<u16>().is_ok_and(|port| port > 0),
        None => false,
    }
}

/// One stdout line read from the handshake pipe (bounded memory: an overlong
/// line is drained but never retained). `Ok(None)` = EOF before any byte.
#[cfg(any(not(unix), test))]
fn read_bounded_stdout_line(
    reader: &mut impl std::io::Read,
    line: &mut Vec<u8>,
) -> std::io::Result<Option<()>> {
    /// One line's retained cap: the startup line is tiny, so a hostile or
    /// wedged writer can never grow launcher memory without bound.
    const LINE_MAX_BYTES: usize = 8 * 1024;
    line.clear();
    let mut byte = [0u8; 1];
    loop {
        if reader.read(&mut byte)? == 0 {
            return Ok(if line.is_empty() { None } else { Some(()) });
        }
        if byte[0] == b'\n' {
            return Ok(Some(()));
        }
        if line.len() < LINE_MAX_BYTES {
            line.push(byte[0]);
        }
    }
}

/// The non-unix readiness source: a launcher-owned pipe carries the release's
/// stdout; every line is handed to `forward` (production forwards it to the
/// launcher's stdout, preserving the supervisor's frozen startup-line
/// contract). Readiness is proven ONLY by the child itself: the frozen
/// startup line (the daemon serves) plus the child's digest attestation line
/// ([`CHILD_DIGEST_PREFIX`]); both arrive on the SAME child pipe. The proof
/// carries the digest the CHILD attested — never the launcher's expected
/// value — so `run_launch_window` can refuse a child that attests a different
/// release. One proof per child: the first complete (attestation + startup)
/// pair is final, so a later line can never repair a wrong attestation.
#[cfg(any(not(unix), test))]
struct StdoutReadiness {
    proofs: std::sync::mpsc::Receiver<LaunchProof>,
}

#[cfg(any(not(unix), test))]
impl StdoutReadiness {
    fn start_with<F>(stdout: impl std::io::Read + Send + 'static, forward: F) -> Self
    where
        F: Fn(&[u8]) + Send + 'static,
    {
        let (sender, proofs) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut stdout = stdout;
            let mut line = Vec::new();
            let mut attested: Option<String> = None;
            let mut startup_seen = false;
            // EOF and a read error both end the handshake: the window bound,
            // never this reader, decides the launch.
            while let Ok(Some(())) = read_bounded_stdout_line(&mut stdout, &mut line) {
                forward(&line);
                if let Some(digest) = parse_child_digest_attestation(&line) {
                    attested = Some(digest.to_string());
                } else if is_daemon_startup_line(&line) {
                    startup_seen = true;
                }
                if startup_seen {
                    if let Some(digest) = attested.take() {
                        let _ = sender.send(LaunchProof { digest });
                        return;
                    }
                }
            }
        });
        StdoutReadiness { proofs }
    }
}

#[cfg(any(not(unix), test))]
impl LaunchReadiness for StdoutReadiness {
    fn try_proof(&mut self) -> Result<Option<LaunchProof>, UpdateError> {
        match self.proofs.try_recv() {
            Ok(proof) => Ok(Some(proof)),
            // No proof yet, or the pipe closed without one: the window
            // bound decides, never this check.
            Err(
                std::sync::mpsc::TryRecvError::Empty | std::sync::mpsc::TryRecvError::Disconnected,
            ) => Ok(None),
        }
    }
}

/// The frozen release-started handshake line the IDE supervisors parse:
/// `faktor release started pid=<pid> digest=<64 lowercase hex>`. `pid` is the
/// spawned RELEASE process (never the launcher's own), so a supervisor can
/// track the daemon after the launcher exits at readiness. The digest is the
/// already-public verified release digest; the line carries no secrets.
#[cfg(any(not(unix), test))]
const RELEASE_STARTED_PREFIX: &str = "faktor release started pid=";

/// Write the ONE release-started handshake line to the supervisor-facing
/// stream and flush it. Called immediately after the release is spawned and
/// BEFORE readiness forwarding starts, so no release line can precede it.
/// This write is a launch precondition, not a best-effort log: the caller
/// ([`announce_release_started`]) terminates the just-spawned release and
/// refuses to detach typed when it fails.
#[cfg(any(not(unix), test))]
fn write_release_started_line(
    out: &mut impl std::io::Write,
    pid: u32,
    digest: &str,
) -> std::io::Result<()> {
    writeln!(out, "{RELEASE_STARTED_PREFIX}{pid} digest={digest}")?;
    out.flush()
}

/// Forward one handshake line to the launcher's stdout, exactly as the
/// release wrote it (the supervisor parses this stream). A vanished
/// supervisor is never a launcher panic: the release keeps running.
#[cfg(not(unix))]
fn forward_stdout_line(line: &[u8]) {
    use std::io::Write as _;
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = lock.write_all(line);
    let _ = lock.write_all(b"\n");
    let _ = lock.flush();
}

/// The production child view of the non-unix launcher: a spawned
/// `std::process::Child` with the launcher's exit-code convention (the
/// status code, or 3 when the platform reports none). Dropping it AFTER a
/// proven readiness detaches: the child handle closes and the release keeps
/// running, never killed and never reaped as a zombie by the launcher. An
/// unproven child is terminated explicitly through [`LaunchChild::terminate`].
#[cfg(any(not(unix), test))]
struct ProcessChild {
    child: std::process::Child,
}

#[cfg(any(not(unix), test))]
impl LaunchChild for ProcessChild {
    fn try_exit_code(&mut self) -> Result<Option<i32>, UpdateError> {
        let status = self
            .child
            .try_wait()
            .map_err(|e| UpdateError::LaunchRefused {
                detail: format!("wait: {e}"),
            })?;
        Ok(status.map(|status| status.code().unwrap_or(3)))
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn terminate(&mut self) -> Result<(), UpdateError> {
        match self.child.kill() {
            Ok(()) => {}
            // Already exited between the last poll and the kill: the reap
            // below still settles it.
            Err(e) if e.kind() == std::io::ErrorKind::InvalidInput => {}
            Err(e) => {
                return Err(UpdateError::LaunchRefused {
                    detail: format!(
                        "kill release process {}: {e}; it may still be running",
                        self.child.id()
                    ),
                })
            }
        }
        self.child.wait().map_err(|e| UpdateError::LaunchRefused {
            detail: format!("reap release process {}: {e}", self.child.id()),
        })?;
        Ok(())
    }
}

/// The production monotonic clock of the non-unix launch window.
#[cfg(any(not(unix), test))]
struct MonotonicClock {
    start: std::time::Instant,
}

#[cfg(any(not(unix), test))]
impl MonotonicClock {
    fn new() -> Self {
        MonotonicClock {
            start: std::time::Instant::now(),
        }
    }
}

#[cfg(any(not(unix), test))]
impl LaunchClock for MonotonicClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    fn sleep_ms(&self, ms: u64) {
        std::thread::sleep(Duration::from_millis(ms));
    }
}

/// sha256 of the RUNNING executable (streamed, bounded memory): the build
/// report a supervisor reads to prove which artifact is live.
pub fn self_digest() -> Result<String, UpdateError> {
    let exe = std::env::current_exe()
        .map_err(|e| UpdateError::Install(format!("current executable: {e}")))?;
    file_digest(&exe)
}

/// The release identity this process was launched with (set only by the
/// bootstrap launcher; absent when the binary was started directly).
pub fn running_release() -> Option<(String, String)> {
    let id = std::env::var(RELEASE_ID_ENV).ok()?;
    let digest = std::env::var(RELEASE_DIGEST_ENV).ok()?;
    if id.is_empty() || !crate::manifest::is_lower_hex(&digest, 64) {
        return None;
    }
    Some((id, digest))
}

/// The install root this process was launched from (set only by the
/// bootstrap launcher).
pub fn running_install_root() -> Option<PathBuf> {
    let raw = std::env::var(INSTALL_ROOT_ENV).ok()?;
    if raw.is_empty() {
        return None;
    }
    Some(PathBuf::from(raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_ids_are_plain_directory_names() {
        let id = release_id_for("0.9.1", &"a".repeat(64));
        assert_eq!(id, format!("0.9.1-{}", "a".repeat(12)));
        assert!(validate_release_id(&id).is_ok());
        for hostile in ["", ".", "..", "../escape", "a/b", "a\\b", "a b", "rélease"] {
            assert!(
                validate_release_id(hostile).is_err(),
                "{hostile:?} must be refused"
            );
        }
        assert!(validate_release_id(&"a".repeat(161)).is_err());
    }

    #[test]
    fn trust_anchor_round_trips_and_refuses_malformed_material() {
        let key = TrustedKey::from_base64("operator", &{
            use base64::Engine as _;
            base64::engine::general_purpose::STANDARD.encode(
                ed25519_dalek::SigningKey::from_bytes(&[3u8; 32])
                    .verifying_key()
                    .to_bytes(),
            )
        })
        .unwrap();
        let keys = TrustedKeys::new(vec![key]).unwrap();
        let file = TrustFile::from_keys(&keys);
        let parsed =
            serde_json::from_slice::<TrustFile>(&serde_json::to_vec(&file).unwrap()).unwrap();
        assert_eq!(parsed.to_keys().unwrap().identities(), vec!["operator"]);
        // Duplicate identities survive the file but refuse at allowlist build.
        let mut duplicate = file.clone();
        duplicate.keys.push(file.keys[0].clone());
        assert!(duplicate.to_keys().is_err());
        // A foreign schema is refused.
        let mut foreign = file;
        foreign.schema = "faktor-install-trust/v9".into();
        assert!(foreign.to_keys().is_err());
        // A missing file is an EMPTY allowlist, never an implicit anchor.
        let dir = tempfile::tempdir().unwrap();
        assert!(TrustFile::read(&dir.path().join("absent.json"))
            .unwrap()
            .is_empty());
        // A malformed file is a typed refusal.
        let bad = dir.path().join("bad.json");
        std::fs::write(&bad, b"{not json").unwrap();
        assert!(TrustFile::read(&bad).is_err());
    }

    /// One scripted launch-window observation set: no real clock, child or
    /// pipe, so the window decisions are exercised identically on every
    /// platform (including the Windows-only seam).
    struct MockChild {
        exit_code: Option<i32>,
        terminate_error: Option<String>,
        polls: std::cell::Cell<usize>,
        terminates: std::cell::Cell<usize>,
    }

    impl MockChild {
        fn running() -> Self {
            MockChild {
                exit_code: None,
                terminate_error: None,
                polls: std::cell::Cell::new(0),
                terminates: std::cell::Cell::new(0),
            }
        }

        fn exited(code: i32) -> Self {
            MockChild {
                exit_code: Some(code),
                ..MockChild::running()
            }
        }
    }

    impl LaunchChild for MockChild {
        fn try_exit_code(&mut self) -> Result<Option<i32>, UpdateError> {
            self.polls.set(self.polls.get() + 1);
            Ok(self.exit_code)
        }

        fn pid(&self) -> u32 {
            4242
        }

        fn terminate(&mut self) -> Result<(), UpdateError> {
            self.terminates.set(self.terminates.get() + 1);
            match &self.terminate_error {
                Some(detail) => Err(UpdateError::LaunchRefused {
                    detail: detail.clone(),
                }),
                None => Ok(()),
            }
        }
    }

    struct MockReadiness {
        proof: Option<LaunchProof>,
        yields: usize,
        polls: std::cell::Cell<usize>,
    }

    impl LaunchReadiness for MockReadiness {
        fn try_proof(&mut self) -> Result<Option<LaunchProof>, UpdateError> {
            self.polls.set(self.polls.get() + 1);
            if self.yields == 0 {
                return Ok(None);
            }
            self.yields -= 1;
            Ok(self.proof.clone())
        }
    }

    struct MockClock {
        now_ms: std::cell::Cell<u64>,
        sleeps: std::cell::Cell<u64>,
    }

    impl MockClock {
        fn new() -> Self {
            MockClock {
                now_ms: std::cell::Cell::new(0),
                sleeps: std::cell::Cell::new(0),
            }
        }
    }

    impl LaunchClock for MockClock {
        fn now_ms(&self) -> u64 {
            self.now_ms.get()
        }

        fn sleep_ms(&self, ms: u64) {
            self.sleeps.set(self.sleeps.get() + 1);
            self.now_ms.set(self.now_ms.get() + ms);
        }
    }

    const TEST_WINDOW: Duration = Duration::from_millis(LAUNCH_FORWARD_WINDOW_MS);

    /// Ready inside the window: the launcher detaches at the first poll —
    /// no forwarding loop, no sleep — and only a concluded Ready exits 0
    /// without terminating the release.
    #[test]
    fn a_release_that_proves_readiness_detaches_immediately() {
        let digest = "a".repeat(64);
        let mut child = MockChild::running();
        let mut readiness = MockReadiness {
            proof: Some(LaunchProof {
                digest: digest.clone(),
            }),
            yields: usize::MAX,
            polls: std::cell::Cell::new(0),
        };
        let clock = MockClock::new();
        let decision =
            run_launch_window(&mut child, &mut readiness, &clock, &digest, TEST_WINDOW).unwrap();
        assert_eq!(decision, LaunchDecision::Ready);
        assert_eq!(
            clock.sleeps.get(),
            0,
            "readiness ends the window immediately"
        );
        assert_eq!(
            conclude_launch(&mut child, decision, &digest).unwrap(),
            0,
            "readiness is the only success and detaches with exit 0"
        );
        assert_eq!(
            child.terminates.get(),
            0,
            "a proven release is never terminated"
        );
    }

    /// A fast crash inside the window is still forwarded exactly, and the
    /// exit status — never a fabricated success — is the launcher's.
    #[test]
    fn a_failure_inside_the_window_is_forwarded() {
        let digest = "b".repeat(64);
        let mut child = MockChild::exited(7);
        let mut readiness = MockReadiness {
            proof: None,
            yields: 0,
            polls: std::cell::Cell::new(0),
        };
        let clock = MockClock::new();
        let decision =
            run_launch_window(&mut child, &mut readiness, &clock, &digest, TEST_WINDOW).unwrap();
        assert_eq!(decision, LaunchDecision::Exited(7));
        assert_eq!(
            clock.sleeps.get(),
            0,
            "an exit is a process fact, not a wait"
        );
        assert_eq!(conclude_launch(&mut child, decision, &digest).unwrap(), 7);
        assert_eq!(child.terminates.get(), 0);
    }

    /// A release that never confirms ends the window as a FAILURE: the
    /// window decision contains no success, and concluding it terminates the
    /// unconfirmed child and returns a typed refusal (never Ok(0)).
    #[test]
    fn an_unconfirmed_release_is_terminated_with_a_typed_failure() {
        let digest = "c".repeat(64);
        let mut child = MockChild::running();
        let mut readiness = MockReadiness {
            proof: None,
            yields: 0,
            polls: std::cell::Cell::new(0),
        };
        let clock = MockClock::new();
        let decision =
            run_launch_window(&mut child, &mut readiness, &clock, &digest, TEST_WINDOW).unwrap();
        assert_eq!(decision, LaunchDecision::WindowElapsed);
        // The deadline is observed within one poll interval, the poll count
        // is the bounded window/poll cadence, and the loop stopped polling
        // (the call returned) — never the daemon's lifetime.
        assert!(clock.now_ms.get() <= LAUNCH_FORWARD_WINDOW_MS + LAUNCH_POLL_INTERVAL_MS);
        assert_eq!(
            child.polls.get() as u64,
            LAUNCH_FORWARD_WINDOW_MS / LAUNCH_POLL_INTERVAL_MS + 1
        );
        assert_eq!(child.polls.get(), readiness.polls.get());
        // Concluding the window terminates the unconfirmed release and
        // reports a typed failure — never a silent success.
        let err = conclude_launch(&mut child, decision, &digest).unwrap_err();
        assert_eq!(err.code(), "launch_refused");
        assert!(err.to_string().contains("did not prove readiness"), "{err}");
        assert!(err.to_string().contains(&digest), "{err}");
        assert_eq!(child.terminates.get(), 1, "the unconfirmed child is killed");
    }

    /// Regression guard: the window is seconds, never a multi-day residency.
    #[test]
    fn the_forward_window_is_short_and_never_regresses_to_days() {
        const { assert!(LAUNCH_FORWARD_WINDOW_MS >= 1_000) };
        const {
            assert!(
                LAUNCH_FORWARD_WINDOW_MS <= 5 * 60 * 1000,
                "the launcher must never stay resident for the daemon's lifetime"
            )
        };
        const { assert!(LAUNCH_POLL_INTERVAL_MS > 0) };
        const { assert!(LAUNCH_POLL_INTERVAL_MS <= LAUNCH_FORWARD_WINDOW_MS) };
        // The coordinated contract: the window is the documented 30 s.
        const { assert!(LAUNCH_FORWARD_WINDOW_MS == 30_000) };
    }

    /// The digest/readiness gate: a proof that attests another digest is
    /// never accepted, so the window (not the proof) ends the launch — and a
    /// concluded expiry kills the mismatching child.
    #[test]
    fn a_proof_for_another_digest_never_detaches() {
        let mut child = MockChild::running();
        let mut readiness = MockReadiness {
            proof: Some(LaunchProof {
                digest: "d".repeat(64),
            }),
            yields: usize::MAX,
            polls: std::cell::Cell::new(0),
        };
        let clock = MockClock::new();
        let decision = run_launch_window(
            &mut child,
            &mut readiness,
            &clock,
            &"a".repeat(64),
            Duration::from_millis(500),
        )
        .unwrap();
        assert_eq!(decision, LaunchDecision::WindowElapsed);
        assert!(
            child.polls.get() > 1,
            "the window, not the wrong-digest proof, ended the launch"
        );
        assert!(conclude_launch(&mut child, decision, &"a".repeat(64)).is_err());
        assert_eq!(child.terminates.get(), 1);
    }

    /// A terminate failure is named in the typed refusal (the launcher never
    /// claims a clean kill it could not perform).
    #[test]
    fn a_failed_termination_is_reported_not_swallowed() {
        let mut child = MockChild {
            exit_code: None,
            terminate_error: Some("kill was refused".into()),
            polls: std::cell::Cell::new(0),
            terminates: std::cell::Cell::new(0),
        };
        let err = conclude_launch(&mut child, LaunchDecision::WindowElapsed, &"a".repeat(64))
            .unwrap_err();
        assert_eq!(err.code(), "launch_refused");
        assert!(err.to_string().contains("kill was refused"), "{err}");
        assert!(err.to_string().contains("may still be running"), "{err}");
    }

    /// The child digest attestation grammar: exactly the frozen prefix plus
    /// 64 lowercase hex; uppercase/short/long/embedded forms never attest.
    #[test]
    fn a_child_digest_attestation_requires_64_lowercase_hex() {
        let digest = "0123456789abcdef".repeat(4);
        assert_eq!(
            parse_child_digest_attestation(format!("{CHILD_DIGEST_PREFIX}{digest}").as_bytes()),
            Some(digest.as_str())
        );
        assert_eq!(
            parse_child_digest_attestation(format!("{CHILD_DIGEST_PREFIX}{digest}\r\n").as_bytes()),
            Some(digest.as_str())
        );
        for hostile in [
            format!("{CHILD_DIGEST_PREFIX}{}", "A".repeat(64)),
            format!("{CHILD_DIGEST_PREFIX}{}", "a".repeat(63)),
            format!("{CHILD_DIGEST_PREFIX}{}", "a".repeat(65)),
            format!("{CHILD_DIGEST_PREFIX}{digest} trailing"),
            format!("leading {CHILD_DIGEST_PREFIX}{digest}"),
            CHILD_DIGEST_PREFIX.to_string(),
            String::new(),
        ] {
            assert!(
                parse_child_digest_attestation(hostile.as_bytes()).is_none(),
                "{hostile:?} must not attest a digest"
            );
        }
        assert!(parse_child_digest_attestation(&[0xff, 0xfe]).is_none());
    }

    /// The handshake write is a launch precondition: a closed supervisor
    /// stdout terminates the just-spawned release and returns a typed
    /// refusal — never a silent detach of an unannounced process.
    #[test]
    fn a_failed_launch_handshake_terminates_the_release_and_refuses_to_detach() {
        struct ClosedWriter;
        impl std::io::Write for ClosedWriter {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "closed stdout",
                ))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "closed stdout",
                ))
            }
        }

        let digest = "a".repeat(64);
        let mut child = MockChild::running();
        let err = announce_release_started(&mut child, &mut ClosedWriter, &digest).unwrap_err();
        assert_eq!(err.code(), "launch_refused");
        assert!(err.to_string().contains("handshake"), "{err}");
        assert!(err.to_string().contains("refusing to detach"), "{err}");
        assert_eq!(
            child.terminates.get(),
            1,
            "the unannounced release is never left running"
        );

        // The happy path still detaches without touching the child.
        let mut out = Vec::new();
        let mut child = MockChild::running();
        announce_release_started(&mut child, &mut out, &digest).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("faktor release started pid=4242 digest={digest}\n")
        );
        assert_eq!(child.terminates.get(), 0);
    }

    /// Only the frozen startup line with a usable port proves readiness: a
    /// lookalike or a zero/overflowing/reordered port never does.
    #[test]
    fn the_startup_line_predicate_requires_a_usable_port() {
        assert!(is_daemon_startup_line(
            format!("{STARTUP_LINE_PREFIX}4711").as_bytes()
        ));
        assert!(is_daemon_startup_line(
            format!("{STARTUP_LINE_PREFIX}65535\r\n").as_bytes()
        ));
        for hostile in ["0", "65536", "not-a-port", "", "-1", " 4711"] {
            assert!(
                !is_daemon_startup_line(format!("{STARTUP_LINE_PREFIX}{hostile}").as_bytes()),
                "port {hostile:?} must not prove readiness"
            );
        }
        assert!(!is_daemon_startup_line(
            b"faktor server listening elsewhere"
        ));
        assert!(!is_daemon_startup_line(b""));
        assert!(!is_daemon_startup_line(&[0xff, 0xfe]));
    }

    /// The frozen release-started line: exactly `faktor release started
    /// pid=<pid> digest=<64 lowercase hex>`, newline-terminated, flushed, and
    /// nothing else — the grammar both IDE supervisors pin
    /// (`^faktor release started pid=([1-9]\d*) digest=([0-9a-f]{64})$`).
    #[test]
    fn the_release_started_line_is_exactly_the_frozen_flushed_handshake() {
        struct FlushProbe {
            bytes: Vec<u8>,
            flushed: bool,
        }
        impl std::io::Write for FlushProbe {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.bytes.extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.flushed = true;
                Ok(())
            }
        }

        let digest = "0123456789abcdef".repeat(4);
        assert!(manifest::is_lower_hex(&digest, 64));
        let mut probe = FlushProbe {
            bytes: Vec::new(),
            flushed: false,
        };
        write_release_started_line(&mut probe, 4242, &digest).unwrap();
        assert!(
            probe.flushed,
            "the supervisor must receive the handshake immediately"
        );
        let text = String::from_utf8(probe.bytes).unwrap();
        assert_eq!(
            text,
            format!("faktor release started pid=4242 digest={digest}\n")
        );
        assert_eq!(text.matches('\n').count(), 1, "exactly one handshake line");
        let rest = text
            .trim_end_matches('\n')
            .strip_prefix(RELEASE_STARTED_PREFIX)
            .expect("the line starts with the frozen prefix");
        let (pid, parsed_digest) = rest.split_once(" digest=").unwrap();
        assert_eq!(pid, "4242");
        assert_eq!(parsed_digest, digest);
        assert!(manifest::is_lower_hex(parsed_digest, 64));
    }

    /// The production launch seam over a REAL child and pipe: the launcher
    /// announces the spawned release's pid + digest on its stdout BEFORE the
    /// readiness forwarder exists, so the first supervisor-visible line is
    /// the handshake and the forwarded startup line follows it. The non-unix
    /// `launch()` cannot compile its spawn branch on unix, so this test
    /// simulates that launch order against the same production writer.
    #[cfg(unix)]
    #[test]
    fn the_launch_announces_the_real_release_pid_and_digest_before_forwarding() {
        use std::io::Write as _;

        let (reader, writer) = std::io::pipe().unwrap();
        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "printf '{CHILD_DIGEST_PREFIX}{digest}\\n'; printf '{STARTUP_LINE_PREFIX}4711\\n'; sleep 30",
                digest = "9a".repeat(32),
            ))
            .stdout(writer)
            .spawn()
            .unwrap();
        let digest = "9a".repeat(32);
        assert!(manifest::is_lower_hex(&digest, 64));
        // The supervisor-facing stdout: the launcher's handshake, then every
        // forwarded release line, in real emission order.
        let supervisor: std::sync::Arc<std::sync::Mutex<Vec<u8>>> = Default::default();
        {
            let mut stream = supervisor.lock().unwrap();
            write_release_started_line(&mut *stream, child.id(), &digest).unwrap();
        }
        let forwarded = supervisor.clone();
        let mut process = ProcessChild { child };
        let mut readiness = StdoutReadiness::start_with(reader, move |line| {
            let mut stream = forwarded.lock().unwrap();
            stream.write_all(line).unwrap();
            stream.write_all(b"\n").unwrap();
        });
        let clock = MonotonicClock::new();
        let decision = run_launch_window(
            &mut process,
            &mut readiness,
            &clock,
            &digest,
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(decision, LaunchDecision::Ready);
        assert_eq!(conclude_launch(&mut process, decision, &digest).unwrap(), 0);
        let real_pid = process.child.id();
        let text = String::from_utf8(supervisor.lock().unwrap().clone()).unwrap();
        let mut lines = text.lines();
        let handshake = lines
            .next()
            .expect("the launcher announces the release before forwarding");
        assert_eq!(
            handshake,
            format!("faktor release started pid={real_pid} digest={digest}")
        );
        let rest = handshake.strip_prefix(RELEASE_STARTED_PREFIX).unwrap();
        let (pid, parsed_digest) = rest.split_once(" digest=").unwrap();
        assert_eq!(
            pid.parse::<u32>().unwrap(),
            real_pid,
            "the announced pid is the REAL spawned release process"
        );
        assert!(pid.bytes().all(|b| b.is_ascii_digit()));
        assert!(!pid.starts_with('0'), "the frozen grammar refuses pid 0");
        assert!(manifest::is_lower_hex(parsed_digest, 64));
        assert_eq!(parsed_digest, digest);
        assert_eq!(
            text.matches(RELEASE_STARTED_PREFIX).count(),
            1,
            "exactly one handshake line, never repeated"
        );
        let child_attestation = format!("{CHILD_DIGEST_PREFIX}{digest}");
        assert_eq!(
            lines.next(),
            Some(child_attestation.as_str()),
            "the child's OWN digest attestation is forwarded AFTER the launcher handshake"
        );
        let startup = format!("{STARTUP_LINE_PREFIX}4711");
        assert_eq!(
            lines.next(),
            Some(startup.as_str()),
            "the readiness line follows the child attestation"
        );
        let _ = process.child.kill();
        let _ = process.child.wait();
    }

    /// The production glue over a REAL pipe and child (the same code the
    /// non-unix launcher runs; exercised on unix so the Windows seam is
    /// behavior-checked here even though it cannot be compiled for Windows):
    /// readiness needs the frozen startup line AND the child's digest
    /// attestation, both from this same pipe.
    #[cfg(unix)]
    #[test]
    fn the_stdout_handshake_detaches_on_the_frozen_startup_line() {
        let (reader, writer) = std::io::pipe().unwrap();
        let digest = "e".repeat(64);
        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "printf '{CHILD_DIGEST_PREFIX}{digest}\\n'; printf '{STARTUP_LINE_PREFIX}4711\\n'; sleep 30"
            ))
            .stdout(writer)
            .spawn()
            .unwrap();
        let mut process = ProcessChild { child };
        let mut readiness = StdoutReadiness::start_with(reader, |_| {});
        let clock = MonotonicClock::new();
        let decision = run_launch_window(
            &mut process,
            &mut readiness,
            &clock,
            &digest,
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(decision, LaunchDecision::Ready);
        assert_eq!(conclude_launch(&mut process, decision, &digest).unwrap(), 0);
        assert!(
            process.child.try_wait().unwrap().is_none(),
            "detaching never kills or reaps a proven release"
        );
        let _ = process.child.kill();
        let _ = process.child.wait();
    }

    /// A real child that attests the WRONG digest is never ready even though
    /// it printed a valid-looking startup line: the proof carries the child's
    /// attestation (never the launcher's expected value), the window expires,
    /// and concluding it terminates the spoofing child.
    #[cfg(unix)]
    #[test]
    fn a_real_child_attesting_the_wrong_digest_is_refused_and_terminated() {
        let (reader, writer) = std::io::pipe().unwrap();
        let expected = "e".repeat(64);
        let spoof = "f".repeat(64);
        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!(
                "printf '{CHILD_DIGEST_PREFIX}{spoof}\\n'; printf '{STARTUP_LINE_PREFIX}4711\\n'; sleep 30"
            ))
            .stdout(writer)
            .spawn()
            .unwrap();
        let mut process = ProcessChild { child };
        let mut readiness = StdoutReadiness::start_with(reader, |_| {});
        // The proof the child presents names ITS attested digest, so the
        // mismatch branch is reachable (this is the exact property the old
        // launcher-stamped proof made unreachable).
        let proof = {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(proof) = readiness.try_proof().unwrap() {
                    break proof;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "the child emitted both lines; a proof must arrive"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        assert_eq!(proof.digest, spoof, "the proof is the CHILD's claim");
        assert_ne!(proof.digest, expected);
        let clock = MonotonicClock::new();
        let decision = run_launch_window(
            &mut process,
            &mut readiness,
            &clock,
            &expected,
            Duration::from_millis(300),
        )
        .unwrap();
        assert_eq!(
            decision,
            LaunchDecision::WindowElapsed,
            "a wrong-digest attestation never proves readiness"
        );
        let err = conclude_launch(&mut process, decision, &expected).unwrap_err();
        assert_eq!(err.code(), "launch_refused");
        assert!(
            process.child.try_wait().unwrap().is_some(),
            "the spoofing child was terminated and reaped (no orphan)"
        );
    }

    /// A real child that exits is forwarded; an empty handshake pipe (EOF
    /// without a proof) is never itself an error.
    #[cfg(unix)]
    #[test]
    fn a_real_child_that_exits_is_forwarded_within_the_window() {
        let (reader, writer) = std::io::pipe().unwrap();
        drop(writer);
        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg("exit 7")
            .spawn()
            .unwrap();
        let digest = "f".repeat(64);
        let mut process = ProcessChild { child };
        let mut readiness = StdoutReadiness::start_with(reader, |_| {});
        let clock = MonotonicClock::new();
        let decision = run_launch_window(
            &mut process,
            &mut readiness,
            &clock,
            &digest,
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(decision, LaunchDecision::Exited(7));
        assert_eq!(conclude_launch(&mut process, decision, &digest).unwrap(), 7);
        let _ = process.child.wait();
    }

    /// A real child that never proves readiness is TERMINATED at the bound
    /// (never detached, never polled past it): the launch fails typed and no
    /// orphan is left behind.
    #[cfg(unix)]
    #[test]
    fn a_real_child_that_never_confirms_is_terminated_at_the_bound() {
        let (reader, writer) = std::io::pipe().unwrap();
        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg("printf 'still starting\\n'; sleep 30")
            .stdout(writer)
            .spawn()
            .unwrap();
        let digest = "1".repeat(64);
        let mut process = ProcessChild { child };
        let mut readiness = StdoutReadiness::start_with(reader, |_| {});
        let clock = MonotonicClock::new();
        let started = std::time::Instant::now();
        let decision = run_launch_window(
            &mut process,
            &mut readiness,
            &clock,
            &digest,
            Duration::from_millis(300),
        )
        .unwrap();
        assert_eq!(decision, LaunchDecision::WindowElapsed);
        assert!(started.elapsed() < Duration::from_secs(2));
        let err = conclude_launch(&mut process, decision, &digest).unwrap_err();
        assert_eq!(err.code(), "launch_refused");
        assert!(err.to_string().contains("did not prove readiness"), "{err}");
        assert!(
            process.child.try_wait().unwrap().is_some(),
            "the unconfirmed release is killed and reaped (no orphan)"
        );
    }
}
