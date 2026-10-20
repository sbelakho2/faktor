//! Native task runs, driven by the daemon's one TaskExecutor.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use faktor_core::contract::{
    Constraint, ContractOrigin, EngineeringContract, Invariant, ProofEvidenceKind, RiskHypothesis,
    RiskSeverity,
};
use faktor_core::state::TaskState;
use faktor_protocol::error::ApiError;

use super::*;
use crate::api::AppState;

/// Linkage-row kind of TaskExecutor in-session runs (orchestrator crate).
pub(crate) const TASK_RUN_ROW_KIND: &str =
    faktor_orchestrator::runtime::task_executor::TASK_RUN_ROW_KIND;

/// Map the durable task phase to the run tag. A non-terminal task is NOT
/// "Done" merely because the turn-level session machine settled into
/// `ready_for_next_turn`; a waiting approval, budget stop or pending
/// verification must stay visible as its own phase.
pub(crate) fn run_state_tag_for(
    task: Option<TaskState>,
    session: faktor_core::state::AgentState,
) -> &'static str {
    // A failed/cancelled turn machine is authoritative over a stale row
    // that still says Running/Pending: the run did not complete.
    if matches!(
        session,
        faktor_core::state::AgentState::FailedRecoverable
            | faktor_core::state::AgentState::FailedPermanent
    ) {
        return "Failed";
    }
    if session == faktor_core::state::AgentState::Cancelled {
        return "Cancelled";
    }
    match task {
        Some(TaskState::VerifiedComplete) => "Done",
        Some(TaskState::Failed) => "Failed",
        Some(TaskState::Cancelled) => "Cancelled",
        Some(TaskState::NeedsVerification | TaskState::Verifying) => "Verifying",
        Some(TaskState::Blocked) => "Blocked",
        Some(TaskState::Waiting) => "Waiting",
        // A stale task row that still says Running/Pending while the turn
        // machine has already settled into `ready_for_next_turn` describes
        // no live operation: the RUN is done (the task view reports the
        // row's own phase separately). Genuine verification/blocked phases
        // above never take this branch.
        Some(TaskState::Pending | TaskState::Planning | TaskState::Running)
            if session == faktor_core::state::AgentState::ReadyForNextTurn =>
        {
            "Done"
        }
        Some(TaskState::Pending | TaskState::Planning | TaskState::Running) => "Running",
        None => session_run_state_tag(session),
    }
}

/// The ORCHESTRATED run ids named by the session's durable facts rows. The
/// classification is a membership question, so the run-id set is built ONCE
/// (a scan of the facts) instead of re-scanning every facts row per run —
/// the historical `facts.iter().any(...)` inside the run loop was O(runs x
/// facts) on the hot task-run projection.
fn orchestrated_run_ids(facts: &[(String, String, String)]) -> std::collections::HashSet<&str> {
    facts
        .iter()
        .filter(|(kind, _, _)| kind == TASK_RUN_ROW_KIND)
        .map(|(_, key, _)| key.as_str())
        .collect()
}

/// The task-run projection of a session (wave-24): every TaskExecutor run
/// — in-session (linkage rows) and orchestrated (plan rows + children) —
/// with its durable run identity, per-run state and goal. Built from
/// [`native_agents_body`]'s parent `self` entries (the SAME durable-row
/// derivation the agent listing serves; no second projection can drift),
/// with the run `mode` re-derived from the durable row kinds that name the
/// run. Empty ONLY when the session genuinely has no task run.
pub(crate) fn native_task_run_entries(
    state: &AppState,
    handle: &faktor_session::SessionHandle,
) -> Result<Vec<serde_json::Value>, ApiError> {
    let facts = orchestrator_graph_facts(handle).map_err(internal_graph_err)?;
    let orchestrated = orchestrated_run_ids(&facts);
    let session_row = handle
        .row()
        .map_err(|e| faktor_protocol::error::from_core(&e))?;
    let task_id = session_row.task_id.raw();
    let mut out = Vec::new();
    for e in native_agents_body(state, handle)? {
        if e.get("kind").and_then(|k| k.as_str()) != Some("self") {
            continue;
        }
        let Some(run_id) = e.get("run_id").and_then(|r| r.as_str()) else {
            continue;
        };
        let mode = if orchestrated.contains(run_id) {
            "in_session"
        } else {
            "orchestrated"
        };
        out.push(serde_json::json!({
            "task_id": task_id,
            "run_id": run_id,
            "mode": mode,
            "state": e.get("state").cloned().unwrap_or(serde_json::Value::String("Unknown".into())),
            "goal": e.get("goal").cloned().unwrap_or(serde_json::Value::Null),
            "item_ids": e.get("item_ids").cloned().unwrap_or(serde_json::json!([])),
            "model": e.get("model").cloned().unwrap_or(serde_json::Value::Null),
        }));
    }
    Ok(out)
}

/// The one-run projection ([`native_task_run_entries`] filtered); an
/// unknown run id is a typed NotFound — a per-run read never answers with a
/// phantom.
pub(crate) fn native_task_run_entry(
    state: &AppState,
    handle: &faktor_session::SessionHandle,
    run_id: &str,
) -> Result<serde_json::Value, ApiError> {
    for e in native_task_run_entries(state, handle)? {
        if e.get("run_id").and_then(|r| r.as_str()) == Some(run_id) {
            return Ok(e);
        }
    }
    Err(not_found(&format!(
        "task run {run_id:?} under session {}",
        handle.id()
    )))
}

/// `GET /native/session/{id}/task-runs` — the session's durable task runs
/// with per-run state (see [`native_task_run_entries`]).
pub(crate) async fn native_task_runs(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    match native_task_run_entries(&state, &handle) {
        Ok(entries) => Json(entries).into_response(),
        Err(e) => wire_status(e),
    }
}

/// `GET /native/session/{id}/task-runs/{run_id}` — the durable state of ONE
/// task run. Unknown runs are typed 404s.
pub(crate) async fn native_task_run_state(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, run_id)): Path<(String, String)>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    match native_task_run_entry(&state, &handle, &run_id) {
        Ok(entry) => Json(entry).into_response(),
        Err(e) => wire_status(e),
    }
}

/// The strict request DTO of ONE native task start
/// (`POST /native/session/{id}/task-runs`). `submission_id` is the REQUIRED
/// client submission UUID of this logical start (1..=64 ASCII `[0-9a-f-]`,
/// UUID-shaped): the executor admits the start durably under that key BEFORE
/// any task mutation, so a repeated key replays the original run receipt
/// byte-for-byte (a lost response is retried with the SAME submission id;
/// a new start uses a fresh one). `work_items` is optional: an
/// absent list starts the goal as a single MUTATING work item (`main`) —
/// that run ALWAYS works in a daemon-owned isolated candidate and
/// integrates on a verified completion. `criteria` ride the durable task
/// row's acceptance criteria. `mutation_mode` is legacy vocabulary only
/// (the sole decodable value is `shadow`; `direct_compat` is a strict
/// decode error naming the removal) — it can never disable isolation.
/// `ownership` is the LEGACY plan-global value of old clients: it is
/// converted ONCE onto mutating items that carry no explicit ownership of
/// their own (per-item `work_items[].ownership` always wins); the converted
/// plan reaches the executor with explicit per-item ownership only.
/// `routing_mode` is parsed strictly but refused until the daemon routing
/// policy (the single routing authority) can honor a per-run override —
/// never silently ignored.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StartTaskRunRequest {
    goal: String,
    submission_id: String,
    criteria: Option<Vec<String>>,
    work_items: Option<Vec<NativeTaskRunWorkItem>>,
    /// Legacy plan-global ownership (old clients only): converted at THIS
    /// boundary, never consulted by the runtime.
    ownership: Option<faktor_orchestrator::OwnershipModel>,
    model: Option<String>,
    /// Optional saved preset bound to this submission BEFORE dispatch. The
    /// captured envelope is immutable per submission and applied to the run
    /// (coder role model / declared model); a read-only preset refuses a
    /// mutating plan typed.
    #[serde(default)]
    preset_id: Option<String>,
    max_tokens: Option<u64>,
    /// Money input: decimal string or legacy JSON integer (see
    /// `faktor_cloud::money`).
    #[serde(default, with = "faktor_cloud::money::option")]
    max_cost_micro: Option<u64>,
    mutation_mode: Option<faktor_orchestrator::runtime::task_executor::MutationMode>,
    routing_mode: Option<faktor_core::model::RoutingMode>,
    /// Attached file paths (the same ordinary-prompt vocabulary the SDK
    /// `PromptRequest.files` carries): additive so IDE clients can attach
    /// files to an explicit task start. Absent/empty leaves the run
    /// attachment-free; the daemon never reads the filesystem at THIS
    /// boundary (the drive's own permission requester gates every tool).
    files: Option<Vec<String>>,
    /// Durable typed binary/image attachments (`AttachmentId` rows) uploaded
    /// through `POST /native/session/{id}/attachments` BEFORE this start.
    /// SEPARATE from `files`: CAS bytes addressed by digest, never workspace
    /// paths. Strict (`deny_unknown_fields` on the id itself); every digest
    /// must resolve to a byte-identical durable row of THIS session, and
    /// every image is validated against the chosen model (vision required,
    /// deliverable mime, provider per-image bound) with a typed refusal
    /// that keeps the draft and bytes intact. Absent/empty = the
    /// attachment-free path.
    #[serde(default)]
    attachments: Option<Vec<faktor_core::attachment::AttachmentId>>,
    /// The PR/CI-fix completion contract of this run (P2): when present with
    /// at least one requested step, the executor records it durably BEFORE
    /// the first model call and `VerifiedComplete` requires a durable
    /// `Succeeded` step-status row for every requested step. Parsed
    /// STRICTLY (`deny_unknown_fields`; missing or non-boolean members are
    /// a plain 400 — never a silent default). Absent = today's behavior.
    completion_contract: Option<faktor_core::completion::CompletionContract>,
    /// Additive user-authored engineering-contract entries (see
    /// `docs/specs/autonomous-engineering.md` and the frozen
    /// [`EngineeringContract`] model): constraints that cannot be violated,
    /// invariants that must remain true, risk hypotheses (severity starts
    /// `unknown`) and proof obligations (a bare claim text defaults its
    /// evidence kind to `test_execution`; `{claim, evidence}` states it
    /// explicitly). Ids are DERIVED daemon-side, so a repeated start unions
    /// by id instead of duplicating. Each list is bounded
    /// (<= [`faktor_session::MAX_TASK_CONTRACT_ENTRIES`] entries, each
    /// non-empty and <= [`faktor_session::MAX_TASK_CONTRACT_STRING_BYTES`]
    /// bytes); out-of-bound input is a typed 400 before any dispatch, never
    /// a truncated contract.
    #[serde(default)]
    constraints: Option<Vec<String>>,
    #[serde(default)]
    invariants: Option<Vec<String>>,
    #[serde(default)]
    risks: Option<Vec<String>>,
    #[serde(default)]
    proof_obligations: Option<Vec<ProofObligationInput>>,
}

