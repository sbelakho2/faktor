//! The update lifecycle state machine.
//!
//! `check` → `stage` → `apply` with `rollback` and crash `recover`:
//!
//! 1. `check` authenticates the manifest (signature, expiry, channel pin),
//!    checks the running components, selects the host's artifact, and
//!    records a durable `check` operation. It never downloads;
//! 2. `stage` re-verifies the manifest (no TOCTOU: the bytes are the input),
//!    streams the artifact through the checked transport into the same
//!    filesystem's staging area under the configured byte bound, verifies
//!    the sha256, publishes it content-addressed, and records a durable
//!    `stage` row. The install is untouched;
//! 3. `apply` re-hashes the staged artifact, records the apply BEFORE the
//!    swap, atomically replaces the `current` pointer, and runs the health
//!    probe. A probe failure automatically restores the previous pointer
//!    (exact previous artifact + digest) and records a `rollback` row;
//! 4. `recover` resolves `running` rows left by a crash: an interrupted
//!    stage is abandoned with the install intact; an interrupted apply is
//!    either resumed (pointer already swapped → probe → applied) or rolled
//!    back (probe failure), never guessed.
//!
//! Every step is role-gated at the server layer (`apply` requires the admin
//! role in the control plane) and every transition carries before/after
//! versions and artifact digests.

use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use tokio::io::AsyncWriteExt as _;

use crate::channel::Channel;
use crate::compat::{self, CompatibilityReport, RunningComponents};
use crate::error::UpdateError;
use crate::install::{DigestProbe, HealthProbe, InstallLayout, InstallPointer};
use crate::keys::TrustedKeys;
use crate::manifest::{self, Artifact};
use crate::store::{UpdateOpKind, UpdateOpStatus, UpdateOperation, UpdaterStore};
use crate::transport::ArtifactFetcher;

/// The native protocol schema version this runtime speaks (the `schema`
/// compatibility entry is checked against it).
pub const NATIVE_SCHEMA_SUPPORTED: u32 = 1;

/// The distribution vocabulary for one host: the certification tooling
/// derives tokens from `uname` (`darwin`, `arm64`, `x86_64`, `linux`,
/// `windows`), while `std::env::consts` says `macos`/`aarch64`. Normalizing
/// here means an artifact recorded by the release scripts is selected on the
/// host it was built for. Unknown tokens pass through unchanged (never
/// guessed).
pub fn normalize_host_os_arch(os: &str, arch: &str) -> (String, String) {
    let os = match os {
        "macos" => "darwin",
        other => other,
    };
    let arch = match arch {
        "aarch64" => "arm64",
        "amd64" => "x86_64",
        other => other,
    };
    (os.to_string(), arch.to_string())
}

/// One updater instance's immutable configuration.
#[derive(Debug, Clone)]
pub struct UpdaterConfig {
    /// The configured channel: only manifests this channel accepts advance.
    pub channel: Channel,
    pub install_root: PathBuf,
    /// The operator key allowlist. Empty = every manifest refused.
    pub keys: TrustedKeys,
    pub max_artifact_bytes: u64,
    pub clock_skew_ms: i64,
    pub host_os: String,
    pub host_arch: String,
    /// The running local version (daemon/cli); the compatibility check
    /// applies it to the `cli` and `daemon` entries unless the caller
    /// overrides them.
    pub local_version: String,
}

/// The artifact summary rendered on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArtifactView {
    pub name: String,
    pub os: String,
    pub arch: String,
    pub sha256: String,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

impl From<&Artifact> for ArtifactView {
    fn from(artifact: &Artifact) -> Self {
        ArtifactView {
            name: artifact.name.clone(),
            os: artifact.os.clone(),
            arch: artifact.arch.clone(),
            sha256: artifact.sha256.clone(),
            url: artifact.url.clone(),
            size: artifact.size,
        }
    }
}

/// One compatibility row on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CompatibilityView {
    pub component: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub running: Option<String>,
    pub range: String,
    pub verdict: &'static str,
}

impl From<&compat::ComponentVerdict> for CompatibilityView {
    fn from(row: &compat::ComponentVerdict) -> Self {
        CompatibilityView {
            component: row.component.as_str(),
            running: row.running.clone(),
            range: row.range.clone(),
            verdict: row.verdict.as_str(),
        }
    }
}

fn compatibility_views(report: &CompatibilityReport) -> Vec<CompatibilityView> {
    report.rows.iter().map(CompatibilityView::from).collect()
}

/// The outcome of one successful `check`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckOutcome {
    pub op_id: String,
    pub version: String,
    pub channel: String,
    pub commit: String,
    pub identity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub certification_level: Option<String>,
    pub artifact: ArtifactView,
    pub compatible: bool,
    pub compatibility: Vec<CompatibilityView>,
}

/// The outcome of one `stage` (also used for an idempotent replay).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StageOutcome {
    pub op_id: String,
    pub version: String,
    pub channel: String,
    pub artifact: ArtifactView,
    pub digest: String,
    pub bytes: u64,
    /// True when the same idempotency key replayed an already-staged
    /// operation instead of downloading again.
    pub idempotent: bool,
}

