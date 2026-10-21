//! Engineering contract IR: constraints, invariants, risk hypotheses and
//! proof obligations with CLAIM-BOUND, SNAPSHOT-BOUND proof state.
//!
//! The contract is the provider-agnostic answer to "what must be true, and
//! what evidence may discharge that claim". Two laws shape this module:
//!
//! 1. **Claim-bound evidence.** A [`ProofObligation`] names the exact thing
//!    that can discharge it ([`EvidenceBinding`]: check ids, test selectors,
//!    contract ids, property ids, runtime assertions, trace properties, a
//!    verification-provider claim id). Evidence whose binding does not
//!    intersect the claim's binding is structurally incapable of discharging
//!    — or disproving — it. The only exception is the explicit legacy
//!    [`EvidenceBinding::Category`], kept for rows written before bindings
//!    existed and for callers that can only state a claim and an evidence
//!    kind; the contract compiler must never emit it.
//!
//! 2. **Snapshot-bound proof.** Proof state is
//!    `(claim, subject_snapshot, proof_basis) -> state`, never a timeless
//!    `claim -> Supported`. Every [`ProofRecord`] names the candidate
//!    snapshot it was established against and the basis on which it may be
//!    reused ([`ProofBasis::survives`]); a Supported claim whose basis does
//!    not demonstrably survive into the current candidate degrades to
//!    [`ClaimState::Unknown`] until reproof.
//!
//! Each obligation carries a [`Requirement`] (`Required` claims gate
//! completion; `Advisory` uncertainty is disclosure-only) and an
//! [`AcceptedStrength`] (the minimum state that counts as discharged).
//! [`ClaimState`] remains the six-state truth taxonomy shared with the wire.

use serde::{Deserialize, Serialize};

/// Hard bound on the entries of one [`EvidenceBinding`] target list.
pub const MAX_EVIDENCE_BINDING_TARGETS: usize = 64;
/// Hard bound on ONE [`EvidenceBinding`] target string (UTF-8 bytes).
pub const MAX_EVIDENCE_BINDING_TARGET_BYTES: usize = 512;
/// Hard bound on the proof records one obligation may carry (oldest records
/// are dropped when a fresh proof is recorded).
pub const MAX_PROOF_RECORDS: usize = 8;
/// Hard bound on the proven binding updates one obligation may carry.
pub const MAX_BINDING_UPDATES: usize = 8;
/// Hard bound on one binding-update basis string (UTF-8 bytes).
pub const MAX_BINDING_UPDATE_BASIS_BYTES: usize = 512;

// ------------------------------------------------------------------ claims

/// The truth state of one claim, in the frozen six-state taxonomy.
///
/// Ordering is meaningful through [`ClaimState::rank`]: `KnownTrue` is the
/// strongest, `Unknown` the honest zero, and `Disproven` is outside the
/// scale (an established FALSE can never satisfy any accepted strength).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClaimState {
    /// Proof-grade truth from an authority (external provider with a
    /// verifiable artifact; never minted by native category evidence).
    KnownTrue,
    /// Supported by the attempt's own evidence (native checks, review).
    Supported,
    /// A plausible inference with partial support.
    Inferred,
    /// Evidence is contradictory or too weak to decide.
    Uncertain,
    /// No evidence (the honest default).
    #[default]
    Unknown,
    /// An established falsehood (a counterexample or failed check).
    Disproven,
}

impl ClaimState {
    /// The stable wire name (identical to the serde name).
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::KnownTrue => "known_true",
            Self::Supported => "supported",
            Self::Inferred => "inferred",
            Self::Uncertain => "uncertain",
            Self::Unknown => "unknown",
            Self::Disproven => "disproven",
        }
    }

    /// The strength rank (`KnownTrue` strongest, `Unknown` = 0, `Disproven`
    /// = -1: below every accepted strength).
    pub const fn rank(self) -> i8 {
        match self {
            Self::KnownTrue => 4,
            Self::Supported => 3,
            Self::Inferred => 2,
            Self::Uncertain => 1,
            Self::Unknown => 0,
            Self::Disproven => -1,
        }
    }

    /// TRUE only for an established falsehood.
    pub const fn is_disproven(self) -> bool {
        matches!(self, Self::Disproven)
    }

    /// TRUE for claim-grade states: these are the states that must never be
    /// carried across a changed snapshot without reproof.
    pub const fn is_claim_grade(self) -> bool {
        matches!(self, Self::KnownTrue | Self::Supported)
    }
}

// ------------------------------------------------------------- requirement

/// What an obligation's failure means to completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Requirement {
    /// The claim gates completion: while it is unresolved or disproven a
    /// passing gate cannot stand (typed NeedsVerification / block).
    #[default]
    Required,
    /// The claim is disclosed but never blocks completion by its own
    /// uncertainty (a disproof of an advisory claim is still reported).
    Advisory,
}

impl Requirement {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Required => "required",
            Self::Advisory => "advisory",
        }
    }
}

/// The minimum [`ClaimState`] that counts as discharged for one obligation.
/// `Uncertain`/`Unknown` can never be an accepted strength.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptedStrength {
    /// Only proof-grade truth (external verified claims).
    KnownTrue,
    /// Supported native evidence (the default).
    #[default]
    Supported,
    /// Partial support suffices (weaker advisory bars).
    Inferred,
}

impl AcceptedStrength {
    /// The minimum acceptable state.
    pub const fn min_state(self) -> ClaimState {
        match self {
            Self::KnownTrue => ClaimState::KnownTrue,
            Self::Supported => ClaimState::Supported,
            Self::Inferred => ClaimState::Inferred,
        }
    }

