//! @authority-collapse: authority-bearing
//!
//! Claim-bound verification results: the NEUTRAL per-claim round trip of the
//! verification spine.
//!
//! The registry dispatches proof obligations as [`ClaimRequest`]s through the
//! neutral [`VerificationOp`](crate::provider::VerificationOp) vocabulary;
//! every provider answers with [`ClaimVerificationResult`]s keyed by the
//! obligation id it was asked about. Two laws are structural here:
//!
//! 1. **Claim identity is preserved.** A result names the exact obligation it
//!    adjudicates ([`ClaimVerificationResult::obligation_id`]); ids a request
//!    never named are refused at import
//!    ([`VerificationResponse`](crate::provider::VerificationResponse)
//!    validation), never accepted data. No provider-private id syntax is
//!    defined or recognized anywhere in this crate.
//! 2. **Positive verdicts need evidence.** A `Supported`/`KnownTrue` result
//!    without evidence is a typed refusal ([`evidence_adequacy`]) — exactly
//!    the ground a `Disproven` result is already held to. `KnownTrue`
//!    additionally requires proof-grade
//!    ([`EvidenceStrength::Proof`]) evidence.
//! 3. **Subject identity is never synthesized.** When the request declares a
//!    candidate subject snapshot, every provider result must ECHO that exact
//!    identity in both [`ClaimVerificationResult::subject_snapshot`] and the
//!    [`ProofBasis::subject`] of its verdict basis; a missing or mismatched
//!    subject is a typed refusal
//!    ([`ClaimVerificationResult::validate_subject_identity`]). Faktor never
//!    stamps the current candidate onto a provider result after the fact: an
//!    external verdict stands only for the subject the provider itself
//!    answered for. A request carrying no declared subject requires no
//!    identity (the documented no-subject semantics).
//!
//! The routing from a claim to one neutral operation
//! ([`op_for_claim`]) is deterministic: the obligation's evidence binding
//! class decides first (contract ids, property ids, runtime assertions,
//! trace properties, provider claim ids), then the evidence kind; a
//! conservative check-id vocabulary refines `Compile` claims to
//! lint/typecheck operations so a claim can never silently ride an unrelated
//! check.

use faktor_core::contract::{
    AcceptedStrength, ClaimState, EvidenceBinding, ProofBasis, ProofEvidenceKind, ProofObligation,
    ProofScope, Requirement, SubjectSnapshot,
};
use serde::{Deserialize, Serialize};

use crate::provider::{
    ArtifactRef, VerificationError, VerificationOp, VerificationProviderId,
    MAX_ARTIFACT_DETAIL_BYTES, MAX_ARTIFACT_SOURCE_BYTES, MAX_CLAIM_BYTES, MAX_EVIDENCE_REFS,
};

/// Maximum bytes of one obligation id on the wire.
pub const MAX_CLAIM_ID_BYTES: usize = 256;
/// Maximum counterexample entries one claim result may carry.
pub const MAX_COUNTEREXAMPLES: usize = 64;

/// How strong the evidence behind one claim verdict is. The rank is
/// meaningful: a verdict may never claim less ground than the state it
/// establishes (`KnownTrue` requires [`EvidenceStrength::Proof`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceStrength {
    /// No evidence backs the verdict (only honest `Unknown`).
    None,
    /// Bounded observed evidence (a check run, a review, a trace).
    Observed,
    /// Proof-grade evidence (a verifiable provider artifact grounding
    /// `KnownTrue`).
    Proof,
}

impl EvidenceStrength {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Observed => "observed",
            Self::Proof => "proof",
        }
    }

    /// The strength a verdict state implies when a provider does not state
    /// one explicitly: proof-grade truth implies proof-grade evidence, a
    /// decided state implies observed evidence, an undecided state implies
    /// none.
    pub const fn for_state(state: ClaimState) -> Self {
        match state {
            ClaimState::KnownTrue => Self::Proof,
            ClaimState::Supported | ClaimState::Disproven => Self::Observed,
            ClaimState::Inferred | ClaimState::Uncertain | ClaimState::Unknown => Self::None,
        }
    }
}

