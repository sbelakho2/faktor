// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! @authority-collapse: authority-bearing
//!
//! The bounded Kiwi wire contract.
//!
//! Wire schema version [`KIWI_SCHEMA_VERSION`] (v1):
//!
//! Request (`POST <endpoint>`, JSON):
//!
//! ```json
//! {
//!   "v": 1,
//!   "op": "verify_claim",
//!   "job": {
//!     "job_id": "v1:7-3-verify_claim",
//!     "op": "verify_claim",
//!     "workspace_id": 1,
//!     "session_id": 3,
//!     "objective": "...",
//!     "acceptance_criteria": ["..."],
//!     "constraints": [{"id":"c1","statement":"...","origin":"user"}],
//!     "invariants": [{"id":"i1","statement":"...","origin":"policy"}],
//!     "risks": [{"id":"r1","description":"...","severity":"high","origin":"heuristic"}],
//!     "proof_obligations": [{"id":"po1:...","claim":"...","evidence":"test_execution",
//!                            "origin":"user","state":"unknown","confidence_bps":null,
//!                            "evidence_refs":[]}],
//!     "target": "workspace" | {"paths": ["src/lib.rs"]},
//!     "claim": "..." | null,
//!     "max_output_bytes": 262144,
//!     "deadline_ms": 1791648000000 | null
//!   }
//! }
//! ```
//!
//! Response (JSON):
//!
//! ```json
//! {
//!   "v": 1,
//!   "op": "verify_claim",
//!   "job_id": "v1:7-3-verify_claim",
//!   "outcome": "passed" | "failed" | "unavailable" | "refused",
//!   "summary": "...",
//!   "claims": [
//!     {"claim_id": "po1:...", "state": "supported" | "disproven" | "unknown" | ...,
//!      "evidence": [{"kind":"test_execution","source":"...","detail":"..."}],
//!      "counterexample": "..." | null}
//!   ],
//!   "evidence": [{"kind":"runtime_trace","source":"...","detail":"..."}]
//! }
//! ```
//!
//! Rules enforced by this module:
//!
//! - The submission is assembled ONLY from the neutral request: a claim
//!   batch carries exactly its claim obligations (prior evaluation state,
//!   confidence and evidence refs stripped), and an op-only job carries the
//!   derived required behavior. Production never submits the engineering
//!   contract's constraints/invariants/risks, so ephemeral design-readiness
//!   findings and other local context can never prescribe anything to Kiwi.
//! - Every string, list and claim id is bounded; a job or reply that exceeds
//!   a bound is a typed refusal, never silently cut by the wire layer.
//! - Evidence must reference obligation ids the job actually submitted;
//!   unknown or duplicated claim ids are [`VerificationError::Malformed`],
//!   and every REQUIRED submitted obligation must come back with a result
//!   (or an explicit unknown) — a silently omitted required claim is a typed
//!   refusal.
//! - A positive verdict (`supported`/`known_true`) must carry evidence (at
//!   least the ground a `disproven` verdict needs), and `known_true` must
//!   declare proof-grade evidence.

use std::collections::BTreeSet;

use faktor_core::{
    contains_design_readiness_marker, is_design_readiness_id, ClaimState, Constraint,
    ContractOrigin, EngineeringContract, Invariant, ProofEvidenceKind, ProofObligation,
    Requirement, RiskHypothesis, SessionId, WorkspaceId,
};
use faktor_verify::{
    evidence_adequacy, ArtifactRef, EvidenceStrength, VerificationError, VerificationOp,
    VerificationOutcome, VerificationRequest, VerificationTarget,
};
use serde::{Deserialize, Serialize};

