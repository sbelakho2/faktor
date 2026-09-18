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

    /// The release id derived from one (version, digest) pair WHEN its
    /// binary is materialized with exactly that digest. `None` = the legacy
    /// layout (no immutable release directory for this artifact), so callers
    /// keep the legacy pointer shape untouched.
    pub fn release_id_if_materialized(&self, version: &str, digest: &str) -> Option<String> {
        if digest.len() < 12 {
            return None;
        }
        let release_id = release_id_for(version, digest);
        if validate_release_id(&release_id).is_err() {
            return None;
        }
        let binary = self.release_binary(&release_id);
        if !binary.is_file() {
            return None;
        }
        match file_digest(&binary) {
            Ok(actual) if actual == digest => Some(release_id),
            _ => None,
        }
    }

    /// Publish one release immutably: `versions/<release-id>/faktor` from the
    /// content-addressed artifact plus `versions/<release-id>/manifest` (the
    /// SIGNED update manifest bytes that authorized those exact bytes). The
    /// artifact digest is re-checked here; a mismatch is refused before a
    /// version directory exists. Idempotent: an already-materialized release
    /// with the identical binary and manifest is kept.
    pub fn materialize_release(
        &self,
        release_id: &str,
        artifact_name: &str,
        digest: &str,
        manifest_bytes: &[u8],
    ) -> Result<PathBuf, UpdateError> {
        validate_release_id(release_id)?;
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
        let dir = self.release_dir(release_id);
        fs::create_dir_all(&dir).map_err(|e| {
            UpdateError::Install(format!("create release dir {}: {e}", dir.display()))
        })?;
        let binary = self.release_binary(release_id);
        let manifest_path = self.release_manifest(release_id);
        let binary_current =
            binary.is_file() && file_digest(&binary).ok().as_deref() == Some(digest);
        let manifest_current = fs::read(&manifest_path)
            .map(|existing| existing == manifest_bytes)
            .unwrap_or(false);
        if binary_current && manifest_current {
            return Ok(dir);
        }
        if !binary_current {
            let tmp = dir.join(format!(
                ".{RELEASE_BINARY_NAME}.kp-tmp-{}-{}",
                std::process::id(),
                unique_nonce()
            ));
            if let Err(e) = copy_file(&artifact, &tmp) {
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
            if let Err(e) = faktor_fs::atomic::atomic_adopt(&tmp, &binary) {
                let _ = fs::remove_file(&tmp);
                return Err(UpdateError::Install(format!(
                    "publish release binary {}: {e}",
                    binary.display()
                )));
            }
        }
        faktor_fs::atomic::atomic_replace(&manifest_path, manifest_bytes).map_err(|e| {
            UpdateError::Install(format!(
                "write release manifest {}: {e}",
                manifest_path.display()
            ))
        })?;
        faktor_fs::atomic::fsync_parent(&dir);
        Ok(dir)
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
            ".{LAUNCHER_FILE_NAME}.kp-tmp-{}-{}",
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
/// 3. load `versions/<release-id>/manifest` and verify the ed25519 signature
///    against the anchor (unsigned/unknown-key/key-mismatch/tampered are
///    distinct typed refusals);
/// 4. bind pointer ↔ manifest: version, channel and the pointer's digest
///    must all appear in the signed document;
/// 5. `faktor` must be a regular file whose content digest equals the
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
    let manifest_path = layout.release_manifest(&release_id);
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
    let meta = fs::symlink_metadata(&binary).map_err(|e| UpdateError::LaunchRefused {
        detail: format!("release binary {} is missing: {e}", binary.display()),
    })?;
    if !meta.file_type().is_file() {
        return Err(UpdateError::LaunchRefused {
            detail: format!(
                "release binary {} is not a regular file (symlinks/directories are refused)",
                binary.display()
            ),
        });
    }
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
/// code is the daemon's. On non-unix platforms the child is spawned and its
/// exit code forwarded under [`RELEASE_FORWARD_CEILING_MS`].
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
        let mut child = command.spawn().map_err(|e| UpdateError::LaunchRefused {
            detail: format!("spawn {}: {e}", target.binary.display()),
        })?;
        match wait_bounded(
            &mut child,
            Duration::from_millis(RELEASE_FORWARD_CEILING_MS),
        )? {
            Some(status) => Ok(status.code().unwrap_or(3)),
            None => {
                // The release outlived the forwarding window: it is a
                // service, not a short-lived child, so the launcher detaches
                // instead of killing it (zero-orphans is preserved by the
                // release's own process ownership, not by the launcher).
                tracing::warn!(
                    "release {} outlived the {} ms exit-forwarding ceiling; detaching",
                    target.binary.display(),
                    RELEASE_FORWARD_CEILING_MS
                );
                Ok(0)
            }
        }
    }
}

/// Documented ceiling on how long the non-unix launcher forwards a release's
/// exit code before detaching. The wait is bounded so a wedged child can
/// never pin the launcher forever without observability; a healthy daemon
/// outliving the ceiling keeps running and the launcher exits 0.
pub const RELEASE_FORWARD_CEILING_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// Poll one child until it exits or `ceiling` elapses. `None` means the child
/// is still running at the ceiling (the caller decides to detach; this
/// helper never kills). Platform-neutral so it is unit-testable everywhere.
#[cfg_attr(unix, allow(dead_code))]
fn wait_bounded(
    child: &mut std::process::Child,
    ceiling: Duration,
) -> Result<Option<std::process::ExitStatus>, UpdateError> {
    let deadline = std::time::Instant::now() + ceiling;
    loop {
        let status = child.try_wait().map_err(|e| UpdateError::LaunchRefused {
            detail: format!("wait: {e}"),
        })?;
        if let Some(status) = status {
            return Ok(Some(status));
        }
        if std::time::Instant::now() >= deadline {
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(100));
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

    /// The non-unix launcher wait is bounded: an exited child yields its
    /// status exactly, and a child still running at the ceiling yields
    /// `None` (the caller detaches; this helper never kills).
    #[cfg(unix)]
    #[test]
    fn wait_bounded_returns_status_or_detaches_at_the_ceiling() {
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("exit 7")
            .spawn()
            .unwrap();
        let status = wait_bounded(&mut child, Duration::from_secs(5))
            .unwrap()
            .expect("a child that exits must yield its status");
        assert_eq!(status.code(), Some(7));

        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 5")
            .spawn()
            .unwrap();
        let started = std::time::Instant::now();
        assert!(
            wait_bounded(&mut child, Duration::from_millis(150))
                .unwrap()
                .is_none(),
            "a child outliving the ceiling must be reported as still running"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        let _ = child.kill();
        let _ = child.wait();
    }
}
