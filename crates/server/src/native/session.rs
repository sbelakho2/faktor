//! Durable session projections and cursor pages of the native protocol.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use faktor_core::state::SessionLifecycle;
use faktor_protocol::error::ApiError;
use faktor_store::PromptAdmissionClaim;

use super::verification::native_verification_facts;
use super::*;
use crate::api::AppState;
use faktor_cloud::money;

/// The snake_case lifecycle tag for native projections.
pub(crate) fn lifecycle_tag(l: SessionLifecycle) -> String {
    serde_json::to_string(&l)
        .unwrap_or_else(|_| "unknown".into())
        .trim_matches('"')
        .to_string()
}

/// One native projection snapshot (docs/native-protocol.md, GET
/// /session/{id}/projection). Every field maps to what the server can
/// read durably: state/session come from the session row; activeModel is
/// the effective provider/model
/// envelope of the current or most recent logical turn (durable turn
/// records); activeTool is the newest still-running durable tool-run row;
/// filesChanged comes from the durable task ledger (`changed_files`);
/// lastCheckpoint is the newest checkpoint row, present only when a
/// checkpoint service is wired (`ServerDeps.snapshots`); verification
/// lists still-open tool runs whose durable recovery strategy is
/// MarkUnknown (unknown external effects are forced to verification —
/// spec §7), bounded; queued is the durable queued-prompt count.
/// `progress` and `contextUsage` are always null in this revision: the
/// runtime has no numeric progress channel, and provider-call usage rows
/// carry no context-usage aggregate yet — the machine state, activeTool and
/// the journal carry the phase information. `prefixStability` (v13) IS
/// populated from the durable per-call prefix observations once a turn has
/// settled one.
pub(crate) fn build_native_projection(
    deps: &ServerDeps,
    handle: &faktor_session::SessionHandle,
) -> faktor_core::Result<serde_json::Value> {
    let row = handle.row()?;
    let state = row.state;
    // Durable task ledger: changed files (bounded by construction in the
    // ledger; hostile rows are read defensively — strings only, capped).
    let mut files_changed: Vec<String> = Vec::new();
    if let Some(ledger) = handle.get_task_ledger()? {
        if let Some(files) = ledger.get("changed_files").and_then(|f| f.as_array()) {
            for f in files.iter().take(256) {
                if let Some(p) = f.as_str() {
                    files_changed.push(p.to_string());
                }
            }
        }
    }
    // Effective model envelope: newest durable turn record (oldest first
    // in the store), null before the first turn.
    let active_model = handle.turn_records()?.last().map(|t| {
        serde_json::json!({
            "provider": t.effective_provider,
            "model": t.effective_model,
            "variant": t.variant,
        })
    });
    // Active tool: the newest durable tool-run row that is still running
    // (an interrupted row is reconstructed by crash recovery before the
    // next turn; none pending = no active tool).
    let pending = handle.pending_tool_runs()?;
    let active_tool = pending
        .iter()
        .max_by_key(|r| (r.started_ms, r.id))
        .map(|r| {
            serde_json::json!({
                "tool": r.tool,
                "opId": r.op_id.to_string(),
                "startedMs": r.started_ms,
                "status": r.status,
            })
        });
    // Verification: still-open runs carrying unknown external effects
    // (recovery strategy mark_unknown) are owed verification (§7).
    let verification: Vec<serde_json::Value> = pending
        .iter()
        .filter(|r| r.recovery.get("strategy").and_then(|s| s.as_str()) == Some("mark_unknown"))
        .take(MAX_NATIVE_VERIFICATION)
        .map(|r| {
            serde_json::json!({
                "opId": r.op_id.to_string(),
                "tool": r.tool,
                "startedMs": r.started_ms,
                "effectStatus": r.effect_status,
            })
        })
        .collect();
    // Checkpoint presence requires the real snapshot service; the newest
    // durable checkpoint row is projected when one exists.
    let last_checkpoint = if deps.snapshots.is_some() {
        handle.latest_checkpoint()?.map(|c| {
            serde_json::json!({
                "sequence": c.sequence,
                "path": c.path,
                "createdMs": c.created_ms,
                "restoredMs": c.restored_ms,
            })
        })
    } else {
        None
    };
    Ok(serde_json::json!({
        "session": {
            "id": row.id.to_string(),
            "title": row.title,
            "provider": row.provider,
            "model": row.model,
            "lifecycle": lifecycle_tag(row.lifecycle),
        },
        "state": {
            "machine": agent_state_tag(state),
            "label": state.label(),
            "active": state.is_active(),
            "terminal": state.is_terminal(),
        },
        "activeModel": active_model,
        "activeTool": active_tool,
        "progress": deps
            .agent
            .progress_view(row.id)
            .unwrap_or(serde_json::Value::Null),
        "filesChanged": files_changed,
        "lastCheckpoint": last_checkpoint,
        "verification": verification,
        "contextUsage": serde_json::Value::Null,
        "queued": handle.queued_prompt_count()?.max(0),
        // Additive prefix-cache stability (v13, audits 65-66): the session's
        // stored aggregate over the per-call prefix observations recorded by
        // the usage-settlement fill site; null before any completed provider
        // call recorded one (fresh session or pre-v13 rows).
        "prefixStability": handle
            .stored_prefix_stability()?
            .map(|a| {
                serde_json::json!({
                    "observations": a.observations,
                    "mean": a.mean,
                    "stdDev": a.std_dev,
                })
            }),
    }))
}

/// `GET /session/{id}/projection` — native v1 session projection
/// (auth-required like every native endpoint).
pub(crate) async fn native_session_projection(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let sid = match parse_session_id(&id) {
        Ok(s) => s,
        Err(e) => return wire_status(e),
    };
    let handle = match state.deps.session.get_session(sid) {
        Ok(Some(h)) => h,
        Ok(None) => return wire_status(not_found(&format!("session {sid}"))),
        Err(e) => return api_err(&e),
    };
    match build_native_projection(&state.deps, &handle) {
        Ok(mut v) => {
            super::longtask::augment_projection(&state, &handle, &mut v);
            Json(v).into_response()
        }
        Err(e) => api_err(&e),
    }
}

/// `GET /native/session/{id}/turns` — the durable turn records of the
/// session (one per admitted logical turn): envelope, status, timestamps.
/// Newest first, bounded.
pub(crate) async fn native_session_turns(
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
    match handle.turn_records() {
        Ok(rows) => {
            let out: Vec<serde_json::Value> = rows
                .iter()
                .rev()
                .take(MAX_NATIVE_LIST)
                .map(|t| {
                    serde_json::json!({
                        "opId": t.turn_op_id.to_string(),
                        "status": t.status,
                        "provider": t.effective_provider,
                        "model": t.effective_model,
                        "variant": t.variant,
                        "toolMode": t.tool_mode,
                        "startedAt": t.started_at,
                        "updatedMs": t.updated_ms,
                        "queueSeq": t.queue_seq,
                        "promptMessageId": t.prompt_message_id,
                    })
                })
                .collect();
            Json(out).into_response()
        }
        Err(e) => api_err(&e),
    }
}

/// Defensive string-array reader over the durable ledger JSON (hostile
/// values are skipped, capped at MAX_NATIVE_LIST).
pub(crate) fn ledger_strings(ledger: &serde_json::Value, key: &str) -> Vec<String> {
    ledger
        .get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str())
                .take(MAX_NATIVE_LIST)
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// The served task-view state. A durable task row is authoritative (its own
/// machine state, including `verifying`/`needs_verification`); the session
/// row is only a fallback for pre-task ledgers. `ReadyForNextTurn` means the
/// turn machine is idle between prompts, so it must never project as a live
/// "running" task.
fn task_view_state_tag(
    durable: Option<faktor_core::state::TaskState>,
    session_state: faktor_core::state::AgentState,
    open: &[String],
    completed: &[String],
    goal: &str,
) -> &'static str {
    if let Some(state) = durable {
        return state.label();
    }
    if session_state.is_active()
        && session_state != faktor_core::state::AgentState::ReadyForNextTurn
    {
        "running"
    } else if !open.is_empty() || (completed.is_empty() && !goal.is_empty()) {
        "in_progress"
    } else if !completed.is_empty() {
        "done"
    } else {
        "idle"
    }
}