/// The Kiwi wire schema version this build speaks.
pub const KIWI_SCHEMA_VERSION: u32 = 1;
/// Maximum bytes of one job id / claim id.
pub const MAX_KIWI_ID_BYTES: usize = 256;
/// Maximum bytes of one free-text job/response field.
pub const MAX_KIWI_JOB_TEXT_BYTES: usize = 16 * 1024;
/// Maximum bytes of one reply text field (summary/counterexample). Aligned
/// with `faktor_verify::MAX_ARTIFACT_DETAIL_BYTES`, so an ingested
/// counterexample always fits the evidence-detail bound without truncation.
pub const MAX_KIWI_TEXT_BYTES: usize = 4 * 1024;
/// Maximum acceptance criteria in one job.
pub const MAX_KIWI_ACCEPTANCE_CRITERIA: usize = 256;
/// Maximum entries per contract list (constraints/invariants/risks).
pub const MAX_KIWI_CONTRACT_ENTRIES: usize = 1024;
/// Maximum proof obligations in one job.
pub const MAX_KIWI_OBLIGATIONS: usize = 1024;
/// Maximum claim evidence entries in one reply.
pub const MAX_KIWI_CLAIMS: usize = 1024;
/// Maximum evidence refs attached to one claim.
pub const MAX_KIWI_EVIDENCE_PER_CLAIM: usize = 64;

/// The bounded submission payload. Built from the frozen contract types; no
/// field prescribes an expected result. `workspace_id`/`session_id` are
/// IDENTITY (which code under verification the target paths are relative
/// to), not expectations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KiwiJob {
    pub job_id: String,
    pub op: VerificationOp,
    pub workspace_id: WorkspaceId,
    pub session_id: SessionId,
    pub objective: String,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub constraints: Vec<Constraint>,
    #[serde(default)]
    pub invariants: Vec<Invariant>,
    #[serde(default)]
    pub risks: Vec<RiskHypothesis>,
    #[serde(default)]
    pub proof_obligations: Vec<ProofObligation>,
    pub target: VerificationTarget,
    #[serde(default)]
    pub claim: Option<String>,
    pub max_output_bytes: u64,
    #[serde(default)]
    pub deadline_ms: Option<i64>,
}

/// The deterministic job id of one interface request. Stable across
/// retries of the same operation, bounded, and echoed back by the provider.
pub fn job_id_for(request: &VerificationRequest) -> String {
    format!(
        "v1:{}-{}-{}",
        request.op_id.raw(),
        request.session_id.raw(),
        request.op.as_str()
    )
}

impl KiwiJob {
    /// The minimal job for an op-only interface request: the objective and
    /// one proof obligation are derived from the requested operation (never
    /// from a caller expectation).
    pub fn for_request(request: &VerificationRequest) -> Result<Self, VerificationError> {
        let claim = request.claim.as_deref();
        let obligation_claim = derived_claim(request.op, claim);
        let job = Self {
            job_id: job_id_for(request),
            op: request.op,
            workspace_id: request.workspace_id,
            session_id: request.session_id,
            objective: derived_objective(request.op, claim),
            acceptance_criteria: vec![obligation_claim.clone()],
            constraints: Vec::new(),
            invariants: Vec::new(),
            risks: Vec::new(),
            proof_obligations: vec![EngineeringContract::obligation(
                &obligation_claim,
                evidence_kind_for(request.op),
                ContractOrigin::Policy,
            )],
            target: request.target.clone(),
            claim: request.claim.clone(),
            max_output_bytes: request.max_output_bytes as u64,
            deadline_ms: request.deadline_ms,
        };
        job.validate()?;
        Ok(job)
    }