/// The neutral evidence-adequacy law shared by every provider import: a
/// POSITIVE verdict (`Supported`/`KnownTrue`) needs at least the evidence a
/// NEGATIVE one (`Disproven`) does — an empty `Supported` is a typed refusal,
/// never a cheaper pass than a disproof.
pub fn evidence_adequacy(
    state: ClaimState,
    evidence_len: usize,
    counterexample_len: usize,
) -> Result<(), String> {
    match state {
        ClaimState::Supported | ClaimState::KnownTrue => {
            if evidence_len == 0 {
                return Err(format!(
                    "a {} claim requires evidence; an unevidenced positive verdict is refused",
                    state.as_str()
                ));
            }
        }
        ClaimState::Disproven => {
            if evidence_len == 0 && counterexample_len == 0 {
                return Err("a disproven claim requires a counterexample or evidence".to_string());
            }
        }
        ClaimState::Inferred | ClaimState::Uncertain | ClaimState::Unknown => {}
    }
    Ok(())
}

/// One claim verdict from a verification provider, keyed by the obligation
/// id the request actually named. Neutral by construction: no provider may
/// introduce its own claim id syntax into this shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimVerificationResult {
    pub obligation_id: String,
    /// The candidate subject the verdict was produced against, when the
    /// caller declared one. When the request declares a subject snapshot the
    /// result MUST echo it exactly: a missing or mismatched subject is a
    /// typed refusal at the registry boundary
    /// ([`Self::validate_subject_identity`]); the runtime never stamps the
    /// current candidate onto a result after the fact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject_snapshot: Option<SubjectSnapshot>,
    pub state: ClaimState,
    pub evidence_strength: EvidenceStrength,
    #[serde(default)]
    pub evidence: Vec<ArtifactRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub counterexample: Vec<String>,
    pub provider: VerificationProviderId,
    #[serde(default = "default_basis")]
    pub basis: ProofBasis,
}

fn default_basis() -> ProofBasis {
    ProofBasis {
        subject: None,
        scope: ProofScope::SnapshotBound,
    }
}

impl ClaimVerificationResult {
    /// One bounded result for an explicitly adjudicated claim.
    pub fn verdict(
        provider: VerificationProviderId,
        obligation_id: impl Into<String>,
        state: ClaimState,
        evidence_strength: EvidenceStrength,
        evidence: Vec<ArtifactRef>,
        counterexample: Vec<String>,
    ) -> Self {
        Self {
            obligation_id: obligation_id.into(),
            subject_snapshot: None,
            state,
            evidence_strength,
            evidence,
            counterexample,
            provider,
            basis: ProofBasis {
                subject: None,
                scope: ProofScope::SnapshotBound,
            },
        }
    }

    /// The explicit no-verdict result: the provider was asked and answered
    /// `Unknown` (never a silent omission).
    pub fn unavailable_unknown(
        provider: VerificationProviderId,
        obligation_id: impl Into<String>,
    ) -> Self {
        Self::verdict(
            provider,
            obligation_id,
            ClaimState::Unknown,
            EvidenceStrength::None,
            Vec::new(),
            Vec::new(),
        )
    }

    pub fn with_subject_snapshot(mut self, snapshot: SubjectSnapshot) -> Self {
        self.basis = ProofBasis {
            subject: Some(snapshot.clone()),
            scope: self.basis.scope,
        };
        self.subject_snapshot = Some(snapshot);
        self
    }

    pub fn with_basis(mut self, basis: ProofBasis) -> Self {
        self.basis = basis;
        self
    }

