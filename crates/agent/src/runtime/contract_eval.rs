//! @authority-collapse: authority-bearing
//!
//! Evaluate the engineering contract's proof obligations against ONE
//! attempt's verification evidence (`docs/specs/autonomous-engineering.md`),
//! with CLAIM-BOUND and SNAPSHOT-BOUND proof semantics.
//!
//! Laws enforced here (audit items 2-4):
//! * **Claim-bound evidence**: an obligation is discharged or disproved only
//!   by evidence whose [`EvidenceBinding`] intersects the obligation's
//!   binding ([`ProofObligation::evidence_binding`]). A check id, test
//!   selector, contract id, property id, runtime assertion, trace property
//!   or provider claim id that the claim never named is structurally
//!   incapable of discharging — or disproving — it. The explicit legacy
//!   [`EvidenceBinding::Category`] keeps the pre-binding semantics for rows
//!   that predate bindings; the contract compiler never emits it.
//! * **Snapshot-bound proof**: before any fresh evidence is applied, a
//!   claim-grade state whose recorded [`ProofBasis`] does not demonstrably
//!   survive into the current candidate degrades to `Unknown`. A fresh proof
//!   is recorded per `(claim, candidate snapshot, basis)`.
//! * **Requirements**: a `Required` obligation that is
//!   Unknown/Inferred/Uncertain (or a hard constraint/invariant with no
//!   bound evidence) refuses a passing gate with a typed NeedsVerification;
//!   `Advisory` uncertainty is disclosure-only; a disproof always blocks.
//! * **Executable contract**: violated constraints (and violated invariant
//!   preservation checks) block completion; unevaluated hard constraints and
//!   invariants force their named check; a counterexample (or a disproven
//!   heuristic assumption) re-opens work through a typed replan cause; high
//!   or unknown risk raises the verification depth / routing quality floor;
//!   unresolved required proof escalates intelligence/retrieval.
//!
//! Native evidence yields `Supported` (8_000 bps) or `Disproven` (0 bps).
//! `KnownTrue` is reserved for proof-grade external providers.
//! Evidence references are appended and deduplicated by (kind, source,
//! detail); identical re-evaluation is idempotent (no update).

use std::collections::BTreeMap;

use faktor_core::state::{OutcomeReason, ReasonCode};
use faktor_core::{
    CancellationToken, ClaimState, ContractDisclosure, EngineeringContract, EvidenceBinding,
    EvidenceRef, OpId, ProofBasis, ProofEvidenceKind, ProofEvidenceRef, ProofObligation,
    ProofRecord, ProofScope, Requirement, SubjectSnapshot, VerificationDepth,
};
use faktor_session::{SessionHandle, TaskError};
use faktor_verify::{
    op_for_claim, ClaimRequest, ClaimVerificationResult, VerificationError, VerificationRegistry,
    VerificationRequest,
};

use super::{CompletionGate, ExecutedCheck};

/// Outcome of evaluating + persisting the contract at a gate site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ContractGateOutcome {
    /// Evaluated (possibly with zero changes). `disproven` blocks a passing
    /// gate; `unresolved_required`/`constraints.unresolved` downgrade it to
    /// the typed NeedsVerification; `constraints.violated` blocks it;
    /// `advisory_uncertain` is disclosure-only; `replan` re-opens work.
    Evaluated {
        updates_applied: usize,
        disproven: Vec<ContractDisclosure>,
        unresolved_required: Vec<ContractDisclosure>,
        advisory_uncertain: Vec<ContractDisclosure>,
        constraints: ConstraintGate,
        replan: Vec<ReplanCause>,
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

/// The constraint/invariant preservation gate of one evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ConstraintGate {
    /// Violated constraints and invariants (bound evidence observed FALSE):
    /// completion must be refused.
    pub(crate) violated: Vec<ContractDisclosure>,
    /// Hard constraints and invariants with NO bound evidence this attempt:
    /// the named check is forced before completion may stand.
    pub(crate) unresolved: Vec<PreservationCheck>,
}

/// One preservation/predicate check the contract demands: the statement id,
/// its text, and the exact evidence binding a check must exercise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PreservationCheck {
    pub(crate) id: String,
    pub(crate) statement: String,
    pub(crate) evidence_kind: ProofEvidenceKind,
    pub(crate) binding: EvidenceBinding,
}

/// A counterexample (or an invalidated heuristic assumption) that re-opens
/// work: the drive layer records a durable replan decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReplanCause {
    pub(crate) obligation_id: String,
    pub(crate) claim: String,
    pub(crate) counterexample: Option<String>,
    pub(crate) detail: String,
}

/// High-risk verification policy (audit item 4): the depth, quality floor
/// and escalation the contract demands for the current candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContractVerificationPolicy {
    pub(crate) depth: VerificationDepth,
    /// Raise the end-of-turn verification/review quality to Strict.
    pub(crate) quality_strict: bool,
    /// The hard routing quality floor (60 default; higher under deep risk).
    pub(crate) routing_quality_floor: u8,
    /// The semantic-risk floor fed to the router consult.
    pub(crate) semantic_risk_floor: u8,
    /// Unresolved required proof escalates the intelligence tier.
    pub(crate) intelligence_escalation: bool,
    /// Unresolved required proof escalates retrieval.
    pub(crate) retrieval_escalation: bool,
}

impl ContractVerificationPolicy {
    /// The neutral policy of an empty contract (parity: every floor keeps
    /// today's behavior).
    pub(crate) const NEUTRAL: Self = Self {
        depth: VerificationDepth::Standard,
        quality_strict: false,
        routing_quality_floor: 60,
        semantic_risk_floor: 10,
        intelligence_escalation: false,
        retrieval_escalation: false,
    };
}

/// Longest evidence detail recorded on a ref (the session validator bounds
/// strings at 512 bytes; stay well under).
const MAX_EVIDENCE_DETAIL: usize = 200;
/// Longest preserved counterexample text (bounded, deterministic).
const MAX_COUNTEREXAMPLE: usize = 200;

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

