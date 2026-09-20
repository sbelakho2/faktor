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
use crate::error::{ManifestRefusal, UpdateError};
use crate::install::{DigestProbe, HealthProbe, InstallLayout, InstallPointer};
use crate::keys::TrustedKeys;
use crate::manifest::{self, Artifact, UpdateManifest};
use crate::release::release_id_for;
use crate::store::{HighWaterMark, UpdateOpKind, UpdateOpStatus, UpdateOperation, UpdaterStore};
use crate::transport::ArtifactFetcher;

/// The native protocol schema version this runtime speaks (the `schema`
/// compatibility entry is checked against it).
pub const NATIVE_SCHEMA_SUPPORTED: u32 = 1;

/// Run one blocking filesystem step on the blocking pool. The updater's
/// install layout operations (digest, publish, staging cleanup) are
/// synchronous `std::fs` work; executing them directly inside an async fn
/// would park a Tokio worker for the duration of the disk I/O.
async fn spawn_blocking_fs<T, F>(what: &str, f: F) -> Result<T, UpdateError>
where
    F: FnOnce() -> Result<T, UpdateError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(join) => Err(UpdateError::Install(format!(
            "{what}: blocking task failed: {join}"
        ))),
    }
}

/// The durable, never-silent note about a post-failure staging cleanup:
/// empty when the directory was removed, otherwise naming the cleanup
/// failure. The original download error stays the RETURNED typed error; the
/// cleanup failure is appended to the operation row's durable detail and
/// logged, so it can never be discarded.
fn staging_cleanup_note(cleanup: &Result<(), UpdateError>) -> String {
    match cleanup {
        Ok(()) => String::new(),
        Err(e) => {
            tracing::warn!("failed update step: the staging directory cleanup failed: {e}");
            format!("; additionally the failed step's staging directory could not be removed: {e}")
        }
    }
}

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
    /// The one-time legacy-manifest allowance (default `false`). When true,
    /// a signed manifest WITHOUT `release_generation` (legacy, treated as
    /// generation 0) is admissible while the durable high-water mark of its
    /// channel is still 0; admitting one records and consumes the allowance
    /// durably, so it can be used exactly once. Everything else about the
    /// manifest (signature, expiry, channel pin, compatibility) is unchanged.
    pub allow_legacy_manifests_once: bool,
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
    /// The signed anti-rollback generation of the manifest (0 = legacy).
    pub release_generation: u64,
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
    /// The signed anti-rollback generation of the staged manifest (0 =
    /// legacy).
    pub release_generation: u64,
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

/// The result of `stage_release`: the ordinary checked stage plus the
/// immutable release materialization the bootstrap launcher authenticates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReleaseStageOutcome {
    #[serde(flatten)]
    pub stage: StageOutcome,
    /// The immutable release id (`<version>-<digest[..12]>`).
    pub release_id: String,
    /// The materialized `versions/<release-id>/faktor` path.
    pub binary: String,
}

/// The outcome of the release-aware activation state machine. `Activated`
/// and `Applied` are deliberately distinct: only `Applied` means a process
/// was OBSERVED running the activated digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum ReleaseOutcome {
    /// The pointer was swapped and the filesystem probe passed, but no
    /// supervised restart has been observed: the running process is still
    /// the PREVIOUS release. Restart the daemon through the bootstrap
    /// launcher and finalize with the digest it reports (or abort to
    /// restore the previous pointer).
    Activated {
        op_id: String,
        release_id: String,
        version: String,
        digest: String,
        artifact: String,
        restart_required: bool,
    },
    /// The restarted process was observed reporting the activated digest and
    /// the health probe passed: final.
    Applied {
        op_id: String,
        release_id: String,
        version: String,
        digest: String,
        artifact: String,
        running_digest: String,
    },
    /// A failure at activation, restart, digest verification, health or
    /// finalize restored the exact previous pointer and (when a restarter
    /// was wired) restarted the previous binary.
    RolledBack {
        op_id: String,
        release_id: String,
        version: String,
        digest: String,
        artifact: String,
        restored_version: Option<String>,
        restored_digest: Option<String>,
        /// The digest the restarted previous process reported, when observed.
        running_digest: Option<String>,
        reason: String,
    },
}

/// The supervised restart seam. A supervisor (the IDE daemon launcher, a
/// service manager, a test harness) implements this: stop the daemon, start
/// it again THROUGH the bootstrap launcher (so the pointer decides the
/// binary), and return the release digest the new process reports (from its
/// build report / health payload). The updater never finalizes an activation
/// whose restart it cannot observe, so "activated" is never reported as
/// "running".
pub trait ReleaseRestarter: Send + Sync {
    /// Restart the supervised process so `active` (the current pointer) is
    /// what runs; return the release digest the restarted process reported.
    fn restart(
        &self,
        layout: &InstallLayout,
        active: &InstallPointer,
    ) -> Result<String, UpdateError>;
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
    /// The caller-supplied wall clock the view was assembled at (the
    /// operations/high-water rows carry their own timestamps; this makes the
    /// observation instant explicit for staleness judgments).
    pub observed_at_ms: i64,
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
    /// The durable anti-rollback high-water mark of the configured channel,
    /// when one has been recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub high_water: Option<HighWaterMark>,
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

/// The admission decision for one authenticated manifest: its signed
/// generation and whether it is a legacy (generation-0) manifest admitted
/// under the one-time allowance.
#[derive(Debug, Clone, Copy)]
struct Admission {
    generation: u64,
    legacy: bool,
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

    /// Attach the immutable release id when `versions/<id>/faktor` is
    /// materialized with EXACTLY this digest: the pointer then names the
    /// bytes the bootstrap launcher authenticates and execs. Legacy layouts
    /// (no materialized version directory) keep the legacy pointer shape
    /// byte-identically.
    fn with_launch_release(&self, pointer: InstallPointer) -> Result<InstallPointer, UpdateError> {
        match self
            .layout
            .release_id_if_materialized(&pointer.version, &pointer.digest)?
        {
            Some(release_id) => pointer.with_release_id(Some(release_id)),
            None => Ok(pointer),
        }
    }