/// `GET /native/session/{id}/tasks` — the durable task ledger as typed
/// JSON (goal, milestones, decisions, failures, changed files) plus the
/// session's durable verification facts. One entry per tracked task; today
/// the session ledger is single-task, so the array is either `[]` (no
/// task data yet) or one entry. The ledger row is read defensively: only
/// strings are copied, arrays are capped.
pub(crate) async fn native_session_tasks(
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
    let ledger = match handle.get_task_ledger() {
        Ok(l) => l,
        Err(e) => return api_err(&e),
    };
    let Some(ledger) = ledger else {
        // No ledger row yet: no tracked task.
        return Json(serde_json::json!([])).into_response();
    };
    let goal = ledger
        .get("goal")
        .and_then(|g| g.as_str())
        .unwrap_or("")
        .to_string();
    let completed = ledger_strings(&ledger, "completed_steps");
    let open = ledger_strings(&ledger, "open_steps");
    if goal.is_empty()
        && completed.is_empty()
        && open.is_empty()
        && ledger_strings(&ledger, "decisions").is_empty()
        && ledger_strings(&ledger, "changed_files").is_empty()
    {
        // A stored-but-empty ledger (a turn ran without task data) is not a
        // task; list nothing rather than a phantom entry.
        return Json(serde_json::json!([])).into_response();
    }
    // State derivation (documented): a durable task row is authoritative.
    // Without one, a genuinely live machine wins ("running"); open
    // milestones or a fresh goal with no completed work yet are
    // "in_progress"; completed work with nothing left open is "done".
    // `ReadyForNextTurn` is an idle-between-turns state, NOT a live
    // operation: treating it as active left a completed turn rendered as
    // "running" forever (the turn-level machine stays ready until the next
    // prompt).
    let row = match handle.row() {
        Ok(r) => r,
        Err(e) => return api_err(&e),
    };
    let task_row = match handle.get_task(row.task_id) {
        Ok(t) => t,
        Err(e) => return api_err(&e),
    };
    let state_tag = task_view_state_tag(
        task_row.as_ref().map(|task| task.state),
        row.state,
        &open,
        &completed,
        &goal,
    );
    // Additive progress + budget (audit P0-64): `progress` is the session's
    // live bounded progress record (null before the runtime tracked one);
    // `budget` is the DURABLE budget envelope of the session's typed task
    // row — token and monetary caps/spend from the task + cost-ledger
    // columns and the open (in-flight) reservation micro sum — null when no
    // typed task row exists yet (null-safe pre-first-reservation: a typed
    // row without any reservation reads openReservedMicro 0).
    let mut budget: Option<serde_json::Value> = None;
    // The additive engineering contract projection (frozen
    // `faktor_core::contract::EngineeringContract`, schema v31): always
    // present on the task entry — an empty contract for rows written before
    // the column existed — so clients can render constraints, invariants,
    // risks and proof obligations with their state/confidence/provenance
    // without a second read.
    let contract = task_row
        .as_ref()
        .map(|t| t.contract.clone())
        .unwrap_or_default();
    if let Some(task) = task_row {
        let cost = match state
            .deps
            .session
            .store()
            .cost_task_row(handle.id(), row.task_id)
        {
            Ok(c) => c,
            Err(e) => return api_err(&store_err_to_core(e)),
        };
        // In-flight reservations (schema v17 vocabulary: `reserved` —
        // dispatch never provably began — and `dispatched`, the request left
        // the process and may have billed) both hold their prediction; the
        // v15-era `open` state was renamed away at schema v17 and never
        // occurs in a migrated store.
        let reservations = match state.deps.session.store().cost_reservations_of(
            handle.id(),
            row.task_id,
            MAX_NATIVE_RESERVATIONS_SCAN,
        ) {
            Ok(rs) => rs,
            Err(e) => return api_err(&store_err_to_core(e)),
        };
        let open_micro = reservations
            .iter()
            .filter(|r| r.status == "reserved" || r.status == "dispatched")
            .fold(0u64, |acc, r| acc.saturating_add(r.predicted_micro));
        budget = Some(serde_json::json!({
            "maxTokens": task.budget.max_tokens,
            "maxTurns": task.budget.max_turns,
            "spentTokens": task.budget.spent_tokens,
            "spentTurns": task.budget.spent_turns,
            "maxCostMicro": money::json_opt(cost.as_ref().and_then(|c| c.max_cost_micro)),
            "spentCostMicro": money::json(cost.as_ref().map(|c| c.spent_cost_micro).unwrap_or(0)),
            "openReservedMicro": money::json(open_micro),
        }));
    }
    let verification = match native_verification_facts(&handle) {
        Ok(v) => v,
        Err(e) => return api_err(&e),
    };
    let progress = state
        .deps
        .agent
        .progress_view(handle.id())
        .unwrap_or(serde_json::Value::Null);
    Json(serde_json::json!([{
        "goal": goal,
        "constraints": ledger_strings(&ledger, "constraints"),
        "state": state_tag,
        "milestones": { "completed": completed, "open": open },
        "decisions": ledger_strings(&ledger, "decisions"),
        "failures": ledger_strings(&ledger, "known_failures"),
        "changedFiles": ledger_strings(&ledger, "changed_files"),
        "tests": {
            "run": ledger_strings(&ledger, "tests_run"),
            "failed": ledger_strings(&ledger, "tests_failed"),
        },
        "preferences": ledger_strings(&ledger, "user_preferences"),
        "verification": verification,
        "progress": progress,
        "budget": budget,
        "contract": contract,
    }]))
    .into_response()
}

/// `GET /native/session/{id}/checkpoints` — the durable checkpoint rows of
/// the session (newest first), each with sequence/path/before-after hashes
/// and the restore audit. Empty array when the daemon runs without a
/// checkpoint service wired (`ServerDeps.snapshots` is `None`) or no
/// checkpoint was recorded yet.
pub(crate) async fn native_session_checkpoints(
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
    if state.deps.snapshots.is_none() {
        return Json(serde_json::json!([])).into_response();
    }
    match handle.checkpoints_newest(MAX_NATIVE_LIST as u64) {
        Ok(rows) => {
            let out: Vec<serde_json::Value> = rows
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "sequence": c.sequence,
                        "path": c.path,
                        "beforeHash": c.before_hash,
                        "afterHash": c.after_hash,
                        "beforeExists": c.before_exists,
                        "afterExists": c.after_exists,
                        "createdMs": c.created_ms,
                        "restoredMs": c.restored_ms,
                    })
                })
                .collect();
            Json(out).into_response()
        }
        Err(e) => api_err(&e),
    }
}

/// `GET /native/session/{id}/agents` — the real agent listing of one
/// session (path-id form of `/native/agents?session=`): every orchestrated
/// run's children plus the parent's own task runs, projected from the
/// durable rows ([`native_agents_body`]). Empty ONLY when the session
/// genuinely has no task run.
pub(crate) async fn native_session_agents(
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
    match native_agents_body(&state, &handle) {
        Ok(entries) => Json(entries).into_response(),
        Err(e) => wire_status(e),
    }
}

/// The native abort request DTO (strict `deny_unknown_fields`: an unknown
/// field or typo is a 400).
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeAbortRequest {
    pub session_id: String,
    pub op_id: Option<String>,
}