fn bounded_counterexample(text: &str) -> String {
    if text.len() <= MAX_COUNTEREXAMPLE {
        return text.to_string();
    }
    let mut end = MAX_COUNTEREXAMPLE;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// Apply a contract outcome to a completion gate at a gate site:
/// * a disproven obligation blocks a passing gate (and is appended to an
///   existing blocker) with [`ReasonCode::ProofObligationDisproven`];
/// * a violated constraint/invariant blocks it with [`ReasonCode::CheckFailed`];
/// * a required-unresolved obligation or an unevaluated hard constraint
///   downgrades ONLY a passing gate to the typed [`CompletionGate::Unverified`]
///   (the machine's `NeedsVerification` landing) — never silent;
/// * an unevaluable contract blocks a passing gate with
///   [`ReasonCode::CheckUnavailable`];
/// * already-blocked/failed/pending gates keep their precedence beyond the
///   appended reason.
pub(crate) fn apply_contract_outcome(
    completion: CompletionGate,
    outcome: ContractGateOutcome,
) -> CompletionGate {
    match outcome {
        ContractGateOutcome::Evaluated {
            disproven,
            unresolved_required,
            constraints,
            ..
        } => {
            if !disproven.is_empty() {
                let detail = format!(
                    "disproven proof obligations: {}",
                    disproven
                        .iter()
                        .map(|d| d.summary.clone())
                        .collect::<Vec<_>>()
                        .join("; ")
                );
                return push_block(completion, ReasonCode::ProofObligationDisproven, detail);
            }
            if !constraints.violated.is_empty() {
                let detail = format!(
                    "violated contract constraints: {}",
                    constraints
                        .violated
                        .iter()
                        .map(|d| d.summary.clone())
                        .collect::<Vec<_>>()
                        .join("; ")
                );
                return push_block(completion, ReasonCode::CheckFailed, detail);
            }
            if !unresolved_required.is_empty() || !constraints.unresolved.is_empty() {
                return match completion {
                    CompletionGate::VerifiedComplete => CompletionGate::Unverified,
                    other => other,
                };
            }
            completion
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
        ContractGateOutcome::TerminalFrozen => completion,
    }
}

fn push_block(completion: CompletionGate, code: ReasonCode, detail: String) -> CompletionGate {
    match completion {
        CompletionGate::BlockedVerification { mut reasons } => {
            reasons.push(OutcomeReason::new(code, detail));
            CompletionGate::BlockedVerification { reasons }
        }
        CompletionGate::VerifiedComplete => CompletionGate::BlockedVerification {
            reasons: vec![OutcomeReason::new(code, detail)],
        },
        other => other,
    }
}

/// The durable rationale for a replan, when this outcome carries one (the
/// drive/settlement layers record it as a durable ledger decision).
pub(crate) fn replan_detail(outcome: &ContractGateOutcome) -> Option<String> {
    let ContractGateOutcome::Evaluated { replan, .. } = outcome else {
        return None;
    };
    if replan.is_empty() {
        return None;
    }
    Some(
        replan
            .iter()
            .map(|cause| cause.detail.clone())
            .collect::<Vec<_>>()
            .join("; "),
    )
}

/// The durable rationale for refusing completion pending verification, when
/// this outcome carries one (required-unresolved obligations or unevaluated
/// hard constraints/invariants forcing their checks).
pub(crate) fn needs_verification_detail(outcome: &ContractGateOutcome) -> Option<String> {
    let ContractGateOutcome::Evaluated {
        unresolved_required,
        constraints,
        ..
    } = outcome
    else {
        return None;
    };
    let mut parts: Vec<String> = unresolved_required
        .iter()
        .map(|disclosure| format!("required proof unresolved: {}", disclosure.summary))
        .collect();
    parts.extend(constraints.unresolved.iter().map(|check| {
        format!(
            "forced preservation check: {} ({})",
            check.id, check.statement
        )
    }));
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("; "))
    }
}

/// The bounded context of one contract→verify provider dispatch (audit item
/// 6: the verification registry is the completion spine). The runtime
/// installs its registry through `AgentRuntime::set_verification_providers`;
/// this envelope carries the turn's operation identity, cancellation and the
/// registry itself.
pub(crate) struct ContractProviderDispatch<'a> {
    pub(crate) registry: &'a VerificationRegistry,
    pub(crate) op_id: OpId,
    pub(crate) cancellation: CancellationToken,
}

/// Why a provider claim dispatch could not produce evidence.
enum ProviderDispatchFailure {
    /// No capable provider / provider unreachable: honest degrade — the
    /// unresolved claims stay unresolved (the gate still requires them).
    Recoverable(String),
    /// The provider answered data that violates the neutral contract
    /// (silently omitted required claim, fabricated ids, unevidenced
    /// positive verdict, oversized payload): a passing gate must never
    /// stand on it.
    Refused(String),
}

/// Dispatch every UNRESOLVED contract claim whose routed operation an
/// EXTERNAL provider serves through the neutral registry (the completion
/// spine) and project the per-claim verdicts into contract evidence.
///
/// Bounded and parity-preserving by construction:
/// - a registry with no external provider (the default daemon, including
///   native-only) dispatches nothing and returns no evidence;
/// - only unresolved obligations are asked (already-decided claims are not
///   re-verified, so a configured external provider never duplicates the
///   native executed-check path);
/// - a provider verdict becomes evidence bound to the obligation's own
///   binding (the provider was asked exactly that question), tagged with the
///   provider identity for the proof record.
async fn dispatch_provider_claims(
    handle: &SessionHandle,
    contract: &EngineeringContract,
    candidate: Option<&SubjectSnapshot>,
    dispatch: &ContractProviderDispatch<'_>,
) -> Result<Vec<ContractEvidence>, ProviderDispatchFailure> {
    let requests: Vec<ClaimRequest> = contract
        .proof_obligations
        .iter()
        .filter(|obligation| obligation.is_unresolved())
        .map(ClaimRequest::from_obligation)
        .filter(|claim| {
            dispatch
                .registry
                .has_external_provider_for(op_for_claim(claim))
        })
        .collect();
    if requests.is_empty() {
        return Ok(Vec::new());
    }
    let row = handle.row().map_err(|error| {
        ProviderDispatchFailure::Recoverable(format!(
            "session row unreadable before claim dispatch: {error}"
        ))
    })?;
    let mut request = VerificationRequest::new(
        dispatch.op_id,
        handle.id(),
        row.workspace_id,
        faktor_verify::now_epoch_ms(),
        dispatch.cancellation.clone(),
        op_for_claim(&requests[0]),
    )
    .with_claims(requests.clone());
    if let Some(snapshot) = candidate {
        request = request.with_subject_snapshot(snapshot.clone());
    }
    let results = dispatch
        .registry
        .verify_claims(request, requests)
        .await
        .map_err(classify_dispatch_failure)?;
    let obligations: BTreeMap<&str, &ProofObligation> = contract
        .proof_obligations
        .iter()
        .map(|obligation| (obligation.id.as_str(), obligation))
        .collect();
    let mut evidence = Vec::new();
    for result in &results {
        // An explicit Unknown is an honest no-verdict: no evidence exists to
        // present, and none is fabricated.
        if result.state == ClaimState::Unknown {
            continue;
        }
        let Some(obligation) = obligations.get(result.obligation_id.as_str()) else {
            continue;
        };
        evidence.push(provider_claim_evidence(obligation, result, candidate));
    }
    Ok(evidence)
}

fn classify_dispatch_failure(error: VerificationError) -> ProviderDispatchFailure {
    match error {
        // No capable provider, provider down, caller deadline: honest
        // degrade — the unresolved claims simply stay unresolved and the
        // existing gate refuses completion until real evidence exists.
        VerificationError::Unavailable(detail) => ProviderDispatchFailure::Recoverable(detail),
        VerificationError::Timeout(detail) => ProviderDispatchFailure::Recoverable(detail),
        other => ProviderDispatchFailure::Refused(other.to_string()),
    }
}

/// Project one neutral per-claim verdict into the contract evidence model.
fn provider_claim_evidence(
    obligation: &ProofObligation,
    result: &ClaimVerificationResult,
    candidate: Option<&SubjectSnapshot>,
) -> ContractEvidence {
    let snapshot = result
        .subject_snapshot
        .clone()
        .or_else(|| candidate.cloned());
    let bindings = if obligation.evidence_binding.is_bound() {
        vec![obligation.evidence_binding.clone()]
    } else {
        vec![EvidenceBinding::ProviderClaim(result.obligation_id.clone())]
    };
    let detail = result
        .evidence
        .first()
        .map(|artifact| format!("{}: {}", artifact.source, artifact.detail))
        .unwrap_or_else(|| format!("provider verdict: {}", result.state.as_str()));
    ContractEvidence {
        kind: obligation.evidence,
        source: format!("{}:{}", result.provider.as_str(), result.obligation_id),
        detail: bounded_detail(&detail),
        passed: None,
        observed_state: Some(result.state),
        counterexample: result
            .counterexample
            .first()
            .map(|counterexample| bounded_counterexample(counterexample)),
        bindings,
        op: Some(
            op_for_claim(&ClaimRequest::from_obligation(obligation))
                .as_str()
                .to_string(),
        ),
        provider: Some(result.provider.as_str().to_string()),
        provider_schema_version: None,
        artifact_digest: None,
        snapshot,
        environment: None,
        scope: result.basis.scope,
    }
}

