//! Session-owned terminal projection — a thin adapter over the ONE durable
//! [`TerminalService`](crate::native::terminal_authority::TerminalService).
//!
//! Since the terminal unification there is exactly ONE daemon terminal
//! authority: the durable `terminal_*` ledger rows governed by
//! [`TerminalService`]. The native HTTP surface below only parses requests,
//! calls the service, and projects the durable rows; it keeps no registry of
//! its own. `AppState.terminal_events` is a strictly derived bounded frame
//! cache — not an authority.

use super::terminal_authority::{
    ProcessIdentity, TerminalReconcileDisposition, TerminalService, TerminalServiceError,
    TerminalSpawnRequest,
};
use super::*;
use crate::api::AppState;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

/// `GET /native/session/{id}/terminal` — the daemon-level terminal view:
/// every durable session-owned terminal row (`{id, pid, alive}`), with the
/// numeric `ptyId` projected additively when a live handle of this boot
/// owns it. The SESSION-SCOPED projection is
/// `GET /native/terminals?session=<id>`.
pub(crate) async fn native_session_terminal(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    if let Err(r) = native_resolve_session(&state, &id) {
        return *r;
    }
    // Every row is a durable session-owned terminal row of this daemon
    // (the ONE authority); ids are the terminal UUIDs and the numeric
    // `ptyId` is projected additively for rows a live pty handle of this
    // boot owns. There is no unowned daemon-level PTY registry anymore.
    let mut rows: Vec<serde_json::Value> = Vec::new();
    let sessions = match state.deps.session.list_sessions(None) {
        Ok(sessions) => sessions,
        Err(e) => return api_err(&e),
    };
    let service = TerminalService::for_manager(&state.deps.session);
    for handle in sessions {
        match service.list(&handle.id().to_string()) {
            Ok(views) => {
                for view in views {
                    if rows.len() >= MAX_NATIVE_LIST {
                        break;
                    }
                    let mut row = serde_json::json!({
                        "id": view.terminal_id,
                        "pid": view.pid,
                        "alive": view.alive,
                        "sessionId": view.session_id.to_string(),
                        "taskId": view.task_id.to_string(),
                        "agentId": view.agent_id,
                        "operationId": view.operation_id.to_string(),
                        "spawnedMs": view.spawned_ms,
                        "state": view.state_tag(),
                    });
                    if let Some(pty_id) = view.pty_id {
                        row["ptyId"] = serde_json::json!(pty_id.to_string());
                    }
                    rows.push(row);
                }
            }
            Err(e) => return terminal_error_response(e),
        }
    }
    Json(serde_json::json!(rows)).into_response()
}

/// `GET /native/session/{id}/terminals/{terminal_id}/output` — the bounded
/// output snapshot of ONE session-owned terminal (the live PTY ring; a
/// terminal whose live handle is gone answers a typed 409, never
/// fabricated bytes). The session must own the terminal.
pub(crate) async fn native_terminal_output(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, terminal_id)): Path<(String, String)>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    if terminal_id.trim().is_empty() || terminal_id.len() > 256 {
        return wire_status(malformed_body("invalid terminal id"));
    }
    let service = TerminalService::for_manager(&state.deps.session);
    match service.output_snapshot(&handle.id().to_string(), &terminal_id) {
        Ok((output, alive)) => Json(serde_json::json!({
            "ok": true,
            "output": output,
            "alive": alive,
        }))
        .into_response(),
        Err(e) => terminal_error_response(e),
    }
}

/// Bound of one session-scoped terminal listing.
pub(crate) const MAX_NATIVE_TERMINALS: usize = 256;

/// Bounded daemon-wide terminal lifetime-event cache depth (strictly derived
/// from the durable rows on every read).
pub(crate) const TERMINAL_EVENT_RING: usize = 512;

/// Cap on one terminal spawn (mirrors `/pty/create`).
pub(crate) const MAX_NATIVE_TERMINAL_FIELD_BYTES: usize = 4096;

/// Cap on one terminal spawn arg list.
pub(crate) const MAX_NATIVE_TERMINAL_ARGS: usize = 256;

/// Cap on one native terminal input payload.
pub(crate) const MAX_NATIVE_TERMINAL_INPUT_BYTES: usize = 64 * 1024;

/// One bounded terminal lifetime frame derived from a durable row.
fn terminal_event_frame(record: &faktor_session::TerminalLedgerRecord) -> serde_json::Value {
    serde_json::json!({
        "id": record.seq,
        "type": record.kind.as_tag(),
        "terminalId": record.row.terminal_id,
        "ptyId": record.row.terminal_id,
        "pid": record.row.pid,
        "tsMs": record.row.at_ms,
        "sessionId": record.row.session_id.to_string(),
        "startTimeMs": record.row.start_time_ms,
        "detail": record.detail,
        "exitCode": record.exit_code,
    })
}