/// One wire proof obligation of an additive task-start contract: either the
/// bare claim text (evidence defaults to `test_execution`) or the explicit
/// `{claim, evidence}` pair. Both members are REQUIRED in the object form;
/// an unknown member is a strict 400 (never silently dropped).
#[derive(serde::Deserialize)]
#[serde(untagged)]
pub(crate) enum ProofObligationInput {
    Claim(String),
    Detailed(ProofObligationInputDetailed),
}

/// The explicit `{claim, evidence}` object form of [`ProofObligationInput`].
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProofObligationInputDetailed {
    claim: String,
    evidence: ProofEvidenceKind,
}

/// One wire work item of [`StartTaskRunRequest`]. `kind` speaks the
/// orchestrator's own JSON vocabulary (the same `"Analysis"` /
/// `"Implementation"` / ... strings the durable plan rows carry).
/// `ownership` is the item's OWN [`faktor_orchestrator::OwnershipSpec`]; a
/// mutating item without one (and without a legacy plan-global conversion)
/// is an InvalidPlan, never defaulted.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeTaskRunWorkItem {
    id: String,
    kind: faktor_orchestrator::WorkKind,
    summary: Option<String>,
    depends_on: Option<Vec<String>>,
    acceptance_checks: Option<Vec<String>>,
    #[serde(default)]
    ownership: Option<faktor_orchestrator::OwnershipSpec>,
    #[serde(default)]
    required_capabilities: Option<faktor_orchestrator::caps::CapabilitySet>,
}

/// The capability ceiling of a native task run's parent (ReadWorkspace on
/// the whole workspace — the ceiling the orchestrator's own test surface
/// grants read-only runs). Every actual tool call of a drive still passes
/// the permission requester; this is the run's typed policy record, never
/// a permission grant.
pub(crate) fn native_run_parent_caps() -> faktor_orchestrator::caps::CapabilitySet {
    // The run's typed policy record: every lattice class over the widest
    // scope. Narrowing happens through task/child specs, the sandbox
    // envelope, semantic restrictions and the tool permission requester —
    // this default only makes the previously-implicit authority explicit.
    faktor_orchestrator::caps::all_lattice()
}

/// True when a task-start body must be refused with a typed 400 because a
/// non-default completion contract arrived on the plain-prompt path (no
/// explicit `work_items`): that path carries no contract seam, so dropping
/// the contract silently would claim steps the run never recorded.
fn completion_contract_needs_work_items(
    has_work_items: bool,
    completion_contract: Option<faktor_core::completion::CompletionContract>,
) -> bool {
    !has_work_items && completion_contract.is_some_and(|c| !c.is_default())
}

/// The strict shape of the required `submission_id`: non-empty, at most
/// [`MAX_SUBMISSION_ID_BYTES`] ASCII bytes of `[0-9a-f-]` (UUID-shaped;
/// lowercase hex only). Anything else is a typed 400 at the wire boundary,
/// before any session resolution or executor dispatch.
fn validate_submission_id(id: &str) -> Result<(), ApiError> {
    if faktor_orchestrator::runtime::task_executor::valid_submission_id(id) {
        return Ok(());
    }
    Err(ApiError {
        code: "malformed",
        message: format!(
            "submission_id must be 1..={} ASCII [0-9a-f-] characters",
            faktor_orchestrator::runtime::task_executor::MAX_SUBMISSION_ID_BYTES
        ),
        http_status: 400,
        retryable: false,
    })
}

/// The typed 400 of one malformed/oversized user-authored contract entry.
fn contract_refusal(code: &'static str, message: String) -> ApiError {
    ApiError {
        code,
        message,
        http_status: 400,
        retryable: false,
    }
}

/// The model selector a saved preset contributes to a task start. The task
/// executor carries ONE model id; the PROVIDER is the session's own routing
/// authority. A preset that names a DIFFERENT provider therefore cannot be
/// applied silently (the old code joined it into `provider/model`, which
/// reached Ollama as a nonexistent model id and failed the turn with a 404).
/// Same-provider presets contribute their exact model id; a different
/// provider is a typed refusal until the executor can switch providers.
fn preset_model_selector(
    preset_provider: &str,
    preset_model: &str,
    session_provider: &str,
) -> Result<Option<String>, ApiError> {
    if preset_model.is_empty() {
        return Err(contract_refusal(
            "malformed",
            "the bound preset names an empty model id".into(),
        ));
    }
    if preset_provider != session_provider {
        return Err(ApiError {
            code: "preset_provider_unsupported",
            message: format!(
                "the bound preset selects provider {preset_provider:?} but this session runs on {session_provider:?}; a preset cannot switch the session provider"
            ),
            http_status: 409,
            retryable: false,
        });
    }
    Ok(Some(preset_model.to_string()))
}

/// Validate one user-authored contract string list: bounded count, every
/// entry non-empty (after trimming) and bounded in bytes. `what` names the
/// list in the refusal message.
fn validate_contract_strings(what: &str, entries: &[String]) -> Result<(), ApiError> {
    if entries.len() > faktor_session::MAX_TASK_CONTRACT_ENTRIES {
        return Err(contract_refusal(
            "oversized",
            format!(
                "{what}: {} entries exceed MAX_TASK_CONTRACT_ENTRIES ({})",
                entries.len(),
                faktor_session::MAX_TASK_CONTRACT_ENTRIES
            ),
        ));
    }
    for entry in entries {
        if entry.trim().is_empty() {
            return Err(contract_refusal(
                "malformed",
                format!("{what}: an entry is empty"),
            ));
        }
        if entry.len() > faktor_session::MAX_TASK_CONTRACT_STRING_BYTES {
            return Err(contract_refusal(
                "oversized",
                format!(
                    "{what}: an entry of {} bytes exceeds MAX_TASK_CONTRACT_STRING_BYTES ({})",
                    entry.len(),
                    faktor_session::MAX_TASK_CONTRACT_STRING_BYTES
                ),
            ));
        }
    }
    Ok(())
}

/// Build the additive user-authored engineering contract of ONE task start.
/// Ids are DERIVED (`c1:`/`i1:`/`r1:` from the entry text via
/// [`faktor_session::task::derived_contract_entry_id`], obligations via
/// [`EngineeringContract::obligation_id`]), so the same request always
/// produces the same ids and duplicates within one request dedupe by id.
/// Everything is validated HERE, before any dispatch: an out-of-bound entry
/// is a typed 400, never a truncated contract.
fn build_user_contract(
    constraints: Option<Vec<String>>,
    invariants: Option<Vec<String>>,
    risks: Option<Vec<String>>,
    proof_obligations: Option<Vec<ProofObligationInput>>,
) -> Result<EngineeringContract, ApiError> {
    let constraints = constraints.unwrap_or_default();
    let invariants = invariants.unwrap_or_default();
    let risks = risks.unwrap_or_default();
    let proof_obligations = proof_obligations.unwrap_or_default();
    validate_contract_strings("constraints", &constraints)?;
    validate_contract_strings("invariants", &invariants)?;
    validate_contract_strings("risks", &risks)?;
    if proof_obligations.len() > faktor_session::MAX_TASK_CONTRACT_ENTRIES {
        return Err(contract_refusal(
            "oversized",
            format!(
                "proof_obligations: {} entries exceed MAX_TASK_CONTRACT_ENTRIES ({})",
                proof_obligations.len(),
                faktor_session::MAX_TASK_CONTRACT_ENTRIES
            ),
        ));
    }
    let mut seen = std::collections::HashSet::new();
    let mut contract = EngineeringContract::default();
    for statement in constraints {
        let id = faktor_session::task::derived_contract_entry_id("c1", &statement);
        if seen.insert(id.clone()) {
            contract.constraints.push(Constraint {
                id,
                statement,
                origin: ContractOrigin::User,
            });
        }
    }
    for statement in invariants {
        let id = faktor_session::task::derived_contract_entry_id("i1", &statement);
        if seen.insert(id.clone()) {
            contract.invariants.push(Invariant {
                id,
                statement,
                origin: ContractOrigin::User,
            });
        }
    }
    for description in risks {
        let id = faktor_session::task::derived_contract_entry_id("r1", &description);
        if seen.insert(id.clone()) {
            contract.risks.push(RiskHypothesis {
                id,
                description,
                severity: RiskSeverity::Unknown,
                origin: ContractOrigin::User,
            });
        }
    }
    for input in proof_obligations {
        let (claim, evidence) = match input {
            ProofObligationInput::Claim(claim) => (claim, ProofEvidenceKind::TestExecution),
            ProofObligationInput::Detailed(detailed) => (detailed.claim, detailed.evidence),
        };
        if claim.trim().is_empty() {
            return Err(contract_refusal(
                "malformed",
                "proof_obligations: an entry has an empty claim".into(),
            ));
        }
        if claim.len() > faktor_session::MAX_TASK_CONTRACT_STRING_BYTES {
            return Err(contract_refusal(
                "oversized",
                format!(
                    "proof_obligations: a claim of {} bytes exceeds MAX_TASK_CONTRACT_STRING_BYTES ({})",
                    claim.len(),
                    faktor_session::MAX_TASK_CONTRACT_STRING_BYTES
                ),
            ));
        }
        let obligation = EngineeringContract::obligation(&claim, evidence, ContractOrigin::User);
        if seen.insert(obligation.id.clone()) {
            contract.proof_obligations.push(obligation);
        }
    }
    Ok(contract)
}