/// `POST /native/session/{id}/abort` — the native abort (audit 56): the
/// strict `NativeAbortRequest` body (`deny_unknown_fields` — an unknown
/// field or typo is a 400) carries the session id, which must match the
/// path id. `op_id` targets one queued prompt or the active turn; absent =
/// abort everything. Unknown sessions are 404; the semantics are
/// `sdk_abort`'s (queued-prompt kills never touch the machine).
pub(crate) async fn native_session_abort(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<NativeAbortRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    // Strict native DTO: every body rejection (syntax AND data errors —
    // unknown fields, typos, missing fields) is a plain 400, never a 422.
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => {
            let e = ApiError {
                code: "malformed",
                message: "invalid native abort request body".into(),
                http_status: 400,
                retryable: false,
            };
            return wire_status(e);
        }
    };
    let sid = match parse_session_id(&id) {
        Ok(s) => s,
        Err(e) => return wire_status(e),
    };
    let body_sid = match parse_session_id(&req.session_id) {
        Ok(s) => s,
        Err(e) => return wire_status(e),
    };
    if sid != body_sid {
        let e = ApiError {
            code: "malformed",
            message: format!("path session id {sid} does not match body session id {body_sid}"),
            http_status: 400,
            retryable: false,
        };
        return wire_status(e);
    }
    match state.deps.session.get_session(sid) {
        Ok(Some(_)) => {}
        Ok(None) => return wire_status(not_found(&format!("session {sid}"))),
        Err(e) => return api_err(&e),
    }
    let target = match &req.op_id {
        Some(raw) => {
            let parsed = match raw.parse::<u64>() {
                Ok(v) => v,
                Err(_) => {
                    let e = ApiError {
                        code: "malformed",
                        message: format!("invalid op_id {raw:?}: expected a decimal u64"),
                        http_status: 400,
                        retryable: false,
                    };
                    return wire_status(e);
                }
            };
            // Hostile request surface: a value that parses as u64 but is
            // outside the id contract (zero) must be a typed 400 naming the
            // field, never `OpId::new(parsed)` panicking the request task.
            match faktor_core::id::OpId::try_from(parsed) {
                Ok(op) => Some(op),
                Err(e) => {
                    let e = ApiError {
                        code: "malformed",
                        message: format!("invalid op_id {raw:?}: {e}"),
                        http_status: 400,
                        retryable: false,
                    };
                    return wire_status(e);
                }
            }
        }
        None => None,
    };
    match state.deps.agent.abort_op(sid, target) {
        Ok(ops) => Json(serde_json::json!({
            "aborted": ops.iter().map(|o| o.to_string()).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => api_err(&e),
    }
}

/// Strict native cursor page: `session` required; `before`/`limit`
/// optional. An unknown query field is a 400.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeMessagesQuery {
    session: String,
    #[serde(default)]
    before: Option<i64>,
    #[serde(default)]
    limit: Option<u64>,
}

/// One native message page bound (bounded everything): oversized `limit`
/// values are rejected with a 400, never silently clamped. The comparison
/// happens in the `u64` domain before any cast, so `u64::MAX` can never wrap
/// into a negative `i64` and slip past the bound.
pub(crate) fn page_limit(limit: Option<u64>, max: i64) -> Result<i64, ApiError> {
    let max_u64 = u64::try_from(max).map_err(|_| malformed_body("invalid server page bound"))?;
    match limit {
        None => Ok(max),
        Some(0) => Err(malformed_body("limit must be >= 1")),
        Some(l) if l > max_u64 => Err(malformed_body(&format!(
            "limit {l} exceeds the native page bound {max}"
        ))),
        Some(l) => Ok(i64::try_from(l).expect("bounded above by max")),
    }
}

/// The store-side read bound of one validated native page: one extra row is
/// read so `hasMore` is exact. Checked arithmetic keeps the bound inside
/// `u64`/`i64`, so the store never receives a wrapped negative `LIMIT`.
fn store_page_bound(limit: i64) -> Result<u64, ApiError> {
    u64::try_from(limit)
        .ok()
        .and_then(|l| l.checked_add(1))
        .ok_or_else(|| malformed_body("invalid server page bound"))
}

/// `GET /native/messages?session=<id>&before=<seq>&limit=<n>` — cursor
/// paging over the durable message rows of one session (audit P0-64):
/// newest first, `before` cuts strictly (`seq < before`; absent = the
/// newest page), the page cap is 200. `hasMore`/`nextBefore` give the next
/// older page; rows are gapless per session, so paging across a fixture
/// never duplicates and never gaps. Parts are loaded per message in the
/// page only. Hostile ids 400, unknown sessions 404.
pub(crate) async fn native_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<NativeMessagesQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(query) => query,
        Err(_) => return wire_status(malformed_body("invalid query parameters (strict DTO)")),
    };
    if let Some(before) = q.before {
        if before < 1 {
            return wire_status(malformed_body("before must be >= 1"));
        }
    }
    let limit = match page_limit(q.limit, MAX_NATIVE_CURSOR_PAGE) {
        Ok(l) => l,
        Err(e) => return wire_status(e),
    };
    let read_bound = match store_page_bound(limit) {
        Ok(b) => b,
        Err(e) => return wire_status(e),
    };
    let handle = match native_resolve_session(&state, &q.session) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    let store = state.deps.session.store();
    // `before` is validated `>= 1` above, so the cast is lossless.
    let before = q.before.map(|seq| seq as u64);
    // ONE page read + ONE bounded batch parts read for the whole page (the
    // historical handler ran one `parts_of` query per message: 200 SQLite
    // round trips for a full page).
    let rows =
        match store.messages_window_with_parts(handle.id(), before, None, read_bound, u64::MAX) {
            Ok(r) => r,
            Err(e) => return api_err(&store_err_to_core(e)),
        };
    let has_more = rows.len() as i64 > limit;
    let mut page_rows = rows;
    page_rows.truncate(limit as usize);
    let next_before = if has_more {
        page_rows.last().map(|(r, _)| r.seq)
    } else {
        None
    };
    let mut messages: Vec<serde_json::Value> = Vec::with_capacity(page_rows.len());
    for (row, parts) in page_rows {
        let parts: Vec<serde_json::Value> = parts
            .iter()
            .map(|p| {
                serde_json::json!({
                    "kind": p.kind,
                    "createdMs": p.created_ms,
                    "data": p.data,
                })
            })
            .collect();
        messages.push(serde_json::json!({
            "seq": row.seq,
            "id": row.id,
            "role": row.role,
            "createdMs": row.created_ms,
            "data": row.data,
            "parts": parts,
        }));
    }
    Json(serde_json::json!({
        "sessionId": handle.id().to_string(),
        "messages": messages,
        "hasMore": has_more,
        "nextBefore": next_before,
    }))
    .into_response()
}

/// Strict native journal-page query of `/native/events`.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeEventsQuery {
    session: String,
    #[serde(default)]
    after: Option<u64>,
    #[serde(default)]
    limit: Option<u64>,
}

/// `GET /native/events?session=<id>&after=<seq>&limit=<n>` — the paged
/// native journal read: durable journal events with `seq > after`
/// ascending, one strict-DTO page at a time (`hasMore`/`nextCursor`), the
/// same bounded catch-up paging as the native SSE stream (page cap 256). `after` is the raw per-session journal
/// sequence (0 = from the beginning). Unknown sessions 404; hostile ids
/// 400.
pub(crate) async fn native_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<NativeEventsQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(query) => query,
        Err(_) => return wire_status(malformed_body("invalid query parameters (strict DTO)")),
    };
    let limit = match page_limit(q.limit, MAX_NATIVE_EVENT_PAGE) {
        Ok(l) => l,
        Err(e) => return wire_status(e),
    };
    let read_bound = match store_page_bound(limit) {
        Ok(b) => b,
        Err(e) => return wire_status(e),
    };
    let handle = match native_resolve_session(&state, &q.session) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    let after = q.after.unwrap_or(0);
    // Bounded read pool: a page read never blocks a Tokio worker.
    let events = match handle
        .events_range_async(after.saturating_add(1), Some(read_bound))
        .await
    {
        Ok(e) => e,
        Err(e) => return api_err(&e),
    };
    let has_more = events.len() as i64 > limit;
    let mut page_events = events;
    page_events.truncate(limit as usize);
    let next_cursor = if has_more {
        page_events.last().map(|e| e.seq.raw())
    } else {
        None
    };
    let rows: Vec<serde_json::Value> = page_events.iter().map(native_event_row).collect();
    Json(serde_json::json!({
        "sessionId": handle.id().to_string(),
        "events": rows,
        "hasMore": has_more,
        "nextCursor": next_cursor.map(|v| serde_json::json!(v)).unwrap_or(serde_json::Value::Null),
    }))
    .into_response()
}