/// A staged operation summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StageSummary {
    pub op_id: String,
    pub version: String,
    pub digest: String,
    pub channel: Option<String>,
    pub artifact: Option<String>,
}

/// The outcome of `apply`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum ApplyOutcome {
    Applied {
        op_id: String,
        version: String,
        digest: String,
        artifact: String,
    },
    RolledBack {
        op_id: String,
        version: String,
        digest: String,
        artifact: String,
        reason: String,
    },
}

/// What recovery did with one crash residue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum RecoveryOutcome {
    /// An interrupted stage/check was abandoned; the install was untouched.
    Abandoned { op_id: String, detail: String },
    /// An interrupted apply's swap was confirmed healthy.
    Resumed { op_id: String },
    /// An interrupted apply failed its probe and the previous artifact was
    /// restored.
    RolledBack { op_id: String, detail: String },
    /// The durable state could not prove either side; verification is
    /// forced (never a silent success).
    NeedsVerification { op_id: String, detail: String },
}

/// The full updater status view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusView {
    pub channel: String,
    pub os: String,
    pub arch: String,
    pub local_version: String,
    pub keys: Vec<String>,
    pub max_artifact_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed: Option<InstallPointer>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub staged: Option<StageSummary>,
    pub operations: Vec<UpdateOperation>,
    /// True while a crash residue still needs `recover`/verification.
    pub recovery_required: bool,
}

/// One updater instance: config + durable store + checked transport +
/// health probe.
pub struct Updater {
    config: UpdaterConfig,
    layout: InstallLayout,
    store: Arc<dyn UpdaterStore>,
    fetcher: Arc<dyn ArtifactFetcher>,
    probe: Arc<dyn HealthProbe>,
}

impl Updater {
    pub fn new(
        config: UpdaterConfig,
        store: Arc<dyn UpdaterStore>,
        fetcher: Arc<dyn ArtifactFetcher>,
        probe: Arc<dyn HealthProbe>,
    ) -> Result<Self, UpdateError> {
        let layout = InstallLayout::open(config.install_root.clone())?;
        let (host_os, host_arch) = normalize_host_os_arch(&config.host_os, &config.host_arch);
        let config = UpdaterConfig {
            host_os,
            host_arch,
            ..config
        };
        Ok(Updater {
            config,
            layout,
            store,
            fetcher,
            probe,
        })
    }

    /// The default digest probe plus a caller-provided transport.
    pub fn with_default_probe(
        config: UpdaterConfig,
        store: Arc<dyn UpdaterStore>,
        fetcher: Arc<dyn ArtifactFetcher>,
    ) -> Result<Self, UpdateError> {
        Updater::new(config, store, fetcher, Arc::new(DigestProbe))
    }

    pub fn config(&self) -> &UpdaterConfig {
        &self.config
    }

    pub fn layout(&self) -> &InstallLayout {
        &self.layout
    }

    pub fn store(&self) -> &Arc<dyn UpdaterStore> {
        &self.store
    }

    /// The running components this host can attest to (the local cli/daemon
    /// version plus the native schema); callers may override any entry to
    /// report attached components (VS Code / JetBrains panels).
    pub fn running_components(
        &self,
        cli: Option<&str>,
        daemon: Option<&str>,
        vscode: Option<&str>,
        jetbrains: Option<&str>,
    ) -> Result<RunningComponents, UpdateError> {
        RunningComponents::from_strs(
            Some(cli.unwrap_or(&self.config.local_version)),
            Some(daemon.unwrap_or(&self.config.local_version)),
            vscode,
            jetbrains,
            Some(NATIVE_SCHEMA_SUPPORTED),
        )
        .map_err(UpdateError::Config)
    }

    /// Refuse to proceed while an unresolved (crashed) operation exists.
    fn ensure_no_running(&self) -> Result<(), UpdateError> {
        let running = self.store.running()?;
        if let Some(op) = running.first() {
            return Err(UpdateError::Conflict(format!(
                "unresolved update operation {} ({}); run recovery before a new update",
                op.id,
                op.kind.as_str()
            )));
        }
        Ok(())
    }

    fn latest_staged(&self) -> Result<UpdateOperation, UpdateError> {
        self.store
            .latest(UpdateOpKind::Stage, UpdateOpStatus::Staged)?
            .ok_or(UpdateError::NothingStaged)
    }