    /// Strict structural validation plus the evidence-adequacy law. Every
    /// failure is typed; nothing is silently trimmed.
    pub fn validate(&self) -> Result<(), VerificationError> {
        if self.obligation_id.is_empty() || self.obligation_id.len() > MAX_CLAIM_ID_BYTES {
            return Err(VerificationError::Malformed(format!(
                "claim obligation_id must be 1..={MAX_CLAIM_ID_BYTES} bytes"
            )));
        }
        if let Some(snapshot) = &self.subject_snapshot {
            if snapshot.candidate.is_empty() {
                return Err(VerificationError::Malformed(
                    "claim subject_snapshot candidate must not be empty".to_string(),
                ));
            }
        }
        if let Some(subject) = &self.basis.subject {
            if subject.candidate.is_empty() {
                return Err(VerificationError::Malformed(
                    "claim basis subject candidate must not be empty".to_string(),
                ));
            }
        }
        if self.evidence.len() > MAX_EVIDENCE_REFS {
            return Err(VerificationError::OutputTooLarge {
                max: MAX_EVIDENCE_REFS,
                actual: self.evidence.len(),
            });
        }
        for artifact in &self.evidence {
            if artifact.source.is_empty() || artifact.source.len() > MAX_ARTIFACT_SOURCE_BYTES {
                return Err(VerificationError::Malformed(format!(
                    "claim evidence source must be 1..={MAX_ARTIFACT_SOURCE_BYTES} bytes"
                )));
            }
            if artifact.detail.len() > MAX_ARTIFACT_DETAIL_BYTES {
                return Err(VerificationError::OutputTooLarge {
                    max: MAX_ARTIFACT_DETAIL_BYTES,
                    actual: artifact.detail.len(),
                });
            }
        }
        if self.counterexample.len() > MAX_COUNTEREXAMPLES {
            return Err(VerificationError::OutputTooLarge {
                max: MAX_COUNTEREXAMPLES,
                actual: self.counterexample.len(),
            });
        }
        for entry in &self.counterexample {
            if entry.is_empty() {
                return Err(VerificationError::Malformed(
                    "claim counterexample entry must not be empty".to_string(),
                ));
            }
            if entry.len() > MAX_ARTIFACT_DETAIL_BYTES {
                return Err(VerificationError::OutputTooLarge {
                    max: MAX_ARTIFACT_DETAIL_BYTES,
                    actual: entry.len(),
                });
            }
        }
        evidence_adequacy(self.state, self.evidence.len(), self.counterexample.len())
            .map_err(VerificationError::Malformed)?;
        if self.state == ClaimState::KnownTrue && self.evidence_strength != EvidenceStrength::Proof
        {
            return Err(VerificationError::Malformed(format!(
                "a known_true claim requires proof-grade evidence, not {}",
                self.evidence_strength.as_str()
            )));
        }
        if self.state == ClaimState::Unknown && !self.evidence.is_empty() {
            return Err(VerificationError::Malformed(
                "an unknown claim must not carry evidence".to_string(),
            ));
        }
        Ok(())
    }

    /// The neutral subject-identity law of the claim round trip (law 3 in
    /// the module docs): when the request declared a subject snapshot, the
    /// result MUST echo that exact identity in both
    /// [`Self::subject_snapshot`] and the [`ProofBasis::subject`] of its
    /// verdict basis. A missing or mismatched subject is a typed refusal —
    /// Faktor never stamps the current candidate onto a provider result after
    /// the fact, so an external verdict can never borrow an identity the
    /// provider did not itself answer for. A request without a declared
    /// subject requires no identity (the documented no-subject semantics).
    pub fn validate_subject_identity(
        &self,
        request_snapshot: Option<&SubjectSnapshot>,
    ) -> Result<(), VerificationError> {
        let Some(request_snapshot) = request_snapshot else {
            return Ok(());
        };
        match &self.subject_snapshot {
            Some(subject) if subject == request_snapshot => {}
            Some(subject) => {
                return Err(VerificationError::Malformed(format!(
                    "claim {} result answered for subject snapshot {} but the request declared {}",
                    self.obligation_id, subject.candidate, request_snapshot.candidate
                )));
            }
            None => {
                return Err(VerificationError::Malformed(format!(
                    "claim {} result omitted the subject snapshot the request declared ({}); \
                     a provider verdict must echo its own subject identity",
                    self.obligation_id, request_snapshot.candidate
                )));
            }
        }
        match &self.basis.subject {
            Some(subject) if subject == request_snapshot => Ok(()),
            Some(subject) => Err(VerificationError::Malformed(format!(
                "claim {} result basis subject {} does not match the request subject {}",
                self.obligation_id, subject.candidate, request_snapshot.candidate
            ))),
            None => Err(VerificationError::Malformed(format!(
                "claim {} result omitted the basis subject the request declared ({}); \
                 an unbound verdict basis is refused",
                self.obligation_id, request_snapshot.candidate
            ))),
        }
    }
}

