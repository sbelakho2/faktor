//! Rev8 long-task native surfaces (S08/S10/S12/S62/S66/K-03/K-04/K-10/K-11).
//!
//! Two authorities live here:
//!
//! - **compaction operations**: an explicit, durable, cancellable state
//!   machine (`/native/session/{id}/context/compaction...`) whose accepted
//!   commit atomically writes the replacement checkpoint, the compaction row
//!   and the journal event.
//! - **memory records**: V2 memory CRUD with provenance, optimistic versions,
//!   tombstones/restore, dedupe and a bounded retention pass.
//!
//! Every DTO is strict (`deny_unknown_fields`), every read is paged and every
//! failure is typed. Nothing here fabricates progress, a measured reduction
//! or a completed deletion.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use faktor_context::compactor::{CompactionRequest, Compactor};
use faktor_context::RecentTurn;
use faktor_core::cancellation::CancellationToken;
use faktor_memory::{
    MemoryAdminError, MemoryFactV2, MemoryPolicy, MemoryRecordStore, MemoryScope, NewMemoryRecord,
    Provenance, ProvenanceOrigin, TypedMemoryValue,
};
use faktor_protocol::error::ApiError;
use faktor_session::compaction::{CompactionOperation, CompactionOperationState};

use super::*;
use crate::api::AppState;

const MAX_TURNS_PER_COMPACTION: u64 = 10_000;
const MAX_TURN_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TURN_TEXT_BYTES: usize = 256 * 1024;
const MAX_OPERATION_ID_BYTES: usize = 128;

fn session_u64(id: SessionId) -> u64 {
    id.raw()
}

fn conflict(code: &'static str, message: impl Into<String>) -> Response {
    wire_status(ApiError {
        code,
        message: message.into(),
        http_status: 409,
        retryable: false,
    })
}

fn malformed(message: impl Into<String>) -> Response {
    wire_status(ApiError {
        code: "malformed",
        message: message.into(),
        http_status: 400,
        retryable: false,
    })
}

fn unavailable(message: impl Into<String>) -> Response {
    wire_status(ApiError {
        code: "unavailable",
        message: message.into(),
        http_status: 503,
        retryable: false,
    })
}

fn memory_error(error: MemoryAdminError) -> Response {
    let status = match &error {
        MemoryAdminError::NotFound => StatusCode::NOT_FOUND,
        MemoryAdminError::Conflict { .. }
        | MemoryAdminError::VersionMismatch { .. }
        | MemoryAdminError::Deleted { .. }
        | MemoryAdminError::NotDeleted { .. }
        | MemoryAdminError::PolicyLimit { .. } => StatusCode::CONFLICT,
        MemoryAdminError::InvalidScope(_) | MemoryAdminError::Malformed(_) => {
            StatusCode::BAD_REQUEST
        }
        MemoryAdminError::Oversized(_) => StatusCode::PAYLOAD_TOO_LARGE,
        MemoryAdminError::Store(_) => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let mut body = serde_json::json!({
        "error": {
            "code": error.code(),
            "message": error.to_string(),
            "retryable": false,
        }
    });
    match &error {
        MemoryAdminError::Conflict {
            current_version,
            current_value_hash,
        } => {
            body["error"]["currentVersion"] = serde_json::json!(current_version);
            body["error"]["currentValueHash"] = serde_json::json!(current_value_hash);
        }
        MemoryAdminError::VersionMismatch { expected, actual } => {
            body["error"]["expectedVersion"] = serde_json::json!(expected);
            body["error"]["currentVersion"] = serde_json::json!(actual);
        }
        MemoryAdminError::Deleted { version } | MemoryAdminError::NotDeleted { version } => {
            body["error"]["currentVersion"] = serde_json::json!(version);
        }
        MemoryAdminError::PolicyLimit { scope, max_records } => {
            body["error"]["scope"] = serde_json::json!(scope);
            body["error"]["maxRecords"] = serde_json::json!(max_records);
        }
        _ => {}
    }
    (status, Json(body)).into_response()
}

// ------------------------------------------------------- compaction operations

type ActiveKey = (u64, String);

fn active_compactions() -> &'static Mutex<HashMap<ActiveKey, CancellationToken>> {
    static ACTIVE: OnceLock<Mutex<HashMap<ActiveKey, CancellationToken>>> = OnceLock::new();
    ACTIVE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register the executor token ONLY when this operation is not already
/// running in this process. Two concurrent `POST .../compaction` calls with
/// the same operation id must not run two executors against one durable row.
fn register_active_if_absent(
    session: SessionId,
    operation_id: &str,
    token: CancellationToken,
) -> bool {
    let mut active = active_compactions()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let key = (session.raw(), operation_id.to_string());
    if active.contains_key(&key) {
        return false;
    }
    active.insert(key, token);
    true
}

fn operation_is_active(session: SessionId, operation_id: &str) -> bool {
    active_compactions()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .contains_key(&(session.raw(), operation_id.to_string()))
}

fn process_start_ms() -> i64 {
    static START: OnceLock<i64> = OnceLock::new();
    *START.get_or_init(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
            .unwrap_or(0)
    })
}

/// A non-terminal operation whose executor vanished with a previous daemon
/// can never advance again. Reading it must not paint a live "compacting"
/// spinner forever (Rev8 S10: truthful states, no fabricated progress):
///
/// - `committing` is the ONE phase where the atomic commit may have landed
///   before the process died, so the outcome is named `effect_unknown`;
/// - every earlier phase has not touched the checkpoint/event, so the
///   operation is `cancelled` and the prior basis remains authoritative.
///
/// Runs only for rows last updated BEFORE this process started and with no
/// registered executor, so a just-admitted operation is never rewritten.
fn reconcile_stale_operation(
    handle: &faktor_session::SessionHandle,
    op: CompactionOperation,
) -> CompactionOperation {
    if op.is_terminal()
        || operation_is_active(handle.id(), &op.operation_id)
        || op.updated_ms >= process_start_ms()
    {
        return op;
    }
    let (state, error_code, message, result) = match op.state {
        CompactionOperationState::Committing => (
            CompactionOperationState::EffectUnknown,
            Some("effect_unknown"),
            "the daemon stopped during the commit phase; inspect the latest compaction row and checkpoint before retrying",
            serde_json::json!({
                "accepted": serde_json::Value::Null,
                "effectStatus": "unknown",
                "reason": "executor vanished during commit; the atomic checkpoint/event commit may or may not have landed",
                "basis": {"kind": "estimated", "method": "history-estimator", "version": "1"},
            }),
        ),
        _ => (
            CompactionOperationState::Cancelled,
            None,
            "the daemon restarted before the commit phase; the prior basis remains",
            serde_json::json!({
                "accepted": false,
                "cancelled": true,
                "reason": "executor vanished before commit; the prior basis remains",
                "basis": {"kind": "estimated", "method": "history-estimator", "version": "1"},
            }),
        ),
    };
    handle
        .advance_compaction_operation(
            &op.operation_id,
            state,
            state.as_str(),
            1_000_000,
            None,
            None,
            None,
            None,
            Some(&result),
            error_code,
            Some(message),
        )
        .ok()
        .flatten()
        .unwrap_or(op)
}

