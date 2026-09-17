//! The install layout and its atomic swap discipline.
//!
//! ```text
//! <install_root>/
//!   current                                   pointer file (the ONLY
//!                                             authority on what is installed)
//!   artifacts/<artifact-name>/<sha256>        content-addressed staged bytes
//!   staging/<op-id>/<artifact-name>           in-flight download (same fs)
//! ```
//!
//! The ADDITIVE immutable-version layout (see [`crate::release`]) lives in
//! the same root and adds `launcher`, `trusted-keys.json` and
//! `versions/<release-id>/{faktor,manifest}`. When a pointer carries a
//! `release_id`, [`InstallLayout::verify_installed`] verifies the immutable
//! `versions/<release-id>/faktor` bytes instead of the legacy artifact path;
//! a pointer without one keeps the legacy behavior byte-identically.
//!
//! Never in-place: a downloaded artifact is streamed into `staging/` (same
//! filesystem as `artifacts/`), digest-verified, flushed+fsynced, and only
//! then renamed into `artifacts/<name>/<digest>` through
//! [`faktor_fs::atomic::atomic_adopt`]. The install is switched by atomically
//! REPLACING the `current` pointer (faktor-fs atomic replace: temp + fsync +
//! rename + parent fsync) — there is no window in which `current` names a
//! half-written artifact, and a crash leaves either the old pointer or the
//! new one.
//!
//! The default health probe re-hashes the artifact the pointer names and
//! checks the pointer's shape: a "doctor-style" post-swap probe that needs
//! no subprocess and no store access (hosts may inject a richer probe —
//! the CLI's local apply runs the real `doctor` quick check).

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::UpdateError;
use crate::manifest::{is_lower_hex, Artifact};

/// Schema tag of the `current` pointer file (the `release_id` member is
/// additive; legacy pointers parse with `release_id: None`).
pub const INSTALL_POINTER_SCHEMA: &str = "faktor-install-pointer/v1";

/// The stable bootstrap launcher file name.
pub const LAUNCHER_FILE_NAME: &str = "launcher";
/// The immutable release directories.
pub const VERSIONS_DIR_NAME: &str = "versions";
/// The exact release binary inside one release directory.
pub const RELEASE_BINARY_NAME: &str = "faktor";
/// The signed release manifest inside one release directory.
pub const RELEASE_MANIFEST_NAME: &str = "manifest";
/// The launch trust anchor (operator key allowlist).
pub const TRUST_FILE_NAME: &str = "trusted-keys.json";

/// The durable answer to "what is installed here".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallPointer {
    pub schema: String,
    pub artifact: String,
    pub digest: String,
    pub version: String,
    pub channel: String,
    pub applied_ms: i64,
    /// The immutable release this pointer names (`versions/<release-id>/`).
    /// `None` = the legacy content-addressed layout. Additive: absent on
    /// every pre-release-layout pointer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_id: Option<String>,
}

impl InstallPointer {
    pub fn new(
        artifact: &str,
        digest: &str,
        version: &str,
        channel: &str,
        applied_ms: i64,
    ) -> Result<Self, UpdateError> {
        let pointer = InstallPointer {
            schema: INSTALL_POINTER_SCHEMA.to_string(),
            artifact: artifact.to_string(),
            digest: digest.to_string(),
            version: version.to_string(),
            channel: channel.to_string(),
            applied_ms,
            release_id: None,
        };
        pointer.validate()?;
        Ok(pointer)
    }

    /// Attach (or clear) the immutable release id. The value is validated;
    /// callers pass `Some(id)` only when `versions/<id>/faktor` is
    /// materialized with exactly the pointer's digest.
    pub fn with_release_id(mut self, release_id: Option<String>) -> Result<Self, UpdateError> {
        self.release_id = release_id;
        self.validate()?;
        Ok(self)
    }

    pub fn validate(&self) -> Result<(), UpdateError> {
        if self.schema != INSTALL_POINTER_SCHEMA {
            return Err(UpdateError::Install(format!(
                "install pointer schema {:?} != {INSTALL_POINTER_SCHEMA:?}",
                self.schema
            )));
        }
        if self.artifact.is_empty()
            || self.artifact.contains('/')
            || self.artifact.contains('\\')
            || self.artifact.contains("..")
        {
            return Err(UpdateError::Install(format!(
                "install pointer artifact {:?} is not a plain file name",
                self.artifact
            )));
        }
        if !is_lower_hex(&self.digest, 64) {
            return Err(UpdateError::Install(
                "install pointer digest must be 64 lowercase hex".into(),
            ));
        }
        if let Some(release_id) = &self.release_id {
            crate::release::validate_release_id(release_id)?;
        }
        Ok(())
    }
}

