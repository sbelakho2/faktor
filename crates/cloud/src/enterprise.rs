//! The enterprise retention / deletion / audit / admin-settings domain.
//!
//! Four separable authorities live here, each with its own durable tables
//! (migration v3 of the control-plane ladder, [`crate::enterprise_store`]):
//!
//! 1. **Retention** ([`RetentionClass`], [`ArtifactRecord`],
//!    [`EnterpriseService::gc_pass`]): every stored artifact carries its
//!    class, its expiry and a deletion state. A GC pass deletes ONLY
//!    artifacts whose class policy is a bounded TTL, that are expired, and
//!    whose deletion state is [`DeletionState::Eligible`]. Before any delete
//!    the pass asks a [`RetentionReferenceOracle`] whether the digest is
//!    referenced by recoverable durable state (open landing transactions,
//!    verification records, run bases). A referenced digest is REFUSED with
//!    a typed skip and an audit row — the invariant is enforced again at
//!    the blob store ([`RetentionBlobStore`], and in `faktor-cas` the
//!    guarded delete primitive), so a buggy oracle cannot lose rollback
//!    material. Reference scans are bounded and loud: a store error is a
//!    typed refusal with an audit row, never a silent delete.
//! 2. **Deletion workflow** ([`DeletionJob`]): organization/account
//!    deletion is a durable multi-step job (freeze admissions -> export
//!    manifest -> delete by class with billing retained by policy ->
//!    tombstone + audit). Every step is idempotent, so a crash between
//!    steps resumes from the recorded `next_step` and a crash mid-step
//!    re-runs the step without duplicating its audit row.
//! 3. **Enterprise audit ledger** ([`AuditEvent`]): append-only rows
//!    {principal, organization, action, object, timestamp, before/after
//!    refs, correlation id}. It is a SEPARATE table and type from the
//!    engineering proof/verification records (never mixed); the cursor
//!    export is the only read surface. Appends are idempotent per logical
//!    mutation (derived event key), so a retried HTTP call never forks the
//!    ledger.
//! 4. **Admin settings** ([`OrgSettings`]): allowed providers/models,
//!    retention overrides within the class policy ceilings and the SSO/OIDC
//!    config reference; writes are role-gated and audited with before/after
//!    digests.
//!
//! Layered configuration (system -> org -> repository -> user -> session ->
//! task, policy vs preference) lives in [`crate::layered_config`]; the
//! OIDC adapter seam in [`crate::oidc`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::ControlPlaneError;
use crate::ids::{ArtifactId, DeletionJobId, OrganizationId, UserId};
use crate::rbac::{authorize, Action, Principal, Resource, Role};

/// Bound on one artifact kind / object / principal text.
pub const MAX_ENTERPRISE_TEXT_BYTES: usize = 256;
/// Bound on one canonical digest (64 lowercase hex BLAKE3).
pub const DIGEST_HEX_BYTES: usize = 64;
/// Hard bound on one GC pass (bounded everything: a bigger store pages).
pub const MAX_GC_PASS: usize = 500;
/// Hard bound on one deletion-manifest enumeration.
pub const MAX_DELETION_MANIFEST: usize = 10_000;
/// Hard bound on one artifact/deletion-job listing page.
pub const MAX_ARTIFACT_PAGE: usize = 200;
/// Hard bound on one audit export page.
pub const MAX_AUDIT_PAGE: usize = 200;
/// Smallest accepted retention TTL (one minute): the policy floor.
pub const MIN_RETENTION_TTL_MS: i64 = 60_000;
/// Domain separation of the deletion-manifest digest.
const MANIFEST_DIGEST_DOMAIN: &str = "faktor-deletion-manifest:v1";
/// Domain separation of the tombstone digest.
const TOMBSTONE_DIGEST_DOMAIN: &str = "faktor-deletion-tombstone:v1";

/// One canonical-object digest: optional `blake3:` prefix + 64 lowercase
/// hex characters.
pub fn valid_digest(value: &str) -> bool {
    let hex = value.strip_prefix("blake3:").unwrap_or(value);
    hex.len() == DIGEST_HEX_BYTES
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn require_digest(field: &str, value: &str) -> Result<(), ControlPlaneError> {
    if !valid_digest(value) {
        return Err(ControlPlaneError::Malformed(format!(
            "{field} must be a 64-char lowercase hex digest"
        )));
    }
    Ok(())
}

fn require_text(field: &str, value: &str) -> Result<(), ControlPlaneError> {
    if value.is_empty() || value.len() > MAX_ENTERPRISE_TEXT_BYTES {
        return Err(ControlPlaneError::Malformed(format!(
            "{field} must be 1..={MAX_ENTERPRISE_TEXT_BYTES} bytes"
        )));
    }
    if value.bytes().any(|b| b.is_ascii_control()) {
        return Err(ControlPlaneError::Malformed(format!(
            "{field} contains control characters"
        )));
    }
    Ok(())
}

fn canonical_bytes(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for part in parts {
        out.extend_from_slice(&(part.len() as u64).to_le_bytes());
        out.extend_from_slice(part);
    }
    out
}

/// The deterministic digest of one configuration layer (the before/after
/// reference of `PolicyChanged` audit rows).
pub fn config_layer_digest(
    layer: &crate::layered_config::ConfigLayer,
) -> Result<String, ControlPlaneError> {
    let bytes = serde_json::to_vec(layer)
        .map_err(|e| ControlPlaneError::Malformed(format!("layer digest: {e}")))?;
    Ok(blake3_hex("faktor-config-layer:v1", &[&bytes]))
}

fn blake3_hex(domain: &str, parts: &[&[u8]]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(domain.as_bytes());
    hasher.update(&[0]);
    hasher.update(&canonical_bytes(parts));
    hasher.finalize().to_hex().to_string()
}

// ------------------------------------------------------------- retention

/// One retention class. The class decides the default policy (TTL or
/// keep-forever) and is the unit deletion jobs reconcile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetentionClass {
    /// Authority-critical rollback data (landing-transaction base blobs).
    /// Bounded TTL: it exists to make a recoverable transaction recoverable.
    AuthorityCriticalRollback,
    /// Verification evidence / candidate proof references. Keep forever:
    /// deletion would destroy the engineering audit trail.
    VerificationEvidence,
    /// Source excerpts captured for context. Bounded TTL.
    SourceExcerpt,
    /// Provider transcripts. Bounded TTL (privacy-sensitive).
    ProviderTranscript,
    /// Terminal output. Bounded TTL.
    TerminalOutput,
    /// Diagnostics bundles. Bounded TTL.
    Diagnostics,
    /// User-authored artifacts. Keep forever.
    UserArtifact,
    /// Billing records. Keep forever: retained by policy during
    /// organization/account deletion.
    BillingRecord,
}

impl RetentionClass {
    pub const ALL: [RetentionClass; 8] = [
        RetentionClass::AuthorityCriticalRollback,
        RetentionClass::VerificationEvidence,
        RetentionClass::SourceExcerpt,
        RetentionClass::ProviderTranscript,
        RetentionClass::TerminalOutput,
        RetentionClass::Diagnostics,
        RetentionClass::UserArtifact,
        RetentionClass::BillingRecord,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            RetentionClass::AuthorityCriticalRollback => "authority_critical_rollback",
            RetentionClass::VerificationEvidence => "verification_evidence",
            RetentionClass::SourceExcerpt => "source_excerpt",
            RetentionClass::ProviderTranscript => "provider_transcript",
            RetentionClass::TerminalOutput => "terminal_output",
            RetentionClass::Diagnostics => "diagnostics",
            RetentionClass::UserArtifact => "user_artifact",
            RetentionClass::BillingRecord => "billing_record",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|c| c.as_str() == raw)
    }

    /// The default policy of this class (TTL classes carry a default TTL and
    /// a hard ceiling; keep-forever classes carry neither).
    pub const fn default_policy(self) -> RetentionPolicy {
        const DAY: i64 = 24 * 60 * 60 * 1000;
        match self {
            RetentionClass::AuthorityCriticalRollback => RetentionPolicy::Ttl {
                ttl_ms: 30 * DAY,
                ceiling_ms: 180 * DAY,
            },
            RetentionClass::VerificationEvidence => RetentionPolicy::KeepForever,
            RetentionClass::SourceExcerpt => RetentionPolicy::Ttl {
                ttl_ms: 30 * DAY,
                ceiling_ms: 365 * DAY,
            },
            RetentionClass::ProviderTranscript => RetentionPolicy::Ttl {
                ttl_ms: 7 * DAY,
                ceiling_ms: 30 * DAY,
            },
            RetentionClass::TerminalOutput => RetentionPolicy::Ttl {
                ttl_ms: 3 * DAY,
                ceiling_ms: 30 * DAY,
            },
            RetentionClass::Diagnostics => RetentionPolicy::Ttl {
                ttl_ms: 14 * DAY,
                ceiling_ms: 90 * DAY,
            },
            RetentionClass::UserArtifact => RetentionPolicy::KeepForever,
            RetentionClass::BillingRecord => RetentionPolicy::KeepForever,
        }
    }
}

/// One class policy: a bounded TTL with its hard ceiling, or keep-forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum RetentionPolicy {
    Ttl { ttl_ms: i64, ceiling_ms: i64 },
    KeepForever,
}