    /// The full-contract claim-batch job: every claim the neutral request
    /// named rides as a proof obligation with the SAME obligation id (claim
    /// identity is preserved end to end), the same evidence kind, requirement,
    /// binding and accepted strength, and every prior evaluation state
    /// stripped.
    pub fn for_claims(request: &VerificationRequest) -> Result<Self, VerificationError> {
        if request.claims.is_empty() {
            return Err(VerificationError::Malformed(
                "a claim-batch job requires at least one claim".to_string(),
            ));
        }
        let obligations: Vec<ProofObligation> = request
            .claims
            .iter()
            .map(|claim| ProofObligation {
                id: claim.obligation_id.clone(),
                claim: claim.claim.clone(),
                requirement: claim.requirement,
                evidence: claim.evidence_kind,
                evidence_binding: claim.binding.clone(),
                accepted_strength: claim.accepted_strength,
                state: ClaimState::Unknown,
                origin: ContractOrigin::Policy,
                confidence_bps: None,
                evidence_refs: Vec::new(),
                proofs: Vec::new(),
                binding_updates: Vec::new(),
            })
            .collect();
        let job = Self {
            job_id: job_id_for(request),
            op: request.op,
            workspace_id: request.workspace_id,
            session_id: request.session_id,
            objective: derived_objective(request.op, request.claim.as_deref()),
            acceptance_criteria: request
                .claims
                .iter()
                .map(|claim| claim.claim.clone())
                .collect(),
            constraints: Vec::new(),
            invariants: Vec::new(),
            risks: Vec::new(),
            proof_obligations: obligations,
            target: request.target.clone(),
            claim: request.claim.clone(),
            max_output_bytes: request.max_output_bytes as u64,
            deadline_ms: request.deadline_ms,
        };
        job.validate()?;
        Ok(job)
    }

    /// The obligation ids this job submitted: the only claim ids a reply may
    /// reference.
    pub fn obligation_ids(&self) -> BTreeSet<&str> {
        self.proof_obligations
            .iter()
            .map(|obligation| obligation.id.as_str())
            .collect()
    }

    /// The REQUIRED obligation ids this job submitted: every one of them must
    /// come back with a result (or an explicit unknown), never silently
    /// omitted.
    pub fn required_obligation_ids(&self) -> BTreeSet<&str> {
        self.proof_obligations
            .iter()
            .filter(|obligation| obligation.requirement == Requirement::Required)
            .map(|obligation| obligation.id.as_str())
            .collect()
    }

    /// Strict bounds. Every failure is typed; nothing is silently cut.
    pub fn validate(&self) -> Result<(), VerificationError> {
        validate_id("job_id", &self.job_id)?;
        validate_independent_text("objective", &self.objective, true, MAX_KIWI_JOB_TEXT_BYTES)?;
        if self.acceptance_criteria.len() > MAX_KIWI_ACCEPTANCE_CRITERIA {
            return Err(VerificationError::Malformed(format!(
                "job carries {} acceptance criteria, over the {MAX_KIWI_ACCEPTANCE_CRITERIA} bound",
                self.acceptance_criteria.len()
            )));
        }
        for (index, criterion) in self.acceptance_criteria.iter().enumerate() {
            validate_independent_text(
                &format!("acceptance_criteria[{index}]"),
                criterion,
                true,
                MAX_KIWI_JOB_TEXT_BYTES,
            )?;
        }
        for (name, count) in [
            ("constraints", self.constraints.len()),
            ("invariants", self.invariants.len()),
            ("risks", self.risks.len()),
        ] {
            if count > MAX_KIWI_CONTRACT_ENTRIES {
                return Err(VerificationError::Malformed(format!(
                    "job carries {count} {name}, over the {MAX_KIWI_CONTRACT_ENTRIES} bound"
                )));
            }
        }
        for constraint in &self.constraints {
            validate_independent_id("constraint.id", &constraint.id)?;
            validate_independent_text(
                "constraint.statement",
                &constraint.statement,
                true,
                MAX_KIWI_JOB_TEXT_BYTES,
            )?;
        }
        for invariant in &self.invariants {
            validate_independent_id("invariant.id", &invariant.id)?;
            validate_independent_text(
                "invariant.statement",
                &invariant.statement,
                true,
                MAX_KIWI_JOB_TEXT_BYTES,
            )?;
        }
        for risk in &self.risks {
            validate_independent_id("risk.id", &risk.id)?;
            validate_independent_text(
                "risk.description",
                &risk.description,
                true,
                MAX_KIWI_JOB_TEXT_BYTES,
            )?;
        }
        if self.proof_obligations.len() > MAX_KIWI_OBLIGATIONS {
            return Err(VerificationError::Malformed(format!(
                "job carries {} proof obligations, over the {MAX_KIWI_OBLIGATIONS} bound",
                self.proof_obligations.len()
            )));
        }
        if self.proof_obligations.is_empty() {
            return Err(VerificationError::Malformed(
                "job carries no proof obligation to verify".to_string(),
            ));
        }
        let mut seen = BTreeSet::new();
        for obligation in &self.proof_obligations {
            validate_independent_id("obligation.id", &obligation.id)?;
            validate_independent_text(
                "obligation.claim",
                &obligation.claim,
                true,
                MAX_KIWI_JOB_TEXT_BYTES,
            )?;
            if !seen.insert(obligation.id.as_str()) {
                return Err(VerificationError::Malformed(format!(
                    "job carries duplicate obligation id {:?}",
                    obligation.id
                )));
            }
        }
        self.target.validate()?;
        if let Some(claim) = &self.claim {
            validate_independent_text("claim", claim, true, MAX_KIWI_JOB_TEXT_BYTES)?;
        }
        Ok(())
    }