/// `POST /native/session/{id}/task-runs` — start ONE task through the
/// daemon's TaskExecutor ([`ServerDeps::tasks`]; the ONLY task-start
/// authority the server reaches). The strict DTO is validated by the
/// executor itself (goal/work-item/criteria bounds and plan rules are typed
/// 400s, never silent truncation). The response carries the run's durable
/// identity + task id + its current per-run state.
///
/// Multi-item MUTATING plans are accepted: `TaskExecutor::start_task`
/// allocates the run's daemon-owned isolated root itself (through the
/// executor's single `CandidateWorkspaceService` authority), so this
/// surface never carries a filesystem path; a single-item mutating task is
/// shadowed by default.
pub(crate) async fn native_task_run_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<StartTaskRunRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    // Strict native DTO: every body rejection (syntax AND data errors —
    // unknown fields, typos, bad enum values, missing fields) is a plain
    // 400, never a 422.
    let Json(mut req) = match body {
        Ok(b) => b,
        Err(_) => {
            return wire_status(ApiError {
                code: "malformed",
                message: "invalid native task-run start body".into(),
                http_status: 400,
                retryable: false,
            });
        }
    };
    if let Err(e) = validate_submission_id(&req.submission_id) {
        return wire_status(e);
    }
    // The additive user-authored contract entries: validated and id-derived
    // HERE (typed 400s before any session resolution/dispatch), then merged
    // durably into the session's task row after a successful start.
    let contract = match build_user_contract(
        req.constraints.take(),
        req.invariants.take(),
        req.risks.take(),
        req.proof_obligations.take(),
    ) {
        Ok(c) => c,
        Err(e) => return wire_status(e),
    };
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    let sid = handle.id();
    // Saved-preset binding (M-05/M-06 runtime application): the envelope is
    // captured durably BEFORE any plan validation, so even a refused start
    // leaves a replay-proof binding. A read-only preset refuses mutating
    // plans; a plan-less mutating admission is refused explicitly.
    let mut bound_envelope: Option<faktor_foundation::presets::PresetEnvelope> = None;
    if let Some(preset_id) = req.preset_id.as_deref() {
        let foundation = match state.deps.foundation.as_deref() {
            Some(foundation) => foundation,
            None => {
                return wire_status(ApiError {
                    code: "unavailable",
                    message: "the foundation authority is not wired in this host".into(),
                    http_status: 503,
                    retryable: false,
                })
            }
        };
        let envelope = match foundation.presets.envelope_with(preset_id, &[]) {
            Ok(envelope) => envelope,
            Err(e) => return crate::native::foundation::foundation_error(e),
        };
        if let Err(e) = foundation
            .presets
            .bind(sid.raw(), &req.submission_id, &envelope)
        {
            return crate::native::foundation::foundation_error(e);
        }
        if !envelope.admission_mutation_allowed {
            let has_plan = req
                .work_items
                .as_ref()
                .is_some_and(|items| !items.is_empty());
            if !has_plan {
                return wire_status(ApiError {
                    code: "read_only_preset_needs_plan",
                    message:
                        "the bound preset is read-only; a mutating start requires an explicit plan"
                            .into(),
                    http_status: 400,
                    retryable: false,
                });
            }
            if req
                .work_items
                .as_ref()
                .is_some_and(|items| items.iter().any(|item| item.kind.is_mutating()))
            {
                return wire_status(ApiError {
                    code: "read_only_preset",
                    message: "the bound preset forbids mutating work at admission".into(),
                    http_status: 409,
                    retryable: false,
                });
            }
        }
        bound_envelope = Some(envelope.clone());
        if req.model.is_none() {
            let session_provider = match handle.row() {
                Ok(row) => row.provider,
                Err(e) => {
                    return wire_status(ApiError {
                        code: "internal",
                        message: format!("session row unreadable: {e}"),
                        http_status: 500,
                        retryable: false,
                    })
                }
            };
            let selector = if let Some(binding) = envelope.role_models.get("coder") {
                preset_model_selector(&binding.provider, &binding.model, &session_provider)
            } else if let (Some(provider), Some(model)) =
                (envelope.model_provider.as_ref(), envelope.model_id.as_ref())
            {
                preset_model_selector(provider, model, &session_provider)
            } else {
                Ok(None)
            };
            match selector {
                Ok(selector) => req.model = selector,
                Err(e) => return wire_status(e),
            }
        }
    }
    // Per-run routing overrides are refused until the daemon's routing
    // policy — the single routing authority — can honor them: a parsed-but-
    // ignored policy field would silently claim a mode the run never used.
    if req.routing_mode.is_some() {
        let e = ApiError {
            code: "unsupported",
            message:
                "routing_mode on a task start is not supported yet; the daemon routing policy is the single authority"
                    .into(),
            http_status: 400,
            retryable: false,
        };
        return wire_status(e);
    }
    // The DTO boundary: per-item ownership is explicit; the legacy
    // plan-global `ownership` value is converted exactly ONCE onto mutating
    // items that carry none of their own (never consulted by the runtime
    // afterwards). A mutating item that still holds NoWrites fails the
    // executor's plan validation loudly.
    //
    // Shadow 409 root cause: a session created by any surface before (or
    // without) creation-time registration carries the standalone default
    // worktree 1; the executor's owner adoption only runs when the
    // workspace holds NO worktree row at all, so a workspace with rows
    // left the session unbound and shadowed runs refused with a 409. The
    // registration is idempotent and self-healing for older sessions.
    if let Err(response) = write_tool_enablement(&state, &handle, bound_envelope.as_ref()) {
        return response;
    }
    if let Err(e) = state.deps.session.ensure_owner_worktree(sid) {
        return api_err(&e);
    }
    let prompts = PromptExecutionService::from_state(&state);
    let files = req.files.take().unwrap_or_default();
    // Additive attachment validation at the DTO boundary: the SAME one rule
    // the executor enforces (MAX_FILES_PER_PROMPT / MAX_FILE_PATH_BYTES +
    // the typed hostile-path refusal) is a plain 400 BEFORE any dispatch —
    // on both the explicit-work-items and the plain-prompt path.
    if let Err(e) = faktor_orchestrator::runtime::validate_attachment_files(&files) {
        return exec_error_response(&e);
    }
    // Binary attachments: every digest must resolve to a byte-identical
    // durable row of THIS session (uploaded first) and every IMAGE is
    // validated against the CHOSEN model's capabilities (vision, deliverable
    // mime, provider per-image bound) — BEFORE any shadow/task/run row, so a
    // refused start leaves no partial durable admission and the draft/bytes
    // remain intact.
    let attachments = req.attachments.take().unwrap_or_default();
    if let Err(e) = validate_wire_attachments(&state, &handle, req.model.as_deref(), &attachments) {
        return wire_status(e);
    }
    // P2: the plain-prompt path (`work_items` absent) carries no completion
    // contract seam; a non-default contract is refused loudly here, never
    // silently dropped. The default all-false contract is accepted and
    // changes nothing.
    if completion_contract_needs_work_items(req.work_items.is_some(), req.completion_contract) {
        return wire_status(ApiError {
            code: "unsupported",
            message: "completion_contract requires explicit work_items; the plain-prompt path carries no contract seam"
                .into(),
            http_status: 400,
            retryable: false,
        });
    }
    let receipt = match req.work_items {
        Some(items) => {
            let mut work_items: Vec<faktor_orchestrator::WorkItem> = items
                .into_iter()
                .map(|w| {
                    let mut item = faktor_orchestrator::WorkItem::with_ownership(
                        w.id,
                        w.summary.unwrap_or_default(),
                        w.kind,
                        w.ownership
                            .unwrap_or(faktor_orchestrator::OwnershipSpec::NoWrites),
                    );
                    item.depends_on = w.depends_on.unwrap_or_default();
                    item.acceptance_checks = w.acceptance_checks.unwrap_or_default();
                    if let Some(caps) = w.required_capabilities {
                        item.required_capabilities = caps;
                    }
                    item
                })
                .collect();
            if let Some(legacy) = &req.ownership {
                faktor_orchestrator::adopt_legacy_ownership(&mut work_items, legacy);
            }
            let request = faktor_orchestrator::runtime::task_executor::TaskRunRequest {
                goal: req.goal,
                work_items,
                submission_id: Some(req.submission_id),
                model: req.model,
                max_tokens: req.max_tokens,
                max_cost_micro: req.max_cost_micro,
                criteria: req.criteria.unwrap_or_default(),
                mutation_mode: req.mutation_mode,
                files,
                attachments: attachments.clone(),
                parent_caps: native_run_parent_caps(),
                completion_contract: req.completion_contract,
                ..Default::default()
            };
            let max_turns = session_turn_cap(&state, sid);
            match prompts.start_task(sid, request, max_turns) {
                Ok(r) => r,
                Err(e) => return exec_error_response(&e),
            }
        }
        None => {
            // The ordinary native prompt: one in-session mutating run
            // through the SAME PromptExecutionService (shadow by default).
            // Budget caps ride the SAME durable task row the executor seeds
            // for the prompt, so they are enforced instead of refused. A
            // zero cap stays absent (the historical "no override" shape),
            // exactly like the explicit task-start path.
            let request = PromptRequest {
                prompt: req.goal,
                submission_id: Some(req.submission_id),
                // The task-start admission inside the executor reserves its
                // own op id and computes the digest itself.
                reserved_op_id: None,
                admission_digest: None,
                files,
                attachments,
                model: req.model,
                max_tokens: req.max_tokens,
                max_cost_micro: req.max_cost_micro,
                max_turns: session_turn_cap(&state, sid),
                criteria: req.criteria.unwrap_or_default(),
                mutation_mode: req.mutation_mode,
            };
            let prompt_receipt = match prompts.prompt(sid, request).await {
                Ok(r) => r,
                Err(e) => return exec_error_response(&e),
            };
            faktor_orchestrator::runtime::task_executor::TaskRunReceipt {
                run_id: prompt_receipt.run_id,
                mode: prompt_receipt.mode,
                op_id: prompt_receipt.op_id,
                queued: prompt_receipt.queued,
            }
        }
    };
    // The additive engineering-contract merge: the task row exists by now
    // (the executor seeds/re-goals it synchronously at start) and the merge
    // is a union by id — existing entries and their claim states are
    // preserved, only NEW entries are appended, and a replayed identical
    // start changes nothing (no revision bump). Durable failures after the
    // run was already admitted are LOUD (never a silent drop of the user's
    // contract).
    if !contract.is_empty() {
        let task_id = match handle.row() {
            Ok(row) => row.task_id,
            Err(e) => return api_err(&e),
        };
        if let Err(e) = handle.merge_task_contract(task_id, &contract) {
            return task_contract_merge_response(&e);
        }
    }
    // The response carries the durable run identity + the run's own state
    // projection (the same derivation the list/state endpoints serve). An
    // orchestrated run's plan row is committed by the detached drive AFTER
    // this response is minted, so its receipt is answered directly (the
    // run converges through the list/state endpoints).
    let entry = match receipt.mode {
        faktor_orchestrator::runtime::task_executor::TaskRunMode::InSession => {
            match native_task_run_entry(&state, &handle, &receipt.run_id) {
                Ok(e) => e,
                Err(e) => return wire_status(e),
            }
        }
        faktor_orchestrator::runtime::task_executor::TaskRunMode::Orchestrated => {
            serde_json::json!({
                "task_id": handle
                    .row()
                    .map(|r| r.task_id.raw())
                    .unwrap_or(0),
                "run_id": receipt.run_id,
                "state": "Pending",
            })
        }
        // Only reachable when the worker plane is enabled: the run was
        // placed on a remote worker and no local run exists. The receipt
        // names the remote job; its durable state is served by
        // `GET /native/jobs/{id}`.
        faktor_orchestrator::runtime::task_executor::TaskRunMode::Remote => {
            serde_json::json!({
                "task_id": handle
                    .row()
                    .map(|r| r.task_id.raw())
                    .unwrap_or(0),
                "run_id": receipt.run_id,
                "state": "Remote",
            })
        }
    };
    Json(serde_json::json!({
        "task_id": entry.get("task_id").cloned().unwrap_or(serde_json::Value::Null),
        "run_id": entry.get("run_id").cloned().unwrap_or(serde_json::Value::Null),
        "state": entry.get("state").cloned().unwrap_or(serde_json::Value::Null),
    }))
    .into_response()
}

