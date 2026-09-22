//! Additive read-only proof surfacing for the task cockpit.
//!
//! `GET /native/tasks/{id}/proof` — the full VERIFIED story of ONE durable
//! task in ONE strict payload: criteria (with bindings/verdicts/evidence
//! refs), required-check totals, the review verdict, the run-base / verified
//! / landed tree digests, the published commit OID and the remote PR head
//! when present, the cost (spend fold), the durable completion-step
//! statuses and the integration transaction state.
//!
//! `GET /native/tasks/{id}/completion-steps` — the same durable
//! completion-contract read (per-step status + report) as its own route.
//!
//! Both routes are read-only and SESSION-SCOPED: `?session=<session_id>` is
//! required, the task must be that session's durable task identity or one of
//! its typed task rows, and a foreign/unknown task is a typed 404 — one
//! session's proof can never surface through another session's path. Store
//! failures and present-but-corrupt durable rows fail closed (500 with a
//! typed code naming the component); genuinely absent components are honest
//! nulls, and the verdict distinguishes `missing` from `unavailable` from
//! `corrupt` — a client never renders VERIFIED from a broken chain.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use faktor_core::completion::CompletionStep;
use faktor_core::id::{TaskId, TaskRevision};
use faktor_core::state::{CriterionBinding, TaskState, VerificationStatus};
use faktor_protocol::error::ApiError;
use faktor_session::ledger::{CompletionStepStatusRow, DurableRead, IntegrationTxnRow};
use faktor_session::task::{CompletionContractGate, TaskError, VerificationRecord};

use super::*;
use crate::api::AppState;

/// The frozen schema tag of the proof payload.
pub(crate) const TASK_PROOF_SCHEMA: &str = "faktor-task-proof/v1";
/// The frozen schema tag of the completion-steps payload.
pub(crate) const TASK_COMPLETION_STEPS_SCHEMA: &str = "faktor-task-completion-steps/v1";
/// Bound on the per-step history rows one read serves.
pub(crate) const MAX_PROOF_STEPS: usize = 64;