    /// A claim batch may carry ONLY its neutral claim obligations — never
    /// the engineering contract's constraints, invariants or risk
    /// hypotheses. This is the structural independence guard: local
    /// design-readiness findings are ephemeral and can never ride a
    /// production claim-batch submission.
    pub fn validate_claim_batch(&self) -> Result<(), VerificationError> {
        if !self.constraints.is_empty() || !self.invariants.is_empty() || !self.risks.is_empty() {
            return Err(VerificationError::Malformed(
                "a production claim batch must not carry contract constraints, invariants or risks"
                    .to_string(),
            ));
        }
        Ok(())
    }
}

/// One claim verdict from the external verifier: evidence and/or a
/// counterexample per claim id. Strictly bounded. `evidence_strength` is the
/// provider's declared strength; when absent it is derived from the state
/// (proof-grade truth implies proof-grade evidence).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KiwiEvidence {
    pub claim_id: String,
    /// The evaluated state, using the frozen contract taxonomy.
    pub state: ClaimState,
    /// The provider's declared evidence strength; absent = derived from the
    /// state ([`EvidenceStrength::for_state`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_strength: Option<EvidenceStrength>,
    #[serde(default)]
    pub evidence: Vec<ArtifactRef>,
    #[serde(default)]
    pub counterexample: Option<String>,
}

impl KiwiEvidence {
    /// The effective evidence strength of this verdict.
    pub fn effective_strength(&self) -> EvidenceStrength {
        self.evidence_strength
            .unwrap_or_else(|| EvidenceStrength::for_state(self.state))
    }

    pub fn validate(&self) -> Result<(), VerificationError> {
        validate_id("claim_id", &self.claim_id)?;
        if self.evidence.len() > MAX_KIWI_EVIDENCE_PER_CLAIM {
            return Err(VerificationError::Malformed(format!(
                "claim {} carries {} evidence refs, over the {MAX_KIWI_EVIDENCE_PER_CLAIM} bound",
                self.claim_id,
                self.evidence.len()
            )));
        }
        if let Some(counterexample) = &self.counterexample {
            validate_text("counterexample", counterexample, true, MAX_KIWI_TEXT_BYTES)?;
        }
        // The neutral evidence-adequacy law: a positive verdict needs at
        // least the evidence a negative one does; a disproof needs a ground.
        evidence_adequacy(
            self.state,
            self.evidence.len(),
            usize::from(self.counterexample.is_some()),
        )
        .map_err(VerificationError::Malformed)?;
        if self.state == ClaimState::KnownTrue
            && self.effective_strength() != EvidenceStrength::Proof
        {
            return Err(VerificationError::Malformed(format!(
                "claim {} is known_true with {} evidence; proof-grade evidence is required",
                self.claim_id,
                self.effective_strength().as_str()
            )));
        }
        Ok(())
    }
}

