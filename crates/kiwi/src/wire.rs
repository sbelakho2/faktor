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
//! - The submission is assembled ONLY from the frozen contract types and
//!   never carries an expected result: prior obligation state, confidence and
//!   evidence refs are stripped in [`KiwiJob::for_contract`] /
//!   [`KiwiJob::for_request`].
//! - Every string, list and claim id is bounded; a job or reply that exceeds
//!   a bound is a typed refusal, never silently cut by the wire layer.
//! - Evidence must reference obligation ids the job actually submitted;
//!   unknown or duplicated claim ids are [`VerificationError::Malformed`].

use std::collections::BTreeSet;

use faktor_core::{
    ClaimState, Constraint, ContractOrigin, EngineeringContract, Invariant, ProofEvidenceKind,
    ProofObligation, RiskHypothesis, SessionId, WorkspaceId,
};
use faktor_verify::{
    ArtifactRef, VerificationError, VerificationOp, VerificationOutcome, VerificationRequest,
    VerificationTarget,
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

    /// The full job for a caller that holds the engineering contract: the
    /// objective and acceptance criteria are the caller's, the contract's
    /// constraints/invariants/risks/obligations ride verbatim EXCEPT that
    /// every obligation's prior evaluation state, confidence and evidence
    /// refs are stripped — Faktor asks a question, it does not prescribe the
    /// answer.
    pub fn for_contract(
        request: &VerificationRequest,
        objective: impl Into<String>,
        acceptance_criteria: Vec<String>,
        contract: &EngineeringContract,
    ) -> Result<Self, VerificationError> {
        let mut obligations = sanitize_obligations(&contract.proof_obligations);
        if obligations.is_empty() {
            let claim = derived_claim(request.op, request.claim.as_deref());
            obligations.push(EngineeringContract::obligation(
                &claim,
                evidence_kind_for(request.op),
                ContractOrigin::Policy,
            ));
        }
        let job = Self {
            job_id: job_id_for(request),
            op: request.op,
            workspace_id: request.workspace_id,
            session_id: request.session_id,
            objective: objective.into(),
            acceptance_criteria,
            constraints: contract.constraints.clone(),
            invariants: contract.invariants.clone(),
            risks: contract.risks.clone(),
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

    /// Strict bounds. Every failure is typed; nothing is silently cut.
    pub fn validate(&self) -> Result<(), VerificationError> {
        validate_id("job_id", &self.job_id)?;
        validate_text("objective", &self.objective, true, MAX_KIWI_JOB_TEXT_BYTES)?;
        if self.acceptance_criteria.len() > MAX_KIWI_ACCEPTANCE_CRITERIA {
            return Err(VerificationError::Malformed(format!(
                "job carries {} acceptance criteria, over the {MAX_KIWI_ACCEPTANCE_CRITERIA} bound",
                self.acceptance_criteria.len()
            )));
        }
        for (index, criterion) in self.acceptance_criteria.iter().enumerate() {
            validate_text(
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
            validate_text("constraint.id", &constraint.id, true, MAX_KIWI_ID_BYTES)?;
            validate_text(
                "constraint.statement",
                &constraint.statement,
                true,
                MAX_KIWI_JOB_TEXT_BYTES,
            )?;
        }
        for invariant in &self.invariants {
            validate_text("invariant.id", &invariant.id, true, MAX_KIWI_ID_BYTES)?;
            validate_text(
                "invariant.statement",
                &invariant.statement,
                true,
                MAX_KIWI_JOB_TEXT_BYTES,
            )?;
        }
        for risk in &self.risks {
            validate_text("risk.id", &risk.id, true, MAX_KIWI_ID_BYTES)?;
            validate_text(
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
            validate_id("obligation.id", &obligation.id)?;
            validate_text(
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
            validate_text("claim", claim, true, MAX_KIWI_JOB_TEXT_BYTES)?;
        }
        Ok(())
    }
}

/// One claim verdict from the external verifier: evidence and/or a
/// counterexample per claim id. Strictly bounded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KiwiEvidence {
    pub claim_id: String,
    /// The evaluated state, using the frozen contract taxonomy.
    pub state: ClaimState,
    #[serde(default)]
    pub evidence: Vec<ArtifactRef>,
    #[serde(default)]
    pub counterexample: Option<String>,
}

impl KiwiEvidence {
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
        if self.state == ClaimState::Disproven
            && self.counterexample.is_none()
            && self.evidence.is_empty()
        {
            return Err(VerificationError::Malformed(format!(
                "claim {} is disproven without a counterexample or evidence",
                self.claim_id
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

    /// Every claim id must be an obligation the job submitted, exactly once.
    /// Evidence for a claim Faktor never asked about is refused, never
    /// accepted into the registry's response.
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
        Ok(())
    }
}

/// Strip prior evaluation state from contract obligations: Faktor submits
/// REQUIRED BEHAVIOR, never a claimed prior result.
fn sanitize_obligations(obligations: &[ProofObligation]) -> Vec<ProofObligation> {
    obligations
        .iter()
        .map(|obligation| ProofObligation {
            id: obligation.id.clone(),
            claim: obligation.claim.clone(),
            evidence: obligation.evidence,
            origin: obligation.origin.clone(),
            state: ClaimState::Unknown,
            confidence_bps: None,
            evidence_refs: Vec::new(),
        })
        .collect()
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

    #[test]
    fn contract_job_carries_every_contract_surface_without_expected_results() {
        let mut contract = EngineeringContract {
            constraints: vec![Constraint {
                id: "c1".into(),
                statement: "existing sessions remain valid".into(),
                origin: ContractOrigin::User,
            }],
            invariants: vec![Invariant {
                id: "i1".into(),
                statement: "revoked sessions cannot authenticate".into(),
                origin: ContractOrigin::User,
            }],
            risks: vec![RiskHypothesis {
                id: "r1".into(),
                description: "race conditions".into(),
                severity: faktor_core::RiskSeverity::High,
                origin: ContractOrigin::Heuristic,
            }],
            proof_obligations: Vec::new(),
        };
        let mut satisfied = EngineeringContract::obligation(
            "concurrent refresh produces one valid result",
            ProofEvidenceKind::TestExecution,
            ContractOrigin::User,
        );
        satisfied.state = ClaimState::KnownTrue;
        satisfied.confidence_bps = Some(9_000);
        satisfied.evidence_refs.push(faktor_core::EvidenceRef {
            kind: ProofEvidenceKind::TestExecution,
            source: "cargo test".into(),
            detail: "passed".into(),
        });
        contract.proof_obligations.push(satisfied);
        let job = KiwiJob::for_contract(
            &request(),
            "migrate authentication to rotating refresh tokens",
            vec!["existing sessions remain valid".to_string()],
            &contract,
        )
        .unwrap();
        assert_eq!(job.constraints.len(), 1);
        assert_eq!(job.invariants.len(), 1);
        assert_eq!(job.risks.len(), 1);
        assert_eq!(job.proof_obligations.len(), 1);
        let obligation = &job.proof_obligations[0];
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
