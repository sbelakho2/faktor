//! Evaluate the engineering contract's proof obligations against ONE
//! attempt's native verification evidence (`docs/specs/autonomous-engineering.md`).
//!
//! Rules (provider-agnostic, standalone):
//! * Compile obligations are discharged by Compile/Lint check outcomes;
//!   TestExecution by Test outcomes; Review by the review verdict.
//!   ContractCheck/RuntimeTrace obligations can only be discharged by an
//!   optional external verification provider — native evaluation leaves them
//!   untouched (Unknown), never fabricated.
//! * Native evidence yields `Supported` (8_000 bps) or `Disproven` (0 bps).
//!   `KnownTrue` is reserved for proof-grade external providers.
//! * An obligation with NO evidence in this attempt keeps its prior state
//!   (a previously supported claim is not erased because a later attempt did
//!   not re-run that evidence; a disproven claim stays disproven until
//!   passing evidence re-establishes it).
//! * Evidence references are appended and deduplicated by
//!   (kind, source, detail).

use faktor_core::state::{OutcomeReason, ReasonCode};
use faktor_core::{
    ClaimState, ContractDisclosure, EngineeringContract, EvidenceRef, ProofEvidenceKind,
    ProofObligation,
};
use faktor_session::{SessionHandle, TaskError};

use super::{CompletionGate, ExecutedCheck};

/// Outcome of evaluating + persisting the contract at a gate site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ContractGateOutcome {
    /// Evaluated (possibly with zero changes); disproven obligations block a
    /// passing gate.
    Evaluated {
        updates_applied: usize,
        disproven: Vec<ContractDisclosure>,
    },
    /// The durable task row or its contract could not be read: a PASSING gate
    /// must not stand on an unevaluated contract (authority law).
    Unavailable(String),
    /// The evaluated states could not be persisted: the passing gate must
    /// not stand on an unauditable evaluation.
    PersistFailed(String),
    /// The task row is already terminal and frozen by design: nothing to
    /// update, and this attempt is not the row's completion authority.
    TerminalFrozen,
}

/// Longest evidence detail recorded on a ref (the session validator bounds
/// strings at 512 bytes; stay well under).
const MAX_EVIDENCE_DETAIL: usize = 200;