    /// The default accepted strength of one requirement: a `Required` claim
    /// needs Supported evidence, an `Advisory` claim accepts Inferred.
    pub const fn for_requirement(requirement: Requirement) -> Self {
        match requirement {
            Requirement::Required => Self::Supported,
            Requirement::Advisory => Self::Inferred,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::KnownTrue => "known_true",
            Self::Supported => "supported",
            Self::Inferred => "inferred",
        }
    }
}

// ------------------------------------------------------- evidence bindings

/// What can discharge one claim — the structural identity of acceptable
/// evidence. Evidence only ever discharges a claim via an intersecting
/// binding (see [`EvidenceBinding::intersects`]); an unrelated check id,
/// test selector or provider claim is incapable of discharging or
/// disproving the claim.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceBinding {
    /// LEGACY: any evidence of the obligation's evidence kind. Pre-binding
    /// rows decode here; the contract compiler must emit an explicit
    /// binding instead.
    #[default]
    Category,
    /// Stable check ids (e.g. `cargo-check`, `cargo-test`).
    CheckIds(Vec<String>),
    /// Exactly one test selector (`auth::refresh::replay_is_rejected`).
    TestSelector(String),
    /// A set of test selectors a suite-level check exercised.
    TestSelectors(Vec<String>),
    /// Contract ids a contract-check evidence evaluated.
    ContractIds(Vec<String>),
    /// Property ids a property check exercised.
    PropertyIds(Vec<String>),
    /// Named runtime assertions an evidence run observed.
    RuntimeAssertions(Vec<String>),
    /// Trace properties a runtime-trace evidence evaluated.
    TraceProperties(Vec<String>),
    /// The claim id of a verification-provider response.
    ProviderClaim(String),
}

impl EvidenceBinding {
    /// TRUE when the binding constrains what can discharge the claim (every
    /// binding except the legacy [`EvidenceBinding::Category`]).
    pub const fn is_bound(&self) -> bool {
        !matches!(self, Self::Category)
    }

    /// The binding's wire class name (for disclosure and diagnostics).
    pub const fn class(&self) -> &'static str {
        match self {
            Self::Category => "category",
            Self::CheckIds(_) => "check_ids",
            Self::TestSelector(_) => "test_selector",
            Self::TestSelectors(_) => "test_selectors",
            Self::ContractIds(_) => "contract_ids",
            Self::PropertyIds(_) => "property_ids",
            Self::RuntimeAssertions(_) => "runtime_assertions",
            Self::TraceProperties(_) => "trace_properties",
            Self::ProviderClaim(_) => "provider_claim",
        }
    }

    /// TRUE when one CLAIM binding and one EVIDENCE binding identify the
    /// same thing. Bindings of different classes never intersect; the legacy
    /// `Category` binding never intersects a specific one (the category
    /// fallback is decided by the evaluator, not here).
    pub fn intersects(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Category, Self::Category) => true,
            (Self::Category, _) | (_, Self::Category) => false,
            (Self::CheckIds(a), Self::CheckIds(b)) => sets_intersect(a, b),
            (Self::TestSelector(a), Self::TestSelector(b)) => a == b,
            (Self::TestSelector(a), Self::TestSelectors(b))
            | (Self::TestSelectors(b), Self::TestSelector(a)) => b.iter().any(|s| s == a),
            (Self::TestSelectors(a), Self::TestSelectors(b)) => sets_intersect(a, b),
            (Self::ContractIds(a), Self::ContractIds(b)) => sets_intersect(a, b),
            (Self::PropertyIds(a), Self::PropertyIds(b)) => sets_intersect(a, b),
            (Self::RuntimeAssertions(a), Self::RuntimeAssertions(b)) => sets_intersect(a, b),
            (Self::TraceProperties(a), Self::TraceProperties(b)) => sets_intersect(a, b),
            (Self::ProviderClaim(a), Self::ProviderClaim(b)) => a == b,
            _ => false,
        }
    }

    /// Structural validation: bounded, non-empty targets for every bound
    /// variant. Every violation is a typed reason string.
    pub fn validate(&self) -> Result<(), String> {
        let check = |what: &str, targets: &[String]| -> Result<(), String> {
            if targets.len() > MAX_EVIDENCE_BINDING_TARGETS {
                return Err(format!(
                    "{what}: {} targets exceed MAX_EVIDENCE_BINDING_TARGETS ({MAX_EVIDENCE_BINDING_TARGETS})",
                    targets.len()
                ));
            }
            for target in targets {
                if target.is_empty() {
                    return Err(format!("{what}: a target is empty"));
                }
                if target.len() > MAX_EVIDENCE_BINDING_TARGET_BYTES {
                    return Err(format!(
                        "{what}: a target of {} bytes exceeds MAX_EVIDENCE_BINDING_TARGET_BYTES ({MAX_EVIDENCE_BINDING_TARGET_BYTES})",
                        target.len()
                    ));
                }
            }
            Ok(())
        };
        match self {
            Self::Category => Ok(()),
            Self::CheckIds(t) => check("check_ids", t),
            Self::TestSelector(s) => check("test_selector", std::slice::from_ref(s)),
            Self::TestSelectors(t) => check("test_selectors", t),
            Self::ContractIds(t) => check("contract_ids", t),
            Self::PropertyIds(t) => check("property_ids", t),
            Self::RuntimeAssertions(t) => check("runtime_assertions", t),
            Self::TraceProperties(t) => check("trace_properties", t),
            Self::ProviderClaim(s) => check("provider_claim", std::slice::from_ref(s)),
        }
    }

    /// The first target of a bound binding (the value a [`ProofEvidenceRef`]
    /// records as the selector actually exercised).
    pub fn selector(&self) -> Option<&str> {
        match self {
            Self::Category => None,
            Self::CheckIds(t)
            | Self::TestSelectors(t)
            | Self::ContractIds(t)
            | Self::PropertyIds(t)
            | Self::RuntimeAssertions(t)
            | Self::TraceProperties(t) => t.first().map(String::as_str),
            Self::TestSelector(s) | Self::ProviderClaim(s) => Some(s.as_str()),
        }
    }
}