fn cancel_active(session: SessionId, operation_id: &str) -> bool {
    let token = {
        let active = active_compactions()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        active
            .get(&(session.raw(), operation_id.to_string()))
            .cloned()
    };
    match token {
        Some(token) => {
            token.cancel();
            true
        }
        None => false,
    }
}

fn unregister_active(session: SessionId, operation_id: &str) {
    let mut active = active_compactions()
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    active.remove(&(session.raw(), operation_id.to_string()));
}

fn operation_json(op: &faktor_session::compaction::CompactionOperation) -> serde_json::Value {
    let terminal = op.is_terminal();
    let error = match (&op.error_code, &op.error_message) {
        (Some(code), message) => serde_json::json!({
            "code": code,
            "message": message.clone().unwrap_or_default(),
        }),
        (None, _) => serde_json::Value::Null,
    };
    serde_json::json!({
        "operationId": op.operation_id,
        "state": op.state.as_str(),
        "phase": op.phase,
        "terminal": terminal,
        "progress": (op.progress_ppm as f64 / 1_000_000.0).clamp(0.0, 1.0),
        "expectedRevision": op.expected_revision,
        "manifestRevision": op.manifest_revision,
        "before": op.before_tokens,
        "after": op.after_tokens,
        "target": op.target_tokens,
        "minimumReduction": op.min_reduction_ppm as f64 / 1_000_000.0,
        "strategy": op.strategy,
        "result": op.result,
        "error": error,
        "createdMs": op.created_ms,
        "updatedMs": op.updated_ms,
        "basis": {
            "kind": "estimated",
            "method": "history-estimator",
            "version": "1",
        },
    })
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeCompactionStartBody {
    #[serde(default, rename = "operationId")]
    operation_id: Option<String>,
    #[serde(default, rename = "targetTokens")]
    target_tokens: Option<u64>,
    #[serde(default, rename = "minReductionRatio")]
    min_reduction_ratio: Option<f64>,
    #[serde(default, rename = "expectedRevision")]
    expected_revision: Option<u64>,
    #[serde(default, rename = "maxHistoryMessages")]
    max_history_messages: Option<u64>,
    /// K-02 explicit compaction model (`provider/model`). Absent/`inherit`
    /// leaves the deterministic pruner in charge.
    #[serde(default)]
    model: Option<String>,
}

/// `POST /native/session/{id}/context/compaction` — start and run one
/// durable compaction operation. The response is the exact operation state
/// after completion; a client can also poll the status route.
pub(crate) async fn native_compaction_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<NativeCompactionStartBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return malformed("invalid compaction body"),
    };
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    let operation_id = match req.operation_id {
        Some(id) => {
            if id.is_empty() || id.len() > MAX_OPERATION_ID_BYTES {
                return malformed("operationId must be 1..=128 bytes");
            }
            id
        }
        None => format!(
            "compact-{}-{:016x}",
            state.deps.agent.deps().clock.now_ms(),
            rand::random::<u64>()
        ),
    };
    let compaction_model = match req.model.as_deref().map(str::trim) {
        None | Some("") | Some("inherit") => None,
        Some(raw) => {
            let Some((provider, model_id)) = raw.split_once('/') else {
                return malformed("model must be `inherit` or `provider/model`");
            };
            if state
                .deps
                .agent
                .deps()
                .providers
                .capabilities(provider, model_id)
                .is_none()
            {
                return conflict(
                    "unserved_compaction_model",
                    format!("compaction model {raw:?} is not served by this daemon"),
                );
            }
            Some((raw.to_string(), provider.to_string(), model_id.to_string()))
        }
    };
    let min_reduction = req.min_reduction_ratio.unwrap_or(0.25);
    if !(0.0..1.0).contains(&min_reduction) {
        return malformed("minReductionRatio must be in [0, 1)");
    }
    if let Some(target) = req.target_tokens {
        if target == 0 {
            return malformed("targetTokens must be > 0");
        }
    }
    // Expected context revision is a compare-and-swap on the durable
    // manifest; a missing foundation cannot prove the revision, so an
    // explicit expectation fails closed.
    let manifest_revision = match (&state.deps.foundation, req.expected_revision) {
        (Some(foundation), Some(expected)) => {
            let latest = match foundation.context.latest(session_u64(handle.id())) {
                Ok(latest) => latest.map(|m| m.revision).unwrap_or(0),
                Err(e) => return super::foundation::foundation_error(e),
            };
            if latest != expected {
                return conflict(
                    "context_revision_conflict",
                    format!("expected context revision {expected}, current {latest}"),
                );
            }
            Some(expected)
        }
        (Some(_), None) => state
            .deps
            .foundation
            .as_ref()
            .and_then(|f| f.context.latest(session_u64(handle.id())).ok().flatten())
            .map(|m| m.revision),
        (None, Some(_)) => {
            return unavailable(
                "context manifests are not wired; expectedRevision cannot be checked",
            )
        }
        (None, None) => None,
    };
    // Register the executor BEFORE the durable row exists and before any
    // cancel can observe it: a cancel either finds the token (the executor
    // will observe `cancelling`) or the operation does not exist yet
    // (typed 404). That closes the "prepared row with no owner" gap.
    let token = CancellationToken::new();
    if !register_active_if_absent(handle.id(), &operation_id, token.clone()) {
        // Same operation id is already running here: never run a second
        // executor. The current durable state is the truthful answer.
        return match handle.compaction_operation(&operation_id) {
            Ok(Some(op)) => Json(operation_json(&op)).into_response(),
            Ok(None) => wire_status(not_found("compaction operation")),
            Err(e) => api_err(&e),
        };
    }
    let operation = match handle.begin_compaction_operation(
        &operation_id,
        req.target_tokens.unwrap_or(0) as i64,
        min_reduction,
        req.expected_revision,
        manifest_revision,
    ) {
        Ok(op) => op,
        Err(e) => {
            unregister_active(handle.id(), &operation_id);
            return api_err(&e);
        }
    };
    if operation.is_terminal() {
        unregister_active(handle.id(), &operation_id);
        return Json(operation_json(&operation)).into_response();
    }
    let outcome = execute_compaction(
        &state,
        &handle,
        &operation_id,
        req.target_tokens.map(|v| v as i64),
        min_reduction,
        req.max_history_messages
            .unwrap_or(10_000)
            .clamp(1, MAX_TURNS_PER_COMPACTION),
        compaction_model.as_ref(),
        &token,
    )
    .await;
    unregister_active(handle.id(), &operation_id);
    let _ = handle.prune_compaction_operations_keep_newest(64);
    match outcome {
        Ok(op) => Json(operation_json(&op)).into_response(),
        Err(response) => response,
    }
}