    /// `GET status` data: pointer, staged op, recent operations.
    pub fn status(&self, now_ms: i64) -> Result<StatusView, UpdateError> {
        let installed = self.layout.read_pointer()?;
        let staged = self
            .store
            .latest(UpdateOpKind::Stage, UpdateOpStatus::Staged)?
            .map(|op| StageSummary {
                op_id: op.id.to_string(),
                version: op.after_version.clone().unwrap_or_default(),
                digest: op.after_digest.clone().unwrap_or_default(),
                channel: op.channel.clone(),
                artifact: op.artifact.clone(),
            });
        let operations = self.store.list(64)?;
        let recovery_required = operations.iter().any(|op| op.is_running())
            || operations
                .iter()
                .any(|op| op.status == UpdateOpStatus::Unverified);
        let _ = now_ms;
        Ok(StatusView {
            channel: self.config.channel.to_string(),
            os: self.config.host_os.clone(),
            arch: self.config.host_arch.clone(),
            local_version: self.config.local_version.clone(),
            keys: self
                .config
                .keys
                .identities()
                .into_iter()
                .map(str::to_string)
                .collect(),
            max_artifact_bytes: self.config.max_artifact_bytes,
            installed,
            staged,
            operations,
            recovery_required,
        })
    }

    /// Authenticate + compatibility-check one manifest and record the
    /// `check` operation.
    pub fn check(
        &self,
        manifest_bytes: &[u8],
        running: &RunningComponents,
        now_ms: i64,
    ) -> Result<CheckOutcome, UpdateError> {
        let verified = manifest::verify_manifest(
            manifest_bytes,
            &self.config.keys,
            self.config.channel,
            now_ms,
            self.config.clock_skew_ms,
        )?;
        let manifest = verified.manifest();
        let report = compat::check(&manifest.compatibility, running);

        let mut op = UpdateOperation::new(
            UpdateOpKind::Check,
            now_ms,
            Some(format!(
                "manifest {} verified against identity {}",
                manifest.version,
                verified.identity()
            )),
        );
        op.channel = Some(manifest.channel.to_string());
        op.after_version = Some(manifest.version.clone());
        op.identity = Some(verified.identity().to_string());
        op.certification_level = manifest.certification.as_ref().map(|c| c.level.clone());

        if report.refused {
            let detail = format!("incompatible: {report}");
            op.status = UpdateOpStatus::Failed;
            op.detail = Some(detail);
            op.updated_ms = now_ms;
            self.store.insert(&op)?;
            return Err(UpdateError::Incompatible(report));
        }

        let artifact = manifest
            .artifact_for_host(&self.config.host_os, &self.config.host_arch)
            .ok_or_else(|| {
                let error =
                    UpdateError::Refused(crate::error::ManifestRefusal::NoArtifactForHost {
                        os: self.config.host_os.clone(),
                        arch: self.config.host_arch.clone(),
                    });
                // The operation row is recorded even when no artifact
                // matches this host: the refusal is durable evidence.
                let _ = self.record_failed_check(&mut op, &error, now_ms);
                error
            })?;

        op.artifact = Some(artifact.name.clone());
        op.after_digest = Some(artifact.sha256.clone());
        op.status = UpdateOpStatus::Succeeded;
        op.updated_ms = now_ms;
        if artifact
            .size
            .is_some_and(|size| size > self.config.max_artifact_bytes)
        {
            let e = UpdateError::ArtifactTooLarge {
                artifact: artifact.name.clone(),
                max_bytes: self.config.max_artifact_bytes,
            };
            self.record_failed_check(&mut op, &e, now_ms)?;
            return Err(e);
        }
        self.store.insert(&op)?;

        Ok(CheckOutcome {
            op_id: op.id.to_string(),
            version: manifest.version.clone(),
            channel: manifest.channel.to_string(),
            commit: manifest.commit.clone(),
            identity: verified.identity().to_string(),
            certification_level: manifest.certification.as_ref().map(|c| c.level.clone()),
            artifact: ArtifactView::from(artifact),
            compatible: true,
            compatibility: compatibility_views(&report),
        })
    }

    fn record_failed_check(
        &self,
        op: &mut UpdateOperation,
        error: &UpdateError,
        now_ms: i64,
    ) -> Result<(), UpdateError> {
        op.status = UpdateOpStatus::Failed;
        op.detail = Some(error.to_string());
        op.updated_ms = now_ms;
        self.store.insert(op)?;
        Ok(())
    }