fn sets_intersect(a: &[String], b: &[String]) -> bool {
    !a.is_empty() && !b.is_empty() && a.iter().any(|x| b.iter().any(|y| x == y))
}

/// One PROVEN binding update recorded on a proof obligation (the
/// proof/evidence ledger's provenance row): the old binding, the new binding
/// and the deterministic basis that authorized the change. A binding update
/// is never a hint: it is written only when a real authority proved the
/// old→new rename edge (git rename detection / impact or LSP symbol
/// relationship / verification inventory) and the match was unique.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindingUpdate {
    pub old_binding: EvidenceBinding,
    pub new_binding: EvidenceBinding,
    /// Bounded machine-readable basis (`git_rename:...`,
    /// `same_target:...`, `inventory_rename:...`).
    pub basis: String,
}

// -------------------------------------------------------- evidence & origins

/// Which family of evidence may discharge an obligation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofEvidenceKind {
    Compile,
    TestExecution,
    ContractCheck,
    Review,
    RuntimeTrace,
}

impl ProofEvidenceKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Compile => "compile",
            Self::TestExecution => "test_execution",
            Self::ContractCheck => "contract_check",
            Self::Review => "review",
            Self::RuntimeTrace => "runtime_trace",
        }
    }
}

/// Where a contract entry came from.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContractOrigin {
    /// A user-authored entry.
    User,
    /// Derived from policy.
    Policy,
    /// Derived natively by the runtime.
    Native,
    /// A heuristic guess (risk hypotheses start here).
    Heuristic,
    /// An external provider's claim, named.
    External { provider: String },
}

/// One evidence reference recorded on an obligation (wire-stable shape; the
/// snapshot-bound richness lives in [`ProofEvidenceRef`] inside the
/// [`ProofRecord`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceRef {
    pub kind: ProofEvidenceKind,
    pub source: String,
    pub detail: String,
}

// ------------------------------------------------------ snapshot-bound proof

/// Identity of the candidate subject a proof was established against: the
/// content identity of the candidate tree/snapshot plus (when recorded) the
/// environment basis the evidence was produced under.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SubjectSnapshot {
    /// Content identity of the candidate tree/snapshot (e.g. a `tm1:...`
    /// tree digest or a candidate manifest digest). Never empty.
    pub candidate: String,
    /// Environment basis (toolchain/config projection digest) the evidence
    /// was produced under, when one was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
}

impl SubjectSnapshot {
    pub fn new(candidate: impl Into<String>) -> Self {
        Self {
            candidate: candidate.into(),
            environment: None,
        }
    }

    pub fn with_environment(mut self, environment: impl Into<String>) -> Self {
        self.environment = Some(environment.into());
        self
    }
}

/// On what a proof's reuse across candidate changes depends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProofScope {
    /// Valid only for the exact candidate snapshot it was established on.
    #[default]
    SnapshotBound,
    /// Declared independent of the candidate tree: reusable when the
    /// recorded environment basis demonstrably survives (both sides must
    /// carry a recorded, identical environment).
    SnapshotIndependent,
}

/// The declared basis of one proof: the subject it was established against
/// and what its validity depends on. Reuse is decided mechanically by
/// [`ProofBasis::survives`], never assumed.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProofBasis {
    /// The candidate snapshot this proof was established on. `None` = the
    /// snapshot was never recorded: the proof can never be reused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<SubjectSnapshot>,
    #[serde(default)]
    pub scope: ProofScope,
}

impl ProofBasis {
    pub fn snapshot_bound(subject: SubjectSnapshot) -> Self {
        Self {
            subject: Some(subject),
            scope: ProofScope::SnapshotBound,
        }
    }

    pub fn snapshot_independent(subject: SubjectSnapshot) -> Self {
        Self {
            subject: Some(subject),
            scope: ProofScope::SnapshotIndependent,
        }
    }

    /// Whether this basis demonstrably survives into `current` (the current
    /// candidate snapshot, when known). Only [`ProofSurvival::Survives`]
    /// authorizes reuse of the proof's state.
    pub fn survives(&self, current: Option<&SubjectSnapshot>) -> ProofSurvival {
        match self.scope {
            ProofScope::SnapshotBound => {
                let Some(subject) = &self.subject else {
                    return ProofSurvival::Stale(ProofStaleness::SnapshotUnknown);
                };
                let Some(current) = current else {
                    return ProofSurvival::Stale(ProofStaleness::SnapshotUnknown);
                };
                if subject.candidate != current.candidate {
                    return ProofSurvival::Stale(ProofStaleness::SnapshotChanged);
                }
                if subject.environment.is_some() && subject.environment != current.environment {
                    return ProofSurvival::Stale(ProofStaleness::EnvironmentChanged);
                }
                ProofSurvival::Survives
            }
            ProofScope::SnapshotIndependent => {
                let Some(subject) = &self.subject else {
                    return ProofSurvival::Stale(ProofStaleness::EnvironmentUnknown);
                };
                let Some(subject_env) = subject.environment.as_deref() else {
                    return ProofSurvival::Stale(ProofStaleness::EnvironmentUnknown);
                };
                let Some(current) = current else {
                    return ProofSurvival::Stale(ProofStaleness::EnvironmentUnknown);
                };
                match current.environment.as_deref() {
                    None => ProofSurvival::Stale(ProofStaleness::EnvironmentUnknown),
                    Some(current_env) if current_env == subject_env => ProofSurvival::Survives,
                    Some(_) => ProofSurvival::Stale(ProofStaleness::EnvironmentChanged),
                }
            }
        }
    }
}