impl RetentionPolicy {
    pub const fn is_keep_forever(self) -> bool {
        matches!(self, RetentionPolicy::KeepForever)
    }

    /// The expiry of an artifact registered now under this policy
    /// (`None` for keep-forever).
    pub const fn expiry_of(self, created_at_ms: i64) -> Option<i64> {
        match self {
            RetentionPolicy::Ttl { ttl_ms, .. } => created_at_ms.checked_add(ttl_ms),
            RetentionPolicy::KeepForever => None,
        }
    }

    /// Validate an override within this policy's ceiling (the intersect-only
    /// retention ceiling: an override may shorten a TTL, never lengthen it
    /// past the ceiling, and keep-forever classes refuse overrides).
    pub fn validate_override(
        self,
        class: RetentionClass,
        ttl_ms: i64,
    ) -> Result<(), ControlPlaneError> {
        match self {
            RetentionPolicy::KeepForever => Err(ControlPlaneError::Conflict(format!(
                "retention class {} is keep-forever and cannot be shortened",
                class.as_str()
            ))),
            RetentionPolicy::Ttl { ceiling_ms, .. } => {
                if ttl_ms < MIN_RETENTION_TTL_MS {
                    return Err(ControlPlaneError::Malformed(format!(
                        "retention override {ttl_ms} is below the {MIN_RETENTION_TTL_MS} ms floor"
                    )));
                }
                if ttl_ms > ceiling_ms {
                    return Err(ControlPlaneError::Conflict(format!(
                        "retention override {ttl_ms} exceeds the {} ceiling {ceiling_ms}",
                        class.as_str()
                    )));
                }
                Ok(())
            }
        }
    }
}

/// The kind of a retained artifact (semantic label; the class is the policy
/// unit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    RollbackBlob,
    ProofArtifact,
    SourceExcerpt,
    ProviderTranscript,
    TerminalOutput,
    DiagnosticBundle,
    UserUpload,
    BillingLedger,
}

impl ArtifactKind {
    pub const ALL: [ArtifactKind; 8] = [
        ArtifactKind::RollbackBlob,
        ArtifactKind::ProofArtifact,
        ArtifactKind::SourceExcerpt,
        ArtifactKind::ProviderTranscript,
        ArtifactKind::TerminalOutput,
        ArtifactKind::DiagnosticBundle,
        ArtifactKind::UserUpload,
        ArtifactKind::BillingLedger,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            ArtifactKind::RollbackBlob => "rollback_blob",
            ArtifactKind::ProofArtifact => "proof_artifact",
            ArtifactKind::SourceExcerpt => "source_excerpt",
            ArtifactKind::ProviderTranscript => "provider_transcript",
            ArtifactKind::TerminalOutput => "terminal_output",
            ArtifactKind::DiagnosticBundle => "diagnostic_bundle",
            ArtifactKind::UserUpload => "user_upload",
            ArtifactKind::BillingLedger => "billing_ledger",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.as_str() == raw)
    }
}

/// The deletion state of one artifact. Only [`DeletionState::Eligible`]
/// artifacts are candidates for GC; `Retained` marks rows kept by deletion
/// policy (billing) and `Deleted` is a tombstone state (re-running GC on it
/// is a no-op, never a second delete).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeletionState {
    /// Registered, still within its retention window or not yet marked.
    Active,
    /// Expired and offered to the GC pass.
    Eligible,
    /// Retained by deletion policy (billing records).
    Retained,
    /// Deleted (the durable tombstone state; the blob is gone).
    Deleted,
}

impl DeletionState {
    pub const fn as_str(self) -> &'static str {
        match self {
            DeletionState::Active => "active",
            DeletionState::Eligible => "eligible",
            DeletionState::Retained => "retained",
            DeletionState::Deleted => "deleted",
        }
    }
}

/// One retained artifact row. `digest` is the content address of the blob;
/// `session`/`task`/`owner` are optional correlation columns (bounded text /
/// numeric ids, never free-form).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRecord {
    pub id: ArtifactId,
    pub organization: OrganizationId,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub task: Option<u64>,
    /// The account/user this artifact belongs to (account-scoped deletion);
    /// `None` for organization-scoped artifacts.
    #[serde(default)]
    pub owner: Option<String>,
    pub kind: ArtifactKind,
    pub digest: String,
    pub size: u64,
    pub retention_class: RetentionClass,
    pub created_at_ms: i64,
    /// `None` for keep-forever classes.
    #[serde(default)]
    pub expires_at_ms: Option<i64>,
    pub deletion_state: DeletionState,
}

impl ArtifactRecord {
    pub fn validate(&self) -> Result<(), ControlPlaneError> {
        require_text("artifact id", self.id.as_str())?;
        require_text("organization", self.organization.as_str())?;
        if let Some(session) = &self.session {
            require_text("artifact session", session)?;
        }
        if let Some(owner) = &self.owner {
            require_text("artifact owner", owner)?;
        }
        require_digest("artifact digest", &self.digest)?;
        if self.size == 0 {
            return Err(ControlPlaneError::Malformed(
                "artifact size must be > 0".into(),
            ));
        }
        if self.created_at_ms <= 0 {
            return Err(ControlPlaneError::Malformed(
                "artifact created_at_ms must be positive".into(),
            ));
        }
        match self.retention_class.default_policy() {
            RetentionPolicy::KeepForever => {
                if self.expires_at_ms.is_some() {
                    return Err(ControlPlaneError::Malformed(
                        "keep-forever artifacts carry no expiry".into(),
                    ));
                }
            }
            RetentionPolicy::Ttl { .. } => {
                let Some(expires) = self.expires_at_ms else {
                    return Err(ControlPlaneError::Malformed(
                        "TTL artifacts must carry an expiry".into(),
                    ));
                };
                if expires <= self.created_at_ms {
                    return Err(ControlPlaneError::Malformed(
                        "artifact expiry must be after created_at_ms".into(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Whether the GC pass may consider this row: `Eligible`, expired and a
    /// TTL class.
    pub fn is_gc_candidate(&self, now_ms: i64) -> bool {
        if self.deletion_state != DeletionState::Eligible {
            return false;
        }
        if self.retention_class.default_policy().is_keep_forever() {
            return false;
        }
        matches!(self.expires_at_ms, Some(expiry) if expiry <= now_ms)
    }
}

/// A freshly registered artifact (the caller carries an explicit id so
/// re-registration is idempotent by construction).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewArtifact {
    pub id: ArtifactId,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub task: Option<u64>,
    #[serde(default)]
    pub owner: Option<String>,
    pub kind: ArtifactKind,
    pub digest: String,
    pub size: u64,
    pub retention_class: RetentionClass,
    /// `None` = the class default TTL (or keep-forever).
    #[serde(default)]
    pub ttl_ms: Option<i64>,
}

/// One durable reference that protects a digest from deletion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactReference {
    pub kind: ReferenceKind,
    /// The durable row id (run id, record id, snapshot hash...).
    pub reference: String,
    /// Bounded human-readable detail (never free-form data).
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceKind {
    /// An open/recoverable landing transaction (`IntegrationTxnRow`).
    IntegrationTxn,
    /// A durable verification record.
    VerificationRecord,
    /// A recorded run base snapshot.
    RunBase,
    /// An open durable edit transaction (staged base content).
    OpenEditTxn,
}

impl ReferenceKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            ReferenceKind::IntegrationTxn => "integration_txn",
            ReferenceKind::VerificationRecord => "verification_record",
            ReferenceKind::RunBase => "run_base",
            ReferenceKind::OpenEditTxn => "open_edit_txn",
        }
    }
}

/// Typed reference-scan failure. Every variant REFUSES a deletion (fail
/// closed) and is surfaced in the GC report + audit ledger.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReferenceScanError {
    #[error("reference scan unavailable: {0}")]
    Unavailable(String),
    #[error("reference scan exceeded its bound: {0}")]
    BoundExceeded(String),
    #[error("reference scan found corrupt durable state: {0}")]
    Malformed(String),
}

impl ReferenceScanError {
    pub const fn code_tag(&self) -> &'static str {
        match self {
            ReferenceScanError::Unavailable(_) => "unavailable",
            ReferenceScanError::BoundExceeded(_) => "bound_exceeded",
            ReferenceScanError::Malformed(_) => "malformed",
        }
    }
}

/// The reference oracle seam: does this digest back recoverable durable
/// state? Implementations own their bound (a store error is a typed
/// [`ReferenceScanError`], never an empty reference list).
pub trait RetentionReferenceOracle: Send + Sync {
    fn references(
        &self,
        organization: &OrganizationId,
        digest: &str,
    ) -> Result<Vec<ArtifactReference>, ReferenceScanError>;
}

/// An oracle for hosts with no recoverable durable state wired: it returns
/// no references (the blob store's own guard remains the hard invariant).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoReferences;

impl RetentionReferenceOracle for NoReferences {
    fn references(
        &self,
        _organization: &OrganizationId,
        _digest: &str,
    ) -> Result<Vec<ArtifactReference>, ReferenceScanError> {
        Ok(Vec::new())
    }
}

/// The outcome of one guarded blob deletion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobDeletion {
    /// The blob was present and removed.
    Deleted,
    /// The blob was already absent (idempotent replay).
    Absent,
    /// A reference guard refused the deletion (typed; the blob is intact).
    Refused { reference: ArtifactReference },
}