/// One native journal frame row. The `/native/events` cursor pages and the
/// native SSE stream carry the exact same shape: BOTH serialize this one row
/// (the paged path through [`serde_json::to_value`]), so their bytes agree
/// under either `serde_json` object map — the alphabetical field order below
/// is the BTree order and stays the order `preserve_order` produces.
pub(crate) fn native_event_row(e: &faktor_core::event::Event) -> serde_json::Value {
    serde_json::to_value(NativeEventRowRef {
        kind: e.kind.as_tag(),
        op_id: e.op_id.map(OpIdWire),
        payload: &e.payload,
        seq: e.seq.raw(),
        state: e.state.as_tag(),
        ts_ms: e.ts_ms,
    })
    .unwrap_or(serde_json::Value::Null)
}

/// Borrowed variant of [`native_event_row`] for the SSE hot path: the row is
/// serialized straight from the event (payload borrowed, never deep-cloned
/// into an intermediate `Value` tree). The pre-serialized `kind`/`state` tags
/// are passed in so one frame serializes each tag once. Field declaration
/// order is alphabetical (`kind`, `opId`, `payload`, `seq`, `state`, `tsMs`):
/// the same row backs the paged path, so the two wire forms are byte-identical
/// whether `serde_json` uses BTree (sorts) or `preserve_order` (declaration).
#[derive(serde::Serialize)]
struct NativeEventRowRef<'a> {
    #[serde(rename = "kind")]
    kind: &'a str,
    #[serde(rename = "opId")]
    op_id: Option<OpIdWire>,
    #[serde(rename = "payload")]
    payload: &'a Option<serde_json::Value>,
    #[serde(rename = "seq")]
    seq: u64,
    #[serde(rename = "state")]
    state: &'a str,
    #[serde(rename = "tsMs")]
    ts_ms: i64,
}

/// Wire form of a durable op id: a DECIMAL STRING, exactly like the
/// historical `o.to_string()` field, but written through the serializer's
/// `collect_str` without building an intermediate `String`.
#[derive(Clone, Copy)]
struct OpIdWire(faktor_core::id::OpId);

impl serde::Serialize for OpIdWire {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(&self.0)
    }
}

/// Strict native SSE query of `/native/session/{id}/events`.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeSessionEventsQuery {
    #[serde(default)]
    after: Option<u64>,
}

/// Bounded flush policy of the native journal SSE stream.
///
/// Frames are NEVER merged: every durable event keeps its own `id:`/`event:`/
/// `data:` triple, so ordering, cursor resume and per-event semantics are
/// unchanged. Coalescing happens one layer below the protocol, at the HTTP
/// chunk boundary: up to [`SSE_CHUNK_MAX_FRAMES`] frames are written per
/// body chunk, flushing once [`SSE_CHUNK_MAX_BYTES`] is reached (a single
/// frame is never split), which cuts write/syscall and task-wakeup count
/// without changing a single wire byte of any frame.
///
/// - Live deltas (`model_chunk_received`) are collected for at most
///   [`SSE_DELTA_COALESCE_MS`] so a 100-300 token/s stream becomes a few
///   chunks per second instead of one wakeup per token.
/// - Any non-delta event (state/permission/tool/lifecycle) disables the
///   linger and is flushed immediately: attention events never wait behind
///   a delta window.
/// - Catch-up pages are already batched (up to
///   [`MAX_NATIVE_EVENT_PAGE`] rows) and never linger.
/// - With no durable tail change the stream sleeps on the session's journal
///   generation (an append wakes it immediately) with a
///   [`SSE_FALLBACK_POLL_MS`] fallback poll, and emits a heartbeat frame at
///   most every [`SSE_HEARTBEAT_MS`] (clients ignore heartbeats).
pub(crate) const SSE_DELTA_COALESCE_MS: u64 = 8;
pub(crate) const SSE_FALLBACK_POLL_MS: u64 = 250;
pub(crate) const SSE_HEARTBEAT_MS: u64 = 5_000;
pub(crate) const SSE_CHUNK_MAX_FRAMES: usize = 64;
pub(crate) const SSE_CHUNK_MAX_BYTES: usize = 64 * 1024;

/// `io::Write` adapter so serde_json serializes a row DIRECTLY into the
/// reused chunk buffer (no intermediate `String`, no `Value` tree).
struct ChunkWriter<'a>(&'a mut bytes::BytesMut);

impl std::io::Write for ChunkWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Append a decimal u64 without allocating (stack digits + one extend).
fn put_u64(buf: &mut bytes::BytesMut, mut value: u64) {
    let mut digits = [0u8; 20];
    let mut at = digits.len();
    loop {
        at -= 1;
        digits[at] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    buf.extend_from_slice(&digits[at..]);
}

/// Encode ONE durable journal event as an SSE frame into `buf`:
/// `id: <seq>\nevent: <kind>\ndata: <native_event_row>\n\n`. Byte-identical
/// to the `axum::response::sse::Event` encoding this stream used before;
/// `native_event_row` and this writer share one row serializer.
pub(crate) fn write_native_event_frame(buf: &mut bytes::BytesMut, e: &faktor_core::event::Event) {
    buf.extend_from_slice(b"id: ");
    put_u64(buf, e.seq.raw());
    buf.extend_from_slice(b"\nevent: ");
    buf.extend_from_slice(e.kind.as_tag().as_bytes());
    buf.extend_from_slice(b"\ndata: ");
    let frame_start = buf.len();
    let row = NativeEventRowRef {
        kind: e.kind.as_tag(),
        op_id: e.op_id.map(OpIdWire),
        payload: &e.payload,
        seq: e.seq.raw(),
        state: e.state.as_tag(),
        ts_ms: e.ts_ms,
    };
    if serde_json::to_writer(ChunkWriter(buf), &row).is_err() {
        // A row that cannot serialize must never produce a torn frame: roll
        // back to the data start and emit the historical `{}` fallback.
        buf.truncate(frame_start);
        buf.extend_from_slice(b"{}");
    }
    buf.extend_from_slice(b"\n\n");
}

/// `GET /native/session/{id}/events?after=<seq>` — the native durable
/// journal SSE stream (cursor `after` = replay `seq > after`, 0 = from the
/// beginning). Frames are `id: <seq>`, `event: <kind>`, `data:
/// <native_event_row>`; heartbeats (`event: heartbeat`) keep proxies alive
/// and are ignored by clients. Catch-up is paged (bounded), so a reconnect
/// against a huge journal can never balloon RAM and resumes exactly from
/// the cursor. Unknown sessions 404; hostile ids 400.
pub(crate) async fn native_session_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    query: Result<Query<NativeSessionEventsQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(query) => query,
        Err(_) => return wire_status(malformed_body("invalid query parameters (strict DTO)")),
    };
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    let cursor = q.after.unwrap_or(0) as i64;
    let stream = native_journal_stream(handle, cursor, std::sync::Arc::clone(&state.ready));
    let mut response = Response::new(axum::body::Body::from_stream(stream));
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/event-stream"),
    );
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-cache"),
    );
    response
}

type JournalStream = std::pin::Pin<
    Box<dyn futures_util::Stream<Item = Result<bytes::Bytes, std::convert::Infallible>> + Send>,
>;

