//! The P0 proof-basis surface (mechanically split out of `task/mod.rs` to
//! keep every source file under the shrink-only size ceiling): the exact
//! basis a verification proof is bound to, the layered effective-
//! configuration binding, and the immutable completion-proof binding. The
//! `task` module re-exports every public item, so external paths
//! (`faktor_session::task::...`) are unchanged.

use faktor_core::authority::{
    authority_digest_hex, refuse_legacy_authority_digest, Fields, LegacyAuthorityDigest,
    DOMAIN_CHECK_BASIS,
};
use faktor_core::id::{TaskId, TaskRevision, VerificationRecordId};
use faktor_core::state::{CheckExecution, ToolVersion};

use super::TaskError;

// ---------------------------------------------------- proof basis (P0)

/// Hard bound on the ordered entry lists of one [`ProofBasis`].
pub const MAX_PROOF_BASIS_ENTRIES: usize = 256;

/// One ordered check of the proof basis: the check id plus its typed
/// (program, argv) basis.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProofBasisCheck {
    pub check_id: String,
    pub program: String,
    pub args: Vec<String>,
}

/// One criterion of the proof basis: id plus its binding's content digest
/// (`None` = the criterion carries no binding).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProofBasisCriterion {
    pub criterion_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding_digest: Option<String>,
}

/// The exact basis a verification proof is bound to (P0 proof binding): a
/// record may be REUSED only while every component below is identical.
/// Deliberately one fingerprint system: this feeds
/// [`faktor_core::state::EnvironmentFingerprint::proof_basis_digest`], never a parallel one.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProofBasis {
    pub task_id: u64,
    pub task_revision: u64,
    pub task_contract_digest: String,
    pub candidate_snapshot: String,
    pub integration_sources_digest: String,
    pub changed_files_digest: String,
    pub checks: Vec<ProofBasisCheck>,
    pub verification_impl_version: String,
    pub tool_versions: Vec<ToolVersion>,
    /// The documented verification-relevant environment projection.
    pub env_projection: Vec<(String, String)>,
    pub instruction_epoch: Option<u64>,
    pub criteria: Vec<ProofBasisCriterion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_digest: Option<String>,
    #[serde(default)]
    pub evidence_digests: Vec<String>,
}

/// Monotonic nonce for the (unreachable) proof-basis serialization fault: a
/// poison digest differs on every fault, so a serialization break can never
/// make two different bases compare equal (which would authorize a FALSE
/// proof reuse) — it fails closed instead.
static PROOF_BASIS_SERIALIZE_FAULTS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(1);

impl ProofBasis {
    /// The canonical digest of this basis (`blake3:` prefixed).
    pub fn digest(&self) -> String {
        match serde_json::to_vec(self) {
            Ok(bytes) => format!("blake3:{}", blake3::hash(&bytes).to_hex()),
            Err(err) => {
                // Never `unwrap_or_default()`: defaulting to empty bytes
                // would collapse EVERY basis to one constant digest and
                // silently authorize reuse across different proofs. The flat
                // owned structure cannot fail to encode; if that invariant
                // ever breaks, log loudly and mint a unique NON-HEX poison
                // digest — it can never equal a real digest, so reuse is
                // refused (fail closed), never falsely allowed.
                tracing::error!(
                    error = %err,
                    "proof basis failed to serialize; minting a non-reusable poison digest"
                );
                let nonce =
                    PROOF_BASIS_SERIALIZE_FAULTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                format!("blake3:encode-error-{nonce:016x}")
            }
        }
    }

    /// The reserved `env_projection` key carrying the layered effective
    /// configuration digest of this basis.
    pub const CONFIG_DIGEST_KEY: &'static str = "faktor.config.effective_digest";

    /// The layered effective-configuration digest bound into this basis,
    /// when one was bound. A record's reuse is refused whenever the current
    /// effective configuration (any layer: system/organization/repository/
    /// user/session/task) no longer digests to the same value, because the
    /// binding rides the basis digest the record was written under.
    pub fn config_digest(&self) -> Option<&str> {
        self.env_projection
            .iter()
            .rev()
            .find(|(key, _)| key == Self::CONFIG_DIGEST_KEY)
            .map(|(_, value)| value.as_str())
    }