/// Refresh the bounded derived event cache of one session from the durable
/// rows (the cache is NEVER an authority: it is cleared and rebuilt here).
fn refresh_terminal_events(state: &AppState, session_id: &str) -> Result<(), TerminalServiceError> {
    let service = TerminalService::for_manager(&state.deps.session);
    let (records, _) = service.events(session_id, None, TERMINAL_EVENT_RING as u64)?;
    let mut ring = state
        .terminal_events
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    ring.clear();
    for record in records {
        let cache_id = state
            .next_terminal_event_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        ring.push_back((cache_id, terminal_event_frame(&record)));
    }
    Ok(())
}

/// Strict query DTO of the session-scoped terminal listing
/// (`?session=<id>` only).
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeTerminalsQuery {
    session: String,
}

/// `GET /native/terminals?session=<id>` — the session-owned terminal
/// projection: ONLY the durable rows whose ownership names `session`
/// (including Lost rows; their state is explicit). `unowned` counts the
/// frozen daemon-level `/pty/*` rows, which are never projected into a
/// session view. Hostile session ids are 400; unknown sessions 404.
pub(crate) async fn native_terminals(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<NativeTerminalsQuery>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let handle = match native_resolve_session(&state, &q.session) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    let sid = handle.id();
    let service = TerminalService::for_manager(&state.deps.session);
    let sid_string = sid.to_string();
    let views = match tokio::task::spawn_blocking(move || service.list(&sid_string)).await {
        Ok(Ok(views)) => views,
        Ok(Err(e)) => return terminal_error_response(e),
        Err(_) => return wire_refused("terminal listing task failed"),
    };
    // The unowned daemon-level PTY registry was retired with the wire
    // surface; the shape stays honest with a constant zero.
    let unowned = 0u64;
    let note = String::new();
    let rows: Vec<serde_json::Value> = views
        .into_iter()
        .take(MAX_NATIVE_TERMINALS)
        .map(|view| {
            serde_json::json!({
                "id": view.terminal_id,
                "terminalId": view.terminal_id,
                "ptyId": view.pty_id.map(|id| id.to_string()),
                "pid": view.pid,
                "alive": view.alive,
                "state": view.state_tag(),
                "sessionId": view.session_id.to_string(),
                "taskId": view.task_id.to_string(),
                "agentId": view.agent_id,
                "operationId": view.operation_id.to_string(),
                "spawnedMs": view.spawned_ms,
                "updatedMs": view.updated_ms,
                "startTimeMs": view.start_time_ms,
                "exitCode": view.exit_code,
                "detail": view.detail,
            })
        })
        .collect();
    Json(serde_json::json!({
        "sessionId": sid.to_string(),
        "terminals": rows,
        "unowned": unowned,
        "note": note,
    }))
    .into_response()
}

/// Strict query DTO of the terminal event log page.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeTerminalEventsQuery {
    #[serde(default)]
    after: Option<u64>,
    #[serde(default)]
    limit: Option<u64>,
}

/// `GET /native/session/{id}/terminal/events?after=<n>&limit=<n>` — the
/// session-owned terminal LIFETIME event log: the durable `terminal_*` rows
/// of the session, `id` (the durable seq) ascending strictly above `after`.
/// The response's `id` is the durable seq, so cursors are stable across
/// reads and restarts. Unknown sessions 404, hostile ids 400.
pub(crate) async fn native_terminal_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<NativeTerminalEventsQuery>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    let limit = match page_limit(q.limit, MAX_NATIVE_CURSOR_PAGE) {
        Ok(l) => l,
        Err(e) => return wire_status(e),
    };
    let sid_string = handle.id().to_string();
    if let Err(e) = refresh_terminal_events(&state, &sid_string) {
        return terminal_error_response(e);
    }
    let after = q.after.unwrap_or(0);
    let ring = state
        .terminal_events
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut page: Vec<serde_json::Value> = Vec::new();
    let mut more = false;
    let mut last: Option<u64> = None;
    for (_cache_id, event) in ring.iter() {
        let event_id = event.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
        if event_id <= after {
            continue;
        }
        if page.len() as i64 >= limit {
            more = true;
            break;
        }
        page.push(event.clone());
        last = Some(event_id);
    }
    Json(serde_json::json!({
        "sessionId": sid_string,
        "events": page,
        "hasMore": more,
        "nextCursor": if more { last.map(|v| serde_json::json!(v)) } else { Some(serde_json::Value::Null) },
    }))
    .into_response()
}

/// Strict native body of a session-owned terminal spawn
/// (`deny_unknown_fields` — a typo is a 400).
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeTerminalSpawnBody {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    rows: Option<u32>,
    #[serde(default)]
    cols: Option<u32>,
}