/// The mechanical verdict of [`ProofBasis::survives`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProofSurvival {
    Survives,
    Stale(ProofStaleness),
}

impl ProofSurvival {
    pub const fn survives(self) -> bool {
        matches!(self, Self::Survives)
    }
}

/// Why a prior proof does not survive into the current candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProofStaleness {
    /// The prior proof names no snapshot, or the current one is unknown.
    SnapshotUnknown,
    /// The candidate snapshot changed since the proof was established.
    SnapshotChanged,
    /// The environment basis is absent on an independent proof or the
    /// current side.
    EnvironmentUnknown,
    /// The environment basis changed since the proof was established.
    EnvironmentChanged,
}

impl ProofStaleness {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SnapshotUnknown => "snapshot_unknown",
            Self::SnapshotChanged => "snapshot_changed",
            Self::EnvironmentUnknown => "environment_unknown",
            Self::EnvironmentChanged => "environment_changed",
        }
    }
}

/// The rich, snapshot-bound evidence reference of one proof: candidate
/// tree/snapshot identity, verification op, the selector/binding actually
/// exercised, provider id + schema version, the evidence artifact digest and
/// the environment basis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofEvidenceRef {
    pub kind: ProofEvidenceKind,
    /// Candidate tree/snapshot identity the evidence was produced against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    /// The verification op that produced the evidence (wire op name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<String>,
    /// The selector/binding value actually exercised.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    /// The structural binding class that matched.
    #[serde(default)]
    pub binding: EvidenceBinding,
    /// Verification provider id (absent for native evidence).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// The provider's wire schema version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_schema_version: Option<u32>,
    /// Digest of the evidence artifact (never the artifact itself).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_digest: Option<String>,
    /// The environment basis the evidence was produced under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    /// Bounded human-readable detail.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
}

/// One recorded proof of a claim against one subject snapshot and basis.
/// `(claim, subject_snapshot, proof_basis) -> state`: the state is never a
/// timeless property of the claim alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofRecord {
    /// The claim this record proves (a record can never be re-pointed at
    /// another claim).
    pub claim: String,
    pub state: ClaimState,
    pub confidence_bps: u16,
    pub basis: ProofBasis,
    #[serde(default)]
    pub evidence: Vec<EvidenceRef>,
    /// The rich reference of the evidence the proof rests on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<ProofEvidenceRef>,
    /// The observed counterexample when the record is a disproof.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counterexample: Option<String>,
}

// --------------------------------------------------------- proof obligation

/// One claim-bound, snapshot-bound proof obligation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofObligation {
    /// The deterministic obligation id (`po1:` + blake3 hex).
    pub id: String,
    /// The claim under proof.
    pub claim: String,
    /// Whether the claim gates completion (`Required`) or is disclosure-only
    /// on uncertainty (`Advisory`).
    #[serde(default)]
    pub requirement: Requirement,
    /// The evidence family that may discharge the claim.
    #[serde(rename = "evidence")]
    pub evidence: ProofEvidenceKind,
    /// Exactly what can discharge the claim (see [`EvidenceBinding`]).
    #[serde(default)]
    pub evidence_binding: EvidenceBinding,
    /// The minimum state that counts as discharged.
    #[serde(default)]
    pub accepted_strength: AcceptedStrength,
    /// The CURRENT state of the claim (at the latest evaluated candidate).
    pub state: ClaimState,
    /// Provenance of the claim.
    pub origin: ContractOrigin,
    /// Confidence in `state` (0..=10_000 bps; `None` when never evaluated).
    pub confidence_bps: Option<u16>,
    /// Wire-compatible evidence references accumulated for this claim.
    #[serde(default)]
    pub evidence_refs: Vec<EvidenceRef>,
    /// Snapshot-bound proof records (bounded, newest last). The obligation's
    /// `state` is only reusable across candidates when one of these records'
    /// bases demonstrably survives.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub proofs: Vec<ProofRecord>,
    /// Proven binding updates (old→new + basis). Append-only provenance:
    /// a rebind is only recorded after an authority proved the mapping.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub binding_updates: Vec<BindingUpdate>,
}

impl ProofObligation {
    /// TRUE when the claim is established false.
    pub fn is_disproven(&self) -> bool {
        self.state.is_disproven()
    }

    /// TRUE when the current state meets the accepted strength (and is not a
    /// disproof).
    pub fn meets_accepted_strength(&self) -> bool {
        !self.state.is_disproven() && self.state.rank() >= self.accepted_strength.min_state().rank()
    }

    /// TRUE when the claim is neither discharged nor disproven.
    pub fn is_unresolved(&self) -> bool {
        !self.is_disproven() && !self.meets_accepted_strength()
    }

    /// TRUE when a `Required` claim is unresolved: completion must not stand
    /// silently (typed NeedsVerification).
    pub fn is_unresolved_required(&self) -> bool {
        self.requirement == Requirement::Required && self.is_unresolved()
    }

    /// TRUE when an `Advisory` claim is merely uncertain: disclosure-only.
    pub fn is_advisory_uncertain(&self) -> bool {
        self.requirement == Requirement::Advisory && self.is_unresolved()
    }

    /// The newest recorded proof whose basis demonstrably survives into
    /// `current`.
    pub fn surviving_proof(&self, current: Option<&SubjectSnapshot>) -> Option<&ProofRecord> {
        self.proofs
            .iter()
            .rev()
            .find(|record| record.basis.survives(current).survives())
    }