    /// Download + verify + publish one artifact, leaving the install
    /// untouched.
    pub async fn stage(
        &self,
        manifest_bytes: &[u8],
        running: &RunningComponents,
        idempotency_key: Option<&str>,
        now_ms: i64,
    ) -> Result<StageOutcome, UpdateError> {
        self.ensure_no_running()?;
        // A stage always re-runs the full verification; the check row is
        // recorded as its own durable step.
        let outcome = self.check(manifest_bytes, running, now_ms)?;
        let verified = manifest::verify_manifest(
            manifest_bytes,
            &self.config.keys,
            self.config.channel,
            now_ms,
            self.config.clock_skew_ms,
        )?;
        let manifest = verified.manifest();
        let artifact = manifest
            .artifact_for_host(&self.config.host_os, &self.config.host_arch)
            .ok_or_else(|| {
                UpdateError::Refused(crate::error::ManifestRefusal::NoArtifactForHost {
                    os: self.config.host_os.clone(),
                    arch: self.config.host_arch.clone(),
                })
            })?
            .clone();

        // Idempotent replay: the same key + the same target artifact returns
        // the recorded outcome without touching the network.
        if let Some(key) = idempotency_key {
            if let Some(existing) = self.store.by_key(key)? {
                if existing.kind == UpdateOpKind::Stage && existing.status == UpdateOpStatus::Staged
                {
                    if existing.after_digest.as_deref() == Some(artifact.sha256.as_str())
                        && existing.artifact.as_deref() == Some(artifact.name.as_str())
                    {
                        return Ok(self.stage_outcome(&existing, true));
                    }
                    return Err(UpdateError::Conflict(format!(
                        "idempotency key {key:?} was used for a different artifact"
                    )));
                }
            }
        }

        let current = self.layout.read_pointer()?;
        let mut op = UpdateOperation::new(
            UpdateOpKind::Stage,
            now_ms,
            Some(format!(
                "staging {} from {}",
                artifact.name,
                verified.identity()
            )),
        );
        op.channel = Some(manifest.channel.to_string());
        op.before_version = current.as_ref().map(|p| p.version.clone());
        op.before_digest = current.as_ref().map(|p| p.digest.clone());
        op.before_artifact = current.as_ref().map(|p| p.artifact.clone());
        op.after_version = Some(manifest.version.clone());
        op.after_digest = Some(artifact.sha256.clone());
        op.artifact = Some(artifact.name.clone());
        op.identity = Some(verified.identity().to_string());
        op.certification_level = manifest.certification.as_ref().map(|c| c.level.clone());
        op.idempotency_key = idempotency_key.map(str::to_string);
        self.store.insert(&op)?;

        match self
            .download_and_publish(&op.id.to_string(), &artifact)
            .await
        {
            Ok(bytes) => {
                op.status = UpdateOpStatus::Staged;
                op.updated_ms = now_ms;
                op.detail = Some(format!(
                    "{} bytes staged; check operation {}",
                    bytes, outcome.op_id
                ));
                self.store.update(&op)?;
                Ok(StageOutcome {
                    op_id: op.id.to_string(),
                    version: manifest.version.clone(),
                    channel: manifest.channel.to_string(),
                    artifact: ArtifactView::from(&artifact),
                    digest: artifact.sha256.clone(),
                    bytes,
                    idempotent: false,
                })
            }
            Err(e) => {
                op.status = UpdateOpStatus::Failed;
                op.updated_ms = now_ms;
                op.detail = Some(e.to_string());
                self.store.update(&op)?;
                let _ = self.layout.clear_staging(op.id.as_str());
                Err(e)
            }
        }
    }

    fn stage_outcome(&self, op: &UpdateOperation, idempotent: bool) -> StageOutcome {
        StageOutcome {
            op_id: op.id.to_string(),
            version: op.after_version.clone().unwrap_or_default(),
            channel: op.channel.clone().unwrap_or_default(),
            artifact: ArtifactView {
                name: op.artifact.clone().unwrap_or_default(),
                os: self.config.host_os.clone(),
                arch: self.config.host_arch.clone(),
                sha256: op.after_digest.clone().unwrap_or_default(),
                url: String::new(),
                size: None,
            },
            digest: op.after_digest.clone().unwrap_or_default(),
            bytes: 0,
            idempotent,
        }
    }

    async fn download_and_publish(
        &self,
        op_id: &str,
        artifact: &Artifact,
    ) -> Result<u64, UpdateError> {
        if let Some(size) = artifact.size {
            if size > self.config.max_artifact_bytes {
                return Err(UpdateError::ArtifactTooLarge {
                    artifact: artifact.name.clone(),
                    max_bytes: self.config.max_artifact_bytes,
                });
            }
        }
        self.layout.clear_staging(op_id)?;
        let dir = self.layout.staging_dir_for(op_id);
        std::fs::create_dir_all(&dir).map_err(|e| {
            UpdateError::Install(format!("create staging dir {}: {e}", dir.display()))
        })?;
        let staged = self.layout.staging_file(op_id, &artifact.name);
        let mut file = tokio::fs::File::create(&staged)
            .await
            .map_err(|e| UpdateError::Install(format!("create {}: {e}", staged.display())))?;
        let streamed = self
            .fetcher
            .fetch_to(&artifact.url, self.config.max_artifact_bytes, &mut file)
            .await?;
        file.flush()
            .await
            .map_err(|e| UpdateError::Install(format!("flush {}: {e}", staged.display())))?;
        // The staged bytes are durable before anything is published.
        file.sync_all()
            .await
            .map_err(|e| UpdateError::Install(format!("fsync {}: {e}", staged.display())))?;
        drop(file);

        let actual = crate::install::file_digest(&staged)?;
        if actual != artifact.sha256 {
            return Err(UpdateError::DigestMismatch {
                artifact: artifact.name.clone(),
                expected: artifact.sha256.clone(),
                actual,
            });
        }
        self.layout
            .publish_staged(&staged, &artifact.name, &artifact.sha256)?;
        self.layout.clear_staging(op_id)?;
        Ok(streamed)
    }