/// The bounded, paged journal poll behind the native SSE stream.
///
/// One durable page (<= [`MAX_NATIVE_EVENT_PAGE`]) is read per catch-up
/// iteration; at most [`SSE_CHUNK_MAX_FRAMES`] frames /
/// [`SSE_CHUNK_MAX_BYTES`] bytes are emitted per HTTP chunk. A journal read
/// failure is terminal: the stream emits one `error` frame naming
/// `journal_read_failed` and then ends — it never heartbeats over a corrupt
/// or unreadable authority.
fn native_journal_stream(
    handle: faktor_session::SessionHandle,
    cursor: i64,
    ready: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> JournalStream {
    let rx = handle.subscribe_journal();
    let state = futures_util::stream::unfold(
        (
            handle,
            cursor,
            std::collections::VecDeque::new(),
            rx,
            bytes::BytesMut::new(),
            false,
            tokio::time::Instant::now(),
            false,
            tokio::time::Instant::now(),
        ),
        move |(
            handle,
            mut cursor,
            mut pending,
            mut rx,
            mut buf,
            terminated,
            mut last_emit,
            mut saw_data,
            mut linger_until,
        )| {
            let ready = std::sync::Arc::clone(&ready);
            async move {
                if terminated {
                    return None;
                }
                // `drain_now` flushes the queued frames on the next iteration;
                // `false` after a journal wake means "read the newer tail first"
                // so the coalescing window can absorb it.
                let mut drain_now = false;
                loop {
                    // Graceful daemon shutdown ends every long-lived stream on
                    // the next bounded tick; a dropped client body can never
                    // hold the listener open past that bound.
                    if !ready.load(std::sync::atomic::Ordering::Relaxed) {
                        return None;
                    }
                    // Drain whenever a flush was decided, the queued frames
                    // already fill a chunk, or the coalescing window expired.
                    // The deadline term is REQUIRED for a partially drained
                    // page: a 65..255-event catch-up leaves 1..63 frames after
                    // the first 64-frame chunk, and with no newer durable tail
                    // a pending-only stream would otherwise never flush them
                    // (the old state machine only flushed on a new read).
                    if !pending.is_empty()
                        && (drain_now
                            || pending.len() >= SSE_CHUNK_MAX_FRAMES
                            || tokio::time::Instant::now() >= linger_until)
                    {
                        let mut frames = 0usize;
                        while let Some(e) = pending.pop_front() {
                            if frames > 0
                                && (frames >= SSE_CHUNK_MAX_FRAMES
                                    || buf.len() >= SSE_CHUNK_MAX_BYTES)
                            {
                                pending.push_front(e);
                                break;
                            }
                            write_native_event_frame(&mut buf, &e);
                            cursor = e.seq.raw() as i64;
                            frames += 1;
                        }
                        let chunk = bytes::Bytes::copy_from_slice(&buf);
                        buf.clear();
                        last_emit = tokio::time::Instant::now();
                        saw_data = true;
                        return Some((
                            Ok::<bytes::Bytes, std::convert::Infallible>(chunk),
                            (
                                handle,
                                cursor,
                                pending,
                                rx,
                                buf,
                                terminated,
                                last_emit,
                                saw_data,
                                linger_until,
                            ),
                        ));
                    }

                    // Mark the generation BEFORE the read: an append that lands
                    // after this point changes the watched value, so the wait
                    // below wakes immediately instead of sleeping the fallback.
                    rx.borrow_and_update();
                    // Resume the READ at the last queued frame: a coalescing
                    // wake re-enters this loop with pending events that are not
                    // yet emitted, and re-reading them would duplicate frames.
                    let read_after = pending.back().map(|e| e.seq.raw() as i64).unwrap_or(cursor);
                    let events = match handle
                        .events_range_async(
                            read_after.saturating_add(1) as u64,
                            Some(MAX_NATIVE_EVENT_PAGE as u64),
                        )
                        .await
                    {
                        Ok(events) => events,
                        Err(e) => {
                            tracing::error!(
                                session_id = %handle.id(),
                                error = %e,
                                "native journal read failed; terminating the SSE stream"
                            );
                            buf.extend_from_slice(
                                b"event: error\ndata: {\"code\":\"journal_read_failed\"}\n\n",
                            );
                            let chunk = bytes::Bytes::copy_from_slice(&buf);
                            buf.clear();
                            return Some((
                                Ok::<bytes::Bytes, std::convert::Infallible>(chunk),
                                (
                                    handle,
                                    cursor,
                                    pending,
                                    rx,
                                    buf,
                                    true,
                                    last_emit,
                                    saw_data,
                                    linger_until,
                                ),
                            ));
                        }
                    };

                    if events.is_empty() {
                        if !pending.is_empty() {
                            // The durable tail is caught up while frames from a
                            // partially drained page are still queued. Flush
                            // them NOW: waiting for a new append (or the idle
                            // heartbeat/fallback loop) would strand the final
                            // delta-only frames of this connection.
                            drain_now = true;
                            continue;
                        }
                        if saw_data {
                            // One heartbeat immediately after the durable tail
                            // drains (clients/proxies see liveness right away),
                            // then at most one per SSE_HEARTBEAT_MS while idle.
                            saw_data = false;
                            buf.extend_from_slice(b"event: heartbeat\ndata: {}\n\n");
                            let chunk = bytes::Bytes::copy_from_slice(&buf);
                            buf.clear();
                            last_emit = tokio::time::Instant::now();
                            return Some((
                                Ok::<bytes::Bytes, std::convert::Infallible>(chunk),
                                (
                                    handle,
                                    cursor,
                                    pending,
                                    rx,
                                    buf,
                                    terminated,
                                    last_emit,
                                    saw_data,
                                    linger_until,
                                ),
                            ));
                        }
                        let now = tokio::time::Instant::now();
                        let heartbeat_due = last_emit
                            .checked_add(std::time::Duration::from_millis(SSE_HEARTBEAT_MS))
                            .unwrap_or(now);
                        let fallback = tokio::time::sleep(std::time::Duration::from_millis(
                            SSE_FALLBACK_POLL_MS,
                        ));
                        tokio::pin!(fallback);
                        let heartbeat = tokio::time::sleep_until(heartbeat_due);
                        tokio::pin!(heartbeat);
                        tokio::select! {
                            changed = rx.changed() => {
                                // A missed/failed watch channel falls back to
                                // the bounded poll; the durable cursor decides.
                                let _ = changed;
                                drain_now = false;
                            }
                            _ = &mut fallback => drain_now = false,
                            _ = &mut heartbeat => {
                                buf.extend_from_slice(b"event: heartbeat\ndata: {}\n\n");
                                let chunk = bytes::Bytes::copy_from_slice(&buf);
                                buf.clear();
                                last_emit = tokio::time::Instant::now();
                                return Some((
                                    Ok::<bytes::Bytes, std::convert::Infallible>(chunk),
                                    (handle, cursor, pending, rx, buf, terminated, last_emit, saw_data, linger_until),
                                ));
                            }
                        }
                        continue;
                    }

                    let page_full = events.len() >= MAX_NATIVE_EVENT_PAGE as usize;
                    let was_empty = pending.is_empty();
                    let mut has_non_delta = false;
                    for e in events {
                        has_non_delta |=
                            e.kind != faktor_core::event::EventKind::ModelChunkReceived;
                        pending.push_back(e);
                    }
                    if was_empty {
                        // The linger window is anchored at the FIRST unflushed
                        // delta, never reset by later arrivals: an unbounded
                        // reset would starve a continuous stream.
                        linger_until = tokio::time::Instant::now()
                            + std::time::Duration::from_millis(SSE_DELTA_COALESCE_MS);
                    }

                    // Immediate flush for non-delta events, full catch-up pages
                    // and an already-full chunk. Only a caught-up delta-only
                    // tail lingers for the bounded coalescing window.
                    if has_non_delta
                        || page_full
                        || pending.len() >= SSE_CHUNK_MAX_FRAMES
                        || tokio::time::Instant::now() >= linger_until
                    {
                        drain_now = true;
                        continue;
                    }
                    let linger = tokio::time::sleep_until(linger_until);
                    tokio::pin!(linger);
                    tokio::select! {
                        _ = rx.changed() => drain_now = false,
                        _ = &mut linger => drain_now = true,
                    }
                }
            }
        },
    );
    Box::pin(state)
}

// ------------------------------------------------------------ permissions

/// Strict native permissions query (`GET /native/permissions`).
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativePermissionsQuery {
    #[serde(default)]
    session: Option<String>,
}

/// `GET /native/permissions?session=<id>` — the pending permission
/// requests of the daemon (optionally filtered to one session):
/// `{permissions: [{id, session_id, capability, detail}]}`. Bounded by the
/// requester's live pending set.
pub(crate) async fn native_permissions(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<NativePermissionsQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(query) => query,
        Err(_) => return wire_status(malformed_body("invalid query parameters (strict DTO)")),
    };
    let filter = match q.session.as_deref() {
        Some(raw) => {
            let id = match parse_session_id(raw) {
                Ok(id) => id,
                Err(e) => return wire_status(e),
            };
            match state.deps.session.get_session(id) {
                Ok(Some(_)) => {}
                Ok(None) => return wire_status(not_found(&format!("session {id}"))),
                Err(e) => return api_err(&e),
            }
            Some(id)
        }
        None => None,
    };
    let permissions: Vec<serde_json::Value> = state
        .deps
        .permissions
        .pending_views()
        .into_iter()
        .filter(|v| filter.is_none_or(|f| v.session_id == f))
        .map(|v| {
            serde_json::json!({
                "id": v.id.to_string(),
                "session_id": v.session_id.to_string(),
                "capability": v.capability,
                "detail": v.detail,
            })
        })
        .collect();
    Json(serde_json::json!({ "permissions": permissions })).into_response()
}