/// Typed blob-store failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BlobStoreError {
    #[error("blob store unavailable: {0}")]
    Unavailable(String),
}

/// The blob deletion seam. Implementations MUST enforce the referential
/// protection themselves as well (the `faktor-cas` guarded delete refuses a
/// digest its guard reports as live), so a caller bug cannot delete
/// rollback material.
pub trait RetentionBlobStore: Send + Sync {
    fn delete_blob(&self, digest: &str) -> Result<BlobDeletion, BlobStoreError>;
}

/// One GC decision. Every `Refused*`/`Skipped*` row is also an audit row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "disposition")]
pub enum GcOutcome {
    Deleted {
        artifact: ArtifactId,
        digest: String,
    },
    AlreadyAbsent {
        artifact: ArtifactId,
        digest: String,
    },
    SkippedNotEligible {
        artifact: ArtifactId,
        digest: String,
        state: DeletionState,
    },
    SkippedNotExpired {
        artifact: ArtifactId,
        digest: String,
    },
    SkippedKeepForever {
        artifact: ArtifactId,
        digest: String,
        class: RetentionClass,
    },
    RefusedProtected {
        artifact: ArtifactId,
        digest: String,
        reference: ArtifactReference,
    },
    RefusedByBlobStore {
        artifact: ArtifactId,
        digest: String,
        reference: ArtifactReference,
    },
    ScanUnavailable {
        artifact: ArtifactId,
        digest: String,
        error: String,
    },
    DeleteFailed {
        artifact: ArtifactId,
        digest: String,
        error: String,
    },
}

impl GcOutcome {
    pub fn artifact(&self) -> &ArtifactId {
        match self {
            GcOutcome::Deleted { artifact, .. }
            | GcOutcome::AlreadyAbsent { artifact, .. }
            | GcOutcome::SkippedNotEligible { artifact, .. }
            | GcOutcome::SkippedNotExpired { artifact, .. }
            | GcOutcome::SkippedKeepForever { artifact, .. }
            | GcOutcome::RefusedProtected { artifact, .. }
            | GcOutcome::RefusedByBlobStore { artifact, .. }
            | GcOutcome::ScanUnavailable { artifact, .. }
            | GcOutcome::DeleteFailed { artifact, .. } => artifact,
        }
    }
}

/// One GC pass report: bounded, loud, and the input to the audit rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GcReport {
    pub organization: OrganizationId,
    pub scanned: usize,
    pub outcomes: Vec<GcOutcome>,
    pub deleted: u64,
    pub refused_protected: u64,
    pub scan_failures: u64,
    pub delete_failures: u64,
}

impl GcReport {
    pub fn is_clean(&self) -> bool {
        self.scan_failures == 0 && self.delete_failures == 0
    }
}

// ----------------------------------------------------------------- audit

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditPrincipalKind {
    User,
    ServiceAccount,
    System,
}

impl AuditPrincipalKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            AuditPrincipalKind::User => "user",
            AuditPrincipalKind::ServiceAccount => "service_account",
            AuditPrincipalKind::System => "system",
        }
    }
}

/// The principal recorded in an audit row (bounded; never a secret).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditPrincipal {
    pub kind: AuditPrincipalKind,
    pub id: String,
}

impl AuditPrincipal {
    pub fn user(id: &UserId) -> Self {
        Self {
            kind: AuditPrincipalKind::User,
            id: id.as_str().to_string(),
        }
    }

    pub fn from_principal(principal: &Principal) -> Self {
        match &principal.subject {
            crate::rbac::PrincipalSubject::User(id) => Self::user(id),
            crate::rbac::PrincipalSubject::ServiceAccount(id) => Self {
                kind: AuditPrincipalKind::ServiceAccount,
                id: id.as_str().to_string(),
            },
        }
    }

    pub fn system() -> Self {
        Self {
            kind: AuditPrincipalKind::System,
            id: "system".into(),
        }
    }
}

/// Every audited enterprise mutation. Stored as a stable string; parsing an
/// unknown action is `None` (never a guessed row).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditAction {
    MemberAdded,
    MemberInvited,
    MemberRemoved,
    MemberRoleChanged,
    RepositoryInstallationAdded,
    RepositoryInstallationChanged,
    RepositoryInstallationRemoved,
    ProviderCredentialCreated,
    ProviderCredentialRotated,
    ProviderCredentialRevoked,
    PolicyChanged,
    SecretCreated,
    SecretRotated,
    SecretDeleted,
    SecretAccessed,
    HumanApprovalRequested,
    HumanApprovalDecided,
    WorkerRegistered,
    WorkerRevoked,
    EntitlementChanged,
    AdminSettingChanged,
    PrivilegedToolGranted,
    PrivilegedToolRevoked,
    RetentionArtifactRegistered,
    RetentionDelete,
    RetentionDeleteRefused,
    RetentionScanFailed,
    RetentionDeleteFailed,
    DeletionJobStarted,
    DeletionAdmissionsFrozen,
    DeletionManifestExported,
    DeletionClassReconciled,
    DeletionTombstoned,
}

impl AuditAction {
    pub const ALL: [AuditAction; 33] = [
        AuditAction::MemberAdded,
        AuditAction::MemberInvited,
        AuditAction::MemberRemoved,
        AuditAction::MemberRoleChanged,
        AuditAction::RepositoryInstallationAdded,
        AuditAction::RepositoryInstallationChanged,
        AuditAction::RepositoryInstallationRemoved,
        AuditAction::ProviderCredentialCreated,
        AuditAction::ProviderCredentialRotated,
        AuditAction::ProviderCredentialRevoked,
        AuditAction::PolicyChanged,
        AuditAction::SecretCreated,
        AuditAction::SecretRotated,
        AuditAction::SecretDeleted,
        AuditAction::SecretAccessed,
        AuditAction::HumanApprovalRequested,
        AuditAction::HumanApprovalDecided,
        AuditAction::WorkerRegistered,
        AuditAction::WorkerRevoked,
        AuditAction::EntitlementChanged,
        AuditAction::AdminSettingChanged,
        AuditAction::PrivilegedToolGranted,
        AuditAction::PrivilegedToolRevoked,
        AuditAction::RetentionArtifactRegistered,
        AuditAction::RetentionDelete,
        AuditAction::RetentionDeleteRefused,
        AuditAction::RetentionScanFailed,
        AuditAction::RetentionDeleteFailed,
        AuditAction::DeletionJobStarted,
        AuditAction::DeletionAdmissionsFrozen,
        AuditAction::DeletionManifestExported,
        AuditAction::DeletionClassReconciled,
        AuditAction::DeletionTombstoned,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            AuditAction::MemberAdded => "member_added",
            AuditAction::MemberInvited => "member_invited",
            AuditAction::MemberRemoved => "member_removed",
            AuditAction::MemberRoleChanged => "member_role_changed",
            AuditAction::RepositoryInstallationAdded => "repository_installation_added",
            AuditAction::RepositoryInstallationChanged => "repository_installation_changed",
            AuditAction::RepositoryInstallationRemoved => "repository_installation_removed",
            AuditAction::ProviderCredentialCreated => "provider_credential_created",
            AuditAction::ProviderCredentialRotated => "provider_credential_rotated",
            AuditAction::ProviderCredentialRevoked => "provider_credential_revoked",
            AuditAction::PolicyChanged => "policy_changed",
            AuditAction::SecretCreated => "secret_created",
            AuditAction::SecretRotated => "secret_rotated",
            AuditAction::SecretDeleted => "secret_deleted",
            AuditAction::SecretAccessed => "secret_accessed",
            AuditAction::HumanApprovalRequested => "human_approval_requested",
            AuditAction::HumanApprovalDecided => "human_approval_decided",
            AuditAction::WorkerRegistered => "worker_registered",
            AuditAction::WorkerRevoked => "worker_revoked",
            AuditAction::EntitlementChanged => "entitlement_changed",
            AuditAction::AdminSettingChanged => "admin_setting_changed",
            AuditAction::PrivilegedToolGranted => "privileged_tool_granted",
            AuditAction::PrivilegedToolRevoked => "privileged_tool_revoked",
            AuditAction::RetentionArtifactRegistered => "retention_artifact_registered",
            AuditAction::RetentionDelete => "retention_delete",
            AuditAction::RetentionDeleteRefused => "retention_delete_refused",
            AuditAction::RetentionScanFailed => "retention_scan_failed",
            AuditAction::RetentionDeleteFailed => "retention_delete_failed",
            AuditAction::DeletionJobStarted => "deletion_job_started",
            AuditAction::DeletionAdmissionsFrozen => "deletion_admissions_frozen",
            AuditAction::DeletionManifestExported => "deletion_manifest_exported",
            AuditAction::DeletionClassReconciled => "deletion_class_reconciled",
            AuditAction::DeletionTombstoned => "deletion_tombstoned",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|a| a.as_str() == raw)
    }
}

/// One audit row BEFORE the store assigns its durable sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewAuditEvent {
    pub organization: OrganizationId,
    pub principal: AuditPrincipal,
    pub action: AuditAction,
    pub object_kind: String,
    pub object: String,
    pub timestamp_ms: i64,
    #[serde(default)]
    pub before_ref: Option<String>,
    #[serde(default)]
    pub after_ref: Option<String>,
    /// Shared correlation id (task/run id, idempotency key, approval id)
    /// where applicable.
    #[serde(default)]
    pub correlation_id: Option<String>,
}