    /// Swap the pointer to the staged artifact, run the probe, and roll back
    /// automatically on a probe failure.
    pub fn apply(&self, now_ms: i64) -> Result<ApplyOutcome, UpdateError> {
        self.ensure_no_running()?;
        let staged = self.latest_staged()?;
        // An already-applied staged artifact is not silently re-applied: the
        // operator asked for an update, and it is installed.
        if let Some(last) = self
            .store
            .latest(UpdateOpKind::Apply, UpdateOpStatus::Applied)?
        {
            if last.after_digest.is_some() && last.after_digest == staged.after_digest {
                return Err(UpdateError::Conflict(format!(
                    "the staged artifact {} ({}) is already applied",
                    staged.after_version.as_deref().unwrap_or_default(),
                    staged.after_digest.as_deref().unwrap_or_default()
                )));
            }
        }
        let target = InstallPointer::new(
            staged.artifact.as_deref().ok_or_else(|| {
                UpdateError::Conflict("staged operation carries no artifact".into())
            })?,
            staged.after_digest.as_deref().ok_or_else(|| {
                UpdateError::Conflict("staged operation carries no digest".into())
            })?,
            staged.after_version.as_deref().unwrap_or_default(),
            staged.channel.as_deref().unwrap_or("stable"),
            now_ms,
        )?;
        // The staged artifact is re-hashed before the swap (deterministic FS
        // op verifying the expected resulting hash).
        self.layout.verify_installed(&target)?;

        let current = self.layout.read_pointer()?;
        let mut op = UpdateOperation::new(
            UpdateOpKind::Apply,
            now_ms,
            Some(format!("applying {} ({})", target.version, staged.id)),
        );
        op.channel = staged.channel.clone();
        op.before_version = current.as_ref().map(|p| p.version.clone());
        op.before_digest = current.as_ref().map(|p| p.digest.clone());
        op.before_artifact = current.as_ref().map(|p| p.artifact.clone());
        op.after_version = Some(target.version.clone());
        op.after_digest = Some(target.digest.clone());
        op.artifact = Some(target.artifact.clone());
        op.identity = staged.identity.clone();
        op.certification_level = staged.certification_level.clone();
        // Durable BEFORE the swap: a crash after this row exists is
        // recoverable from the pointer alone.
        self.store.insert(&op)?;

        if let Err(e) = self.layout.write_pointer(&target) {
            op.status = UpdateOpStatus::Failed;
            op.updated_ms = now_ms;
            op.detail = Some(format!("pointer swap failed: {e}"));
            self.store.update(&op)?;
            return Err(e);
        }

        match self.probe.probe(&self.layout, &target) {
            Ok(()) => {
                op.status = UpdateOpStatus::Applied;
                op.updated_ms = now_ms;
                op.detail = Some(format!(
                    "applied {} and the health probe passed",
                    target.version
                ));
                self.store.update(&op)?;
                Ok(ApplyOutcome::Applied {
                    op_id: op.id.to_string(),
                    version: target.version,
                    digest: target.digest,
                    artifact: target.artifact,
                })
            }
            Err(probe_error) => {
                let reason = probe_error.to_string();
                let restored = self.restore_previous(&current, &reason);
                op.status = UpdateOpStatus::RolledBack;
                op.updated_ms = now_ms;
                op.detail = Some(format!("health probe failed: {reason}; {restored}"));
                self.store.update(&op)?;
                self.record_rollback(&current, &target, now_ms, &reason)?;
                // The rollback restored the previous pointer; report it as
                // an outcome (the install is consistent), with the reason.
                Ok(ApplyOutcome::RolledBack {
                    op_id: op.id.to_string(),
                    version: target.version,
                    digest: target.digest,
                    artifact: target.artifact,
                    reason,
                })
            }
        }
    }

    /// Restore the previous pointer (or remove it for a first install).
    /// `reason` is logged context only; the pointer write is the recovery.
    fn restore_previous(&self, previous: &Option<InstallPointer>, reason: &str) -> String {
        tracing::warn!("updater rollback: {reason}");
        match previous {
            Some(pointer) => match self.layout.write_pointer(pointer) {
                Ok(()) => {
                    // Confirm the restored install, but never fail the
                    // rollback itself: the pointer write is the recovery.
                    match self.probe.probe(&self.layout, pointer) {
                        Ok(()) => format!("restored {} ({})", pointer.version, pointer.digest),
                        Err(e) => format!(
                            "restored {} ({}) but its probe reports: {e}",
                            pointer.version, pointer.digest
                        ),
                    }
                }
                Err(e) => format!("FAILED to restore the previous pointer: {e}"),
            },
            None => match self.layout.remove_pointer() {
                Ok(()) => "removed the pointer (no previous install)".to_string(),
                Err(e) => format!("FAILED to remove the pointer: {e}"),
            },
        }
    }