/// Strict native permission-reply DTO (`POST /native/permission/reply`).
/// Resolution is CONTEXTUAL: the reply must name the session that owns the
/// permission, never only the id.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativePermissionReplyRequest {
    pub session_id: String,
    pub permission_id: String,
    pub decision: String,
}

/// `POST /native/permission/reply` — resolve ONE LIVE pending permission
/// request with `allow`/`deny`. `200 {ok:true}` means a live waiter owned by
/// the named session actually received the decision; an unknown/already
/// resolved/timed-out/expired id is a typed 409 (never a pre-authorized
/// decision planted for a later request), a live waiter owned by a DIFFERENT
/// session a typed 409 `permission_session_mismatch`, malformed bodies a
/// 400.
pub(crate) async fn native_permission_reply(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<NativePermissionReplyRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid native permission reply body")),
    };
    let session_id = match parse_session_id(&req.session_id) {
        Ok(id) => id,
        Err(e) => return wire_status(e),
    };
    let pid: i64 = match req.permission_id.parse() {
        Ok(p) if p > 0 => p,
        _ => {
            return wire_status(malformed_body(&format!(
                "invalid permission id {:?}",
                req.permission_id
            )));
        }
    };
    let decision = match req.decision.as_str() {
        "allow" => faktor_core::capability::PermissionDecision::Allow,
        "deny" => faktor_core::capability::PermissionDecision::Deny,
        other => return wire_status(malformed_body(&format!("invalid decision {other:?}"))),
    };
    match state.deps.permissions.resolve(session_id, pid, decision) {
        Ok(true) => {}
        Ok(false) => {
            return wire_status(ApiError {
                code: "conflict",
                message: format!("permission {pid} unknown or already resolved"),
                http_status: 409,
                retryable: false,
            });
        }
        Err(mismatch) => {
            return wire_status(ApiError {
                code: "permission_session_mismatch",
                message: mismatch.to_string(),
                http_status: 409,
                retryable: false,
            });
        }
    }
    Json(serde_json::json!({ "ok": true })).into_response()
}

// ------------------------------------------------------- session bootstrap

/// Strict native create-session DTO (`POST /native/session`).
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeCreateSessionRequest {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub workspace: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
}

/// `POST /native/session` — create one durable session on a workspace root
/// (the request's `workspace`, else the daemon's own directory). Strict
/// DTO: an unknown field is a 400. Provider/model bounds are enforced by
/// the session authority; a workspace that cannot be registered is a
/// typed error, never a half-created session.
pub(crate) async fn native_create_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<NativeCreateSessionRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid native create-session body")),
    };
    // Default provider/model settings apply at "new conversation" timing.
    // A host may send empty strings when the user has not made a per-session
    // choice; resolve the durable daemon defaults before validating, so the
    // settings screen is not a write-only decoration. An unset default still
    // refuses typed, exactly as before.
    let mut provider = req.provider.trim().to_string();
    let mut model = req.model.trim().to_string();
    if provider.is_empty() || model.is_empty() {
        if let Some(foundation) = state.deps.foundation.as_deref() {
            if let Ok(snapshot) = foundation.config.snapshot() {
                let value = |key: &str| {
                    snapshot
                        .values
                        .get(key)
                        .and_then(|entry| entry.value.as_str())
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(str::to_string)
                };
                if provider.is_empty() {
                    provider = value("faktor.defaultProvider").unwrap_or_default();
                }
                if model.is_empty() {
                    model = value("faktor.defaultModel").unwrap_or_default();
                }
            }
        }
    }
    if provider.is_empty() || model.is_empty() {
        return wire_status(malformed_body(
            "provider and model must not be empty (no durable default is set)",
        ));
    }
    // The registry is the authority: an unregistered provider refuses HERE,
    // before any workspace/session effect (the CLI's `run` preflight mirrors
    // this rule). Without it an unknown id created a durable workspace and a
    // permanently failed session.
    let registered = state.deps.agent.deps().providers.ids();
    if !registered.iter().any(|id| id == &provider) {
        return api_err(&faktor_core::Error::new(
            faktor_core::error::ErrorKind::NotFound,
            format!(
                "provider '{}' is not registered; registered: {:?}",
                provider, registered
            ),
        ));
    }
    // The session model must be SERVED. The router's priced candidate set is
    // the authority: Economy may substitute among served models, but an
    // unserved model must refuse typed instead of being silently replaced
    // (docs: "A session whose model the router cannot serve is refused with
    // a typed failure — never silently replaced").
    let served: Vec<String> = state
        .deps
        .agent
        .deps()
        .routing
        .served_models()
        .into_iter()
        .filter(|(registered_provider, _)| registered_provider == &provider)
        .map(|(_, model)| model)
        .collect();
    // Permissive when the policy exposes no candidate set (fixed/passthrough
    // test policies); production Economy validates against its priced set.
    if !served.is_empty() && !served.iter().any(|m| m == &model) {
        let mut listed = served.clone();
        listed.sort();
        listed.dedup();
        return api_err(&faktor_core::Error::new(
            faktor_core::error::ErrorKind::NotFound,
            format!(
                "session model '{}' is not served by provider '{}'; served: {:?}",
                model, provider, listed
            ),
        ));
    }
    let root = req
        .workspace
        .clone()
        .or_else(|| state.deps.directory.clone())
        .unwrap_or_else(|| ".".to_string());
    // The handler contract is "a workspace that cannot be registered is a
    // typed error, never a half-created session": a 256 KiB single-component
    // path used to create a durable session for a root that can never exist
    // (NAME_MAX), and a missing root registered silently.
    const MAX_WORKSPACE_ROOT_BYTES: usize = 4096;
    if root.len() > MAX_WORKSPACE_ROOT_BYTES {
        return wire_status(malformed_body(&format!(
            "workspace root exceeds {MAX_WORKSPACE_ROOT_BYTES} bytes"
        )));
    }
    if !std::path::Path::new(&root).is_dir() {
        return wire_status(malformed_body(&format!(
            "workspace root {root} does not exist or is not a directory"
        )));
    }
    let ws = match state.deps.session.create_workspace(&root) {
        Ok(ws) => ws,
        Err(e) => return api_err(&e),
    };
    let title = req.title.unwrap_or_else(|| "session".into());
    match state
        .deps
        .session
        .create_session(ws, &title, &provider, &model)
    {
        Ok(handle) => {
            // Bind the fresh session to the workspace's real owner worktree
            // before it can run. A fresh row carries the standalone default
            // worktree 1; when any other worktree row already exists the
            // first shadowed mutating run refused "no registered worktree
            // row". The adoption is idempotent.
            if let Err(e) = state.deps.session.ensure_owner_worktree(handle.id()) {
                return api_err(&e);
            }
            // SessionStart lifecycle hook (audit): the durable row exists
            // now, and the session has not been used yet — the SAME
            // ordering as the ACP daemon entry. Best-effort: the registry
            // bounds the hook (deadline/caps) and a fail-closed verdict is
            // audit-only, so session creation can never fail or hang
            // unboundedly on a hook.
            state.deps.agent.run_session_start_hook(handle.id());
            let row = handle.row().ok();
            Json(serde_json::json!({
                "id": handle.id().to_string(),
                "title": row.as_ref().map(|r| r.title.clone()).unwrap_or(title),
                "created_ms": row.map(|r| r.created_ms).unwrap_or(0),
            }))
            .into_response()
        }
        Err(e) => api_err(&e),
    }
}