/// The bounded response payload of one Kiwi verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KiwiReply {
    pub outcome: VerificationOutcome,
    pub summary: String,
    #[serde(default)]
    pub claims: Vec<KiwiEvidence>,
    #[serde(default)]
    pub evidence: Vec<ArtifactRef>,
}

impl KiwiReply {
    /// Strict bounds over the whole reply.
    pub fn validate(&self) -> Result<(), VerificationError> {
        validate_text("summary", &self.summary, false, MAX_KIWI_TEXT_BYTES)?;
        if self.claims.len() > MAX_KIWI_CLAIMS {
            return Err(VerificationError::Malformed(format!(
                "reply carries {} claims, over the {MAX_KIWI_CLAIMS} bound",
                self.claims.len()
            )));
        }
        for claim in &self.claims {
            claim.validate()?;
        }
        if self.evidence.len() > MAX_KIWI_CLAIMS {
            return Err(VerificationError::Malformed(format!(
                "reply carries {} evidence refs, over the {MAX_KIWI_CLAIMS} bound",
                self.evidence.len()
            )));
        }
        Ok(())
    }

    /// Every claim id must be an obligation the job submitted, exactly once,
    /// and every REQUIRED obligation the job submitted must come back with a
    /// result (or an explicit unknown) — a silently omitted required claim is
    /// a typed refusal, never accepted data.
    pub fn validate_claims_for(&self, job: &KiwiJob) -> Result<(), VerificationError> {
        let known = job.obligation_ids();
        let mut seen = BTreeSet::new();
        for claim in &self.claims {
            if !known.contains(claim.claim_id.as_str()) {
                return Err(VerificationError::Malformed(format!(
                    "reply references claim {} which the submitted job does not carry",
                    claim.claim_id
                )));
            }
            if !seen.insert(claim.claim_id.as_str()) {
                return Err(VerificationError::Malformed(format!(
                    "reply carries claim {} more than once",
                    claim.claim_id
                )));
            }
        }
        for required in job.required_obligation_ids() {
            if !seen.contains(required) {
                return Err(VerificationError::Malformed(format!(
                    "reply silently omitted required claim {required} which the submitted job carries"
                )));
            }
        }
        Ok(())
    }
}

/// The required behavior of one op, stated as a claim (never an expected
/// result).
fn derived_claim(op: VerificationOp, request_claim: Option<&str>) -> String {
    match op {
        VerificationOp::VerifyClaim => request_claim
            .unwrap_or("the submitted claim holds")
            .to_string(),
        VerificationOp::Build => "the workspace builds".to_string(),
        VerificationOp::Typecheck => "the workspace type-checks".to_string(),
        VerificationOp::Lint => "the analyzed scope carries no lint findings".to_string(),
        VerificationOp::RunTests => "the project's test suite passes".to_string(),
        VerificationOp::RunGeneratedTests => "the generated tests pass".to_string(),
        VerificationOp::CheckContracts => "the stated contracts hold".to_string(),
        VerificationOp::CheckCompatibility => {
            "the stated compatibility constraints hold".to_string()
        }
        VerificationOp::RunSecurityChecks => {
            "no security violation is demonstrated in the analyzed scope".to_string()
        }
        VerificationOp::RunPerformanceChecks => {
            "the stated performance obligations hold".to_string()
        }
        VerificationOp::RunRuntimeChecks => "the stated runtime obligations hold".to_string(),
    }
}

fn derived_objective(op: VerificationOp, request_claim: Option<&str>) -> String {
    match op {
        VerificationOp::VerifyClaim => {
            format!("verify the claim: {}", derived_claim(op, request_claim))
        }
        _ => derived_claim(op, request_claim),
    }
}