/// The sha256 hex of one file's full content, streamed (bounded memory).
pub fn file_digest(path: &Path) -> Result<String, UpdateError> {
    let mut file = std::fs::File::open(path)
        .map_err(|e| UpdateError::Install(format!("open {}: {e}", path.display())))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|e| UpdateError::Install(format!("read {}: {e}", path.display())))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// The install root and its derived paths.
#[derive(Debug, Clone)]
pub struct InstallLayout {
    root: PathBuf,
}

impl InstallLayout {
    /// Open (creating) one install root. The layout directories are created
    /// here so `stage`/`apply` never race directory creation.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, UpdateError> {
        let root = root.into();
        if root.as_os_str().is_empty() {
            return Err(UpdateError::Config("install root must not be empty".into()));
        }
        let layout = InstallLayout { root };
        fs::create_dir_all(layout.artifacts_dir()).map_err(|e| {
            UpdateError::Install(format!(
                "create artifacts dir {}: {e}",
                layout.artifacts_dir().display()
            ))
        })?;
        fs::create_dir_all(layout.staging_dir()).map_err(|e| {
            UpdateError::Install(format!(
                "create staging dir {}: {e}",
                layout.staging_dir().display()
            ))
        })?;
        Ok(layout)
    }

    /// Open one EXISTING install root read-only: no directory is created and
    /// no side effect is possible. The bootstrap launcher uses this so a
    /// launched process never mutates the layout it is authenticating.
    pub fn open_readonly(root: impl Into<PathBuf>) -> Self {
        InstallLayout { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The pointer file: the single atomic switch of the whole install.
    pub fn pointer_path(&self) -> PathBuf {
        self.root.join("current")
    }

    pub fn artifacts_dir(&self) -> PathBuf {
        self.root.join("artifacts")
    }

    pub fn staging_dir(&self) -> PathBuf {
        self.root.join("staging")
    }

    /// The per-operation staging directory (same filesystem as `artifacts/`).
    pub fn staging_dir_for(&self, op_id: &str) -> PathBuf {
        self.staging_dir().join(op_id)
    }

    /// The content-addressed destination of one artifact.
    pub fn artifact_path(&self, name: &str, digest: &str) -> PathBuf {
        self.artifacts_dir().join(name).join(digest)
    }

    /// Read the installed pointer, if any. A present-but-malformed pointer is
    /// a typed install error (never treated as "nothing installed").
    pub fn read_pointer(&self) -> Result<Option<InstallPointer>, UpdateError> {
        let path = self.pointer_path();
        match fs::read(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(UpdateError::Install(format!(
                "read install pointer {}: {e}",
                path.display()
            ))),
            Ok(bytes) => {
                let pointer: InstallPointer = serde_json::from_slice(&bytes).map_err(|e| {
                    UpdateError::Install(format!(
                        "install pointer {} is not a valid pointer: {e}",
                        path.display()
                    ))
                })?;
                pointer.validate()?;
                Ok(Some(pointer))
            }
        }
    }

    /// Atomically replace the pointer (faktor-fs temp+fsync+rename+parent
    /// fsync). This IS the swap; nothing else changes the installed version.
    pub fn write_pointer(&self, pointer: &InstallPointer) -> Result<(), UpdateError> {
        pointer.validate()?;
        let bytes = serde_json::to_vec(pointer)
            .map_err(|e| UpdateError::Install(format!("install pointer serialization: {e}")))?;
        faktor_fs::atomic::atomic_replace(&self.pointer_path(), &bytes)?;
        Ok(())
    }

    /// Remove the pointer (only used when rolling back a FIRST install: the
    /// previous state was "nothing installed").
    pub fn remove_pointer(&self) -> Result<(), UpdateError> {
        let path = self.pointer_path();
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(UpdateError::Install(format!(
                    "remove install pointer {}: {e}",
                    path.display()
                )))
            }
        }
        if let Some(parent) = path.parent() {
            faktor_fs::atomic::fsync_parent(parent);
        }
        Ok(())
    }

    /// Publish a fully-staged artifact into the content-addressed store.
    /// Idempotent: an already-present artifact with the same digest is kept.
    pub fn publish_staged(
        &self,
        staged: &Path,
        name: &str,
        digest: &str,
    ) -> Result<PathBuf, UpdateError> {
        if !is_lower_hex(digest, 64) {
            return Err(UpdateError::Install(
                "cannot publish an artifact under a non-digest name".into(),
            ));
        }
        let dest = self.artifact_path(name, digest);
        let parent = dest
            .parent()
            .ok_or_else(|| UpdateError::Install("artifact path has no parent".into()))?;
        fs::create_dir_all(parent).map_err(|e| {
            UpdateError::Install(format!("create artifact dir {}: {e}", parent.display()))
        })?;
        faktor_fs::atomic::atomic_adopt(staged, &dest)?;
        Ok(dest)
    }

    /// The sha256 hex of one artifact's full content, streamed (bounded
    /// memory) — the post-swap probe and the pre-apply re-check both use it.
    pub fn artifact_digest(&self, name: &str, digest: &str) -> Result<String, UpdateError> {
        let path = self.artifact_path(name, digest);
        file_digest(&path).map_err(|e| UpdateError::StagedArtifactUnusable {
            artifact: name.to_string(),
            detail: e.to_string(),
        })
    }

    /// Verify the bytes the pointer names are still present AND intact.
    /// A release pointer (`release_id` present) is verified against the
    /// IMMUTABLE `versions/<id>/faktor`; a legacy pointer against the
    /// content-addressed artifact path, exactly as before.
    pub fn verify_installed(&self, pointer: &InstallPointer) -> Result<(), UpdateError> {
        if let Some(release_id) = &pointer.release_id {
            let binary = self.release_binary(release_id);
            let actual = file_digest(&binary).map_err(|e| UpdateError::StagedArtifactUnusable {
                artifact: format!("{release_id}/{RELEASE_BINARY_NAME}"),
                detail: e.to_string(),
            })?;
            if actual != pointer.digest {
                return Err(UpdateError::StagedArtifactUnusable {
                    artifact: format!("{release_id}/{RELEASE_BINARY_NAME}"),
                    detail: format!(
                        "release binary digest {actual} != pointer digest {}",
                        pointer.digest
                    ),
                });
            }
            return Ok(());
        }
        let actual = self.artifact_digest(&pointer.artifact, &pointer.digest)?;
        if actual != pointer.digest {
            return Err(UpdateError::StagedArtifactUnusable {
                artifact: pointer.artifact.clone(),
                detail: format!(
                    "artifact content digest {actual} != pointer digest {}",
                    pointer.digest
                ),
            });
        }
        Ok(())
    }

    /// Remove one operation's staging directory (residue of a crash or a
    /// completed stage). Missing directories are fine.
    pub fn clear_staging(&self, op_id: &str) -> Result<(), UpdateError> {
        let dir = self.staging_dir_for(op_id);
        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(UpdateError::Install(format!(
                "remove staging dir {}: {e}",
                dir.display()
            ))),
        }
    }

    /// The staging file path for one artifact download.
    pub fn staging_file(&self, op_id: &str, name: &str) -> PathBuf {
        self.staging_dir_for(op_id).join(name)
    }
}