/// Strict paging query for the durable History listing. `beforeRevision`
/// and `beforeId` must be supplied together; `limit` is bounded by
/// [`MAX_NATIVE_SESSION_LISTING`] and rejected (not silently clamped) above
/// it — paging is fundamental, not an afterthought.
///
/// `snapshotRevision` is the durable ordering bound of the walk (returned by
/// page 1, passed back unchanged). `snapshotMs`/`beforeMs` remain accepted
/// presentation metadata for older clients, but never bound the page: the
/// cursor is the monotonic `(listing_revision, id)` pair.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeSessionListQuery {
    #[serde(default, rename = "beforeRevision")]
    before_revision: Option<i64>,
    #[serde(default, rename = "beforeId")]
    before_id: Option<u64>,
    /// Legacy metadata of the previous row; echoed in `nextBefore` but not
    /// used for ordering (a same-millisecond write cannot move the cursor).
    #[serde(default, rename = "beforeMs")]
    before_ms: Option<i64>,
    /// Snapshot bound of the walk, returned by page 1 and passed back on
    /// every later page. Conversations updated mid-walk are excluded from
    /// the rest of that walk instead of jumping to page one / being skipped.
    #[serde(default, rename = "snapshotRevision")]
    snapshot_revision: Option<i64>,
    /// Wall-clock presentation metadata of the walk (first page).
    #[serde(default, rename = "snapshotMs")]
    snapshot_ms: Option<i64>,
    #[serde(default)]
    limit: Option<u64>,
}

/// `GET /native/sessions?beforeRevision=&beforeId=&snapshotRevision=&snapshotMs=&limit=`
/// — the durable session listing (newest listing revision first,
/// cursor-paged): `{sessions, hasMore, nextBefore, snapshotRevision,
/// snapshotMs}`. ONE bounded statement reads at most `limit + 1` rows, so a
/// 2,000-conversation History is reachable page by page without ever
/// materializing the whole table. Only the first page is served when the
/// client omits the cursor; `hasMore`/`nextBefore` are exact and drive the
/// next read. `snapshotMs`/`beforeMs` are presentation metadata only.
pub(crate) async fn native_list_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<NativeSessionListQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(query) => query,
        Err(_) => return wire_status(malformed_body("invalid query parameters (strict DTO)")),
    };
    let cursor = match (q.before_revision, q.before_id) {
        (None, None) => None,
        (Some(revision), Some(id)) if revision >= 0 => {
            Some((revision, i64::try_from(id).unwrap_or(i64::MAX)))
        }
        (Some(_), Some(_)) => return wire_status(malformed_body("beforeRevision must be >= 0")),
        _ => {
            return wire_status(malformed_body(
                "session cursor requires beforeRevision and beforeId together",
            ))
        }
    };
    if q.before_ms.is_some_and(|ms| ms < 0) {
        return wire_status(malformed_body("beforeMs must be >= 0"));
    }
    let limit = match q.limit {
        None => MAX_NATIVE_SESSION_LISTING,
        Some(0) => return wire_status(malformed_body("limit must be >= 1")),
        Some(value) if value > MAX_NATIVE_SESSION_LISTING as u64 => {
            return wire_status(malformed_body(&format!(
                "limit {value} exceeds the native page bound {MAX_NATIVE_SESSION_LISTING}"
            )))
        }
        Some(value) => value as usize,
    };
    let page_limit = u64::try_from(limit).unwrap_or(0);
    let store = state.deps.session.store();
    let (snapshot_revision, snapshot_ms) = match cursor {
        None => {
            let snapshot = match store.session_listing_snapshot() {
                Ok(snapshot) => snapshot,
                Err(e) => return api_err(&store_err_to_core(e)),
            };
            let snapshot_ms = q.snapshot_ms.unwrap_or(snapshot.wall_ms);
            if snapshot_ms < 0 {
                return wire_status(malformed_body("snapshotMs must be >= 0"));
            }
            let snapshot_revision = q.snapshot_revision.unwrap_or(snapshot.revision);
            if snapshot_revision < 0 {
                return wire_status(malformed_body("snapshotRevision must be >= 0"));
            }
            (snapshot_revision, snapshot_ms)
        }
        Some(_) => match (q.snapshot_revision, q.snapshot_ms) {
            (Some(revision), Some(ms)) if revision >= 0 && ms >= 0 => (revision, ms),
            (Some(revision), _) if revision < 0 => {
                return wire_status(malformed_body("snapshotRevision must be >= 0"))
            }
            (_, Some(ms)) if ms < 0 => {
                return wire_status(malformed_body("snapshotMs must be >= 0"))
            }
            _ => {
                return wire_status(malformed_body(
                    "a cursored session page requires snapshotRevision and snapshotMs from the first page",
                ))
            }
        },
    };
    let rows: Vec<faktor_store::SessionRow> =
        match store.list_sessions_page_at(None, cursor, page_limit, Some(snapshot_revision)) {
            Ok(rows) => rows,
            Err(e) => return api_err(&store_err_to_core(e)),
        };
    let has_more = rows.len() > limit;
    let rows = &rows[..rows.len().min(limit)];
    let next_before = if has_more {
        rows.last().map(|row| {
            serde_json::json!({
                // Presentation metadata retained for existing clients; the
                // cursor the next page is actually bound to is the revision.
                "beforeMs": row.updated_ms,
                "beforeRevision": row.listing_revision,
                "beforeId": row.id.raw(),
            })
        })
    } else {
        None
    };
    let sessions: Vec<serde_json::Value> = rows
        .iter()
        .map(|row| {
            serde_json::json!({
                "id": row.id.to_string(),
                "title": row.title,
                "provider": row.provider,
                "model": row.model,
                "state": agent_state_tag(row.state),
                // Durable creation time for History surfaces (additive;
                // the clients parse it tolerantly and never fabricate a
                // time).
                "created_ms": row.created_ms,
                // Presentation wall clock (never the paging key).
                "updated_ms": row.updated_ms,
                // The durable paging sort key / cursor component.
                "listing_revision": row.listing_revision,
            })
        })
        .collect();
    Json(serde_json::json!({
        "sessions": sessions,
        "hasMore": has_more,
        "nextBefore": next_before,
        "snapshotRevision": snapshot_revision,
        "snapshotMs": snapshot_ms,
    }))
    .into_response()
}

/// Hard cap of one native session listing.
pub(crate) const MAX_NATIVE_SESSION_LISTING: usize = 1000;

/// Strict native ordinary-prompt DTO (`POST /native/session/{id}/prompt`).
/// `submission_id` is the REQUIRED client submission UUID of this logical
/// prompt (1..=64 ASCII `[0-9a-f-]`, UUID-shaped — the SAME contract as the
/// task-start field). It is the durable idempotency key (finding 1): the
/// handler claims a `prompt_admission` row BEFORE the `PromptReceived`
/// journal append and any queue/message mutation, so a repeated key with
/// the same normalized body replays the original receipt byte-for-byte, a
/// pending key refuses in flight, and a different body under the same key
/// is a typed 409.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativePromptRequestBody {
    pub session_id: String,
    pub submission_id: String,
    pub prompt: String,
    #[serde(default)]
    pub files: Option<Vec<String>>,
}

/// The strict shape of the required prompt `submission_id`: the SAME
/// predicate as the native task-start field (non-empty, at most 64 ASCII
/// bytes of `[0-9a-f-]`). Anything else is a typed 400 at the wire
/// boundary, before any session resolution or admission claim.
fn validate_prompt_submission_id(id: &str) -> Result<(), ApiError> {
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

/// The canonical request digest of ONE ordinary prompt: the session id, the
/// prompt text and the attached file paths under a domain-separated BLAKE3
/// (the crate's canonical authority-digest construction). Computed
/// caller-side, before the admission claim enqueues anything.
fn prompt_admission_digest(
    session: faktor_core::id::SessionId,
    prompt: &str,
    files: &[String],
) -> String {
    let fields = faktor_core::authority::Fields::new()
        .text(&session.to_string())
        .text(prompt)
        .list(files);
    faktor_core::authority::authority_digest_hex(b"faktor.native-prompt-admission/v1", 1, fields)
}

/// Serialize one accepted prompt receipt into the exact response bytes the
/// admission row stores, so a replay is byte-for-byte the first success.
/// `pub(crate)`: the admission-recovery module rebuilds the same bytes from
/// the durable turn facts.
pub(crate) fn prompt_receipt_json(receipt: &PromptReceipt) -> String {
    serde_json::json!({
        "op_id": receipt.op_id.map(|op| op.to_string()),
        "run_id": receipt.run_id,
        "accepted": receipt.accepted,
        "queued": receipt.queued,
    })
    .to_string()
}

/// The raw-bytes 200 response of one prompt receipt (the stored JSON is
/// served verbatim; it was already validated as JSON at the admission
/// boundary).
fn prompt_receipt_response(receipt_json: &str) -> Response {
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        receipt_json.to_string(),
    )
        .into_response()
}

