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
        handle
            .checkpoints_of()?
            .into_iter()
            .max_by_key(|c| (c.sequence, c.id))
            .map(|c| {
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
        Ok(v) => Json(v).into_response(),
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
    // State derivation (documented): the live machine wins ("running");
    // open milestones or a fresh goal with no completed work yet are
    // "in_progress"; completed work with nothing left open is "done".
    let row = match handle.row() {
        Ok(r) => r,
        Err(e) => return api_err(&e),
    };
    let state_tag = if row.state.is_active() {
        "running"
    } else if !open.is_empty() || (completed.is_empty() && !goal.is_empty()) {
        "in_progress"
    } else if !completed.is_empty() {
        "done"
    } else {
        "idle"
    };
    // Additive progress + budget (audit P0-64): `progress` is the session's
    // live bounded progress record (null before the runtime tracked one);
    // `budget` is the DURABLE budget envelope of the session's typed task
    // row — token and monetary caps/spend from the task + cost-ledger
    // columns and the open (in-flight) reservation micro sum — null when no
    // typed task row exists yet (null-safe pre-first-reservation: a typed
    // row without any reservation reads openReservedMicro 0).
    let mut budget: Option<serde_json::Value> = None;
    if let Ok(Some(task)) = handle.get_task(row.task_id) {
        let cost = state
            .deps
            .session
            .store()
            .cost_task_row(handle.id(), row.task_id)
            .ok()
            .flatten();
        // In-flight reservations (schema v17 vocabulary: `reserved` —
        // dispatch never provably began — and `dispatched`, the request left
        // the process and may have billed) both hold their prediction; the
        // v15-era `open` state was renamed away at schema v17 and never
        // occurs in a migrated store.
        let open_micro = state
            .deps
            .session
            .store()
            .cost_reservations_of(handle.id(), row.task_id, MAX_NATIVE_RESERVATIONS_SCAN)
            .map(|rs| {
                rs.iter()
                    .filter(|r| r.status == "reserved" || r.status == "dispatched")
                    .fold(0u64, |acc, r| acc.saturating_add(r.predicted_micro))
            })
            .unwrap_or(0);
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
        "verification": native_verification_facts(&handle),
        "progress": progress,
        "budget": budget,
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
    match handle.checkpoints_of() {
        Ok(rows) => {
            let mut rows = rows;
            rows.sort_by_key(|c| std::cmp::Reverse((c.sequence, c.id)));
            let out: Vec<serde_json::Value> = rows
                .iter()
                .take(MAX_NATIVE_LIST)
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
    let rows = match store.messages_before(handle.id(), q.before, read_bound) {
        Ok(r) => r,
        Err(e) => return api_err(&store_err_to_core(e)),
    };
    let has_more = rows.len() as i64 > limit;
    let mut page_rows = rows;
    page_rows.truncate(limit as usize);
    let next_before = if has_more {
        page_rows.last().map(|r| r.seq)
    } else {
        None
    };
    let mut messages: Vec<serde_json::Value> = Vec::new();
    for row in page_rows {
        let parts: Vec<serde_json::Value> = match store.parts_of(row.id) {
            Ok(ps) => ps
                .iter()
                .map(|p| {
                    serde_json::json!({
                        "kind": p.kind,
                        "createdMs": p.created_ms,
                        "data": p.data,
                    })
                })
                .collect(),
            Err(_) => Vec::new(),
        };
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
    let events = match handle.events_range(after.saturating_add(1), Some(read_bound)) {
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
/// native SSE stream carry the exact same shape.
pub(crate) fn native_event_row(e: &faktor_core::event::Event) -> serde_json::Value {
    serde_json::json!({
        "seq": e.seq.raw(),
        "kind": serde_json::to_string(&e.kind)
            .unwrap_or_default()
            .trim_matches('"'),
        "state": agent_state_tag(e.state),
        "opId": e.op_id.map(|o| o.to_string()),
        "tsMs": e.ts_ms,
        "payload": e.payload,
    })
}

/// Strict native SSE query of `/native/session/{id}/events`.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeSessionEventsQuery {
    #[serde(default)]
    after: Option<u64>,
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
    let stream = native_journal_stream(handle, cursor);
    axum::response::sse::Sse::new(stream)
        .keep_alive(
            axum::response::sse::KeepAlive::new()
                .interval(std::time::Duration::from_secs(5))
                .text("keep-alive"),
        )
        .into_response()
}

/// The bounded, paged journal poll behind the native SSE stream: at most
/// [`MAX_NATIVE_EVENT_PAGE`] frames per poll are materialized; when the
/// page is exhausted the stream sleeps and emits a heartbeat. The journal
/// is the source of truth and the frame `id:` is the resume cursor. A
/// journal read failure is terminal: the stream emits one `error` frame
/// naming `journal_read_failed` and then ends — it never heartbeats over a
/// corrupt or unreadable authority.
fn native_journal_stream(
    handle: faktor_session::SessionHandle,
    cursor: i64,
) -> impl futures_util::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>
       + Send
       + 'static {
    futures_util::stream::unfold(
        (
            handle,
            cursor,
            std::collections::VecDeque::<axum::response::sse::Event>::new(),
            false,
        ),
        move |(handle, mut cursor, mut queue, terminated)| async move {
            if terminated {
                return None;
            }
            if let Some(frame) = queue.pop_front() {
                return Some((
                    Ok::<axum::response::sse::Event, std::convert::Infallible>(frame),
                    (handle, cursor, queue, terminated),
                ));
            }
            let events = match handle.events_range(
                cursor.saturating_add(1) as u64,
                Some(MAX_NATIVE_EVENT_PAGE as u64),
            ) {
                Ok(events) => events,
                Err(e) => {
                    tracing::error!(
                        session_id = %handle.id(),
                        error = %e,
                        "native journal read failed; terminating the SSE stream"
                    );
                    return Some((
                        Ok::<axum::response::sse::Event, std::convert::Infallible>(
                            axum::response::sse::Event::default()
                                .event("error")
                                .data(r#"{"code":"journal_read_failed"}"#),
                        ),
                        (handle, cursor, queue, true),
                    ));
                }
            };
            let mut batch = std::collections::VecDeque::new();
            let mut advanced = false;
            for e in events {
                let seq = e.seq.raw();
                let kind = serde_json::to_string(&e.kind)
                    .unwrap_or_default()
                    .trim_matches('"')
                    .to_string();
                let data =
                    serde_json::to_string(&native_event_row(&e)).unwrap_or_else(|_| "{}".into());
                batch.push_back(
                    axum::response::sse::Event::default()
                        .event(kind)
                        .id(seq.to_string())
                        .data(data),
                );
                cursor = e.seq.raw() as i64;
                advanced = true;
            }
            if advanced {
                if let Some(frame) = batch.pop_front() {
                    return Some((
                        Ok::<axum::response::sse::Event, std::convert::Infallible>(frame),
                        (handle, cursor, batch, terminated),
                    ));
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            Some((
                Ok::<axum::response::sse::Event, std::convert::Infallible>(
                    axum::response::sse::Event::default()
                        .event("heartbeat")
                        .data("{}"),
                ),
                (handle, cursor, queue, terminated),
            ))
        },
    )
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
        Some(raw) => match parse_session_id(raw) {
            Ok(id) => Some(id),
            Err(e) => return wire_status(e),
        },
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
            )))
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
            })
        }
        Err(mismatch) => {
            return wire_status(ApiError {
                code: "permission_session_mismatch",
                message: mismatch.to_string(),
                http_status: 409,
                retryable: false,
            })
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
    if req.provider.trim().is_empty() || req.model.trim().is_empty() {
        return wire_status(malformed_body("provider and model must not be empty"));
    }
    // The registry is the authority: an unregistered provider refuses HERE,
    // before any workspace/session effect (the CLI's `run` preflight mirrors
    // this rule). Without it an unknown id created a durable workspace and a
    // permanently failed session.
    let registered = state.deps.agent.deps().providers.ids();
    if !registered.iter().any(|id| id == &req.provider) {
        return api_err(&faktor_core::Error::new(
            faktor_core::error::ErrorKind::NotFound,
            format!(
                "provider '{}' is not registered; registered: {:?}",
                req.provider, registered
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
        .filter(|(provider, _)| provider == &req.provider)
        .map(|(_, model)| model)
        .collect();
    // Permissive when the policy exposes no candidate set (fixed/passthrough
    // test policies); production Economy validates against its priced set.
    if !served.is_empty() && !served.iter().any(|m| m == &req.model) {
        let mut listed = served.clone();
        listed.sort();
        listed.dedup();
        return api_err(&faktor_core::Error::new(
            faktor_core::error::ErrorKind::NotFound,
            format!(
                "session model '{}' is not served by provider '{}'; served: {:?}",
                req.model, req.provider, listed
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
        .create_session(ws, &title, &req.provider, &req.model)
    {
        Ok(handle) => {
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

/// `GET /native/sessions` — the durable session listing (newest first,
/// bounded): `{sessions: [{id, title, provider, model, state}]}`. The
/// listing is naturally bounded by the store's newest-first read; the
/// response caps at [`MAX_NATIVE_SESSION_LISTING`] entries.
pub(crate) async fn native_list_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let sessions = match state.deps.session.list_sessions(None) {
        Ok(s) => s,
        Err(e) => return api_err(&e),
    };
    let rows: Vec<serde_json::Value> = sessions
        .iter()
        .take(MAX_NATIVE_SESSION_LISTING)
        .map(|h| match h.row() {
            Ok(row) => serde_json::json!({
                "id": row.id.to_string(),
                "title": row.title,
                "provider": row.provider,
                "model": row.model,
                "state": agent_state_tag(row.state),
            }),
            Err(_) => serde_json::json!({
                "id": h.id().to_string(),
                "title": "",
                "provider": "",
                "model": "",
                "state": "unknown",
            }),
        })
        .collect();
    Json(serde_json::json!({ "sessions": rows })).into_response()
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
        "op_id": receipt.op_id.to_string(),
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
    let handle = match state.deps.session.get_session(sid) {
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
        handle.now_ms(),
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
            handle.now_ms(),
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
mod tests {
    use super::*;
    use std::sync::Arc;

    fn test_state(root: &std::path::Path, title: &str) -> (AppState, faktor_core::id::SessionId) {
        let deps = crate::api::tests::test_deps(root);
        let session = deps.session.clone();
        let state = AppState {
            deps: Arc::new(deps),
            auth: Arc::new(std::sync::RwLock::new(None)),
            terminal_events: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
            next_terminal_event_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let ws = session.create_workspace("/tmp").unwrap();
        let sid = session.create_session(ws, title, "fake", "m").unwrap().id();
        (state, sid)
    }

    fn abort_state(root: &std::path::Path) -> (AppState, faktor_core::id::SessionId) {
        test_state(root, "abort")
    }

    #[tokio::test]
    async fn create_session_refuses_an_unregistered_provider_before_any_effect() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = test_state(dir.path(), "seed");
        let before = state.deps.session.list_sessions(None).unwrap().len();
        let mut headers = authed_headers(&state);
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        let response = super::native_create_session(
            State(state.clone()),
            headers,
            Ok(Json(NativeCreateSessionRequest {
                provider: "ghost".into(),
                model: "m".into(),
                workspace: Some("/tmp".into()),
                title: None,
            })),
        )
        .await;
        let (status, body) = json_body(response).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], "not_found");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("not registered"),
            "{body}"
        );
        assert_eq!(
            state.deps.session.list_sessions(None).unwrap().len(),
            before,
            "an unregistered provider must leave no session behind"
        );
    }

    #[tokio::test]
    async fn create_session_accepts_a_registered_provider() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _) = test_state(dir.path(), "seed");
        let mut headers = authed_headers(&state);
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        let response = super::native_create_session(
            State(state.clone()),
            headers,
            Ok(Json(NativeCreateSessionRequest {
                provider: "fake".into(),
                model: "m".into(),
                workspace: Some("/tmp".into()),
                title: Some("accepted".into()),
            })),
        )
        .await;
        let (status, body) = json_body(response).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(!body["id"].as_str().unwrap_or_default().is_empty());
    }

    fn authed_headers(state: &AppState) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", state.deps.auth_token.as_str())
                .parse()
                .unwrap(),
        );
        headers
    }

    async fn json_body(response: Response) -> (StatusCode, serde_json::Value) {
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null),
        )
    }

    async fn abort_request(
        state: &AppState,
        sid: faktor_core::id::SessionId,
        op_id: Option<&str>,
    ) -> (StatusCode, serde_json::Value) {
        let response = native_session_abort(
            State(state.clone()),
            authed_headers(state),
            Path(sid.to_string()),
            Ok(Json(NativeAbortRequest {
                session_id: sid.to_string(),
                op_id: op_id.map(str::to_string),
            })),
        )
        .await;
        json_body(response).await
    }

    /// A hostile `op_id` that parses as a `u64` but violates the id contract
    /// (zero), or never parses (u64 overflow, negative), must be a typed 400
    /// naming the field — never `OpId::new(0)` panicking the request task.
    /// Valid and absent op ids still round-trip.
    #[tokio::test]
    async fn native_abort_op_id_validation_never_panics() {
        let dir = tempfile::tempdir().unwrap();
        let (state, sid) = abort_state(dir.path());

        // Valid round-trips: a real queued op id aborts exactly that row,
        // and the absent "abort everything" form answers OK.
        state
            .deps
            .session
            .store()
            .enqueue_prompt(
                sid,
                faktor_core::id::OpId::new(7),
                "q",
                &[],
                None,
                None,
                None,
                1,
            )
            .unwrap();
        let (status, body) = abort_request(&state, sid, Some("7")).await;
        assert_eq!(status, StatusCode::OK, "valid op_id: {body}");
        assert_eq!(body["aborted"], serde_json::json!(["7"]), "{body}");
        let (status, body) = abort_request(&state, sid, None).await;
        assert_eq!(status, StatusCode::OK, "absent op_id: {body}");

        for raw in ["0", "18446744073709551616", "-1", "-9223372036854775808"] {
            let (status, body) = abort_request(&state, sid, Some(raw)).await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "op_id {raw:?} must be a typed 400: {body}"
            );
            let message = body["error"]["message"].as_str().unwrap_or_default();
            assert!(
                message.contains("op_id"),
                "the refusal must name op_id for {raw:?}: {body}"
            );
            assert_eq!(body["error"]["code"], "malformed", "{body}");
        }

        // The session survives the hostile requests: a fresh valid abort
        // still answers OK afterwards.
        state
            .deps
            .session
            .store()
            .enqueue_prompt(
                sid,
                faktor_core::id::OpId::new(8),
                "q",
                &[],
                None,
                None,
                None,
                2,
            )
            .unwrap();
        let (status, body) = abort_request(&state, sid, Some("8")).await;
        assert_eq!(status, StatusCode::OK, "authority stays usable: {body}");
        assert_eq!(body["aborted"], serde_json::json!(["8"]), "{body}");
    }

    async fn events_request(
        state: &AppState,
        sid: faktor_core::id::SessionId,
        after: Option<u64>,
        limit: Option<u64>,
    ) -> (StatusCode, serde_json::Value) {
        let response = native_events(
            State(state.clone()),
            authed_headers(state),
            Ok(Query(NativeEventsQuery {
                session: sid.to_string(),
                after,
                limit,
            })),
        )
        .await;
        json_body(response).await
    }

    async fn messages_request(
        state: &AppState,
        sid: faktor_core::id::SessionId,
        before: Option<i64>,
        limit: Option<u64>,
    ) -> (StatusCode, serde_json::Value) {
        let response = native_messages(
            State(state.clone()),
            authed_headers(state),
            Ok(Query(NativeMessagesQuery {
                session: sid.to_string(),
                before,
                limit,
            })),
        )
        .await;
        json_body(response).await
    }

    async fn read_sse(
        response: Response,
        max_chunks: usize,
        timeout: std::time::Duration,
    ) -> (String, bool) {
        use futures_util::StreamExt as _;
        let mut body = response.into_body().into_data_stream();
        let mut out = String::new();
        let mut ended = false;
        for _ in 0..max_chunks {
            match tokio::time::timeout(timeout, body.next()).await {
                Ok(Some(Ok(chunk))) => out.push_str(&String::from_utf8_lossy(&chunk)),
                Ok(Some(Err(e))) => panic!("SSE body read failed: {e}"),
                Ok(None) => {
                    ended = true;
                    break;
                }
                Err(_) => break,
            }
        }
        (out, ended)
    }

    /// Finding 3 (P1): the bound is compared in the `u64` domain before any
    /// cast. `u64::MAX` used to wrap to -1 and slip past the comparison;
    /// every oversized value is now a typed 400 and every accepted value
    /// stays inside the cap.
    #[test]
    fn native_page_limit_matrix_never_bypasses_the_cap() {
        for max in [MAX_NATIVE_CURSOR_PAGE, MAX_NATIVE_EVENT_PAGE] {
            assert_eq!(page_limit(None, max).unwrap(), max);
            assert_eq!(page_limit(Some(1), max).unwrap(), 1);
            assert_eq!(page_limit(Some(max as u64), max).unwrap(), max);
            for l in [
                0u64,
                max as u64 + 1,
                i64::MAX as u64,
                i64::MAX as u64 + 1,
                u64::MAX,
            ] {
                let e = page_limit(Some(l), max)
                    .expect_err(&format!("limit {l} against bound {max} must be refused"));
                assert_eq!(e.http_status, 400, "limit {l}: {e:?}");
                assert_eq!(e.code, "malformed", "limit {l}: {e:?}");
            }
        }
    }

    /// Finding 3 (P1) over the wire: `/native/events` never accepts a
    /// wrapping limit. Every accepted value yields at most the 256-event cap
    /// against a 301-event journal; `i64::MAX`, `i64::MAX + 1` and
    /// `u64::MAX` are typed 400s, never an oversized or unbounded page.
    #[tokio::test]
    async fn native_events_limit_matrix_never_bypasses_the_page_cap() {
        let dir = tempfile::tempdir().unwrap();
        let (state, sid) = test_state(dir.path(), "events-limit");
        let handle = state.deps.session.get_session(sid).unwrap().unwrap();
        for _ in 0..300 {
            handle
                .force_append_event(
                    faktor_core::event::EventKind::PhaseChanged,
                    faktor_core::state::AgentState::WaitingForModel,
                    None,
                    None,
                )
                .unwrap();
        }
        let cap = MAX_NATIVE_EVENT_PAGE;
        for (limit, expected) in [(None, cap), (Some(1), 1), (Some(cap as u64), cap)] {
            let (status, body) = events_request(&state, sid, None, limit).await;
            assert_eq!(status, StatusCode::OK, "limit {limit:?}: {body}");
            let events = body["events"].as_array().unwrap();
            assert_eq!(events.len() as i64, expected, "limit {limit:?}: {body}");
            assert_eq!(body["hasMore"], serde_json::json!(true), "{body}");
            assert!(
                events.len() as i64 <= cap,
                "a bounded page can never exceed the cap: {body}"
            );
        }
        for limit in [
            Some(0u64),
            Some(cap as u64 + 1),
            Some(i64::MAX as u64),
            Some(i64::MAX as u64 + 1),
            Some(u64::MAX),
        ] {
            let (status, body) = events_request(&state, sid, None, limit).await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "limit {limit:?} must be a typed 400: {body}"
            );
            assert_eq!(
                body["error"]["code"], "malformed",
                "limit {limit:?}: {body}"
            );
            assert!(
                body.get("events").is_none(),
                "a refused limit must not carry a page: {body}"
            );
        }
    }

    /// Finding 3 (P1) over the wire: `/native/messages` is the same matrix
    /// against the 200-message cursor cap. A validated limit also keeps the
    /// `limit + 1` store read inside the integer domain.
    #[tokio::test]
    async fn native_messages_limit_matrix_never_bypasses_the_page_cap() {
        let dir = tempfile::tempdir().unwrap();
        let (state, sid) = test_state(dir.path(), "messages-limit");
        let handle = state.deps.session.get_session(sid).unwrap().unwrap();
        for seq in 1..=300i64 {
            handle
                .put_message(
                    seq,
                    "user",
                    serde_json::json!({ "text": format!("m{seq}") }),
                )
                .unwrap();
        }
        let cap = MAX_NATIVE_CURSOR_PAGE;
        for (limit, expected) in [(None, cap), (Some(1), 1), (Some(cap as u64), cap)] {
            let (status, body) = messages_request(&state, sid, None, limit).await;
            assert_eq!(status, StatusCode::OK, "limit {limit:?}: {body}");
            let messages = body["messages"].as_array().unwrap();
            assert_eq!(messages.len() as i64, expected, "limit {limit:?}: {body}");
            assert_eq!(body["hasMore"], serde_json::json!(true), "{body}");
            assert!(
                messages.len() as i64 <= cap,
                "a bounded page can never exceed the cap: {body}"
            );
        }
        for limit in [
            Some(0u64),
            Some(cap as u64 + 1),
            Some(i64::MAX as u64),
            Some(i64::MAX as u64 + 1),
            Some(u64::MAX),
        ] {
            let (status, body) = messages_request(&state, sid, None, limit).await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "limit {limit:?} must be a typed 400: {body}"
            );
            assert_eq!(
                body["error"]["code"], "malformed",
                "limit {limit:?}: {body}"
            );
            assert!(
                body.get("messages").is_none(),
                "a refused limit must not carry a page: {body}"
            );
        }
    }

    /// Finding 4: the happy path is unchanged — durable frames stream in
    /// journal order with their seq as the frame id, and an exhausted page
    /// heartbeats instead of terminating.
    #[tokio::test]
    async fn native_journal_sse_streams_frames_and_heartbeats() {
        let dir = tempfile::tempdir().unwrap();
        let (state, sid) = test_state(dir.path(), "sse-happy");
        let handle = state.deps.session.get_session(sid).unwrap().unwrap();
        handle
            .force_append_event(
                faktor_core::event::EventKind::PhaseChanged,
                faktor_core::state::AgentState::WaitingForModel,
                None,
                None,
            )
            .unwrap();
        let response = native_session_events(
            State(state.clone()),
            authed_headers(&state),
            Path(sid.to_string()),
            Ok(Query(NativeSessionEventsQuery { after: Some(1) })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let (frames, ended) = read_sse(response, 6, std::time::Duration::from_secs(5)).await;
        assert!(!ended, "a healthy journal stream stays open: {frames}");
        assert!(frames.contains("event: phase_changed"), "{frames}");
        assert!(frames.contains("id: 2"), "{frames}");
        assert!(frames.contains("event: heartbeat"), "{frames}");
    }

    /// Finding 4 (P1) hostile: a corrupt durable event must terminate the
    /// SSE stream with the typed error frame, never a heartbeat loop over an
    /// unreadable authority. The paged twin is loud on the same store.
    #[tokio::test]
    async fn native_journal_sse_terminates_with_an_error_frame_on_corrupt_journal() {
        let dir = tempfile::tempdir().unwrap();
        let (state, sid) = test_state(dir.path(), "sse-corrupt");
        let handle = state.deps.session.get_session(sid).unwrap().unwrap();
        handle
            .force_append_event(
                faktor_core::event::EventKind::PhaseChanged,
                faktor_core::state::AgentState::WaitingForModel,
                None,
                None,
            )
            .unwrap();
        state
            .deps
            .session
            .store()
            .sql_execute(&format!(
                "UPDATE event SET state = '\"not_a_state\"' WHERE session_id = {} AND seq = 2",
                sid.raw()
            ))
            .unwrap();
        let (status, body) = events_request(&state, sid, None, None).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
        assert_eq!(body["error"]["code"], "store_error", "{body}");

        let response = native_session_events(
            State(state.clone()),
            authed_headers(&state),
            Path(sid.to_string()),
            Ok(Query(NativeSessionEventsQuery { after: None })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let (frames, ended) = read_sse(response, 16, std::time::Duration::from_secs(5)).await;
        assert!(
            ended,
            "the stream must terminate on a journal read failure: {frames}"
        );
        assert!(frames.contains("event: error"), "{frames}");
        assert!(
            frames.contains(r#"{"code":"journal_read_failed"}"#),
            "{frames}"
        );
        assert!(
            !frames.contains("heartbeat"),
            "no heartbeat may follow a failed authority read: {frames}"
        );
    }

    // ------------------------------------------------ prompt admission (v27)

    const PROMPT_KEY: &str = "b0000000-0000-4000-8000-000000000001";

    /// Call the native prompt handler directly with one JSON body and
    /// return the status plus the RAW response text (byte-for-byte replay
    /// assertions need the exact body, not a re-serialized value).
    async fn prompt_call(
        state: &AppState,
        sid: faktor_core::id::SessionId,
        body: serde_json::Value,
    ) -> (StatusCode, String) {
        let req: NativePromptRequestBody =
            serde_json::from_value(body).expect("test prompt body parses");
        let response = native_prompt(
            State(state.clone()),
            authed_headers(state),
            Path(sid.to_string()),
            Ok(Json(req)),
        )
        .await;
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    fn prompt_body(sid: faktor_core::id::SessionId, key: &str, prompt: &str) -> serde_json::Value {
        serde_json::json!({
            "session_id": sid.to_string(),
            "submission_id": key,
            "prompt": prompt,
        })
    }

    fn prompt_received_count(handle: &faktor_session::SessionHandle) -> usize {
        handle
            .events_range(1, Some(10_000))
            .unwrap()
            .iter()
            .filter(|e| e.kind == faktor_core::event::EventKind::PromptReceived)
            .count()
    }

    /// The REQUIRED prompt submission id is validated with the SAME strict
    /// predicate as the task-start field: absent, empty, oversized,
    /// uppercase, non-hex and non-ASCII shapes are typed refusals, never a
    /// silently generated or dropped key.
    #[test]
    fn prompt_dto_requires_the_same_canonical_submission_id_shape() {
        let valid = serde_json::json!({
            "session_id": "1",
            "submission_id": "b0000000-0000-4000-8000-0000000000ff",
            "prompt": "hi",
        });
        let req: NativePromptRequestBody =
            serde_json::from_value(valid).expect("a canonical prompt body parses");
        assert_eq!(req.files, None, "absent files normalize to empty");
        // Missing is a typed parse error (never a silent fresh key).
        let err = match serde_json::from_value::<NativePromptRequestBody>(serde_json::json!({
            "session_id": "1", "prompt": "hi",
        })) {
            Ok(_) => panic!("missing submission_id must refuse"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("missing field"), "{err}");
        // A typo of the field name is a 400, never a dropped key.
        let err = match serde_json::from_value::<NativePromptRequestBody>(serde_json::json!({
            "session_id": "1",
            "submissionId": "b0000000-0000-4000-8000-0000000000ff",
            "prompt": "hi",
        })) {
            Ok(_) => panic!("a camelCase typo must refuse"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("unknown field"), "{err}");
        assert!(
            serde_json::from_value::<NativePromptRequestBody>(serde_json::json!({
                "session_id": "1", "submission_id": null, "prompt": "hi",
            }))
            .is_err()
        );
        for hostile in [
            "",
            " ",
            "b0000000-0000-4000-8000-00000000000Z",
            "b0000000-0000-4000-8000-00000000000_",
            "no-uuid-here",
            "☃",
        ] {
            let err = validate_prompt_submission_id(hostile)
                .expect_err("hostile shape must be a typed refusal");
            assert_eq!(err.http_status, 400, "{hostile:?}");
            assert_eq!(err.code, "malformed", "{hostile:?}");
        }
        let oversized =
            "a".repeat(faktor_orchestrator::runtime::task_executor::MAX_SUBMISSION_ID_BYTES + 1);
        assert!(validate_prompt_submission_id(&oversized).is_err());
        assert!(validate_prompt_submission_id(PROMPT_KEY).is_ok());
    }

    /// A lost response is retried with the SAME submission id and the SAME
    /// body: the replay returns the stored receipt BYTE-FOR-BYTE and creates
    /// no second prompt — exactly one `PromptReceived` row, no second user
    /// message, no queue row, no admission row beyond the one.
    #[tokio::test]
    async fn prompt_replay_returns_the_stored_receipt_byte_for_byte_with_one_prompt_row() {
        let dir = tempfile::tempdir().unwrap();
        let (state, sid) = test_state(dir.path(), "prompt-replay");
        let handle = state.deps.session.get_session(sid).unwrap().unwrap();
        let store = state.deps.session.store();
        let body = prompt_body(sid, PROMPT_KEY, "hi");

        let (status, first_text) = prompt_call(&state, sid, body.clone()).await;
        assert_eq!(status, StatusCode::OK, "{first_text}");
        let first: serde_json::Value = serde_json::from_str(&first_text).unwrap();
        assert!(!first["op_id"].as_str().unwrap().is_empty(), "{first}");
        assert_eq!(first["accepted"], serde_json::json!(true), "{first}");
        let messages_after_first = store.message_count(sid).unwrap();
        let events_after_first = prompt_received_count(&handle);
        let queued_after_first = handle.queued_prompt_count().unwrap();
        assert_eq!(events_after_first, 1, "exactly one prompt was received");

        // The lost-response retry: same key, same body.
        let (status, second_text) = prompt_call(&state, sid, body).await;
        assert_eq!(status, StatusCode::OK, "{second_text}");
        assert_eq!(
            first_text, second_text,
            "the retry must replay the FIRST response byte-for-byte"
        );
        assert_eq!(
            store.message_count(sid).unwrap(),
            messages_after_first,
            "the replay must create no second message row"
        );
        assert_eq!(
            prompt_received_count(&handle),
            events_after_first,
            "the replay must append no second PromptReceived event"
        );
        assert_eq!(
            handle.queued_prompt_count().unwrap(),
            queued_after_first,
            "the replay must not enqueue a second prompt"
        );
        // The key carries exactly ONE completed admission storing the exact
        // response bytes (a claim for any other row would answer Fresh/
        // KeyReused, and the byte compare pins the stored receipt).
        let digest = prompt_admission_digest(sid, "hi", &[]);
        assert_eq!(
            store
                .prompt_admission_claim(sid, PROMPT_KEY, &digest, "tx-replay-probe", 0)
                .unwrap(),
            PromptAdmissionClaim::Complete(first_text.clone()),
            "exactly one completed admission storing the response bytes"
        );
    }

    /// The same key with a DIFFERENT body is a typed 409 conflict with zero
    /// mutation: no new prompt event, message or queue row.
    #[tokio::test]
    async fn prompt_same_key_different_body_is_a_typed_conflict_with_no_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let (state, sid) = test_state(dir.path(), "prompt-key-reused");
        let handle = state.deps.session.get_session(sid).unwrap().unwrap();
        let store = state.deps.session.store();

        let (status, text) = prompt_call(&state, sid, prompt_body(sid, PROMPT_KEY, "hi")).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let messages = store.message_count(sid).unwrap();
        let events = prompt_received_count(&handle);

        let (status, text) =
            prompt_call(&state, sid, prompt_body(sid, PROMPT_KEY, "something else")).await;
        assert_eq!(status, StatusCode::CONFLICT, "{text}");
        let body: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(body["error"]["code"], "conflict", "{body}");
        assert_eq!(
            store.message_count(sid).unwrap(),
            messages,
            "the refusal must create no message row"
        );
        assert_eq!(
            prompt_received_count(&handle),
            events,
            "the refusal must append no prompt event"
        );
    }

    /// A key whose claim is still pending is an in-flight 409 BEFORE any
    /// `PromptReceived` append or queue/message write: the claim is the
    /// first durable act of the prompt path.
    #[tokio::test]
    async fn prompt_pending_key_is_an_in_flight_409_before_any_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let (state, sid) = test_state(dir.path(), "prompt-in-flight");
        let handle = state.deps.session.get_session(sid).unwrap().unwrap();
        let store = state.deps.session.store();
        assert_eq!(
            store
                .prompt_admission_claim(
                    sid,
                    PROMPT_KEY,
                    "digest-from-another-writer",
                    "tx-00000000000000bb",
                    handle.now_ms()
                )
                .unwrap(),
            PromptAdmissionClaim::Fresh,
            "the test plants the pending claim an in-flight winner would hold"
        );
        let (status, text) = prompt_call(&state, sid, prompt_body(sid, PROMPT_KEY, "hi")).await;
        assert_eq!(status, StatusCode::CONFLICT, "{text}");
        let body: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(body["error"]["code"], "conflict", "{body}");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("in flight"),
            "{body}"
        );
        assert_eq!(
            prompt_received_count(&handle),
            0,
            "an in-flight refusal must append no prompt event"
        );
        assert_eq!(store.message_count(sid).unwrap(), 0, "no message row");
        assert_eq!(handle.queued_prompt_count().unwrap(), 0, "no queue row");
    }

    /// A known pre-admission refusal (an empty prompt) releases the claimed
    /// key so the SAME submission may be retried; the refusal itself leaves
    /// no prompt event, message or queue row, and the retry admits fresh.
    #[tokio::test]
    async fn prompt_release_then_retry_admits_and_the_refusal_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (state, sid) = test_state(dir.path(), "prompt-release-retry");
        let handle = state.deps.session.get_session(sid).unwrap().unwrap();
        let store = state.deps.session.store();

        let (status, text) = prompt_call(&state, sid, prompt_body(sid, PROMPT_KEY, "   ")).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
        assert_eq!(
            prompt_received_count(&handle),
            0,
            "the pre-admission refusal must append no prompt event"
        );
        assert_eq!(store.message_count(sid).unwrap(), 0, "no message row");
        assert_eq!(handle.queued_prompt_count().unwrap(), 0, "no queue row");

        let (status, text) = prompt_call(&state, sid, prompt_body(sid, PROMPT_KEY, "hi")).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "the released key must admit the retry: {text}"
        );
        assert_eq!(
            prompt_received_count(&handle),
            1,
            "the retry admits exactly one prompt"
        );

        // A reuse of the now-completed key with a different body still
        // refuses typed: accepting the retry did not drop the receipt.
        let (status, text) = prompt_call(&state, sid, prompt_body(sid, PROMPT_KEY, "other")).await;
        assert_eq!(status, StatusCode::CONFLICT, "{text}");
    }

    // --------------------------------------- admission crash matrix (P1)

    /// Reopen the SAME data root as a new daemon boot generation without
    /// creating a second workspace/session (the crash matrix retries the
    /// original session id).
    fn reopen_state(root: &std::path::Path) -> AppState {
        let deps = crate::api::tests::test_deps(root);
        AppState {
            deps: Arc::new(deps),
            auth: Arc::new(std::sync::RwLock::new(None)),
            terminal_events: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
            next_terminal_event_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    fn user_message_count(handle: &faktor_session::SessionHandle) -> usize {
        handle
            .messages_page(None, 100)
            .unwrap()
            .messages
            .iter()
            .filter(|m| m.role == "user")
            .count()
    }

    /// Audit P1 crash matrix, shared harness: arm ONE admission boundary,
    /// drive the native prompt handler into the injected crash (the store
    /// writer dies and the handler answers typed), drop the "daemon",
    /// reopen the SAME data root as a NEW boot generation, run boot
    /// admission recovery and assert the pending table is EMPTY.
    async fn prompt_crash_and_reopen(
        dir: &tempfile::TempDir,
        point: &'static str,
        ordinal: u64,
    ) -> (AppState, faktor_core::id::SessionId, serde_json::Value) {
        let (state, sid) = test_state(dir.path(), "prompt-crash");
        let body = prompt_body(sid, PROMPT_KEY, "crash matrix prompt");
        state
            .deps
            .session
            .store()
            .crash_arm(faktor_store::CrashArm { point, ordinal });
        let (status, text) = prompt_call(&state, sid, body.clone()).await;
        assert!(
            status.is_server_error(),
            "seam {point}#{ordinal} must interrupt the prompt: {status} {text}"
        );
        assert!(
            !state.deps.session.store().writer_available(),
            "seam {point}#{ordinal} must kill the durable writer"
        );
        drop(state);
        let state = reopen_state(dir.path());
        let summary = crate::native::recover_pending_admissions(&state.deps.session);
        assert_eq!(summary.failed, 0, "{summary:?}");
        let pending = state
            .deps
            .session
            .store()
            .prompt_admission_pending_page(None, 100)
            .unwrap();
        assert!(
            pending.rows.is_empty(),
            "boot recovery never leaves a pending claim: {summary:?}"
        );
        (state, sid, body)
    }

    /// Boundary 1: the claim never committed. No row exists; the retry
    /// admits exactly one prompt.
    #[tokio::test]
    async fn prompt_crash_before_claim_retries_as_one_fresh_admission() {
        let dir = tempfile::tempdir().unwrap();
        let (state, sid, body) =
            prompt_crash_and_reopen(&dir, "prompt_admission_claim_precommit", 0).await;
        let (status, text) = prompt_call(&state, sid, body).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let handle = state.deps.session.get_session(sid).unwrap().unwrap();
        assert_eq!(prompt_received_count(&handle), 1);
        assert_eq!(user_message_count(&handle), 1);
    }

    /// Boundary 2: the claim committed, no mutation followed. Boot recovery
    /// reclaims it; the retry admits exactly one prompt.
    #[tokio::test]
    async fn prompt_crash_after_claim_before_mutation_reclaims_and_retries_once() {
        let dir = tempfile::tempdir().unwrap();
        let (state, sid, body) =
            prompt_crash_and_reopen(&dir, "prompt_admission_claim_committed", 0).await;
        let (status, text) = prompt_call(&state, sid, body).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let handle = state.deps.session.get_session(sid).unwrap().unwrap();
        assert_eq!(prompt_received_count(&handle), 1);
        assert_eq!(user_message_count(&handle), 1);
    }

    /// Boundary 3: the first durable mutation (the `PromptReceived` append)
    /// committed before the crash. A bare journal entry is not an accepted
    /// prompt (no message, no turn record): recovery reclaims it, repairs
    /// the phantom active state and the retry admits exactly one prompt.
    #[tokio::test]
    async fn prompt_crash_after_first_durable_mutation_reclaims_and_retries_once() {
        let dir = tempfile::tempdir().unwrap();
        let (state, sid, body) = prompt_crash_and_reopen(&dir, "ev_committed", 0).await;
        let (status, text) = prompt_call(&state, sid, body).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let handle = state.deps.session.get_session(sid).unwrap().unwrap();
        assert_eq!(
            user_message_count(&handle),
            1,
            "exactly one user prompt was materialized"
        );
    }

    /// Boundary 4: the accepted prompt exists (turn record durable) but the
    /// run linkage and the receipt are not. Recovery rebuilds the receipt
    /// from the facts and the retry replays it byte-for-byte.
    #[tokio::test]
    async fn prompt_crash_after_accepted_prompt_before_receipt_replays_from_facts() {
        let dir = tempfile::tempdir().unwrap();
        let (state, sid) = test_state(dir.path(), "prompt-crash-accepted");
        let handle = state.deps.session.get_session(sid).unwrap().unwrap();
        let prompt = "crash matrix prompt";
        let digest = prompt_admission_digest(sid, prompt, &[]);
        let op = state.deps.session.try_next_op_id().unwrap();
        let reservation = format!("tx-{:016x}", op.raw());
        assert!(state
            .deps
            .session
            .store()
            .prompt_admission_claim(sid, PROMPT_KEY, &digest, &reservation, handle.now_ms())
            .unwrap()
            .is_fresh());
        let submitted = handle
            .submit_prompt_with_op_id(prompt, &[], Some(op))
            .unwrap();
        let expected = prompt_receipt_json(&PromptReceipt {
            run_id: reservation,
            op_id: op,
            queued: submitted.queued,
            accepted: true,
        });
        drop(state);

        let state = reopen_state(dir.path());
        let summary = crate::native::recover_pending_admissions(&state.deps.session);
        assert_eq!(summary.replayed, 1, "{summary:?}");
        let (status, text) = prompt_call(&state, sid, prompt_body(sid, PROMPT_KEY, prompt)).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert_eq!(
            text, expected,
            "the receipt is rebuilt byte-for-byte from the durable turn facts"
        );
        let handle = state.deps.session.get_session(sid).unwrap().unwrap();
        assert_eq!(user_message_count(&handle), 1, "no second prompt row");
        assert_eq!(prompt_received_count(&handle), 1);
    }

    /// Boundary 5: full durable acceptance (prompt, turn, run linkage) before
    /// the receipt completion transaction. Boot recovery completes the receipt
    /// from the facts; the retry replays it and admits no second prompt.
    #[tokio::test]
    async fn prompt_crash_after_full_acceptance_before_receipt_completes_from_facts() {
        let dir = tempfile::tempdir().unwrap();
        let (state, sid, body) =
            prompt_crash_and_reopen(&dir, "prompt_admission_complete_precommit", 0).await;
        let (status, text) = prompt_call(&state, sid, body).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        let handle = state.deps.session.get_session(sid).unwrap().unwrap();
        assert_eq!(prompt_received_count(&handle), 1);
        assert_eq!(user_message_count(&handle), 1);
        let receipt: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(receipt["accepted"], serde_json::json!(true), "{text}");
        let run_id = receipt["run_id"].as_str().expect("run id").to_string();
        assert_eq!(
            run_id,
            format!(
                "tx-{:016x}",
                receipt["op_id"]
                    .as_str()
                    .expect("op id")
                    .parse::<u64>()
                    .unwrap()
            ),
            "the recovered run id names the reserved turn: {text}"
        );
    }

    /// Boundary 6: the receipt completed but the owner died before the HTTP
    /// response. The retry replays the EXACT stored bytes with zero writes.
    #[tokio::test]
    async fn prompt_crash_after_receipt_completion_replays_stored_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let (state, sid, body) =
            prompt_crash_and_reopen(&dir, "prompt_admission_complete_committed", 0).await;
        // The durable receipt is the answer the retry must return exactly.
        let digest = prompt_admission_digest(sid, "crash matrix prompt", &[]);
        let stored = match state
            .deps
            .session
            .store()
            .prompt_admission_claim(sid, PROMPT_KEY, &digest, "tx-probe", 0)
        {
            Ok(PromptAdmissionClaim::Complete(receipt)) => receipt,
            other => panic!("the completed row must replay: {other:?}"),
        };
        let (status, text) = prompt_call(&state, sid, body).await;
        assert_eq!(status, StatusCode::OK, "{text}");
        assert_eq!(
            text, stored,
            "the replay is the stored receipt byte-for-byte"
        );
        let handle = state.deps.session.get_session(sid).unwrap().unwrap();
        assert_eq!(prompt_received_count(&handle), 1);
        assert_eq!(user_message_count(&handle), 1);
    }

    /// An unlinkable pending prompt row (no recognizable reservation) is
    /// never guessed at: recovery lands the typed conflict and the retry
    /// never executes.
    #[tokio::test]
    async fn prompt_unlinkable_pending_row_lands_typed_conflict_and_never_executes() {
        let dir = tempfile::tempdir().unwrap();
        let (state, sid) = test_state(dir.path(), "prompt-legacy");
        let handle = state.deps.session.get_session(sid).unwrap().unwrap();
        let digest = prompt_admission_digest(sid, "crash matrix prompt", &[]);
        assert!(state
            .deps
            .session
            .store()
            .prompt_admission_claim(
                sid,
                PROMPT_KEY,
                &digest,
                "not-a-reservation",
                handle.now_ms()
            )
            .unwrap()
            .is_fresh());
        drop(state);
        let state = reopen_state(dir.path());
        let summary = crate::native::recover_pending_admissions(&state.deps.session);
        assert_eq!(summary.conflicts, 1, "{summary:?}");
        let (status, text) = prompt_call(
            &state,
            sid,
            prompt_body(sid, PROMPT_KEY, "crash matrix prompt"),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{text}");
        assert!(!text.contains("in flight"), "{text}");
        let handle = state.deps.session.get_session(sid).unwrap().unwrap();
        assert_eq!(prompt_received_count(&handle), 0);
        assert_eq!(user_message_count(&handle), 0);
    }

    // ------------------------------------------- SessionStart lifecycle hook

    /// One test AppState over deps whose agent carries an explicit hook
    /// registry (the SAME `AgentDeps.hooks` field the production daemon's
    /// `FAKTOR_HOOKS` registry occupies).
    #[cfg(unix)]
    fn hooked_state(
        root: &std::path::Path,
        specs: Vec<faktor_hooks::HookSpec>,
    ) -> (AppState, Arc<faktor_hooks::HookRegistry>) {
        let hooks = Arc::new(
            faktor_hooks::HookRegistry::try_new().expect("standalone hook registry for tests"),
        );
        for spec in specs {
            hooks.register(spec).unwrap();
        }
        let deps = crate::api::tests::test_deps_with_hooks(root, vec![], Some(hooks.clone()));
        let state = AppState {
            deps: Arc::new(deps),
            auth: Arc::new(std::sync::RwLock::new(None)),
            terminal_events: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
            next_terminal_event_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        (state, hooks)
    }

    /// The exact spec shape `FAKTOR_HOOKS=session_start:<script>` produces.
    #[cfg(unix)]
    fn session_start_spec(
        id: &str,
        script: String,
        deadline_ms: u64,
        failure_policy: faktor_hooks::FailurePolicy,
    ) -> faktor_hooks::HookSpec {
        faktor_hooks::HookSpec {
            id: id.into(),
            events: vec![faktor_hooks::HookEvent::SessionStart],
            command: "sh".into(),
            args: vec!["-c".into(), script],
            env_allowlist: true,
            deadline_ms,
            failure_policy,
            ..Default::default()
        }
    }

    #[cfg(unix)]
    async fn create_session_call(state: &AppState, title: &str) -> (StatusCode, serde_json::Value) {
        let mut headers = authed_headers(state);
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        let response = super::native_create_session(
            State(state.clone()),
            headers,
            Ok(Json(NativeCreateSessionRequest {
                provider: "fake".into(),
                model: "m".into(),
                workspace: Some("/tmp".into()),
                title: Some(title.into()),
            })),
        )
        .await;
        json_body(response).await
    }

    /// The native `POST /native/session` path fires SessionStart through the
    /// agent's hook registry exactly once, after the durable session exists,
    /// with the created session id — the SAME ordering as the ACP daemon
    /// entry.
    #[cfg(unix)]
    #[tokio::test]
    async fn native_create_session_fires_session_start_exactly_once() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("native-session-start.log");
        let script = format!("echo \"$FAKTOR_HOOK_INPUT\" >> '{}'", marker.display());
        let (state, hooks) = hooked_state(
            dir.path(),
            vec![session_start_spec(
                "env-0",
                script,
                5000,
                faktor_hooks::FailurePolicy::FailClosed,
            )],
        );
        let (status, body) = create_session_call(&state, "hooked").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let sid = body["id"].as_str().expect("session id").to_string();
        assert!(
            state
                .deps
                .session
                .get_session(faktor_core::id::SessionId::new(sid.parse().unwrap()))
                .unwrap()
                .is_some(),
            "the hook fired after the durable row existed"
        );
        let log = std::fs::read_to_string(&marker).unwrap();
        let lines: Vec<&str> = log.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 1, "the hook runs exactly once: {log}");
        assert!(
            lines[0].contains(&format!("\"session_id\":\"{sid}\"")),
            "the payload names the created session: {log}"
        );
        assert!(lines[0].contains("\"event\":\"session_start\""), "{log}");
        let audit = hooks.audit();
        assert_eq!(audit.len(), 1, "exactly one audit record");
        assert_eq!(audit[0].hook_id, "env-0");
        assert_eq!(audit[0].event, faktor_hooks::HookEvent::SessionStart);
        assert_eq!(audit[0].exit_code, Some(0));
    }

    /// A failing or hanging SessionStart hook is bounded by its registry
    /// deadline and can NEVER fail (or unboundedly stall) session creation:
    /// the route answers 200 and the durable row exists in both cases.
    #[cfg(unix)]
    #[tokio::test]
    async fn native_create_session_survives_a_failing_or_hanging_hook() {
        for (id, script, deadline_ms) in [
            ("fail", "exit 3".to_string(), 2000u64),
            ("hang", "sleep 30".to_string(), 250u64),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (state, hooks) = hooked_state(
                dir.path(),
                vec![session_start_spec(
                    id,
                    script,
                    deadline_ms,
                    faktor_hooks::FailurePolicy::FailClosed,
                )],
            );
            let t0 = std::time::Instant::now();
            let (status, body) = create_session_call(&state, id).await;
            assert_eq!(status, StatusCode::OK, "{id}: {body}");
            assert!(
                t0.elapsed() < std::time::Duration::from_secs(8),
                "{id}: the hook deadline must bound session creation"
            );
            let sid = body["id"].as_str().expect("session id").parse().unwrap();
            assert!(
                state
                    .deps
                    .session
                    .get_session(faktor_core::id::SessionId::new(sid))
                    .unwrap()
                    .is_some(),
                "{id}: the durable session exists"
            );
            let audit = hooks.audit();
            assert_eq!(audit.len(), 1, "{id}: exactly one audit record");
            assert_eq!(
                audit[0].verdict, "deny",
                "{id}: a fail-closed hook outcome is audit-only"
            );
        }
    }
}