/// `POST /native/session/{id}/terminal` — spawn a SESSION-OWNED terminal
/// through the ONE durable terminal service: the durable row carries
/// `{session_id, task_id, agent_id, operation_id}` plus the terminal UUID
/// and the child's process identity. The strict body mirrors `/pty/create`.
pub(crate) async fn native_terminal_spawn(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<NativeTerminalSpawnBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    // Strict native DTO: every body rejection is a plain 400 (unknown
    // fields, typos, missing fields).
    let Json(body) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid native terminal spawn body")),
    };
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    if body.command.is_empty() || body.command.len() > MAX_NATIVE_TERMINAL_FIELD_BYTES {
        return wire_status(malformed_body(
            "terminal command must be non-empty and at most 4096 bytes",
        ));
    }
    if body.args.len() > MAX_NATIVE_TERMINAL_ARGS
        || body
            .args
            .iter()
            .any(|a| a.len() > MAX_NATIVE_TERMINAL_FIELD_BYTES)
    {
        return wire_status(malformed_body("terminal args are oversized"));
    }
    if let Some(cwd) = &body.cwd {
        if cwd.len() > MAX_NATIVE_TERMINAL_FIELD_BYTES {
            return wire_status(malformed_body("terminal cwd is oversized"));
        }
    }
    let rows = u16::try_from(body.rows.unwrap_or(24))
        .unwrap_or(u16::MAX)
        .max(1);
    let cols = u16::try_from(body.cols.unwrap_or(80))
        .unwrap_or(u16::MAX)
        .max(1);
    let request = TerminalSpawnRequest {
        command: body.command.clone(),
        args: body.args.clone(),
        cwd: body.cwd.clone(),
        env: Vec::new(),
        rows,
        cols,
    };
    let service = TerminalService::for_manager(&state.deps.session);
    let sid = handle.id().to_string();
    let creation = match tokio::task::spawn_blocking(move || service.spawn(&sid, &request)).await {
        Ok(Ok(creation)) => creation,
        Ok(Err(e)) => return terminal_error_response(e),
        Err(_) => return wire_refused("terminal spawn task failed"),
    };
    let view = creation.view;
    Json(serde_json::json!({
        "ok": true,
        "terminalId": view.terminal_id,
        "ptyId": view.terminal_id,
        "pid": view.pid,
        "sessionId": view.session_id.to_string(),
        "taskId": view.task_id.to_string(),
        "agentId": view.agent_id,
        "operationId": view.operation_id.to_string(),
        "state": view.state_tag(),
        "startTimeMs": view.start_time_ms,
        "spawnedMs": view.spawned_ms,
    }))
    .into_response()
}

/// Strict native body of one terminal input write.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeTerminalInputBody {
    data: String,
}

/// `POST /native/session/{id}/terminals/{terminal_id}/input` — write input
/// to a Running terminal through the durable service. A Lost/unreachable
/// row (restart) is refused with the typed 409; a foreign terminal id is an
/// indistinguishable 404.
pub(crate) async fn native_terminal_input(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, terminal_id)): Path<(String, String)>,
    body: Result<Json<NativeTerminalInputBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid native terminal input body")),
    };
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    if body.data.len() > MAX_NATIVE_TERMINAL_INPUT_BYTES {
        return wire_status(malformed_body("terminal input is oversized"));
    }
    let service = TerminalService::for_manager(&state.deps.session);
    let sid = handle.id().to_string();
    let data = body.data.into_bytes();
    match tokio::task::spawn_blocking(move || service.input(&sid, &terminal_id, &data)).await {
        Ok(Ok(())) => Json(serde_json::json!({ "ok": true })).into_response(),
        Ok(Err(e)) => terminal_error_response(e),
        Err(_) => wire_refused("terminal input task failed"),
    }
}

/// Strict native body of one terminal resize.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeTerminalResizeBody {
    rows: u32,
    cols: u32,
}

/// `POST /native/session/{id}/terminals/{terminal_id}/resize`.
pub(crate) async fn native_terminal_resize(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, terminal_id)): Path<(String, String)>,
    body: Result<Json<NativeTerminalResizeBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid native terminal resize body")),
    };
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    let rows = match u16::try_from(body.rows) {
        Ok(rows) if rows > 0 => rows,
        _ => return wire_status(malformed_body("terminal rows must be 1..=65535")),
    };
    let cols = match u16::try_from(body.cols) {
        Ok(cols) if cols > 0 => cols,
        _ => return wire_status(malformed_body("terminal cols must be 1..=65535")),
    };
    let service = TerminalService::for_manager(&state.deps.session);
    let sid = handle.id().to_string();
    match tokio::task::spawn_blocking(move || service.resize(&sid, &terminal_id, rows, cols)).await
    {
        Ok(Ok(())) => Json(serde_json::json!({ "ok": true })).into_response(),
        Ok(Err(e)) => terminal_error_response(e),
        Err(_) => wire_refused("terminal resize task failed"),
    }
}