/// Evaluate the session's engineering contract against this attempt's
/// executed checks, optional review verdict and optional candidate snapshot,
/// and persist the evaluated obligation states. Infallible by construction:
/// every failure is a typed outcome the gate site can act on.
pub(crate) async fn evaluate_and_persist_contract(
    handle: &SessionHandle,
    executed: &[ExecutedCheck],
    review: Option<&serde_json::Value>,
    candidate: Option<&SubjectSnapshot>,
    dispatch: Option<&ContractProviderDispatch<'_>>,
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
            );
        }
        Err(e) => {
            return ContractGateOutcome::Unavailable(format!("durable task row unreadable: {e}"));
        }
    };
    if task.contract.is_empty() {
        return ContractGateOutcome::Evaluated {
            updates_applied: 0,
            disproven: Vec::new(),
            unresolved_required: Vec::new(),
            advisory_uncertain: Vec::new(),
            constraints: ConstraintGate::default(),
            replan: Vec::new(),
        };
    }
    if task.state.is_terminal() {
        return ContractGateOutcome::TerminalFrozen;
    }
    let mut evidence: Vec<ContractEvidence> = executed
        .iter()
        .map(|run| ContractEvidence::from_executed(run, candidate))
        .collect();
    // A review verdict of `clean` supports Review obligations; any other
    // parsed verdict disproves them; an absent/unparseable verdict is no
    // evidence (never a fabricated clean).
    let review_clean = review
        .and_then(|value| value.get("verdict").and_then(|v| v.as_str()))
        .map(|verdict| verdict == "clean");
    if let Some(clean) = review_clean {
        evidence.push(ContractEvidence::from_review(clean));
    }
    // The completion spine (audit item 6): unresolved claims whose routed
    // operation an external verification provider serves are dispatched
    // through the neutral registry; per-claim verdicts land as provider
    // evidence, are written back into the contract below through the SAME
    // evaluation/persistence/replan path as native evidence, and a disproven
    // claim re-opens work with a typed replan cause.
    if let Some(dispatch) = dispatch {
        match dispatch_provider_claims(handle, &task.contract, candidate, dispatch).await {
            Ok(provider_evidence) => evidence.extend(provider_evidence),
            Err(ProviderDispatchFailure::Recoverable(detail)) => {
                // Provider absence/failure is never a silent pass: the
                // affected required claims stay unresolved and the gate
                // refuses completion (NeedsVerification) exactly as before.
                tracing::warn!(
                    "session {}: verification provider claim dispatch degraded: {detail}",
                    handle.id()
                );
            }
            Err(ProviderDispatchFailure::Refused(detail)) => {
                return ContractGateOutcome::Unavailable(format!(
                    "verification provider claim dispatch refused: {detail}"
                ));
            }
        }
    }
    let evaluation = evaluate_contract(&task.contract, &evidence, candidate);
    if evaluation.updates.is_empty() {
        return ContractGateOutcome::Evaluated {
            updates_applied: 0,
            disproven: evaluation.disproven,
            unresolved_required: evaluation.unresolved_required,
            advisory_uncertain: evaluation.advisory_uncertain,
            constraints: evaluation.constraints,
            replan: evaluation.replan,
        };
    }
    match handle.apply_task_contract_obligation_states(task_id, &evaluation.updates) {
        Ok(_) => ContractGateOutcome::Evaluated {
            updates_applied: evaluation.updates.len(),
            disproven: evaluation.disproven,
            unresolved_required: evaluation.unresolved_required,
            advisory_uncertain: evaluation.advisory_uncertain,
            constraints: evaluation.constraints,
            replan: evaluation.replan,
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

/// The confidence recorded for one claim state.
pub(crate) const fn confidence_for(state: ClaimState) -> u16 {
    match state {
        ClaimState::KnownTrue => 10_000,
        ClaimState::Supported => SUPPORTED_CONFIDENCE_BPS,
        ClaimState::Inferred => 5_000,
        ClaimState::Uncertain => 3_000,
        ClaimState::Unknown | ClaimState::Disproven => DISPROVEN_CONFIDENCE_BPS,
    }
}

/// One piece of evidence presented to the contract, with the full binding
/// identity it can discharge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContractEvidence {
    pub(crate) kind: ProofEvidenceKind,
    /// The producing surface (check id, provider id, review).
    pub(crate) source: String,
    /// Bounded human detail.
    pub(crate) detail: String,
    /// Native verdict (`None` for provider claims, which carry an observed
    /// state instead).
    pub(crate) passed: Option<bool>,
    /// The provider-asserted state (external claim verification).
    pub(crate) observed_state: Option<ClaimState>,
    /// The observed counterexample (external disproofs, failed summaries).
    pub(crate) counterexample: Option<String>,
    /// Exactly what this evidence can discharge.
    pub(crate) bindings: Vec<EvidenceBinding>,
    /// The verification op that produced it (wire name).
    pub(crate) op: Option<String>,
    pub(crate) provider: Option<String>,
    pub(crate) provider_schema_version: Option<u32>,
    pub(crate) artifact_digest: Option<String>,
    /// The candidate snapshot the evidence was produced against.
    pub(crate) snapshot: Option<SubjectSnapshot>,
    /// The environment basis, when recorded separately from the snapshot.
    pub(crate) environment: Option<String>,
    /// The declared reuse scope of the evidence.
    pub(crate) scope: ProofScope,
}

impl ContractEvidence {
    /// Project one executed native check into contract evidence. A native
    /// check is snapshot-bound to the candidate it ran against and carries
    /// its check id plus (for test checks) the test selectors it exercised.
    pub(crate) fn from_executed(run: &ExecutedCheck, snapshot: Option<&SubjectSnapshot>) -> Self {
        let counterexample = if run.passed {
            None
        } else {
            run.summary.as_deref().map(bounded_counterexample)
        };
        Self {
            kind: evidence_kind_for_check(run.kind),
            source: run.id.clone(),
            detail: bounded_detail(&format!("{} {}", run.program, run.args.join(" "))),
            passed: Some(run.passed),
            observed_state: None,
            counterexample,
            bindings: bindings_for_executed_check(run),
            op: Some(op_for_check(run.kind).to_string()),
            provider: None,
            provider_schema_version: None,
            artifact_digest: None,
            snapshot: snapshot.cloned(),
            environment: snapshot.and_then(|s| s.environment.clone()),
            scope: ProofScope::SnapshotBound,
        }
    }

    /// The review verdict as evidence: review evidence carries the legacy
    /// category binding (a review is not claim-selector bound).
    pub(crate) fn from_review(clean: bool) -> Self {
        Self {
            kind: ProofEvidenceKind::Review,
            source: "independent_review".to_string(),
            detail: if clean {
                "review verdict: clean".to_string()
            } else {
                "review verdict: blocking or concerning".to_string()
            },
            passed: Some(clean),
            observed_state: None,
            counterexample: None,
            bindings: vec![EvidenceBinding::Category],
            op: Some("review".to_string()),
            provider: None,
            provider_schema_version: None,
            artifact_digest: None,
            snapshot: None,
            environment: None,
            scope: ProofScope::SnapshotBound,
        }
    }
}

/// The result of evaluating one attempt's evidence against a contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContractEvaluation {
    /// The contract with evaluated states/refs/proofs applied.
    pub(crate) contract: EngineeringContract,
    /// Only the obligations whose state/confidence/refs/proofs changed (the
    /// update payload for the durable row; empty = write nothing).
    pub(crate) updates: Vec<ProofObligation>,
    /// The disproven obligations (completion must be blocked).
    pub(crate) disproven: Vec<ContractDisclosure>,
    /// Required obligations unresolved at the candidate (typed
    /// NeedsVerification).
    pub(crate) unresolved_required: Vec<ContractDisclosure>,
    /// Advisory obligations uncertain at the candidate (disclosure-only).
    pub(crate) advisory_uncertain: Vec<ContractDisclosure>,
    /// The constraint/invariant preservation gate.
    pub(crate) constraints: ConstraintGate,
    /// Counterexamples / invalidated assumptions that re-open work.
    pub(crate) replan: Vec<ReplanCause>,
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

fn op_for_check(check: faktor_verify::CheckKind) -> &'static str {
    match check {
        faktor_verify::CheckKind::Compile => "build",
        faktor_verify::CheckKind::Lint => "lint",
        faktor_verify::CheckKind::Test => "run_tests",
    }
}