    fn record_rollback(
        &self,
        previous: &Option<InstallPointer>,
        target: &InstallPointer,
        now_ms: i64,
        reason: &str,
    ) -> Result<(), UpdateError> {
        let mut rollback = UpdateOperation::new(
            UpdateOpKind::Rollback,
            now_ms,
            Some(format!(
                "automatic rollback after a health-probe failure: {reason}"
            )),
        );
        rollback.status = UpdateOpStatus::RolledBack;
        rollback.channel = Some(target.channel.clone());
        rollback.before_version = Some(target.version.clone());
        rollback.before_digest = Some(target.digest.clone());
        rollback.before_artifact = Some(target.artifact.clone());
        rollback.after_version = previous.as_ref().map(|p| p.version.clone());
        rollback.after_digest = previous.as_ref().map(|p| p.digest.clone());
        rollback.artifact = previous.as_ref().map(|p| p.artifact.clone());
        rollback.updated_ms = now_ms;
        self.store.insert(&rollback)?;
        Ok(())
    }

    /// Explicit rollback to the previous artifact of the last applied
    /// operation.
    pub fn rollback(&self, now_ms: i64) -> Result<ApplyOutcome, UpdateError> {
        self.ensure_no_running()?;
        let last = self
            .store
            .latest(UpdateOpKind::Apply, UpdateOpStatus::Applied)?
            .ok_or_else(|| {
                UpdateError::Conflict("no applied update operation to roll back".into())
            })?;
        let current = self.layout.read_pointer()?.ok_or_else(|| {
            UpdateError::Conflict("the install has no pointer to roll back".into())
        })?;
        if Some(current.digest.as_str()) != last.after_digest.as_deref() {
            return Err(UpdateError::Conflict(format!(
                "the installed digest {} does not match the last applied operation ({})",
                current.digest,
                last.after_digest.unwrap_or_default()
            )));
        }
        let (Some(previous_digest), Some(previous_version)) =
            (last.before_digest.clone(), last.before_version.clone())
        else {
            return Err(UpdateError::Conflict(
                "the last applied operation had no previous artifact; there is nothing to roll back to"
                    .into(),
            ));
        };
        let previous_artifact = last.before_artifact.clone().unwrap_or_default();
        let target = InstallPointer::new(
            &previous_artifact,
            &previous_digest,
            &previous_version,
            last.channel.as_deref().unwrap_or("stable"),
            now_ms,
        )?;
        self.layout.verify_installed(&target)?;

        let mut op = UpdateOperation::new(
            UpdateOpKind::Rollback,
            now_ms,
            Some(format!(
                "explicit rollback to {} ({previous_digest})",
                target.version
            )),
        );
        op.channel = last.channel.clone();
        op.before_version = Some(current.version.clone());
        op.before_digest = Some(current.digest.clone());
        op.before_artifact = Some(current.artifact.clone());
        op.after_version = Some(target.version.clone());
        op.after_digest = Some(target.digest.clone());
        op.artifact = Some(target.artifact.clone());
        self.store.insert(&op)?;
        self.layout.write_pointer(&target)?;
        match self.probe.probe(&self.layout, &target) {
            Ok(()) => {
                op.status = UpdateOpStatus::RolledBack;
                op.updated_ms = now_ms;
                self.store.update(&op)?;
                Ok(ApplyOutcome::RolledBack {
                    op_id: op.id.to_string(),
                    version: target.version,
                    digest: target.digest,
                    artifact: target.artifact,
                    reason: "explicit rollback requested".into(),
                })
            }
            Err(e) => {
                op.status = UpdateOpStatus::Unverified;
                op.updated_ms = now_ms;
                op.detail = Some(format!(
                    "rolled back to {} but its probe failed: {e}; forced verification",
                    target.version
                ));
                self.store.update(&op)?;
                Err(UpdateError::HealthFailed {
                    detail: format!(
                        "rollback target {} ({}) is unhealthy: {e}",
                        target.version, target.digest
                    ),
                })
            }
        }
    }

    /// Resolve every crash residue. Never re-runs a download; an
    /// interrupted apply is resumed (probe) or rolled back.
    pub fn recover(&self, now_ms: i64) -> Result<Vec<RecoveryOutcome>, UpdateError> {
        let mut outcomes = Vec::new();
        for mut op in self.store.running()? {
            match op.kind {
                UpdateOpKind::Check => {
                    op.status = UpdateOpStatus::Failed;
                    op.updated_ms = now_ms;
                    op.detail = Some("crashed during check; nothing changed".into());
                    self.store.update(&op)?;
                    outcomes.push(RecoveryOutcome::Abandoned {
                        op_id: op.id.to_string(),
                        detail: "crashed during check; nothing changed".into(),
                    });
                }
                UpdateOpKind::Stage => {
                    self.layout.clear_staging(op.id.as_str())?;
                    op.status = UpdateOpStatus::Failed;
                    op.updated_ms = now_ms;
                    op.detail = Some("crashed during stage; the install was never touched".into());
                    self.store.update(&op)?;
                    outcomes.push(RecoveryOutcome::Abandoned {
                        op_id: op.id.to_string(),
                        detail: "crashed during stage; the install was never touched".into(),
                    });
                }
                UpdateOpKind::Apply => {
                    outcomes.push(self.recover_apply(&mut op, now_ms)?);
                }
                UpdateOpKind::Rollback => {
                    outcomes.push(self.recover_rollback(&mut op, now_ms)?);
                }
            }
        }
        Ok(outcomes)
    }