/// The native wire mapping of one durable contract-merge refusal (the DTO
/// already screened bounds, so these are durable-state causes): a missing
/// task row is 404, a frozen terminal row or a concurrent move is 409, and
/// any store failure is a loud 500 — the user's contract is never silently
/// dropped.
fn task_contract_merge_response(e: &faktor_session::TaskError) -> Response {
    use faktor_session::TaskError as T;
    let (code, status) = match e {
        T::NotFound(_) => ("not_found", 404),
        T::Oversized(_) => ("oversized", 400),
        T::Malformed(_) => ("malformed", 400),
        T::TerminalTask { .. } | T::RevisionMismatch { .. } | T::Conflict(_) => ("conflict", 409),
        _ => ("internal", 500),
    };
    wire_status(ApiError {
        code,
        message: e.to_string(),
        http_status: status,
        retryable: false,
    })
}

/// `POST /native/session/{id}/task-runs/{run_id}/cancel` — task-level
/// cancel of ONE run through the executor's single cancel authority
/// ([`TaskExecutor::cancel_run`]): an in-session run aborts its drive op,
/// cancels the session's task row and discards the run's shadow; an
/// orchestrated run fans a durable Cancel control to every non-terminal
/// child. Unknown runs are typed 404s; already-terminal runs are typed
/// 409s. Response state converges through the durable rows (the per-run
/// GET/list endpoints reflect it once applied).
pub(crate) async fn native_task_run_cancel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, run_id)): Path<(String, String)>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    match state.deps.tasks.cancel_run(handle.id(), &run_id) {
        Ok(()) => Json(serde_json::json!({
            "run_id": run_id,
            "cancelled": true,
        }))
        .into_response(),
        Err(e) => exec_error_response(&e),
    }
}

/// The strict request DTO of ONE native tournament start
/// (`POST /native/session/{id}/tournament`). `n` is the candidate count
/// (the executor enforces the typed 2..=4 band), `criteria` the acceptance
/// criteria fanned out byte-identically to every candidate, and
/// `mutation_mode` legacy vocabulary only (sole value: `shadow`;
/// candidates are always isolated worktrees). Unknown fields, typos, a
/// missing body field or an out-of-band `n` are plain 400s.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StartTournamentRequest {
    goal: String,
    criteria: Vec<String>,
    n: usize,
    model: Option<String>,
    max_tokens: Option<u64>,
    /// Money input: decimal string or legacy JSON integer (see
    /// `faktor_cloud::money`).
    #[serde(default, with = "faktor_cloud::money::option")]
    max_cost_micro: Option<u64>,
    mutation_mode: Option<faktor_orchestrator::runtime::task_executor::MutationMode>,
    files: Option<Vec<String>>,
}

/// The capability ceiling of a tournament's parent: read + write on the
/// whole workspace — candidate writes land ONLY in daemon-owned isolated
/// worktrees, and integration stays the explicit approved-merge path. The
/// per-tool permission requester still gates every actual call.
pub(crate) fn native_tournament_parent_caps() -> faktor_orchestrator::caps::CapabilitySet {
    faktor_orchestrator::runtime::task_executor::child_caps(
        faktor_orchestrator::WorkKind::Implementation,
    )
}

/// `POST /native/session/{id}/tournament` — start a multi-candidate
/// implementation tournament (N = 2..=4) through the executor's ONE
/// tournament entry: N isolated candidate worktrees + children under the
/// existing executor/assignments machinery, all receiving the identical
/// goal+criteria, plus the durable `TournamentStarted` ledger anchor. The
/// response is the durable tournament state (unknown/hostile bodies are
/// 400s; no automatic integration happens at any point).
pub(crate) async fn native_tournament_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<StartTournamentRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    // Strict native DTO: every body rejection (syntax AND data errors —
    // unknown fields, typos, bad enum values, missing fields) is a plain
    // 400, never a 422.
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => {
            return wire_status(ApiError {
                code: "malformed",
                message: "invalid native tournament start body".into(),
                http_status: 400,
                retryable: false,
            });
        }
    };
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    let sid = handle.id();
    // Same self-healing owner registration as the task-run start (older
    // sessions may carry the standalone default worktree id); tournament
    // candidate children need a real owner worktree.
    if let Err(e) = state.deps.session.ensure_owner_worktree(sid) {
        return api_err(&e);
    }
    // Additive attachment validation (the ONE shared rule): hostile or
    // oversized candidate file lists are plain 400s before any durable row.
    if let Err(e) =
        faktor_orchestrator::runtime::validate_attachment_files(req.files.as_deref().unwrap_or(&[]))
    {
        return exec_error_response(&e);
    }
    let request = faktor_orchestrator::runtime::task_executor::TournamentStartRequest {
        goal: req.goal,
        criteria: req.criteria,
        n: req.n,
        mutation_mode: req.mutation_mode,
        model: req.model,
        max_tokens: req.max_tokens,
        max_cost_micro: req.max_cost_micro,
        files: req.files.unwrap_or_default(),
        parent_caps: native_tournament_parent_caps(),
        ..Default::default()
    };
    let receipt = match state.deps.tasks.start_tournament_with(sid, request) {
        Ok(r) => r,
        Err(e) => return exec_error_response(&e),
    };
    // Converge through the durable rows: the just-persisted anchor is read
    // back so the response is the real tournament state, never a phantom.
    match state
        .deps
        .tasks
        .tournament_state(sid, &receipt.tournament_id)
    {
        Ok(tournament) => money_json(serde_json::json!({
            "tournament_id": receipt.tournament_id,
            "run_id": receipt.run_id,
            "candidates": receipt.candidates,
            "state": tournament.state,
            "winner": tournament.winner,
        })),
        Err(e) => exec_error_response(&e),
    }
}

/// Serialize one durable tournament (its candidates carry `cost_micro`)
/// through the native money encoding: money-named keys are decimal strings
/// even though the orchestrator's own serde shape is shared with durable
/// ledger rows.
fn tournament_json(tournament: &faktor_orchestrator::tournament::Tournament) -> Response {
    match serde_json::to_value(tournament) {
        Ok(value) => money_json(value),
        Err(e) => wire_status(internal_graph_err(format!(
            "tournament state serialization: {e}"
        ))),
    }
}

/// `GET /native/session/{id}/tournament/{tournament_id}` — the durable
/// state of ONE tournament reconstructed from its ledger rows
/// (`TournamentStarted` + settlements + decision), with running candidates
/// refreshed from the run's registry rows. Unknown ids are typed 404s.
pub(crate) async fn native_tournament_state(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, tournament_id)): Path<(String, String)>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    match state
        .deps
        .tasks
        .tournament_state(handle.id(), &tournament_id)
    {
        Ok(tournament) => tournament_json(&tournament),
        Err(e) => exec_error_response(&e),
    }
}

/// The native wire mapping of one tournament error (the executor's private
/// mapping mirrored for the additive listing read: unknown ids are 404s,
/// corrupt/ledger failures are loud 500s, typed shape refusals are 400s).
fn tournament_error_response(e: &faktor_orchestrator::tournament::TournamentError) -> Response {
    use faktor_orchestrator::tournament::TournamentError as T;
    let (code, status) = match e {
        T::NotFound(_) => ("not_found", 404),
        T::Oversized(_) => ("oversized", 400),
        T::NotOpen(_) | T::DuplicateSettlement(_) | T::NoEligibleWinner(_) => ("conflict", 409),
        T::Corrupt { .. } | T::Ledger(_) | T::Cleanup(_) => ("internal", 500),
        T::InvalidCandidateCount { .. }
        | T::InvalidCriteriaCount { .. }
        | T::InvalidCriterion(_)
        | T::DuplicateCriterion(_)
        | T::InvalidId(_)
        | T::UnknownCandidate(_)
        | T::IllegalSettlementState(_)
        | T::VerificationSpecDrift
        | T::ReviewNotIndependent(_) => ("malformed", 400),
    };
    wire_status(ApiError {
        code,
        message: e.to_string(),
        http_status: status,
        retryable: false,
    })
}

/// Max bytes of a native abort reason (bounded everything; the rationale
/// persisted on the durable decision row is never operator-unbounded).
pub(crate) const MAX_ABORT_REASON_BYTES: usize = 512;

/// Strict request DTO of the additive tournament decide route
/// (`POST /native/session/{id}/tournaments/{tournament_id}/decide`): the
/// engine's comparison takes NO operator input, so the body is exactly `{}`
/// (a typo, a hostile member or a missing body is a plain 400).
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeTournamentDecideBody {}

/// Strict request DTO of the additive tournament abort route
/// (`POST /native/session/{id}/tournaments/{tournament_id}/abort`): an
/// optional bounded `reason` recorded verbatim on the durable decision row.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeTournamentAbortBody {
    #[serde(default)]
    reason: Option<String>,
}