impl NewAuditEvent {
    /// The idempotent-append key: retrying the same logical mutation appends
    /// ONE row. Derived from every non-timestamp field.
    pub fn event_key(&self) -> String {
        blake3_hex(
            "faktor-audit-event-key:v1",
            &[
                self.organization.as_str().as_bytes(),
                self.principal.kind.as_str().as_bytes(),
                self.principal.id.as_bytes(),
                self.action.as_str().as_bytes(),
                self.object_kind.as_bytes(),
                self.object.as_bytes(),
                self.before_ref.as_deref().unwrap_or("").as_bytes(),
                self.after_ref.as_deref().unwrap_or("").as_bytes(),
                self.correlation_id.as_deref().unwrap_or("").as_bytes(),
            ],
        )
    }

    pub fn validate(&self) -> Result<(), ControlPlaneError> {
        require_text("audit object kind", &self.object_kind)?;
        require_text("audit object", &self.object)?;
        require_text("audit principal", &self.principal.id)?;
        if self.timestamp_ms <= 0 {
            return Err(ControlPlaneError::Malformed(
                "audit timestamp must be positive".into(),
            ));
        }
        for (field, value) in [
            ("before_ref", &self.before_ref),
            ("after_ref", &self.after_ref),
            ("correlation_id", &self.correlation_id),
        ] {
            if let Some(value) = value {
                require_text(field, value)?;
            }
        }
        Ok(())
    }
}

/// One durable audit row (append-only; `seq` is the cursor).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditEvent {
    pub seq: i64,
    pub event_key: String,
    #[serde(flatten)]
    pub event: NewAuditEvent,
}

/// One page of the audit ledger.
#[derive(Debug, Clone)]
pub struct AuditExport {
    pub events: Vec<AuditEvent>,
    pub next_cursor: Option<String>,
    pub head_seq: i64,
}

// -------------------------------------------------------------- deletion

/// What a deletion job removes: the whole tenant, or one account's data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "scope")]
pub enum DeletionScope {
    Organization,
    Account { user: UserId },
}

impl DeletionScope {
    pub fn key(&self, organization: &OrganizationId) -> String {
        match self {
            DeletionScope::Organization => format!("org:{}", organization.as_str()),
            DeletionScope::Account { user } => {
                format!("account:{}:{}", organization.as_str(), user.as_str())
            }
        }
    }
}

/// The ordered steps of one deletion job. Each step is idempotent; the job
/// resumes at `next_step` after a crash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeletionStep {
    /// Freeze new admissions for the scope (no new artifact writes).
    FreezeAdmissions,
    /// Enumerate the scope's artifacts into the durable export manifest,
    /// splitting ordinary rows from billing-retained rows.
    ExportManifest,
    /// Delete ordinary classes (reference-protected); billing rows are
    /// retained by policy.
    DeleteByClass,
    /// Write the tombstone (redacted scope identity) and complete.
    Tombstone,
}

impl DeletionStep {
    pub const ORDER: [DeletionStep; 4] = [
        DeletionStep::FreezeAdmissions,
        DeletionStep::ExportManifest,
        DeletionStep::DeleteByClass,
        DeletionStep::Tombstone,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            DeletionStep::FreezeAdmissions => "freeze_admissions",
            DeletionStep::ExportManifest => "export_manifest",
            DeletionStep::DeleteByClass => "delete_by_class",
            DeletionStep::Tombstone => "tombstone",
        }
    }

    fn next(self) -> Option<DeletionStep> {
        match self {
            DeletionStep::FreezeAdmissions => Some(DeletionStep::ExportManifest),
            DeletionStep::ExportManifest => Some(DeletionStep::DeleteByClass),
            DeletionStep::DeleteByClass => Some(DeletionStep::Tombstone),
            DeletionStep::Tombstone => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeletionJobState {
    Running,
    Completed,
}

/// One manifest row (id/class/digest/size only — never content).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestEntry {
    pub id: ArtifactId,
    pub retention_class: RetentionClass,
    pub digest: String,
    pub size: u64,
}

/// The durable export manifest of one deletion scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeletionManifest {
    pub generated_at_ms: i64,
    /// Ordinary artifacts the job deletes.
    pub entries: Vec<ManifestEntry>,
    /// Billing-retained rows (kept by policy; recorded for the audit).
    pub retained: Vec<ManifestEntry>,
    /// Deterministic digest over the canonical (ordered) manifest rows.
    pub digest: String,
}

impl DeletionManifest {
    pub fn compute_digest(
        generated_at_ms: i64,
        entries: &[ManifestEntry],
        retained: &[ManifestEntry],
    ) -> String {
        let mut rows: Vec<String> = Vec::new();
        for (label, list) in [("delete", entries), ("retain", retained)] {
            for entry in list {
                rows.push(format!(
                    "{label}\0{}\0{}\0{}\0{}",
                    entry.id.as_str(),
                    entry.retention_class.as_str(),
                    entry.digest,
                    entry.size
                ));
            }
        }
        rows.sort();
        blake3_hex(
            MANIFEST_DIGEST_DOMAIN,
            &[&generated_at_ms.to_le_bytes(), rows.join("\n").as_bytes()],
        )
    }
}

/// One durable deletion job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeletionJob {
    pub id: DeletionJobId,
    pub organization: OrganizationId,
    pub scope: DeletionScope,
    pub next_step: DeletionStep,
    pub state: DeletionJobState,
    #[serde(default)]
    pub frozen_at_ms: Option<i64>,
    #[serde(default)]
    pub manifest: Option<DeletionManifest>,
    /// Per-class deleted counts (ordinary classes only).
    #[serde(default)]
    pub deleted_by_class: BTreeMap<RetentionClass, u64>,
    /// Artifacts skipped because a recoverable reference protects them.
    #[serde(default)]
    pub skipped_protected: u64,
    #[serde(default)]
    pub tombstone_digest: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl DeletionJob {
    /// The deterministic job id: one live job per (organization, scope).
    pub fn deterministic_id(organization: &OrganizationId, scope: &DeletionScope) -> DeletionJobId {
        let key = scope.key(organization);
        let digest = blake3_hex(
            "faktor-deletion-job:v1",
            &[organization.as_str().as_bytes(), key.as_bytes()],
        );
        DeletionJobId::try_new(format!("del_{}", &digest[..32]))
            .expect("a 32-hex deletion job id is always valid")
    }
}

/// One tombstone row: the redacted record a completed deletion leaves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tombstone {
    pub scope_key: String,
    pub organization: OrganizationId,
    /// Digest over the redacted scope identity + manifest digest.
    pub digest: String,
    #[serde(default)]
    pub manifest_digest: Option<String>,
    pub deleted_artifacts: u64,
    pub retained_artifacts: u64,
    pub created_at_ms: i64,
}

impl Tombstone {
    pub fn compute_digest(
        scope_key: &str,
        manifest_digest: Option<&str>,
        created_at_ms: i64,
    ) -> String {
        blake3_hex(
            TOMBSTONE_DIGEST_DOMAIN,
            &[
                scope_key.as_bytes(),
                manifest_digest.unwrap_or("").as_bytes(),
                &created_at_ms.to_le_bytes(),
            ],
        )
    }
}

// ----------------------------------------------------- admin settings

/// The SSO/OIDC configuration reference of one organization: WHERE the IdP
/// lives and HOW claims map to membership. No client secret is stored here
/// (the reference names a secret slot; secrets live in the secret plane).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SsoConfigRef {
    /// The issuer URL the discovery document is resolved from.
    pub issuer: String,
    pub client_id: String,
    /// The ID-token claim carrying group membership.
    pub membership_claim: String,
    /// group -> role mapping (bounded).
    pub group_role_map: BTreeMap<String, Role>,
    /// The name of the secret slot holding the client secret.
    #[serde(default)]
    pub client_secret_ref: Option<String>,
    pub enabled: bool,
}

impl SsoConfigRef {
    pub fn validate(&self) -> Result<(), ControlPlaneError> {
        if !(self.issuer.starts_with("https://") || self.issuer.starts_with("http://")) {
            return Err(ControlPlaneError::Malformed(
                "sso issuer must be an http(s) URL".into(),
            ));
        }
        if self.issuer.len() > 2048 {
            return Err(ControlPlaneError::Malformed("sso issuer too long".into()));
        }
        require_text("sso client_id", &self.client_id)?;
        require_text("sso membership_claim", &self.membership_claim)?;
        if self.group_role_map.len() > 64 {
            return Err(ControlPlaneError::Malformed(
                "sso group_role_map exceeds 64 entries".into(),
            ));
        }
        for group in self.group_role_map.keys() {
            require_text("sso group", group)?;
        }
        if let Some(reference) = &self.client_secret_ref {
            require_text("sso client_secret_ref", reference)?;
        }
        Ok(())
    }
}

/// One organization's admin settings. Empty allowed lists mean
/// "unrestricted" (documented); retention overrides must fit their class
/// ceiling (intersect-only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrgSettings {
    pub organization: OrganizationId,
    pub revision: u64,
    #[serde(default)]
    pub allowed_providers: Vec<String>,
    #[serde(default)]
    pub allowed_models: Vec<String>,
    #[serde(default)]
    pub retention_overrides: BTreeMap<RetentionClass, i64>,
    #[serde(default)]
    pub sso: Option<SsoConfigRef>,
    pub updated_at_ms: i64,
}

