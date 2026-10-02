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

/// The ONE terminal service behind every native terminal operation,
/// constructed under the INJECTED terminal execution-authority policy (the
/// host's configured `[sandbox]` shell contract, wired through
/// [`ServerDeps::terminal_policy`](crate::api::ServerDeps::terminal_policy)).
/// No handler reads a global policy: the configured mode is enforced and
/// recorded because it is the policy of the service this daemon built. The
/// process-wide registry keeps exactly one service per session manager, so
/// the ACP host and this surface share the same authority.
fn terminal_service(state: &AppState) -> std::sync::Arc<TerminalService> {
    TerminalService::for_manager_with_policy(
        &state.deps.session,
        state.deps.terminal_policy.clone(),
    )
}

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
    let service = terminal_service(&state);
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
    let service = terminal_service(&state);
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

/// The newest bounded slice of one session's durable terminal rows — the
/// authority every page is derived from, never the cache.
fn terminal_event_records(
    state: &AppState,
    session_id: &str,
) -> Result<Vec<faktor_session::TerminalLedgerRecord>, TerminalServiceError> {
    let service = terminal_service(state);
    let (records, _) = service.events(session_id, None, TERMINAL_EVENT_RING as u64)?;
    Ok(records)
}

/// Build one page of the session-owned terminal lifetime log.
///
/// `refresh` runs after the request's durable rows are fetched and before
/// the ONE critical section that clears, refills and pages the shared
/// bounded cache: the seam exists so tests can force another session's
/// complete refresh to land at the most dangerous interleaving point.
/// Production passes a no-op. Because the clear/refill/page sequence is
/// covered by a single lock hold, an interleaved refresh of another session
/// cannot swap the ring contents mid-read; the per-frame session filter is
/// belt-and-braces so no other session's frame can ever be emitted even if
/// a future writer planted one.
fn terminal_events_page(
    state: &AppState,
    session_id: &str,
    after: u64,
    limit: i64,
    refresh: impl FnOnce(),
) -> Result<serde_json::Value, TerminalServiceError> {
    let records = terminal_event_records(state, session_id)?;
    refresh();
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
    let mut page: Vec<serde_json::Value> = Vec::new();
    let mut more = false;
    let mut last: Option<u64> = None;
    for (_cache_id, event) in ring.iter() {
        if event.get("sessionId").and_then(|v| v.as_str()) != Some(session_id) {
            continue;
        }
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
    Ok(serde_json::json!({
        "sessionId": session_id,
        "events": page,
        "hasMore": more,
        "nextCursor": if more { last.map(|v| serde_json::json!(v)) } else { Some(serde_json::Value::Null) },
    }))
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
    let service = terminal_service(&state);
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
                "executionProfile": if view.execution_profile.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::from_str::<serde_json::Value>(&view.execution_profile)
                        .unwrap_or(serde_json::Value::Null)
                },
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
    let after = q.after.unwrap_or(0);
    match terminal_events_page(&state, &sid_string, after, limit, || {}) {
        Ok(body) => Json(body).into_response(),
        Err(e) => terminal_error_response(e),
    }
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
    let service = terminal_service(&state);
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
        "executionProfile": if view.execution_profile.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_str::<serde_json::Value>(&view.execution_profile)
                .unwrap_or(serde_json::Value::Null)
        },
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
    let service = terminal_service(&state);
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
    let service = terminal_service(&state);
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
    let service = terminal_service(&state);
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
    let service = terminal_service(&state);
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
        TerminalServiceError::Denied(message) => (
            StatusCode::FORBIDDEN,
            Json(serde_json::json!({
                "ok": false,
                "code": "execution_denied",
                "message": message,
            })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod terminal_event_tests {
    use super::*;
    use crate::api::tests::test_deps;
    use crate::api::{AppState, ServerDeps};
    use faktor_core::id::SessionId;
    use faktor_session::{SessionManager, TerminalDurableRow};
    use std::sync::Arc;

    fn test_state(deps: ServerDeps) -> AppState {
        AppState {
            deps: Arc::new(deps),
            auth: Arc::new(std::sync::RwLock::new(None)),
            terminal_events: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
            next_terminal_event_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    fn create_session(manager: &Arc<SessionManager>, title: &str) -> String {
        let ws = manager.create_workspace("/tmp").unwrap();
        manager
            .create_session(ws, title, "fake", "m")
            .unwrap()
            .id()
            .to_string()
    }

    /// Mark the session recovered on the process-wide service BEFORE any
    /// rows are planted, so the lazy restart scan never journals a Lost row
    /// for the fixture terminal (the scan is not what these tests exercise).
    fn mark_recovered(state: &AppState, sid: &str) {
        terminal_service(state).events(sid, None, 1).unwrap();
    }

    /// Journal one terminal's durable rows directly (created + running, and
    /// killed when asked): the ledger is the ONE authority and no pty is
    /// needed to exercise the lifetime projection.
    fn seed_terminal(
        manager: &Arc<SessionManager>,
        sid: &str,
        terminal_id: &str,
        pid: u32,
        kill: bool,
    ) {
        let handle = manager
            .get_session(SessionId::new(sid.parse().unwrap()))
            .unwrap()
            .unwrap();
        let now = manager.now_ms();
        let row = TerminalDurableRow {
            terminal_id: terminal_id.to_string(),
            session_id: sid.parse().unwrap(),
            task_id: 1,
            agent_id: None,
            operation_id: 1,
            pid,
            start_time_ms: now,
            at_ms: now,
            execution_profile: String::new(),
        };
        handle.ledger_terminal_created(&row).unwrap();
        handle.ledger_terminal_running(&row).unwrap();
        if kill {
            handle.ledger_terminal_killed(&row, "test kill").unwrap();
        }
    }

    fn auth_headers(state: &AppState) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            format!("Bearer {}", state.deps.auth_token.as_str())
                .parse()
                .unwrap(),
        );
        headers
    }

    /// Call the endpoint exactly as the router does and decode its JSON.
    async fn read_page(
        state: &AppState,
        sid: &str,
        after: Option<u64>,
        limit: Option<u64>,
    ) -> (StatusCode, serde_json::Value) {
        let query = NativeTerminalEventsQuery { after, limit };
        let response = native_terminal_events(
            State(state.clone()),
            auth_headers(state),
            Path(sid.to_string()),
            Query(query),
        )
        .await;
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    fn event_ids(page: &serde_json::Value) -> Vec<u64> {
        page["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["id"].as_u64().unwrap())
            .collect()
    }

    #[tokio::test]
    async fn terminal_event_reads_are_session_isolated_sequentially() {
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        let state = test_state(deps);
        let manager = state.deps.session.clone();
        let a_sid = create_session(&manager, "term-iso-a");
        let b_sid = create_session(&manager, "term-iso-b");
        mark_recovered(&state, &a_sid);
        mark_recovered(&state, &b_sid);
        seed_terminal(&manager, &a_sid, "iso-a", 41001, true);
        seed_terminal(&manager, &b_sid, "iso-b", 41002, false);

        let (status, a) = read_page(&state, &a_sid, None, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(a["sessionId"], a_sid);
        let events = a["events"].as_array().unwrap();
        assert_eq!(events.len(), 3, "created + running + killed: {a}");
        for event in events {
            assert_eq!(event["sessionId"], a_sid);
            assert_eq!(event["terminalId"], "iso-a");
        }

        let (status, b) = read_page(&state, &b_sid, None, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(b["sessionId"], b_sid);
        let events = b["events"].as_array().unwrap();
        assert_eq!(events.len(), 2, "created + running: {b}");
        for event in events {
            assert_eq!(event["sessionId"], b_sid);
            assert_eq!(event["terminalId"], "iso-b");
        }

        // Reading A again after B refilled the shared ring still yields
        // only A's frames.
        let (_, a_again) = read_page(&state, &a_sid, None, None).await;
        assert_eq!(event_ids(&a_again).len(), 3);
        assert!(a_again["events"]
            .as_array()
            .unwrap()
            .iter()
            .all(|event| event["terminalId"] == "iso-a"));
    }

    /// Deterministic interleaving: B's durable rows are fetched FIRST, then
    /// a complete session-A refresh + page runs at the seam, and only then
    /// does B enter its clear/refill/page critical section. A split
    /// refresh/read (two lock holds) would serve B the A frames planted
    /// here; the one-lock design plus the session filter cannot.
    #[test]
    fn interleaved_foreign_refresh_cannot_cross_into_a_terminal_event_page() {
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        let state = test_state(deps);
        let manager = state.deps.session.clone();
        let a_sid = create_session(&manager, "term-seam-a");
        let b_sid = create_session(&manager, "term-seam-b");
        mark_recovered(&state, &a_sid);
        mark_recovered(&state, &b_sid);
        seed_terminal(&manager, &a_sid, "seam-a", 41101, false);
        seed_terminal(&manager, &b_sid, "seam-b", 41102, false);

        let b_page = terminal_events_page(&state, &b_sid, 0, 200, || {
            let foreign = terminal_events_page(&state, &a_sid, 0, 200, || {}).unwrap();
            let foreign_events = foreign["events"].as_array().unwrap();
            assert_eq!(foreign_events.len(), 2, "{foreign}");
            assert_eq!(foreign_events[0]["terminalId"], "seam-a");
        })
        .unwrap();

        assert_eq!(b_page["sessionId"], b_sid);
        let events = b_page["events"].as_array().unwrap();
        assert_eq!(events.len(), 2, "{b_page}");
        for event in events {
            assert_eq!(event["sessionId"], b_sid);
            assert_eq!(event["terminalId"], "seam-b");
        }
        let ring = state
            .terminal_events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(
            ring.iter().all(|(_, frame)| frame["sessionId"] == b_sid),
            "the derived ring must hold only the last reader's session"
        );
    }

    #[tokio::test]
    async fn terminal_event_paging_over_one_session_is_gapless_and_duplicate_free() {
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        let state = test_state(deps);
        let manager = state.deps.session.clone();
        let a_sid = create_session(&manager, "term-page-a");
        mark_recovered(&state, &a_sid);
        seed_terminal(&manager, &a_sid, "page-a1", 41201, false);
        seed_terminal(&manager, &a_sid, "page-a2", 41202, true);

        let mut seen: Vec<u64> = Vec::new();
        let mut cursor = 0u64;
        loop {
            let (status, page) = read_page(&state, &a_sid, Some(cursor), Some(2)).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(page["sessionId"], a_sid);
            let events = page["events"].as_array().unwrap();
            assert!(!events.is_empty(), "page after {cursor} must not be empty");
            for event in events {
                let id = event["id"].as_u64().unwrap();
                assert!(
                    seen.last().is_none_or(|last| *last < id),
                    "ids must strictly ascend without duplicates: {seen:?} then {id}"
                );
                seen.push(id);
                assert_eq!(event["sessionId"], a_sid);
            }
            if page["hasMore"].as_bool().unwrap() {
                assert_eq!(page["nextCursor"].as_u64().unwrap(), *seen.last().unwrap());
                cursor = page["nextCursor"].as_u64().unwrap();
            } else {
                assert!(page["nextCursor"].is_null());
                break;
            }
        }
        assert_eq!(seen.len(), 5, "no duplicates, no gaps: {seen:?}");

        let (_, full) = read_page(&state, &a_sid, None, Some(200)).await;
        assert_eq!(event_ids(&full), seen, "cursor walk equals one full read");
    }

    #[tokio::test]
    async fn terminal_event_after_cursor_is_strictly_exclusive_at_both_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        let state = test_state(deps);
        let manager = state.deps.session.clone();
        let a_sid = create_session(&manager, "term-after-a");
        mark_recovered(&state, &a_sid);
        seed_terminal(&manager, &a_sid, "after-a1", 41301, false);
        seed_terminal(&manager, &a_sid, "after-a2", 41302, false);

        let (_, full) = read_page(&state, &a_sid, None, None).await;
        let ids = event_ids(&full);
        assert_eq!(ids.len(), 4, "{full}");
        let first = ids[0];
        let last = *ids.last().unwrap();

        let (_, after_first) = read_page(&state, &a_sid, Some(first), None).await;
        assert_eq!(event_ids(&after_first), ids[1..], "after is exclusive");
        let (_, before_first) = read_page(&state, &a_sid, Some(first - 1), None).await;
        assert_eq!(
            event_ids(&before_first),
            ids,
            "below the first id returns all"
        );

        let (status, after_last) = read_page(&state, &a_sid, Some(last), None).await;
        assert_eq!(status, StatusCode::OK);
        assert!(event_ids(&after_last).is_empty());
        assert_eq!(after_last["hasMore"], false);
        assert!(after_last["nextCursor"].is_null());

        let (_, after_max) = read_page(&state, &a_sid, Some(u64::MAX), None).await;
        assert!(event_ids(&after_max).is_empty());
    }

    #[tokio::test]
    async fn terminal_event_reads_for_hostile_and_unknown_sessions_are_typed() {
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        let state = test_state(deps);
        let manager = state.deps.session.clone();
        let a_sid = create_session(&manager, "term-typed-a");
        let empty_sid = create_session(&manager, "term-typed-empty");
        mark_recovered(&state, &a_sid);
        mark_recovered(&state, &empty_sid);
        seed_terminal(&manager, &a_sid, "typed-a", 41401, false);

        for path in ["abc", "0", "-1", "1.5", "99999999999999999999"] {
            let (status, _) = read_page(&state, path, None, None).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
        }
        let (status, body) = read_page(&state, "999999", None, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], "not_found", "{body}");

        // A valid session that owns no terminal is an empty page, never a
        // projection of another session's frames.
        let (status, empty) = read_page(&state, &empty_sid, None, None).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(empty["sessionId"], empty_sid);
        assert!(empty["events"].as_array().unwrap().is_empty(), "{empty}");
    }

    /// Adversarial concurrency: many A/B reads race the ONE derived ring.
    /// Every response must be self-consistent and carry only its own
    /// session's frames, whatever the interleaving.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_terminal_event_reads_never_cross_session_frames() {
        let dir = tempfile::tempdir().unwrap();
        let deps = test_deps(dir.path());
        let state = test_state(deps);
        let manager = state.deps.session.clone();
        let a_sid = create_session(&manager, "term-conc-a");
        let b_sid = create_session(&manager, "term-conc-b");
        mark_recovered(&state, &a_sid);
        mark_recovered(&state, &b_sid);
        seed_terminal(&manager, &a_sid, "conc-a1", 41501, false);
        seed_terminal(&manager, &a_sid, "conc-a2", 41502, true);
        seed_terminal(&manager, &b_sid, "conc-b1", 41503, false);
        seed_terminal(&manager, &b_sid, "conc-b2", 41504, true);

        let mut tasks = Vec::new();
        for _ in 0..8 {
            for (sid, prefix) in [(&a_sid, "conc-a"), (&b_sid, "conc-b")] {
                let state = state.clone();
                let sid = sid.clone();
                let prefix = prefix.to_string();
                tasks.push(tokio::spawn(async move {
                    let (status, page) = read_page(&state, &sid, None, None).await;
                    (status, page, sid, prefix)
                }));
            }
        }
        for task in tasks {
            let (status, page, sid, prefix) = task.await.unwrap();
            assert_eq!(status, StatusCode::OK);
            assert_eq!(page["sessionId"], sid);
            let events = page["events"].as_array().unwrap();
            assert!(!events.is_empty());
            for event in events {
                assert_eq!(event["sessionId"], sid, "cross-session frame: {page}");
                assert!(
                    event["terminalId"].as_str().unwrap().starts_with(&prefix),
                    "foreign terminal in {page}"
                );
            }
        }
    }
}