/// `POST /native/session/{id}/tournaments/{tournament_id}/decide` — run the
/// deterministic comparison of ONE durable tournament through the executor's
/// ONE decide authority ([`faktor_orchestrator::runtime::task_executor::TaskExecutor::decide_tournament`]):
/// the `TournamentDecided` audit row is persisted, every loser is discarded
/// and the PROPOSED winner is returned (integration stays the explicit
/// approved-merge path). Engine refusals are typed: unknown ids 404, a
/// non-open tournament or no eligible candidate 409 (the candidate band must
/// have settled with a passing verification and an independent review).
/// Strict body (`{}` only); hostile bodies are 400s.
pub(crate) async fn native_tournament_decide(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, tournament_id)): Path<(String, String)>,
    body: Result<Json<NativeTournamentDecideBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(_) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid native tournament decide body")),
    };
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    match state
        .deps
        .tasks
        .decide_tournament(handle.id(), &tournament_id)
    {
        Ok(decision) => Json(serde_json::json!({
            "tournament_id": decision.tournament_id,
            "winner": decision.winner.child_id,
            "rationale": decision.rationale,
            "discarded": decision
                .discarded
                .iter()
                .map(|(child_id, reason)| serde_json::json!({
                    "child_id": child_id,
                    "reason": reason,
                }))
                .collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => exec_error_response(&e),
    }
}

/// `POST /native/session/{id}/tournaments/{tournament_id}/abort` — discard
/// ONE tournament through the executor's ONE abort authority
/// ([`faktor_orchestrator::runtime::task_executor::TaskExecutor::abort_tournament`]):
/// every candidate is settled terminal + its worktree removed, the terminal
/// `TournamentDecided { outcome: aborted }` row records the reason and no
/// winner is proposed. The response is the reconstructed durable tournament.
/// Unknown ids are 404s; a terminal tournament is a typed 409. Strict body
/// (`{"reason"?: "..."}`, bounded); hostile bodies are 400s.
pub(crate) async fn native_tournament_abort(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, tournament_id)): Path<(String, String)>,
    body: Result<Json<NativeTournamentAbortBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid native tournament abort body")),
    };
    let reason = body.reason.unwrap_or_default();
    if reason.len() > MAX_ABORT_REASON_BYTES {
        return wire_status(malformed_body(&format!(
            "abort reason must be <= {MAX_ABORT_REASON_BYTES} bytes"
        )));
    }
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    match state
        .deps
        .tasks
        .abort_tournament(handle.id(), &tournament_id, &reason)
    {
        Ok(tournament) => tournament_json(&tournament),
        Err(e) => exec_error_response(&e),
    }
}

/// `GET /native/session/{id}/tournaments` — the durable listing of the
/// session's tournaments (id, state, candidate count, winner, decided_ms),
/// folded from the typed ledger rows (`TournamentStarted` + settlements +
/// `TournamentDecided`). Open tournaments carry `winner: null` and
/// `decided_ms: null`; corrupt rows are loud 500s, never a silently partial
/// list. Hostile sessions are typed (malformed 400 / unknown 404).
pub(crate) async fn native_tournaments_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    match faktor_orchestrator::tournament::Tournament::summaries(&handle) {
        Ok(list) => Json(list).into_response(),
        Err(e) => tournament_error_response(&e),
    }
}

#[cfg(test)]
mod run_mode_classification_tests {
    //! The task-run mode classification is a membership question scanned
    //! ONCE per projection: this pins the exact classification vocabulary
    //! and the bounded-work property at the 1k-run scan size.
    use super::{orchestrated_run_ids, run_state_tag_for, TASK_RUN_ROW_KIND};

    fn fact(kind: &str, key: &str) -> (String, String, String) {
        (kind.to_string(), key.to_string(), "{}".to_string())
    }

    #[test]
    fn non_terminal_task_phases_never_render_as_done() {
        use faktor_core::state::{AgentState, TaskState};
        assert_eq!(
            run_state_tag_for(None, AgentState::ReadyForNextTurn),
            "Done",
            "no task row keeps the historical session-tag fallback"
        );
        assert_eq!(
            run_state_tag_for(Some(TaskState::Verifying), AgentState::ReadyForNextTurn),
            "Verifying"
        );
        assert_eq!(
            run_state_tag_for(
                Some(TaskState::NeedsVerification),
                AgentState::ReadyForNextTurn
            ),
            "Verifying"
        );
        assert_eq!(
            run_state_tag_for(Some(TaskState::Blocked), AgentState::ReadyForNextTurn),
            "Blocked"
        );
        assert_eq!(
            run_state_tag_for(Some(TaskState::Waiting), AgentState::ReadyForNextTurn),
            "Waiting"
        );
        assert_eq!(
            run_state_tag_for(Some(TaskState::Running), AgentState::ReadyForNextTurn),
            "Done",
            "a stale Running row with an idle turn machine is the run's completion"
        );
        assert_eq!(
            run_state_tag_for(Some(TaskState::Running), AgentState::Streaming),
            "Running",
            "a genuinely live turn still renders running"
        );
        assert_eq!(
            run_state_tag_for(Some(TaskState::VerifiedComplete), AgentState::Idle),
            "Done"
        );
        assert_eq!(
            run_state_tag_for(Some(TaskState::Failed), AgentState::ReadyForNextTurn),
            "Failed"
        );
        assert_eq!(
            run_state_tag_for(Some(TaskState::Cancelled), AgentState::Cancelled),
            "Cancelled"
        );
    }

    #[test]
    fn modes_classify_by_membership_in_one_facts_scan() {
        let mut facts: Vec<(String, String, String)> = Vec::new();
        for run in 0..256 {
            facts.push(fact(TASK_RUN_ROW_KIND, &format!("run-{run}")));
        }
        facts.push(fact("other-kind", "run-0"));
        facts.push(fact("orchestrator", "run-9"));
        let orchestrated = orchestrated_run_ids(&facts);
        assert_eq!(orchestrated.len(), 256, "only the run row kind counts");
        assert!(orchestrated.contains("run-0"));
        assert!(orchestrated.contains("run-255"));
        // A foreign row kind that names a run id never reclassifies it.
        assert!(!orchestrated.contains("run-256"));

        // Bounded deterministic workload: 1k runs against 10k facts. The
        // classification result is asserted exactly; the elapsed time is
        // printed for the audit report (the historical nested scan and this
        // single-pass set build answer identically).
        let runs: Vec<String> = (0..1000).map(|i| format!("scan-run-{i}")).collect();
        let mut big_facts: Vec<(String, String, String)> = Vec::new();
        for chunk in 0..8 {
            for run in 0..1000usize {
                big_facts.push(fact("other-kind", &format!("noise-{chunk}-{run}")));
            }
        }
        for run in (0..1000).step_by(2) {
            big_facts.push(fact(TASK_RUN_ROW_KIND, &format!("scan-run-{run}")));
        }
        // The historical per-run nested scan, replayed verbatim on the same
        // workload so both answers and both costs are visible in one run.
        let started = std::time::Instant::now();
        let mut historical = Vec::with_capacity(runs.len());
        for run in &runs {
            historical.push(
                big_facts
                    .iter()
                    .any(|(kind, key, _)| kind == TASK_RUN_ROW_KIND && key == run),
            );
        }
        let historical_elapsed = started.elapsed();

        let started = std::time::Instant::now();
        let set = orchestrated_run_ids(&big_facts);
        let mut classified = Vec::with_capacity(runs.len());
        for run in &runs {
            classified.push(set.contains(run.as_str()));
        }
        let elapsed = started.elapsed();
        eprintln!(
            "task-run modes: 1k runs x 10k facts: nested {historical_elapsed:?} vs set {elapsed:?}"
        );
        assert_eq!(historical, classified);
        for (index, run) in runs.iter().enumerate() {
            assert_eq!(classified[index], index % 2 == 0, "{run}");
        }
    }
}

#[cfg(test)]
mod attachment_dto_tests {
    //! Additive cover of the task-start attachment validation: the SAME one
    //! rule the executor enforces is applied at the DTO boundary and maps to
    //! a plain 400 (via [`super::exec_error_response`]) before any dispatch —
    //! hostile and oversized lists never reach a spawn.
    use super::exec_error_response;
    use faktor_orchestrator::runtime::validate_attachment_files;

    fn status_of(files: &[String]) -> u16 {
        let err = validate_attachment_files(files).expect_err("hostile/oversized list must refuse");
        exec_error_response(&err).status().as_u16()
    }

    #[test]
    fn task_attachment_lists_are_validated_with_the_shared_rule() {
        // Workspace-relative paths (including nested and dot segments) pass.
        assert!(validate_attachment_files(&[
            "src/a.rs".to_string(),
            "docs/sub/b.md".to_string(),
            "./c.txt".to_string(),
        ])
        .is_ok());
        // Oversized COUNT is a typed 400.
        let many: Vec<String> = (0..faktor_session::MAX_FILES_PER_PROMPT + 1)
            .map(|i| format!("f{i}.rs"))
            .collect();
        assert_eq!(status_of(&many), 400, "count over MAX_FILES_PER_PROMPT");
        // Oversized PATH is a typed 400.
        let huge = vec!["x".repeat(faktor_session::MAX_FILE_PATH_BYTES + 1)];
        assert_eq!(status_of(&huge), 400, "path over MAX_FILE_PATH_BYTES");
        // Hostile paths are typed 400s: absolute, traversal, empty,
        // control-character, and Windows drive-prefixed.
        for hostile in [
            "/etc/passwd",
            "../secrets",
            "src/../../secrets",
            "",
            "   ",
            "bad\0path",
            "C:\\Windows",
        ] {
            assert_eq!(status_of(&[hostile.to_string()]), 400, "{hostile:?}");
        }
    }

    /// The additive binary `attachments` DTO member is STRICT and SEPARATE
    /// from `files`: a valid typed id parses, unknown/missing/hostile
    /// members are 400s, and a typo of the field name never silently drops
    /// the attachments.
    #[test]
    fn task_binary_attachments_are_strict_and_separate_from_files() {
        use super::StartTaskRunRequest;
        let digest = "a".repeat(64);
        let valid = serde_json::json!({
            "goal": "g",
            "submission_id": "11111111-1111-4111-8111-111111111111",
            "files": ["src/a.rs"],
            "attachments": [
                {"digest": digest, "mime": "application/pdf", "filename": "spec.pdf", "size": 7},
            ],
        });
        let req: StartTaskRunRequest = serde_json::from_value(valid).unwrap();
        let attachments = req.attachments.expect("attachments parsed");
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].mime, "application/pdf");
        assert_eq!(req.files.as_deref(), Some(&["src/a.rs".to_string()][..]));
        // A missing REQUIRED id member (`size`) is a serde 400 (never a
        // silently partial id); `filename` is legitimately optional.
        let missing = serde_json::json!({
            "goal": "g",
            "submission_id": "11111111-1111-4111-8111-111111111111",
            "attachments": [{"digest": digest, "mime": "application/pdf"}],
        });
        assert!(serde_json::from_value::<StartTaskRunRequest>(missing).is_err());
        // An unknown id member is a 400 (AttachmentId is deny_unknown_fields).
        let unknown_member = serde_json::json!({
            "goal": "g",
            "submission_id": "11111111-1111-4111-8111-111111111111",
            "attachments": [
                {"digest": digest, "mime": "application/pdf", "filename": null, "size": 7, "url": "data:x"},
            ],
        });
        assert!(serde_json::from_value::<StartTaskRunRequest>(unknown_member).is_err());
        // A typo of the field name is a 400: the DTO never silently drops
        // attachments into a run that never declared them.
        let typo = serde_json::json!({
            "goal": "g",
            "submission_id": "11111111-1111-4111-8111-111111111111",
            "attachment": [{"digest": digest, "mime": "application/pdf", "filename": null, "size": 7}],
        });
        assert!(serde_json::from_value::<StartTaskRunRequest>(typo).is_err());
        // A malformed digest (not 64 hex) is a 400.
        let bad_digest = serde_json::json!({
            "goal": "g",
            "submission_id": "11111111-1111-4111-8111-111111111111",
            "attachments": [{"digest": "zz", "mime": "application/pdf", "filename": null, "size": 7}],
        });
        assert!(serde_json::from_value::<StartTaskRunRequest>(bad_digest).is_err());
    }
}