impl OrgSettings {
    pub fn empty(organization: OrganizationId, now_ms: i64) -> Self {
        Self {
            organization,
            revision: 0,
            allowed_providers: Vec::new(),
            allowed_models: Vec::new(),
            retention_overrides: BTreeMap::new(),
            sso: None,
            updated_at_ms: now_ms,
        }
    }

    pub fn validate(&self) -> Result<(), ControlPlaneError> {
        for (field, list, bound) in [
            ("allowed_providers", &self.allowed_providers, 64usize),
            ("allowed_models", &self.allowed_models, 256usize),
        ] {
            if list.len() > bound {
                return Err(ControlPlaneError::Malformed(format!(
                    "settings {field} exceeds {bound} entries"
                )));
            }
            let mut seen = BTreeSet::new();
            for value in list {
                require_text(&format!("settings {field} entry"), value)?;
                if !seen.insert(value.as_str()) {
                    return Err(ControlPlaneError::Malformed(format!(
                        "settings {field} repeats {value:?}"
                    )));
                }
            }
        }
        for (class, ttl_ms) in &self.retention_overrides {
            class.default_policy().validate_override(*class, *ttl_ms)?;
        }
        if let Some(sso) = &self.sso {
            sso.validate()?;
        }
        Ok(())
    }

    /// A deterministic digest of the settings (before/after refs in audit
    /// rows; digest stored with proof/audit surfaces).
    pub fn digest(&self) -> String {
        let mut rows: Vec<String> = Vec::new();
        for value in &self.allowed_providers {
            rows.push(format!("provider\0{value}"));
        }
        for value in &self.allowed_models {
            rows.push(format!("model\0{value}"));
        }
        for (class, ttl) in &self.retention_overrides {
            rows.push(format!("retention\0{}\0{ttl}", class.as_str()));
        }
        if let Some(sso) = &self.sso {
            rows.push(format!(
                "sso\0{}\0{}\0{}\0{}\0{}",
                sso.issuer,
                sso.client_id,
                sso.membership_claim,
                sso.enabled,
                sso.client_secret_ref.as_deref().unwrap_or("")
            ));
            for (group, role) in &sso.group_role_map {
                rows.push(format!("sso_group\0{group}\0{}", role.as_str()));
            }
        }
        rows.sort();
        blake3_hex("faktor-org-settings:v1", &[rows.join("\n").as_bytes()])
    }
}

/// One retention class with its effective policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassPolicyView {
    pub class: RetentionClass,
    pub policy: RetentionPolicy,
}

/// The administrative status view of one organization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnterpriseStatus {
    pub organization: OrganizationId,
    pub admissions_frozen: bool,
    pub audit_head_seq: i64,
    pub artifact_count: u64,
    pub open_deletion_jobs: u64,
    pub settings_revision: u64,
    pub retention_classes: Vec<ClassPolicyView>,
}

// -------------------------------------------------------- service core

use crate::enterprise_store::{EnterpriseStore, StoredConfigLayer};
use crate::layered_config::ConfigLayer;

/// The enterprise service: retention GC, deletion jobs, audit ledger and
/// admin settings over one durable [`EnterpriseStore`].
pub struct EnterpriseService {
    store: Arc<dyn EnterpriseStore>,
    clock: Arc<dyn crate::service::Clock>,
    policies: BTreeMap<RetentionClass, RetentionPolicy>,
}

impl EnterpriseService {
    /// Build the service with the default class policy table. The durable
    /// admission-freeze set is replayed from the store, so a restart keeps a
    /// running deletion job's freeze intact.
    pub fn new(
        store: Arc<dyn EnterpriseStore>,
        clock: Arc<dyn crate::service::Clock>,
    ) -> Result<Self, ControlPlaneError> {
        let policies = RetentionClass::ALL
            .iter()
            .map(|class| (*class, class.default_policy()))
            .collect::<BTreeMap<_, _>>();
        Ok(Self {
            store,
            clock,
            policies,
        })
    }

    /// The production constructor (wall clock).
    pub fn with_system_clock(store: Arc<dyn EnterpriseStore>) -> Result<Self, ControlPlaneError> {
        Self::new(store, Arc::new(crate::service::SystemClock))
    }

    pub fn policy_table(&self) -> &BTreeMap<RetentionClass, RetentionPolicy> {
        &self.policies
    }

    pub fn store(&self) -> &Arc<dyn EnterpriseStore> {
        &self.store
    }

    /// Whether a deletion job froze new admissions for this organization.
    pub fn admissions_frozen(
        &self,
        organization: &OrganizationId,
    ) -> Result<bool, ControlPlaneError> {
        self.store
            .frozen(organization)
            .map_err(ControlPlaneError::from)
    }

    /// Refuse a new artifact registration/admission while the scope is
    /// frozen (the freeze step of a deletion job).
    pub fn ensure_admissions_open(
        &self,
        organization: &OrganizationId,
    ) -> Result<(), ControlPlaneError> {
        if self.admissions_frozen(organization)? {
            return Err(ControlPlaneError::Conflict(format!(
                "organization {} has frozen admissions (a deletion job is running)",
                organization.as_str()
            )));
        }
        Ok(())
    }

    fn policy_for(&self, class: RetentionClass) -> RetentionPolicy {
        self.policies
            .get(&class)
            .copied()
            .unwrap_or_else(|| class.default_policy())
    }

    /// Register one artifact. Role-gated (retention write), refused while
    /// admissions are frozen, validates the row against the class policy
    /// (including the caller's TTL override within the ceiling), and audited
    /// with before/after refs. Re-registering the same id with the same row
    /// is idempotent; a different row is a conflict.
    pub fn register_artifact(
        &self,
        principal: &Principal,
        new: NewArtifact,
    ) -> Result<ArtifactRecord, ControlPlaneError> {
        let organization = principal.organization.clone();
        authorize(
            principal,
            &organization,
            Resource::Enterprise,
            Action::RetentionWrite,
        )?;
        self.ensure_admissions_open(&organization)?;
        let now = self.clock.now_ms();
        let policy = self.policy_for(new.retention_class);
        if let Some(ttl) = new.ttl_ms {
            policy.validate_override(new.retention_class, ttl)?;
        }
        let record = ArtifactRecord {
            id: new.id,
            organization: organization.clone(),
            session: new.session,
            task: new.task,
            owner: new.owner,
            kind: new.kind,
            digest: new.digest,
            size: new.size,
            retention_class: new.retention_class,
            created_at_ms: now,
            expires_at_ms: match new.ttl_ms {
                Some(ttl) => now.checked_add(ttl),
                None => policy.expiry_of(now),
            },
            deletion_state: DeletionState::Active,
        };
        record.validate()?;
        if let Some(existing) = self
            .store
            .artifact(&record.id)
            .map_err(ControlPlaneError::from)?
        {
            if existing != record {
                return Err(ControlPlaneError::Conflict(format!(
                    "artifact {} already exists with a different row",
                    record.id.as_str()
                )));
            }
            return Ok(existing);
        }
        self.store
            .put_artifact(&record)
            .map_err(ControlPlaneError::from)?;
        self.append_audit(
            &organization,
            AuditPrincipal::from_principal(principal),
            AuditAction::RetentionArtifactRegistered,
            "artifact",
            record.id.as_str(),
            None,
            Some(record.digest.as_str()),
            None,
        )?;
        Ok(record)
    }