/// `GET /native/session/{id}/context/compaction/{operation}` — exact durable
/// operation state (progress, retained facts, measured before/after basis).
pub(crate) async fn native_compaction_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, operation_id)): Path<(String, String)>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    match handle.compaction_operation(&operation_id) {
        Ok(Some(op)) => {
            Json(operation_json(&reconcile_stale_operation(&handle, op))).into_response()
        }
        Ok(None) => wire_status(not_found("compaction operation")),
        Err(e) => api_err(&e),
    }
}

/// `GET /native/session/{id}/context/compaction` — newest durable operation
/// of the session (K-04 post-compaction inspection after a restart).
pub(crate) async fn native_compaction_latest(
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
    match handle.latest_compaction_operation() {
        Ok(Some(op)) => {
            Json(operation_json(&reconcile_stale_operation(&handle, op))).into_response()
        }
        Ok(None) => wire_status(not_found("compaction operation")),
        Err(e) => api_err(&e),
    }
}

/// `POST /native/session/{id}/context/compaction/{operation}/cancel` — stop
/// BEFORE commit. The prior basis remains (no partial checkpoint/event).
pub(crate) async fn native_compaction_cancel(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, operation_id)): Path<(String, String)>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    let signaled = cancel_active(handle.id(), &operation_id);
    match handle.request_compaction_cancel(&operation_id) {
        Ok(Some(op)) if op.is_terminal() => {
            // The durable state is the authority; a late cancel never
            // rewrites an applied/refused/cancelled/failed/effect-unknown
            // outcome.
            Json(serde_json::json!({
                "cancelRequested": false,
                "signaled": signaled,
                "operation": operation_json(&op),
            }))
            .into_response()
        }
        Ok(Some(op)) => {
            // With no registered executor nobody will observe `cancelling`.
            // Finalize the durable state now instead of leaving a permanent
            // fake "compacting" row (truthful UI, no zombie progress).
            if !signaled && !operation_is_active(handle.id(), &operation_id) {
                if let Ok(finalized) = finish_cancelled(&handle, &operation_id, None) {
                    return Json(serde_json::json!({
                        "cancelRequested": true,
                        "signaled": false,
                        "operation": operation_json(&finalized),
                    }))
                    .into_response();
                }
                // The executor won a late terminal race; re-read the exact
                // outcome rather than guessing.
                if let Ok(Some(current)) = handle.compaction_operation(&operation_id) {
                    return Json(serde_json::json!({
                        "cancelRequested": false,
                        "signaled": signaled,
                        "operation": operation_json(&current),
                    }))
                    .into_response();
                }
            }
            Json(serde_json::json!({
                "cancelRequested": true,
                "signaled": signaled,
                "operation": operation_json(&op),
            }))
            .into_response()
        }
        Ok(None) => wire_status(not_found("compaction operation")),
        Err(e) => api_err(&e),
    }
}

fn decode_checkpoint_turns(handle: &faktor_session::SessionHandle) -> Vec<RecentTurn> {
    match handle.context_checkpoint() {
        Ok(Some(checkpoint)) => serde_json::from_slice(&checkpoint.payload).unwrap_or_else(|e| {
            tracing::error!(
                session = %handle.id(),
                error = %e,
                "context checkpoint turns are malformed; manual compaction falls back to the durable transcript"
            );
            Vec::new()
        }),
        Ok(None) => Vec::new(),
        Err(e) => {
            tracing::error!(
                session = %handle.id(),
                error = %e,
                "context checkpoint is unreadable; manual compaction falls back to the durable transcript"
            );
            Vec::new()
        }
    }
}

fn checkpoint_through_seq(handle: &faktor_session::SessionHandle) -> Option<u64> {
    handle
        .context_checkpoint()
        .ok()
        .flatten()
        .map(|c| c.through_seq)
}

/// Load the LOGICAL history (checkpoint replacement + strictly newer rows)
/// bounded by messages and bytes. Parts are loaded in one batch, never N+1.
fn load_turns(
    state: &AppState,
    handle: &faktor_session::SessionHandle,
    max_messages: u64,
) -> faktor_core::Result<(Vec<RecentTurn>, u64)> {
    let base_seq = checkpoint_through_seq(handle);
    let mut turns = decode_checkpoint_turns(handle);
    // The window is read from the STORE ordered newest-first and bounded by
    // the checkpoint high-water mark, so a compacted session never replays
    // rows the checkpoint already replaced. Reverse it to the wire's
    // oldest-first order before appending to the checkpoint turns: keeping
    // DESC order here would put older rows after newer ones and make the
    // compactor's newest-suffix rule keep the WRONG end of the history.
    let rows = state
        .deps
        .session
        .store()
        .messages_window_with_parts(handle.id(), None, base_seq, max_messages, MAX_TURN_BYTES)
        .map_err(|e| faktor_core::Error::from(faktor_session::SessionError::Store(e)))?;
    let mut newest_seq = base_seq.unwrap_or(0);
    for (row, parts) in rows.into_iter().rev() {
        if base_seq.is_some_and(|seq| (row.seq as u64) <= seq) {
            continue;
        }
        newest_seq = newest_seq.max(row.seq.max(0) as u64);
        let mut pushed_text = false;
        for part in &parts {
            match part.kind.as_str() {
                "text" => {
                    if let Some(text) = part.data.get("text").and_then(|v| v.as_str()) {
                        turns.push(RecentTurn {
                            role: row.role.clone(),
                            text: truncate_turn(text),
                        });
                        pushed_text = true;
                    }
                }
                "tool_result" => {
                    let excerpt = part
                        .data
                        .get("output")
                        .or_else(|| part.data.get("text"))
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    turns.push(RecentTurn {
                        role: "tool".into(),
                        text: truncate_turn(excerpt),
                    });
                }
                "tool_call" => {
                    let name = part
                        .data
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("tool");
                    let input = part
                        .data
                        .get("input")
                        .map(|v| v.to_string())
                        .unwrap_or_default();
                    turns.push(RecentTurn {
                        role: "tool_call".into(),
                        text: truncate_turn(&format!("{name}({input})")),
                    });
                }
                _ => {}
            }
        }
        if row.role == "user" && !pushed_text {
            if let Some(text) = row.data.get("text").and_then(|v| v.as_str()) {
                if !text.is_empty() {
                    turns.push(RecentTurn {
                        role: "user".into(),
                        text: truncate_turn(text),
                    });
                }
            }
        }
    }
    Ok((turns, newest_seq))
}