    /// Bind the layered effective-configuration digest into this basis.
    /// Bounded and strict: an empty/oversized/whitespace-bearing digest is
    /// refused, and a second bind replaces the previous value (one binding
    /// per basis). Any layer change yields a different digest, so the basis
    /// digest — and with it the record's reuse key — changes.
    pub fn bind_config_digest(mut self, digest: &str) -> Result<Self, TaskError> {
        validate_proof_config_digest(digest)?;
        self.env_projection
            .retain(|(key, _)| key != Self::CONFIG_DIGEST_KEY);
        self.env_projection
            .push((Self::CONFIG_DIGEST_KEY.to_string(), digest.to_string()));
        Ok(self)
    }

    /// Fail closed: whether this basis names the layered effective
    /// configuration it was produced under. A basis without a binding can
    /// never certify a completion proof that must be attributable to a
    /// configuration (the caller refuses before writing the record).
    pub fn require_config_digest(&self) -> Result<&str, TaskError> {
        self.config_digest().ok_or_else(|| {
            TaskError::Malformed(
                "proof basis carries no layered effective-configuration digest; a \
                 configuration-attributable proof must bind one (bind_config_digest)"
                    .into(),
            )
        })
    }

    /// The first legacy 64-bit FNV authority digest carried by this basis,
    /// when any: such a basis may be VIEWED but never authorizes a new
    /// record or a proof reuse — the caller refuses typed and forces a
    /// restage/reverification under the canonical BLAKE3 identities.
    pub fn legacy_authority_digest(&self) -> Option<LegacyAuthorityDigest> {
        let members: [(&'static str, &str); 3] = [
            ("task_contract_digest", &self.task_contract_digest),
            (
                "integration_sources_digest",
                &self.integration_sources_digest,
            ),
            ("changed_files_digest", &self.changed_files_digest),
        ];
        for (what, value) in members {
            if let Err(legacy) = refuse_legacy_authority_digest(what, value) {
                return Some(legacy);
            }
        }
        for criterion in &self.criteria {
            if let Some(digest) = &criterion.binding_digest {
                if let Err(legacy) =
                    refuse_legacy_authority_digest("criterion binding_digest", digest)
                {
                    return Some(legacy);
                }
            }
        }
        if let Some(reviewer) = &self.reviewer_digest {
            if let Err(legacy) = refuse_legacy_authority_digest("reviewer_digest", reviewer) {
                return Some(legacy);
            }
        }
        None
    }
}

/// Hard bound on one layered effective-configuration digest string.
pub const MAX_PROOF_CONFIG_DIGEST_BYTES: usize = 256;
/// Hard bound on the ordered layers of one layered configuration binding.
pub const MAX_PROOF_CONFIG_LAYERS: usize = 6;

/// One configuration layer scope, outermost to innermost (the same order the
/// effective configuration is resolved in). The vocabulary is the proof
/// binding's own; the DIGEST of a layer's effective value is supplied by the
/// configuration authority (never re-derived here).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ProofConfigScope {
    System,
    Organization,
    Repository,
    User,
    Session,
    Task,
}

impl ProofConfigScope {
    pub const fn as_str(self) -> &'static str {
        match self {
            ProofConfigScope::System => "system",
            ProofConfigScope::Organization => "organization",
            ProofConfigScope::Repository => "repository",
            ProofConfigScope::User => "user",
            ProofConfigScope::Session => "session",
            ProofConfigScope::Task => "task",
        }
    }

    /// Scope order rank (outermost = 0).
    pub const fn rank(self) -> u8 {
        match self {
            ProofConfigScope::System => 0,
            ProofConfigScope::Organization => 1,
            ProofConfigScope::Repository => 2,
            ProofConfigScope::User => 3,
            ProofConfigScope::Session => 4,
            ProofConfigScope::Task => 5,
        }
    }
}

/// One ordered stamp of the layered effective configuration as bound into a
/// proof basis: the scope, the layer's monotonic revision, and the digest of
/// the layer's effective values. Any of the three changing is a different
/// layer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProofConfigLayer {
    pub scope: ProofConfigScope,
    pub revision: u64,
    pub digest: String,
}

impl ProofConfigLayer {
    /// Build one layer stamp from an opaque effective-value digest supplied
    /// by the configuration authority. The digest is validated (bounded,
    /// printable, non-empty) — never trusted blindly into a durable row.
    pub fn new(
        scope: ProofConfigScope,
        revision: u64,
        digest: impl Into<String>,
    ) -> Result<Self, TaskError> {
        let digest = digest.into();
        validate_proof_config_digest(&digest)?;
        Ok(Self {
            scope,
            revision,
            digest,
        })
    }