/// The bindings one executed native check carries: always its check id; for
/// test checks additionally the test selectors it exercised (`cargo test
/// auth::refresh::replay_is_rejected` carries that selector, so it can only
/// discharge claims bound to it).
pub(crate) fn bindings_for_executed_check(run: &ExecutedCheck) -> Vec<EvidenceBinding> {
    let mut bindings = vec![EvidenceBinding::CheckIds(vec![run.id.clone()])];
    if run.kind == faktor_verify::CheckKind::Test {
        let selectors = test_selectors(&run.program, &run.args);
        match selectors.len() {
            0 => {}
            1 => bindings.push(EvidenceBinding::TestSelector(selectors[0].clone())),
            _ => bindings.push(EvidenceBinding::TestSelectors(selectors)),
        }
    }
    bindings
}

/// The test selectors a test-check command exercised (`cargo test <sel>`,
/// `cargo nextest run <sel>`). Flags and flag values are not selectors.
fn test_selectors(program: &str, args: &[String]) -> Vec<String> {
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

/// TRUE when one piece of evidence may discharge (or disprove) one claim:
/// the evidence kind must match AND the bindings must intersect. The legacy
/// [`EvidenceBinding::Category`] on the CLAIM accepts any evidence of the
/// kind (pre-binding rows); a bound claim is structurally closed to
/// evidence that does not name it.
pub(crate) fn evidence_matches(obligation: &ProofObligation, evidence: &ContractEvidence) -> bool {
    if obligation.evidence != evidence.kind {
        return false;
    }
    if !obligation.evidence_binding.is_bound() {
        return true;
    }
    evidence
        .bindings
        .iter()
        .any(|binding| obligation.evidence_binding.intersects(binding))
}

fn combine_evidence(matching: &[&ContractEvidence]) -> (ClaimState, Option<String>) {
    let mut best = ClaimState::Unknown;
    let mut best_rank = i8::MIN;
    let mut counterexample = None;
    for evidence in matching {
        let state = match (evidence.observed_state, evidence.passed) {
            (Some(observed), _) => observed,
            (None, Some(false)) => ClaimState::Disproven,
            (None, Some(true)) => ClaimState::Supported,
            (None, None) => ClaimState::Unknown,
        };
        if state.is_disproven() {
            if evidence.counterexample.is_some() {
                counterexample = evidence.counterexample.clone();
            }
            return (ClaimState::Disproven, counterexample);
        }
        if state.rank() > best_rank {
            best_rank = state.rank();
            best = state;
        }
    }
    (best, counterexample)
}

fn disclosure_of(obligation: &ProofObligation) -> ContractDisclosure {
    ContractDisclosure {
        id: obligation.id.clone(),
        summary: obligation.claim.clone(),
        state: obligation.state.as_str().to_string(),
        requirement: Some(obligation.requirement),
    }
}

fn preservation_check_for(id: &str, statement: &str) -> PreservationCheck {
    PreservationCheck {
        id: id.to_string(),
        statement: bounded_detail(statement),
        evidence_kind: ProofEvidenceKind::ContractCheck,
        binding: EvidenceBinding::ContractIds(vec![id.to_string()]),
    }
}

/// Evaluate the contract's constraints and invariants: bound evidence that
/// FAILED violates them (completion must be refused); a hard constraint or
/// invariant with no bound evidence forces its named preservation check.
pub(crate) fn evaluate_constraints(
    contract: &EngineeringContract,
    evidence: &[ContractEvidence],
) -> ConstraintGate {
    let mut gate = ConstraintGate::default();
    for check in planned_preservation_checks(contract) {
        let bound = &check.binding;
        let matching: Vec<&ContractEvidence> = evidence
            .iter()
            .filter(|item| {
                item.kind == check.evidence_kind
                    && item.bindings.iter().any(|b| bound.intersects(b))
            })
            .collect();
        if matching.is_empty() {
            gate.unresolved.push(check);
            continue;
        }
        let violated = matching.iter().any(|item| {
            item.passed == Some(false) || item.observed_state == Some(ClaimState::Disproven)
        });
        if violated {
            gate.violated.push(ContractDisclosure {
                id: check.id.clone(),
                summary: check.statement.clone(),
                state: "violated".to_string(),
                requirement: Some(Requirement::Required),
            });
        }
    }
    gate
}

/// The preservation checks the contract requires for the current candidate
/// (one bound check per constraint and invariant). The verification planner
/// consumes this list to execute the checks.
pub(crate) fn planned_preservation_checks(
    contract: &EngineeringContract,
) -> Vec<PreservationCheck> {
    contract
        .constraints
        .iter()
        .map(|constraint| preservation_check_for(&constraint.id, &constraint.statement))
        .chain(
            contract
                .invariants
                .iter()
                .map(|invariant| preservation_check_for(&invariant.id, &invariant.statement)),
        )
        .collect()
}

/// The high-risk / uncertainty policy of the contract (audit item 4), read
/// from the RECORDED obligation states (the route/finish sites run before a
/// fresh candidate identity exists; the gate itself re-evaluates against the
/// exact candidate). Empty contracts and calm contracts yield
/// [`ContractVerificationPolicy::NEUTRAL`] (parity).
pub(crate) fn contract_verification_policy(
    contract: &EngineeringContract,
) -> ContractVerificationPolicy {
    let mut policy = ContractVerificationPolicy::NEUTRAL;
    if contract.max_risk().escalates() {
        policy.depth = VerificationDepth::Deep;
        policy.quality_strict = true;
        policy.routing_quality_floor = 80;
        policy.semantic_risk_floor = 70;
    }
    if contract
        .proof_obligations
        .iter()
        .any(ProofObligation::is_unresolved_required)
    {
        // Uncertainty escalates intelligence and retrieval on the NEXT
        // turn: the unresolved required proof is the trigger.
        policy.intelligence_escalation = true;
        policy.retrieval_escalation = true;
    }
    policy
}

/// Evaluate `contract` against this attempt's evidence at `candidate`.
pub(crate) fn evaluate_contract(
    contract: &EngineeringContract,
    evidence: &[ContractEvidence],
    candidate: Option<&SubjectSnapshot>,
) -> ContractEvaluation {
    let mut updated = contract.clone();
    let mut updates = Vec::new();
    let mut replan = Vec::new();
    for obligation in updated.proof_obligations.iter_mut() {
        let before = obligation.clone();
        // 1. Snapshot law: a claim-grade state whose recorded basis does not
        // demonstrably survive the current candidate degrades to Unknown
        // BEFORE any fresh evidence is considered; the stale confidence is
        // replaced with the degraded state's honest confidence.
        let candidate_state = obligation.state_for_candidate(candidate);
        if candidate_state != obligation.state {
            obligation.state = candidate_state;
            obligation.confidence_bps = Some(confidence_for(candidate_state));
        }
        let matching: Vec<&ContractEvidence> = evidence
            .iter()
            .filter(|item| evidence_matches(obligation, item))
            .collect();
        if matching.is_empty() {
            if *obligation != before {
                updates.push(obligation.clone());
            }
            continue;
        }
        // 2. Fresh evidence: combine into a state, append deduplicated refs,
        // and record the snapshot-bound proof for this candidate.
        let (state, counterexample) = combine_evidence(&matching);
        obligation.state = state;
        obligation.confidence_bps = Some(confidence_for(state));
        for item in &matching {
            let reference = EvidenceRef {
                kind: item.kind,
                source: item.source.clone(),
                detail: item.detail.clone(),
            };
            if !obligation.evidence_refs.contains(&reference) {
                obligation.evidence_refs.push(reference);
            }
        }
        let reference = proof_reference(obligation, &matching);
        let independent = matching
            .iter()
            .all(|item| item.scope == ProofScope::SnapshotIndependent);
        let basis = match (candidate, independent) {
            (Some(snapshot), true) => ProofBasis::snapshot_independent(snapshot.clone()),
            (Some(snapshot), false) => ProofBasis::snapshot_bound(snapshot.clone()),
            (None, true) => ProofBasis {
                subject: None,
                scope: ProofScope::SnapshotIndependent,
            },
            (None, false) => ProofBasis {
                subject: None,
                scope: ProofScope::SnapshotBound,
            },
        };
        obligation.record_proof(ProofRecord {
            claim: obligation.claim.clone(),
            state,
            confidence_bps: confidence_for(state),
            basis,
            evidence: obligation.evidence_refs.clone(),
            reference: Some(reference),
            counterexample: counterexample.clone(),
        });
        if state.is_disproven()
            && (counterexample.is_some()
                || obligation.origin == faktor_core::ContractOrigin::Heuristic)
        {
            replan.push(ReplanCause {
                obligation_id: obligation.id.clone(),
                claim: obligation.claim.clone(),
                counterexample: counterexample.clone(),
                detail: match &counterexample {
                    Some(example) => format!(
                        "counterexample invalidated the assumption {:?}: {example}",
                        obligation.claim
                    ),
                    None => format!(
                        "disproof invalidated the heuristic assumption {:?}",
                        obligation.claim
                    ),
                },
            });
        }
        if *obligation != before {
            updates.push(obligation.clone());
        }
    }
    let disproven: Vec<ContractDisclosure> = updated
        .proof_obligations
        .iter()
        .filter(|obligation| obligation.state.is_disproven())
        .map(disclosure_of)
        .collect();
    let unresolved_required: Vec<ContractDisclosure> = updated
        .proof_obligations
        .iter()
        .filter(|obligation| obligation.is_unresolved_required())
        .map(disclosure_of)
        .collect();
    let advisory_uncertain: Vec<ContractDisclosure> = updated
        .proof_obligations
        .iter()
        .filter(|obligation| obligation.is_advisory_uncertain())
        .map(disclosure_of)
        .collect();
    let constraints = evaluate_constraints(&updated, evidence);
    ContractEvaluation {
        contract: updated,
        updates,
        disproven,
        unresolved_required,
        advisory_uncertain,
        constraints,
        replan,
    }
}

/// The rich, snapshot-bound reference of the deciding evidence.
fn proof_reference(
    obligation: &ProofObligation,
    matching: &[&ContractEvidence],
) -> ProofEvidenceRef {
    let deciding = matching
        .iter()
        .find(|item| item.state_is_deciding(obligation))
        .or_else(|| matching.first())
        .copied()
        .expect("proof_reference is only called with matching evidence");
    let matched_binding = deciding
        .bindings
        .iter()
        .find(|binding| {
            obligation.evidence_binding.is_bound()
                && obligation.evidence_binding.intersects(binding)
        })
        .or_else(|| deciding.bindings.first())
        .cloned()
        .unwrap_or_default();
    ProofEvidenceRef {
        kind: deciding.kind,
        snapshot: deciding
            .snapshot
            .as_ref()
            .map(|snapshot| snapshot.candidate.clone()),
        op: deciding.op.clone(),
        selector: matched_binding.selector().map(str::to_string),
        binding: matched_binding,
        provider: deciding.provider.clone(),
        provider_schema_version: deciding.provider_schema_version,
        artifact_digest: deciding.artifact_digest.clone(),
        environment: deciding
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.environment.clone())
            .or_else(|| deciding.environment.clone()),
        detail: deciding.detail.clone(),
    }
}