    /// The claim state valid at `current`: a claim-grade state whose proof
    /// basis does not demonstrably survive degrades to [`ClaimState::Unknown`]
    /// (never carry Supported across a changed snapshot without reproof).
    pub fn state_for_candidate(&self, current: Option<&SubjectSnapshot>) -> ClaimState {
        if self.state.is_claim_grade() && self.surviving_proof(current).is_none() {
            ClaimState::Unknown
        } else {
            self.state
        }
    }

    /// Record a fresh proof, replacing any existing record with the same
    /// (claim, subject candidate, scope) identity and keeping the list
    /// bounded (oldest dropped).
    pub fn record_proof(&mut self, record: ProofRecord) {
        let same = |existing: &ProofRecord| {
            existing.claim == record.claim
                && existing.basis.scope == record.basis.scope
                && existing
                    .basis
                    .subject
                    .as_ref()
                    .map(|s| s.candidate.as_str())
                    == record.basis.subject.as_ref().map(|s| s.candidate.as_str())
        };
        self.proofs.retain(|existing| !same(existing));
        self.proofs.push(record);
        if self.proofs.len() > MAX_PROOF_RECORDS {
            let excess = self.proofs.len() - MAX_PROOF_RECORDS;
            self.proofs.drain(..excess);
        }
    }

    /// Rebind this obligation to a PROVEN new evidence binding (a renamed
    /// test selector). The update is appended to the provenance ledger; the
    /// prior evaluation state and proof records are INVALIDATED (they were
    /// established against the old binding and must never silently satisfy
    /// the new one). The claim text is re-pointed at the new selector when it
    /// names the old one. Returns `false` when the binding is unchanged.
    pub fn rebind_evidence_binding(&mut self, new_binding: EvidenceBinding, basis: String) -> bool {
        if self.evidence_binding == new_binding {
            return false;
        }
        let old_binding = self.evidence_binding.clone();
        if let (Some(old), Some(new)) = (
            old_binding.selector().map(str::to_string),
            new_binding.selector().map(str::to_string),
        ) {
            if old != new {
                self.claim = self.claim.replace(&old, &new);
            }
        }
        let mut basis = basis;
        while basis.len() > MAX_BINDING_UPDATE_BASIS_BYTES {
            basis.pop();
        }
        self.binding_updates.push(BindingUpdate {
            old_binding,
            new_binding: new_binding.clone(),
            basis,
        });
        if self.binding_updates.len() > MAX_BINDING_UPDATES {
            let excess = self.binding_updates.len() - MAX_BINDING_UPDATES;
            self.binding_updates.drain(..excess);
        }
        self.evidence_binding = new_binding;
        self.state = ClaimState::Unknown;
        self.confidence_bps = None;
        self.evidence_refs.clear();
        self.proofs.clear();
        true
    }

    /// The newest record's staleness reason when the obligation carries a
    /// claim-grade state whose basis does not survive; `None` when the state
    /// is reusable, has no claim-grade state, or carries no records (an
    /// unrecorded claim-grade state is degraded by [`Self::state_for_candidate`]
    /// and its staleness is [`ProofStaleness::SnapshotUnknown`]).
    pub fn staleness(&self, current: Option<&SubjectSnapshot>) -> Option<ProofStaleness> {
        if !self.state.is_claim_grade() {
            return None;
        }
        if self.proofs.is_empty() {
            return Some(ProofStaleness::SnapshotUnknown);
        }
        let mut first_stale = None;
        for record in self.proofs.iter().rev() {
            match record.basis.survives(current) {
                ProofSurvival::Survives => return None,
                ProofSurvival::Stale(reason) => {
                    if first_stale.is_none() {
                        first_stale = Some(reason);
                    }
                }
            }
        }
        first_stale.or(Some(ProofStaleness::SnapshotUnknown))
    }

    /// A copy of this obligation with all evaluated state stripped (the
    /// submission form: Faktor asks a question, never prescribes the
    /// answer). Binding, requirement and accepted strength ride along — they
    /// are the question's shape, not a prior result.
    pub fn without_evaluation_state(&self) -> Self {
        Self {
            id: self.id.clone(),
            claim: self.claim.clone(),
            requirement: self.requirement,
            evidence: self.evidence,
            evidence_binding: self.evidence_binding.clone(),
            accepted_strength: self.accepted_strength,
            state: ClaimState::Unknown,
            origin: self.origin.clone(),
            confidence_bps: None,
            evidence_refs: Vec::new(),
            proofs: Vec::new(),
            binding_updates: self.binding_updates.clone(),
        }
    }
}

// ------------------------------------------------------------------ contract

/// One constraint: a statement that must not be violated. Constraints with
/// bound evaluation evidence gate completion (a violated constraint refuses
/// it; an unevaluated hard constraint forces the check).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Constraint {
    pub id: String,
    pub statement: String,
    pub origin: ContractOrigin,
}

/// One invariant: a statement that must remain true (preservation). Each
/// invariant produces a preservation check in the verification plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Invariant {
    pub id: String,
    pub statement: String,
    pub origin: ContractOrigin,
}

/// How dangerous a risk hypothesis is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskSeverity {
    Low,
    Medium,
    High,
    /// No trustworthy evidence about the risk: escalates exactly like High
    /// (Unknown is never Safe).
    Unknown,
}

impl RiskSeverity {
    pub const fn severity_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Unknown => "unknown",
        }
    }

    /// TRUE for the severities that escalate verification (High and the
    /// untrusted Unknown).
    pub const fn escalates(self) -> bool {
        matches!(self, Self::High | Self::Unknown)
    }

    /// The verification depth this severity raises to.
    pub const fn verification_depth(self) -> VerificationDepth {
        if self.escalates() {
            VerificationDepth::Deep
        } else {
            VerificationDepth::Standard
        }
    }
}