#[cfg(test)]
mod completion_contract_dto_tests {
    //! Adversarial strict-DTO covers for the additive `completion_contract`
    //! field. Every `Err` below is mapped to a plain 400 by
    //! [`native_task_run_start`]'s uniform body-rejection handling (never a
    //! 422, never a silent default).
    use super::{completion_contract_needs_work_items, StartTaskRunRequest};
    use faktor_core::completion::CompletionContract;

    fn parse(value: serde_json::Value) -> Result<StartTaskRunRequest, String> {
        serde_json::from_value(value).map_err(|e| e.to_string())
    }

    /// The rejection message of a hostile body (the DTO itself is not
    /// `Debug`; only the error shape matters here).
    fn expect_err(value: serde_json::Value) -> String {
        match parse(value.clone()) {
            Ok(_) => panic!("hostile DTO must be rejected: {value}"),
            Err(e) => e,
        }
    }

    /// The plain-prompt path (no `work_items`) refuses a non-default
    /// contract with a typed 400 instead of silently dropping it; the
    /// default contract and the explicit-work-items path are accepted.
    #[test]
    fn non_default_contract_on_the_plain_prompt_path_is_a_typed_400() {
        let push = CompletionContract {
            include_commit: false,
            include_push: true,
            include_pr: false,
        };
        assert!(completion_contract_needs_work_items(false, Some(push)));
        assert!(!completion_contract_needs_work_items(true, Some(push)));
        assert!(!completion_contract_needs_work_items(
            false,
            Some(CompletionContract::default())
        ));
        assert!(!completion_contract_needs_work_items(false, None));
    }

    #[test]
    fn completion_contract_is_strict_typed_and_never_defaults_silently() {
        // Absent = today's behavior (no contract, no durable row, no gate).
        let req = parse(serde_json::json!({
            "goal": "g",
            "submission_id": "11111111-1111-4111-8111-111111111111",
        }))
        .unwrap();
        assert!(req.completion_contract.is_none());
        // An explicit all-false contract parses and IS the default behavior.
        let req = parse(serde_json::json!({
            "goal": "g",
            "submission_id": "11111111-1111-4111-8111-111111111111",
            "completion_contract": {
                "include_commit": false,
                "include_push": false,
                "include_pr": false,
            },
        }))
        .unwrap();
        assert_eq!(req.completion_contract, Some(CompletionContract::default()));
        // A full non-default contract parses.
        let req = parse(serde_json::json!({
            "goal": "g",
            "submission_id": "11111111-1111-4111-8111-111111111111",
            "completion_contract": {
                "include_commit": true,
                "include_push": true,
                "include_pr": true,
            },
        }))
        .unwrap();
        assert_eq!(
            req.completion_contract,
            Some(CompletionContract {
                include_commit: true,
                include_push: true,
                include_pr: true,
            })
        );

        // A missing member is a 400, never a silent false.
        let err = expect_err(serde_json::json!({
            "goal": "g",
            "submission_id": "11111111-1111-4111-8111-111111111111",
            "completion_contract": {"include_commit": true},
        }));
        assert!(err.contains("missing field"), "{err}");
        // A non-boolean member is a 400.
        let err = expect_err(serde_json::json!({
            "goal": "g",
            "submission_id": "11111111-1111-4111-8111-111111111111",
            "completion_contract": {
                "include_commit": "yes",
                "include_push": false,
                "include_pr": false,
            },
        }));
        assert!(err.contains("invalid type"), "{err}");
        // An unknown member inside the contract is a 400.
        let err = expect_err(serde_json::json!({
            "goal": "g",
            "submission_id": "11111111-1111-4111-8111-111111111111",
            "completion_contract": {
                "include_commit": true,
                "include_push": false,
                "include_pr": false,
                "include_release": true,
            },
        }));
        assert!(err.contains("unknown field"), "{err}");
        // A non-object contract (string / bool / array) is a 400.
        for hostile in [
            serde_json::json!("include_push"),
            serde_json::json!(true),
            serde_json::json!([true, true, true]),
        ] {
            let err = expect_err(serde_json::json!({
                "goal": "g",
                "submission_id": "11111111-1111-4111-8111-111111111111",
                "completion_contract": hostile,
            }));
            assert!(err.contains("invalid type"), "{err}");
        }
        // An unknown TOP-LEVEL field is still a 400 (the DTO strictness is
        // unchanged by the additive field).
        let err = expect_err(serde_json::json!({
            "goal": "g",
            "submission_id": "11111111-1111-4111-8111-111111111111",
            "completion_contracts": {"include_commit": true},
        }));
        assert!(err.contains("unknown field"), "{err}");
    }

    #[test]
    fn explicit_work_items_parse_and_are_not_silently_dropped() {
        let req = parse(serde_json::json!({
            "goal": "g",
            "submission_id": "11111111-1111-4111-8111-111111111111",
            "work_items": [{
                "id": "w1",
                "kind": "Implementation",
                "summary": "s",
                "ownership": "isolated_worktree",
            }],
            "completion_contract": {
                "include_commit": true,
                "include_push": false,
                "include_pr": false,
            },
        }))
        .expect("explicit work_items must parse");
        assert!(
            req.work_items.is_some(),
            "explicit work_items must reach the handler, not become None"
        );
        assert!(
            !completion_contract_needs_work_items(
                req.work_items.is_some(),
                req.completion_contract
            ),
            "explicit work_items plus a non-default contract must be accepted"
        );
        assert!(
            completion_contract_needs_work_items(
                None::<Vec<super::NativeTaskRunWorkItem>>.is_some(),
                req.completion_contract
            ),
            "the plain-prompt path with a non-default contract must be refused"
        );
    }
}

#[cfg(test)]
mod submission_id_dto_tests {
    //! Adversarial strict-DTO cover for the REQUIRED `submission_id` of a
    //! native task start: absent, empty, oversized, uppercase, non-hex and
    //! non-ASCII shapes are all typed refusals (mapped to plain 400s by the
    //! handler), never a silently generated or dropped key.
    use super::{validate_submission_id, StartTaskRunRequest};

    fn parse(value: serde_json::Value) -> Result<StartTaskRunRequest, String> {
        serde_json::from_value(value).map_err(|e| e.to_string())
    }

    #[test]
    fn submission_id_is_required_and_survives_a_valid_parse() {
        let valid = serde_json::json!({
            "goal": "g",
            "submission_id": "11111111-1111-4111-8111-111111111111",
        });
        let req = parse(valid).expect("a UUID-shaped submission id parses");
        assert_eq!(req.submission_id, "11111111-1111-4111-8111-111111111111");
        // Missing is a typed parse error (never a silent fresh key).
        let err = match parse(serde_json::json!({"goal": "g"})) {
            Ok(_) => panic!("missing submission_id must refuse"),
            Err(e) => e,
        };
        assert!(err.contains("missing field"), "{err}");
        // A typo of the field name is a 400, never a dropped key.
        let err = match parse(serde_json::json!({
            "goal": "g",
            "submissionId": "11111111-1111-4111-8111-111111111111",
        })) {
            Ok(_) => panic!("a camelCase typo must refuse"),
            Err(e) => e,
        };
        assert!(err.contains("unknown field"), "{err}");
        // `null` is not a string.
        assert!(parse(serde_json::json!({"goal": "g", "submission_id": null})).is_err());
    }

    #[test]
    fn hostile_submission_id_shapes_are_typed_400s() {
        for hostile in [
            "",
            " ",
            "11111111-1111-4111-8111-11111111111Z",
            "11111111-1111-4111-8111-1111111111_1",
            "..111111-1111-4111-8111-111111111111",
            "no-uuid-here",
            "☃",
        ] {
            let err =
                validate_submission_id(hostile).expect_err("hostile shape must be a typed refusal");
            assert_eq!(err.http_status, 400, "{hostile:?}");
            assert_eq!(err.code, "malformed", "{hostile:?}");
        }
        let oversized =
            "a".repeat(faktor_orchestrator::runtime::task_executor::MAX_SUBMISSION_ID_BYTES + 1);
        assert!(validate_submission_id(&oversized).is_err());
        let at_bound =
            "a".repeat(faktor_orchestrator::runtime::task_executor::MAX_SUBMISSION_ID_BYTES);
        assert!(validate_submission_id(&at_bound).is_ok());
        assert!(validate_submission_id("11111111-1111-4111-8111-111111111111").is_ok());
        assert!(validate_submission_id("deadbeef-dead-beef-dead-beefdeadbeef").is_ok());
    }
}

#[cfg(test)]
mod money_dto_tests {
    //! The `max_cost_micro` inputs accept the wire decimal string AND the
    //! legacy JSON integer at every boundary; malformed, negative and
    //! overflowing forms are typed serde errors (the handlers map them to
    //! plain 400s). See `faktor_cloud::money` for the encoding rule.
    use super::{StartTaskRunRequest, StartTournamentRequest};
    use crate::native::agents::NativeBudgetBody;

    const BOUNDARIES: [u64; 4] = [0, (1 << 53) - 1, 1 << 53, i64::MAX as u64];

