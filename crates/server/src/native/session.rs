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
    // Durable read failures are LOUD: a store error must never project as
    // "no budget"/"$0 reserved" on the money surface (error is not absence).
    let task_row = match handle.get_task(row.task_id) {
        Ok(t) => t,
        Err(e) => return api_err(&e),
    };
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
                    let events = match handle.events_range(
                        read_after.saturating_add(1) as u64,
                        Some(MAX_NATIVE_EVENT_PAGE as u64),
                    ) {
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
    // ONE statement read of the durable listing per page: the historical
    // shape ran the manager's listing (a whole-table read plus a journal
    // verification per session) and then this batch join — the session
    // table was read twice per page. The single listing now feeds the
    // served page directly, in the same newest-first order the historical
    // handle listing used (the manager listing and this read execute the
    // same store listing), so the served DTO is byte-identical; an
    // unreadable listing is a typed error, never a partial page.
    let rows: Vec<faktor_store::SessionRow> = match state.deps.session.store().list_sessions(None) {
        Ok(rows) => rows,
        Err(e) => return api_err(&store_err_to_core(e)),
    };
    let rows: Vec<serde_json::Value> = rows
        .into_iter()
        .take(MAX_NATIVE_SESSION_LISTING)
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
            })
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

    /// Perf-wave follow-up (residual): the listing route reads the session
    /// table exactly ONCE per page. The historical shape ran the manager's
    /// listing (whole-table read plus a journal verification per session)
    /// and then a batch join — two full reads per page. The single listing
    /// now feeds the served page directly; the DTO stays byte-identical to
    /// the historical per-handle projection, and the handler performs
    /// exactly one store read (one listing statement) at 5k sessions.
    #[tokio::test]
    async fn session_listing_reads_the_table_once_per_page() {
        let dir = tempfile::tempdir().unwrap();
        let (state, _seed) = test_state(dir.path(), "seed");
        let session = state.deps.session.clone();
        let ws = session.create_workspace("/tmp").unwrap();
        const EXTRA_SESSIONS: usize = 5000;
        let started = std::time::Instant::now();
        for i in 0..EXTRA_SESSIONS {
            session
                .create_session(ws, &format!("batch {i}"), "fake", "m")
                .unwrap();
        }
        let seed_elapsed = started.elapsed();

        // The historical projection, replayed verbatim from the manager
        // listing order: take(cap) then one get_session read per listed
        // handle. This is the golden the single-read DTO must reproduce
        // byte for byte.
        let started = std::time::Instant::now();
        let handles = session.list_sessions(None).unwrap();
        let historical: Vec<serde_json::Value> = handles
            .iter()
            .take(MAX_NATIVE_SESSION_LISTING)
            .map(|h| match h.row() {
                Ok(row) => serde_json::json!({
                    "id": row.id.to_string(),
                    "title": row.title,
                    "provider": row.provider,
                    "model": row.model,
                    "state": agent_state_tag(row.state),
                    "created_ms": row.created_ms,
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
        let historical_elapsed = started.elapsed();
        assert!(handles.len() > EXTRA_SESSIONS);

        // ONE store listing is all the served page needs (the same query
        // the manager listing runs, so the rows and their order are
        // identical).
        let started = std::time::Instant::now();
        let listing = session.store().list_sessions(None).unwrap();
        let single: Vec<serde_json::Value> = listing
            .into_iter()
            .take(MAX_NATIVE_SESSION_LISTING)
            .map(|row| {
                serde_json::json!({
                    "id": row.id.to_string(),
                    "title": row.title,
                    "provider": row.provider,
                    "model": row.model,
                    "state": agent_state_tag(row.state),
                    "created_ms": row.created_ms,
                })
            })
            .collect();
        let single_projection = started.elapsed();
        assert_eq!(
            single, historical,
            "the single-read projection answers byte-identically to the per-handle read"
        );

        let headers = authed_headers(&state);
        let reads_before = session.store().reader_telemetry().reads;
        let response = super::native_list_sessions(State(state.clone()), headers).await;
        let reads_after = session.store().reader_telemetry().reads;
        let (status, body) = json_body(response).await;
        assert_eq!(status, StatusCode::OK);
        let rows = body["sessions"].as_array().expect("sessions array");
        assert_eq!(
            rows, &historical,
            "the served handler answers byte-identically to the per-handle read"
        );
        assert_eq!(
            reads_after - reads_before,
            1,
            "the page must read the session table exactly once (one listing statement)"
        );
        eprintln!(
            "session listing: {} sessions (seed {seed_elapsed:?}, manager list+verify {historical_elapsed:?}): single-read projection {single_projection:?}; handler reads delta 1",
            handles.len()
        );
    }

    /// The History surfaces need a durable time: the listing serves the
    /// session row's `created_ms` (additive DTO field; clients parse it
    /// tolerantly and never fabricate one).
    #[tokio::test]
    async fn session_listing_serves_the_durable_creation_time() {
        let dir = tempfile::tempdir().unwrap();
        let (state, sid) = test_state(dir.path(), "history row");
        let headers = authed_headers(&state);
        let response = super::native_list_sessions(State(state.clone()), headers).await;
        let (status, body) = json_body(response).await;
        assert_eq!(status, StatusCode::OK);
        let rows = body["sessions"].as_array().expect("sessions array");
        let row = rows
            .iter()
            .find(|r| r["id"].as_str() == Some(&sid.to_string()))
            .expect("the created session is listed");
        let served = row["created_ms"].as_i64().expect("created_ms is served");
        let durable = state
            .deps
            .session
            .get_session(sid)
            .unwrap()
            .unwrap()
            .row()
            .unwrap()
            .created_ms;
        assert_eq!(
            served, durable,
            "the listing serves the DURABLE creation time"
        );
        assert_eq!(row["title"], "history row");
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
        let (frames, ended) = read_sse(response, 2, std::time::Duration::from_secs(5)).await;
        assert!(!ended, "a healthy journal stream stays open: {frames}");
        assert!(frames.contains("event: phase_changed"), "{frames}");
        assert!(frames.contains("id: 2"), "{frames}");
        assert!(frames.contains("event: heartbeat"), "{frames}");
    }

    /// Graceful daemon shutdown must END a long-lived stream on the next
    /// bounded tick. The readiness flag is the signal: a client that keeps its
    /// body open can never hold the listener past the 250 ms poll bound.
    #[tokio::test]
    async fn native_journal_sse_ends_when_readiness_turns_off() {
        let dir = tempfile::tempdir().unwrap();
        let (state, sid) = test_state(dir.path(), "sse-shutdown");
        let response = native_session_events(
            State(state.clone()),
            authed_headers(&state),
            Path(sid.to_string()),
            Ok(Query(NativeSessionEventsQuery { after: Some(0) })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        state
            .ready
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let (frames, ended) = read_sse(response, 2, std::time::Duration::from_secs(2)).await;
        assert!(
            ended,
            "a shutdown readiness flag must end the stream: {frames}"
        );
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

    /// Read complete SSE frames off a body stream until `wanted` data
    /// frames arrived (heartbeats/comment lines are skipped). Returns
    /// `(id, event, data)` triples in wire order and the number of body
    /// chunks consumed.
    async fn read_wire_frames(
        body: axum::body::Body,
        wanted: usize,
        deadline: std::time::Duration,
    ) -> (Vec<(u64, String, String)>, usize) {
        use futures_util::StreamExt as _;
        let mut stream = body.into_data_stream();
        let mut pending = Vec::new();
        let mut frames = Vec::new();
        let mut chunks = 0usize;
        while frames.len() < wanted {
            let next = tokio::time::timeout(deadline, stream.next())
                .await
                .expect("SSE read deadline");
            let Some(Ok(chunk)) = next else { break };
            chunks += 1;
            pending.extend_from_slice(&chunk);
            while let Some(end) = pending.windows(2).position(|w| w == b"\n\n") {
                let raw: Vec<u8> = pending.drain(..end + 2).collect();
                let text = String::from_utf8_lossy(&raw);
                let mut id = None;
                let mut event = None;
                let mut data = String::new();
                for line in text.lines() {
                    if let Some(v) = line.strip_prefix("id: ") {
                        id = v.trim().parse::<u64>().ok();
                    } else if let Some(v) = line.strip_prefix("event: ") {
                        event = Some(v.trim().to_string());
                    } else if let Some(v) = line.strip_prefix("data: ") {
                        if !data.is_empty() {
                            data.push('\n');
                        }
                        data.push_str(v);
                    }
                }
                if let (Some(id), Some(event)) = (id, event) {
                    if event != "heartbeat" {
                        frames.push((id, event, data));
                    }
                }
            }
            // A single coalesced chunk may carry more frames than the
            // consumer reads before disconnecting; model exactly that.
            frames.truncate(wanted);
        }
        (frames, chunks)
    }

    /// Coalescing is a TRANSPORT concern only: a pre-seeded catch-up page is
    /// batched into one HTTP chunk (≤ 64 frames), yet every journal event
    /// keeps its own complete frame, id and payload, in journal order.
    #[tokio::test]
    async fn native_journal_sse_coalesced_chunks_preserve_every_frame_and_order() {
        let dir = tempfile::tempdir().unwrap();
        let (state, sid) = test_state(dir.path(), "sse-coalesce");
        let handle = state.deps.session.get_session(sid).unwrap().unwrap();
        for i in 0..40u64 {
            let kind = if i % 2 == 0 {
                faktor_core::event::EventKind::ModelChunkReceived
            } else {
                faktor_core::event::EventKind::PhaseChanged
            };
            handle
                .force_append_event(
                    kind,
                    faktor_core::state::AgentState::Streaming,
                    Some(faktor_core::id::OpId::new(1)),
                    Some(serde_json::json!({"i": i})),
                )
                .unwrap();
        }
        let response = native_session_events(
            State(state.clone()),
            authed_headers(&state),
            Path(sid.to_string()),
            Ok(Query(NativeSessionEventsQuery { after: Some(1) })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let (frames, chunks) =
            read_wire_frames(response.into_body(), 40, std::time::Duration::from_secs(5)).await;
        assert_eq!(frames.len(), 40, "every durable event must be delivered");
        assert_eq!(
            chunks, 1,
            "a 40-frame catch-up page must coalesce into one HTTP chunk"
        );
        for (index, (id, event, data)) in frames.iter().enumerate() {
            assert_eq!(*id, index as u64 + 2, "journal order is preserved");
            let expected_kind = if index % 2 == 0 {
                "model_chunk_received"
            } else {
                "phase_changed"
            };
            assert_eq!(event, expected_kind);
            let row: serde_json::Value = serde_json::from_str(data).unwrap();
            assert_eq!(row["seq"], serde_json::json!(*id));
            assert_eq!(row["kind"], serde_json::json!(expected_kind));
            assert_eq!(row["payload"]["i"], serde_json::json!(index));
            assert_eq!(row["opId"], serde_json::json!("1"));
        }
    }

    /// An abrupt disconnect after an arbitrary frame plus a reconnect at the
    /// last delivered id yields EXACTLY the events the client missed: no
    /// duplicate of the last id, no gap, no dropped final frame (the
    /// coalescing buffer is never a cursor authority).
    #[tokio::test]
    async fn native_journal_sse_reconnect_is_duplicate_and_gap_free() {
        let dir = tempfile::tempdir().unwrap();
        let (state, sid) = test_state(dir.path(), "sse-reconnect");
        let handle = state.deps.session.get_session(sid).unwrap().unwrap();
        for i in 0..20u64 {
            handle
                .force_append_event(
                    faktor_core::event::EventKind::ModelChunkReceived,
                    faktor_core::state::AgentState::Streaming,
                    Some(faktor_core::id::OpId::new(1)),
                    Some(serde_json::json!({"i": i})),
                )
                .unwrap();
        }
        let first = native_session_events(
            State(state.clone()),
            authed_headers(&state),
            Path(sid.to_string()),
            Ok(Query(NativeSessionEventsQuery { after: Some(0) })),
        )
        .await;
        let (head, _) =
            read_wire_frames(first.into_body(), 7, std::time::Duration::from_secs(5)).await;
        assert_eq!(head.len(), 7);
        let last = head.last().unwrap().0;
        assert_eq!(last, 7, "seq 1 is the durable SessionCreated");

        // More events land while the client is disconnected, including the
        // one the dropped connection would have coalesced.
        for i in 20..25u64 {
            handle
                .force_append_event(
                    faktor_core::event::EventKind::ModelChunkReceived,
                    faktor_core::state::AgentState::Streaming,
                    Some(faktor_core::id::OpId::new(1)),
                    Some(serde_json::json!({"i": i})),
                )
                .unwrap();
        }
        let second = native_session_events(
            State(state.clone()),
            authed_headers(&state),
            Path(sid.to_string()),
            Ok(Query(NativeSessionEventsQuery { after: Some(last) })),
        )
        .await;
        // The first connection had already coalesced ALL 20 seeded frames
        // into one chunk; the client consumed 7 of them before dropping the
        // body. Reconnect from id 7 must replay 8..=21 plus the 5 new
        // events: nothing is lost just because the server buffered it.
        let (tail, _) =
            read_wire_frames(second.into_body(), 19, std::time::Duration::from_secs(5)).await;
        assert_eq!(tail.len(), 19, "20 seeded - 7 already seen + 5 new");
        let ids: Vec<u64> = head.iter().chain(tail.iter()).map(|f| f.0).collect();
        assert_eq!(
            ids,
            (1..=26u64).collect::<Vec<_>>(),
            "reconnect resumes strictly above the last delivered id"
        );
        let payloads: Vec<u64> = tail
            .iter()
            .map(|(_, _, data)| {
                serde_json::from_str::<serde_json::Value>(data).unwrap()["payload"]["i"]
                    .as_u64()
                    .unwrap()
            })
            .collect();
        assert_eq!(payloads, (6..=24u64).collect::<Vec<_>>());
    }

    /// The emitted chunk bounds are hard: at most 64 data frames and a flush
    /// once 64 KiB is reached (a single frame is never split). The byte bound
    /// is exercised with ~4 KiB payloads so the flushes happen well below 64
    /// frames; the tail remainder must still flush (65..255 partial page).
    #[tokio::test]
    async fn native_journal_sse_chunk_bounds_never_split_or_reorder_frames() {
        use futures_util::StreamExt as _;
        let dir = tempfile::tempdir().unwrap();
        let (state, sid) = test_state(dir.path(), "sse-bounds");
        let handle = state.deps.session.get_session(sid).unwrap().unwrap();
        let pad = "x".repeat(4000);
        for i in 0..200u64 {
            handle
                .force_append_event(
                    faktor_core::event::EventKind::ModelChunkReceived,
                    faktor_core::state::AgentState::Streaming,
                    Some(faktor_core::id::OpId::new(1)),
                    Some(serde_json::json!({"i": i, "pad": pad})),
                )
                .unwrap();
        }
        let response = native_session_events(
            State(state.clone()),
            authed_headers(&state),
            Path(sid.to_string()),
            Ok(Query(NativeSessionEventsQuery { after: Some(1) })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let mut body = response.into_body().into_data_stream();
        let mut ids: Vec<u64> = Vec::new();
        let mut chunks = 0usize;
        while ids.len() < 200 {
            let chunk = tokio::time::timeout(std::time::Duration::from_secs(3), body.next())
                .await
                .expect("SSE chunk deadline")
                .expect("stream ended before every seeded frame")
                .expect("SSE body read failed");
            chunks += 1;
            let text = String::from_utf8_lossy(&chunk);
            let frame_ids: Vec<u64> = text
                .lines()
                .filter_map(|line| line.strip_prefix("id: "))
                .filter_map(|raw| raw.parse::<u64>().ok())
                .collect();
            assert!(
                frame_ids.len() <= 64,
                "chunk {chunks} carries {} frames (>64)",
                frame_ids.len()
            );
            assert!(
                chunk.len() <= 64 * 1024 + 8 * 1024,
                "chunk {chunks} is {} bytes (64 KiB + one ~4 KiB frame max)",
                chunk.len()
            );
            ids.extend(frame_ids);
        }
        assert!(
            ids == (2..=201u64).collect::<Vec<_>>(),
            "every seeded frame exactly once in journal order"
        );
        assert!(
            chunks >= 4,
            "the 4 KiB payloads must trigger the byte flush, not just the frame cap (got {chunks})"
        );
    }

    /// A non-delta event flushes the pending delta batch WITH it in one
    /// chunk: it never waits behind the delta linger window, and the deltas
    /// before/after it stay in journal order.
    #[tokio::test]
    async fn native_journal_sse_non_delta_flushes_the_pending_delta_batch() {
        let dir = tempfile::tempdir().unwrap();
        let (state, sid) = test_state(dir.path(), "sse-non-delta");
        let handle = state.deps.session.get_session(sid).unwrap().unwrap();
        for i in 0..10u64 {
            handle
                .force_append_event(
                    faktor_core::event::EventKind::ModelChunkReceived,
                    faktor_core::state::AgentState::Streaming,
                    Some(faktor_core::id::OpId::new(1)),
                    Some(serde_json::json!({"i": i})),
                )
                .unwrap();
        }
        handle
            .force_append_event(
                faktor_core::event::EventKind::PhaseChanged,
                faktor_core::state::AgentState::WaitingForModel,
                None,
                None,
            )
            .unwrap();
        for i in 10..20u64 {
            handle
                .force_append_event(
                    faktor_core::event::EventKind::ModelChunkReceived,
                    faktor_core::state::AgentState::Streaming,
                    Some(faktor_core::id::OpId::new(1)),
                    Some(serde_json::json!({"i": i})),
                )
                .unwrap();
        }
        let response = native_session_events(
            State(state.clone()),
            authed_headers(&state),
            Path(sid.to_string()),
            Ok(Query(NativeSessionEventsQuery { after: Some(1) })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let (first, ended) = read_sse(response, 1, std::time::Duration::from_secs(2)).await;
        assert!(
            !ended,
            "the live stream stays open after the flush: {first}"
        );
        assert_eq!(
            first.matches("event: model_chunk_received").count(),
            20,
            "all 20 pending deltas flush with the non-delta: {first}"
        );
        assert_eq!(
            first.matches("event: phase_changed").count(),
            1,
            "the non-delta is in the same chunk: {first}"
        );
        let phase_at = first.find("event: phase_changed").unwrap();
        let before = first[..phase_at]
            .matches("event: model_chunk_received")
            .count();
        assert_eq!(
            before, 10,
            "the 10 deltas before the non-delta stay ahead of it"
        );
    }

    /// Regression: a delta-only catch-up page of 65..255 events is emitted as
    /// a 64-frame chunk plus a small remainder. With no newer durable tail the
    /// remainder must still flush at the coalescing deadline (it is the FINAL
    /// data of this connection); it must never be stranded behind the idle
    /// heartbeat/fallback loop until another append or a reconnect.
    #[tokio::test]
    async fn native_journal_sse_partial_delta_page_tail_flushes_without_a_new_append() {
        let dir = tempfile::tempdir().unwrap();
        let (state, sid) = test_state(dir.path(), "sse-partial-tail");
        let handle = state.deps.session.get_session(sid).unwrap().unwrap();
        for i in 0..100u64 {
            handle
                .force_append_event(
                    faktor_core::event::EventKind::ModelChunkReceived,
                    faktor_core::state::AgentState::Streaming,
                    Some(faktor_core::id::OpId::new(1)),
                    Some(serde_json::json!({"i": i})),
                )
                .unwrap();
        }
        let response = native_session_events(
            State(state.clone()),
            authed_headers(&state),
            Path(sid.to_string()),
            // Seq 1 is the durable SessionCreated row; the 100 deltas follow.
            Ok(Query(NativeSessionEventsQuery { after: Some(1) })),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        // The stream stays open after the tail (it is a live subscription), so
        // stop reading as soon as every seeded data frame arrived. A stranded
        // remainder hits the per-read deadline and panics.
        let started = std::time::Instant::now();
        let (frames, chunks) =
            read_wire_frames(response.into_body(), 100, std::time::Duration::from_secs(3)).await;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "the final delta frames must flush at the 8 ms coalescing deadline"
        );
        assert_eq!(frames.len(), 100, "every seeded delta must be delivered");
        let ids: Vec<u64> = frames.iter().map(|f| f.0).collect();
        assert_eq!(ids, (2..=101u64).collect::<Vec<_>>(), "journal order");
        let payloads: Vec<u64> = frames
            .iter()
            .map(|(_, _, data)| {
                serde_json::from_str::<serde_json::Value>(data).unwrap()["payload"]["i"]
                    .as_u64()
                    .unwrap()
            })
            .collect();
        assert_eq!(payloads, (0..100u64).collect::<Vec<_>>());
        assert!(
            chunks <= 4,
            "the 100-frame tail coalesces into the 64-frame chunk plus one remainder (got {chunks})"
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

    /// A key whose claim is still pending with the SAME body is an in-flight
    /// 409 BEFORE any `PromptReceived` append or queue/message write: the
    /// claim is the first durable act of the prompt path.
    #[tokio::test]
    async fn prompt_pending_key_with_the_same_body_is_an_in_flight_409_before_any_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let (state, sid) = test_state(dir.path(), "prompt-in-flight");
        let handle = state.deps.session.get_session(sid).unwrap().unwrap();
        let store = state.deps.session.store();
        let digest = prompt_admission_digest(sid, "hi", &[]);
        assert_eq!(
            store
                .prompt_admission_claim(
                    sid,
                    PROMPT_KEY,
                    &digest,
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

    /// A pending key whose stored digest is DIFFERENT from this body is the
    /// typed key-reuse conflict (never a lease-long in-flight answer), with
    /// zero mutation: no `PromptReceived` append, message or queue row.
    #[tokio::test]
    async fn prompt_pending_key_with_a_different_body_is_a_typed_key_reuse_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let (state, sid) = test_state(dir.path(), "prompt-pending-key-reused");
        let handle = state.deps.session.get_session(sid).unwrap().unwrap();
        let store = state.deps.session.store();
        assert_eq!(
            store
                .prompt_admission_claim(
                    sid,
                    PROMPT_KEY,
                    "digest-from-another-writer",
                    "tx-00000000000000bc",
                    handle.now_ms()
                )
                .unwrap(),
            PromptAdmissionClaim::Fresh
        );
        let (status, text) = prompt_call(&state, sid, prompt_body(sid, PROMPT_KEY, "hi")).await;
        assert_eq!(status, StatusCode::CONFLICT, "{text}");
        let body: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(body["error"]["code"], "conflict", "{body}");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("different prompt"),
            "a pending key with different bytes names the reuse, not in-flight: {body}"
        );
        assert_eq!(prompt_received_count(&handle), 0);
        assert_eq!(store.message_count(sid).unwrap(), 0);
        assert_eq!(handle.queued_prompt_count().unwrap(), 0);
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

    /// Perf-wave evidence for the SSE hot path: the borrowed
    /// [`NativeEventRowRef`] serializer must emit BYTE-IDENTICAL JSON to the
    /// historical `Value` round-trip (`json!` row + `to_string`), for the
    /// full field matrix (op id present/absent, payload present (large
    /// object + unicode)/absent, several kinds/states). The elapsed probe
    /// runs the production 1k-frame replay shape with 2 KiB payloads.
    #[test]
    fn native_event_row_direct_serializer_is_byte_identical_and_cheaper() {
        let session = faktor_core::id::SessionId::new(42);
        let payload: serde_json::Value = serde_json::json!({
            "kind": "tool_output",
            "text": "x".repeat(2048),
            "unicode": "颜色 π-😀",
            "nested": { "a": [1, 2, 3], "b": null },
        });
        let events: Vec<faktor_core::event::Event> = (0..1000u64)
            .map(|i| {
                faktor_core::event::Event::new(
                    faktor_core::id::EventSeq::new(i + 1),
                    session,
                    if i % 3 == 0 {
                        Some(faktor_core::id::OpId::new(i + 7))
                    } else {
                        None
                    },
                    if i % 2 == 0 {
                        faktor_core::event::EventKind::PhaseChanged
                    } else {
                        faktor_core::event::EventKind::PromptReceived
                    },
                    if i % 5 == 0 {
                        faktor_core::state::AgentState::WaitingForModel
                    } else {
                        faktor_core::state::AgentState::Streaming
                    },
                    1_700_000_000_000 + i as i64,
                    if i % 7 == 0 {
                        None
                    } else {
                        Some(payload.clone())
                    },
                )
            })
            .collect();

        // Independent historical shape: the row exactly as the `json!`
        // path built it (opId a decimal STRING, alphabetical fields).
        let render = |e: &faktor_core::event::Event| -> String {
            serde_json::to_string(&serde_json::json!({
                "kind": e.kind.as_tag(),
                "opId": e.op_id.map(|o| o.to_string()),
                "payload": &e.payload,
                "seq": e.seq.raw(),
                "state": e.state.as_tag(),
                "tsMs": e.ts_ms,
            }))
            .unwrap()
        };
        for e in &events {
            assert_eq!(
                serde_json::to_string(&native_event_row(e)).unwrap(),
                render(e),
                "direct serializer must be byte-identical at seq {}",
                e.seq.raw()
            );
        }

        // 1k frames x 2 KiB payloads (the production catch-up page shape):
        // the historical path deep-cloned the payload into a Value tree and
        // re-serialized the tree; the direct path serializes once.
        let t0 = std::time::Instant::now();
        let mut old_bytes = 0usize;
        for _ in 0..3 {
            for e in &events {
                old_bytes += serde_json::to_string(&native_event_row(e)).unwrap().len();
            }
        }
        let old_elapsed = t0.elapsed();
        let t1 = std::time::Instant::now();
        let mut new_bytes = 0usize;
        let mut row_buf: Vec<u8> = Vec::with_capacity(4096);
        for _ in 0..3 {
            for e in &events {
                row_buf.clear();
                serde_json::to_writer(
                    &mut row_buf,
                    &NativeEventRowRef {
                        kind: e.kind.as_tag(),
                        op_id: e.op_id.map(OpIdWire),
                        payload: &e.payload,
                        seq: e.seq.raw(),
                        state: e.state.as_tag(),
                        ts_ms: e.ts_ms,
                    },
                )
                .unwrap();
                new_bytes += row_buf.len();
            }
        }
        let new_elapsed = t1.elapsed();
        assert_eq!(old_bytes, new_bytes);
        eprintln!(
            "sse frame row (3 x 1000 frames, 2 KiB payloads): value round-trip {old_elapsed:?} -> direct {new_elapsed:?} ({old_bytes} bytes)"
        );
        assert!(
            new_elapsed < old_elapsed,
            "direct serialization must beat the Value round-trip: old {old_elapsed:?} new {new_elapsed:?}"
        );
    }
}