fn truncate_turn(text: &str) -> String {
    if text.len() <= MAX_TURN_TEXT_BYTES {
        return text.to_string();
    }
    let mut cut = MAX_TURN_TEXT_BYTES;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text[..cut].to_string()
}

#[allow(clippy::too_many_arguments)]
async fn execute_compaction(
    state: &AppState,
    handle: &faktor_session::SessionHandle,
    operation_id: &str,
    target_tokens: Option<i64>,
    min_reduction: f64,
    max_messages: u64,
    model: Option<&(String, String, String)>,
    token: &CancellationToken,
) -> Result<faktor_session::compaction::CompactionOperation, Response> {
    use faktor_session::compaction::{CompactionOperationState, CompactionPolicy};
    let advance = |state: CompactionOperationState, phase: &str, progress: i64| {
        handle
            .advance_compaction_operation(
                operation_id,
                state,
                phase,
                progress,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
            .map_err(|e| api_err(&e))
    };
    if token.is_cancelled()
        || handle
            .compaction_cancel_requested(operation_id)
            .unwrap_or(false)
    {
        return finish_cancelled(handle, operation_id, None).map_err(|e| api_err(&e));
    }
    advance(
        CompactionOperationState::Planning,
        "loading-history",
        100_000,
    )?;
    let (turns, newest_seq) = match load_turns(state, handle, max_messages) {
        Ok(turns) => turns,
        Err(e) => return finish_failed(handle, operation_id, &e),
    };
    if token.is_cancelled()
        || handle
            .compaction_cancel_requested(operation_id)
            .unwrap_or(false)
    {
        return finish_cancelled(handle, operation_id, None).map_err(|e| api_err(&e));
    }
    let ledger: faktor_context::ledger::TaskLedger = handle
        .get_task_ledger()
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    let before = (faktor_context::estimate_history_tokens(&turns)
        + faktor_context::estimate_history_tokens(&[RecentTurn {
            role: "system".into(),
            text: ledger.compact_render(),
        }])) as i64;
    if before <= 0 {
        return finish_failed_with(
            handle,
            operation_id,
            "empty_context",
            "no compactable history",
        );
    }
    let target = target_tokens.unwrap_or_else(|| {
        let keep = ((before as f64) * (1.0 - min_reduction)).floor() as i64;
        keep.clamp(1, before - 1)
    });
    let target = target.clamp(1, before - 1);
    let _ = advance(
        CompactionOperationState::Reducing,
        "reducing-history",
        450_000,
    )?;
    let compactor = match model {
        Some((full, provider, model_id)) => {
            let Some(provider_impl) = state.deps.agent.deps().providers.get(provider) else {
                return finish_failed_with(
                    handle,
                    operation_id,
                    "unserved_compaction_model",
                    &format!("provider {provider:?} is not registered"),
                );
            };
            let op_id = match state.deps.session.try_next_op_id() {
                Ok(op_id) => op_id,
                Err(e) => return Err(api_err(&faktor_core::Error::from(e))),
            };
            let summarizer = std::sync::Arc::new(ManualSummarizer {
                provider: provider_impl,
                model: model_id.to_string(),
                op_id,
                session_id: handle.id(),
                cancellation: token.child(),
            });
            let _ = full;
            Compactor::new(Some(summarizer))
        }
        None => Compactor::deterministic_only(),
    };
    let plan = compactor
        .compact(
            &turns,
            &ledger,
            &CompactionRequest {
                before_tokens: before as usize,
                target_tokens: target as usize,
                min_reduction_ratio: min_reduction,
                summarizer_residue_budget_tokens:
                    faktor_context::DEFAULT_SUMMARIZER_RESIDUE_BUDGET_TOKENS,
            },
        )
        .await;
    if token.is_cancelled()
        || handle
            .compaction_cancel_requested(operation_id)
            .unwrap_or(false)
    {
        return finish_cancelled(
            handle,
            operation_id,
            Some((before, plan.after_tokens as i64)),
        )
        .map_err(|e| api_err(&e));
    }
    let through_seq = checkpoint_through_seq(handle).unwrap_or(0).max(newest_seq);
    let digest = faktor_context::history_digest(&plan.kept_recent);
    let payload = match serde_json::to_vec(&plan.kept_recent) {
        Ok(payload) => payload,
        Err(e) => {
            return finish_failed_with(
                handle,
                operation_id,
                "checkpoint_serialization_failed",
                &e.to_string(),
            )
        }
    };
    let _ = advance(CompactionOperationState::Committing, "committing", 900_000)?;
    // The last cancellation point: after this line the commit is atomic
    // (checkpoint + compaction row + journal event in one transaction).
    if token.is_cancelled()
        || handle
            .compaction_cancel_requested(operation_id)
            .unwrap_or(false)
    {
        return finish_cancelled(
            handle,
            operation_id,
            Some((before, plan.after_tokens as i64)),
        )
        .map_err(|e| api_err(&e));
    }
    let strategy = match plan.strategy {
        faktor_context::CompactionStrategy::LlmSummary => "llm_summary",
        faktor_context::CompactionStrategy::DeterministicPruning => "deterministic_pruning",
        faktor_context::CompactionStrategy::Rejected => "rejected",
    };
    let record = match handle.commit_compaction_with_checkpoint(
        before,
        plan.after_tokens as i64,
        plan.target_tokens as i64,
        plan.accepted,
        strategy,
        &CompactionPolicy {
            min_reduction_ratio: min_reduction,
        },
        through_seq,
        &digest,
        &payload,
    ) {
        Ok(record) => record,
        Err(e) => return finish_failed(handle, operation_id, &e),
    };
    let reduction = if before == 0 {
        0.0
    } else {
        1.0 - (plan.after_tokens as f64 / before as f64)
    };
    let result = serde_json::json!({
        "accepted": record.accepted,
        "before": before,
        "after": plan.after_tokens,
        "target": plan.target_tokens,
        "reduction": reduction,
        "minimumReduction": min_reduction,
        "strategy": strategy,
        "summaryRevision": record.id,
        "summaryVersion": record.id.to_string(),
        "summaryDigest": digest,
        "checkpointThroughSeq": through_seq,
        "rejectedReason": record.reason,
        "retainedFacts": {
            "goal": plan.ledger.goal,
            "constraints": plan.ledger.constraints,
            "openSteps": plan.ledger.open_steps,
            "decisions": plan.ledger.decisions,
            "knownFailures": plan.ledger.known_failures,
            "changedFiles": plan.ledger.changed_files,
        },
        "keptRecentTurns": plan.kept_recent.len(),
        "removed": {
            "archivedChunks": plan.archive_chunks.len(),
            "archivedRefs": plan.archived.len(),
            "evidenceRefs": plan.evidence_refs.len(),
            "originalHistory": {
                "kind": "session_transcript",
                "sessionId": handle.id().to_string(),
                "throughSeq": through_seq,
                "note": "the immutable original rows remain in the durable transcript; re-expanding UI text does not restore model context",
            },
        },
        "basis": {
            "kind": "estimated",
            "method": "history-estimator",
            "version": "1",
        },
    });
    let terminal = if record.accepted {
        CompactionOperationState::Applied
    } else {
        CompactionOperationState::Refused
    };
    let op = match handle.advance_compaction_operation(
        operation_id,
        terminal,
        terminal.as_str(),
        1_000_000,
        Some(before),
        Some(plan.after_tokens as i64),
        Some(plan.target_tokens as i64),
        Some(strategy),
        Some(&result),
        None,
        None,
    ) {
        Ok(Some(op)) => op,
        Ok(None) => {
            return finish_failed_with(
                handle,
                operation_id,
                "operation_state_race",
                "the operation became terminal while committing",
            )
        }
        Err(e) => return Err(api_err(&e)),
    };
    Ok(op)
}

fn finish_cancelled(
    handle: &faktor_session::SessionHandle,
    operation_id: &str,
    sizes: Option<(i64, i64)>,
) -> faktor_core::Result<faktor_session::compaction::CompactionOperation> {
    let result = serde_json::json!({
        "accepted": false,
        "cancelled": true,
        "before": sizes.map(|s| s.0),
        "after": sizes.map(|s| s.1),
        "reason": "cancelled before commit; the prior basis remains",
        "basis": {"kind": "estimated", "method": "history-estimator", "version": "1"},
    });
    let (before, after) = match sizes {
        Some((before, after)) => (Some(before), Some(after)),
        None => (None, None),
    };
    handle
        .advance_compaction_operation(
            operation_id,
            faktor_session::compaction::CompactionOperationState::Cancelled,
            "cancelled",
            1_000_000,
            before,
            after,
            None,
            None,
            Some(&result),
            Some("cancelled"),
            Some("compaction was cancelled before commit; the prior basis remains"),
        )?
        .ok_or_else(|| {
            faktor_session::SessionError::Conflict("compaction operation already terminal".into())
                .into()
        })
}

fn finish_failed(
    handle: &faktor_session::SessionHandle,
    operation_id: &str,
    error: &faktor_core::Error,
) -> Result<faktor_session::compaction::CompactionOperation, Response> {
    finish_failed_with(
        handle,
        operation_id,
        "compaction_failed",
        &error.to_string(),
    )
}

fn finish_failed_with(
    handle: &faktor_session::SessionHandle,
    operation_id: &str,
    code: &str,
    message: &str,
) -> Result<faktor_session::compaction::CompactionOperation, Response> {
    handle
        .advance_compaction_operation(
            operation_id,
            faktor_session::compaction::CompactionOperationState::Failed,
            "failed",
            1_000_000,
            None,
            None,
            None,
            None,
            None,
            Some(code),
            Some(message),
        )
        .map_err(|e| api_err(&e))?
        .ok_or_else(|| {
            conflict(
                "operation_terminal",
                "the operation already reached a terminal state",
            )
        })
}

// ------------------------------------------------------------- memory records

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeMemoryRecordsQuery {
    session: String,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default, rename = "includeDeleted")]
    include_deleted: Option<bool>,
    #[serde(default, rename = "afterMs")]
    after_ms: Option<i64>,
    #[serde(default, rename = "afterKind")]
    after_kind: Option<String>,
    #[serde(default, rename = "afterKey")]
    after_key: Option<String>,
    #[serde(default)]
    limit: Option<u64>,
}