    #[test]
    fn max_cost_micro_accepts_decimal_strings_and_legacy_numbers() {
        for amount in BOUNDARIES {
            let req: StartTaskRunRequest = serde_json::from_value(serde_json::json!({
                "goal": "g",
                "submission_id": "11111111-1111-4111-8111-111111111111",
                "max_cost_micro": amount.to_string(),
            }))
            .unwrap();
            assert_eq!(req.max_cost_micro, Some(amount), "string form");
            let req: StartTaskRunRequest = serde_json::from_value(serde_json::json!({
                "goal": "g",
                "submission_id": "11111111-1111-4111-8111-111111111111",
                "max_cost_micro": amount,
            }))
            .unwrap();
            assert_eq!(req.max_cost_micro, Some(amount), "legacy number form");
            let req: StartTaskRunRequest = serde_json::from_value(serde_json::json!({
                "goal": "g",
                "submission_id": "11111111-1111-4111-8111-111111111111",
            }))
            .unwrap();
            assert_eq!(req.max_cost_micro, None, "absent = no cap");
            let req: StartTaskRunRequest = serde_json::from_value(serde_json::json!({
                "goal": "g",
                "submission_id": "11111111-1111-4111-8111-111111111111",
                "max_cost_micro": null,
            }))
            .unwrap();
            assert_eq!(req.max_cost_micro, None, "null = no cap");

            let tournament: StartTournamentRequest = serde_json::from_value(serde_json::json!({
                "goal": "g",
                "criteria": ["c"],
                "n": 2,
                "max_cost_micro": amount.to_string(),
            }))
            .unwrap();
            assert_eq!(tournament.max_cost_micro, Some(amount));
            let tournament: StartTournamentRequest = serde_json::from_value(serde_json::json!({
                "goal": "g",
                "criteria": ["c"],
                "n": 2,
                "max_cost_micro": amount,
            }))
            .unwrap();
            assert_eq!(tournament.max_cost_micro, Some(amount));

            assert!(
                serde_json::from_value::<NativeBudgetBody>(serde_json::json!({
                    "max_cost_micro": amount.to_string(),
                }))
                .is_ok()
            );
            assert!(serde_json::from_value::<NativeBudgetBody>(
                serde_json::json!({"max_cost_micro": amount})
            )
            .is_ok());
        }
    }

    #[test]
    fn max_cost_micro_refuses_malformed_negative_and_overflowing_input() {
        for bad in [
            serde_json::json!("abc"),
            serde_json::json!(""),
            serde_json::json!("-1"),
            serde_json::json!("+1"),
            serde_json::json!("1.5"),
            serde_json::json!(" 1"),
            serde_json::json!("1e3"),
            serde_json::json!("18446744073709551616"),
            serde_json::json!(-1),
            serde_json::json!(1.5),
            serde_json::json!(1e3),
            serde_json::json!(true),
            serde_json::json!([]),
        ] {
            assert!(
                serde_json::from_value::<StartTaskRunRequest>(serde_json::json!({
                    "goal": "g",
                    "submission_id": "11111111-1111-4111-8111-111111111111",
                    "max_cost_micro": bad,
                }))
                .is_err(),
                "task start must refuse {bad}"
            );
            assert!(
                serde_json::from_value::<StartTournamentRequest>(serde_json::json!({
                    "goal": "g",
                    "criteria": ["c"],
                    "n": 2,
                    "max_cost_micro": bad,
                }))
                .is_err(),
                "tournament must refuse {bad}"
            );
            assert!(
                serde_json::from_value::<NativeBudgetBody>(
                    serde_json::json!({"max_cost_micro": bad})
                )
                .is_err(),
                "agent budget must refuse {bad}"
            );
        }
    }
}

#[cfg(test)]
mod engineering_contract_dto_tests {
    //! Adversarial strict-DTO covers for the additive user-authored contract
    //! arrays of a native task start. Every `Err` below is mapped to a plain
    //! 400 by [`native_task_run_start`]'s uniform body/validation handling
    //! (never a 422, never a silent truncation), and the daemon derives all
    //! ids so a repeated start unions instead of duplicating.
    use super::{build_user_contract, ProofObligationInput, StartTaskRunRequest};
    use faktor_core::contract::{ClaimState, ContractOrigin, ProofEvidenceKind, RiskSeverity};

    fn parse(value: serde_json::Value) -> StartTaskRunRequest {
        serde_json::from_value(value).expect("valid bodies parse")
    }

    fn parse_err(value: serde_json::Value) -> String {
        match serde_json::from_value::<StartTaskRunRequest>(value.clone()) {
            Ok(_) => panic!("hostile body must be refused: {value}"),
            Err(e) => e.to_string(),
        }
    }

    fn base_body() -> serde_json::Value {
        serde_json::json!({
            "goal": "g",
            "submission_id": "11111111-1111-4111-8111-111111111111",
        })
    }

    #[test]
    fn contract_arrays_parse_and_ids_are_derived_deterministically() {
        let mut req = parse(serde_json::json!({
            "goal": "g",
            "submission_id": "11111111-1111-4111-8111-111111111111",
            "constraints": ["existing sessions remain valid"],
            "invariants": ["revoked sessions cannot authenticate"],
            "risks": ["race conditions"],
            "proof_obligations": [
                "replayed refresh tokens fail",
                {"claim": "legacy sessions survive the migration", "evidence": "compile"},
            ],
        }));
        let contract = build_user_contract(
            req.constraints.take(),
            req.invariants.take(),
            req.risks.take(),
            req.proof_obligations.take(),
        )
        .unwrap();
        assert_eq!(contract.constraints.len(), 1);
        let constraint = &contract.constraints[0];
        assert!(constraint.id.starts_with("c1:"), "{}", constraint.id);
        assert_eq!(constraint.id.len(), 3 + 16);
        assert_eq!(constraint.origin, ContractOrigin::User);
        assert!(constraint.id.bytes().all(|b| b.is_ascii()));
        assert!(contract.invariants[0].id.starts_with("i1:"));
        assert!(contract.risks[0].id.starts_with("r1:"));
        assert_eq!(contract.risks[0].severity, RiskSeverity::Unknown);
        assert_eq!(contract.risks[0].origin, ContractOrigin::User);
        // Plain claims default their evidence to test_execution; the object
        // form states it explicitly; both get canonical obligation ids.
        assert_eq!(contract.proof_obligations.len(), 2);
        assert_eq!(
            contract.proof_obligations[0].evidence,
            ProofEvidenceKind::TestExecution
        );
        assert_eq!(
            contract.proof_obligations[1].evidence,
            ProofEvidenceKind::Compile
        );
        for obligation in &contract.proof_obligations {
            assert_eq!(
                obligation.id,
                faktor_core::contract::EngineeringContract::obligation_id(
                    &obligation.claim,
                    obligation.evidence
                )
            );
            assert!(obligation.id.starts_with("po1:"));
            assert_eq!(obligation.state, ClaimState::Unknown);
            assert_eq!(obligation.origin, ContractOrigin::User);
        }
        // Deterministic: a repeated parse derives identical ids.
        let mut req2 = parse(serde_json::json!({
            "goal": "g",
            "submission_id": "22222222-2222-4222-8222-222222222222",
            "constraints": ["existing sessions remain valid"],
            "invariants": ["revoked sessions cannot authenticate"],
            "risks": ["race conditions"],
            "proof_obligations": ["replayed refresh tokens fail"],
        }));
        let again = build_user_contract(
            req2.constraints.take(),
            req2.invariants.take(),
            req2.risks.take(),
            req2.proof_obligations.take(),
        )
        .unwrap();
        assert_eq!(again.constraints[0].id, constraint.id);
        assert_eq!(again.invariants[0].id, contract.invariants[0].id);
        assert_eq!(again.risks[0].id, contract.risks[0].id);
        assert_eq!(
            again.proof_obligations[0].id,
            contract.proof_obligations[0].id
        );
    }

    #[test]
    fn duplicate_claims_dedupe_by_obligation_id() {
        let mut req = parse(serde_json::json!({
            "goal": "g",
            "submission_id": "11111111-1111-4111-8111-111111111111",
            "constraints": ["same constraint", "same constraint"],
            "proof_obligations": [
                "same claim",
                "same claim",
                {"claim": "same claim", "evidence": "test_execution"},
                {"claim": "same claim", "evidence": "compile"},
            ],
        }));
        assert_eq!(req.constraints.as_ref().unwrap().len(), 2);
        let contract = build_user_contract(
            req.constraints.take(),
            req.invariants.take(),
            req.risks.take(),
            req.proof_obligations.take(),
        )
        .unwrap();
        assert_eq!(contract.constraints.len(), 1, "duplicates dedupe by id");
        // `same claim` + test_execution dedupes; compile is a DIFFERENT
        // obligation id (domain-separated by evidence kind).
        assert_eq!(contract.proof_obligations.len(), 2);
        assert!(contract
            .proof_obligations
            .iter()
            .any(|o| o.evidence == ProofEvidenceKind::TestExecution));
        assert!(contract
            .proof_obligations
            .iter()
            .any(|o| o.evidence == ProofEvidenceKind::Compile));
    }

    #[test]
    fn contract_bounds_and_hostile_shapes_are_typed_400s() {
        // A field-name typo is still an unknown-field refusal.
        let err = parse_err(serde_json::json!({
            "goal": "g",
            "submission_id": "11111111-1111-4111-8111-111111111111",
            "constrained_by": ["same"],
        }));
        assert!(err.contains("unknown field"), "{err}");

        // More than MAX entries in any string list (constraints / invariants
        // / risks share one helper).
        let many: Vec<String> = (0..=faktor_session::MAX_TASK_CONTRACT_ENTRIES)
            .map(|i| format!("entry {i}"))
            .collect();
        for field in ["constraints", "invariants", "risks"] {
            let mut body = base_body();
            body[field] = serde_json::json!(many);
            let req = parse(body);
            let err = build_user_contract(
                req.constraints,
                req.invariants,
                req.risks,
                req.proof_obligations,
            )
            .expect_err("over-count must refuse");
            assert_eq!(err.http_status, 400, "{field}");
            assert_eq!(err.code, "oversized", "{field}");
        }
        // The obligation list carries the same count bound.
        let mut req = parse(serde_json::json!({
            "goal": "g",
            "submission_id": "11111111-1111-4111-8111-111111111111",
            "proof_obligations": (0..=faktor_session::MAX_TASK_CONTRACT_ENTRIES)
                .map(|i| serde_json::json!(format!("claim {i}")))
                .collect::<Vec<_>>(),
        }));
        let err = build_user_contract(
            req.constraints.take(),
            req.invariants.take(),
            req.risks.take(),
            req.proof_obligations.take(),
        )
        .expect_err("over-count obligations must refuse");
        assert_eq!(err.http_status, 400);
        assert_eq!(err.code, "oversized");

        // Empty and whitespace-only strings.
        for hostile in ["", "   "] {
            let err = build_user_contract(Some(vec![hostile.to_string()]), None, None, None)
                .expect_err("empty entry must refuse");
            assert_eq!(err.http_status, 400);
            assert_eq!(err.code, "malformed");
            let err = build_user_contract(
                None,
                None,
                None,
                Some(vec![ProofObligationInput::Claim(hostile.to_string())]),
            )
            .expect_err("empty claim must refuse");
            assert_eq!(err.http_status, 400);
        }
        // Oversized strings at the exact bound and one byte over.
        let at_bound = "x".repeat(faktor_session::MAX_TASK_CONTRACT_STRING_BYTES);
        assert!(build_user_contract(Some(vec![at_bound]), None, None, None).is_ok());
        let over = "x".repeat(faktor_session::MAX_TASK_CONTRACT_STRING_BYTES + 1);
        let err = build_user_contract(Some(vec![over]), None, None, None)
            .expect_err("oversized entry must refuse");
        assert_eq!(err.http_status, 400);
        assert_eq!(err.code, "oversized");

        // The DTO is still strict: wrong member types, a missing evidence
        // member in the object form, an unknown member and an unknown
        // evidence kind are all parse errors (plain 400s).
        for hostile in [
            serde_json::json!({"constraints": "not-a-list"}),
            serde_json::json!({"constraints": [1, 2]}),
            serde_json::json!({"risks": [{"description": "x"}]}),
            serde_json::json!({"proof_obligations": [{"claim": "c"}]}),
            serde_json::json!({"proof_obligations": [{"claim": "c", "evidence": "guess"}]}),
            serde_json::json!({"proof_obligations": [{"claim": "c", "evidence": "compile", "extra": true}]}),
            serde_json::json!({"proof_obligations": [{"evidence": "compile"}]}),
            serde_json::json!({"proof_obligations": [true]}),
            serde_json::json!({"proof_obligations": [null]}),
        ] {
            let mut body = base_body();
            let object = body.as_object_mut().unwrap();
            for (key, value) in hostile.as_object().unwrap() {
                object.insert(key.clone(), value.clone());
            }
            let err = parse_err(body);
            assert!(!err.is_empty(), "{hostile}");
        }
    }
}