/// The frozen evidence taxonomy kind that can discharge one op's claim.
pub fn evidence_kind_for(op: VerificationOp) -> ProofEvidenceKind {
    match op {
        VerificationOp::Build | VerificationOp::Lint | VerificationOp::Typecheck => {
            ProofEvidenceKind::Compile
        }
        VerificationOp::RunTests | VerificationOp::RunGeneratedTests => {
            ProofEvidenceKind::TestExecution
        }
        VerificationOp::CheckContracts | VerificationOp::CheckCompatibility => {
            ProofEvidenceKind::ContractCheck
        }
        VerificationOp::RunSecurityChecks
        | VerificationOp::RunPerformanceChecks
        | VerificationOp::RunRuntimeChecks => ProofEvidenceKind::RuntimeTrace,
        VerificationOp::VerifyClaim => ProofEvidenceKind::Review,
    }
}

fn validate_id(name: &str, value: &str) -> Result<(), VerificationError> {
    validate_text(name, value, true, MAX_KIWI_ID_BYTES)
}

/// IDs on an independent-verification submission must never be readiness
/// finding identities: those are local advisory state, not claims.
fn validate_independent_id(name: &str, value: &str) -> Result<(), VerificationError> {
    validate_id(name, value)?;
    if is_design_readiness_id(value) {
        return Err(VerificationError::Malformed(format!(
            "kiwi {name} must not carry an ephemeral design-readiness finding id"
        )));
    }
    Ok(())
}

/// Free text on an independent-verification submission must never carry
/// readiness markers (the applied description prefix or an embedded
/// `dr1:` identity): Kiwi must adjudicate Faktor's required behavior, not
/// Faktor's local readiness hypotheses.
fn validate_independent_text(
    name: &str,
    value: &str,
    non_empty: bool,
    max: usize,
) -> Result<(), VerificationError> {
    validate_text(name, value, non_empty, max)?;
    if contains_design_readiness_marker(value) {
        return Err(VerificationError::Malformed(format!(
            "kiwi {name} must not carry ephemeral design-readiness text"
        )));
    }
    Ok(())
}

fn validate_text(
    name: &str,
    value: &str,
    non_empty: bool,
    max: usize,
) -> Result<(), VerificationError> {
    if non_empty && value.is_empty() {
        return Err(VerificationError::Malformed(format!(
            "kiwi {name} must not be empty"
        )));
    }
    if value.len() > max {
        return Err(VerificationError::Malformed(format!(
            "kiwi {name} is {} bytes, over the {max}-byte bound",
            value.len()
        )));
    }
    Ok(())
}

/// The outbound frame.
#[derive(Serialize)]
pub(crate) struct WireRequest<'a> {
    pub(crate) v: u32,
    pub(crate) op: &'a str,
    pub(crate) job: &'a KiwiJob,
}

/// The inbound frame. `reply` is flattened into the wire object; the frame
/// fields are the protocol echoes (`v`, `op`, `job_id`).
#[derive(Deserialize)]
pub(crate) struct WireResponse {
    pub(crate) v: u32,
    pub(crate) op: String,
    pub(crate) job_id: String,
    #[serde(flatten)]
    pub(crate) reply: KiwiReply,
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_core::{CancellationToken, OpId, SessionId, WorkspaceId};

    fn request() -> VerificationRequest {
        VerificationRequest::new(
            OpId::new(7),
            SessionId::new(3),
            WorkspaceId::new(1),
            1_000,
            CancellationToken::new(),
            VerificationOp::VerifyClaim,
        )
        .with_claim("replayed refresh tokens fail")
    }