fn record_json(fact: &MemoryFactV2) -> serde_json::Value {
    serde_json::json!({
        "id": fact.id,
        "scope": fact.scope.kind(),
        "scopeId": fact.scope.id(),
        "subject": fact.subject,
        "predicate": fact.predicate,
        "value": fact.value,
        "provenance": {
            "origin": fact.provenance.origin.as_str(),
            "recordedMs": fact.provenance.recorded_ms,
            "assertedBy": fact.provenance.asserted_by,
            "sourceRef": fact.provenance.source_ref,
        },
        "confidencePpm": fact.confidence_ppm,
        "version": fact.version,
        "createdMs": fact.created_ms,
        "updatedMs": fact.updated_ms,
        "deleted": fact.deleted.as_ref().map(|d| serde_json::json!({
            "deletedMs": d.deleted_ms,
            "reason": d.reason,
            "atRevision": d.at_revision,
        })),
        "previous": fact.previous.as_ref().map(|p| serde_json::json!({
            "version": p.version,
            "value": p.value,
            "updatedMs": p.updated_ms,
        })),
    })
}

fn memory_scope_for(
    handle: &faktor_session::SessionHandle,
    requested: &str,
) -> Result<MemoryScope, MemoryAdminError> {
    let row = handle
        .row()
        .map_err(|e| MemoryAdminError::Store(faktor_store::StoreError::Malformed(e.to_string())))?;
    faktor_memory::scope_from_request(requested, handle.id(), row.workspace_id.raw())
}

fn policy_scope(handle: &faktor_session::SessionHandle) -> Result<(String, i64), Response> {
    let row = handle.row().map_err(|e| api_err(&e))?;
    Ok(("workspace".to_string(), row.workspace_id.raw() as i64))
}

fn memory_store(state: &AppState, handle: &faktor_session::SessionHandle) -> MemoryRecordStore {
    MemoryRecordStore::new(state.deps.session.store(), handle.id())
}