/// The resolved B-09 turn bound: the per-session setting wins, else the
/// served workspace descriptor; an absent value leaves the daemon default.
pub(crate) fn session_turn_cap(
    state: &AppState,
    session: faktor_core::id::SessionId,
) -> Option<u32> {
    let foundation = state.deps.foundation.as_deref()?;
    if let Ok(Some(value)) = foundation.session_settings.get(
        session.raw(),
        faktor_foundation::session_settings::MAX_TURNS_KEY,
    ) {
        if let Some(turns) = value.as_u64() {
            return u32::try_from(turns).ok();
        }
    }
    foundation
        .config
        .u64_value("faktor.agent.maxTurns")
        .ok()
        .flatten()
        .and_then(|turns| u32::try_from(turns).ok())
}

#[cfg(test)]
mod preset_model_selector_tests {
    use super::*;

    /// Regression: a preset's model provider was joined into the model id
    /// (`ollama/qwen3.8:latest`) and sent verbatim to the adapter, which
    /// failed the first tool-less turn with an HTTP 404 "model not found".
    #[test]
    fn same_provider_preset_contributes_the_exact_model_id() {
        assert_eq!(
            preset_model_selector("ollama", "qwen3.8:latest", "ollama").unwrap(),
            Some("qwen3.8:latest".to_string())
        );
    }

    #[test]
    fn different_provider_preset_is_a_typed_refusal_not_a_joined_model() {
        let err = preset_model_selector("openai", "gpt-4o", "ollama").unwrap_err();
        assert_eq!(err.code, "preset_provider_unsupported");
        assert_eq!(err.http_status, 409);
        assert!(
            err.message.contains("cannot switch the session provider"),
            "{err:?}"
        );
    }

    #[test]
    fn empty_preset_model_is_malformed() {
        let err = preset_model_selector("ollama", "", "ollama").unwrap_err();
        assert_eq!(err.code, "malformed");
    }
}

/// Compute and durably record the EFFECTIVE tool enablement of this session
/// before a run starts (F165). Authority inputs:
///
/// - `faktor.permissions.allowedTools` (config, NextTask) — a non-empty list
///   is the baseline allow set;
/// - the bound preset's `tool_allow`/`tool_deny` — an allow list INTERSECTS
///   the config baseline, denies always win.
///
/// The runtime consumes the fact when it builds the turn's tool bundle, so a
/// disabled tool is absent from the offered schema AND refused by the
/// bundle-membership guard if a stale model call names it.
#[allow(clippy::result_large_err)]
pub(crate) fn write_tool_enablement(
    state: &AppState,
    handle: &faktor_session::SessionHandle,
    envelope: Option<&faktor_foundation::presets::PresetEnvelope>,
) -> Result<(), Response> {
    let mut config_allow: Vec<String> = state
        .deps
        .foundation
        .as_ref()
        .and_then(|foundation| {
            foundation
                .config
                .value("faktor.permissions.allowedTools")
                .ok()
                .flatten()
        })
        .and_then(|value| value.as_array().cloned())
        .map(|items| {
            items
                .into_iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    config_allow.sort();
    config_allow.dedup();
    let preset_allow: Vec<String> = envelope
        .map(|envelope| envelope.tools.allow.clone())
        .unwrap_or_default();
    let mut deny: Vec<String> = envelope
        .map(|envelope| envelope.tools.deny.clone())
        .unwrap_or_default();
    deny.sort();
    deny.dedup();
    let allow = match (config_allow.is_empty(), preset_allow.is_empty()) {
        // No restriction at either authority: the documented all-tools
        // default (denies still apply).
        (true, true) => Vec::new(),
        (false, true) => config_allow,
        (true, false) => preset_allow,
        (false, false) => {
            let preset: std::collections::BTreeSet<&str> =
                preset_allow.iter().map(String::as_str).collect();
            let mut narrowed: Vec<String> = config_allow
                .into_iter()
                .filter(|name| preset.contains(name.as_str()))
                .collect();
            narrowed.sort();
            narrowed.dedup();
            narrowed
        }
    };
    let value = serde_json::json!({ "allow": allow, "deny": deny }).to_string();
    handle
        .upsert_memory_fact(
            faktor_agent::TOOL_ENABLEMENT_FACT_KIND,
            faktor_agent::TOOL_ENABLEMENT_FACT_KEY,
            &value,
        )
        .map_err(|e| {
            wire_status(ApiError {
                code: "internal",
                message: format!("tool enablement authority write failed: {e}"),
                http_status: 500,
                retryable: false,
            })
        })?;
    Ok(())
}

const MAX_COMPARE_CANDIDATES: usize = 4;

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct NativeCompareBody {
    /// The session supplying the WORKSPACE context (the comparison never
    /// reads or mutates this session's run).
    pub session_id: String,
    pub prompt: String,
    #[serde(default)]
    pub submission_id: Option<String>,
    pub candidates: Vec<NativeCompareCandidate>,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub criteria: Vec<String>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub max_cost_micro: Option<u64>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct NativeCompareCandidate {
    pub provider: String,
    pub model: String,
}

/// `POST /native/models/compare` — bounded side-by-side comparison of exact
/// served models on the SAME captured prompt (F045). Each candidate runs in
/// its OWN session/task scope with its own durable route/budget; the calling
/// session and its run are never touched. The candidates share the requested
/// budget (split evenly) and are capped at [`MAX_COMPARE_CANDIDATES`].
pub(crate) async fn native_models_compare(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<NativeCompareBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid model comparison body")),
    };
    if body.prompt.trim().is_empty() {
        return wire_status(malformed_body("comparison prompt is empty"));
    }
    if body.candidates.len() < 2 || body.candidates.len() > MAX_COMPARE_CANDIDATES {
        return wire_status(ApiError {
            code: "malformed",
            message: format!("comparison needs 2..={MAX_COMPARE_CANDIDATES} candidates"),
            http_status: 400,
            retryable: false,
        });
    }
    let context = match state
        .deps
        .session
        .get_session(match body.session_id.parse::<u64>() {
            Ok(id) => faktor_core::id::SessionId::new(id),
            Err(_) => {
                return wire_status(malformed_body("sessionId must be a u64"));
            }
        }) {
        Ok(Some(handle)) => handle,
        Ok(None) => return wire_status(not_found("session")),
        Err(e) => return api_err(&e),
    };
    let workspace = match context.row() {
        Ok(row) => row.workspace_id,
        Err(e) => return api_err(&e),
    };
    for candidate in &body.candidates {
        if state
            .deps
            .agent
            .deps()
            .providers
            .capabilities(&candidate.provider, &candidate.model)
            .is_none()
        {
            return crate::native::foundation_ext::conflict(
                "unserved_comparison_model",
                format!(
                    "candidate {}/{} is not served by this daemon",
                    candidate.provider, candidate.model
                ),
            );
        }
    }
    let count = body.candidates.len() as u64;
    let per_candidate_cost = body.max_cost_micro.map(|cap| cap / count);
    let per_candidate_tokens = body.max_tokens.map(|cap| cap / count);
    let submission = body
        .submission_id
        .unwrap_or_else(|| format!("{:x}-{:x}", context.id().raw(), state.deps.started_ms));
    let prompts = PromptExecutionService::from_state(&state);
    let mut candidates = Vec::with_capacity(body.candidates.len());
    for (index, candidate) in body.candidates.iter().enumerate() {
        let title = format!("compare: {}/{}", candidate.provider, candidate.model);
        let fork = match state.deps.session.create_session(
            workspace,
            &title,
            &candidate.provider,
            &candidate.model,
        ) {
            Ok(handle) => handle,
            Err(e) => return api_err(&e),
        };
        let run = PromptExecutionService::comparison_candidate_run(
            body.prompt.clone(),
            format!("{submission}-{index}"),
            candidate.model.clone(),
            body.prompt.clone(),
            body.files.clone(),
            body.criteria.clone(),
            per_candidate_tokens,
            per_candidate_cost,
            native_run_parent_caps(),
        );
        let receipt = match prompts.start_comparison_candidate(
            fork.id(),
            run,
            session_turn_cap(&state, fork.id()),
        ) {
            Ok(receipt) => receipt,
            Err(e) => return exec_error_response(&e),
        };
        candidates.push(serde_json::json!({
            "provider": candidate.provider,
            "model": candidate.model,
            "sessionId": fork.id().to_string(),
            "runId": receipt.run_id,
            "opId": receipt.op_id.map(|op| op.raw().to_string()),
            "actualModel": format!("{}/{}", candidate.provider, candidate.model),
            "maxCostMicro": per_candidate_cost,
            "maxTokens": per_candidate_tokens,
        }));
    }
    Json(serde_json::json!({
        "comparisonId": submission,
        "workspaceId": workspace.raw().to_string(),
        "mainRunUnchanged": true,
        "budget": {
            "maxCostMicro": body.max_cost_micro,
            "maxTokens": body.max_tokens,
            "perCandidateCostMicro": per_candidate_cost,
            "perCandidateTokens": per_candidate_tokens,
        },
        "candidates": candidates,
    }))
    .into_response()
}