/// Strict native body of one terminal kill (the reason is bounded audit
/// text; empty = the default reason).
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeTerminalKillBody {
    #[serde(default)]
    reason: Option<String>,
}

/// `POST /native/session/{id}/terminals/{terminal_id}/kill` — kill through
/// the pty authority and journal `terminal_killed` exactly once (idempotent
/// afterwards).
pub(crate) async fn native_terminal_kill(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, terminal_id)): Path<(String, String)>,
    body: Result<Json<NativeTerminalKillBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let reason = match body {
        Ok(Json(body)) => body.reason.unwrap_or_else(|| "kill requested".into()),
        // An empty body is the default kill; malformed non-empty bodies are
        // strict 400s.
        Err(_) => return wire_status(malformed_body("invalid native terminal kill body")),
    };
    if reason.len() > 4096 || reason.contains('\0') {
        return wire_status(malformed_body(
            "terminal kill reason is invalid or oversized",
        ));
    }
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    let service = TerminalService::for_manager(&state.deps.session);
    let sid = handle.id().to_string();
    match tokio::task::spawn_blocking(move || service.kill(&sid, &terminal_id, &reason)).await {
        Ok(Ok(killed)) => Json(serde_json::json!({ "ok": true, "killed": killed })).into_response(),
        Ok(Err(e)) => terminal_error_response(e),
        Err(_) => wire_refused("terminal kill task failed"),
    }
}

/// Strict native body of one stale-row reconciliation.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeTerminalReconcileBody {
    /// `killed` | `collected`.
    disposition: String,
    /// The caller-observed process identity, when it has one. A mismatch
    /// with the durable row is refused and nothing is journaled.
    #[serde(default)]
    observed_pid: Option<u32>,
    #[serde(default)]
    observed_start_time_ms: Option<i64>,
}

/// `POST /native/session/{id}/terminals/{terminal_id}/reconcile` — finish a
/// stale Lost row exactly once, typed.
pub(crate) async fn native_terminal_reconcile(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, terminal_id)): Path<(String, String)>,
    body: Result<Json<NativeTerminalReconcileBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid native terminal reconcile body")),
    };
    let disposition = match body.disposition.as_str() {
        "killed" => TerminalReconcileDisposition::Killed,
        "collected" => TerminalReconcileDisposition::Collected,
        _ => {
            return wire_status(malformed_body(
                "terminal reconcile disposition must be killed|collected",
            ))
        }
    };
    let observed = match (body.observed_pid, body.observed_start_time_ms) {
        (Some(pid), Some(start_time_ms)) => Some(ProcessIdentity { pid, start_time_ms }),
        (None, None) => None,
        _ => {
            return wire_status(malformed_body(
                "observed_pid and observed_start_time_ms must be provided together",
            ))
        }
    };
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    let service = TerminalService::for_manager(&state.deps.session);
    let sid = handle.id().to_string();
    match tokio::task::spawn_blocking(move || {
        service.reconcile(&sid, &terminal_id, disposition, observed)
    })
    .await
    {
        Ok(Ok(view)) => Json(serde_json::json!({
            "ok": true,
            "terminalId": view.terminal_id,
            "state": view.state_tag(),
            "reconciled": view.detail,
        }))
        .into_response(),
        Ok(Err(e)) => terminal_error_response(e),
        Err(_) => wire_refused("terminal reconcile task failed"),
    }
}

/// Map a typed terminal service failure onto the native wire: invalid
/// requests are 400s, foreign/unknown terminals are indistinguishable 404s,
/// and state/lost/identity refusals are typed 409s.
fn terminal_error_response(error: TerminalServiceError) -> Response {
    match error {
        TerminalServiceError::Invalid(message) => wire_status(malformed_body(&message)),
        TerminalServiceError::Unknown { .. } | TerminalServiceError::ForeignScope { .. } => {
            wire_status(not_found("unknown terminal for this session"))
        }
        TerminalServiceError::Lost { terminal_id, detail } => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "ok": false,
                "code": "lost",
                "terminalId": terminal_id,
                "message": detail,
            })),
        )
            .into_response(),
        TerminalServiceError::IdentityMismatch { .. } => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "ok": false, "code": "identity_mismatch", "message": error.to_string() })),
        )
            .into_response(),
        TerminalServiceError::State {
            terminal_id,
            state,
            message,
        } => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "ok": false,
                "code": "state",
                "terminalId": terminal_id,
                "state": state,
                "message": message,
            })),
        )
            .into_response(),
        TerminalServiceError::Unavailable(message) => wire_refused(&message),
        TerminalServiceError::Refused(message) => wire_refused(&message),
    }
}