    #[test]
    fn op_only_job_states_required_behavior_and_strips_prior_state() {
        let req = request();
        let job = KiwiJob::for_request(&req).unwrap();
        assert_eq!(job.job_id, "v1:7-3-verify_claim");
        assert_eq!(job.op, VerificationOp::VerifyClaim);
        assert_eq!(job.workspace_id, WorkspaceId::new(1));
        assert_eq!(job.session_id, SessionId::new(3));
        assert!(job.objective.contains("replayed refresh tokens fail"));
        assert!(job
            .acceptance_criteria
            .iter()
            .any(|c| c.contains("replayed refresh tokens fail")));
        assert_eq!(job.proof_obligations.len(), 1);
        let obligation = &job.proof_obligations[0];
        assert_eq!(obligation.claim, "replayed refresh tokens fail");
        assert_eq!(obligation.evidence, ProofEvidenceKind::Review);
        assert_eq!(obligation.state, ClaimState::Unknown);
        assert_eq!(obligation.confidence_bps, None);
        assert!(obligation.evidence_refs.is_empty());
        // The id is the canonical frozen-contract id.
        assert_eq!(
            obligation.id,
            EngineeringContract::obligation_id(
                "replayed refresh tokens fail",
                ProofEvidenceKind::Review
            )
        );
    }

    fn claim_request() -> faktor_verify::ClaimRequest {
        faktor_verify::ClaimRequest {
            obligation_id: EngineeringContract::obligation_id(
                "concurrent refresh produces one valid result",
                ProofEvidenceKind::TestExecution,
            ),
            claim: "concurrent refresh produces one valid result".to_string(),
            evidence_kind: ProofEvidenceKind::TestExecution,
            requirement: Requirement::Required,
            binding: faktor_core::EvidenceBinding::TestSelector("refresh::concurrent".to_string()),
            accepted_strength: faktor_core::AcceptedStrength::for_requirement(
                Requirement::Required,
            ),
        }
    }

    #[test]
    fn claim_batch_job_carries_exactly_the_neutral_claims() {
        let claim = claim_request();
        let req = request().with_claims(vec![claim.clone()]);
        let job = KiwiJob::for_claims(&req).unwrap();
        assert!(
            job.constraints.is_empty() && job.invariants.is_empty() && job.risks.is_empty(),
            "a production claim batch carries no contract context"
        );
        job.validate_claim_batch().unwrap();
        assert_eq!(job.proof_obligations.len(), 1);
        let obligation = &job.proof_obligations[0];
        assert_eq!(obligation.id, claim.obligation_id);
        assert_eq!(obligation.claim, claim.claim);
        assert_eq!(
            obligation.state,
            ClaimState::Unknown,
            "prior state must never be prescribed to Kiwi"
        );
        assert_eq!(obligation.confidence_bps, None);
        assert!(obligation.evidence_refs.is_empty());
        // The wire JSON has no expected-result concept: a round trip of the
        // serialized job is byte-enumerable and carries no such field.
        let json = serde_json::to_value(&job).unwrap();
        let object = json.as_object().unwrap();
        for forbidden in ["expected", "expected_result", "expect", "result"] {
            assert!(
                !object.contains_key(forbidden),
                "the job must not prescribe {forbidden:?}"
            );
        }
        assert!(!serde_json::to_string(&json).unwrap().contains("expected"));
    }