/// `GET /native/memory/records` — managed memory rows with provenance,
/// versions and tombstones. `scope=all` merges the session and workspace
/// scopes; neither can see another workspace.
pub(crate) async fn native_memory_records_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<NativeMemoryRecordsQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(query) => query,
        Err(_) => return malformed("invalid memory records query"),
    };
    let handle = match native_resolve_session(&state, &q.session) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    let cursor = match (q.after_ms, q.after_kind, q.after_key) {
        (Some(ms), Some(kind), Some(key)) => Some((ms, kind, key)),
        (None, None, None) => None,
        _ => return malformed("memory cursor requires afterMs, afterKind and afterKey together"),
    };
    let limit = q.limit.unwrap_or(50).clamp(1, 200) as usize;
    let include_deleted = q.include_deleted.unwrap_or(false);
    let requested = q.scope.as_deref().unwrap_or("session");
    let store = memory_store(&state, &handle);
    let scopes = if requested == "all" {
        match (
            memory_scope_for(&handle, "session"),
            memory_scope_for(&handle, "workspace"),
        ) {
            (Ok(session), Ok(workspace)) => vec![session, workspace],
            (Err(e), _) | (_, Err(e)) => return memory_error(e),
        }
    } else {
        match memory_scope_for(&handle, requested) {
            Ok(scope) => vec![scope],
            Err(e) => return memory_error(e),
        }
    };
    let mut records = Vec::new();
    let mut has_more = false;
    for (index, scope) in scopes.iter().enumerate() {
        let page = match store.list(
            scope,
            include_deleted,
            if index == 0 && requested != "all" {
                cursor.as_ref()
            } else {
                None
            },
            limit.saturating_sub(records.len()).max(1),
        ) {
            Ok(page) => page,
            Err(e) => return memory_error(e),
        };
        has_more |= page.has_more;
        records.extend(page.records);
        if records.len() >= limit {
            has_more = true;
            break;
        }
    }
    records.sort_by(|a, b| {
        b.updated_ms
            .cmp(&a.updated_ms)
            .then_with(|| b.row_kind().cmp(&a.row_kind()))
            .then_with(|| b.id.cmp(&a.id))
    });
    records.truncate(limit);
    // A merged two-scope page has no single resumable cursor; the IDE pages
    // a concrete scope for stable cursor semantics.
    let next_cursor = if requested == "all" {
        serde_json::Value::Null
    } else {
        records
            .last()
            .map(|f| {
                serde_json::json!({
                    "afterMs": f.updated_ms,
                    "afterKind": f.row_kind(),
                    "afterKey": f.id,
                })
            })
            .unwrap_or(serde_json::Value::Null)
    };
    Json(serde_json::json!({
        "records": records.iter().map(record_json).collect::<Vec<_>>(),
        "hasMore": has_more,
        "nextCursor": next_cursor,
        "scope": requested,
        "includeDeleted": include_deleted,
    }))
    .into_response()
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeMemoryCreateBody {
    session: String,
    scope: String,
    subject: String,
    predicate: String,
    value: TypedMemoryValue,
    #[serde(default)]
    provenance: Option<NativeProvenanceBody>,
    #[serde(default, rename = "confidencePpm")]
    confidence_ppm: Option<u32>,
    #[serde(default)]
    evidence: Vec<String>,
    #[serde(default, rename = "sourceRevision")]
    source_revision: Option<String>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeProvenanceBody {
    #[serde(default)]
    origin: Option<String>,
    #[serde(default, rename = "assertedBy")]
    asserted_by: Option<String>,
    #[serde(default, rename = "sourceRef")]
    source_ref: Option<String>,
}

fn provenance_from(
    body: Option<NativeProvenanceBody>,
    now_ms: i64,
) -> Result<Provenance, MemoryAdminError> {
    let Some(body) = body else {
        return Ok(Provenance::user(now_ms));
    };
    let origin = match body.origin.as_deref().unwrap_or("user") {
        "user" => ProvenanceOrigin::User,
        "tool" => ProvenanceOrigin::Tool,
        "verification" => ProvenanceOrigin::Verification,
        "model" => ProvenanceOrigin::Model,
        "import" => ProvenanceOrigin::Import,
        "recovery" => ProvenanceOrigin::Recovery,
        other => {
            return Err(MemoryAdminError::Malformed(format!(
                "unknown memory provenance origin {other:?}"
            )))
        }
    };
    Ok(Provenance {
        origin,
        recorded_ms: now_ms,
        asserted_by: body.asserted_by,
        source_ref: body.source_ref,
    })
}

fn policy_check_scope(
    state: &AppState,
    handle: &faktor_session::SessionHandle,
    requested: &str,
) -> Result<MemoryPolicy, Response> {
    let (scope, scope_id) = policy_scope(handle)?;
    let policy = faktor_memory::MemoryPolicyStore::new(state.deps.session.store())
        .get(&scope, scope_id)
        .map_err(memory_error)?;
    match requested {
        "session" if !policy.allow_session_scope => Err(conflict(
            "memory_scope_disabled",
            "session memory is disabled by policy",
        )),
        "workspace" | "project" if !policy.allow_workspace_scope => Err(conflict(
            "memory_scope_disabled",
            "workspace memory is disabled by policy",
        )),
        _ => Ok(policy),
    }
}

/// `POST /native/memory/records` — save one memory with identity dedupe and
/// provenance. Identical live content is a true no-op; a different live value
/// is a typed conflict that requires an explicit versioned update.
pub(crate) async fn native_memory_records_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<NativeMemoryCreateBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return malformed("invalid memory create body"),
    };
    let handle = match native_resolve_session(&state, &req.session) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    let policy = match policy_check_scope(&state, &handle, &req.scope) {
        Ok(policy) => policy,
        Err(response) => return response,
    };
    let scope = match memory_scope_for(&handle, &req.scope) {
        Ok(scope) => scope,
        Err(e) => return memory_error(e),
    };
    let now = state.deps.agent.deps().clock.now_ms();
    let provenance = match provenance_from(req.provenance, now) {
        Ok(p) => p,
        Err(e) => return memory_error(e),
    };
    let draft = NewMemoryRecord {
        scope,
        subject: req.subject,
        predicate: req.predicate,
        value: req.value,
        provenance,
        confidence_ppm: req.confidence_ppm.unwrap_or(1_000_000).min(1_000_000),
        evidence: req.evidence,
        source_revision: req.source_revision,
    };
    match memory_store(&state, &handle).create_bounded(draft, now, policy.max_records) {
        Ok(result) => Json(serde_json::json!({
            "created": result.created,
            "deduped": result.deduped,
            "restored": result.restored,
            "record": record_json(&result.record),
        }))
        .into_response(),
        Err(e) => memory_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeMemoryUpdateBody {
    session: String,
    #[serde(rename = "expectedVersion")]
    expected_version: u32,
    value: TypedMemoryValue,
}

/// `PUT /native/memory/records/{id}` — versioned edit.
pub(crate) async fn native_memory_records_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<NativeMemoryUpdateBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return malformed("invalid memory update body"),
    };
    let handle = match native_resolve_session(&state, &req.session) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    let now = state.deps.agent.deps().clock.now_ms();
    match memory_store(&state, &handle).update(&id, req.expected_version, req.value, now) {
        Ok(result) => Json(serde_json::json!({
            "saved": true,
            "record": record_json(&result.record),
        }))
        .into_response(),
        Err(e) => memory_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeMemoryDeleteQuery {
    session: String,
    #[serde(rename = "expectedVersion")]
    expected_version: u32,
    #[serde(default)]
    reason: Option<String>,
}

/// `DELETE /native/memory/records/{id}` — tombstone that stops FUTURE use
/// and returns the S66-scoped receipt (earlier copies are retained).
pub(crate) async fn native_memory_records_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    query: Result<Query<NativeMemoryDeleteQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(query) => query,
        Err(_) => return malformed("invalid memory delete query"),
    };
    let handle = match native_resolve_session(&state, &q.session) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    let revision = handle
        .last_event_seq()
        .ok()
        .flatten()
        .map(|s| s.raw())
        .unwrap_or(0);
    let now = state.deps.agent.deps().clock.now_ms();
    match memory_store(&state, &handle).delete(
        &id,
        q.expected_version,
        q.reason,
        revision,
        now,
    ) {
        Ok(receipt) => Json(serde_json::json!({
            "deleted": true,
            "receipt": {
                "id": receipt.record.id,
                "scope": receipt.record.scope.kind(),
                "scopeId": receipt.record.scope.id(),
                "futureUseStoppedAtRevision": receipt.at_revision,
                "deletedValueHash": receipt.deleted_value_hash,
                "retainedPriorCopies": receipt.retained_prior_copies,
                "retainedPriorArtifactRefs": [],
                "retentionNote": "prior prompt/artifact copies already materialized are retained by their owning stores; this deletion stops future memory reads",
                "record": record_json(&receipt.record),
            },
        }))
        .into_response(),
        Err(e) => memory_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeMemoryRestoreBody {
    session: String,
    #[serde(rename = "expectedVersion")]
    expected_version: u32,
}

/// `POST /native/memory/records/{id}/restore` — explicit revive (S12).
pub(crate) async fn native_memory_records_restore(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<NativeMemoryRestoreBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return malformed("invalid memory restore body"),
    };
    let handle = match native_resolve_session(&state, &req.session) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    let now = state.deps.agent.deps().clock.now_ms();
    match memory_store(&state, &handle).restore(&id, req.expected_version, now) {
        Ok(result) => Json(serde_json::json!({
            "restored": true,
            "record": record_json(&result.record),
        }))
        .into_response(),
        Err(e) => memory_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeMemoryPolicyQuery {
    session: String,
}

/// `GET /native/memory/policy` — the effective workspace memory policy.
pub(crate) async fn native_memory_policy_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<NativeMemoryPolicyQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(query) => query,
        Err(_) => return malformed("invalid memory policy query"),
    };
    let handle = match native_resolve_session(&state, &q.session) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    let (scope, scope_id) = match policy_scope(&handle) {
        Ok(scope) => scope,
        Err(response) => return response,
    };
    match faktor_memory::MemoryPolicyStore::new(state.deps.session.store()).get(&scope, scope_id) {
        Ok(policy) => Json(serde_json::json!({
            "scope": scope,
            "scopeId": scope_id,
            "policy": policy,
            // K-05: the REAL efficiency switch is a separate authority; the
            // memory policy can never turn it on or off.
            "failureLearning": {
                "enabled": state.deps.agent.deps().efficiency.failure_learning,
                "separate": true,
                "authority": "efficiency.failure_learning",
            },
        }))
        .into_response(),
        Err(e) => memory_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeMemoryPolicyBody {
    session: String,
    #[serde(rename = "expectedRevision")]
    expected_revision: u64,
    policy: MemoryPolicy,
}

/// `PUT /native/memory/policy` — atomic policy save with a revision CAS.
pub(crate) async fn native_memory_policy_put(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<NativeMemoryPolicyBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return malformed("invalid memory policy body"),
    };
    let handle = match native_resolve_session(&state, &req.session) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    let (scope, scope_id) = match policy_scope(&handle) {
        Ok(scope) => scope,
        Err(response) => return response,
    };
    let now = state.deps.agent.deps().clock.now_ms();
    match faktor_memory::MemoryPolicyStore::new(state.deps.session.store()).put_cas(
        &scope,
        scope_id,
        req.expected_revision,
        req.policy,
        now,
    ) {
        Ok(policy) => Json(serde_json::json!({
            "saved": true,
            "scope": scope,
            "scopeId": scope_id,
            "policy": policy,
        }))
        .into_response(),
        Err(e) => memory_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeMemoryRetentionBody {
    session: String,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default, rename = "olderThanDays")]
    older_than_days: Option<u64>,
    #[serde(default, rename = "dryRun")]
    dry_run: Option<bool>,
    #[serde(default)]
    limit: Option<u64>,
}

/// `POST /native/memory/records/retention` — bounded tombstone retention
/// (dry-run lists the exact records a real pass would delete).
pub(crate) async fn native_memory_records_retention(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<NativeMemoryRetentionBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return malformed("invalid memory retention body"),
    };
    let handle = match native_resolve_session(&state, &req.session) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    let scope = match memory_scope_for(&handle, req.scope.as_deref().unwrap_or("session")) {
        Ok(scope) => scope,
        Err(e) => return memory_error(e),
    };
    let days = req
        .older_than_days
        .unwrap_or(u64::from(MemoryPolicy::default().retention_days));
    if days > u64::from(faktor_memory::MAX_MEMORY_POLICY_DAYS) {
        return malformed("olderThanDays exceeds the bound");
    }
    let now = state.deps.agent.deps().clock.now_ms();
    let cutoff = now.saturating_sub(days.saturating_mul(86_400_000) as i64);
    let dry_run = req.dry_run.unwrap_or(true);
    match memory_store(&state, &handle).retention(
        &scope,
        cutoff,
        req.limit.unwrap_or(500).clamp(1, 1000) as usize,
        !dry_run,
    ) {
        Ok(plan) => Json(serde_json::json!({
            "dryRun": dry_run,
            "scope": scope.kind(),
            "scopeId": scope.id(),
            "cutoffMs": plan.cutoff_ms,
            "wouldDelete": if dry_run { plan.records.len() } else { 0 },
            "deleted": if dry_run { 0 } else { plan.records.len() },
            "candidates": plan.records.iter().map(|f| serde_json::json!({
                "id": f.id,
                "kind": "memory",
                "scope": f.scope.kind(),
                "deletedMs": f.deleted.as_ref().map(|d| d.deleted_ms),
                "version": f.version,
            })).collect::<Vec<_>>(),
            "scanned": plan.scanned,
            "truncated": plan.truncated,
        }))
        .into_response(),
        Err(e) => memory_error(e),
    }
}

