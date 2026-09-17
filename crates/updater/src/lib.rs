//! faktor-updater — the signed updater/distribution lifecycle of the
//! Faktor runtime.
//!
//! Layout:
//!
//! - [`manifest`]: the `faktor-update/v1` schema (channel, version, commit,
//!   artifacts with sha256 + url, per-component compatibility ranges,
//!   validity window, optional certification provenance) and its canonical
//!   signing payload;
//! - [`keys`]: the operator ed25519 key allowlist (base64 raw keys, the
//!   same signature shape the certification evidence uses);
//! - [`channel`]: stable/beta/dev trust order and deterministic selection;
//! - [`compat`]: the running-component check with a per-component verdict
//!   report (no silent skips);
//! - [`install`]: the install layout (`current` pointer, content-addressed
//!   artifacts, same-filesystem staging) and the atomic swap discipline;
//! - [`release`]: the ADDITIVE immutable-version layout
//!   (`versions/<release-id>/{faktor,manifest}` + the stable bootstrap
//!   `launcher` + the `trusted-keys.json` anchor), the launch-time
//!   authentication of the activated release (signature + digest binding,
//!   unsigned/mismatch refused), and the exec path that replaces the
//!   bootstrap process with the exact verified bytes;
//! - [`store`]: durable `UpdateOperation` rows (check/stage/apply/rollback/
//!   downgrade with before/after versions and digests; memory + SQLite
//!   stores) plus the durable per-channel anti-rollback high-water mark;
//! - [`transport`]: the download seam plus the checked-HTTP implementation;
//! - [`service`]: the `check → stage → apply → rollback` state machine with
//!   automatic rollback on a failed health probe, crash recovery and the
//!   signed-generation anti-rollback floor (the only way below it is an
//!   explicitly authorized, audited downgrade).
//!
//! Trust model: a manifest is never acted on unless it carries an ed25519
//! signature from an allowlisted operator identity, verifies over the
//! canonical payload, is inside its validity window and matches the
//! configured channel. Unsigned, unknown-key, key-mismatched, tampered,
//! malformed and expired manifests are distinct typed refusals — never a
//! silent skip. A signed `release_generation` is folded into that payload
//! and checked against a durable per-channel high-water mark, so replaying
//! an older validly signed manifest is a typed `RollbackRefused`. Artifacts
//! are downloaded through the daemon's CHECKED transport, bounded,
//! digest-verified, and published atomically; the install is switched only
//! by an atomic pointer replacement, and a failed post-swap probe restores
//! the exact previous artifact.

pub mod channel;
pub mod compat;
pub mod error;
pub mod install;
pub mod keys;
pub mod manifest;
pub mod release;
pub mod service;
pub mod store;
pub mod transport;
pub mod version;

pub use channel::{select, Channel, ChannelCandidate};
pub use compat::{
    check as check_compatibility, CompatibilityReport, Component, RunningComponents, SchemaRange,
    Verdict,
};
pub use error::{ManifestRefusal, UpdateError};
pub use install::{DigestProbe, HealthProbe, InstallLayout, InstallPointer};
pub use keys::{TrustedKey, TrustedKeys};
pub use manifest::{
    verify_manifest, verify_manifest_at_launch, Artifact, Compatibility, UpdateManifest,
    VerifiedManifest, DEFAULT_CLOCK_SKEW_MS, DEFAULT_MAX_ARTIFACT_BYTES, UPDATE_MANIFEST_SCHEMA,
};
pub use release::{
    launch, release_id_for, resolve_launch, running_install_root, running_release, self_digest,
    validate_release_id, LaunchInputs, ReleaseTarget, TrustEntry, TrustFile, INSTALL_ROOT_ENV,
    LAUNCHER_ROOT_ENV, RELEASE_DIGEST_ENV, RELEASE_ID_ENV, TRUST_FILE_SCHEMA,
};
pub use service::{
    ApplyOutcome, CheckOutcome, RecoveryOutcome, ReleaseOutcome, ReleaseRestarter,
    ReleaseStageOutcome, StageOutcome, StageSummary, StatusView, Updater, UpdaterConfig,
    NATIVE_SCHEMA_SUPPORTED,
};
pub use store::{
    HighWaterMark, MemoryUpdaterStore, SqliteUpdaterStore, UpdateOpId, UpdateOpKind,
    UpdateOpStatus, UpdateOperation, UpdateStoreError, UpdaterStore,
};
pub use transport::{ArtifactFetcher, CheckedHttpFetcher};
pub use version::{Version, VersionRange};