    /// The durable anti-rollback policy for one ALREADY-AUTHENTICATED
    /// manifest (signature, expiry and channel pin verified first):
    ///
    /// - an explicit signed `release_generation` is admissible when it is
    ///   `>=` the durable high-water mark of its channel;
    /// - a legacy manifest without the field is generation 0: admissible
    ///   only while the mark is still 0 AND the operator enabled the
    ///   one-time legacy allowance AND it was not consumed yet; otherwise a
    ///   typed [`UpdateError::LegacyManifestRefused`];
    /// - anything below the mark is a typed
    ///   [`UpdateError::RollbackRefused`] naming both generations.
    ///
    /// This function only DECIDES; [`Updater::record_admission`] persists the
    /// decision (record-first) before any activation step.
    fn admit(&self, manifest: &UpdateManifest) -> Result<Admission, UpdateError> {
        let channel = manifest.channel.to_string();
        let floor = self.store.high_water(&channel)?;
        let (high_water, legacy_consumed) = floor
            .map(|mark| (mark.generation, mark.legacy_consumed))
            .unwrap_or((0, false));
        let generation = manifest.release_generation();
        if manifest.is_legacy_generation() {
            if high_water > 0 {
                return Err(UpdateError::RollbackRefused {
                    channel,
                    high_water,
                    offered: 0,
                });
            }
            if !self.config.allow_legacy_manifests_once {
                return Err(UpdateError::LegacyManifestRefused {
                    channel,
                    detail: "the manifest carries no signed release_generation and the one-time \
                             legacy allowance is disabled (set [updater] \
                             allow_legacy_manifests_once = true to admit it exactly once)"
                        .into(),
                });
            }
            if legacy_consumed {
                return Err(UpdateError::LegacyManifestRefused {
                    channel,
                    detail: "the one-time legacy-manifest allowance was already consumed by an \
                             earlier admission; refusing a second legacy manifest"
                        .into(),
                });
            }
        }
        if generation < high_water {
            return Err(UpdateError::RollbackRefused {
                channel,
                high_water,
                offered: generation,
            });
        }
        Ok(Admission {
            generation,
            legacy: manifest.is_legacy_generation(),
        })
    }