/// One claim submission: the obligation exactly as the contract states it
/// (id, claim, evidence kind, requirement, binding, accepted strength), with
/// every prior evaluation state stripped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimRequest {
    pub obligation_id: String,
    pub claim: String,
    pub evidence_kind: ProofEvidenceKind,
    #[serde(default)]
    pub requirement: Requirement,
    #[serde(default)]
    pub binding: EvidenceBinding,
    #[serde(default)]
    pub accepted_strength: AcceptedStrength,
}

impl ClaimRequest {
    /// The submission form of one contract obligation (state stripped).
    pub fn from_obligation(obligation: &ProofObligation) -> Self {
        Self {
            obligation_id: obligation.id.clone(),
            claim: obligation.claim.clone(),
            evidence_kind: obligation.evidence,
            requirement: obligation.requirement,
            binding: obligation.evidence_binding.clone(),
            accepted_strength: obligation.accepted_strength,
        }
    }

    pub fn validate(&self) -> Result<(), VerificationError> {
        if self.obligation_id.is_empty() || self.obligation_id.len() > MAX_CLAIM_ID_BYTES {
            return Err(VerificationError::Malformed(format!(
                "claim obligation_id must be 1..={MAX_CLAIM_ID_BYTES} bytes"
            )));
        }
        if self.claim.is_empty() || self.claim.len() > MAX_CLAIM_BYTES {
            return Err(VerificationError::Malformed(format!(
                "claim text must be 1..={MAX_CLAIM_BYTES} bytes"
            )));
        }
        self.binding
            .validate()
            .map_err(|reason| VerificationError::Malformed(format!("claim binding: {reason}")))?;
        Ok(())
    }
}

/// The deterministic neutral operation one claim is dispatched through.
///
/// The binding class decides first (a claim bound to contract ids, property
/// ids, runtime assertions, trace properties or a provider claim id is only
/// adjudicable by that class of verification); then the evidence kind routes
/// to the canonical operation. A conservative check-id vocabulary refines
/// compile claims to the lint/typecheck operations when the named checks are
/// lint/typecheck checks. Everything else routes to the canonical operation
/// of its evidence kind.
pub fn op_for_claim(claim: &ClaimRequest) -> VerificationOp {
    match &claim.binding {
        EvidenceBinding::ProviderClaim(_) => VerificationOp::VerifyClaim,
        EvidenceBinding::ContractIds(_) => VerificationOp::CheckContracts,
        EvidenceBinding::PropertyIds(_) => VerificationOp::RunGeneratedTests,
        EvidenceBinding::RuntimeAssertions(_) | EvidenceBinding::TraceProperties(_) => {
            VerificationOp::RunRuntimeChecks
        }
        EvidenceBinding::Category
        | EvidenceBinding::CheckIds(_)
        | EvidenceBinding::TestSelector(_)
        | EvidenceBinding::TestSelectors(_) => match claim.evidence_kind {
            ProofEvidenceKind::Compile => {
                if binding_names(&claim.binding, &["clippy", "lint"]) {
                    VerificationOp::Lint
                } else if binding_names(&claim.binding, &["typecheck", "tsc"]) {
                    VerificationOp::Typecheck
                } else {
                    VerificationOp::Build
                }
            }
            ProofEvidenceKind::TestExecution => VerificationOp::RunTests,
            ProofEvidenceKind::ContractCheck => VerificationOp::CheckContracts,
            ProofEvidenceKind::Review => VerificationOp::VerifyClaim,
            ProofEvidenceKind::RuntimeTrace => VerificationOp::RunRuntimeChecks,
        },
    }
}

/// The submission form of one contract obligation.
pub fn claim_request_for(obligation: &ProofObligation) -> ClaimRequest {
    ClaimRequest::from_obligation(obligation)
}