    #[test]
    fn readiness_markers_and_contract_context_are_refused() {
        // Ephemeral readiness text can never ride any submission surface,
        // even if a hostile caller constructs the job by hand.
        let mut job = KiwiJob::for_request(&request()).unwrap();
        job.objective = format!(
            "{}design-readiness: a local hypothesis",
            faktor_core::DESIGN_READINESS_DESCRIPTION_PREFIX
        );
        assert!(matches!(
            job.validate(),
            Err(VerificationError::Malformed(_))
        ));
        job.objective = "verify the claim".to_string();
        job.proof_obligations[0].claim = format!(
            "embedded {} marker",
            "dr1:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        );
        assert!(matches!(
            job.validate(),
            Err(VerificationError::Malformed(_))
        ));

        // A claim batch must not carry contract constraints/invariants/risks.
        let claim = claim_request();
        let req = request().with_claims(vec![claim]);
        let mut batch = KiwiJob::for_claims(&req).unwrap();
        batch.risks.push(RiskHypothesis {
            id: "risk-1".into(),
            description: "benign local context".into(),
            severity: faktor_core::RiskSeverity::Low,
            origin: ContractOrigin::Heuristic,
        });
        assert!(matches!(
            batch.validate_claim_batch(),
            Err(VerificationError::Malformed(_))
        ));
        batch.risks.clear();
        batch.invariants.push(Invariant {
            id: "inv-1".into(),
            statement: "local invariant".into(),
            origin: ContractOrigin::User,
        });
        assert!(matches!(
            batch.validate_claim_batch(),
            Err(VerificationError::Malformed(_))
        ));
    }

    #[test]
    fn hostile_jobs_and_replies_are_refused_typed() {
        let mut job = KiwiJob::for_request(&request()).unwrap();
        job.proof_obligations.clear();
        assert!(matches!(
            job.validate(),
            Err(VerificationError::Malformed(_))
        ));
        let mut job = KiwiJob::for_request(&request()).unwrap();
        job.objective = String::new();
        assert!(job.validate().is_err());
        let mut job = KiwiJob::for_request(&request()).unwrap();
        job.acceptance_criteria = vec!["x".repeat(MAX_KIWI_JOB_TEXT_BYTES + 1)];
        assert!(job.validate().is_err());
        let mut job = KiwiJob::for_request(&request()).unwrap();
        job.constraints = (0..=MAX_KIWI_CONTRACT_ENTRIES)
            .map(|i| Constraint {
                id: format!("c{i}"),
                statement: "s".into(),
                origin: ContractOrigin::User,
            })
            .collect();
        assert!(job.validate().is_err());
        let mut job = KiwiJob::for_request(&request()).unwrap();
        job.proof_obligations.push(job.proof_obligations[0].clone());
        assert!(
            job.validate().is_err(),
            "duplicate obligation ids must be refused"
        );

        let obligation_id = job.proof_obligations[0].id.clone();
        let mut reply = KiwiReply {
            outcome: VerificationOutcome::Failed,
            summary: "counterexample below".into(),
            claims: vec![KiwiEvidence {
                claim_id: "po1:not-asked".into(),
                state: ClaimState::Disproven,
                evidence_strength: None,
                evidence: Vec::new(),
                counterexample: Some("token replay succeeded".into()),
            }],
            evidence: Vec::new(),
        };
        assert!(reply.validate().is_ok());
        assert!(
            reply.validate_claims_for(&job).is_err(),
            "a claim the job never submitted must be refused"
        );
        reply.claims[0].claim_id = obligation_id;
        reply.validate_claims_for(&job).unwrap();
        reply.claims.push(reply.claims[0].clone());
        assert!(
            reply.validate_claims_for(&job).is_err(),
            "duplicate claim ids must be refused"
        );

        let mut disproof = KiwiEvidence {
            claim_id: "po1:x".into(),
            state: ClaimState::Disproven,
            evidence_strength: None,
            evidence: Vec::new(),
            counterexample: None,
        };
        assert!(
            disproof.validate().is_err(),
            "a disproof without any ground must be refused"
        );
        disproof.counterexample = Some("observed".into());
        disproof.validate().unwrap();
        disproof.state = ClaimState::Unknown;
        disproof.counterexample = Some(String::new());
        assert!(disproof.validate().is_err());

        // Oversized reply summary is refused, never truncated.
        let reply = KiwiReply {
            outcome: VerificationOutcome::Passed,
            summary: "x".repeat(MAX_KIWI_TEXT_BYTES + 1),
            claims: Vec::new(),
            evidence: Vec::new(),
        };
        assert!(matches!(
            reply.validate(),
            Err(VerificationError::Malformed(_))
        ));
    }

    #[test]
    fn op_round_trips_through_the_wire_names() {
        for op in VerificationOp::ALL {
            assert_eq!(VerificationOp::parse(op.as_str()).unwrap(), op);
            let json = serde_json::to_string(&op).unwrap();
            assert_eq!(json, format!("\"{}\"", op.as_str()));
        }
        assert!(matches!(
            VerificationOp::parse("melted"),
            Err(VerificationError::UnknownOp(_))
        ));
    }
}