fn bounded_detail(text: &str) -> String {
    if text.len() <= MAX_EVIDENCE_DETAIL {
        return text.to_string();
    }
    let mut end = MAX_EVIDENCE_DETAIL;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// Apply a contract outcome to a completion gate at a gate site:
/// * a disproven obligation blocks a passing gate (and is appended to an
///   existing blocker) with [`ReasonCode::ProofObligationDisproven`];
/// * an unevaluable contract blocks a passing gate with
///   [`ReasonCode::CheckUnavailable`];
/// * already-blocked/failed/pending gates keep their precedence beyond the
///   appended reason.
pub(crate) fn apply_contract_outcome(
    completion: CompletionGate,
    outcome: ContractGateOutcome,
) -> CompletionGate {
    match outcome {
        ContractGateOutcome::Evaluated { disproven, .. } if !disproven.is_empty() => {
            let detail = format!(
                "disproven proof obligations: {}",
                disproven
                    .iter()
                    .map(|d| d.summary.clone())
                    .collect::<Vec<_>>()
                    .join("; ")
            );
            match completion {
                CompletionGate::BlockedVerification { mut reasons } => {
                    reasons.push(OutcomeReason::new(
                        ReasonCode::ProofObligationDisproven,
                        detail,
                    ));
                    CompletionGate::BlockedVerification { reasons }
                }
                CompletionGate::VerifiedComplete => CompletionGate::BlockedVerification {
                    reasons: vec![OutcomeReason::new(
                        ReasonCode::ProofObligationDisproven,
                        detail,
                    )],
                },
                other => other,
            }
        }
        ContractGateOutcome::Unavailable(detail) | ContractGateOutcome::PersistFailed(detail) => {
            if matches!(completion, CompletionGate::VerifiedComplete) {
                CompletionGate::BlockedVerification {
                    reasons: vec![OutcomeReason::new(
                        ReasonCode::CheckUnavailable,
                        format!("engineering contract not evaluated: {detail}"),
                    )],
                }
            } else {
                completion
            }
        }
        ContractGateOutcome::Evaluated { .. } | ContractGateOutcome::TerminalFrozen => completion,
    }
}

/// Evaluate the session's engineering contract against this attempt's
/// executed checks and (optional) review verdict, and persist the evaluated
/// obligation states. Infallible by construction: every failure is a typed
/// outcome the gate site can act on.
pub(crate) async fn evaluate_and_persist_contract(
    handle: &SessionHandle,
    executed: &[ExecutedCheck],
    review: Option<&serde_json::Value>,
) -> ContractGateOutcome {
    let task_id = match handle.task_id() {
        Ok(task_id) => task_id,
        Err(e) => return ContractGateOutcome::Unavailable(format!("task id unreadable: {e}")),
    };
    let task = match handle.get_task(task_id) {
        Ok(Some(task)) => task,
        Ok(None) => {
            return ContractGateOutcome::Unavailable(
                "durable task row is missing; the engineering contract cannot be evaluated".into(),
            )
        }
        Err(e) => {
            return ContractGateOutcome::Unavailable(format!("durable task row unreadable: {e}"))
        }
    };
    if task.contract.proof_obligations.is_empty() {
        return ContractGateOutcome::Evaluated {
            updates_applied: 0,
            disproven: Vec::new(),
        };
    }
    if task.state.is_terminal() {
        return ContractGateOutcome::TerminalFrozen;
    }
    let evidence: Vec<CheckEvidence> = executed
        .iter()
        .map(|run| CheckEvidence {
            kind: run.kind,
            id: run.id.clone(),
            passed: run.passed,
            detail: bounded_detail(&format!("{} {}", run.program, run.args.join(" "))),
        })
        .collect();
    // A review verdict of `clean` supports Review obligations; any other
    // parsed verdict disproves them; an absent/unparseable verdict is no
    // evidence (never a fabricated clean).
    let review_clean = review
        .and_then(|value| value.get("verdict").and_then(|v| v.as_str()))
        .map(|verdict| verdict == "clean");
    let evaluation = evaluate_contract(&task.contract, &evidence, review_clean);
    if evaluation.updates.is_empty() {
        return ContractGateOutcome::Evaluated {
            updates_applied: 0,
            disproven: evaluation.disproven,
        };
    }
    match handle.apply_task_contract_obligation_states(task_id, &evaluation.updates) {
        Ok(_) => ContractGateOutcome::Evaluated {
            updates_applied: evaluation.updates.len(),
            disproven: evaluation.disproven,
        },
        Err(TaskError::TerminalTask { .. }) => ContractGateOutcome::TerminalFrozen,
        Err(e) => ContractGateOutcome::PersistFailed(format!(
            "contract obligation states could not be persisted: {e}"
        )),
    }
}

/// Confidence when native evidence supports a claim.
pub(crate) const SUPPORTED_CONFIDENCE_BPS: u16 = 8_000;
/// Confidence when evidence disproves a claim.
pub(crate) const DISPROVEN_CONFIDENCE_BPS: u16 = 0;

/// One executed check outcome projected for obligation evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CheckEvidence {
    pub(crate) kind: faktor_verify::CheckKind,
    pub(crate) id: String,
    pub(crate) passed: bool,
    /// Bounded human detail (program + argv summary).
    pub(crate) detail: String,
}

/// The result of evaluating one attempt's evidence against a contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContractEvaluation {
    /// The contract with evaluated states/refs applied.
    pub(crate) contract: EngineeringContract,
    /// Only the obligations whose state/confidence/refs changed (the update
    /// payload for the durable row; empty = write nothing).
    pub(crate) updates: Vec<ProofObligation>,
    /// The disproven obligations (completion must be blocked).
    pub(crate) disproven: Vec<ContractDisclosure>,
}

/// Which proof evidence kind a native check kind can discharge.
pub(crate) fn evidence_kind_for_check(check: faktor_verify::CheckKind) -> ProofEvidenceKind {
    match check {
        faktor_verify::CheckKind::Compile | faktor_verify::CheckKind::Lint => {
            ProofEvidenceKind::Compile
        }
        faktor_verify::CheckKind::Test => ProofEvidenceKind::TestExecution,
    }
}