/// `true` for the orchestrator refusals that are PROVABLY before any
/// durable admission mutation: a claimed prompt key may be released so the
/// SAME submission can be retried. Ambiguous failures (internal, transport,
/// persistence, injected crash) keep the key pending: a retry then answers
/// in flight rather than ever duplicating the prompt.
fn prompt_refusal_is_pre_admission(e: &faktor_orchestrator::runtime::ExecError) -> bool {
    use faktor_orchestrator::runtime::ExecError as E;
    matches!(
        e,
        E::InvalidPlan(_)
            | E::Malformed(_)
            | E::Oversized(_)
            | E::AdmissionRefused { .. }
            | E::PlacementRefused(_)
    )
}

/// `POST /native/session/{id}/prompt` — run ONE ordinary prompt through
/// the daemon's ONE executor entry ([`PromptExecutionService`]); the body's
/// `session_id` must match the path id. The required `submission_id` is
/// claimed durably BEFORE any `PromptReceived` append or queue/message
/// mutation: a repeated key with the equal body replays the stored receipt
/// byte-for-byte with zero writes, a pending key is a typed 409 in-flight,
/// and a reused key with a different body is a typed 409 conflict. Returns
/// the durable receipt `{op_id, run_id, accepted, queued}`. Empty prompts
/// are a typed 400; unknown sessions 404.
pub(crate) async fn native_prompt(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<NativePromptRequestBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid native prompt body")),
    };
    let sid = match parse_session_id(&id) {
        Ok(s) => s,
        Err(e) => return wire_status(e),
    };
    let body_sid = match parse_session_id(&req.session_id) {
        Ok(s) => s,
        Err(e) => return wire_status(e),
    };
    if sid != body_sid {
        return wire_status(malformed_body(&format!(
            "path session id {sid} does not match body session id {body_sid}"
        )));
    }
    if let Err(e) = validate_prompt_submission_id(&req.submission_id) {
        return wire_status(e);
    }
    // The session must exist, but admission time comes from the manager's
    // MONOTONIC clock (never this handle's wall-clock reading).
    let _handle = match state.deps.session.get_session(sid) {
        Ok(Some(h)) => h,
        Ok(None) => return wire_status(not_found(&format!("session {sid}"))),
        Err(e) => return api_err(&e),
    };
    // The admission claim is the FIRST durable act of the prompt: it
    // precedes the `PromptReceived` append and every queue/message write.
    //
    // Audit P1: reserve the turn's op id durably BEFORE the claim so the
    // claim's `reservation` (`tx-<op>`) names exactly the turn a fresh
    // execution journals; startup recovery then rebuilds the receipt from
    // the durable turn/queue facts instead of guessing.
    let files = req.files.unwrap_or_default();
    if let Err(response) = super::task::write_tool_enablement(&state, &_handle, None) {
        return response;
    }
    let store = state.deps.session.store();
    let digest = prompt_admission_digest(sid, &req.prompt, &files);
    let reserved_op = match state.deps.session.try_next_op_id() {
        Ok(op) => op,
        Err(e) => return api_err(&faktor_core::Error::from(e)),
    };
    let reservation = format!("tx-{:016x}", reserved_op.raw());
    let mut claim = match store.prompt_admission_claim(
        sid,
        &req.submission_id,
        &digest,
        &reservation,
        // MONOTONIC admission time: wall-clock jumps never move the lease.
        state.deps.session.admission_now_ms(),
    ) {
        Ok(claim) => claim,
        Err(e) => return api_err(&store_err_to_core(e)),
    };
    if let PromptAdmissionClaim::Stale(row) = &claim {
        // A stale row (a previous boot's owner or an expired lease) is never
        // answered in flight: classify it against the durable facts, land it
        // exactly once, then retry the claim.
        if let Err(e) =
            super::admission_recovery::land_stale_prompt_admission(&state.deps.session, row)
        {
            return exec_error_response(&e);
        }
        claim = match store.prompt_admission_claim(
            sid,
            &req.submission_id,
            &digest,
            &reservation,
            state.deps.session.admission_now_ms(),
        ) {
            Ok(claim) => claim,
            Err(e) => return api_err(&store_err_to_core(e)),
        };
    }
    match claim {
        PromptAdmissionClaim::Complete(receipt_json) => {
            // The stored receipt is served byte-for-byte with ZERO journal,
            // queue or message writes; a hostile/corrupt injected row is
            // refused loudly instead of returned as a phantom receipt.
            if serde_json::from_str::<serde_json::Value>(&receipt_json).is_err() {
                return wire_status(ApiError {
                    code: "internal",
                    message: "stored prompt admission receipt is not valid JSON".into(),
                    http_status: 500,
                    retryable: false,
                });
            }
            return prompt_receipt_response(&receipt_json);
        }
        PromptAdmissionClaim::InFlight => {
            return wire_status(ApiError {
                code: "conflict",
                message: format!(
                    "prompt submission id {:?} is already in flight; retry once it settles",
                    req.submission_id
                ),
                http_status: 409,
                retryable: false,
            });
        }
        PromptAdmissionClaim::Stale(_) => {
            // A second stale landing raced this claim (rare, bounded): the
            // retryable refusal is honest, never a perpetual in-flight.
            return wire_status(ApiError {
                code: "conflict",
                message: format!(
                    "prompt submission id {:?} has a stale claim being resolved; retry",
                    req.submission_id
                ),
                http_status: 409,
                retryable: true,
            });
        }
        PromptAdmissionClaim::KeyReused { stored_digest } => {
            return wire_status(ApiError {
                code: "conflict",
                message: format!(
                    "prompt submission id {:?} was already used for a different prompt (stored request digest {stored_digest}); use a fresh submission id for a new prompt",
                    req.submission_id
                ),
                http_status: 409,
                retryable: false,
            });
        }
        PromptAdmissionClaim::Fresh => {}
    }
    let service = PromptExecutionService::from_state(&state);
    let request = PromptRequest {
        prompt: req.prompt,
        files,
        // The server-level admission IS this path's durable key: the
        // executor is called unkeyed so a prompt key can never alias a
        // task-start key (separate tables, separate semantics). The
        // reserved op id is threaded so the executor's turn journals exactly
        // the reservation this claim recorded.
        submission_id: None,
        reserved_op_id: Some(reserved_op),
        admission_digest: Some(digest),
        max_turns: super::task::session_turn_cap(&state, sid),
        ..Default::default()
    };
    match service.prompt(sid, request).await {
        Ok(receipt) => {
            let receipt_json = prompt_receipt_json(&receipt);
            if let Err(e) = store.prompt_admission_complete(
                sid,
                &req.submission_id,
                &reservation,
                &receipt_json,
            ) {
                tracing::error!(
                    session_id = %sid,
                    submission_id = %req.submission_id,
                    error = %e,
                    "prompt admission completion failed after the prompt was accepted; the key stays pending and startup recovery completes it from the durable turn facts"
                );
                return api_err(&store_err_to_core(e));
            }
            prompt_receipt_response(&receipt_json)
        }
        Err(e) => {
            // A provably pre-admission refusal releases the key so the SAME
            // submission may be retried; an ambiguous failure keeps it
            // pending (startup recovery then replays the accepted turn or
            // reclaims it — never a perpetual in-flight).
            if prompt_refusal_is_pre_admission(&e) {
                if let Err(release_err) =
                    store.prompt_admission_release(sid, &req.submission_id, &reservation)
                {
                    tracing::error!(
                        session_id = %sid,
                        submission_id = %req.submission_id,
                        error = %release_err,
                        "prompt admission release failed after a pre-admission refusal; startup recovery resolves the key"
                    );
                }
            }
            exec_error_response(&e)
        }
    }
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;