/// How deep verification must go for a contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VerificationDepth {
    Standard,
    Deep,
}

/// One risk hypothesis (an assumption to be invalidated by counterexamples).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RiskHypothesis {
    pub id: String,
    pub description: String,
    pub severity: RiskSeverity,
    pub origin: ContractOrigin,
}

/// One disclosure item of the contract projection: a claim the user must
/// see, never a silent pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractDisclosure {
    pub id: String,
    pub summary: String,
    pub state: String,
    /// The requirement that made this item disclosure-worthy, when it is an
    /// obligation disclosure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requirement: Option<Requirement>,
}

/// The durable engineering contract of one task.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineeringContract {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub constraints: Vec<Constraint>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub invariants: Vec<Invariant>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub risks: Vec<RiskHypothesis>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub proof_obligations: Vec<ProofObligation>,
}

impl EngineeringContract {
    /// TRUE when the contract carries no entries.
    pub fn is_empty(&self) -> bool {
        self.constraints.is_empty()
            && self.invariants.is_empty()
            && self.risks.is_empty()
            && self.proof_obligations.is_empty()
    }

    /// The deterministic id of one proof obligation: domain-separated BLAKE3
    /// over the claim and the evidence kind, `po1:` prefixed (64 hex chars).
    pub fn obligation_id(claim: &str, evidence: ProofEvidenceKind) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"faktor.contract.po1.v1\0");
        hasher.update(claim.as_bytes());
        hasher.update(b"\0");
        hasher.update(evidence.as_str().as_bytes());
        format!("po1:{}", hasher.finalize().to_hex())
    }

    /// One obligation with the LEGACY category-wide binding, `Required`
    /// requirement and `Supported` accepted strength. Prefer
    /// [`Self::obligation_bound`]; this constructor exists for callers that
    /// can only state a claim and an evidence kind.
    pub fn obligation(
        claim: &str,
        evidence: ProofEvidenceKind,
        origin: ContractOrigin,
    ) -> ProofObligation {
        Self::obligation_bound(
            claim,
            evidence,
            origin,
            Requirement::Required,
            EvidenceBinding::Category,
            AcceptedStrength::Supported,
        )
    }

    /// One claim-bound obligation: the compiler's constructor.
    pub fn obligation_bound(
        claim: &str,
        evidence: ProofEvidenceKind,
        origin: ContractOrigin,
        requirement: Requirement,
        binding: EvidenceBinding,
        accepted_strength: AcceptedStrength,
    ) -> ProofObligation {
        ProofObligation {
            id: Self::obligation_id(claim, evidence),
            claim: claim.to_string(),
            requirement,
            evidence,
            evidence_binding: binding,
            accepted_strength,
            state: ClaimState::Unknown,
            origin,
            confidence_bps: None,
            evidence_refs: Vec::new(),
            proofs: Vec::new(),
            binding_updates: Vec::new(),
        }
    }

    /// The highest risk severity carried by any hypothesis (Low when there
    /// are none).
    pub fn max_risk(&self) -> RiskSeverity {
        let mut out = RiskSeverity::Low;
        for risk in &self.risks {
            if risk.severity.escalates() {
                return RiskSeverity::High;
            }
            if out == RiskSeverity::Low && risk.severity == RiskSeverity::Medium {
                out = RiskSeverity::Medium;
            }
        }
        out
    }

    /// TRUE when any risk hypothesis escalates (High or Unknown).
    pub fn has_elevated_risk(&self) -> bool {
        self.risks.iter().any(|risk| risk.severity.escalates())
    }

    /// The verification depth this contract demands.
    pub fn verification_depth(&self) -> VerificationDepth {
        if self.has_elevated_risk() {
            VerificationDepth::Deep
        } else {
            VerificationDepth::Standard
        }
    }

    /// Required obligations unresolved at `candidate` (completion must not
    /// stand on these).
    pub fn unresolved_required(
        &self,
        candidate: Option<&SubjectSnapshot>,
    ) -> Vec<&ProofObligation> {
        self.proof_obligations
            .iter()
            .filter(|obligation| {
                obligation.requirement == Requirement::Required
                    && !obligation.state_for_candidate(candidate).is_disproven()
                    && !candidate_discharged(obligation, candidate)
            })
            .collect()
    }

    /// Advisory obligations uncertain at `candidate` (disclosure-only).
    pub fn advisory_uncertain(&self, candidate: Option<&SubjectSnapshot>) -> Vec<&ProofObligation> {
        self.proof_obligations
            .iter()
            .filter(|obligation| {
                obligation.requirement == Requirement::Advisory
                    && !obligation.state_for_candidate(candidate).is_disproven()
                    && !candidate_discharged(obligation, candidate)
            })
            .collect()
    }

    /// Obligations established false at `candidate` (a disproof is a
    /// disproof regardless of requirement).
    pub fn disproven(&self, candidate: Option<&SubjectSnapshot>) -> Vec<&ProofObligation> {
        self.proof_obligations
            .iter()
            .filter(|obligation| obligation.state_for_candidate(candidate).is_disproven())
            .collect()
    }
}