impl ContractEvidence {
    /// Whether this evidence is the one that decided the obligation's state.
    fn state_is_deciding(&self, obligation: &ProofObligation) -> bool {
        match (self.observed_state, self.passed) {
            (Some(observed), _) => observed == obligation.state,
            (None, Some(passed)) => {
                (passed && obligation.state.rank() >= ClaimState::Supported.rank())
                    || (!passed && obligation.state.is_disproven())
            }
            (None, None) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_core::{AcceptedStrength, ContractOrigin};

    fn native(
        kind: ProofEvidenceKind,
        id: &str,
        passed: bool,
        bindings: Vec<EvidenceBinding>,
    ) -> ContractEvidence {
        // Every executed check carries its check id (the runtime's
        // `from_executed` does the same); tests may add more bindings.
        let bindings = if bindings.is_empty() {
            vec![check_ids(&[id])]
        } else {
            bindings
        };
        ContractEvidence {
            kind,
            source: id.to_string(),
            detail: format!("{id} detail"),
            passed: Some(passed),
            observed_state: None,
            counterexample: None,
            bindings,
            op: None,
            provider: None,
            provider_schema_version: None,
            artifact_digest: None,
            snapshot: None,
            environment: None,
            scope: ProofScope::SnapshotBound,
        }
    }

    fn check_ids(ids: &[&str]) -> EvidenceBinding {
        EvidenceBinding::CheckIds(ids.iter().map(|id| id.to_string()).collect())
    }

    fn selector(value: &str) -> EvidenceBinding {
        EvidenceBinding::TestSelector(value.to_string())
    }

    fn bound(
        claim: &str,
        evidence: ProofEvidenceKind,
        binding: EvidenceBinding,
    ) -> ProofObligation {
        EngineeringContract::obligation_bound(
            claim,
            evidence,
            ContractOrigin::User,
            Requirement::Required,
            binding,
            AcceptedStrength::Supported,
        )
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
            bound(
                "the workspace compiles",
                ProofEvidenceKind::Compile,
                check_ids(&["cargo-check"]),
            ),
            bound(
                "all tests pass",
                ProofEvidenceKind::TestExecution,
                check_ids(&["cargo-test"]),
            ),
        ]);
        let evaluation = evaluate_contract(
            &contract,
            &[
                native(ProofEvidenceKind::Compile, "cargo-check", true, vec![]),
                native(
                    ProofEvidenceKind::TestExecution,
                    "cargo-test",
                    false,
                    vec![],
                ),
            ],
            None,
        );
        assert_eq!(evaluation.updates.len(), 2);
        let compile = &evaluation.contract.proof_obligations[0];
        assert_eq!(compile.state, ClaimState::Supported);
        assert_eq!(compile.confidence_bps, Some(SUPPORTED_CONFIDENCE_BPS));
        assert_eq!(compile.proofs.len(), 1);
        assert!(compile.proofs[0].basis.subject.is_none());
        let tests = &evaluation.contract.proof_obligations[1];
        assert_eq!(tests.state, ClaimState::Disproven);
        assert_eq!(tests.confidence_bps, Some(DISPROVEN_CONFIDENCE_BPS));
        assert_eq!(evaluation.disproven.len(), 1);
        assert_eq!(evaluation.disproven[0].summary, "all tests pass");
        assert!(evaluation.unresolved_required.is_empty());
    }