    fn recover_apply(
        &self,
        op: &mut UpdateOperation,
        now_ms: i64,
    ) -> Result<RecoveryOutcome, UpdateError> {
        let current = self.layout.read_pointer()?;
        let target_digest = op.after_digest.clone().unwrap_or_default();
        let before_digest = op.before_digest.clone();
        let pointer_digest = current.as_ref().map(|p| p.digest.clone());
        if pointer_digest.as_deref() == Some(target_digest.as_str()) {
            // The swap happened; the probe never ran. Resume by probing.
            let target = current.clone().expect("pointer digest implies a pointer");
            match self.probe.probe(&self.layout, &target) {
                Ok(()) => {
                    op.status = UpdateOpStatus::Applied;
                    op.updated_ms = now_ms;
                    op.detail = Some("recovered: the interrupted swap is healthy".into());
                    self.store.update(op)?;
                    Ok(RecoveryOutcome::Resumed {
                        op_id: op.id.to_string(),
                    })
                }
                Err(e) => {
                    let previous = self.previous_pointer(op)?;
                    let restored = self.restore_previous(&previous, &e.to_string());
                    op.status = UpdateOpStatus::RolledBack;
                    op.updated_ms = now_ms;
                    op.detail = Some(format!(
                        "recovered: the interrupted swap failed its probe ({e}); {restored}"
                    ));
                    self.store.update(op)?;
                    self.record_rollback(&previous, &target, now_ms, &e.to_string())?;
                    Ok(RecoveryOutcome::RolledBack {
                        op_id: op.id.to_string(),
                        detail: e.to_string(),
                    })
                }
            }
        } else if pointer_digest == before_digest {
            op.status = UpdateOpStatus::Failed;
            op.updated_ms = now_ms;
            op.detail =
                Some("crashed before the pointer swap; the previous install is intact".into());
            self.store.update(op)?;
            Ok(RecoveryOutcome::Abandoned {
                op_id: op.id.to_string(),
                detail: "crashed before the pointer swap; the previous install is intact".into(),
            })
        } else {
            op.status = UpdateOpStatus::Unverified;
            op.updated_ms = now_ms;
            op.detail = Some(format!(
                "the pointer digest {} matches neither side of the operation; verification forced",
                pointer_digest.unwrap_or_else(|| "none".into())
            ));
            self.store.update(op)?;
            Ok(RecoveryOutcome::NeedsVerification {
                op_id: op.id.to_string(),
                detail: "the pointer matches neither side of the interrupted apply".into(),
            })
        }
    }

    fn recover_rollback(
        &self,
        op: &mut UpdateOperation,
        now_ms: i64,
    ) -> Result<RecoveryOutcome, UpdateError> {
        let current = self.layout.read_pointer()?;
        let target_digest = op.after_digest.clone().unwrap_or_default();
        if current.as_ref().map(|p| p.digest.as_str()) == Some(target_digest.as_str()) {
            op.status = UpdateOpStatus::RolledBack;
            op.updated_ms = now_ms;
            op.detail = Some("recovered: the rollback target is in place".into());
            self.store.update(op)?;
            Ok(RecoveryOutcome::Resumed {
                op_id: op.id.to_string(),
            })
        } else {
            op.status = UpdateOpStatus::Unverified;
            op.updated_ms = now_ms;
            op.detail =
                Some("the pointer does not name the rollback target; verification forced".into());
            self.store.update(op)?;
            Ok(RecoveryOutcome::NeedsVerification {
                op_id: op.id.to_string(),
                detail: "the pointer does not name the interrupted rollback's target".into(),
            })
        }
    }

    fn previous_pointer(
        &self,
        op: &UpdateOperation,
    ) -> Result<Option<InstallPointer>, UpdateError> {
        match (&op.before_artifact, &op.before_digest, &op.before_version) {
            (Some(artifact), Some(digest), version) => Ok(Some(InstallPointer::new(
                artifact,
                digest,
                version.as_deref().unwrap_or_default(),
                op.channel.as_deref().unwrap_or("stable"),
                op.created_ms,
            )?)),
            _ => Ok(None),
        }
    }
}

/// A fetcher that refuses everything: the honest default for hosts with no
/// network transport wired (never a silent no-op).
pub struct RefusingFetcher;