/// TRUE when any target of `binding` contains one of `needles`
/// (case-insensitive, deterministic).
fn binding_names(binding: &EvidenceBinding, needles: &[&str]) -> bool {
    let texts: Vec<&str> = match binding {
        EvidenceBinding::Category => return false,
        EvidenceBinding::CheckIds(t)
        | EvidenceBinding::TestSelectors(t)
        | EvidenceBinding::ContractIds(t)
        | EvidenceBinding::PropertyIds(t)
        | EvidenceBinding::RuntimeAssertions(t)
        | EvidenceBinding::TraceProperties(t) => t.iter().map(String::as_str).collect(),
        EvidenceBinding::TestSelector(s) | EvidenceBinding::ProviderClaim(s) => vec![s.as_str()],
    };
    texts.iter().any(|text| {
        let lower = text.to_ascii_lowercase();
        needles.iter().any(|needle| lower.contains(needle))
    })
}

/// The test selectors a test-check command exercised (`cargo test <sel>`,
/// `cargo nextest run <sel>`): flags and flag values are not selectors. The
/// same conservative extraction the runtime's evidence path uses.
pub(crate) fn test_selectors(program: &str, args: &[String]) -> Vec<String> {
    let program_name = std::path::Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program);
    if program_name != "cargo" {
        return Vec::new();
    }
    let rest: &[String] = match args.first().map(String::as_str) {
        Some("test") => &args[1..],
        Some("nextest") if args.get(1).map(String::as_str) == Some("run") => &args[2..],
        _ => return Vec::new(),
    };
    let value_flags = [
        "-p",
        "--package",
        "--test",
        "--bench",
        "--bin",
        "--example",
        "--features",
        "-F",
        "--exclude",
    ];
    let mut selectors = Vec::new();
    let mut index = 0;
    while index < rest.len() {
        let arg = &rest[index];
        if arg == "--" {
            selectors.extend(rest[index + 1..].iter().cloned());
            break;
        }
        if arg.starts_with('-') {
            if value_flags.contains(&arg.as_str()) {
                index += 2;
                continue;
            }
            index += 1;
            continue;
        }
        selectors.push(arg.clone());
        index += 1;
    }
    selectors
}

/// The bounded evidence binding one executed check carries (its check id
/// plus, for test checks, the selectors it exercised).
pub(crate) fn evidence_binding_for_check(
    id: &str,
    program: &str,
    args: &[String],
    is_test: bool,
) -> Vec<EvidenceBinding> {
    let mut bindings = vec![EvidenceBinding::CheckIds(vec![id.to_string()])];
    if is_test {
        let selectors = test_selectors(program, args);
        match selectors.len() {
            0 => {}
            1 => bindings.push(EvidenceBinding::TestSelector(selectors[0].clone())),
            _ => bindings.push(EvidenceBinding::TestSelectors(selectors)),
        }
    }
    bindings
}