/// Evaluate `contract` against this attempt's check evidence and review
/// verdict. `review_clean` is `Some(true/false)` only when a review verdict
/// existed this attempt.
pub(crate) fn evaluate_contract(
    contract: &EngineeringContract,
    checks: &[CheckEvidence],
    review_clean: Option<bool>,
) -> ContractEvaluation {
    let mut updated = contract.clone();
    let mut updates = Vec::new();
    for obligation in updated.proof_obligations.iter_mut() {
        let (state, confidence, refs): (ClaimState, u16, Vec<EvidenceRef>) =
            match obligation.evidence {
                ProofEvidenceKind::Review => match review_clean {
                    Some(true) => (
                        ClaimState::Supported,
                        SUPPORTED_CONFIDENCE_BPS,
                        vec![EvidenceRef {
                            kind: ProofEvidenceKind::Review,
                            source: "independent_review".to_string(),
                            detail: "review verdict: clean".to_string(),
                        }],
                    ),
                    Some(false) => (
                        ClaimState::Disproven,
                        DISPROVEN_CONFIDENCE_BPS,
                        vec![EvidenceRef {
                            kind: ProofEvidenceKind::Review,
                            source: "independent_review".to_string(),
                            detail: "review verdict: blocking or concerning".to_string(),
                        }],
                    ),
                    // No review this attempt: keep the prior state.
                    None => continue,
                },
                // External-only evidence kinds: native evaluation never
                // fabricates a state for them.
                ProofEvidenceKind::ContractCheck | ProofEvidenceKind::RuntimeTrace => continue,
                ProofEvidenceKind::Compile | ProofEvidenceKind::TestExecution => {
                    let matching: Vec<&CheckEvidence> = checks
                        .iter()
                        .filter(|c| evidence_kind_for_check(c.kind) == obligation.evidence)
                        .collect();
                    if matching.is_empty() {
                        continue;
                    }
                    let failed = matching.iter().any(|c| !c.passed);
                    let (state, confidence) = if failed {
                        (ClaimState::Disproven, DISPROVEN_CONFIDENCE_BPS)
                    } else {
                        (ClaimState::Supported, SUPPORTED_CONFIDENCE_BPS)
                    };
                    let refs = matching
                        .iter()
                        .map(|c| EvidenceRef {
                            kind: obligation.evidence,
                            source: c.id.clone(),
                            detail: c.detail.clone(),
                        })
                        .collect();
                    (state, confidence, refs)
                }
            };
        let mut changed =
            obligation.state != state || obligation.confidence_bps != Some(confidence);
        for reference in refs {
            if !obligation.evidence_refs.contains(&reference) {
                obligation.evidence_refs.push(reference);
                changed = true;
            }
        }
        if changed {
            obligation.state = state;
            obligation.confidence_bps = Some(confidence);
            updates.push(obligation.clone());
        }
    }
    let disproven = updates
        .iter()
        .filter(|obligation| obligation.state == ClaimState::Disproven)
        .map(|obligation| ContractDisclosure {
            id: obligation.id.clone(),
            summary: obligation.claim.clone(),
            state: obligation.state.as_str().to_string(),
        })
        .collect();
    ContractEvaluation {
        contract: updated,
        updates,
        disproven,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_core::ContractOrigin;

    fn check(kind: faktor_verify::CheckKind, id: &str, passed: bool) -> CheckEvidence {
        CheckEvidence {
            kind,
            id: id.to_string(),
            passed,
            detail: format!("{id} detail"),
        }
    }

    fn contract_with(obligations: Vec<ProofObligation>) -> EngineeringContract {
        EngineeringContract {
            proof_obligations: obligations,
            ..Default::default()
        }
    }

    #[test]
    fn compile_and_test_evidence_support_and_disprove() {
        let contract = contract_with(vec![
            EngineeringContract::obligation(
                "the workspace compiles",
                ProofEvidenceKind::Compile,
                ContractOrigin::User,
            ),
            EngineeringContract::obligation(
                "all tests pass",
                ProofEvidenceKind::TestExecution,
                ContractOrigin::User,
            ),
        ]);
        let evaluation = evaluate_contract(
            &contract,
            &[
                check(faktor_verify::CheckKind::Compile, "cargo-check", true),
                check(faktor_verify::CheckKind::Test, "cargo-test", false),
            ],
            None,
        );
        assert_eq!(evaluation.updates.len(), 2);
        let compile = &evaluation.contract.proof_obligations[0];
        assert_eq!(compile.state, ClaimState::Supported);
        assert_eq!(compile.confidence_bps, Some(SUPPORTED_CONFIDENCE_BPS));
        assert_eq!(compile.evidence_refs.len(), 1);
        let tests = &evaluation.contract.proof_obligations[1];
        assert_eq!(tests.state, ClaimState::Disproven);
        assert_eq!(tests.confidence_bps, Some(DISPROVEN_CONFIDENCE_BPS));
        assert_eq!(evaluation.disproven.len(), 1);
        assert_eq!(evaluation.disproven[0].summary, "all tests pass");
    }

    #[test]
    fn lint_discharges_compile_obligations_and_missing_evidence_keeps_state() {
        let mut previously = EngineeringContract::obligation(
            "no new lint warnings",
            ProofEvidenceKind::Compile,
            ContractOrigin::User,
        );
        previously.state = ClaimState::Supported;
        previously.confidence_bps = Some(SUPPORTED_CONFIDENCE_BPS);
        let contract = contract_with(vec![previously.clone()]);
        // No checks at all: nothing changes.
        let evaluation = evaluate_contract(&contract, &[], None);
        assert!(evaluation.updates.is_empty());
        assert_eq!(
            evaluation.contract.proof_obligations[0], previously,
            "no evidence must not erase a supported state"
        );
        // Lint evidence discharges a Compile obligation.
        let evaluation = evaluate_contract(
            &contract,
            &[check(faktor_verify::CheckKind::Lint, "clippy", true)],
            None,
        );
        assert_eq!(evaluation.updates.len(), 1);
        assert_eq!(
            evaluation.contract.proof_obligations[0].state,
            ClaimState::Supported
        );
    }

    #[test]
    fn external_only_kinds_and_review_semantics_are_honest() {
        let contract = contract_with(vec![
            EngineeringContract::obligation(
                "the API contract is preserved",
                ProofEvidenceKind::ContractCheck,
                ContractOrigin::User,
            ),
            EngineeringContract::obligation(
                "review is clean",
                ProofEvidenceKind::Review,
                ContractOrigin::User,
            ),
        ]);
        // Native checks cannot fabricate ContractCheck evidence, and no
        // review ran: nothing changes.
        let evaluation = evaluate_contract(
            &contract,
            &[check(faktor_verify::CheckKind::Test, "cargo-test", true)],
            None,
        );
        assert!(evaluation.updates.is_empty());
        // A blocking review disproves the review obligation.
        let evaluation = evaluate_contract(&contract, &[], Some(false));
        assert_eq!(evaluation.updates.len(), 1);
        assert_eq!(
            evaluation.contract.proof_obligations[1].state,
            ClaimState::Disproven
        );
        // A clean review supports it; unrelated obligations stay untouched.
        let evaluation = evaluate_contract(&contract, &[], Some(true));
        assert_eq!(evaluation.updates.len(), 1);
        assert_eq!(
            evaluation.contract.proof_obligations[1].state,
            ClaimState::Supported
        );
        assert_eq!(
            evaluation.contract.proof_obligations[0].state,
            ClaimState::Unknown
        );
    }

    #[test]
    fn evidence_refs_dedupe_and_disproven_can_be_reestablished() {
        let mut obligation = EngineeringContract::obligation(
            "all tests pass",
            ProofEvidenceKind::TestExecution,
            ContractOrigin::User,
        );
        obligation.state = ClaimState::Disproven;
        obligation.confidence_bps = Some(DISPROVEN_CONFIDENCE_BPS);
        obligation.evidence_refs.push(EvidenceRef {
            kind: ProofEvidenceKind::TestExecution,
            source: "cargo-test".into(),
            detail: "cargo-test detail".into(),
        });
        let contract = contract_with(vec![obligation]);
        // The same passed evidence twice: one update, no duplicate ref.
        let evaluation = evaluate_contract(
            &contract,
            &[
                check(faktor_verify::CheckKind::Test, "cargo-test", true),
                check(faktor_verify::CheckKind::Test, "cargo-test", true),
            ],
            None,
        );
        assert_eq!(evaluation.updates.len(), 1);
        assert_eq!(
            evaluation.contract.proof_obligations[0].state,
            ClaimState::Supported,
            "passing evidence re-establishes a disproven claim"
        );
        assert_eq!(
            evaluation.contract.proof_obligations[0].evidence_refs.len(),
            1,
            "identical refs are deduplicated"
        );
        // Re-evaluating the same evidence is idempotent (no update).
        let evaluation = evaluate_contract(
            &evaluation.contract,
            &[check(faktor_verify::CheckKind::Test, "cargo-test", true)],
            None,
        );
        assert!(evaluation.updates.is_empty());
    }
}