    /// Build one layer stamp from a layer VALUE by digesting it under the
    /// proof-config domain. Deterministic: the same scope/revision/value is
    /// the same stamp; changing the value changes the digest.
    pub fn of_value(scope: ProofConfigScope, revision: u64, value: &str) -> Self {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"faktor-proof-config-layer:v1\0");
        hasher.update(scope.as_str().as_bytes());
        hasher.update(&[0]);
        hasher.update(&revision.to_le_bytes());
        hasher.update(&[0]);
        hasher.update(value.as_bytes());
        Self {
            scope,
            revision,
            digest: format!("blake3:{}", hasher.finalize().to_hex()),
        }
    }
}

/// The layered effective-configuration digest: domain-separated over the
/// ordered (outermost-to-innermost) layer stamps, so ANY layer change — a
/// system-layer value, an organization policy revision — yields a different
/// digest and every proof basis bound to the previous digest stops being
/// reusable. The list must be strictly ordered by scope rank (a duplicated
/// or out-of-order layer is refused, never silently sorted).
pub fn layered_effective_config_digest(layers: &[ProofConfigLayer]) -> Result<String, TaskError> {
    if layers.len() > MAX_PROOF_CONFIG_LAYERS {
        return Err(TaskError::Malformed(format!(
            "layered configuration binding carries {} layers (max {MAX_PROOF_CONFIG_LAYERS})",
            layers.len()
        )));
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"faktor-effective-config:v1\0");
    let mut previous: Option<ProofConfigScope> = None;
    for layer in layers {
        if let Some(previous) = previous {
            if layer.scope.rank() <= previous.rank() {
                return Err(TaskError::Malformed(format!(
                    "layered configuration binding is out of order: {} after {} (strict scope order required)",
                    layer.scope.as_str(),
                    previous.as_str()
                )));
            }
        }
        validate_proof_config_digest(&layer.digest)?;
        hasher.update(&[layer.scope.rank()]);
        hasher.update(layer.scope.as_str().as_bytes());
        hasher.update(&[0]);
        hasher.update(&layer.revision.to_le_bytes());
        hasher.update(&[0]);
        hasher.update(layer.digest.as_bytes());
        hasher.update(&[0]);
        previous = Some(layer.scope);
    }
    Ok(format!("blake3:{}", hasher.finalize().to_hex()))
}

/// The canonical BLAKE3 authority digest of one verification record's
/// check-command basis (the fingerprint's `check_argv_cwd_env_hash`): per
/// check the id, the program and every argv element as its own ordered
/// length-prefixed field — never a joined string. The fingerprint schema
/// requires BARE hex text, so the `blake3:` label stays out of this value.
pub(super) fn check_basis_digest(checks: &[CheckExecution]) -> String {
    let mut fields = Fields::new().uint(checks.len() as u64);
    for check in checks {
        fields = fields
            .text(&check.check)
            .text(&check.program)
            .list(&check.args);
    }
    authority_digest_hex(DOMAIN_CHECK_BASIS, 1, fields)
}

fn validate_proof_config_digest(digest: &str) -> Result<(), TaskError> {
    if digest.is_empty() || digest.len() > MAX_PROOF_CONFIG_DIGEST_BYTES {
        return Err(TaskError::Malformed(format!(
            "configuration digest must be 1..={MAX_PROOF_CONFIG_DIGEST_BYTES} bytes"
        )));
    }
    if !digest.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(TaskError::Malformed(
            "configuration digest must be printable ASCII without whitespace".into(),
        ));
    }
    Ok(())
}

/// The result of asking whether an existing proof record may be reused under
/// a current basis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProofReuse {
    /// The record's basis is byte-identical to the current basis.
    Allowed,
    /// Reuse is refused (the reason names the basis mismatch class).
    Refused { reason: String },
}

impl ProofReuse {
    pub fn is_allowed(&self) -> bool {
        matches!(self, ProofReuse::Allowed)
    }
}

/// The immutable binding of a completion proof, returned by
/// [`crate::SessionHandle::verify_completion_proof_binding`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionProofBinding {
    pub record_id: VerificationRecordId,
    pub task_id: TaskId,
    pub revision: TaskRevision,
    pub tree_hash: Option<String>,
    pub criterion_count: usize,
    pub check_count: usize,
}