fn candidate_discharged(obligation: &ProofObligation, candidate: Option<&SubjectSnapshot>) -> bool {
    let state = obligation.state_for_candidate(candidate);
    !state.is_disproven() && state.rank() >= obligation.accepted_strength.min_state().rank()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_binding(binding: EvidenceBinding) -> ProofObligation {
        EngineeringContract::obligation_bound(
            "the replay claim holds",
            ProofEvidenceKind::TestExecution,
            ContractOrigin::User,
            Requirement::Required,
            binding,
            AcceptedStrength::Supported,
        )
    }

    #[test]
    fn obligation_id_is_deterministic_and_distinct_per_evidence_kind() {
        let a = EngineeringContract::obligation_id("claim", ProofEvidenceKind::Compile);
        let b = EngineeringContract::obligation_id("claim", ProofEvidenceKind::Compile);
        let c = EngineeringContract::obligation_id("claim", ProofEvidenceKind::TestExecution);
        let d = EngineeringContract::obligation_id("other", ProofEvidenceKind::Compile);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, d);
        assert!(a.starts_with("po1:"));
        assert_eq!(a.len(), 4 + 64);
        assert!(a[4..].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn evidence_bindings_only_intersect_their_own_identity() {
        let replay = EvidenceBinding::TestSelector("auth::refresh::replay_is_rejected".into());
        let other = EvidenceBinding::TestSelector("auth::refresh::happy_path".into());
        assert!(replay.intersects(&replay.clone()));
        assert!(!replay.intersects(&other));
        assert!(EvidenceBinding::TestSelectors(vec![
            "auth::refresh::replay_is_rejected".into(),
            "x".into()
        ])
        .intersects(&replay));
        assert!(EvidenceBinding::CheckIds(vec!["cargo-test".into()])
            .intersects(&EvidenceBinding::CheckIds(vec!["cargo-test".into()])));
        assert!(!EvidenceBinding::CheckIds(vec!["cargo-test".into()])
            .intersects(&EvidenceBinding::CheckIds(vec!["cargo-clippy".into()])));
        // Different classes never intersect, and Category only matches itself.
        assert!(!replay.intersects(&EvidenceBinding::CheckIds(vec![
            "auth::refresh::replay_is_rejected".into()
        ])));
        assert!(!EvidenceBinding::Category.intersects(&replay));
        assert!(EvidenceBinding::Category.intersects(&EvidenceBinding::Category));
        // Hostile bindings are refused with typed reasons.
        assert!(EvidenceBinding::CheckIds(Vec::new()).validate().is_ok());
        assert!(EvidenceBinding::CheckIds(vec![String::new()])
            .validate()
            .is_err());
        assert!(EvidenceBinding::CheckIds(
            (0..=MAX_EVIDENCE_BINDING_TARGETS)
                .map(|i| format!("c{i}"))
                .collect()
        )
        .validate()
        .is_err());
        assert!(
            EvidenceBinding::TestSelector("x".repeat(MAX_EVIDENCE_BINDING_TARGET_BYTES + 1))
                .validate()
                .is_err()
        );
    }

    #[test]
    fn required_and_advisory_strength_semantics_are_typed() {
        let mut required = user_binding(EvidenceBinding::Category);
        assert!(required.is_unresolved_required());
        assert!(!required.meets_accepted_strength());
        required.state = ClaimState::Inferred;
        assert!(required.is_unresolved_required(), "Inferred < Supported");
        required.state = ClaimState::Supported;
        assert!(!required.is_unresolved_required());
        assert!(required.meets_accepted_strength());
        required.state = ClaimState::Disproven;
        assert!(required.is_disproven());
        assert!(!required.meets_accepted_strength());
        assert!(!required.is_unresolved_required());

        let mut advisory = user_binding(EvidenceBinding::Category);
        advisory.requirement = Requirement::Advisory;
        advisory.accepted_strength = AcceptedStrength::for_requirement(Requirement::Advisory);
        advisory.state = ClaimState::Uncertain;
        assert!(advisory.is_advisory_uncertain());
        assert!(!advisory.is_unresolved_required());
        assert_eq!(advisory.accepted_strength.min_state(), ClaimState::Inferred);
        advisory.state = ClaimState::Inferred;
        assert!(!advisory.is_advisory_uncertain());
    }

    #[test]
    fn snapshot_change_stales_prior_support_unless_the_basis_survives() {
        let snap_a = SubjectSnapshot::new("tm1:aaaa");
        let snap_b = SubjectSnapshot::new("tm1:bbbb");

        // Snapshot-bound proof: reusable only on the same candidate.
        let mut bound = user_binding(EvidenceBinding::Category);
        bound.state = ClaimState::Supported;
        bound.record_proof(ProofRecord {
            claim: bound.claim.clone(),
            state: ClaimState::Supported,
            confidence_bps: 8_000,
            basis: ProofBasis::snapshot_bound(snap_a.clone()),
            evidence: Vec::new(),
            reference: None,
            counterexample: None,
        });
        assert_eq!(
            bound.state_for_candidate(Some(&snap_a)),
            ClaimState::Supported
        );
        assert_eq!(
            bound.state_for_candidate(Some(&snap_b)),
            ClaimState::Unknown
        );
        assert_eq!(
            bound.state_for_candidate(None),
            ClaimState::Unknown,
            "an unknown candidate can never reuse a snapshot-bound proof"
        );
        assert_eq!(
            bound.staleness(Some(&snap_b)),
            Some(ProofStaleness::SnapshotChanged)
        );

        // Snapshot-independent proof: reuse works when the environment basis
        // genuinely survives.
        let env = snap_a.clone().with_environment("env-1");
        let mut independent = user_binding(EvidenceBinding::Category);
        independent.state = ClaimState::Supported;
        independent.record_proof(ProofRecord {
            claim: independent.claim.clone(),
            state: ClaimState::Supported,
            confidence_bps: 8_000,
            basis: ProofBasis::snapshot_independent(env.clone()),
            evidence: Vec::new(),
            reference: None,
            counterexample: None,
        });
        let same_env_new_tree = SubjectSnapshot::new("tm1:bbbb").with_environment("env-1");
        assert_eq!(
            independent.state_for_candidate(Some(&same_env_new_tree)),
            ClaimState::Supported,
            "an independent proof survives a candidate change when its env basis matches"
        );
        let changed_env = SubjectSnapshot::new("tm1:bbbb").with_environment("env-2");
        assert_eq!(
            independent.state_for_candidate(Some(&changed_env)),
            ClaimState::Unknown
        );
        assert_eq!(
            independent.staleness(Some(&changed_env)),
            Some(ProofStaleness::EnvironmentChanged)
        );

        // A claim-grade state with NO recorded proof can never be reused.
        let mut unrecorded = user_binding(EvidenceBinding::Category);
        unrecorded.state = ClaimState::Supported;
        assert_eq!(
            unrecorded.state_for_candidate(Some(&snap_a)),
            ClaimState::Unknown
        );
        assert_eq!(
            unrecorded.staleness(Some(&snap_a)),
            Some(ProofStaleness::SnapshotUnknown)
        );
    }

    #[test]
    fn proof_records_replace_same_identity_and_stay_bounded() {
        let mut obligation = user_binding(EvidenceBinding::Category);
        for round in 0..(MAX_PROOF_RECORDS + 3) {
            obligation.record_proof(ProofRecord {
                claim: obligation.claim.clone(),
                state: ClaimState::Supported,
                confidence_bps: 8_000,
                basis: ProofBasis::snapshot_bound(SubjectSnapshot::new(format!("tm1:{round}"))),
                evidence: Vec::new(),
                reference: None,
                counterexample: None,
            });
        }
        assert_eq!(obligation.proofs.len(), MAX_PROOF_RECORDS);
        // Same (claim, candidate, scope) replaces instead of appending.
        let before = obligation.proofs.len();
        obligation.record_proof(ProofRecord {
            claim: obligation.claim.clone(),
            state: ClaimState::Disproven,
            confidence_bps: 0,
            basis: ProofBasis::snapshot_bound(SubjectSnapshot::new("tm1:0")),
            evidence: Vec::new(),
            reference: None,
            counterexample: Some("observed replay".into()),
        });
        assert_eq!(obligation.proofs.len(), before);
    }

    #[test]
    fn legacy_json_decodes_with_binding_defaults_and_round_trips() {
        let legacy = serde_json::json!({
            "id": "po1:0000000000000000000000000000000000000000000000000000000000000000",
            "claim": "all tests pass",
            "evidence": "test_execution",
            "origin": "user",
            "state": "unknown",
            "confidence_bps": null,
            "evidence_refs": []
        });
        let obligation: ProofObligation = serde_json::from_value(legacy).unwrap();
        assert_eq!(obligation.requirement, Requirement::Required);
        assert_eq!(obligation.evidence_binding, EvidenceBinding::Category);
        assert_eq!(obligation.accepted_strength, AcceptedStrength::Supported);
        assert!(obligation.proofs.is_empty());
        let encoded = serde_json::to_value(&obligation).unwrap();
        assert_eq!(encoded["requirement"], "required");
        assert_eq!(encoded["evidence_binding"], "category");
        assert_eq!(encoded["accepted_strength"], "supported");
        let decoded: ProofObligation = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, obligation);

        // The submission form strips evaluation state but keeps the shape.
        let submission = obligation.without_evaluation_state();
        assert_eq!(submission.claim, obligation.claim);
        assert_eq!(submission.id, obligation.id);
        assert_eq!(submission.state, ClaimState::Unknown);
        assert!(submission.evidence_refs.is_empty());
        assert!(submission.proofs.is_empty());
    }

    #[test]
    fn contract_helpers_report_unresolved_advisory_and_disproven_claims() {
        let candidate = SubjectSnapshot::new("tm1:cand");
        let mut required = user_binding(EvidenceBinding::Category);
        required.state = ClaimState::Uncertain;
        let mut advisory = user_binding(EvidenceBinding::Category);
        advisory.claim = "advisory claim".into();
        advisory.requirement = Requirement::Advisory;
        advisory.state = ClaimState::Unknown;
        let mut disproven = user_binding(EvidenceBinding::Category);
        disproven.claim = "false claim".into();
        disproven.state = ClaimState::Disproven;
        let mut high = RiskHypothesis {
            id: "r1:x".into(),
            description: "race".into(),
            severity: RiskSeverity::High,
            origin: ContractOrigin::Heuristic,
        };
        let contract = EngineeringContract {
            constraints: vec![Constraint {
                id: "c1:x".into(),
                statement: "no new deps".into(),
                origin: ContractOrigin::User,
            }],
            invariants: vec![Invariant {
                id: "i1:x".into(),
                statement: "existing sessions stay valid".into(),
                origin: ContractOrigin::Policy,
            }],
            risks: vec![high.clone()],
            proof_obligations: vec![required.clone(), advisory.clone(), disproven.clone()],
        };
        assert!(!contract.is_empty());
        assert!(contract.has_elevated_risk());
        assert_eq!(contract.verification_depth(), VerificationDepth::Deep);
        assert_eq!(contract.max_risk(), RiskSeverity::High);
        assert_eq!(contract.unresolved_required(Some(&candidate)).len(), 1);
        assert_eq!(contract.advisory_uncertain(Some(&candidate)).len(), 1);
        assert_eq!(contract.disproven(Some(&candidate)).len(), 1);
        high.severity = RiskSeverity::Low;
        let calm = EngineeringContract {
            risks: vec![high],
            ..contract
        };
        assert!(!calm.has_elevated_risk());
        assert_eq!(calm.verification_depth(), VerificationDepth::Standard);
    }
}