    #[test]
    fn lint_discharges_compile_obligations_and_missing_evidence_keeps_state() {
        let mut previously = bound(
            "no new lint warnings",
            ProofEvidenceKind::Compile,
            check_ids(&["cargo-clippy"]),
        );
        previously.state = ClaimState::Inferred;
        let contract = contract_with(vec![previously.clone()]);
        // No checks at all: nothing changes.
        let evaluation = evaluate_contract(&contract, &[], None);
        assert!(evaluation.updates.is_empty());
        assert_eq!(evaluation.contract.proof_obligations[0], previously);
        // A lint check with the matching id discharges a Compile obligation.
        let evaluation = evaluate_contract(
            &contract,
            &[native(
                ProofEvidenceKind::Compile,
                "cargo-clippy",
                true,
                vec![],
            )],
            None,
        );
        assert_eq!(evaluation.updates.len(), 1);
        assert_eq!(
            evaluation.contract.proof_obligations[0].state,
            ClaimState::Supported
        );
    }

    #[test]
    fn unrelated_evidence_cannot_discharge_or_disprove_a_bound_claim() {
        // Two claims: one bound to the replay selector, one to a check id.
        let contract = contract_with(vec![
            bound(
                "the replay claim holds",
                ProofEvidenceKind::TestExecution,
                selector("auth::refresh::replay_is_rejected"),
            ),
            bound(
                "the whole suite passes",
                ProofEvidenceKind::TestExecution,
                check_ids(&["cargo-test"]),
            ),
        ]);
        // NEGATIVE: an unrelated failing test cannot disprove either claim.
        let unrelated_failing = native(
            ProofEvidenceKind::TestExecution,
            "cargo-test-unrelated",
            false,
            vec![selector("auth::other::happy_path")],
        );
        // (this evidence carries only the selector; it does not name the
        // check id the second claim is bound to either)
        let evaluation = evaluate_contract(&contract, &[unrelated_failing], None);
        assert!(evaluation.updates.is_empty());
        assert_eq!(
            evaluation.contract.proof_obligations[0].state,
            ClaimState::Unknown
        );
        assert_eq!(
            evaluation.contract.proof_obligations[1].state,
            ClaimState::Unknown
        );
        assert!(evaluation.disproven.is_empty());

        // POSITIVE: the matching selector discharges ONLY the replay claim.
        let matching_pass = native(
            ProofEvidenceKind::TestExecution,
            "cargo-test-replay",
            true,
            vec![
                selector("auth::refresh::replay_is_rejected"),
                check_ids(&["cargo-test-replay"]),
            ],
        );
        let evaluation = evaluate_contract(&contract, &[matching_pass], None);
        assert_eq!(evaluation.updates.len(), 1);
        assert_eq!(
            evaluation.contract.proof_obligations[0].state,
            ClaimState::Supported
        );
        assert_eq!(
            evaluation.contract.proof_obligations[1].state,
            ClaimState::Unknown,
            "unrelated check evidence must not discharge the suite claim"
        );

        // NEGATIVE: the matching selector failing disproves ONLY that claim.
        let matching_fail = native(
            ProofEvidenceKind::TestExecution,
            "cargo-test-replay",
            false,
            vec![selector("auth::refresh::replay_is_rejected")],
        );
        let evaluation = evaluate_contract(&contract, &[matching_fail], None);
        assert_eq!(
            evaluation.contract.proof_obligations[0].state,
            ClaimState::Disproven
        );
        assert_eq!(
            evaluation.contract.proof_obligations[1].state,
            ClaimState::Unknown,
            "an unrelated failing test cannot disprove all TestExecution obligations"
        );

        // The check-id binding is equally structural.
        let evaluation = evaluate_contract(
            &contract,
            &[native(
                ProofEvidenceKind::TestExecution,
                "cargo-test",
                true,
                vec![check_ids(&["cargo-test"])],
            )],
            None,
        );
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
    fn required_unknown_blocks_completion_typed_and_advisory_only_discloses() {
        let mut required = bound(
            "the workspace compiles",
            ProofEvidenceKind::Compile,
            check_ids(&["cargo-check"]),
        );
        required.state = ClaimState::Uncertain;
        let mut advisory = bound(
            "nice to have",
            ProofEvidenceKind::Compile,
            check_ids(&["cargo-extra"]),
        );
        advisory.requirement = Requirement::Advisory;
        advisory.accepted_strength = AcceptedStrength::for_requirement(Requirement::Advisory);
        advisory.state = ClaimState::Unknown;
        let contract = contract_with(vec![required, advisory]);
        let evaluation = evaluate_contract(&contract, &[], None);
        assert_eq!(evaluation.unresolved_required.len(), 1);
        assert_eq!(evaluation.advisory_uncertain.len(), 1);
        let outcome = ContractGateOutcome::Evaluated {
            updates_applied: 0,
            disproven: evaluation.disproven,
            unresolved_required: evaluation.unresolved_required,
            advisory_uncertain: evaluation.advisory_uncertain,
            constraints: evaluation.constraints,
            replan: evaluation.replan,
        };
        // The passing gate is downgraded to the typed NeedsVerification.
        let gate = apply_contract_outcome(CompletionGate::VerifiedComplete, outcome.clone());
        assert_eq!(gate, CompletionGate::Unverified);
        assert_eq!(
            gate.task_state(),
            faktor_core::state::TaskState::NeedsVerification
        );
        assert!(needs_verification_detail(&outcome)
            .expect("required unresolved is disclosed")
            .contains("the workspace compiles"));

        // Advisory-only uncertainty keeps the passing gate (disclosure only).
        let advisory_only = contract_with(vec![{
            let mut obligation = bound(
                "optional",
                ProofEvidenceKind::Compile,
                check_ids(&["cargo-extra"]),
            );
            obligation.requirement = Requirement::Advisory;
            obligation.accepted_strength = AcceptedStrength::for_requirement(Requirement::Advisory);
            obligation.state = ClaimState::Unknown;
            obligation
        }]);
        let evaluation = evaluate_contract(&advisory_only, &[], None);
        assert!(evaluation.unresolved_required.is_empty());
        assert_eq!(evaluation.advisory_uncertain.len(), 1);
        let outcome = ContractGateOutcome::Evaluated {
            updates_applied: 0,
            disproven: evaluation.disproven,
            unresolved_required: evaluation.unresolved_required,
            advisory_uncertain: evaluation.advisory_uncertain,
            constraints: evaluation.constraints,
            replan: evaluation.replan,
        };
        assert_eq!(
            apply_contract_outcome(CompletionGate::VerifiedComplete, outcome),
            CompletionGate::VerifiedComplete
        );
    }

    #[test]
    fn snapshot_change_stales_prior_support_and_same_candidate_reuses_it() {
        let snapshot_a = SubjectSnapshot::new("tm1:aaaa").with_environment("env-1");
        let snapshot_b = SubjectSnapshot::new("tm1:bbbb").with_environment("env-1");
        let obligation = bound(
            "the workspace compiles",
            ProofEvidenceKind::Compile,
            check_ids(&["cargo-check"]),
        );
        let contract = contract_with(vec![obligation]);
        let passing = native(
            ProofEvidenceKind::Compile,
            "cargo-check",
            true,
            vec![check_ids(&["cargo-check"])],
        );
        // First proof under snapshot A.
        let evaluation =
            evaluate_contract(&contract, std::slice::from_ref(&passing), Some(&snapshot_a));
        assert_eq!(
            evaluation.contract.proof_obligations[0].state,
            ClaimState::Supported
        );
        let proven = evaluation.contract.clone();
        // Same candidate, evidence re-presented: the recorded proof is
        // reusable and the evaluation stays identical.
        let evaluation =
            evaluate_contract(&proven, std::slice::from_ref(&passing), Some(&snapshot_a));
        assert!(evaluation.updates.is_empty());
        assert_eq!(
            evaluation.contract.proof_obligations[0].state,
            ClaimState::Supported
        );
        // Same candidate, but this attempt produced NO evidence: the state
        // still holds ONLY because the basis survives (never a timeless
        // claim, and the run itself proves nothing new).
        let evaluation = evaluate_contract(&proven, &[], Some(&snapshot_a));
        assert!(evaluation.updates.is_empty());
        assert_eq!(
            evaluation.contract.proof_obligations[0].state,
            ClaimState::Supported
        );
        // Changed candidate, no fresh evidence: the prior proof is stale and
        // the state degrades to Unknown (durably persisted).
        let evaluation = evaluate_contract(&proven, &[], Some(&snapshot_b));
        assert_eq!(evaluation.updates.len(), 1);
        assert_eq!(
            evaluation.contract.proof_obligations[0].state,
            ClaimState::Unknown
        );
        assert_eq!(
            evaluation.contract.proof_obligations[0].confidence_bps,
            Some(DISPROVEN_CONFIDENCE_BPS),
            "a staled proof must not keep its old confidence"
        );
        assert_eq!(
            proven.proof_obligations[0].staleness(Some(&snapshot_b)),
            Some(faktor_core::ProofStaleness::SnapshotChanged)
        );
        // Changed candidate WITH fresh evidence: reproof under the new
        // snapshot supports the claim again with a second record.
        let evaluation = evaluate_contract(&proven, &[passing], Some(&snapshot_b));
        assert_eq!(
            evaluation.contract.proof_obligations[0].state,
            ClaimState::Supported
        );
        assert_eq!(evaluation.contract.proof_obligations[0].proofs.len(), 2);
    }

    #[test]
    fn constraints_gate_on_violation_and_force_unevaluated_checks() {
        use faktor_core::Constraint;
        let mut contract = contract_with(vec![]);
        contract.constraints.push(Constraint {
            id: "c1:0".into(),
            statement: "no new dependencies".into(),
            origin: ContractOrigin::User,
        });
        contract.invariants.push(faktor_core::Invariant {
            id: "i1:0".into(),
            statement: "existing sessions stay valid".into(),
            origin: ContractOrigin::Policy,
        });
        // No bound evidence: both preservation checks are forced.
        let evaluation = evaluate_contract(&contract, &[], None);
        assert_eq!(evaluation.constraints.unresolved.len(), 2);
        assert!(evaluation.constraints.violated.is_empty());
        let outcome = ContractGateOutcome::Evaluated {
            updates_applied: 0,
            disproven: evaluation.disproven,
            unresolved_required: evaluation.unresolved_required,
            advisory_uncertain: evaluation.advisory_uncertain,
            constraints: evaluation.constraints,
            replan: evaluation.replan,
        };
        assert_eq!(
            apply_contract_outcome(CompletionGate::VerifiedComplete, outcome.clone()),
            CompletionGate::Unverified
        );
        assert!(needs_verification_detail(&outcome)
            .unwrap()
            .contains("c1:0"));
        // Bound evidence observed FALSE violates the constraint: the gate is
        // blocked with the typed check-failed reason.
        let violated = native(
            ProofEvidenceKind::ContractCheck,
            "contract-check",
            false,
            vec![EvidenceBinding::ContractIds(vec!["c1:0".into()])],
        );
        let evaluation = evaluate_contract(&contract, &[violated], None);
        assert_eq!(evaluation.constraints.violated.len(), 1);
        let outcome = ContractGateOutcome::Evaluated {
            updates_applied: 0,
            disproven: evaluation.disproven,
            unresolved_required: evaluation.unresolved_required,
            advisory_uncertain: evaluation.advisory_uncertain,
            constraints: evaluation.constraints,
            replan: evaluation.replan,
        };
        match apply_contract_outcome(CompletionGate::VerifiedComplete, outcome) {
            CompletionGate::BlockedVerification { reasons } => {
                assert!(reasons.iter().any(|r| r.code == ReasonCode::CheckFailed));
            }
            other => panic!("expected a blocked gate, got {other:?}"),
        }
        // Bound PASSING evidence satisfies the constraint; the invariant
        // still forces its own check.
        let satisfied = native(
            ProofEvidenceKind::ContractCheck,
            "contract-check",
            true,
            vec![EvidenceBinding::ContractIds(vec!["c1:0".into()])],
        );
        let evaluation = evaluate_contract(&contract, &[satisfied], None);
        assert!(evaluation.constraints.violated.is_empty());
        assert_eq!(evaluation.constraints.unresolved.len(), 1);
        assert_eq!(evaluation.constraints.unresolved[0].id, "i1:0");
    }

    #[test]
    fn high_risk_and_unknown_risk_raise_depth_floor_and_escalation() {
        use faktor_core::{RiskHypothesis, RiskSeverity};
        let calm = EngineeringContract {
            risks: vec![RiskHypothesis {
                id: "r1:0".into(),
                description: "minor".into(),
                severity: RiskSeverity::Low,
                origin: ContractOrigin::Heuristic,
            }],
            ..Default::default()
        };
        let calm_policy = contract_verification_policy(&calm);
        assert_eq!(calm_policy.depth, VerificationDepth::Standard);
        assert!(!calm_policy.quality_strict);
        assert_eq!(calm_policy.routing_quality_floor, 60);
        assert!(!calm_policy.intelligence_escalation);

        let risky = EngineeringContract {
            risks: vec![RiskHypothesis {
                id: "r1:1".into(),
                description: "race conditions".into(),
                severity: RiskSeverity::High,
                origin: ContractOrigin::Heuristic,
            }],
            // An unresolved REQUIRED obligation escalates intelligence and
            // retrieval as well.
            proof_obligations: vec![bound(
                "the race cannot happen",
                ProofEvidenceKind::TestExecution,
                selector("auth::refresh::replay_is_rejected"),
            )],
            ..Default::default()
        };
        let policy = contract_verification_policy(&risky);
        assert_eq!(policy.depth, VerificationDepth::Deep);
        assert!(policy.quality_strict);
        assert_eq!(policy.routing_quality_floor, 80);
        assert!(policy.semantic_risk_floor >= 70);
        assert!(policy.intelligence_escalation);
        assert!(policy.retrieval_escalation);

        // Unknown risk is never Safe: it escalates exactly like High.
        let unknown = EngineeringContract {
            risks: vec![RiskHypothesis {
                id: "r1:2".into(),
                description: "unknown".into(),
                severity: RiskSeverity::Unknown,
                origin: ContractOrigin::User,
            }],
            ..Default::default()
        };
        assert_eq!(
            contract_verification_policy(&unknown).depth,
            VerificationDepth::Deep
        );
        assert_eq!(
            contract_verification_policy(&EngineeringContract::default()),
            ContractVerificationPolicy::NEUTRAL
        );
    }

    #[test]
    fn counterexample_triggers_replan_and_blocks_the_gate() {
        let obligation = bound(
            "replayed refresh tokens fail",
            ProofEvidenceKind::TestExecution,
            selector("auth::refresh::replay_is_rejected"),
        );
        let contract = contract_with(vec![obligation]);
        let mut disproof = native(
            ProofEvidenceKind::TestExecution,
            "cargo-test-replay",
            false,
            vec![selector("auth::refresh::replay_is_rejected")],
        );
        disproof.counterexample = Some("token replay succeeded".into());
        let evaluation = evaluate_contract(&contract, &[disproof], None);
        assert_eq!(evaluation.disproven.len(), 1);
        assert_eq!(evaluation.replan.len(), 1);
        assert_eq!(
            evaluation.replan[0].obligation_id,
            contract.proof_obligations[0].id
        );
        assert_eq!(
            evaluation.replan[0].counterexample.as_deref(),
            Some("token replay succeeded")
        );
        assert!(evaluation.contract.proof_obligations[0].proofs[0]
            .counterexample
            .as_deref()
            .is_some_and(|ce| ce.contains("replay")));
        let outcome = ContractGateOutcome::Evaluated {
            updates_applied: evaluation.updates.len(),
            disproven: evaluation.disproven,
            unresolved_required: evaluation.unresolved_required,
            advisory_uncertain: evaluation.advisory_uncertain,
            constraints: evaluation.constraints,
            replan: evaluation.replan,
        };
        assert!(replan_detail(&outcome)
            .unwrap()
            .contains("counterexample invalidated"));
        match apply_contract_outcome(CompletionGate::VerifiedComplete, outcome) {
            CompletionGate::BlockedVerification { reasons } => {
                assert!(reasons
                    .iter()
                    .any(|r| r.code == ReasonCode::ProofObligationDisproven));
            }
            other => panic!("expected a blocked gate, got {other:?}"),
        }
    }

    #[test]
    fn external_provider_claims_carry_their_state_and_reference() {
        let obligation = EngineeringContract::obligation_bound(
            "the migration is safe",
            ProofEvidenceKind::ContractCheck,
            ContractOrigin::External {
                provider: "kiwi".into(),
            },
            Requirement::Required,
            EvidenceBinding::ProviderClaim("claim-7".into()),
            AcceptedStrength::KnownTrue,
        );
        let contract = contract_with(vec![obligation]);
        let provider_evidence = ContractEvidence {
            kind: ProofEvidenceKind::ContractCheck,
            source: "kiwi".into(),
            detail: "provider verdict".into(),
            passed: None,
            observed_state: Some(ClaimState::KnownTrue),
            counterexample: None,
            bindings: vec![EvidenceBinding::ProviderClaim("claim-7".into())],
            op: Some("verify_claim".into()),
            provider: Some("kiwi".into()),
            provider_schema_version: Some(1),
            artifact_digest: Some("sha256:abc".into()),
            snapshot: Some(SubjectSnapshot::new("tm1:provider")),
            environment: None,
            scope: ProofScope::SnapshotIndependent,
        };
        // A different provider claim id cannot discharge it.
        let unrelated = ContractEvidence {
            bindings: vec![EvidenceBinding::ProviderClaim("claim-8".into())],
            ..provider_evidence.clone()
        };
        let evaluation = evaluate_contract(&contract, &[unrelated], None);
        assert!(evaluation.updates.is_empty());
        // The exact claim id discharges it at proof grade.
        let evaluation =
            evaluate_contract(&contract, std::slice::from_ref(&provider_evidence), None);
        assert_eq!(
            evaluation.contract.proof_obligations[0].state,
            ClaimState::KnownTrue
        );
        let record = &evaluation.contract.proof_obligations[0].proofs[0];
        assert_eq!(record.basis.scope, ProofScope::SnapshotIndependent);
        let reference = record.reference.as_ref().unwrap();
        assert_eq!(reference.provider.as_deref(), Some("kiwi"));
        assert_eq!(reference.provider_schema_version, Some(1));
        assert_eq!(reference.artifact_digest.as_deref(), Some("sha256:abc"));
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
        // Native test evidence cannot fabricate ContractCheck evidence, and
        // no review ran: only the required unresolved claims are reported.
        let evaluation = evaluate_contract(
            &contract,
            &[native(
                ProofEvidenceKind::TestExecution,
                "cargo-test",
                true,
                vec![],
            )],
            None,
        );
        assert!(evaluation.updates.is_empty());
        assert_eq!(evaluation.unresolved_required.len(), 2);
        // A blocking review disproves the review obligation.
        let evaluation =
            evaluate_contract(&contract, &[ContractEvidence::from_review(false)], None);
        assert_eq!(evaluation.updates.len(), 1);
        assert_eq!(
            evaluation.contract.proof_obligations[1].state,
            ClaimState::Disproven
        );
        // A clean review supports it; unrelated obligations stay untouched.
        let evaluation = evaluate_contract(&contract, &[ContractEvidence::from_review(true)], None);
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
        let mut obligation = bound(
            "all tests pass",
            ProofEvidenceKind::TestExecution,
            check_ids(&["cargo-test"]),
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
                native(ProofEvidenceKind::TestExecution, "cargo-test", true, vec![]),
                native(ProofEvidenceKind::TestExecution, "cargo-test", true, vec![]),
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
            &[native(
                ProofEvidenceKind::TestExecution,
                "cargo-test",
                true,
                vec![],
            )],
            None,
        );
        assert!(evaluation.updates.is_empty());
    }

    #[test]
    fn test_selector_extraction_is_flag_aware_and_conservative() {
        assert_eq!(
            test_selectors(
                "cargo",
                &["test".into(), "auth::refresh::replay_is_rejected".into()]
            ),
            vec!["auth::refresh::replay_is_rejected".to_string()]
        );
        assert_eq!(
            test_selectors(
                "/usr/local/bin/cargo",
                &[
                    "test".into(),
                    "--package".into(),
                    "faktor-session".into(),
                    "auth::replay".into()
                ]
            ),
            vec!["auth::replay".to_string()]
        );
        assert_eq!(
            test_selectors(
                "cargo",
                &["nextest".into(), "run".into(), "auth::replay".into()]
            ),
            vec!["auth::replay".to_string()]
        );
        assert!(test_selectors("cargo", &["clippy".into()]).is_empty());
        assert!(test_selectors("cargo", &["test".into()]).is_empty());
        assert!(test_selectors("pytest", &["test".into(), "x".into()]).is_empty());
    }

    #[test]
    fn planned_preservation_checks_cover_every_constraint_and_invariant() {
        use faktor_core::{Constraint, Invariant};
        let contract = EngineeringContract {
            constraints: vec![Constraint {
                id: "c1:0".into(),
                statement: "no new dependencies".into(),
                origin: ContractOrigin::User,
            }],
            invariants: vec![Invariant {
                id: "i1:0".into(),
                statement: "sessions stay valid".into(),
                origin: ContractOrigin::Policy,
            }],
            ..Default::default()
        };
        let checks = planned_preservation_checks(&contract);
        assert_eq!(checks.len(), 2);
        assert_eq!(
            checks[0].binding,
            EvidenceBinding::ContractIds(vec!["c1:0".into()])
        );
        assert_eq!(
            checks[1].binding,
            EvidenceBinding::ContractIds(vec!["i1:0".into()])
        );
    }
}