/// The query scope of both task reads. `session` is REQUIRED (see the module
/// docs); an unknown query member is ignored like every native query surface.
#[derive(serde::Deserialize)]
pub(crate) struct NativeTaskScopeQuery {
    session: Option<String>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskProofSummary {
    schema: &'static str,
    session_id: String,
    task_id: String,
    /// `verified` | `unverified` | `unavailable`.
    proof_state: &'static str,
    proof_state_reason: Option<String>,
    task: Option<TaskProofTask>,
    criteria: TaskProofCriteria,
    checks: TaskProofChecks,
    review: TaskProofReview,
    trees: TaskProofTrees,
    publication: TaskProofPublication,
    cost: TaskProofCost,
    completion: CompletionBlock,
    integration: TaskProofIntegration,
    /// Every component that could not be read as present-and-valid, with the
    /// explicit `missing`/`unavailable`/`corrupt`/`mismatch` distinction.
    unavailable: Vec<TaskProofUnavailable>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskProofTask {
    state: String,
    revision: Option<String>,
    goal: String,
    updated_ms: i64,
    acceptance_criteria: Vec<String>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskProofUnavailable {
    component: String,
    kind: &'static str,
    reason: String,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskProofCriteria {
    /// The record the block was projected from (`null` when no record exists).
    record_id: Option<String>,
    record_status: Option<String>,
    record_revision: Option<String>,
    /// True when a passing record certifies the task's completion binding
    /// (the current revision for an in-flight task, the pre-completion
    /// revision for a VerifiedComplete task); never inferred.
    certifies_completion: bool,
    total: u64,
    passed: u64,
    failed: u64,
    unavailable: u64,
    items: Vec<TaskProofCriterionRow>,
    /// Bounded note when more criteria existed than were served.
    truncated: bool,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskProofCriterionRow {
    criterion_key: String,
    origin: Option<String>,
    requirement: Option<String>,
    passed: bool,
    /// `pass` | `fail` | `unavailable` (unavailable never renders as pass).
    verdict: &'static str,
    binding_kind: String,
    binding: serde_json::Value,
    evidence: Option<String>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskProofChecks {
    total: u64,
    passed: u64,
    failed: u64,
    other: u64,
    required_total: u64,
    required_passed: u64,
    required_failed: u64,
    /// True when every REQUIRED check in the certifying record passed (vacuously
    /// true when the record carries no required check — stated, never implied).
    required_all_passed: bool,
    items: Vec<TaskProofCheckRow>,
    truncated: bool,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskProofCheckRow {
    check: String,
    program: String,
    required: bool,
    status: String,
    exit: Option<i32>,
    summary: Option<String>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskProofReview {
    record_id: Option<String>,
    /// The verification record's status (`passed` | `failed` | ... | null).
    status: Option<String>,
    /// The record's reviewer observation, verbatim (`null` = none ran).
    reviewer: Option<serde_json::Value>,
    /// The verdict of every `independent_review` criterion binding the record
    /// carries: `passed` | `failed` | `none` (no such binding — never a
    /// synthesized review).
    independent_verdict: &'static str,
    independent_criteria: Vec<String>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskProofTrees {
    /// The IMMUTABLE run base snapshot (`null` when no run base/integration
    /// record exists — an honest absence).
    run_base: Option<String>,
    /// The verified candidate snapshot the record proved.
    verified: Option<String>,
    /// The landed (final integration) snapshot.
    landed: Option<String>,
    /// `true`/`false` only when both snapshots exist; `null` otherwise.
    landed_equals_verified: Option<bool>,
    source_count: Option<u64>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskProofPublication {
    verification_record: Option<String>,
    git_tree_oid: Option<String>,
    commit_oid: Option<String>,
    local_ref: Option<String>,
    /// The encoded `<remote>:<ref>@<oid>` remote ref as recorded.
    remote_ref: Option<String>,
    /// The remote head OID parsed out of `remote_ref` (`null` when none).
    remote_head_oid: Option<String>,
    /// The newest completed pull-request external operation of the task's
    /// contract revision, when one exists.
    pull_request: Option<TaskProofPullRequest>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskProofPullRequest {
    provider: String,
    operation_key: String,
    /// The provider-side PR id.
    id: String,
    /// The recorded provider-side version (`<head_sha>@<updated_at>` for the
    /// GitHub adapter); the raw value is served verbatim.
    version: String,
    /// The head SHA parsed out of `version` (`null` when the version does not
    /// carry one).
    head_oid: Option<String>,
    state: String,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskProofCost {
    /// `known` | `unavailable`.
    status: &'static str,
    reason: Option<String>,
    /// Money fields are decimal strings on the wire (see `faktor_cloud::money`).
    #[serde(with = "faktor_cloud::money::option")]
    spent_cost_micro: Option<u64>,
    #[serde(with = "faktor_cloud::money::option")]
    max_cost_micro: Option<u64>,
    #[serde(with = "faktor_cloud::money::option")]
    open_reserved_micro: Option<u64>,
    open_reservations: Option<u64>,
    #[serde(with = "faktor_cloud::money::option")]
    uncertain_reserved_micro: Option<u64>,
    uncertain_reservations: Option<u64>,
    settled_count: Option<u64>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CompletionContractBlock {
    include_commit: bool,
    include_push: bool,
    include_pr: bool,
    /// The run revision the contract was recorded against.
    revision: Option<String>,
    requested_steps: Vec<String>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CompletionStepBlock {
    step: String,
    /// `missing` when no durable row exists for a requested step.
    status: String,
    present: bool,
    detail: Option<String>,
    snapshot: Option<String>,
    seq: Option<i64>,
    at_ms: Option<i64>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CompletionGateBlock {
    /// `satisfied` | `refused` | `no_contract`.
    status: &'static str,
    reason: Option<String>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CompletionBlock {
    contract: Option<CompletionContractBlock>,
    /// Every requested step with its durable status (`missing` when no row).
    steps: Vec<CompletionStepBlock>,
    /// The full durable row history (ascending), bounded.
    records: Vec<CompletionStepBlock>,
    gate: CompletionGateBlock,
    truncated: bool,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskProofIntegration {
    present: bool,
    in_flight: Option<bool>,
    run_id: Option<String>,
    final_root: Option<String>,
    final_snapshot_hash: Option<String>,
    run_base_snapshot: Option<String>,
    candidate_snapshot: Option<String>,
    landed_snapshot: Option<String>,
    integrated_file_count: Option<u64>,
    conflict_count: Option<u64>,
    source_count: Option<u64>,
    proof_basis_digest: Option<String>,
    at_ms: Option<i64>,
    txn: Option<TaskProofIntegrationTxn>,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskProofIntegrationTxn {
    txn_id: String,
    phase: String,
    owner_root: String,
    candidate_root: String,
    path_count: u64,
    applied_count: u64,
    conflicts: Vec<String>,
    at_ms: i64,
}

/// `GET /native/tasks/{id}/proof` — the strict full story (module docs).
pub(crate) async fn native_task_proof(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<NativeTaskScopeQuery>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let scoped = match resolve_scoped_task(&state, query.session.as_deref(), &id) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let handle = &scoped.handle;
    let requested = scoped.task_id;

    // Verification records (bounded pool read; strict v20 evidence decode).
    let mut records = match state
        .deps
        .session
        .verification_records(handle.id(), requested)
        .await
    {
        Ok(r) => r,
        Err(e) => return api_err(&e),
    };
    records.retain(|r| r.workspace_id == scoped.session_row.workspace_id);
    if records.len() > MAX_NATIVE_LIST {
        records.truncate(MAX_NATIVE_LIST);
    }

    // Integration record: classified (corrupt/store failures fail closed).
    let integration = match handle.ledger_integration_record_for_task_read(requested.raw()) {
        DurableRead::Missing => None,
        DurableRead::PresentValid(row) => Some(row),
        DurableRead::PresentMalformed(detail) => {
            return proof_corrupt("integration record", &detail)
        }
        DurableRead::StoreFailure(detail) => {
            return proof_store_failure("integration record", &detail)
        }
    };

    // Run base of the integration's run (classified).
    let run_base = match integration.as_ref().map(|i| i.run_id.as_str()) {
        Some(run_id) => match handle.ledger_run_base_read(run_id) {
            DurableRead::Missing => None,
            DurableRead::PresentValid(row) => Some(row),
            DurableRead::PresentMalformed(detail) => {
                return proof_corrupt("run base record", &detail)
            }
            DurableRead::StoreFailure(detail) => {
                return proof_store_failure("run base record", &detail)
            }
        },
        None => None,
    };

    // The CURRENT durable task revision (the row's optimistic-lock token;
    // the session Task view intentionally drops it). A task without a typed
    // row has no revision — an honest absence, never a synthesized one.
    let task_revision: Option<TaskRevision> = match scoped.task.as_ref() {
        Some(_) => match handle.task_revision(requested) {
            Ok(r) => Some(r),
            Err(TaskError::NotFound(_)) => None,
            Err(e) => {
                return proof_store_failure("task revision", &e.to_string());
            }
        },
        None => None,
    };

    // Completion contract + durable step rows + gate (all classified).
    let completion = match completion_block(handle, requested) {
        Ok(c) => c,
        Err(r) => return *r,
    };

    // Published git artifact of the CURRENT record revision (when one exists).
    let certifying = certifying_record(scoped.task.as_ref(), task_revision, &records);
    let publication_revision = certifying
        .map(|r| r.revision.raw())
        .or_else(|| task_revision.map(|r| r.raw()));
    let git_artifact = match publication_revision {
        Some(revision) => {
            match handle.ledger_verified_git_artifact_get(requested.raw(), revision) {
                Ok(a) => a,
                Err(e) => return api_err(&e),
            }
        }
        None => None,
    };

    // The task's pull-request external operations (completed rows only).
    let external = match handle.ledger_external_operations() {
        Ok(ops) => ops,
        Err(e) => return api_err(&e),
    };
    let pr_prefix = format!("task:{}:rev:", requested.raw());
    let mut pull_request: Option<&faktor_session::ledger::ExternalOperationRow> = None;
    for op in &external {
        if op.kind != "pull_request" || !op.operation_key.starts_with(&pr_prefix) {
            continue;
        }
        if let Some(revision) = publication_revision {
            let scoped_prefix = format!("{pr_prefix}{revision}:");
            if !op.operation_key.starts_with(&scoped_prefix) {
                continue;
            }
        }
        if op.state == faktor_session::ledger::ExternalOperationState::Completed {
            pull_request = Some(op);
        }
    }

    // Cost fold: the durable budget authority's own Known/Unavailable state.
    let budget = state
        .deps
        .session
        .budget_state(handle.id(), requested)
        .await;

    // The certifying/newest record drives criteria/checks/review/trees.
    let projection: Option<&VerificationRecord> = certifying.or(records.last());
    let task_criteria = scoped
        .task
        .as_ref()
        .map(|t| t.criteria())
        .unwrap_or_default();

    let criteria = criteria_block(projection, &task_criteria, certifying.is_some());
    let checks = checks_block(projection);
    let review = review_block(projection);
    let verified_snapshot = projection.and_then(verified_snapshot_of);
    let run_base_snapshot = integration
        .as_ref()
        .and_then(|i| i.run_base_snapshot.clone())
        .or_else(|| run_base.as_ref().map(|r| r.snapshot_hash.clone()))
        .or_else(|| {
            projection
                .and_then(|r| r.candidate_proof_ref.as_ref())
                .and_then(|p| p.run_base_snapshot.clone())
        });
    let landed_snapshot = integration
        .as_ref()
        .map(|i| i.final_snapshot_hash.clone())
        .filter(|h| !h.is_empty());
    let trees = TaskProofTrees {
        run_base: run_base_snapshot,
        verified: verified_snapshot.clone(),
        landed: landed_snapshot.clone(),
        landed_equals_verified: match (&verified_snapshot, &landed_snapshot) {
            (Some(v), Some(l)) => Some(v == l),
            _ => None,
        },
        source_count: integration.as_ref().map(|i| i.source_count),
    };

    let publication = publication_block(git_artifact.as_ref(), pull_request);
    let cost = cost_block(budget);
    let txn = match integration.as_ref().map(|i| i.run_id.as_str()) {
        Some(run_id) => match handle.ledger_integration_txn_read(run_id) {
            DurableRead::Missing => None,
            DurableRead::PresentValid(row) => Some(row),
            DurableRead::PresentMalformed(detail) => {
                return proof_corrupt("integration transaction", &detail)
            }
            DurableRead::StoreFailure(detail) => {
                return proof_store_failure("integration transaction", &detail)
            }
        },
        None => None,
    };
    let integration_block = integration_block(integration.as_ref(), txn.as_ref());

    // The verdict. VERIFIED requires the full chain: a VerifiedComplete task,
    // a passing record at the CURRENT revision, every criterion passing, every
    // REQUIRED check passing, and (when an integration record exists) the
    // landed snapshot equalling the verified one. Anything else is either
    // `unverified` (an honest not-yet/never) or `unavailable` (the task
    // claims VerifiedComplete but the chain does not back it — never a fake
    // VERIFIED).
    let mut unavailable: Vec<TaskProofUnavailable> = Vec::new();
    let task_verified_claim = scoped
        .task
        .as_ref()
        .is_some_and(|t| t.state == TaskState::VerifiedComplete);
    let criteria_all_pass = projection.is_some_and(|r| {
        r.criteria
            .iter()
            .all(|c| criterion_verdict_label(c.passed, c.binding.as_ref()) == "pass")
    });
    let required_all_pass = checks.required_all_passed;
    let landed_ok = match integration.as_ref() {
        Some(_) => matches!(
            (&trees.verified, &trees.landed),
            (Some(v), Some(l)) if v == l
        ),
        None => true,
    };
    let verified_snapshot_present = trees.verified.as_deref().is_some_and(|s| !s.is_empty());
    // A completion is MANIFEST-BOUND when it landed through the integration
    // pipeline (an integration record exists) or its record carries the
    // canonical tree hash. Only then is a missing verified snapshot a broken
    // chain; a legacy single-session record without a tree hash has no
    // snapshot BY CONTRACT (an honest absence the payload states).
    let manifest_bound = integration.is_some()
        || certifying.is_some_and(|r| r.tree_hash.as_deref().is_some_and(|h| !h.is_empty()));
    let proof_state = if !task_verified_claim {
        if records.is_empty() {
            unavailable.push(TaskProofUnavailable {
                component: "verification_record".into(),
                kind: "missing",
                reason: "no durable verification record covers this task".into(),
            });
        }
        "unverified"
    } else if certifying.is_none() {
        unavailable.push(TaskProofUnavailable {
            component: "verification_record".into(),
            kind: "missing",
            reason: "the task is VerifiedComplete but no passing record certifies its completion revision".into(),
        });
        "unavailable"
    } else if manifest_bound && !verified_snapshot_present {
        unavailable.push(TaskProofUnavailable {
            component: "verified_snapshot".into(),
            kind: "missing",
            reason: "the certifying record carries no verified tree snapshot".into(),
        });
        "unavailable"
    } else if !criteria_all_pass {
        unavailable.push(TaskProofUnavailable {
            component: "criteria".into(),
            kind: "mismatch",
            reason: "the certifying record is not an all-pass criterion set".into(),
        });
        "unavailable"
    } else if !required_all_pass {
        unavailable.push(TaskProofUnavailable {
            component: "required_checks".into(),
            kind: "mismatch",
            reason: "a REQUIRED check of the certifying record did not pass".into(),
        });
        "unavailable"
    } else if integration
        .as_ref()
        .is_some_and(|i| i.final_snapshot_hash.is_empty())
    {
        // The record-first in-flight landing marker: the chain is not
        // complete yet. Checked BEFORE the landed equality so the reason
        // names the real cause (an in-flight landing), not a mismatch
        // against a landing that has not happened.
        unavailable.push(TaskProofUnavailable {
            component: "integration_record".into(),
            kind: "unavailable",
            reason: "the integration record is still in flight (no landed snapshot yet)".into(),
        });
        "unavailable"
    } else if !landed_ok {
        unavailable.push(TaskProofUnavailable {
            component: "landed_snapshot".into(),
            kind: "mismatch",
            reason: "the landed integration snapshot does not equal the verified snapshot".into(),
        });
        "unavailable"
    } else if completion.gate.status == "refused" {
        unavailable.push(TaskProofUnavailable {
            component: "completion_gate".into(),
            kind: "unavailable",
            reason: completion
                .gate
                .reason
                .clone()
                .unwrap_or_else(|| "the durable completion gate is not satisfied".into()),
        });
        "unavailable"
    } else {
        "verified"
    };
    let proof_state_reason = unavailable
        .last()
        .map(|u| format!("{} {}: {}", u.component, u.kind, u.reason));

    let summary = TaskProofSummary {
        schema: TASK_PROOF_SCHEMA,
        session_id: handle.id().to_string(),
        task_id: requested.to_string(),
        proof_state,
        proof_state_reason,
        task: scoped.task.as_ref().map(|t| TaskProofTask {
            state: serde_json::to_string(&t.state)
                .unwrap_or_else(|_| "\"unknown\"".into())
                .trim_matches('"')
                .to_string(),
            revision: task_revision.map(|r| r.to_string()),
            goal: t.goal.clone(),
            updated_ms: t.updated_ms,
            acceptance_criteria: t.acceptance_criteria.clone(),
        }),
        criteria,
        checks,
        review,
        trees,
        publication,
        cost,
        completion,
        integration: integration_block,
        unavailable,
    };
    Json(summary).into_response()
}

/// `GET /native/tasks/{id}/completion-steps` — the durable per-step status +
/// report read of ONE task's accepted completion contract (module docs).
pub(crate) async fn native_task_completion_steps(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<NativeTaskScopeQuery>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let scoped = match resolve_scoped_task(&state, query.session.as_deref(), &id) {
        Ok(s) => s,
        Err(r) => return *r,
    };
    let completion = match completion_block(&scoped.handle, scoped.task_id) {
        Ok(c) => c,
        Err(r) => return *r,
    };
    Json(serde_json::json!({
        "schema": TASK_COMPLETION_STEPS_SCHEMA,
        "sessionId": scoped.handle.id().to_string(),
        "taskId": scoped.task_id.to_string(),
        "contract": completion.contract,
        "steps": completion.steps,
        "records": completion.records,
        "gate": completion.gate,
        "truncated": completion.truncated,
    }))
    .into_response()
}

// ------------------------------------------------------------------ shared

/// The session-scoped resolution of a `/native/tasks/{id}` read.
struct ScopedTask {
    handle: faktor_session::SessionHandle,
    task_id: TaskId,
    /// The typed durable task row, when one exists.
    task: Option<faktor_session::task::Task>,
    session_row: faktor_store::SessionRow,
}

/// Resolve one task inside its explicit session scope. Missing session,
/// non-numeric/zero ids and unknown sessions are typed 400/404s; a task that
/// is neither the session's durable identity nor one of its typed task rows
/// is a typed 404 (never a peek into another session's task space).
fn resolve_scoped_task(
    state: &AppState,
    session: Option<&str>,
    task_id: &str,
) -> Result<ScopedTask, Box<Response>> {
    let Some(session) = session.filter(|s| !s.trim().is_empty()) else {
        return Err(Box::new(wire_status(ApiError {
            code: "malformed",
            message: "task reads are session-scoped: pass ?session=<session_id>".into(),
            http_status: 400,
            retryable: false,
        })));
    };
    let handle = native_resolve_session(state, session)?;
    let raw: u64 = match task_id.parse() {
        Ok(v) if v > 0 => v,
        _ => {
            return Err(Box::new(wire_status(malformed_body(&format!(
                "invalid task id {task_id:?} for session {}",
                handle.id()
            )))))
        }
    };
    let requested = TaskId::new(raw);
    let session_row = match handle.row() {
        Ok(r) => r,
        Err(e) => return Err(Box::new(api_err(&e))),
    };
    let owned = match handle.list_tasks() {
        Ok(t) => t,
        Err(e) => return Err(Box::new(api_err(&e))),
    };
    let task = match handle.get_task(requested) {
        Ok(t) => t,
        Err(e) => return Err(Box::new(api_err(&e))),
    };
    let known = requested == session_row.task_id
        || task.is_some()
        || owned.iter().any(|t| t.task_id == requested);
    if !known {
        return Err(Box::new(wire_status(not_found(&format!(
            "task {requested} of session {} (unknown task)",
            handle.id()
        )))));
    }
    Ok(ScopedTask {
        handle,
        task_id: requested,
        task,
        session_row,
    })
}

/// The newest record that PASSES and certifies the task's completion
/// binding. The completion transaction validates `record.revision ==
/// expected_revision` and then bumps the row revision by exactly one, so a
/// `VerifiedComplete` task is certified by a passing record at
/// `current_revision - 1`; every non-completed state is certified (or not)
/// at its current revision. A `VerifiedComplete` task at revision 1 can
/// never have a valid proof.
fn certifying_record<'a>(
    task: Option<&faktor_session::task::Task>,
    task_revision: Option<TaskRevision>,
    records: &'a [VerificationRecord],
) -> Option<&'a VerificationRecord> {
    let revision = task_revision?;
    let certifying_revision = match task.map(|t| t.state) {
        Some(TaskState::VerifiedComplete) => match revision.raw().checked_sub(1) {
            Some(raw) if raw > 0 => TaskRevision::new(raw),
            _ => return None,
        },
        _ => revision,
    };
    records
        .iter()
        .rev()
        .find(|r| r.status == VerificationStatus::Passed && r.revision == certifying_revision)
}

fn verified_snapshot_of(record: &VerificationRecord) -> Option<String> {
    record
        .candidate_proof_ref
        .as_ref()
        .and_then(|c| c.candidate_snapshot.clone())
        .or_else(|| record.tree_hash.clone())
        .filter(|s| !s.is_empty())
}

/// The served three-way verdict of one criterion (unavailable never a pass).
fn criterion_verdict_label(passed: bool, binding: Option<&CriterionBinding>) -> &'static str {
    match binding {
        Some(CriterionBinding::Unavailable { .. }) | None => "unavailable",
        Some(_) if passed => "pass",
        Some(_) => "fail",
    }
}

fn criteria_block(
    record: Option<&VerificationRecord>,
    task_criteria: &[faktor_session::task::Criterion],
    certifies_completion: bool,
) -> TaskProofCriteria {
    let Some(record) = record else {
        return TaskProofCriteria {
            record_id: None,
            record_status: None,
            record_revision: None,
            certifies_completion: false,
            total: 0,
            passed: 0,
            failed: 0,
            unavailable: 0,
            items: Vec::new(),
            truncated: false,
        };
    };
    let mut items = Vec::new();
    let mut passed = 0u64;
    let mut failed = 0u64;
    let mut unavailable = 0u64;
    for criterion in record.criteria.iter().take(MAX_NATIVE_LIST) {
        // Durable criterion keys are the RAW acceptance-criteria entries: a
        // V2 task stores the encoded envelope, so the typed match decodes the
        // key first (and still accepts a plain legacy text key).
        let typed = task_criteria.iter().find(|t| {
            t.encoded_entry() == criterion.criterion_key
                || t.text == criterion.criterion_key
                || faktor_session::task::Criterion::decode(&criterion.criterion_key)
                    .is_some_and(|decoded| decoded.text == t.text)
        });
        let verdict = criterion_verdict_label(criterion.passed, criterion.binding.as_ref());
        match verdict {
            "pass" => passed += 1,
            "fail" => failed += 1,
            _ => unavailable += 1,
        }
        items.push(TaskProofCriterionRow {
            criterion_key: criterion.criterion_key.clone(),
            origin: typed.map(|t| t.origin.label().to_string()),
            requirement: typed.map(|t| t.requirement.label().to_string()),
            passed: criterion.passed,
            verdict,
            binding_kind: criterion
                .binding
                .as_ref()
                .map(|b| b.kind_label().to_string())
                .unwrap_or_else(|| "unavailable".into()),
            binding: serde_json::to_value(criterion.binding.as_ref())
                .unwrap_or(serde_json::Value::Null),
            evidence: criterion.evidence.clone(),
        });
    }
    TaskProofCriteria {
        record_id: Some(record.record_id.to_string()),
        record_status: Some(status_tag(record.status)),
        record_revision: Some(record.revision.to_string()),
        certifies_completion,
        total: items.len() as u64,
        passed,
        failed,
        unavailable,
        truncated: record.criteria.len() > items.len(),
        items,
    }
}

fn checks_block(record: Option<&VerificationRecord>) -> TaskProofChecks {
    let Some(record) = record else {
        return TaskProofChecks {
            total: 0,
            passed: 0,
            failed: 0,
            other: 0,
            required_total: 0,
            required_passed: 0,
            required_failed: 0,
            required_all_passed: true,
            items: Vec::new(),
            truncated: false,
        };
    };
    let mut passed = 0u64;
    let mut failed = 0u64;
    let mut other = 0u64;
    let mut required_total = 0u64;
    let mut required_passed = 0u64;
    let mut required_failed = 0u64;
    let mut items = Vec::new();
    for check in record.checks.iter().take(MAX_NATIVE_LIST) {
        match check.status {
            VerificationStatus::Passed => passed += 1,
            VerificationStatus::Failed => failed += 1,
            _ => other += 1,
        }
        if check.required {
            required_total += 1;
            match check.status {
                VerificationStatus::Passed => required_passed += 1,
                VerificationStatus::Failed => required_failed += 1,
                _ => {}
            }
        }
        items.push(TaskProofCheckRow {
            check: check.check.clone(),
            program: check.program.clone(),
            required: check.required,
            status: status_tag(check.status),
            exit: check.exit,
            summary: check.summary.clone(),
        });
    }
    TaskProofChecks {
        total: items.len() as u64,
        passed,
        failed,
        other,
        required_total,
        required_passed,
        required_failed,
        required_all_passed: required_failed == 0,
        truncated: record.checks.len() > items.len(),
        items,
    }
}

fn review_block(record: Option<&VerificationRecord>) -> TaskProofReview {
    let Some(record) = record else {
        return TaskProofReview {
            record_id: None,
            status: None,
            reviewer: None,
            independent_verdict: "none",
            independent_criteria: Vec::new(),
        };
    };
    let mut independent_criteria = Vec::new();
    let mut independent_failed = false;
    for criterion in &record.criteria {
        if matches!(
            criterion.binding,
            Some(CriterionBinding::IndependentReview { .. })
        ) {
            independent_criteria.push(criterion.criterion_key.clone());
            if !criterion.passed {
                independent_failed = true;
            }
        }
    }
    let independent_verdict = if independent_criteria.is_empty() {
        "none"
    } else if independent_failed {
        "failed"
    } else {
        "passed"
    };
    TaskProofReview {
        record_id: Some(record.record_id.to_string()),
        status: Some(status_tag(record.status)),
        reviewer: record.reviewer.clone(),
        independent_verdict,
        independent_criteria,
    }
}

fn publication_block(
    artifact: Option<&faktor_session::ledger::VerifiedGitArtifact>,
    pull_request: Option<&faktor_session::ledger::ExternalOperationRow>,
) -> TaskProofPublication {
    let remote_ref = artifact.and_then(|a| a.remote_ref.clone());
    let remote_head_oid = remote_ref
        .as_deref()
        .and_then(|r| r.rsplit_once('@'))
        .map(|(_, oid)| oid.to_string())
        .filter(|oid| !oid.is_empty());
    TaskProofPublication {
        verification_record: artifact.map(|a| a.verification_record.to_string()),
        git_tree_oid: artifact.and_then(|a| a.git_tree_oid.clone()),
        commit_oid: artifact.and_then(|a| a.commit_oid.clone()),
        local_ref: artifact.and_then(|a| a.local_ref.clone()),
        remote_ref,
        remote_head_oid,
        pull_request: pull_request.map(|op| {
            let version = op.remote_object_version.clone().unwrap_or_default();
            TaskProofPullRequest {
                provider: op.provider.clone(),
                operation_key: op.operation_key.clone(),
                id: op.remote_object_id.clone().unwrap_or_default(),
                head_oid: version
                    .split_once('@')
                    .map(|(head, _)| head.to_string())
                    .filter(|head| !head.is_empty()),
                version,
                state: op.state.as_tag().to_string(),
            }
        }),
    }
}

fn cost_block(state: faktor_session::budget::BudgetState) -> TaskProofCost {
    match state {
        faktor_session::budget::BudgetState::Known(view) => TaskProofCost {
            status: "known",
            reason: None,
            spent_cost_micro: Some(view.spent_cost_micro),
            max_cost_micro: view.max_cost_micro,
            open_reserved_micro: Some(view.open_reserved_micro),
            open_reservations: Some(view.open_reservations as u64),
            uncertain_reserved_micro: Some(view.uncertain_reserved_micro),
            uncertain_reservations: Some(view.uncertain_reservations as u64),
            settled_count: Some(view.settled_count as u64),
        },
        faktor_session::budget::BudgetState::Unavailable { reason } => TaskProofCost {
            status: "unavailable",
            reason: Some(reason),
            spent_cost_micro: None,
            max_cost_micro: None,
            open_reserved_micro: None,
            open_reservations: None,
            uncertain_reserved_micro: None,
            uncertain_reservations: None,
            settled_count: None,
        },
    }
}

/// The shared durable completion read: contract + requested-step rows +
/// full history + the gate verdict. Corrupt rows and store failures fail
/// closed (500); the gate's own refusals are semantic (200).
fn completion_block(
    handle: &faktor_session::SessionHandle,
    task_id: TaskId,
) -> Result<CompletionBlock, Box<Response>> {
    let contract = match handle.completion_contract(task_id) {
        Ok(c) => c,
        Err(TaskError::CorruptDurableState { detail, .. }) => {
            return Err(Box::new(proof_corrupt("completion contract", &detail)))
        }
        Err(e @ TaskError::Store(_)) => {
            return Err(Box::new(proof_store_failure(
                "completion contract",
                &e.to_string(),
            )))
        }
        Err(_) => None,
    };
    let mut records: Vec<CompletionStepStatusRow> = Vec::new();
    if let Some((rev, _)) = contract.as_ref() {
        match handle.ledger_completion_step_statuses(task_id.raw(), rev.raw()) {
            Ok(rows) => records = rows,
            Err(e) => {
                return Err(Box::new(proof_store_failure(
                    "completion step statuses",
                    &e.to_string(),
                )))
            }
        }
    }
    let contract_block = contract.as_ref().map(|(rev, c)| CompletionContractBlock {
        include_commit: c.include_commit,
        include_push: c.include_push,
        include_pr: c.include_pr,
        revision: Some(rev.to_string()),
        requested_steps: c
            .requested_steps()
            .into_iter()
            .map(|s| s.tag().to_string())
            .collect(),
    });
    let requested: Vec<CompletionStep> = contract
        .as_ref()
        .map(|(_, c)| c.requested_steps())
        .unwrap_or_default();
    let mut steps = Vec::new();
    for step in &requested {
        let latest = records.iter().rev().find(|r| r.step == *step);
        steps.push(step_row(*step, latest));
    }
    let gate = match handle.completion_contract_gate(task_id) {
        Ok(CompletionContractGate::Satisfied) => CompletionGateBlock {
            status: if contract.is_some() {
                "satisfied"
            } else {
                "no_contract"
            },
            reason: None,
        },
        Ok(CompletionContractGate::Refused(e)) => CompletionGateBlock {
            status: "refused",
            reason: Some(e.to_string()),
        },
        Err(TaskError::CorruptDurableState { detail, .. }) => {
            return Err(Box::new(proof_corrupt("completion gate", &detail)))
        }
        Err(e) => {
            return Err(Box::new(proof_store_failure(
                "completion gate",
                &e.to_string(),
            )))
        }
    };
    let truncated = records.len() > MAX_PROOF_STEPS;
    let all_rows: Vec<CompletionStepBlock> = records
        .iter()
        .take(MAX_PROOF_STEPS)
        .map(row_block)
        .collect();
    Ok(CompletionBlock {
        contract: contract_block,
        steps,
        records: all_rows,
        gate,
        truncated,
    })
}

fn step_row(step: CompletionStep, latest: Option<&CompletionStepStatusRow>) -> CompletionStepBlock {
    match latest {
        Some(row) => row_block(row),
        None => CompletionStepBlock {
            step: step.tag().to_string(),
            status: "missing".into(),
            present: false,
            detail: None,
            snapshot: None,
            seq: None,
            at_ms: None,
        },
    }
}

fn row_block(row: &CompletionStepStatusRow) -> CompletionStepBlock {
    CompletionStepBlock {
        step: row.step.tag().to_string(),
        status: row.status.tag().to_string(),
        present: true,
        detail: Some(row.detail.clone()),
        snapshot: row.snapshot.clone(),
        seq: Some(row.seq),
        at_ms: Some(row.at_ms),
    }
}

fn integration_block(
    record: Option<&faktor_session::ledger::IntegrationRecordRow>,
    txn: Option<&IntegrationTxnRow>,
) -> TaskProofIntegration {
    let Some(record) = record else {
        return TaskProofIntegration {
            present: false,
            in_flight: None,
            run_id: None,
            final_root: None,
            final_snapshot_hash: None,
            run_base_snapshot: None,
            candidate_snapshot: None,
            landed_snapshot: None,
            integrated_file_count: None,
            conflict_count: None,
            source_count: None,
            proof_basis_digest: None,
            at_ms: None,
            txn: None,
        };
    };
    TaskProofIntegration {
        present: true,
        in_flight: Some(record.final_snapshot_hash.is_empty()),
        run_id: Some(record.run_id.clone()),
        final_root: Some(record.final_root.clone()),
        final_snapshot_hash: Some(record.final_snapshot_hash.clone()),
        run_base_snapshot: record.run_base_snapshot.clone(),
        candidate_snapshot: record.candidate_snapshot.clone(),
        landed_snapshot: record.landed_snapshot.clone(),
        integrated_file_count: Some(record.integrated_file_count),
        conflict_count: Some(record.conflict_count),
        source_count: Some(record.source_count),
        proof_basis_digest: record.proof_basis_digest.clone(),
        at_ms: Some(record.at_ms),
        txn: txn.map(|t| TaskProofIntegrationTxn {
            txn_id: t.txn_id(),
            phase: t.phase.as_tag().to_string(),
            owner_root: t.owner_root.clone(),
            candidate_root: t.candidate_root.clone(),
            path_count: t.path_count,
            applied_count: t.applied_count,
            conflicts: t.conflicts.clone(),
            at_ms: t.at_ms,
        }),
    }
}

/// The stable snake_case tag of a verification status.
fn status_tag(status: VerificationStatus) -> String {
    serde_json::to_string(&status)
        .unwrap_or_else(|_| "\"unavailable\"".into())
        .trim_matches('"')
        .to_string()
}

pub(crate) fn proof_corrupt(component: &str, detail: &str) -> Response {
    wire_status(ApiError {
        code: "corrupt_durable_state",
        message: format!("{component} is present but corrupt: {detail}"),
        http_status: 500,
        retryable: false,
    })
}

pub(crate) fn proof_store_failure(component: &str, detail: &str) -> Response {
    wire_status(ApiError {
        code: "internal",
        message: format!("{component} could not be read: {detail}"),
        http_status: 500,
        retryable: false,
    })
}

#[cfg(test)]
#[path = "proof_tests.rs"]
mod proof_tests;