    /// One cursor page of the organization's artifacts (retention read).
    pub fn artifacts(
        &self,
        principal: &Principal,
        organization: &OrganizationId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<crate::model::Page<ArtifactRecord>, ControlPlaneError> {
        authorize(
            principal,
            organization,
            Resource::Enterprise,
            Action::RetentionRead,
        )?;
        if limit == 0 || limit > MAX_ARTIFACT_PAGE {
            return Err(ControlPlaneError::Malformed(format!(
                "artifact page limit must be 1..={MAX_ARTIFACT_PAGE}"
            )));
        }
        let mut rows = self
            .store
            .artifacts(organization, after, limit + 1)
            .map_err(ControlPlaneError::from)?;
        let has_more = rows.len() > limit;
        rows.truncate(limit);
        let next_cursor = if has_more {
            rows.last().map(|row| row.id.as_str().to_string())
        } else {
            None
        };
        Ok(crate::model::Page {
            items: rows,
            next_cursor,
        })
    }

    /// Promote an expired artifact to [`DeletionState::Eligible`] (the
    /// registration-side transition; a keep-forever row is never promoted).
    pub fn mark_eligible(
        &self,
        principal: &Principal,
        id: &ArtifactId,
    ) -> Result<bool, ControlPlaneError> {
        let organization = principal.organization.clone();
        authorize(
            principal,
            &organization,
            Resource::Enterprise,
            Action::RetentionWrite,
        )?;
        let Some(mut artifact) = self.store.artifact(id).map_err(ControlPlaneError::from)? else {
            return Ok(false);
        };
        if artifact.organization != organization {
            return Err(ControlPlaneError::NotFound("artifact not found".into()));
        }
        if self.policy_for(artifact.retention_class).is_keep_forever() {
            return Err(ControlPlaneError::Conflict(format!(
                "artifact {} is keep-forever and can never be eligible",
                id.as_str()
            )));
        }
        let now = self.clock.now_ms();
        let expired = matches!(artifact.expires_at_ms, Some(expiry) if expiry <= now);
        if !expired {
            return Ok(false);
        }
        artifact.deletion_state = DeletionState::Eligible;
        self.store
            .put_artifact(&artifact)
            .map_err(ControlPlaneError::from)?;
        Ok(true)
    }

    /// One GC pass over the organization's artifacts. Deletes ONLY expired
    /// TTL-class artifacts in state `Eligible`; REFUSES any digest referenced
    /// by recoverable durable state (typed skip + audit row) and fails
    /// closed on scan/store errors. Every decision is an audit row.
    pub fn gc_pass(
        &self,
        principal: &Principal,
        limit: usize,
        oracle: &dyn RetentionReferenceOracle,
        blobs: &dyn RetentionBlobStore,
    ) -> Result<GcReport, ControlPlaneError> {
        let organization = principal.organization.clone();
        authorize(
            principal,
            &organization,
            Resource::Enterprise,
            Action::RetentionGc,
        )?;
        if limit == 0 || limit > MAX_GC_PASS {
            return Err(ControlPlaneError::Malformed(format!(
                "gc pass limit must be 1..={MAX_GC_PASS}"
            )));
        }
        let now = self.clock.now_ms();
        let rows = self
            .store
            .artifacts(&organization, None, limit)
            .map_err(ControlPlaneError::from)?;
        let mut report = GcReport {
            organization: organization.clone(),
            scanned: rows.len(),
            outcomes: Vec::new(),
            deleted: 0,
            refused_protected: 0,
            scan_failures: 0,
            delete_failures: 0,
        };
        let auditor = AuditPrincipal::from_principal(principal);
        for artifact in rows {
            if !artifact.is_gc_candidate(now) {
                report.outcomes.push(self.skip_outcome(&artifact));
                continue;
            }
            let outcome = self.delete_artifact(&organization, &auditor, artifact, oracle, blobs)?;
            match &outcome {
                GcOutcome::Deleted { .. } | GcOutcome::AlreadyAbsent { .. } => report.deleted += 1,
                GcOutcome::RefusedProtected { .. } | GcOutcome::RefusedByBlobStore { .. } => {
                    report.refused_protected += 1
                }
                GcOutcome::ScanUnavailable { .. } => report.scan_failures += 1,
                GcOutcome::DeleteFailed { .. } => report.delete_failures += 1,
                GcOutcome::SkippedNotEligible { .. }
                | GcOutcome::SkippedNotExpired { .. }
                | GcOutcome::SkippedKeepForever { .. } => {}
            }
            report.outcomes.push(outcome);
        }
        Ok(report)
    }

    fn skip_outcome(&self, artifact: &ArtifactRecord) -> GcOutcome {
        let digest = artifact.digest.clone();
        let id = artifact.id.clone();
        if self.policy_for(artifact.retention_class).is_keep_forever() {
            return GcOutcome::SkippedKeepForever {
                artifact: id,
                digest,
                class: artifact.retention_class,
            };
        }
        if artifact.deletion_state != DeletionState::Eligible {
            return GcOutcome::SkippedNotEligible {
                artifact: id,
                digest,
                state: artifact.deletion_state,
            };
        }
        GcOutcome::SkippedNotExpired {
            artifact: id,
            digest,
        }
    }

    /// The ONE guarded deletion primitive: reference-protected, audited and
    /// fail-closed. Used by the GC pass and by deletion jobs.
    fn delete_artifact(
        &self,
        organization: &OrganizationId,
        auditor: &AuditPrincipal,
        artifact: ArtifactRecord,
        oracle: &dyn RetentionReferenceOracle,
        blobs: &dyn RetentionBlobStore,
    ) -> Result<GcOutcome, ControlPlaneError> {
        let id = artifact.id.clone();
        let digest = artifact.digest.clone();
        if artifact.deletion_state == DeletionState::Deleted
            || artifact.deletion_state == DeletionState::Retained
        {
            return Ok(GcOutcome::SkippedNotEligible {
                artifact: id,
                digest,
                state: artifact.deletion_state,
            });
        }
        let references = match oracle.references(organization, &artifact.digest) {
            Ok(references) => references,
            Err(error) => {
                self.append_audit(
                    organization,
                    auditor.clone(),
                    AuditAction::RetentionScanFailed,
                    "artifact",
                    artifact.id.as_str(),
                    None,
                    Some(&format!("scan_error:{}", error.code_tag())),
                    None,
                )?;
                return Ok(GcOutcome::ScanUnavailable {
                    artifact: id,
                    digest,
                    error: error.to_string(),
                });
            }
        };
        if let Some(reference) = references.into_iter().next() {
            self.append_audit(
                organization,
                auditor.clone(),
                AuditAction::RetentionDeleteRefused,
                "artifact",
                artifact.id.as_str(),
                Some("eligible"),
                Some(&format!("protected:{}", reference.kind.as_str())),
                Some(reference.reference.as_str()),
            )?;
            return Ok(GcOutcome::RefusedProtected {
                artifact: id,
                digest,
                reference,
            });
        }
        let deletion = blobs.delete_blob(&artifact.digest);
        match deletion {
            Ok(BlobDeletion::Deleted) | Ok(BlobDeletion::Absent) => {}
            Ok(BlobDeletion::Refused { reference }) => {
                self.append_audit(
                    organization,
                    auditor.clone(),
                    AuditAction::RetentionDeleteRefused,
                    "artifact",
                    artifact.id.as_str(),
                    Some("eligible"),
                    Some(&format!("blob_refused:{}", reference.kind.as_str())),
                    Some(reference.reference.as_str()),
                )?;
                return Ok(GcOutcome::RefusedByBlobStore {
                    artifact: id,
                    digest,
                    reference,
                });
            }
            Err(error) => {
                self.append_audit(
                    organization,
                    auditor.clone(),
                    AuditAction::RetentionDeleteFailed,
                    "artifact",
                    artifact.id.as_str(),
                    Some("eligible"),
                    Some("delete_error"),
                    None,
                )?;
                return Ok(GcOutcome::DeleteFailed {
                    artifact: id,
                    digest,
                    error: error.to_string(),
                });
            }
        }
        let mut updated = artifact.clone();
        updated.deletion_state = DeletionState::Deleted;
        self.store
            .put_artifact(&updated)
            .map_err(ControlPlaneError::from)?;
        self.append_audit(
            organization,
            auditor.clone(),
            AuditAction::RetentionDelete,
            "artifact",
            artifact.id.as_str(),
            Some("eligible"),
            Some("deleted"),
            Some(artifact.digest.as_str()),
        )?;
        Ok(GcOutcome::Deleted {
            artifact: id,
            digest,
        })
    }

    /// Append one audit row on behalf of another enterprise mutation
    /// (role-gated audit write). `before`/`after` are bounded references.
    #[allow(clippy::too_many_arguments)]
    pub fn record_mutation(
        &self,
        principal: &Principal,
        organization: &OrganizationId,
        action: AuditAction,
        object_kind: &str,
        object: &str,
        before_ref: Option<&str>,
        after_ref: Option<&str>,
        correlation_id: Option<&str>,
    ) -> Result<AuditEvent, ControlPlaneError> {
        authorize(
            principal,
            organization,
            Resource::Enterprise,
            Action::AuditWrite,
        )?;
        self.append_audit(
            organization,
            AuditPrincipal::from_principal(principal),
            action,
            object_kind,
            object,
            before_ref,
            after_ref,
            correlation_id,
        )
    }

    /// One cursor page of the audit ledger (role-gated audit read). The
    /// ledger is append-only: the page is a strict `seq > cursor` scan.
    pub fn audit_export(
        &self,
        principal: &Principal,
        organization: &OrganizationId,
        after_seq: Option<i64>,
        limit: usize,
    ) -> Result<AuditExport, ControlPlaneError> {
        authorize(
            principal,
            organization,
            Resource::Enterprise,
            Action::AuditRead,
        )?;
        if limit == 0 || limit > MAX_AUDIT_PAGE {
            return Err(ControlPlaneError::Malformed(format!(
                "audit page limit must be 1..={MAX_AUDIT_PAGE}"
            )));
        }
        let cursor = after_seq.unwrap_or(0);
        if cursor < 0 {
            return Err(ControlPlaneError::Malformed(
                "audit cursor must be >= 0".into(),
            ));
        }
        let head_seq = self
            .store
            .audit_head_seq(organization)
            .map_err(ControlPlaneError::from)?;
        let mut events = self
            .store
            .audit_events(organization, cursor, limit + 1)
            .map_err(ControlPlaneError::from)?;
        let has_more = events.len() > limit;
        events.truncate(limit);
        let next_cursor = if has_more {
            events.last().map(|event| event.seq.to_string())
        } else {
            None
        };
        Ok(AuditExport {
            events,
            next_cursor,
            head_seq,
        })
    }

    /// The organization's settings (the empty default when never written).
    pub fn settings(
        &self,
        principal: &Principal,
        organization: &OrganizationId,
    ) -> Result<OrgSettings, ControlPlaneError> {
        authorize(
            principal,
            organization,
            Resource::Enterprise,
            Action::SettingsRead,
        )?;
        Ok(self
            .store
            .org_settings(organization)
            .map_err(ControlPlaneError::from)?
            .unwrap_or_else(|| OrgSettings::empty(organization.clone(), self.clock.now_ms())))
    }

    /// One page of the organization's durable configuration layers
    /// (settings read).
    pub fn config_layers(
        &self,
        principal: &Principal,
    ) -> Result<Vec<StoredConfigLayer>, ControlPlaneError> {
        authorize(
            principal,
            &principal.organization,
            Resource::Enterprise,
            Action::SettingsRead,
        )?;
        self.store
            .config_layers(&principal.organization, None, MAX_ARTIFACT_PAGE)
            .map_err(ControlPlaneError::from)
    }

    /// Upsert one configuration layer (settings write; the layer is
    /// validated by the ONE layered-config validator and audited as a
    /// `PolicyChanged` row with before/after layer digests).
    pub fn put_config_layer(
        &self,
        principal: &Principal,
        layer: ConfigLayer,
    ) -> Result<StoredConfigLayer, ControlPlaneError> {
        authorize(
            principal,
            &principal.organization,
            Resource::Enterprise,
            Action::SettingsWrite,
        )?;
        layer
            .validate()
            .map_err(|error| ControlPlaneError::Malformed(error.to_string()))?;
        let id = StoredConfigLayer::row_id(&layer);
        let before = self
            .store
            .config_layers(&principal.organization, None, MAX_ARTIFACT_PAGE)
            .map_err(ControlPlaneError::from)?
            .into_iter()
            .find(|row| row.id == id)
            .map(|row| config_layer_digest(&row.layer))
            .transpose()?;
        let row = StoredConfigLayer {
            organization: principal.organization.clone(),
            id: id.clone(),
            layer,
        };
        let after = config_layer_digest(&row.layer)?;
        self.store
            .put_config_layer(&row)
            .map_err(ControlPlaneError::from)?;
        self.append_audit(
            &principal.organization,
            AuditPrincipal::from_principal(principal),
            AuditAction::PolicyChanged,
            "config_layer",
            id.as_str(),
            before.as_deref(),
            Some(after.as_str()),
            None,
        )?;
        Ok(row)
    }

    /// Remove one configuration layer by row id (settings write; audited
    /// with the removed layer's digest as the before ref).
    pub fn remove_config_layer(
        &self,
        principal: &Principal,
        id: &str,
    ) -> Result<bool, ControlPlaneError> {
        authorize(
            principal,
            &principal.organization,
            Resource::Enterprise,
            Action::SettingsWrite,
        )?;
        let before = self
            .store
            .config_layers(&principal.organization, None, MAX_ARTIFACT_PAGE)
            .map_err(ControlPlaneError::from)?
            .into_iter()
            .find(|row| row.id == id)
            .map(|row| config_layer_digest(&row.layer))
            .transpose()?;
        let removed = self
            .store
            .delete_config_layer(&principal.organization, id)
            .map_err(ControlPlaneError::from)?;
        if removed {
            self.append_audit(
                &principal.organization,
                AuditPrincipal::from_principal(principal),
                AuditAction::PolicyChanged,
                "config_layer",
                id,
                before.as_deref(),
                None,
                None,
            )?;
        }
        Ok(removed)
    }

    /// Replace the organization's settings (role-gated; validated against
    /// the retention class ceilings; audited with before/after digests).
    pub fn set_settings(
        &self,
        principal: &Principal,
        mut settings: OrgSettings,
    ) -> Result<OrgSettings, ControlPlaneError> {
        let organization = principal.organization.clone();
        authorize(
            principal,
            &organization,
            Resource::Enterprise,
            Action::SettingsWrite,
        )?;
        if settings.organization != organization {
            return Err(ControlPlaneError::NotFound("organization not found".into()));
        }
        settings.validate()?;
        let previous = self
            .store
            .org_settings(&organization)
            .map_err(ControlPlaneError::from)?;
        let before_digest = previous.as_ref().map(|s| s.digest());
        settings.revision = previous.as_ref().map(|s| s.revision + 1).unwrap_or(1);
        settings.updated_at_ms = self.clock.now_ms();
        let after_digest = settings.digest();
        self.store
            .put_org_settings(&settings)
            .map_err(ControlPlaneError::from)?;
        self.append_audit(
            &organization,
            AuditPrincipal::from_principal(principal),
            AuditAction::AdminSettingChanged,
            "org_settings",
            organization.as_str(),
            before_digest.as_deref(),
            Some(after_digest.as_str()),
            None,
        )?;
        Ok(settings)
    }

    /// The administrative status view (retention read).
    pub fn status(
        &self,
        principal: &Principal,
        organization: &OrganizationId,
    ) -> Result<EnterpriseStatus, ControlPlaneError> {
        authorize(
            principal,
            organization,
            Resource::Enterprise,
            Action::RetentionRead,
        )?;
        let head_seq = self
            .store
            .audit_head_seq(organization)
            .map_err(ControlPlaneError::from)?;
        let artifacts = self
            .store
            .artifacts(organization, None, MAX_ARTIFACT_PAGE)
            .map_err(ControlPlaneError::from)?;
        let jobs = self
            .store
            .deletion_jobs(organization, None, MAX_ARTIFACT_PAGE)
            .map_err(ControlPlaneError::from)?;
        let settings = self
            .store
            .org_settings(organization)
            .map_err(ControlPlaneError::from)?;
        Ok(EnterpriseStatus {
            organization: organization.clone(),
            admissions_frozen: self.admissions_frozen(organization)?,
            audit_head_seq: head_seq,
            artifact_count: artifacts.len() as u64,
            open_deletion_jobs: jobs
                .iter()
                .filter(|job| job.state == DeletionJobState::Running)
                .count() as u64,
            settings_revision: settings.map(|s| s.revision).unwrap_or(0),
            retention_classes: RetentionClass::ALL
                .iter()
                .map(|class| ClassPolicyView {
                    class: *class,
                    policy: self.policy_for(*class),
                })
                .collect(),
        })
    }

    /// Start (or return) the deterministic deletion job for a scope. Owner
    /// gated; the job is durable before the response, and re-starting is
    /// idempotent.
    pub fn start_deletion(
        &self,
        principal: &Principal,
        scope: DeletionScope,
    ) -> Result<DeletionJob, ControlPlaneError> {
        let organization = principal.organization.clone();
        authorize(
            principal,
            &organization,
            Resource::Enterprise,
            Action::DeletionManage,
        )?;
        let id = DeletionJob::deterministic_id(&organization, &scope);
        if let Some(existing) = self
            .store
            .deletion_job(&id)
            .map_err(ControlPlaneError::from)?
        {
            return Ok(existing);
        }
        let now = self.clock.now_ms();
        let job = DeletionJob {
            id: id.clone(),
            organization: organization.clone(),
            scope,
            next_step: DeletionStep::FreezeAdmissions,
            state: DeletionJobState::Running,
            frozen_at_ms: None,
            manifest: None,
            deleted_by_class: BTreeMap::new(),
            skipped_protected: 0,
            tombstone_digest: None,
            created_at_ms: now,
            updated_at_ms: now,
        };
        self.store
            .put_deletion_job(&job)
            .map_err(ControlPlaneError::from)?;
        self.append_audit(
            &organization,
            AuditPrincipal::from_principal(principal),
            AuditAction::DeletionJobStarted,
            "deletion_job",
            job.id.as_str(),
            None,
            Some(job.scope.key(&organization).as_str()),
            None,
        )?;
        Ok(job)
    }

    /// One durable deletion job by id (owner gated; a foreign organization's
    /// job is a byte-identical not-found).
    pub fn deletion_job(
        &self,
        principal: &Principal,
        id: &DeletionJobId,
    ) -> Result<DeletionJob, ControlPlaneError> {
        let organization = principal.organization.clone();
        authorize(
            principal,
            &organization,
            Resource::Enterprise,
            Action::DeletionManage,
        )?;
        let Some(job) = self
            .store
            .deletion_job(id)
            .map_err(ControlPlaneError::from)?
        else {
            return Err(ControlPlaneError::NotFound("deletion job not found".into()));
        };
        if job.organization != organization {
            return Err(ControlPlaneError::NotFound("deletion job not found".into()));
        }
        Ok(job)
    }

    /// One cursor page of the organization's deletion jobs.
    pub fn deletion_jobs(
        &self,
        principal: &Principal,
        after: Option<&str>,
        limit: usize,
    ) -> Result<crate::model::Page<DeletionJob>, ControlPlaneError> {
        let organization = principal.organization.clone();
        authorize(
            principal,
            &organization,
            Resource::Enterprise,
            Action::DeletionManage,
        )?;
        if limit == 0 || limit > MAX_ARTIFACT_PAGE {
            return Err(ControlPlaneError::Malformed(format!(
                "deletion job page limit must be 1..={MAX_ARTIFACT_PAGE}"
            )));
        }
        let mut rows = self
            .store
            .deletion_jobs(&organization, after, limit + 1)
            .map_err(ControlPlaneError::from)?;
        let has_more = rows.len() > limit;
        rows.truncate(limit);
        let next_cursor = if has_more {
            rows.last().map(|job| job.id.as_str().to_string())
        } else {
            None
        };
        Ok(crate::model::Page {
            items: rows,
            next_cursor,
        })
    }

    /// Advance a deletion job by exactly ONE durable step (or return the
    /// completed job). Every step is idempotent: a crash before a step's
    /// durable write resumes it, and a re-run after the write changes
    /// nothing and appends no duplicate audit row (the audit append key is
    /// derived from the step's effects).
    pub fn advance_deletion(
        &self,
        principal: &Principal,
        id: &DeletionJobId,
        oracle: &dyn RetentionReferenceOracle,
        blobs: &dyn RetentionBlobStore,
    ) -> Result<DeletionJob, ControlPlaneError> {
        let mut job = self.deletion_job(principal, id)?;
        if job.state == DeletionJobState::Completed {
            return Ok(job);
        }
        let auditor = AuditPrincipal::from_principal(principal);
        let now = self.clock.now_ms();
        match job.next_step {
            DeletionStep::FreezeAdmissions => {
                if job.frozen_at_ms.is_none() {
                    self.store
                        .set_frozen(&job.organization, true)
                        .map_err(ControlPlaneError::from)?;
                    job.frozen_at_ms = Some(now);
                    self.append_audit(
                        &job.organization,
                        auditor.clone(),
                        AuditAction::DeletionAdmissionsFrozen,
                        "deletion_job",
                        job.id.as_str(),
                        Some("open"),
                        Some("frozen"),
                        None,
                    )?;
                }
            }
            DeletionStep::ExportManifest => {
                if job.manifest.is_none() {
                    let artifacts = self.scope_artifacts(&job)?;
                    let mut entries = Vec::new();
                    let mut retained = Vec::new();
                    for artifact in artifacts {
                        let entry = ManifestEntry {
                            id: artifact.id.clone(),
                            retention_class: artifact.retention_class,
                            digest: artifact.digest.clone(),
                            size: artifact.size,
                        };
                        if artifact.retention_class == RetentionClass::BillingRecord {
                            retained.push(entry);
                        } else {
                            entries.push(entry);
                        }
                    }
                    let digest = DeletionManifest::compute_digest(now, &entries, &retained);
                    job.manifest = Some(DeletionManifest {
                        generated_at_ms: now,
                        entries,
                        retained,
                        digest: digest.clone(),
                    });
                    self.append_audit(
                        &job.organization,
                        auditor.clone(),
                        AuditAction::DeletionManifestExported,
                        "deletion_job",
                        job.id.as_str(),
                        None,
                        Some(digest.as_str()),
                        None,
                    )?;
                }
            }
            DeletionStep::DeleteByClass => {
                let manifest = job.manifest.clone().ok_or_else(|| {
                    ControlPlaneError::Conflict(
                        "deletion job has no manifest; re-run export_manifest".into(),
                    )
                })?;
                let mut class_counts: BTreeMap<RetentionClass, u64> = BTreeMap::new();
                let mut skipped_protected = job.skipped_protected;
                for entry in &manifest.entries {
                    let Some(artifact) = self
                        .store
                        .artifact(&entry.id)
                        .map_err(ControlPlaneError::from)?
                    else {
                        continue;
                    };
                    let class = artifact.retention_class;
                    let outcome =
                        self.delete_artifact(&job.organization, &auditor, artifact, oracle, blobs)?;
                    match outcome {
                        GcOutcome::Deleted { .. } | GcOutcome::AlreadyAbsent { .. } => {
                            *class_counts.entry(class).or_insert(0) += 1;
                        }
                        GcOutcome::RefusedProtected { .. }
                        | GcOutcome::RefusedByBlobStore { .. } => skipped_protected += 1,
                        GcOutcome::ScanUnavailable { .. } | GcOutcome::DeleteFailed { .. } => {
                            // Fail closed but continue: the artifact stays
                            // for the next advance (the deletion is not
                            // silently declared complete).
                        }
                        GcOutcome::SkippedNotEligible { .. }
                        | GcOutcome::SkippedNotExpired { .. }
                        | GcOutcome::SkippedKeepForever { .. } => {}
                    }
                }
                // Billing-retained rows are marked retained by policy.
                for entry in &manifest.retained {
                    if let Some(mut artifact) = self
                        .store
                        .artifact(&entry.id)
                        .map_err(ControlPlaneError::from)?
                    {
                        if artifact.deletion_state != DeletionState::Retained {
                            artifact.deletion_state = DeletionState::Retained;
                            self.store
                                .put_artifact(&artifact)
                                .map_err(ControlPlaneError::from)?;
                        }
                    }
                }
                for (class, count) in &class_counts {
                    let previous = job.deleted_by_class.get(class).copied().unwrap_or(0);
                    if *count > previous {
                        self.append_audit(
                            &job.organization,
                            auditor.clone(),
                            AuditAction::DeletionClassReconciled,
                            "deletion_job",
                            job.id.as_str(),
                            Some(&format!("deleted:{}:{previous}", class.as_str())),
                            Some(&format!("deleted:{}:{count}", class.as_str())),
                            Some(manifest.digest.as_str()),
                        )?;
                    }
                    job.deleted_by_class.insert(*class, *count);
                }
                job.skipped_protected = skipped_protected;
            }
            DeletionStep::Tombstone => {
                if job.tombstone_digest.is_none() {
                    let scope_key = job.scope.key(&job.organization);
                    // Retry-safe: a tombstone durably written for this scope
                    // before a crash (between the write and the job update)
                    // is REUSED byte-for-byte, so a resumed step never forks
                    // the tombstone.
                    let existing = self
                        .store
                        .tombstone(&scope_key)
                        .map_err(ControlPlaneError::from)?
                        .filter(|row| row.organization == job.organization);
                    let digest = match existing {
                        Some(row) => row.digest,
                        None => {
                            let manifest_digest = job
                                .manifest
                                .as_ref()
                                .map(|manifest| manifest.digest.clone());
                            let digest = Tombstone::compute_digest(
                                &scope_key,
                                manifest_digest.as_deref(),
                                now,
                            );
                            let deleted_artifacts: u64 = job.deleted_by_class.values().sum();
                            let retained_artifacts = job
                                .manifest
                                .as_ref()
                                .map(|manifest| manifest.retained.len() as u64)
                                .unwrap_or(0);
                            self.store
                                .put_tombstone(&Tombstone {
                                    scope_key,
                                    organization: job.organization.clone(),
                                    digest: digest.clone(),
                                    manifest_digest,
                                    deleted_artifacts,
                                    retained_artifacts,
                                    created_at_ms: now,
                                })
                                .map_err(ControlPlaneError::from)?;
                            digest
                        }
                    };
                    job.tombstone_digest = Some(digest.clone());
                    job.state = DeletionJobState::Completed;
                    self.append_audit(
                        &job.organization,
                        auditor.clone(),
                        AuditAction::DeletionTombstoned,
                        "deletion_job",
                        job.id.as_str(),
                        None,
                        Some(digest.as_str()),
                        Some(job.scope.key(&job.organization).as_str()),
                    )?;
                }
            }
        }
        if let Some(next) = job.next_step.next() {
            job.next_step = next;
        }
        job.updated_at_ms = now;
        self.store
            .put_deletion_job(&job)
            .map_err(ControlPlaneError::from)?;
        Ok(job)
    }

    /// The scope's artifacts, bounded (a bigger scope is a typed refusal,
    /// never a silent partial manifest).
    fn scope_artifacts(&self, job: &DeletionJob) -> Result<Vec<ArtifactRecord>, ControlPlaneError> {
        let rows = self
            .store
            .artifacts(&job.organization, None, MAX_DELETION_MANIFEST + 1)
            .map_err(ControlPlaneError::from)?;
        if rows.len() > MAX_DELETION_MANIFEST {
            return Err(ControlPlaneError::Conflict(format!(
                "deletion scope exceeds {MAX_DELETION_MANIFEST} artifacts; page the deletion"
            )));
        }
        Ok(match &job.scope {
            DeletionScope::Organization => rows,
            DeletionScope::Account { user } => rows
                .into_iter()
                .filter(|artifact| artifact.owner.as_deref() == Some(user.as_str()))
                .collect(),
        })
    }

    /// One tombstone by scope key (audit read; used by operators verifying a
    /// completed deletion).
    pub fn tombstone(
        &self,
        principal: &Principal,
        scope_key: &str,
    ) -> Result<Option<Tombstone>, ControlPlaneError> {
        let organization = principal.organization.clone();
        authorize(
            principal,
            &organization,
            Resource::Enterprise,
            Action::AuditRead,
        )?;
        let tombstone = self
            .store
            .tombstone(scope_key)
            .map_err(ControlPlaneError::from)?;
        Ok(tombstone.filter(|row| row.organization == organization))
    }

    #[allow(clippy::too_many_arguments)]
    fn append_audit(
        &self,
        organization: &OrganizationId,
        principal: AuditPrincipal,
        action: AuditAction,
        object_kind: &str,
        object: &str,
        before_ref: Option<&str>,
        after_ref: Option<&str>,
        correlation_id: Option<&str>,
    ) -> Result<AuditEvent, ControlPlaneError> {
        let event = NewAuditEvent {
            organization: organization.clone(),
            principal,
            action,
            object_kind: object_kind.to_string(),
            object: object.to_string(),
            timestamp_ms: self.clock.now_ms(),
            before_ref: before_ref.map(str::to_string),
            after_ref: after_ref.map(str::to_string),
            correlation_id: correlation_id.map(str::to_string),
        };
        event.validate()?;
        self.store
            .append_audit_event(&event)
            .map_err(ControlPlaneError::from)
    }
}