#[async_trait::async_trait]
impl ArtifactFetcher for RefusingFetcher {
    async fn fetch_to(
        &self,
        url: &str,
        _max_bytes: u64,
        _sink: &mut (dyn tokio::io::AsyncWrite + Send + Unpin),
    ) -> Result<u64, UpdateError> {
        Err(UpdateError::Transport {
            artifact: url.to_string(),
            detail: "no artifact transport is wired on this host".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(root: PathBuf) -> UpdaterConfig {
        UpdaterConfig {
            channel: Channel::Stable,
            install_root: root,
            keys: TrustedKeys::empty(),
            max_artifact_bytes: 1024 * 1024,
            clock_skew_ms: manifest::DEFAULT_CLOCK_SKEW_MS,
            host_os: "darwin".into(),
            host_arch: "arm64".into(),
            local_version: "0.1.0".into(),
        }
    }

    #[test]
    fn an_empty_allowlist_refuses_before_any_download() {
        let dir = tempfile::tempdir().unwrap();
        let updater = Updater::with_default_probe(
            config(dir.path().join("install")),
            Arc::new(crate::store::MemoryUpdaterStore::new()),
            Arc::new(RefusingFetcher),
        )
        .unwrap();
        let running = updater.running_components(None, None, None, None).unwrap();
        let err = updater.check(b"{}", &running, 1_000).unwrap_err();
        assert_eq!(err.code(), "manifest_malformed");
        // A well-shaped but unsigned manifest is refused as unsigned.
        let signed_shape = serde_json::to_vec(&serde_json::json!({
            "schema": manifest::UPDATE_MANIFEST_SCHEMA,
            "channel": "stable",
            "version": "0.2.0",
            "commit": "a".repeat(40),
            "artifacts": [{
                "name": "b.tar.gz", "os": "darwin", "arch": "arm64",
                "sha256": "b".repeat(64),
                "url": "https://example.test/b.tar.gz",
            }],
            "compatibility": {
                "cli": {"min": "0.1.0", "max": "1.0.0"},
                "daemon": {"min": "0.1.0", "max": "1.0.0"},
                "vscode": {"min": "*", "max": "*"},
                "jetbrains": {"min": "*", "max": "*"},
                "schema": {"min": 1, "max": 1},
            },
            "issued_at": 500,
            "expires_at": 5_000,
        }))
        .unwrap();
        let err = updater.check(&signed_shape, &running, 1_000).unwrap_err();
        assert_eq!(err.code(), "manifest_unsigned");
    }

    #[test]
    fn host_os_arch_normalization_matches_the_release_vocabulary() {
        assert_eq!(
            normalize_host_os_arch("macos", "aarch64"),
            ("darwin".to_string(), "arm64".to_string())
        );
        assert_eq!(
            normalize_host_os_arch("darwin", "arm64"),
            ("darwin".to_string(), "arm64".to_string()),
            "already-canonical tokens are idempotent"
        );
        assert_eq!(
            normalize_host_os_arch("linux", "x86_64"),
            ("linux".to_string(), "x86_64".to_string())
        );
        assert_eq!(
            normalize_host_os_arch("windows", "amd64"),
            ("windows".to_string(), "x86_64".to_string())
        );
        // The updater reports the normalized tokens.
        let dir = tempfile::tempdir().unwrap();
        let updater = Updater::with_default_probe(
            UpdaterConfig {
                host_os: "macos".into(),
                host_arch: "aarch64".into(),
                ..config(dir.path().join("install"))
            },
            Arc::new(crate::store::MemoryUpdaterStore::new()),
            Arc::new(RefusingFetcher),
        )
        .unwrap();
        let status = updater.status(1).unwrap();
        assert_eq!(status.os, "darwin");
        assert_eq!(status.arch, "arm64");
    }

    #[test]
    fn status_reports_the_empty_install_and_the_channel() {
        let dir = tempfile::tempdir().unwrap();
        let updater = Updater::with_default_probe(
            config(dir.path().join("install")),
            Arc::new(crate::store::MemoryUpdaterStore::new()),
            Arc::new(RefusingFetcher),
        )
        .unwrap();
        let status = updater.status(1_000).unwrap();
        assert_eq!(status.channel, "stable");
        assert!(status.installed.is_none());
        assert!(status.staged.is_none());
        assert!(!status.recovery_required);
    }

    #[test]
    fn apply_without_a_stage_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let updater = Updater::with_default_probe(
            config(dir.path().join("install")),
            Arc::new(crate::store::MemoryUpdaterStore::new()),
            Arc::new(RefusingFetcher),
        )
        .unwrap();
        assert_eq!(updater.apply(1_000).unwrap_err().code(), "nothing_staged");
        assert_eq!(updater.rollback(1_000).unwrap_err().code(), "conflict");
    }

    #[tokio::test]
    async fn an_unresolved_running_operation_blocks_new_work() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::store::MemoryUpdaterStore::new());
        let updater = Updater::with_default_probe(
            config(dir.path().join("install")),
            store.clone(),
            Arc::new(RefusingFetcher),
        )
        .unwrap();
        let running = UpdateOperation::new(UpdateOpKind::Apply, 1, None);
        store.insert(&running).unwrap();
        assert_eq!(updater.apply(2).unwrap_err().code(), "conflict");
        let running_two = updater.running_components(None, None, None, None).unwrap();
        // `stage` fails on the conflict before any manifest parsing.
        let err = updater
            .stage(b"{}", &running_two, None, 2)
            .await
            .unwrap_err();
        assert_eq!(err.code(), "conflict");
        // Recovery resolves it.
        let outcomes = updater.recover(3).unwrap();
        assert_eq!(outcomes.len(), 1);
        assert!(matches!(outcomes[0], RecoveryOutcome::Abandoned { .. }));
    }
}