    /// Persist one admission BEFORE the release is activated: the mark
    /// becomes `max(high_water, generation)` (and the legacy allowance is
    /// consumed), so a crash at any later point cannot lower it.
    fn record_admission(
        &self,
        manifest: &UpdateManifest,
        admission: Admission,
        now_ms: i64,
    ) -> Result<(), UpdateError> {
        self.store.raise_high_water(
            &manifest.channel.to_string(),
            admission.generation,
            admission.legacy,
            now_ms,
        )?;
        Ok(())
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
        let high_water = self.store.high_water(self.config.channel.as_str())?;
        Ok(StatusView {
            observed_at_ms: now_ms,
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
            high_water,
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
        op.release_generation = manifest.release_generation;
        op.identity = Some(verified.identity().to_string());
        op.certification_level = manifest.certification.as_ref().map(|c| c.level.clone());

        // Anti-rollback admission: an older signed manifest is refused with
        // a typed `RollbackRefused` naming both generations, and the refusal
        // is durable evidence (a failed check row) never a silent skip.
        let admission = match self.admit(manifest) {
            Ok(admission) => admission,
            Err(e) => {
                return Err(self.record_failed_check_or_refusal(&mut op, e, now_ms));
            }
        };
        op.release_generation = Some(admission.generation);

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
                // The operation row is recorded even when no artifact
                // matches this host: the refusal is durable evidence. A
                // durable-write failure is surfaced (a retryable marker
                // naming both failures) WITHOUT masking the refusal.
                let error =
                    UpdateError::Refused(crate::error::ManifestRefusal::NoArtifactForHost {
                        os: self.config.host_os.clone(),
                        arch: self.config.host_arch.clone(),
                    });
                self.record_failed_check_or_refusal(&mut op, error, now_ms)
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
            return Err(self.record_failed_check_or_refusal(&mut op, e, now_ms));
        }
        self.store.insert(&op)?;

        Ok(CheckOutcome {
            op_id: op.id.to_string(),
            version: manifest.version.clone(),
            channel: manifest.channel.to_string(),
            commit: manifest.commit.clone(),
            identity: verified.identity().to_string(),
            release_generation: admission.generation,
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

    /// Record a refusal durably and return it; when the durable write itself
    /// fails, the refusal is STILL the returned error (its code and HTTP
    /// status preserved) wrapped with the write diagnostic and marked
    /// retryable, so a retry records the audit row. The refusal is never
    /// masked and the write failure is never discarded.
    fn record_failed_check_or_refusal(
        &self,
        op: &mut UpdateOperation,
        refusal: UpdateError,
        now_ms: i64,
    ) -> UpdateError {
        match self.record_failed_check(op, &refusal, now_ms) {
            Ok(()) => refusal,
            Err(write) => UpdateError::RefusalUnrecorded {
                refusal: Box::new(refusal),
                write: write.to_string(),
            },
        }
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

        // Anti-rollback admission, RECORD-FIRST: the durable high-water mark
        // is raised to `max(mark, generation)` (and a legacy allowance, if
        // this is one, is consumed) BEFORE the download starts, so a crash at
        // any later point cannot lower the floor.
        let admission = self.admit(manifest)?;
        self.record_admission(manifest, admission, now_ms)?;

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
        op.release_generation = Some(admission.generation);
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
                    release_generation: admission.generation,
                    idempotent: false,
                })
            }
            Err(e) => {
                let layout = self.layout.clone();
                let op_id = op.id.to_string();
                let cleanup =
                    spawn_blocking_fs("clear failed staging", move || layout.clear_staging(&op_id))
                        .await;
                // The download failure stays the returned typed error; the
                // cleanup failure is durable evidence in the row (never a
                // silently discarded `let _ =`).
                let cleanup_note = staging_cleanup_note(&cleanup);
                op.status = UpdateOpStatus::Failed;
                op.updated_ms = now_ms;
                op.detail = Some(format!("{e}{cleanup_note}"));
                self.store.update(&op)?;
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
            release_generation: op.release_generation.unwrap_or(0),
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
        // Blocking filesystem steps run on the blocking pool: a slow/held
        // install volume must never park a Tokio worker (and every step
        // keeps its typed error).
        let layout = self.layout.clone();
        let op = op_id.to_string();
        spawn_blocking_fs("clear stale staging", move || layout.clear_staging(&op)).await?;
        let dir = self.layout.staging_dir_for(op_id);
        let dir_for_task = dir.clone();
        spawn_blocking_fs("create staging dir", move || {
            std::fs::create_dir_all(&dir_for_task).map_err(|e| {
                UpdateError::Install(format!(
                    "create staging dir {}: {e}",
                    dir_for_task.display()
                ))
            })
        })
        .await?;
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

        let digest_path = staged.clone();
        let actual = spawn_blocking_fs("digest staged artifact", move || {
            crate::install::file_digest(&digest_path)
        })
        .await?;
        if actual != artifact.sha256 {
            return Err(UpdateError::DigestMismatch {
                artifact: artifact.name.clone(),
                expected: artifact.sha256.clone(),
                actual,
            });
        }
        let layout = self.layout.clone();
        let name = artifact.name.clone();
        let digest = artifact.sha256.clone();
        spawn_blocking_fs("publish staged artifact", move || {
            layout.publish_staged(&staged, &name, &digest).map(|_| ())
        })
        .await?;
        let layout = self.layout.clone();
        let op = op_id.to_string();
        spawn_blocking_fs("clear staging", move || layout.clear_staging(&op)).await?;
        Ok(streamed)
    }

    /// Swap the pointer to the staged artifact, run the probe, and roll back
    /// automatically on a probe failure.
    pub fn apply(&self, now_ms: i64) -> Result<ApplyOutcome, UpdateError> {
        self.ensure_no_running()?;
        let staged = self.latest_staged()?;
        // Anti-rollback: the staged release must still be at or above the
        // durable floor. Normally `stage` already raised the mark to exactly
        // this generation; a mark that moved past it (a concurrent admission
        // or a corrupt/rewritten staged row) refuses the swap.
        let channel = staged
            .channel
            .clone()
            .unwrap_or_else(|| self.config.channel.to_string());
        let high_water = self
            .store
            .high_water(&channel)?
            .map(|mark| mark.generation)
            .unwrap_or(0);
        let offered = staged.release_generation.unwrap_or(0);
        if offered < high_water {
            return Err(UpdateError::RollbackRefused {
                channel,
                high_water,
                offered,
            });
        }
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
        // A materialized immutable release is named by the pointer, so the
        // bootstrap launcher (and every supervisor resolving through it)
        // launches exactly these bytes.
        let target = self.with_launch_release(target)?;
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
        op.release_generation = staged.release_generation;
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

    /// The ONLY path below the durable anti-rollback high-water mark: an
    /// explicitly authorized downgrade to an OLDER signed release. The
    /// manifest is authenticated exactly like a normal update (signature,
    /// validity window, channel pin, compatibility) and must carry an
    /// explicit `release_generation` — a legacy manifest can never be a
    /// downgrade target because its generation cannot be authenticated.
    ///
    /// Discipline:
    ///
    /// - the refusal edge exists only through this method (a normal
    ///   `check`/`stage`/`apply` still honors the floor);
    /// - the durable `downgrade` operation row (the audit row of the
    ///   existing path: actor, before/after versions + digests, generation,
    ///   idempotency correlation) is inserted BEFORE the download/swap;
    /// - the high-water mark is reset to the downgraded generation ONLY
    ///   AFTER the swap and its health probe both succeed, so a crash at any
    ///   earlier point leaves the floor high (fail closed);
    /// - a failed probe restores the previous pointer exactly and leaves the
    ///   floor untouched.
    pub async fn downgrade(
        &self,
        manifest_bytes: &[u8],
        running: &RunningComponents,
        idempotency_key: Option<&str>,
        actor: Option<&str>,
        now_ms: i64,
    ) -> Result<ApplyOutcome, UpdateError> {
        self.ensure_no_running()?;
        let verified = manifest::verify_manifest(
            manifest_bytes,
            &self.config.keys,
            self.config.channel,
            now_ms,
            self.config.clock_skew_ms,
        )?;
        let manifest = verified.manifest();
        let channel = manifest.channel.to_string();
        let Some(generation) = manifest.release_generation else {
            return Err(UpdateError::LegacyManifestRefused {
                channel,
                detail: "an explicit downgrade requires a signed release_generation; a legacy \
                         generation-0 manifest cannot be a downgrade target"
                    .into(),
            });
        };
        let actor = actor.map(|actor| {
            actor
                .chars()
                .take(128)
                .filter(|c| !c.is_control())
                .collect::<String>()
        });
        let current = self.layout.read_pointer()?;
        let mut op = UpdateOperation::new(
            UpdateOpKind::Downgrade,
            now_ms,
            Some(format!(
                "explicitly authorized downgrade to {} release generation {generation} signed by {}",
                manifest.version,
                verified.identity()
            )),
        );
        op.channel = Some(channel.clone());
        op.before_version = current.as_ref().map(|p| p.version.clone());
        op.before_digest = current.as_ref().map(|p| p.digest.clone());
        op.before_artifact = current.as_ref().map(|p| p.artifact.clone());
        op.after_version = Some(manifest.version.clone());
        op.identity = Some(verified.identity().to_string());
        op.certification_level = manifest.certification.as_ref().map(|c| c.level.clone());
        op.release_generation = Some(generation);
        op.actor = actor.clone();
        op.idempotency_key = idempotency_key.map(str::to_string);

        let report = compat::check(&manifest.compatibility, running);
        if report.refused {
            let error = UpdateError::Incompatible(report);
            op.status = UpdateOpStatus::Failed;
            op.updated_ms = now_ms;
            op.detail = Some(format!(
                "{}; the authorized downgrade did not proceed",
                error
            ));
            self.store.insert(&op)?;
            return Err(error);
        }
        let artifact =
            match manifest.artifact_for_host(&self.config.host_os, &self.config.host_arch) {
                Some(artifact) => artifact.clone(),
                None => {
                    let error =
                        UpdateError::Refused(crate::error::ManifestRefusal::NoArtifactForHost {
                            os: self.config.host_os.clone(),
                            arch: self.config.host_arch.clone(),
                        });
                    op.status = UpdateOpStatus::Failed;
                    op.updated_ms = now_ms;
                    op.detail = Some(error.to_string());
                    self.store.insert(&op)?;
                    return Err(error);
                }
            };
        op.artifact = Some(artifact.name.clone());
        op.after_digest = Some(artifact.sha256.clone());
        if artifact
            .size
            .is_some_and(|size| size > self.config.max_artifact_bytes)
        {
            let error = UpdateError::ArtifactTooLarge {
                artifact: artifact.name.clone(),
                max_bytes: self.config.max_artifact_bytes,
            };
            op.status = UpdateOpStatus::Failed;
            op.updated_ms = now_ms;
            op.detail = Some(error.to_string());
            self.store.insert(&op)?;
            return Err(error);
        }
        if current
            .as_ref()
            .is_some_and(|pointer| pointer.digest == artifact.sha256)
        {
            return Err(UpdateError::Conflict(format!(
                "the downgrade target {} ({}) is already installed",
                manifest.version, artifact.sha256
            )));
        }

        // Durable BEFORE any effect, carrying the actor + generations.
        self.store.insert(&op)?;

        if let Err(e) = self
            .download_and_publish(&op.id.to_string(), &artifact)
            .await
        {
            let cleanup = self.layout.clear_staging(op.id.as_str());
            let cleanup_note = staging_cleanup_note(&cleanup);
            op.status = UpdateOpStatus::Failed;
            op.updated_ms = now_ms;
            op.detail = Some(format!("{e}{cleanup_note}"));
            self.store.update(&op)?;
            return Err(e);
        }

        let target = InstallPointer::new(
            &artifact.name,
            &artifact.sha256,
            &manifest.version,
            &channel,
            now_ms,
        )?;
        // A materialized immutable release keeps its release identity in the
        // pointer (the bootstrap launcher then runs exactly these bytes).
        let target = self.with_launch_release(target)?;
        self.layout.verify_installed(&target)?;
        if let Err(e) = self.layout.write_pointer(&target) {
            op.status = UpdateOpStatus::Failed;
            op.updated_ms = now_ms;
            op.detail = Some(format!("pointer swap failed: {e}"));
            self.store.update(&op)?;
            return Err(e);
        }

        match self.probe.probe(&self.layout, &target) {
            Ok(()) => {
                // The floor reset happens ONLY after the swap + probe
                // succeeded: the single authorized lowering of the mark.
                if let Err(e) = self.store.set_high_water(&channel, generation, now_ms) {
                    op.status = UpdateOpStatus::Applied;
                    op.updated_ms = now_ms;
                    op.detail = Some(format!(
                        "downgraded to {} ({}) but the high-water reset to generation \
                         {generation} failed: {e}; the floor stays high (fail closed)",
                        target.version, target.digest
                    ));
                    self.store.update(&op)?;
                    return Err(e.into());
                }
                op.status = UpdateOpStatus::Applied;
                op.updated_ms = now_ms;
                op.detail = Some(format!(
                    "downgraded to {} ({}); the high-water mark is reset to generation {generation}",
                    target.version, target.digest
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
                op.detail = Some(format!(
                    "authorized downgrade failed its health probe: {reason}; {restored}; the \
                     high-water mark is unchanged"
                ));
                self.store.update(&op)?;
                self.record_rollback(&current, &target, now_ms, &reason)?;
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
        // The rollback target keeps its immutable release identity when one
        // is materialized: the bootstrap then restarts the EXACT previous
        // release, not just a metadata pointer.
        let target = self.with_launch_release(target)?;
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

    /// Stage one RELEASE-layout update: the ordinary checked stage PLUS the
    /// immutable materialization `versions/<release-id>/{faktor,manifest}`
    /// and the launch trust anchor. The pointer is NOT touched — activation
    /// is a separate, recorded step. The stable bootstrap launcher is
    /// installed from the running executable when absent (never replaced).
    pub async fn stage_release(
        &self,
        manifest_bytes: &[u8],
        running: &RunningComponents,
        idempotency_key: Option<&str>,
        now_ms: i64,
    ) -> Result<ReleaseStageOutcome, UpdateError> {
        let stage = self
            .stage(manifest_bytes, running, idempotency_key, now_ms)
            .await?;
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
                UpdateError::Refused(ManifestRefusal::NoArtifactForHost {
                    os: self.config.host_os.clone(),
                    arch: self.config.host_arch.clone(),
                })
            })?
            .clone();
        if artifact.sha256 != stage.digest {
            return Err(UpdateError::Conflict(format!(
                "the staged digest {} does not match the signed manifest digest {}",
                stage.digest, artifact.sha256
            )));
        }
        let release_id = release_id_for(&stage.version, &stage.digest);
        crate::release::validate_release_id(&release_id)?;
        // The trust anchor and the stable bootstrap exist BEFORE any pointer
        // can name this release.
        self.layout.write_trust(&self.config.keys)?;
        if !self.layout.launcher_path().is_file() {
            let current = std::env::current_exe().map_err(|e| {
                UpdateError::Install(format!("bootstrap launcher source (current exe): {e}"))
            })?;
            self.layout.install_launcher(&current)?;
        }
        self.layout.materialize_release(
            &release_id,
            &artifact.name,
            &stage.digest,
            manifest_bytes,
        )?;
        let binary = self.layout.release_binary(&release_id);
        Ok(ReleaseStageOutcome {
            stage,
            release_id,
            binary: binary.display().to_string(),
        })
    }

    /// Activate the staged release: record-first, atomic pointer swap, then
    /// the release-aware verification branch:
    ///
    /// - WITH a [`ReleaseRestarter`]: restart through the pointer, require
    ///   the NEW process to report the activated digest, run the health
    ///   probe, finalize (`Applied`). A failure at ANY step restores the
    ///   previous pointer, restarts the previous binary and records the
    ///   rollback;
    /// - WITHOUT one (explicit restart): the pointer swap plus a filesystem
    ///   probe returns `Activated { restart_required: true }` and the apply
    ///   row stays `Running` until [`Updater::finalize_release`] observes the
    ///   restarted process's digest (or [`Updater::abort_release`] restores
    ///   the previous release).
    pub fn activate_release(
        &self,
        now_ms: i64,
        restarter: Option<&dyn ReleaseRestarter>,
    ) -> Result<ReleaseOutcome, UpdateError> {
        self.ensure_no_running()?;
        let staged = self.latest_staged()?;
        // Anti-rollback: the staged release must still be at or above the
        // durable floor (same rule as `apply`).
        let channel = staged
            .channel
            .clone()
            .unwrap_or_else(|| self.config.channel.to_string());
        let high_water = self
            .store
            .high_water(&channel)?
            .map(|mark| mark.generation)
            .unwrap_or(0);
        let offered = staged.release_generation.unwrap_or(0);
        if offered < high_water {
            return Err(UpdateError::RollbackRefused {
                channel,
                high_water,
                offered,
            });
        }
        if let Some(last) = self
            .store
            .latest(UpdateOpKind::Apply, UpdateOpStatus::Applied)?
        {
            if last.after_digest.is_some() && last.after_digest == staged.after_digest {
                return Err(UpdateError::Conflict(format!(
                    "the staged release {} ({}) is already applied",
                    staged.after_version.as_deref().unwrap_or_default(),
                    staged.after_digest.as_deref().unwrap_or_default()
                )));
            }
        }
        let version = staged.after_version.clone().unwrap_or_default();
        let digest = staged
            .after_digest
            .clone()
            .ok_or_else(|| UpdateError::Conflict("staged operation carries no digest".into()))?;
        let artifact = staged
            .artifact
            .clone()
            .ok_or_else(|| UpdateError::Conflict("staged operation carries no artifact".into()))?;
        let release_id = release_id_for(&version, &digest);
        crate::release::validate_release_id(&release_id)?;
        let binary = self.layout.release_binary(&release_id);
        if !binary.is_file() {
            return Err(UpdateError::Conflict(format!(
                "release {release_id} is not materialized under versions/; run stage_release first"
            )));
        }
        // Re-authenticate the stored signed manifest and bind it to the
        // staged operation: the pointer may only name what the operator key
        // signed.
        let manifest_path = self.layout.release_manifest(&release_id);
        let manifest_bytes = std::fs::read(&manifest_path)
            .map_err(|e| UpdateError::Install(format!("read {}: {e}", manifest_path.display())))?;
        let signed = manifest::verify_manifest_at_launch(&manifest_bytes, &self.config.keys)?;
        if signed.version != version {
            return Err(UpdateError::Conflict(format!(
                "the release manifest names version {} but the staged operation names {version}",
                signed.version
            )));
        }
        if !signed
            .artifacts
            .iter()
            .any(|artifact| artifact.sha256 == digest)
        {
            return Err(UpdateError::Conflict(format!(
                "the staged digest {digest} does not appear in the signed release manifest"
            )));
        }
        let target = InstallPointer::new(
            &artifact,
            &digest,
            &version,
            staged.channel.as_deref().unwrap_or("stable"),
            now_ms,
        )?
        .with_release_id(Some(release_id.clone()))?;
        // Deterministic FS op: the exact bytes the launcher will authenticate
        // are re-hashed before the pointer moves.
        self.layout.verify_installed(&target)?;

        let current = self.layout.read_pointer()?;
        let mut op = UpdateOperation::new(
            UpdateOpKind::Apply,
            now_ms,
            Some(format!("release activation {release_id} ({version})")),
        );
        op.channel = staged.channel.clone();
        op.before_version = current.as_ref().map(|p| p.version.clone());
        op.before_digest = current.as_ref().map(|p| p.digest.clone());
        op.before_artifact = current.as_ref().map(|p| p.artifact.clone());
        op.after_version = Some(target.version.clone());
        op.after_digest = Some(target.digest.clone());
        op.artifact = Some(target.artifact.clone());
        op.release_generation = staged.release_generation;
        op.identity = staged.identity.clone();
        op.certification_level = staged.certification_level.clone();
        // Durable BEFORE the swap: a crash after this row exists is
        // recoverable from the pointer alone.
        self.store.insert(&op)?;

        if let Err(e) = self.layout.write_pointer(&target) {
            op.status = UpdateOpStatus::Failed;
            op.updated_ms = now_ms;
            op.detail = Some(format!("release pointer swap failed: {e}"));
            self.store.update(&op)?;
            return Err(e);
        }

        match restarter {
            Some(restarter) => match restarter.restart(&self.layout, &target) {
                Ok(reported) if reported == target.digest => {
                    match self.probe.probe(&self.layout, &target) {
                        Ok(()) => {
                            op.status = UpdateOpStatus::Applied;
                            op.updated_ms = now_ms;
                            op.detail = Some(format!(
                                "applied release {release_id} ({version}); the restarted process reports {}",
                                target.digest
                            ));
                            self.store.update(&op)?;
                            Ok(ReleaseOutcome::Applied {
                                op_id: op.id.to_string(),
                                release_id,
                                version: target.version,
                                digest: target.digest,
                                artifact: target.artifact,
                                running_digest: reported,
                            })
                        }
                        Err(e) => self.fail_release(
                            op,
                            &current,
                            &target,
                            now_ms,
                            Some(restarter),
                            format!("health probe failed after the restart: {e}"),
                        ),
                    }
                }
                Ok(reported) => self.fail_release(
                    op,
                    &current,
                    &target,
                    now_ms,
                    Some(restarter),
                    format!(
                        "the restarted process reports release digest {reported:?}, expected {}",
                        target.digest
                    ),
                ),
                Err(e) => self.fail_release(
                    op,
                    &current,
                    &target,
                    now_ms,
                    Some(restarter),
                    format!("restart failed: {e}"),
                ),
            },
            None => match self.probe.probe(&self.layout, &target) {
                Ok(()) => {
                    op.detail = Some(format!(
                        "activated release {release_id} ({version}); restart the daemon through the \
                         bootstrap launcher and finalize with the digest it reports"
                    ));
                    op.updated_ms = now_ms;
                    self.store.update(&op)?;
                    Ok(ReleaseOutcome::Activated {
                        op_id: op.id.to_string(),
                        release_id,
                        version: target.version,
                        digest: target.digest,
                        artifact: target.artifact,
                        restart_required: true,
                    })
                }
                Err(e) => self.fail_release(
                    op,
                    &current,
                    &target,
                    now_ms,
                    None,
                    format!("health probe failed after activation: {e}"),
                ),
            },
        }
    }

    /// The full supervised release update: activate, restart, observe the
    /// NEW process's reported digest, health-probe and finalize — with every
    /// failure restoring the previous pointer and restarting the previous
    /// binary.
    pub fn apply_release(
        &self,
        now_ms: i64,
        restarter: &dyn ReleaseRestarter,
    ) -> Result<ReleaseOutcome, UpdateError> {
        self.activate_release(now_ms, Some(restarter))
    }

    /// Finalize an explicit (`Activated`) release activation: the caller
    /// observed the RESTARTED process reporting `reported_digest`. It must
    /// equal the activated digest, then the health probe must pass; only
    /// then is the apply row `Applied`. A probe failure restores the
    /// previous pointer and records the rollback (the caller then restarts
    /// the previous binary through the bootstrap).
    pub fn finalize_release(
        &self,
        reported_digest: &str,
        now_ms: i64,
    ) -> Result<ReleaseOutcome, UpdateError> {
        let current = self.layout.read_pointer()?.ok_or_else(|| {
            UpdateError::Conflict("the install has no pointer to finalize".into())
        })?;
        let release_id = current.release_id.clone().ok_or_else(|| {
            UpdateError::Conflict("the installed pointer is not a release pointer".into())
        })?;
        if reported_digest != current.digest {
            return Err(UpdateError::Conflict(format!(
                "the running process reports {reported_digest:?} but the activated release is {} ({release_id})",
                current.digest
            )));
        }
        let mut op = self
            .running_release_apply(&current.digest)?
            .ok_or_else(|| {
                UpdateError::Conflict(
                    "no in-flight release activation matches the installed pointer; nothing to finalize"
                        .into(),
                )
            })?;
        match self.probe.probe(&self.layout, &current) {
            Ok(()) => {
                op.status = UpdateOpStatus::Applied;
                op.updated_ms = now_ms;
                op.detail = Some(format!(
                    "release {release_id} finalized: the running process reports {}",
                    current.digest
                ));
                self.store.update(&op)?;
                Ok(ReleaseOutcome::Applied {
                    op_id: op.id.to_string(),
                    release_id,
                    version: current.version,
                    digest: current.digest,
                    artifact: current.artifact,
                    running_digest: reported_digest.to_string(),
                })
            }
            Err(e) => {
                let previous = self.previous_pointer(&op)?;
                self.fail_release(
                    op,
                    &previous,
                    &current,
                    now_ms,
                    None,
                    format!("health probe failed during finalize: {e}"),
                )
            }
        }
    }

    /// Explicitly abort a pending (`Activated`) release activation: restore
    /// the exact previous pointer and record the rolled-back rows. The
    /// caller then restarts the daemon through the bootstrap launcher, which
    /// now names the previous release.
    pub fn abort_release(&self, now_ms: i64) -> Result<ReleaseOutcome, UpdateError> {
        let current = self
            .layout
            .read_pointer()?
            .ok_or_else(|| UpdateError::Conflict("the install has no pointer to abort".into()))?;
        let release_id = current.release_id.clone().ok_or_else(|| {
            UpdateError::Conflict("the installed pointer is not a release pointer".into())
        })?;
        let op = self
            .running_release_apply(&current.digest)?
            .ok_or_else(|| {
                UpdateError::Conflict(
                    "no in-flight release activation matches the installed pointer; nothing to abort"
                        .into(),
                )
            })?;
        let previous = self.previous_pointer(&op)?;
        let outcome = self.fail_release(
            op,
            &previous,
            &current,
            now_ms,
            None,
            "explicit abort of the pending release activation".into(),
        )?;
        match outcome {
            ReleaseOutcome::RolledBack {
                op_id,
                version,
                digest,
                artifact,
                restored_version,
                restored_digest,
                running_digest,
                reason,
                ..
            } => Ok(ReleaseOutcome::RolledBack {
                op_id,
                release_id,
                version,
                digest,
                artifact,
                restored_version,
                restored_digest,
                running_digest,
                reason,
            }),
            other => Ok(other),
        }
    }

    /// The in-flight release activation whose target digest is `digest`.
    fn running_release_apply(&self, digest: &str) -> Result<Option<UpdateOperation>, UpdateError> {
        Ok(self.store.running()?.into_iter().find(|op| {
            op.kind == UpdateOpKind::Apply && op.after_digest.as_deref() == Some(digest)
        }))
    }

    /// Failure path of the release state machine: restore the exact previous
    /// pointer, restart the previous binary through the restarter when one
    /// is wired (and require it to report the previous digest), record the
    /// rolled-back apply + rollback rows, and return the outcome.
    fn fail_release(
        &self,
        mut op: UpdateOperation,
        previous: &Option<InstallPointer>,
        target: &InstallPointer,
        now_ms: i64,
        restarter: Option<&dyn ReleaseRestarter>,
        reason: String,
    ) -> Result<ReleaseOutcome, UpdateError> {
        tracing::warn!("release activation rollback: {reason}");
        let restored = match previous {
            Some(pointer) => match self.layout.write_pointer(pointer) {
                Ok(()) => format!("restored {} ({})", pointer.version, pointer.digest),
                Err(e) => format!("FAILED to restore the previous pointer: {e}"),
            },
            None => match self.layout.remove_pointer() {
                Ok(()) => "removed the pointer (no previous install)".to_string(),
                Err(e) => format!("FAILED to remove the pointer: {e}"),
            },
        };
        let mut restart_note = String::new();
        let mut running_digest = None;
        if let Some(restarter) = restarter {
            match previous {
                Some(pointer) => match restarter.restart(&self.layout, pointer) {
                    Ok(reported) if reported == pointer.digest => {
                        restart_note = format!(
                            "; the previous process was restarted and reports {}",
                            pointer.digest
                        );
                        running_digest = Some(reported);
                    }
                    Ok(reported) => {
                        restart_note = format!(
                            "; WARNING: the restarted process reports {reported} (expected {})",
                            pointer.digest
                        );
                        running_digest = Some(reported);
                    }
                    Err(e) => {
                        restart_note = format!("; FAILED to restart the previous binary: {e}");
                    }
                },
                None => restart_note = "; no previous release to restart".to_string(),
            }
        }
        op.status = UpdateOpStatus::RolledBack;
        op.updated_ms = now_ms;
        op.detail = Some(format!(
            "release activation failed: {reason}; {restored}{restart_note}"
        ));
        self.store.update(&op)?;
        self.record_rollback(previous, target, now_ms, &reason)?;
        Ok(ReleaseOutcome::RolledBack {
            op_id: op.id.to_string(),
            release_id: target.release_id.clone().unwrap_or_default(),
            version: target.version.clone(),
            digest: target.digest.clone(),
            artifact: target.artifact.clone(),
            restored_version: previous.as_ref().map(|p| p.version.clone()),
            restored_digest: previous.as_ref().map(|p| p.digest.clone()),
            running_digest,
            reason,
        })
    }

    /// Resolve every crash residue. Never re-runs a download; an
    /// interrupted apply is resumed (probe) or rolled back. This entry point
    /// cannot attest which release the RUNNING process is, so release
    /// activations are never resumed as applied from here (see
    /// [`Updater::recover_with_running_digest`]).
    pub fn recover(&self, now_ms: i64) -> Result<Vec<RecoveryOutcome>, UpdateError> {
        self.recover_with_running_digest(now_ms, None)
    }

    /// Like [`Updater::recover`], but the caller attests which release digest
    /// the RUNNING process reports (the daemon passes the
    /// `FAKTOR_RELEASE_DIGEST` its bootstrap launcher verified). A release
    /// activation whose pointer is already in place is only resumed as
    /// `Applied` when the running process is observed as the activated
    /// release; otherwise verification is forced — a metadata-only pointer
    /// swap is never reported as a running update.
    pub fn recover_with_running_digest(
        &self,
        now_ms: i64,
        running_digest: Option<&str>,
    ) -> Result<Vec<RecoveryOutcome>, UpdateError> {
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
                    outcomes.push(self.recover_apply(&mut op, now_ms, running_digest)?);
                }
                UpdateOpKind::Rollback => {
                    outcomes.push(self.recover_rollback(&mut op, now_ms)?);
                }
                // An interrupted authorized downgrade has exactly the swap
                // semantics of an apply; recovery additionally re-applies the
                // floor reset once the resumed probe proves it healthy.
                UpdateOpKind::Downgrade => {
                    outcomes.push(self.recover_apply(&mut op, now_ms, running_digest)?);
                }
            }
        }
        Ok(outcomes)
    }

    fn recover_apply(
        &self,
        op: &mut UpdateOperation,
        now_ms: i64,
        running_digest: Option<&str>,
    ) -> Result<RecoveryOutcome, UpdateError> {
        let current = self.layout.read_pointer()?;
        let target_digest = op.after_digest.clone().unwrap_or_default();
        let before_digest = op.before_digest.clone();
        let pointer_digest = current.as_ref().map(|p| p.digest.clone());
        if pointer_digest.as_deref() == Some(target_digest.as_str()) {
            // The swap happened; the restart/finalize never ran. Resume by
            // probing — but a RELEASE pointer additionally requires the
            // running process to be the activated release: recovery refuses
            // to call a metadata-only swap "applied".
            let target = current.clone().expect("pointer digest implies a pointer");
            if let Some(release_id) = target.release_id.clone() {
                if running_digest != Some(target_digest.as_str()) {
                    op.status = UpdateOpStatus::Unverified;
                    op.updated_ms = now_ms;
                    op.detail = Some(format!(
                        "release {release_id} is activated in the pointer but the running process \
                         reports {} (expected {}); restart it through the bootstrap launcher and \
                         finalize with the digest it reports",
                        running_digest.unwrap_or("no release (started directly)"),
                        target_digest
                    ));
                    self.store.update(op)?;
                    return Ok(RecoveryOutcome::NeedsVerification {
                        op_id: op.id.to_string(),
                        detail: format!(
                            "release {release_id} was never observed running; restart + finalize required"
                        ),
                    });
                }
            }
            // Re-authenticate + re-check the anti-rollback floor on recovery:
            // the signed manifest must still verify and bind this pointer, and
            // a durable high-water mark that moved past the interrupted
            // operation's generation (a concurrent admission between crash and
            // recovery) makes resuming it a rollback. Either failure forces
            // verification — recovery never claims Applied on stale authority.
            if let Err(reason) = self.reauthenticate_recovered_release(op, &target) {
                op.status = UpdateOpStatus::Unverified;
                op.updated_ms = now_ms;
                op.detail = Some(format!(
                    "recovery refused to resume the interrupted activation: {reason}; \
                     verification forced"
                ));
                self.store.update(op)?;
                return Ok(RecoveryOutcome::NeedsVerification {
                    op_id: op.id.to_string(),
                    detail: reason,
                });
            }
            match self.probe.probe(&self.layout, &target) {
                Ok(()) => {
                    // A resumed authorized downgrade must also complete the
                    // deferred floor reset; if that reset cannot be recorded,
                    // the operation is left for verification (fail closed)
                    // instead of claiming a completed downgrade.
                    if op.kind == UpdateOpKind::Downgrade {
                        let channel = op
                            .channel
                            .clone()
                            .unwrap_or_else(|| self.config.channel.to_string());
                        let generation = op.release_generation.unwrap_or(0);
                        if let Err(e) = self.store.set_high_water(&channel, generation, now_ms) {
                            op.status = UpdateOpStatus::Unverified;
                            op.updated_ms = now_ms;
                            op.detail = Some(format!(
                                "recovered the downgraded swap but the high-water reset failed: \
                                 {e}; verification forced"
                            ));
                            self.store.update(op)?;
                            return Ok(RecoveryOutcome::NeedsVerification {
                                op_id: op.id.to_string(),
                                detail: "the authorized downgrade's high-water reset failed".into(),
                            });
                        }
                    }
                    op.status = UpdateOpStatus::Applied;
                    op.updated_ms = now_ms;
                    op.detail = Some("recovered: the interrupted swap is healthy".into());
                    self.store.update(op)?;
                    Ok(RecoveryOutcome::Resumed {
                        op_id: op.id.to_string(),
                    })
                }
                Err(e) => {
                    let previous = match self.previous_pointer(op) {
                        Ok(previous) => previous,
                        Err(previous_error) => {
                            // The previous release cannot be verified for a
                            // restore: never fabricate a "restored" success —
                            // force verification (fail closed).
                            op.status = UpdateOpStatus::Unverified;
                            op.updated_ms = now_ms;
                            op.detail = Some(format!(
                                "recovered: the interrupted swap failed its probe ({e}) and the \
                                 previous release could not be verified for restore \
                                 ({previous_error}); verification forced"
                            ));
                            self.store.update(op)?;
                            return Ok(RecoveryOutcome::NeedsVerification {
                                op_id: op.id.to_string(),
                                detail: previous_error.to_string(),
                            });
                        }
                    };
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
            // The pointer is back at the previous side. If the caller
            // attests that the RUNNING process is something else (a release
            // rollback that crashed after the new binary was restarted), the
            // install is not proven: force verification instead of claiming
            // the previous install is live.
            if let (Some(running), Some(before)) = (running_digest, before_digest.as_deref()) {
                if running != before {
                    op.status = UpdateOpStatus::Unverified;
                    op.updated_ms = now_ms;
                    op.detail = Some(format!(
                        "the pointer was restored to {before} but the running process reports \
                         {running}; restart it through the bootstrap launcher; verification forced"
                    ));
                    self.store.update(op)?;
                    return Ok(RecoveryOutcome::NeedsVerification {
                        op_id: op.id.to_string(),
                        detail: "the restored pointer and the running process disagree".into(),
                    });
                }
            }
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
            // The restored pointer must still be an AUTHENTICATED release:
            // resuming the rollback is never honest when the target's signed
            // manifest no longer verifies or binds the pointer.
            if let Some(target) = current.as_ref() {
                if let Err(reason) = self.reauthenticate_recovered_release(op, target) {
                    op.status = UpdateOpStatus::Unverified;
                    op.updated_ms = now_ms;
                    op.detail = Some(format!(
                        "the rollback target is in place but it no longer re-authenticates: \
                         {reason}; verification forced"
                    ));
                    self.store.update(op)?;
                    return Ok(RecoveryOutcome::NeedsVerification {
                        op_id: op.id.to_string(),
                        detail: reason,
                    });
                }
            }
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

    /// Re-authenticate one release pointer left by a crash before recovery may
    /// resume it as Applied/RolledBack: the signed manifest stored next to the
    /// binary must still verify against the operator allowlist, must still
    /// name the pointer's version/channel/digest, and (outside an explicitly
    /// authorized downgrade) the durable anti-rollback high-water mark must
    /// not have moved past the interrupted operation's signed generation. Any
    /// failure is a typed reason string; recovery then forces verification
    /// instead of claiming the activation succeeded on stale authority.
    fn reauthenticate_recovered_release(
        &self,
        op: &UpdateOperation,
        target: &InstallPointer,
    ) -> Result<(), String> {
        let Some(release_id) = target.release_id.as_deref() else {
            return Ok(());
        };
        let manifest_path = self.layout.release_manifest(release_id);
        let bytes = std::fs::read(&manifest_path).map_err(|e| {
            format!(
                "the signed release manifest {} is unreadable: {e}",
                manifest_path.display()
            )
        })?;
        let signed =
            manifest::verify_manifest_at_launch(&bytes, &self.config.keys).map_err(|e| {
                format!("the signed release manifest for {release_id} no longer verifies: {e}")
            })?;
        if signed.version != target.version {
            return Err(format!(
                "the signed manifest names version {} but the pointer names {}",
                signed.version, target.version
            ));
        }
        let manifest_channel = signed.channel.to_string();
        if manifest_channel != target.channel {
            return Err(format!(
                "the signed manifest names channel {manifest_channel} but the pointer names {}",
                target.channel
            ));
        }
        if !signed
            .artifacts
            .iter()
            .any(|artifact| artifact.sha256 == target.digest)
        {
            return Err(format!(
                "the pointer digest {} does not appear in the signed manifest for {release_id}",
                target.digest
            ));
        }
        if let Some(recorded) = op.release_generation {
            if signed.release_generation() != recorded {
                return Err(format!(
                    "the signed manifest carries generation {} but the operation records {recorded}",
                    signed.release_generation()
                ));
            }
        }
        // An authorized downgrade is the ONE path admitted below the floor; it
        // completes the deferred floor reset after its probe (see the caller),
        // so it is exempt from the floor check here.
        if op.kind != UpdateOpKind::Downgrade {
            let channel = op
                .channel
                .clone()
                .unwrap_or_else(|| self.config.channel.to_string());
            let high_water = self
                .store
                .high_water(&channel)
                .map_err(|e| format!("the high-water read for channel {channel:?} failed: {e}"))?
                .map(|mark| mark.generation)
                .unwrap_or(0);
            let offered = op.release_generation.unwrap_or(0);
            if offered < high_water {
                return Err(format!(
                    "the durable high-water mark of channel {channel:?} moved to {high_water}, \
                     above the interrupted operation's generation {offered}; resuming it would be \
                     a rollback"
                ));
            }
        }
        Ok(())
    }

    fn previous_pointer(
        &self,
        op: &UpdateOperation,
    ) -> Result<Option<InstallPointer>, UpdateError> {
        match (&op.before_artifact, &op.before_digest, &op.before_version) {
            (Some(artifact), Some(digest), version) => {
                Ok(Some(self.with_launch_release(InstallPointer::new(
                    artifact,
                    digest,
                    version.as_deref().unwrap_or_default(),
                    op.channel.as_deref().unwrap_or("stable"),
                    op.created_ms,
                )?)?))
            }
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
            allow_legacy_manifests_once: false,
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
        assert_eq!(status.observed_at_ms, 1_000);
        assert!(status.installed.is_none());
        assert!(status.staged.is_none());
        assert!(!status.recovery_required);
    }

    const CHECK_NOW: i64 = 1_700_000_000_000;

    fn check_key() -> ed25519_dalek::SigningKey {
        ed25519_dalek::SigningKey::from_bytes(&[9u8; 32])
    }

    fn check_trusted_keys(key: &ed25519_dalek::SigningKey) -> TrustedKeys {
        use base64::Engine as _;
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(key.verifying_key().to_bytes());
        TrustedKeys::new(vec![crate::keys::TrustedKey::from_base64(
            "operator", &encoded,
        )
        .unwrap()])
        .unwrap()
    }

    /// A correctly signed stable manifest whose only artifact targets
    /// LINUX, so this darwin host has no matching artifact (the refusal path
    /// under test).
    fn signed_manifest_without_a_host_artifact(key: &ed25519_dalek::SigningKey) -> Vec<u8> {
        use base64::Engine as _;
        use ed25519_dalek::Signer as _;
        let mut doc = manifest::UpdateManifest {
            schema: manifest::UPDATE_MANIFEST_SCHEMA.to_string(),
            channel: Channel::Stable,
            version: "0.2.0".into(),
            commit: "a".repeat(40),
            release_generation: Some(1),
            artifacts: vec![Artifact {
                name: "faktor-cli-0.2.0-linux-x86_64.tar.gz".into(),
                os: "linux".into(),
                arch: "x86_64".into(),
                sha256: "b".repeat(64),
                url: "https://mirror.test/linux-x86_64".into(),
                size: Some(4),
            }],
            compatibility: manifest::Compatibility {
                cli: crate::version::VersionRange::from_parts("0.1.0", "9.9.9").unwrap(),
                daemon: crate::version::VersionRange::from_parts("0.1.0", "9.9.9").unwrap(),
                vscode: crate::version::VersionRange::from_parts("*", "*").unwrap(),
                jetbrains: crate::version::VersionRange::from_parts("*", "*").unwrap(),
                schema: crate::SchemaRange { min: 1, max: 1 },
            },
            issued_at: CHECK_NOW - 1_000,
            expires_at: CHECK_NOW + 600_000,
            certification: None,
            signature: None,
        };
        let payload = doc.signing_payload().unwrap();
        let signature = key.sign(&payload);
        doc.signature = Some(manifest::ManifestSignature {
            algorithm: "ed25519".into(),
            identity: "operator".into(),
            public_key: base64::engine::general_purpose::STANDARD
                .encode(key.verifying_key().to_bytes()),
            value: base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()),
        });
        serde_json::to_vec(&doc).unwrap()
    }

    /// A store that delegates every read but refuses every `insert`: the
    /// deterministic durable-write failure of the refusal-recording path.
    struct FailingInsertStore {
        inner: crate::store::MemoryUpdaterStore,
    }

    impl UpdaterStore for FailingInsertStore {
        fn insert(
            &self,
            _operation: &UpdateOperation,
        ) -> Result<(), crate::store::UpdateStoreError> {
            Err(crate::store::UpdateStoreError::Backend(
                "injected insert failure".into(),
            ))
        }

        fn update(
            &self,
            operation: &UpdateOperation,
        ) -> Result<(), crate::store::UpdateStoreError> {
            self.inner.update(operation)
        }

        fn get(&self, id: &str) -> Result<Option<UpdateOperation>, crate::store::UpdateStoreError> {
            self.inner.get(id)
        }

        fn list(
            &self,
            limit: usize,
        ) -> Result<Vec<UpdateOperation>, crate::store::UpdateStoreError> {
            self.inner.list(limit)
        }

        fn running(&self) -> Result<Vec<UpdateOperation>, crate::store::UpdateStoreError> {
            self.inner.running()
        }

        fn latest(
            &self,
            kind: UpdateOpKind,
            status: UpdateOpStatus,
        ) -> Result<Option<UpdateOperation>, crate::store::UpdateStoreError> {
            self.inner.latest(kind, status)
        }

        fn by_key(
            &self,
            key: &str,
        ) -> Result<Option<UpdateOperation>, crate::store::UpdateStoreError> {
            self.inner.by_key(key)
        }

        fn high_water(
            &self,
            channel: &str,
        ) -> Result<Option<HighWaterMark>, crate::store::UpdateStoreError> {
            self.inner.high_water(channel)
        }

        fn raise_high_water(
            &self,
            channel: &str,
            generation: u64,
            legacy_consumed: bool,
            now_ms: i64,
        ) -> Result<HighWaterMark, crate::store::UpdateStoreError> {
            self.inner
                .raise_high_water(channel, generation, legacy_consumed, now_ms)
        }

        fn set_high_water(
            &self,
            channel: &str,
            generation: u64,
            now_ms: i64,
        ) -> Result<HighWaterMark, crate::store::UpdateStoreError> {
            self.inner.set_high_water(channel, generation, now_ms)
        }
    }

    #[test]
    fn check_without_a_host_artifact_records_the_refusal_durably() {
        let dir = tempfile::tempdir().unwrap();
        let key = check_key();
        let store = Arc::new(crate::store::MemoryUpdaterStore::new());
        let mut cfg = config(dir.path().join("install"));
        cfg.keys = check_trusted_keys(&key);
        let updater =
            Updater::with_default_probe(cfg, store.clone(), Arc::new(RefusingFetcher)).unwrap();
        let running = updater.running_components(None, None, None, None).unwrap();
        let err = updater
            .check(
                &signed_manifest_without_a_host_artifact(&key),
                &running,
                CHECK_NOW,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            UpdateError::Refused(ManifestRefusal::NoArtifactForHost { .. })
        ));
        // The refusal is durable evidence: one failed check row naming it.
        let rows = store.list(10).unwrap();
        assert_eq!(rows.len(), 1, "exactly the failed check row");
        assert_eq!(rows[0].status, UpdateOpStatus::Failed);
        assert!(
            rows[0].detail.as_deref().unwrap().contains("no artifact"),
            "{:?}",
            rows[0].detail
        );
    }

    #[test]
    fn failed_refusal_record_is_surfaced_without_masking_the_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let key = check_key();
        let store = Arc::new(FailingInsertStore {
            inner: crate::store::MemoryUpdaterStore::new(),
        });
        let mut cfg = config(dir.path().join("install"));
        cfg.keys = check_trusted_keys(&key);
        let updater =
            Updater::with_default_probe(cfg, store.clone(), Arc::new(RefusingFetcher)).unwrap();
        let running = updater.running_components(None, None, None, None).unwrap();
        let err = updater
            .check(
                &signed_manifest_without_a_host_artifact(&key),
                &running,
                CHECK_NOW,
            )
            .unwrap_err();
        // The refusal is still the surfaced error (code + HTTP status), and
        // the durable-write failure is explicit, not discarded.
        assert_eq!(err.code(), "manifest_no_artifact_for_host");
        assert_eq!(err.http_status(), 409);
        assert!(err.retryable(), "the missing audit row is a retry marker");
        assert!(err.to_string().contains("injected insert failure"), "{err}");
        match err {
            UpdateError::RefusalUnrecorded { refusal, write } => {
                assert!(matches!(
                    *refusal,
                    UpdateError::Refused(ManifestRefusal::NoArtifactForHost { .. })
                ));
                assert!(write.contains("injected insert failure"), "{write}");
            }
            other => panic!("expected RefusalUnrecorded, got {other:?}"),
        }
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