/// The default basis scope of a provider verdict: bound to the subject it
/// answered for (a verdict never silently outlives its candidate).
pub const fn provider_verdict_scope() -> ProofScope {
    ProofScope::SnapshotBound
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_core::contract::ContractOrigin;

    fn artifact(source: &str) -> ArtifactRef {
        ArtifactRef::new(ProofEvidenceKind::Review, source, "detail").unwrap()
    }

    fn provider() -> VerificationProviderId {
        VerificationProviderId::parse("example").unwrap()
    }

    #[test]
    fn positive_verdicts_need_evidence_not_less_than_negative() {
        // Supported without evidence is refused exactly like a groundless
        // disproof.
        let supported = ClaimVerificationResult::verdict(
            provider(),
            "po1:abc",
            ClaimState::Supported,
            EvidenceStrength::Observed,
            Vec::new(),
            Vec::new(),
        );
        assert!(matches!(
            supported.validate(),
            Err(VerificationError::Malformed(_))
        ));
        let disproven = ClaimVerificationResult::verdict(
            provider(),
            "po1:abc",
            ClaimState::Disproven,
            EvidenceStrength::Observed,
            Vec::new(),
            Vec::new(),
        );
        assert!(matches!(
            disproven.validate(),
            Err(VerificationError::Malformed(_))
        ));
        let known_true = ClaimVerificationResult::verdict(
            provider(),
            "po1:abc",
            ClaimState::KnownTrue,
            EvidenceStrength::Observed,
            vec![artifact("kiwi")],
            Vec::new(),
        );
        assert!(
            matches!(known_true.validate(), Err(VerificationError::Malformed(_))),
            "known_true needs proof-grade evidence, not just any evidence"
        );
        // With evidence both pass; a counterexample alone grounds a disproof.
        let supported = ClaimVerificationResult::verdict(
            provider(),
            "po1:abc",
            ClaimState::Supported,
            EvidenceStrength::Observed,
            vec![artifact("kiwi")],
            Vec::new(),
        );
        supported.validate().unwrap();
        let disproven = ClaimVerificationResult::verdict(
            provider(),
            "po1:abc",
            ClaimState::Disproven,
            EvidenceStrength::Observed,
            Vec::new(),
            vec!["replayed token".to_string()],
        );
        disproven.validate().unwrap();
        let known_true = ClaimVerificationResult::verdict(
            provider(),
            "po1:abc",
            ClaimState::KnownTrue,
            EvidenceStrength::Proof,
            vec![artifact("kiwi")],
            Vec::new(),
        );
        known_true.validate().unwrap();
        // Unknown is free of claims and refuses stray evidence.
        let unknown = ClaimVerificationResult::unavailable_unknown(provider(), "po1:abc");
        unknown.validate().unwrap();
        let mut stray = unknown.clone();
        stray.evidence.push(artifact("kiwi"));
        assert!(matches!(
            stray.validate(),
            Err(VerificationError::Malformed(_))
        ));
    }

    #[test]
    fn hostile_ids_counts_and_bounds_are_refused_typed() {
        let mut result = ClaimVerificationResult::unavailable_unknown(provider(), "");
        assert!(result.validate().is_err());
        result.obligation_id = "x".repeat(MAX_CLAIM_ID_BYTES + 1);
        assert!(result.validate().is_err());
        result.obligation_id = "po1:ok".into();
        result.counterexample = vec![String::new()];
        result.state = ClaimState::Disproven;
        assert!(result.validate().is_err());
        result.counterexample = vec!["x".repeat(MAX_ARTIFACT_DETAIL_BYTES + 1)];
        assert!(matches!(
            result.validate(),
            Err(VerificationError::OutputTooLarge { .. })
        ));
        let counterexamples: Vec<String> = (0..=MAX_COUNTEREXAMPLES)
            .map(|i| format!("example {i}"))
            .collect();
        let result = ClaimVerificationResult::verdict(
            provider(),
            "po1:ok",
            ClaimState::Disproven,
            EvidenceStrength::Observed,
            Vec::new(),
            counterexamples,
        );
        assert!(matches!(
            result.validate(),
            Err(VerificationError::OutputTooLarge { .. })
        ));
        let request = ClaimRequest {
            obligation_id: "po1:ok".into(),
            claim: String::new(),
            evidence_kind: ProofEvidenceKind::Compile,
            requirement: Requirement::Required,
            binding: EvidenceBinding::Category,
            accepted_strength: AcceptedStrength::Supported,
        };
        assert!(request.validate().is_err());
    }

    #[test]
    fn claim_routing_is_binding_first_then_kind_and_deterministic() {
        let route = |binding: EvidenceBinding, kind: ProofEvidenceKind| {
            op_for_claim(&ClaimRequest {
                obligation_id: "po1:x".into(),
                claim: "c".into(),
                evidence_kind: kind,
                requirement: Requirement::Required,
                binding,
                accepted_strength: AcceptedStrength::Supported,
            })
        };
        assert_eq!(
            route(
                EvidenceBinding::ProviderClaim("po1:x".into()),
                ProofEvidenceKind::Review
            ),
            VerificationOp::VerifyClaim
        );
        assert_eq!(
            route(
                EvidenceBinding::ContractIds(vec!["c1".into()]),
                ProofEvidenceKind::ContractCheck
            ),
            VerificationOp::CheckContracts
        );
        assert_eq!(
            route(
                EvidenceBinding::PropertyIds(vec!["p1".into()]),
                ProofEvidenceKind::TestExecution
            ),
            VerificationOp::RunGeneratedTests
        );
        assert_eq!(
            route(
                EvidenceBinding::RuntimeAssertions(vec!["a1".into()]),
                ProofEvidenceKind::RuntimeTrace
            ),
            VerificationOp::RunRuntimeChecks
        );
        assert_eq!(
            route(
                EvidenceBinding::TraceProperties(vec!["t1".into()]),
                ProofEvidenceKind::RuntimeTrace
            ),
            VerificationOp::RunRuntimeChecks
        );
        assert_eq!(
            route(
                EvidenceBinding::CheckIds(vec!["rust_check".into()]),
                ProofEvidenceKind::Compile
            ),
            VerificationOp::Build
        );
        assert_eq!(
            route(
                EvidenceBinding::CheckIds(vec!["rust_clippy".into()]),
                ProofEvidenceKind::Compile
            ),
            VerificationOp::Lint
        );
        assert_eq!(
            route(
                EvidenceBinding::CheckIds(vec!["tsc-typecheck".into()]),
                ProofEvidenceKind::Compile
            ),
            VerificationOp::Typecheck
        );
        assert_eq!(
            route(
                EvidenceBinding::TestSelector("a::b".into()),
                ProofEvidenceKind::TestExecution
            ),
            VerificationOp::RunTests
        );
        assert_eq!(
            route(EvidenceBinding::Category, ProofEvidenceKind::ContractCheck),
            VerificationOp::CheckContracts
        );
        assert_eq!(
            route(EvidenceBinding::Category, ProofEvidenceKind::Review),
            VerificationOp::VerifyClaim
        );
        assert_eq!(
            route(EvidenceBinding::Category, ProofEvidenceKind::RuntimeTrace),
            VerificationOp::RunRuntimeChecks
        );
    }

    #[test]
    fn claim_request_strips_state_and_keeps_identity() {
        let mut obligation = ProofObligation {
            id: "po1:identity".into(),
            claim: "replay is rejected".into(),
            requirement: Requirement::Required,
            evidence: ProofEvidenceKind::TestExecution,
            evidence_binding: EvidenceBinding::TestSelector("auth::replay".into()),
            accepted_strength: AcceptedStrength::Supported,
            state: ClaimState::Supported,
            origin: ContractOrigin::User,
            confidence_bps: Some(8_000),
            evidence_refs: vec![faktor_core::contract::EvidenceRef {
                kind: ProofEvidenceKind::TestExecution,
                source: "cargo-test".into(),
                detail: "passed".into(),
            }],
            proofs: Vec::new(),
            binding_updates: Vec::new(),
        };
        let request = ClaimRequest::from_obligation(&obligation);
        assert_eq!(request.obligation_id, "po1:identity");
        assert_eq!(request.claim, "replay is rejected");
        assert_eq!(
            request.binding,
            EvidenceBinding::TestSelector("auth::replay".into())
        );
        let round_trip: ClaimRequest =
            serde_json::from_str(&serde_json::to_string(&request).unwrap()).unwrap();
        assert_eq!(round_trip, request);
        obligation.state = ClaimState::Disproven;
        let after = ClaimRequest::from_obligation(&obligation);
        assert_eq!(
            after, request,
            "prior evaluation state never rides the submission"
        );
    }

    #[test]
    fn selector_extraction_matches_the_runtime_evidence_path() {
        assert_eq!(
            test_selectors("cargo", &["test".into(), "auth::replay".into()]),
            vec!["auth::replay".to_string()]
        );
        assert_eq!(
            test_selectors(
                "cargo",
                &["nextest".into(), "run".into(), "auth::replay".into()]
            ),
            vec!["auth::replay".to_string()]
        );
        assert_eq!(
            test_selectors(
                "cargo",
                &["test".into(), "--package".into(), "p".into(), "sel".into()]
            ),
            vec!["sel".to_string()]
        );
        assert!(test_selectors("cargo", &["test".into()]).is_empty());
        assert!(test_selectors("pytest", &["test".into(), "x".into()]).is_empty());
    }
}