/// The post-swap health probe. Implementations receive the layout and the
/// pointer that was just made current; `Err` triggers the automatic
/// rollback.
pub trait HealthProbe: Send + Sync {
    fn probe(&self, layout: &InstallLayout, pointer: &InstallPointer) -> Result<(), UpdateError>;
}

/// The default probe: the swapped artifact must exist, re-hash to the
/// pointer's digest, and the pointer's version/channel must be non-empty.
/// This is the doctor-style probe a live daemon can run without a second
/// store handle or a child process.
pub struct DigestProbe;

impl HealthProbe for DigestProbe {
    fn probe(&self, layout: &InstallLayout, pointer: &InstallPointer) -> Result<(), UpdateError> {
        pointer.validate()?;
        if pointer.version.is_empty() || pointer.channel.is_empty() {
            return Err(UpdateError::HealthFailed {
                detail: "install pointer carries an empty version/channel".into(),
            });
        }
        layout
            .verify_installed(pointer)
            .map_err(|e| UpdateError::HealthFailed {
                detail: e.to_string(),
            })
    }
}

/// The artifact entry a staged operation records (kept next to the layout so
/// the service can hand it to the store without a manifest round-trip).
pub fn artifact_digest_of(artifact: &Artifact) -> String {
    artifact.sha256.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> (tempfile::TempDir, InstallLayout) {
        let dir = tempfile::tempdir().unwrap();
        let layout = InstallLayout::open(dir.path().join("install")).unwrap();
        (dir, layout)
    }

    #[test]
    fn pointer_round_trips_and_detects_corruption() {
        let (_dir, layout) = layout();
        assert!(layout.read_pointer().unwrap().is_none());
        let pointer =
            InstallPointer::new("bundle.tar.gz", &"a".repeat(64), "1.0.0", "stable", 42).unwrap();
        layout.write_pointer(&pointer).unwrap();
        assert_eq!(layout.read_pointer().unwrap(), Some(pointer.clone()));
        // Corrupt the pointer: a typed install error, never "nothing
        // installed".
        std::fs::write(layout.pointer_path(), b"{not json").unwrap();
        assert!(matches!(
            layout.read_pointer(),
            Err(UpdateError::Install(_))
        ));
    }

    #[test]
    fn publishing_is_atomic_and_content_addressed() {
        let (_dir, layout) = layout();
        let digest = crate::manifest::sha256_hex(b"artifact bytes");
        let staged = layout.staging_file("op-1", "bundle.tar.gz");
        std::fs::create_dir_all(layout.staging_dir_for("op-1")).unwrap();
        std::fs::write(&staged, b"artifact bytes").unwrap();
        let dest = layout
            .publish_staged(&staged, "bundle.tar.gz", &digest)
            .unwrap();
        assert!(dest.exists());
        assert!(!staged.exists(), "the staged file is renamed, not copied");
        assert_eq!(
            layout.artifact_digest("bundle.tar.gz", &digest).unwrap(),
            digest
        );
        // Publishing the identical artifact again is idempotent.
        let staged_again = layout.staging_file("op-2", "bundle.tar.gz");
        std::fs::create_dir_all(layout.staging_dir_for("op-2")).unwrap();
        std::fs::write(&staged_again, b"artifact bytes").unwrap();
        layout
            .publish_staged(&staged_again, "bundle.tar.gz", &digest)
            .unwrap();
        assert_eq!(
            layout.artifact_digest("bundle.tar.gz", &digest).unwrap(),
            digest
        );
    }

    #[test]
    fn a_missing_artifact_fails_the_digest_probe() {
        let (_dir, layout) = layout();
        let pointer =
            InstallPointer::new("gone.tar.gz", &"b".repeat(64), "1.0.0", "stable", 1).unwrap();
        let err = DigestProbe.probe(&layout, &pointer).unwrap_err();
        assert_eq!(err.code(), "health_failed");
    }

    #[test]
    fn a_corrupted_artifact_fails_the_digest_probe() {
        let (_dir, layout) = layout();
        let digest = crate::manifest::sha256_hex(b"good");
        let staged = layout.staging_file("op-3", "bundle.tar.gz");
        std::fs::create_dir_all(layout.staging_dir_for("op-3")).unwrap();
        std::fs::write(&staged, b"good").unwrap();
        layout
            .publish_staged(&staged, "bundle.tar.gz", &digest)
            .unwrap();
        let pointer = InstallPointer::new("bundle.tar.gz", &digest, "1.0.0", "stable", 1).unwrap();
        assert!(DigestProbe.probe(&layout, &pointer).is_ok());
        // Flip a byte behind the pointer's back.
        std::fs::write(layout.artifact_path("bundle.tar.gz", &digest), b"evil").unwrap();
        let err = DigestProbe.probe(&layout, &pointer).unwrap_err();
        assert_eq!(err.code(), "health_failed");
    }

    #[test]
    fn malformed_pointers_are_refused_by_validate() {
        assert!(InstallPointer::new("../evil", &"a".repeat(64), "1", "stable", 1).is_err());
        assert!(InstallPointer::new("b.tar.gz", "nothex", "1", "stable", 1).is_err());
        let mut pointer =
            InstallPointer::new("b.tar.gz", &"a".repeat(64), "1", "stable", 1).unwrap();
        pointer.schema = "faktor-install-pointer/v9".into();
        assert!(pointer.validate().is_err());
    }
}