/// S08/S62/S10 overlay on the durable session projection:
/// - `contextUsage` = the newest capture's served usage, or `null`;
/// - `compaction` = the newest durable operation (K-04);
/// - `longTask` = live/stale threshold-compaction marker + child phase.
pub(crate) fn augment_projection(
    state: &AppState,
    handle: &faktor_session::SessionHandle,
    body: &mut serde_json::Value,
) {
    let context_usage = state
        .deps
        .foundation
        .as_deref()
        .and_then(|foundation| {
            foundation
                .context
                .latest(handle.id().raw())
                .ok()
                .flatten()
                .map(|manifest| manifest.usage())
        })
        .map(|usage| serde_json::to_value(usage).unwrap_or(serde_json::Value::Null))
        .unwrap_or(serde_json::Value::Null);
    let compaction = latest_compaction_projection(handle);
    let now_ms = state.deps.agent.deps().clock.now_ms();
    let auto_compaction = handle.runtime_compaction().ok().flatten().map(|marker| {
        let started = marker
            .get("started_ms")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let age_ms = now_ms.saturating_sub(started).max(0);
        serde_json::json!({
            "trigger": marker.get("trigger").and_then(|v| v.as_str()).unwrap_or("threshold"),
            "startedMs": started,
            "ageMs": age_ms,
            "stale": age_ms > 10 * 60 * 1000,
        })
    });
    let execution_phase = handle
        .orchestrator_child_identity_get()
        .ok()
        .flatten()
        .and_then(|_| {
            handle
                .execution_phase()
                .ok()
                .map(|p| p.as_str().to_string())
        });
    let manual_compacting = handle
        .latest_compaction_operation()
        .ok()
        .flatten()
        .map(|op| !reconcile_stale_operation(handle, op).is_terminal())
        .unwrap_or(false);
    // `compacting` must be TRUE for either authority and never outlive the
    // threshold marker's stale bound: a crash-residue marker older than the
    // bound is reported as stale, not as live progress.
    let threshold_compacting = auto_compaction
        .as_ref()
        .and_then(|marker| marker.get("stale").and_then(|v| v.as_bool()))
        .map(|stale| !stale)
        .unwrap_or(false);
    body["contextUsage"] = context_usage;
    body["compaction"] = compaction.unwrap_or(serde_json::Value::Null);
    body["longTask"] = serde_json::json!({
        "executionPhase": execution_phase,
        "autoCompaction": auto_compaction,
        "compacting": manual_compacting || threshold_compacting,
    });
}

// Re-exported for the projection: latest compaction operation as DTO.
pub(crate) fn latest_compaction_projection(
    handle: &faktor_session::SessionHandle,
) -> Option<serde_json::Value> {
    handle
        .latest_compaction_operation()
        .ok()
        .flatten()
        .map(|op| operation_json(&reconcile_stale_operation(handle, op)))
}

#[cfg(test)]
#[path = "longtask_r8_tests.rs"]
mod longtask_r8_tests;

/// Explicit K-02 compaction summarizer: ONE bounded streaming provider
/// call over the deterministic residue. A failed/oversized/cancelled stream
/// returns an empty summary, so the compactor falls back to deterministic
/// pruning and never replaces history with a partial state transfer.
struct ManualSummarizer {
    provider: std::sync::Arc<dyn faktor_provider::Provider>,
    model: String,
    op_id: faktor_core::id::OpId,
    session_id: faktor_core::id::SessionId,
    cancellation: CancellationToken,
}

impl faktor_context::Summarizer for ManualSummarizer {
    fn summarize<'a>(
        &'a self,
        residue: &'a [faktor_context::RecentTurn],
        _durable_facts: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + 'a>> {
        Box::pin(async move {
            use faktor_provider::{
                ContentPart, GenericAgentRequest, ProviderChunk, RequestMessage, RequestMeta, Role,
            };
            use futures_util::StreamExt as _;
            const SUMMARY_MAX_CHARS: usize = 60_000;
            const SUMMARY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
            let request = GenericAgentRequest {
                model: self.model.clone(),
                system: "You are the Faktor context compactor. Produce a faithful state transfer that replaces the conversation: preserve the goal, constraints, decisions, changed files, unresolved blockers, verification results, and next actions. Never invent facts.".into(),
                messages: residue
                    .iter()
                    .map(|t| RequestMessage {
                        role: if t.role == "user" { Role::User } else { Role::Assistant },
                        content: vec![ContentPart::text(&t.text)],
                    })
                    .collect(),
                tools: vec![],
                max_output: Some(4096),
                reasoning: None,
                stream: true,
                meta: RequestMeta {
                    operation_id: self.op_id,
                    session_id: self.session_id,
                    provider: self.provider.id().into(),
                    attempt: 0,
                    deadline_ms: SUMMARY_TIMEOUT.as_millis() as u64,
                    cancellation: self.cancellation.child(),
                },
            };
            let mut stream = self.provider.stream(request);
            let mut text = String::new();
            let mut complete = false;
            let outcome = tokio::time::timeout(SUMMARY_TIMEOUT, async {
                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(ProviderChunk::Text { text: delta }) => {
                            if text.len() + delta.len() > SUMMARY_MAX_CHARS {
                                return false;
                            }
                            text.push_str(&delta);
                        }
                        Ok(ProviderChunk::Done) => {
                            complete = true;
                            break;
                        }
                        Ok(_) => {}
                        Err(_) => return false,
                    }
                }
                true
            })
            .await;
            if !matches!(outcome, Ok(true)) || (!complete && text.is_empty()) {
                return String::new();
            }
            text
        })
    }
}
