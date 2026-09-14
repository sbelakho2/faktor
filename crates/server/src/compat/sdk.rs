//! SDK-shaped legacy REST surface (`/session/...`, `/permission/...`,
//! `/global/...`, `/question/...`, `/network/...`, `/config/...`, PTY and auth).

use crate::auth::ServerPassword;
use crate::permission::PendingPermission;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{sse::Event, IntoResponse, Response, Sse};
use axum::Json;
use faktor_core::capability::PermissionDecision;
use faktor_protocol::error::ApiError;
use futures_util::stream::Stream;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use super::v756::sdk_invalid_request;
use super::{submit_and_run, turn_machine_busy};
use crate::api::{AppState, ServerDeps};
use crate::native::{
    api_err, api_error_json, authed, exec_error_response, not_found, parse_session_id,
    wire_refused, wire_status,
};
use faktor_protocol::v756::*;

pub(crate) const MAX_CONFIG_BYTES: usize = 1024 * 1024;

pub(crate) const HEARTBEAT_SECS: u64 = 15;

pub(crate) const POLL_INTERVAL_MS: u64 = 100;

/// Journal catch-up page: one bounded page of SSE frames per poll, so a
/// reconnect against a huge journal never loads it all into memory at once.
pub(crate) const EVENT_CATCHUP_PAGE: u64 = 256;

pub(crate) async fn hello(State(state): State<AppState>) -> Response {
    Json(HelloResponse {
        ok: true,
        version: state.deps.version.clone(),
        protocol: faktor_core::PROTOCOL_V756.to_string(),
        auth_required: true,
        providers: state.deps.agent.deps().providers.ids(),
    })
    .into_response()
}

/// `GET /global/health` — auth-required (the frozen v7.5.6 client
/// authenticates every request, this one included). Serves the SDK's
/// declared `{healthy: true, version}` fields ADDITIVELY next to the frozen
/// `{ok, protocol}` aliases (the legacy fixtures/consumers read those).
pub(crate) async fn health(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    Json(serde_json::json!({
        "ok": true,
        "healthy": true,
        "version": state.deps.version.clone(),
        "protocol": faktor_core::PROTOCOL_V756.to_string(),
    }))
    .into_response()
}

pub(crate) async fn create_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateSessionRequest>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let workspace = req.workspace.clone().unwrap_or_else(|| ".".into());
    let ws = match state.deps.session.create_workspace(&workspace) {
        Ok(ws) => ws,
        Err(e) => return api_err(&e),
    };
    let title = req.title.clone().unwrap_or_else(|| "New session".into());
    match state
        .deps
        .session
        .create_session(ws, &title, &req.provider, &req.model)
    {
        Ok(row) => {
            // Shadow 409 root cause: register the workspace's owner worktree
            // and adopt the session onto it AT CREATION, so every session
            // this surface creates (the VS Code extension's path) carries a
            // durable workspace/worktree identity and shadowed mutating
            // runs work without a standalone-default 409. Idempotent;
            // sessions created before this existed self-heal at task-run
            // start.
            if let Err(e) = state.deps.session.ensure_owner_worktree(row.id()) {
                return api_err(&e);
            }
            match row.row() {
                Ok(row_data) => Json(CreateSessionResponse {
                    id: row_data.id.to_string(),
                    title,
                    created_ms: row_data.created_ms,
                })
                .into_response(),
                Err(e) => api_err(&e),
            }
        }
        Err(e) => api_err(&e),
    }
}

pub(crate) async fn list_sessions(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    match state.deps.session.list_sessions(None) {
        Ok(rows) => Json(serde_json::json!({
            "sessions": rows.iter().map(|r| {
                let title = r.title().unwrap_or_default();
                let provider = r.provider().unwrap_or_default();
                let model = r.model().unwrap_or_default();
                let state = r.state().map(|s| s.label()).unwrap_or("unknown");
                serde_json::json!({
                    "id": r.id().to_string(),
                    "title": title,
                    "provider": provider,
                    "model": model,
                    "state": state,
                })
            }).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => api_err(&e),
    }
}

pub(crate) async fn session_state(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let sid = match parse_session_id(&id) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::from_u16(e.http_status).unwrap(),
                Json(e.to_json()),
            )
                .into_response()
        }
    };
    let handle = match state.deps.session.get_session(sid) {
        Ok(Some(h)) => h,
        Ok(None) => {
            let e = ApiError {
                code: "not_found",
                message: format!("session {sid}"),
                http_status: 404,
                retryable: false,
            };
            return (StatusCode::NOT_FOUND, Json(e.to_json())).into_response();
        }
        Err(e) => return api_err(&e),
    };
    match handle.session_state_view() {
        Ok(view) => Json(view).into_response(),
        Err(e) => api_err(&e),
    }
}

pub(crate) async fn messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<MessagesQuery>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let sid = match parse_session_id(&id) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::from_u16(e.http_status).unwrap(),
                Json(e.to_json()),
            )
                .into_response()
        }
    };
    let handle = match state.deps.session.get_session(sid) {
        Ok(Some(h)) => h,
        Ok(None) => {
            let e = ApiError {
                code: "not_found",
                message: format!("session {sid}"),
                http_status: 404,
                retryable: false,
            };
            return (StatusCode::NOT_FOUND, Json(e.to_json())).into_response();
        }
        Err(e) => return api_err(&e),
    };
    match handle.messages_page(q.before, q.limit) {
        Ok(page) => Json(page).into_response(),
        Err(e) => api_err(&e),
    }
}

pub(crate) async fn prompt(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<PromptRequest>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    if req.prompt.trim().is_empty() {
        let e = ApiError {
            code: "malformed",
            message: "prompt must not be empty".into(),
            http_status: 400,
            retryable: false,
        };
        return (StatusCode::BAD_REQUEST, Json(e.to_json())).into_response();
    }
    let sid = match parse_session_id(&id) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::from_u16(e.http_status).unwrap(),
                Json(e.to_json()),
            )
                .into_response()
        }
    };
    // Unknown sessions are 404, never a phantom 200 (audit round 8).
    match state.deps.session.get_session(sid) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return wire_status(not_found(&format!("session {sid}")));
        }
        Err(e) => return api_err(&e),
    }
    let prompt_text = req.prompt.clone();
    let files = req.files.clone();
    // Synchronous submission so the response carries the TRUE queued state
    // and the REAL operation id (audit: op_id was hardcoded "turn"). The
    // compat surface imposes no mutation policy: the prompt travels the ONE
    // executor path and its mutating drive executes in an isolated
    // candidate (the daemon default).
    let receipt = match submit_and_run(&state, sid, &prompt_text, &files, None).await {
        Ok(r) => r,
        Err(e) => return exec_error_response(&e),
    };
    Json(PromptResponse {
        op_id: receipt.op_id.to_string(),
        accepted: true,
        queued: receipt.queued,
    })
    .into_response()
}

pub(crate) async fn abort(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let sid = match parse_session_id(&id) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::from_u16(e.http_status).unwrap(),
                Json(e.to_json()),
            )
                .into_response()
        }
    };
    match state.deps.agent.abort(sid) {
        Ok(ops) => Json(AbortResponse {
            aborted: ops.iter().map(|o| o.to_string()).collect(),
        })
        .into_response(),
        Err(e) => api_err(&e),
    }
}

pub(crate) async fn resolve_permission(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<PermissionDecisionRequest>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    match resolve_permission_body(&state.deps, &id, &req.decision) {
        Ok(()) => Json(PermissionDecisionResponse { ok: true }).into_response(),
        Err(e) => (
            StatusCode::from_u16(e.http_status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(e.to_json()),
        )
            .into_response(),
    }
}

/// `POST /permission/reply` — SDK form of the same resolution.
pub(crate) async fn permission_reply(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<PermissionDecisionRequest>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    match resolve_permission_body(&state.deps, &req.permission_id, &req.decision) {
        Ok(()) => Json(PermissionDecisionResponse { ok: true }).into_response(),
        Err(e) => (
            StatusCode::from_u16(e.http_status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(e.to_json()),
        )
            .into_response(),
    }
}

pub(crate) fn resolve_permission_body(
    deps: &ServerDeps,
    permission_id: &str,
    decision: &str,
) -> Result<(), ApiError> {
    let pid: i64 = match permission_id.parse() {
        Ok(p) if p > 0 => p,
        _ => {
            return Err(ApiError {
                code: "malformed",
                message: format!("invalid permission id {permission_id:?}"),
                http_status: 400,
                retryable: false,
            });
        }
    };
    let decision = match decision {
        "allow" => PermissionDecision::Allow,
        "deny" => PermissionDecision::Deny,
        other => {
            return Err(ApiError {
                code: "malformed",
                message: format!("invalid decision {other:?}"),
                http_status: 400,
                retryable: false,
            });
        }
    };
    if !deps.permissions.resolve(pid, decision) {
        return Err(ApiError {
            code: "conflict",
            message: format!("permission {pid} unknown or already resolved"),
            http_status: 409,
            retryable: false,
        });
    }
    Ok(())
}

/// `GET /permission/list?session_id=` — pending permission requests.
pub(crate) async fn permission_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<SdkSessionQuery>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let views = state.deps.permissions.pending_views();
    let permissions: Vec<PermissionListEntry> = views
        .iter()
        .filter(|v| {
            q.session_id
                .as_ref()
                .is_none_or(|sid| v.session_id.to_string() == *sid)
        })
        .map(|v| PermissionListEntry {
            id: v.id.to_string(),
            session_id: v.session_id.to_string(),
            capability: v.capability.clone(),
            detail: v.detail.clone(),
        })
        .collect();
    Json(PermissionListResponse { permissions }).into_response()
}

// ------------------------------------------------------------------ SDK handlers

/// `POST /session/prompt` — session_id in the body.
pub(crate) async fn sdk_prompt(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SdkPromptRequest>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    if req.prompt.trim().is_empty() {
        let e = ApiError {
            code: "malformed",
            message: "prompt must not be empty".into(),
            http_status: 400,
            retryable: false,
        };
        return (StatusCode::BAD_REQUEST, Json(e.to_json())).into_response();
    }
    let sid = match parse_session_id(&req.session_id) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::from_u16(e.http_status).unwrap(),
                Json(e.to_json()),
            )
                .into_response();
        }
    };
    match state.deps.session.get_session(sid) {
        Ok(Some(_)) => {}
        Ok(None) => {
            let e = ApiError {
                code: "not_found",
                message: format!("session {sid}"),
                http_status: 404,
                retryable: false,
            };
            return (StatusCode::NOT_FOUND, Json(e.to_json())).into_response();
        }
        Err(e) => return api_err(&e),
    }
    // Unknown sessions are 404, never a phantom 200 (audit round 8).
    match state.deps.session.get_session(sid) {
        Ok(Some(_)) => {}
        Ok(None) => {
            return wire_status(not_found(&format!("session {sid}")));
        }
        Err(e) => return api_err(&e),
    }
    let prompt_text = req.prompt.clone();
    let files = req.files.clone();
    // Synchronous submission so the response carries the TRUE queued state
    // and the REAL operation id (audit: op_id was hardcoded "turn"). The
    // same no-policy translation as `prompt`: mutating drives isolate.
    let receipt = match submit_and_run(&state, sid, &prompt_text, &files, None).await {
        Ok(r) => r,
        Err(e) => return exec_error_response(&e),
    };
    Json(PromptResponse {
        op_id: receipt.op_id.to_string(),
        accepted: true,
        queued: receipt.queued,
    })
    .into_response()
}

/// `POST /session/abort` — session_id in the body.
pub(crate) async fn sdk_abort(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SdkAbortRequest>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let sid = match parse_session_id(&req.session_id) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::from_u16(e.http_status).unwrap(),
                Json(e.to_json()),
            )
                .into_response();
        }
    };
    match state.deps.session.get_session(sid) {
        Ok(Some(_)) => {}
        Ok(None) => {
            let e = ApiError {
                code: "not_found",
                message: format!("session {sid}"),
                http_status: 404,
                retryable: false,
            };
            return (StatusCode::NOT_FOUND, Json(e.to_json())).into_response();
        }
        Err(e) => return api_err(&e),
    }
    // Targeted abort (audit round 8): the request op_id is honored — one
    // queued prompt can be killed without touching the active turn.
    let target = match &req.op_id {
        Some(raw) => match raw.parse::<u64>() {
            Ok(v) => Some(faktor_core::id::OpId::new(v)),
            Err(_) => {
                return wire_refused(&format!("invalid op_id {raw:?}"));
            }
        },
        None => None,
    };
    match state.deps.agent.abort_op(sid, target) {
        Ok(ops) => Json(AbortResponse {
            aborted: ops.iter().map(|o| o.to_string()).collect(),
        })
        .into_response(),
        Err(e) => api_err(&e),
    }
}

/// `GET /session/messages?session_id=&before=&limit=`
pub(crate) async fn sdk_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<SdkMessagesQuery>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let sid = match parse_session_id(&q.session_id) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::from_u16(e.http_status).unwrap(),
                Json(e.to_json()),
            )
                .into_response();
        }
    };
    let handle = match state.deps.session.get_session(sid) {
        Ok(Some(h)) => h,
        Ok(None) => {
            let e = ApiError {
                code: "not_found",
                message: format!("session {sid}"),
                http_status: 404,
                retryable: false,
            };
            return (StatusCode::NOT_FOUND, Json(e.to_json())).into_response();
        }
        Err(e) => return api_err(&e),
    };
    match handle.messages_page(q.before, q.limit) {
        Ok(page) => Json(page).into_response(),
        Err(e) => api_err(&e),
    }
}

/// `GET /session/state?session_id=`
pub(crate) async fn sdk_session_state(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<SdkStateQuery>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let sid = match parse_session_id(&q.session_id) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::from_u16(e.http_status).unwrap(),
                Json(e.to_json()),
            )
                .into_response();
        }
    };
    let handle = match state.deps.session.get_session(sid) {
        Ok(Some(h)) => h,
        Ok(None) => {
            let e = ApiError {
                code: "not_found",
                message: format!("session {sid}"),
                http_status: 404,
                retryable: false,
            };
            return (StatusCode::NOT_FOUND, Json(e.to_json())).into_response();
        }
        Err(e) => return api_err(&e),
    };
    match handle.session_state_view() {
        Ok(view) => Json(view).into_response(),
        Err(e) => api_err(&e),
    }
}

// ------------------------------------------------- v7.5.6 wire surface (subset)
// The routes the frozen v7.5.6 extension actually calls. Path params are
// wire session ids (numeric strings): non-numeric → 400, unknown → 404.

/// The `x-faktor-directory` header value (the workspace root the extension
/// operates on). Bounded by the mapper.
pub(crate) fn directory_header(headers: &HeaderMap) -> Option<&str> {
    headers
        .get("x-faktor-directory")
        .and_then(|v| v.to_str().ok())
}

/// `POST /pty/create` — spawn a session-scoped interactive terminal.
/// Body: {command, args?, cwd?, rows?, cols?}. Returns {pty_id, pid}.
/// Non-Unix platforms refuse honestly (ConPTY/Job Objects are the declared
/// platform blocker).
pub(crate) async fn pty_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<serde_json::Value>>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Some(Json(body)) = body else {
        return wire_refused("pty/create requires a body");
    };
    let command = match body.get("command").and_then(|c| c.as_str()) {
        Some(c) if !c.is_empty() && c.len() <= 4096 => c.to_string(),
        _ => return wire_refused("pty/create requires a non-empty command"),
    };
    let args: Vec<String> = body
        .get("args")
        .and_then(|a| a.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.to_string())
                .collect()
        })
        .unwrap_or_default();
    if args.iter().any(|a| a.len() > 4096) {
        return wire_refused("pty/create args are oversized");
    }
    let rows = body.get("rows").and_then(|r| r.as_u64()).unwrap_or(24);
    let cols = body.get("cols").and_then(|c| c.as_u64()).unwrap_or(80);
    let rows = u16::try_from(rows).unwrap_or(24).max(1);
    let cols = u16::try_from(cols).unwrap_or(80).max(1);
    let cfg = faktor_pty::PtyConfig {
        command,
        args,
        cwd: body
            .get("cwd")
            .and_then(|c| c.as_str())
            .map(|s| s.to_string()),
        env: faktor_pty::EnvSpec::default_baseline(),
        rows,
        cols,
    };
    // Spawning is quick (non-blocking master) but do it off the async
    // thread to be safe with process setup.
    let pty = match tokio::task::spawn_blocking(move || faktor_pty::Pty::spawn(&cfg)).await {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => {
            return (StatusCode::BAD_REQUEST, Json(api_error_json(&e))).into_response();
        }
        Err(_) => return wire_refused("pty spawn task failed"),
    };
    let id = state
        .next_pty_id
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let pid = pty.pid();
    state.ptys.lock().unwrap().insert(id, pty);
    Json(serde_json::json!({ "ok": true, "pty_id": id.to_string(), "pid": pid })).into_response()
}

// --------------------------------------------------- SDK-exact PTY routes
// The unmodified `@kilocode/sdk@7.5.6` client calls `POST /pty` (create,
// returns the `Pty` object), `DELETE /pty/{ptyID}` (remove, returns
// boolean), `GET /pty` (list), `GET/PATCH /pty/{ptyID}` and
// `POST /pty/{ptyID}/connect-token`. This slice registers the create/remove
// pair over the daemon's REAL PTY registry (`AppState.ptys` — the same
// process authority the native session-owned terminal surface drives); the
// remaining routes would need terminal metadata the registry deliberately
// does not keep (command/args/cwd/title are not durable), so they stay
// unregistered rather than projecting fabricated rows. The SDK's create
// body carries no session id, so an SDK-created terminal has no durable
// session ownership row (the native session-scoped route mints those when a
// session is known); `sessionID` is therefore omitted from the projection,
// never invented.

/// Bound of one SDK PTY create field (mirrors the native terminal spawn).
pub(crate) const MAX_SDK_PTY_FIELD_BYTES: usize = 4096;
/// Bound of one SDK PTY create arg list.
pub(crate) const MAX_SDK_PTY_ARGS: usize = 256;
/// Bound of one SDK PTY env name allowlist.
pub(crate) const MAX_SDK_PTY_ENV_NAMES: usize = 64;

/// The default command of an SDK `pty.create` without one: the platform's
/// documented interactive shell. Never a fabricated path.
fn default_pty_command() -> String {
    if cfg!(windows) {
        "cmd.exe".to_string()
    } else {
        std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string())
    }
}

/// `POST /pty` — the SDK `pty.create` route over the daemon's real PTY
/// registry. Body (all optional, exactly the SDK's declared set):
/// `{command?, args?, cwd?, title?, env?, size?{rows,cols}}`. The response
/// is the SDK `Pty` object built from the values the spawn actually used:
/// `status` is the live child state at response time, `exitCode` is omitted
/// (the PTY backend exposes no exit-code authority — never fabricated), and
/// `sessionID` is omitted because the SDK body carries no session id and no
/// ownership row is minted. `env` VALUES never cross the daemon's ONE env
/// authority: the map's NAMES become the spawn allowlist and the values are
/// ignored (documented in docs/wire-compat.md).
pub(crate) async fn sdk_pty_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<serde_json::Value>>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let body = body
        .map(|Json(v)| v)
        .unwrap_or_else(|| serde_json::json!({}));
    let Some(obj) = body.as_object() else {
        return sdk_invalid_request("pty create body must be a JSON object");
    };
    let command = match obj.get("command").and_then(|v| v.as_str()) {
        Some(c) if !c.is_empty() && c.len() <= MAX_SDK_PTY_FIELD_BYTES && !c.contains('\0') => {
            c.to_string()
        }
        Some(_) => {
            return sdk_invalid_request(
                "pty command must be non-empty and at most 4096 bytes without NUL",
            )
        }
        None => default_pty_command(),
    };
    let args: Vec<String> = match obj.get("args") {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(serde_json::Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                let Some(text) = item.as_str() else {
                    return sdk_invalid_request("pty args must be an array of strings");
                };
                out.push(text.to_string());
            }
            out
        }
        Some(_) => return sdk_invalid_request("pty args must be an array of strings"),
    };
    if args.len() > MAX_SDK_PTY_ARGS
        || args
            .iter()
            .any(|a| a.len() > MAX_SDK_PTY_FIELD_BYTES || a.contains('\0'))
    {
        return sdk_invalid_request("pty args are oversized");
    }
    let cwd: Option<String> = match obj.get("cwd") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(s))
            if !s.is_empty() && s.len() <= MAX_SDK_PTY_FIELD_BYTES && !s.contains('\0') =>
        {
            Some(s.clone())
        }
        Some(_) => return sdk_invalid_request("pty cwd is invalid or oversized"),
    };
    let title = match obj.get("title").and_then(|v| v.as_str()) {
        Some(t) if t.len() <= MAX_SDK_PTY_FIELD_BYTES && !t.contains('\0') => t.to_string(),
        Some(_) => return sdk_invalid_request("pty title is oversized"),
        None => command.clone(),
    };
    // env: {name: value}; only the NAMES feed the env authority.
    let env_names: Vec<String> = match obj.get("env") {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(serde_json::Value::Object(map)) => {
            let mut names = Vec::with_capacity(map.len());
            for (name, value) in map {
                if !value.is_string() {
                    return sdk_invalid_request("pty env values must be strings");
                }
                if name.is_empty() || name.len() > 256 || name.contains('\0') || name.contains('=')
                {
                    return sdk_invalid_request("pty env name is invalid or oversized");
                }
                names.push(name.clone());
            }
            names
        }
        Some(_) => return sdk_invalid_request("pty env must be an object of string values"),
    };
    if env_names.len() > MAX_SDK_PTY_ENV_NAMES {
        return sdk_invalid_request("pty env allowlist exceeds 64 names");
    }
    let (rows, cols) = match obj.get("size") {
        None | Some(serde_json::Value::Null) => (24u16, 80u16),
        Some(serde_json::Value::Object(size)) => {
            let rows = size
                .get("rows")
                .and_then(|v| v.as_u64())
                .and_then(|v| u16::try_from(v).ok())
                .unwrap_or(24)
                .max(1);
            let cols = size
                .get("cols")
                .and_then(|v| v.as_u64())
                .and_then(|v| u16::try_from(v).ok())
                .unwrap_or(80)
                .max(1);
            (rows, cols)
        }
        Some(_) => return sdk_invalid_request("pty size must be an object"),
    };
    let env = if env_names.is_empty() {
        faktor_pty::EnvSpec::default_baseline()
    } else {
        // The name allowlist rides the ONE env authority: values never
        // cross, the secret deny-set applies inside `resolve()`.
        faktor_pty::EnvSpec::Allowlisted(env_names)
    };
    let cfg = faktor_pty::PtyConfig {
        command: command.clone(),
        args: args.clone(),
        cwd: cwd.clone(),
        env,
        rows,
        cols,
    };
    let pty = match tokio::task::spawn_blocking(move || faktor_pty::Pty::spawn(&cfg)).await {
        Ok(Ok(p)) => p,
        Ok(Err(e)) => return (StatusCode::BAD_REQUEST, Json(api_error_json(&e))).into_response(),
        Err(_) => return wire_refused("pty spawn task failed"),
    };
    let id = state
        .next_pty_id
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let pid = pty.pid();
    let status = if pty.is_alive() { "running" } else { "exited" };
    let effective_cwd = cwd.unwrap_or_else(|| {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| ".".to_string())
    });
    state.ptys.lock().unwrap().insert(id, pty);
    Json(serde_json::json!({
        "id": id.to_string(),
        "title": title,
        "command": command,
        "args": args,
        "cwd": effective_cwd,
        "status": status,
        "pid": pid,
    }))
    .into_response()
}

/// `DELETE /pty/{ptyID}` — the SDK `pty.remove` route: terminate the real
/// child process tree, drop the registry row and answer the SDK-declared
/// boolean. An unknown (or already removed) id is the SDK-declared
/// `404 PtyNotFoundError`; the kill is bounded by the PTY backend's own
/// shutdown (SIGTERM → grace → SIGKILL → reader join on unix, job close on
/// Windows).
pub(crate) async fn sdk_pty_remove(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(pty_id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let id: u64 = match pty_id.parse() {
        Ok(id) if id > 0 => id,
        _ => return sdk_invalid_request("invalid pty id"),
    };
    state.terminal_owners.lock().unwrap().remove(&id);
    match state.ptys.lock().unwrap().remove(&id) {
        Some(mut pty) => {
            pty.kill();
            Json(true).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "_tag": "PtyNotFoundError",
                "ptyID": id.to_string(),
                "message": format!("pty {id} unknown"),
            })),
        )
            .into_response(),
    }
}

/// `POST /pty/update` — write input and/or resize. Body: {pty_id,
/// data?, rows?, cols?}. A pty that no longer exists is a loud 404.
pub(crate) async fn pty_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<serde_json::Value>>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Some(Json(body)) = body else {
        return wire_refused("pty/update requires a body");
    };
    let id = match body
        .get("pty_id")
        .and_then(|i| i.as_str())
        .and_then(|s| s.parse::<u64>().ok())
    {
        Some(id) => id,
        None => return wire_refused("pty/update requires pty_id"),
    };
    let ptys = state.ptys.lock().unwrap();
    let pty = match ptys.get(&id) {
        Some(p) => p,
        None => return wire_status(not_found(&format!("pty {id}"))),
    };
    if let Some(data) = body.get("data").and_then(|d| d.as_str()) {
        if let Err(e) = pty.write_all(data.as_bytes()) {
            return api_err(&e);
        }
    }
    if let (Some(r), Some(c)) = (
        body.get("rows").and_then(|v| v.as_u64()),
        body.get("cols").and_then(|v| v.as_u64()),
    ) {
        let r = u16::try_from(r).unwrap_or(24).max(1);
        let c = u16::try_from(c).unwrap_or(80).max(1);
        if let Err(e) = pty.resize(r, c) {
            return api_err(&e);
        }
    }
    Json(serde_json::json!({ "ok": true })).into_response()
}

/// `POST /pty/remove` — terminate and close. Body: {pty_id}. Idempotent
/// for unknown ids (the terminal is already gone).
pub(crate) async fn pty_remove(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Option<Json<serde_json::Value>>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Some(Json(body)) = body else {
        return wire_refused("pty/remove requires a body");
    };
    let id = match body
        .get("pty_id")
        .and_then(|i| i.as_str())
        .and_then(|s| s.parse::<u64>().ok())
    {
        Some(id) => id,
        None => return wire_refused("pty/remove requires pty_id"),
    };
    if let Some(mut pty) = state.ptys.lock().unwrap().remove(&id) {
        pty.kill();
    }
    Json(serde_json::json!({ "ok": true })).into_response()
}

/// `GET /pty/{id}/output` — snapshot available output (does NOT drain).
pub(crate) async fn pty_output(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let id = match id.parse::<u64>() {
        Ok(id) => id,
        Err(_) => return wire_refused("invalid pty id"),
    };
    let ptys = state.ptys.lock().unwrap();
    let pty = match ptys.get(&id) {
        Some(p) => p,
        None => return wire_status(not_found(&format!("pty {id}"))),
    };
    let out = pty.snapshot();
    let text = String::from_utf8_lossy(&out).into_owned();
    Json(serde_json::json!({ "ok": true, "output": text, "alive": pty.is_alive() })).into_response()
}

/// `POST /global/dispose` and `POST /instance/dispose` — stop everything:
/// every supervised process owned by a session is killed via the agent
/// (which owns the supervisor), then each session is durably ended
/// (SessionEnded journal event + lifecycle Closed). The SDK declares
/// `200: boolean`; the boolean is the honest overall outcome (an incomplete
/// dispose is an error response, never `false`).
pub(crate) async fn dispose_all_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let handles = match state.deps.session.list_sessions(None) {
        Ok(h) => h,
        Err(e) => return api_err(&e),
    };
    for handle in handles {
        let id = handle.id();
        // Idempotent: sessions that are already durably ended are skipped
        // (a second dispose must still answer true).
        let row = match handle.row() {
            Ok(r) => r,
            Err(e) => return api_err(&e),
        };
        if row.lifecycle.is_terminal() {
            continue;
        }
        // Cancel any live turn first so the durable end transition is legal
        // from the landing state.
        let _ = state.deps.agent.abort(id);
        if let Err(e) = state.deps.agent.end_session(id) {
            if e.kind == faktor_core::error::ErrorKind::NotFound {
                continue; // vanished mid-dispose
            }
            return wire_refused(&format!("dispose incomplete: session {id}: {}", e.message));
        }
    }
    Json(true).into_response()
}

/// `POST /instance/reload` — re-run the daemon's crash recovery sweep over
/// every session (idempotent) and acknowledge with the SDK's declared
/// boolean.
pub(crate) async fn instance_reload(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    match state.deps.agent.recover() {
        Ok(_reports) => Json(true).into_response(),
        Err(e) => api_err(&e),
    }
}

/// Upper bound on an `auth.set` password (bounded everything).
pub(crate) const MAX_AUTH_PASSWORD_BYTES: usize = 1024;

/// `POST /auth/set` — rotate the server password. `password` absent rotates
/// to a fresh random secret; either way the response carries the new
/// effective secret so the client can keep authenticating. Every other
/// endpoint immediately checks the new secret (old credentials → 401).
pub(crate) async fn auth_set(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<AuthSetRequest>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let password = match req.password {
        Some(p) if p.is_empty() || p.len() > MAX_AUTH_PASSWORD_BYTES => {
            let e = ApiError {
                code: "malformed",
                message: format!(
                    "password must be non-empty and at most {MAX_AUTH_PASSWORD_BYTES} bytes"
                ),
                http_status: 400,
                retryable: false,
            };
            return (StatusCode::BAD_REQUEST, Json(e.to_json())).into_response();
        }
        Some(p) => p,
        None => ServerPassword::generate().as_str().to_string(),
    };
    *state.auth.write().unwrap() = Some(ServerPassword::new(password.clone()));
    Json(AuthSetResponse { ok: true, password }).into_response()
}

/// `POST /auth/remove` — drop the runtime override: authentication returns
/// to the startup env password (`FAKTOR_SERVER_PASSWORD` at daemon start).
pub(crate) async fn auth_remove(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    *state.auth.write().unwrap() = None;
    Json(OkResponse { ok: true }).into_response()
}

/// The frozen capability tag a network permission carries.
pub(crate) const NETWORK_CAPABILITY: &str = "network";

pub(crate) fn pending_permission_json(p: &PendingPermission) -> serde_json::Value {
    serde_json::json!({
        "id": p.id.to_string(),
        "session_id": p.session_id.to_string(),
        "capability": p.capability,
        "detail": p.detail,
    })
}

/// Resolve one pending permission by wire id through the permission
/// requester. `class`: `Some("network")` restricts to network requests,
/// `Some("question")` to everything else.
pub(crate) fn resolve_pending_permission(
    deps: &ServerDeps,
    raw_id: &str,
    decision: &str,
    class: &str,
) -> Result<(), ApiError> {
    let id: i64 = match raw_id.parse() {
        Ok(p) if p > 0 => p,
        _ => {
            return Err(ApiError {
                code: "not_found",
                message: format!("{} {raw_id} unknown", class_kind(class)),
                http_status: 404,
                retryable: false,
            })
        }
    };
    let decision = match decision {
        "allow" => PermissionDecision::Allow,
        "deny" => PermissionDecision::Deny,
        other => {
            return Err(ApiError {
                code: "malformed",
                message: format!("invalid decision {other:?}"),
                http_status: 400,
                retryable: false,
            })
        }
    };
    // The permission must exist AND belong to the requested class: an id
    // from the other class is unknown HERE (it stays resolvable through its
    // own surface).
    let pending = deps
        .permissions
        .pending_views()
        .into_iter()
        .find(|v| v.id == id);
    let Some(view) = pending else {
        return Err(ApiError {
            code: "not_found",
            message: format!("{} {raw_id} unknown", class_kind(class)),
            http_status: 404,
            retryable: false,
        });
    };
    let is_network = view.capability == NETWORK_CAPABILITY;
    match class {
        "network" if !is_network => {
            return Err(ApiError {
                code: "not_found",
                message: format!("network {raw_id} unknown"),
                http_status: 404,
                retryable: false,
            })
        }
        "question" if is_network => {
            return Err(ApiError {
                code: "not_found",
                message: format!("question {raw_id} unknown"),
                http_status: 404,
                retryable: false,
            })
        }
        _ => {}
    }
    if !deps.permissions.resolve(id, decision) {
        return Err(ApiError {
            code: "conflict",
            message: format!("{} {raw_id} unknown or already resolved", class_kind(class)),
            http_status: 409,
            retryable: false,
        });
    }
    Ok(())
}

pub(crate) fn class_kind(class: &str) -> &'static str {
    if class == "network" {
        "network"
    } else {
        "question"
    }
}

pub(crate) async fn question_reply(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<QuestionReplyRequest>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    if req.question_id.trim().is_empty() || req.decision.trim().is_empty() {
        let e = ApiError {
            code: "malformed",
            message: "question_id and decision are required".into(),
            http_status: 400,
            retryable: false,
        };
        return (StatusCode::BAD_REQUEST, Json(e.to_json())).into_response();
    }
    match resolve_pending_permission(&state.deps, &req.question_id, &req.decision, "question") {
        Ok(()) => Json(PermissionDecisionResponse { ok: true }).into_response(),
        Err(e) => (
            StatusCode::from_u16(e.http_status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(e.to_json()),
        )
            .into_response(),
    }
}

/// `POST /question/reject` — deny is the whole semantics (never allow).
pub(crate) async fn question_reject(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<QuestionRejectRequest>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    if req.question_id.trim().is_empty() {
        let e = ApiError {
            code: "malformed",
            message: "question_id is required".into(),
            http_status: 400,
            retryable: false,
        };
        return (StatusCode::BAD_REQUEST, Json(e.to_json())).into_response();
    }
    match resolve_pending_permission(&state.deps, &req.question_id, "deny", "question") {
        Ok(()) => Json(PermissionDecisionResponse { ok: true }).into_response(),
        Err(e) => (
            StatusCode::from_u16(e.http_status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(e.to_json()),
        )
            .into_response(),
    }
}

pub(crate) async fn question_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<SdkSessionQuery>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let questions: Vec<serde_json::Value> = state
        .deps
        .permissions
        .pending_views()
        .into_iter()
        .filter(|v| v.capability != NETWORK_CAPABILITY)
        .filter(|v| {
            q.session_id
                .as_ref()
                .is_none_or(|sid| v.session_id.to_string() == *sid)
        })
        .map(|v| pending_permission_json(&v))
        .collect();
    Json(QuestionListResponse { questions }).into_response()
}

pub(crate) async fn network_reply(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<NetworkReplyRequest>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    if req.network_id.trim().is_empty() || req.decision.trim().is_empty() {
        let e = ApiError {
            code: "malformed",
            message: "network_id and decision are required".into(),
            http_status: 400,
            retryable: false,
        };
        return (StatusCode::BAD_REQUEST, Json(e.to_json())).into_response();
    }
    match resolve_pending_permission(&state.deps, &req.network_id, &req.decision, "network") {
        Ok(()) => Json(PermissionDecisionResponse { ok: true }).into_response(),
        Err(e) => (
            StatusCode::from_u16(e.http_status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(e.to_json()),
        )
            .into_response(),
    }
}

/// `POST /network/reject` — deny is the whole semantics.
pub(crate) async fn network_reject(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<NetworkRejectRequest>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    if req.network_id.trim().is_empty() {
        let e = ApiError {
            code: "malformed",
            message: "network_id is required".into(),
            http_status: 400,
            retryable: false,
        };
        return (StatusCode::BAD_REQUEST, Json(e.to_json())).into_response();
    }
    match resolve_pending_permission(&state.deps, &req.network_id, "deny", "network") {
        Ok(()) => Json(PermissionDecisionResponse { ok: true }).into_response(),
        Err(e) => (
            StatusCode::from_u16(e.http_status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(e.to_json()),
        )
            .into_response(),
    }
}

pub(crate) async fn network_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<SdkSessionQuery>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let networks: Vec<serde_json::Value> = state
        .deps
        .permissions
        .pending_views()
        .into_iter()
        .filter(|v| v.capability == NETWORK_CAPABILITY)
        .filter(|v| {
            q.session_id
                .as_ref()
                .is_none_or(|sid| v.session_id.to_string() == *sid)
        })
        .map(|v| pending_permission_json(&v))
        .collect();
    Json(NetworkListResponse { networks }).into_response()
}

/// One pending request as the SDK's declared `PermissionRequest`:
/// `{id, sessionID, permission, patterns, metadata, always, tool?}`.
/// `permission` is the daemon's real capability tag; `patterns` the
/// concrete requested target string(s) inside the capability detail (empty
/// when the detail carries none); the full capability payload rides
/// `metadata`; `always` is empty because this slice has no always-allow
/// rule store (never a fabricated rule).
fn sdk_permission_request(p: &PendingPermission) -> serde_json::Value {
    let mut patterns: Vec<String> = Vec::new();
    if let Some(obj) = p.detail.get("detail").and_then(|d| d.as_object()) {
        for value in obj.values() {
            if let Some(s) = value.as_str() {
                patterns.push(s.to_string());
            }
        }
    }
    serde_json::json!({
        "id": p.id.to_string(),
        "sessionID": p.session_id.to_string(),
        "permission": p.capability,
        "patterns": patterns,
        "metadata": {"detail": p.detail},
        "always": [],
    })
}

/// `GET /permission` — the SDK's declared BARE `PermissionRequest[]` over
/// the daemon's REAL pending permission requests (every class: the
/// capability detail carries the class; the network-class asks surface here
/// too, never hidden).
pub(crate) async fn permission_list_sdk(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let permissions: Vec<serde_json::Value> = state
        .deps
        .permissions
        .pending_views()
        .into_iter()
        .map(|v| sdk_permission_request(&v))
        .collect();
    Json(permissions).into_response()
}

/// `GET /question` — the SDK's declared BARE `QuestionRequest[]`. This
/// slice has NO structured-question subsystem: its question-class asks ARE
/// permission requests and surface (with every other class) under
/// `GET /permission`, so the truthful projection is the empty array —
/// never a fabricated question/option object.
pub(crate) async fn question_list_sdk(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    Json(Vec::<serde_json::Value>::new()).into_response()
}

/// `GET /network` — the SDK's declared BARE `SessionNetworkWait[]`. The
/// SDK type is a network RECONNECT wait with a required creation time; the
/// daemon has no reconnect-wait state (its network-class asks are
/// permission requests on `GET /permission`), so the truthful projection is
/// the empty array — never a fabricated wait or timestamp.
pub(crate) async fn network_list_sdk(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    Json(Vec::<serde_json::Value>::new()).into_response()
}

pub(crate) async fn config_get(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let config = state.config.read().unwrap().clone();
    Json(ConfigGetResponse { config }).into_response()
}

/// `GET /config` — the SDK's `config.get` route: the BARE daemon config
/// object (Config has no required fields; the daemon exposes exactly the
/// runtime keys it owns). The legacy `/config/get` envelope stays for the
/// frozen consumers.
pub(crate) async fn config_get_bare(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let config = state.config.read().unwrap().clone();
    Json(config).into_response()
}

/// The daemon-editable top-level config keys (`config.update` allowlist):
/// provider configuration is out of scope by design — only the model,
/// the compaction threshold and the system instructions may be applied.
pub(crate) const CONFIG_EDITABLE_KEYS: [&str; 3] = ["model", "compact_at_usage", "instructions"];

/// `POST /config/set` — the SDK full-replacement form (kept for the old
/// tests/clients): the whole config view is replaced, bounded.
pub(crate) async fn config_set(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ConfigSetRequest>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let bytes = serde_json::to_vec(&req.config).unwrap_or_default();
    if bytes.len() > MAX_CONFIG_BYTES {
        return config_oversized(bytes.len());
    }
    *state.config.write().unwrap() = req.config;
    Json(ConfigSetResponse { ok: true }).into_response()
}

pub(crate) fn config_oversized(bytes: usize) -> Response {
    let e = ApiError {
        code: "oversized",
        message: format!("config of {bytes} bytes exceeds {MAX_CONFIG_BYTES}"),
        http_status: 413,
        retryable: false,
    };
    (StatusCode::PAYLOAD_TOO_LARGE, Json(e.to_json())).into_response()
}

pub(crate) fn config_must_be_object(config: &serde_json::Value) -> Option<Response> {
    if !config.is_object() {
        let e = ApiError {
            code: "malformed",
            message: "config must be a JSON object".into(),
            http_status: 400,
            retryable: false,
        };
        return Some((StatusCode::BAD_REQUEST, Json(e.to_json())).into_response());
    }
    None
}

/// `POST /config/update` — apply ONLY the daemon-editable keys
/// (model/compact_at_usage/instructions) onto the stored config; any other
/// top-level key is rejected with a clear error, never silently dropped.
pub(crate) async fn config_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ConfigSetRequest>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let bytes = serde_json::to_vec(&req.config).unwrap_or_default();
    if bytes.len() > MAX_CONFIG_BYTES {
        return config_oversized(bytes.len());
    }
    if let Some(resp) = config_must_be_object(&req.config) {
        return resp;
    }
    let incoming = req.config.as_object().unwrap();
    for key in incoming.keys() {
        if !CONFIG_EDITABLE_KEYS.contains(&key.as_str()) {
            let e = ApiError {
                code: "malformed",
                message: format!(
                    "config key {key:?} is not daemon-editable; allowed keys: {}",
                    CONFIG_EDITABLE_KEYS.join(", ")
                ),
                http_status: 400,
                retryable: false,
            };
            return (StatusCode::BAD_REQUEST, Json(e.to_json())).into_response();
        }
    }
    {
        let mut config = state.config.write().unwrap();
        let target = config
            .as_object_mut()
            .expect("daemon config is always an object");
        for (key, value) in incoming {
            target.insert(key.clone(), value.clone());
        }
    }
    Json(ConfigSetResponse { ok: true }).into_response()
}

/// `GET /config/warnings` — the SDK's declared BARE
/// `Array<{path,message,detail?}>`, produced by real validation over the
/// stored config (empty when everything validates). `path` is the literal
/// source label `runtime`: the daemon's config is an in-memory runtime
/// object with no file layer in this slice, and a fabricated filename would
/// be worse than the honest label. The legacy `{warnings:[…]}` envelope is
/// gone (the SDK type is the contract; checked-in consumers were updated).
pub(crate) async fn config_warnings(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let config = state.config.read().unwrap().clone();
    let mut warnings: Vec<serde_json::Value> = Vec::new();
    let mut push = |message: String| {
        warnings.push(serde_json::json!({"path": "runtime", "message": message}));
    };
    if let Some(obj) = config.as_object() {
        for (key, value) in obj {
            match key.as_str() {
                "model" => {
                    if !value.is_string() {
                        push("config \"model\" must be a string".into());
                    }
                }
                "compact_at_usage" => {
                    let ok = value.as_f64().is_some_and(|v| (0.0..=1.0).contains(&v));
                    if !ok {
                        push("config \"compact_at_usage\" must be a number in [0, 1]".into());
                    }
                }
                "instructions" => {
                    if !value.is_string() {
                        push("config \"instructions\" must be a string".into());
                    }
                }
                other => push(format!(
                    "unknown config key {other:?} (daemon-editable keys: {})",
                    CONFIG_EDITABLE_KEYS.join(", ")
                )),
            }
        }
    }
    Json(warnings).into_response()
}

/// `GET /config/overlay` — the SDK's `config.overlay` read. This daemon has
/// exactly ONE configuration layer (the in-memory runtime object; the CLI
/// resolves providers before serve and this slice exposes no config FILE),
/// so the projection is honest about that: the runtime layer is the single
/// `source` (kind `runtime`, editable `false`), it applies as the global
/// layer (`effective == global == runtime config`), no project layer is
/// loaded (`project: {}`), and the SDK's file `targets` all report
/// `exists:false` with empty paths/revisions (there are no config files to
/// point at). `fields` carries the real per-key values with
/// `source:"system"` and the daemon's honest `editable` allowlist; a file
/// path/revision/inheritance chain is never fabricated.
pub(crate) async fn config_overlay_get(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let config = state.config.read().unwrap().clone();
    let mut fields = serde_json::Map::new();
    if let Some(obj) = config.as_object() {
        for (key, value) in obj {
            fields.insert(
                key.clone(),
                serde_json::json!({
                    "key": key,
                    "path": [key],
                    "value": value,
                    "source": "system",
                    "inherited": false,
                    "overridden": false,
                    "editable": CONFIG_EDITABLE_KEYS.contains(&key.as_str()),
                    "reason": "daemon runtime config (no file layer in this slice)",
                }),
            );
        }
    }
    let empty_target = |scope: &str| {
        serde_json::json!({
            "scope": scope,
            "path": "",
            "revision": "",
            "exists": false,
            "writable": false,
            "raw": {},
        })
    };
    Json(serde_json::json!({
        "scope": "global",
        "effective": config.clone(),
        "global": config,
        "project": {},
        "sources": [{
            "order": 0,
            "kind": "runtime",
            "scope": "global",
            "label": "daemon runtime config",
            "source": "runtime",
            "exists": true,
            "editable": false,
            "reason": "in-memory runtime configuration (no file backing); applies as the global layer",
        }],
        "targets": {
            "global": empty_target("global"),
            "project": empty_target("project"),
            "active": empty_target("global"),
        },
        "fields": fields,
        "collections": {},
    }))
    .into_response()
}

/// `POST /config/overlay` — store a bounded overlay, replacing the whole
/// daemon config view. `POST /config/overlayUpdate` shallow-merges instead.
pub(crate) async fn config_overlay(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ConfigSetRequest>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let bytes = serde_json::to_vec(&req.config).unwrap_or_default();
    if bytes.len() > MAX_CONFIG_BYTES {
        return config_oversized(bytes.len());
    }
    if let Some(resp) = config_must_be_object(&req.config) {
        return resp;
    }
    *state.config.write().unwrap() = req.config;
    Json(ConfigSetResponse { ok: true }).into_response()
}

/// `POST /config/overlayUpdate` — bounded shallow merge of the overlay keys
/// into the current config view.
pub(crate) async fn config_overlay_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ConfigSetRequest>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let bytes = serde_json::to_vec(&req.config).unwrap_or_default();
    if bytes.len() > MAX_CONFIG_BYTES {
        return config_oversized(bytes.len());
    }
    if let Some(resp) = config_must_be_object(&req.config) {
        return resp;
    }
    let incoming = req.config.as_object().unwrap();
    {
        let mut config = state.config.write().unwrap();
        let target = config
            .as_object_mut()
            .expect("daemon config is always an object");
        for (key, value) in incoming {
            target.insert(key.clone(), value.clone());
        }
    }
    Json(ConfigSetResponse { ok: true }).into_response()
}

pub(crate) async fn provider_list(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let ids = state.deps.agent.deps().providers.ids();
    let mut providers = Vec::new();
    for id in ids {
        let models = if let Some(p) = state.deps.agent.deps().providers.get(&id) {
            // Dynamic registry (audit round 8): the adapter's REAL model
            // list with REAL capabilities — the model selector can now
            // enumerate what the daemon can actually serve.
            p.known_models()
                .into_iter()
                .map(|m| ModelInfo {
                    id: m.clone(),
                    name: m.clone(),
                    capabilities: p.capabilities(&m),
                })
                .collect()
        } else {
            vec![]
        };
        providers.push(ProviderInfo {
            id: id.clone(),
            name: id.clone(),
            kind: id.clone(),
            models,
        });
    }
    Json(ProviderList { providers }).into_response()
}

// ------------------------------------------------- SDK `provider.list` (v2)
// `GET /provider` is the unmodified v7.5.6 client's `provider.list` route.
// The declared response is `{all: Provider[], default: {[key]: modelID},
// connected: string[], failed: string[]}` and every `Provider.models[].Model`
// is the models.dev-style object whose `api`, numeric `cost` and `limit`
// fields are REQUIRED. This projection builds those objects from the REAL
// provider catalog:
//
// - `cost` comes from the catalog row's frozen `PricingSnapshot` quote lines
//   (microUSD per MILLION tokens — the published $/M unit, converted by the
//   exact 1e6 ratio; never re-derived, never rounded per line);
// - `limit.context`/`limit.output` come from the row's REAL
//   `ModelCapabilities`;
// - `capabilities` maps the same real flags (tools/thinking/vision) onto the
//   SDK booleans; `temperature` and `interleaved` are the documented
//   conservative `false` (the catalog carries no such capability);
// - `api.{id,url,npm}` comes from the configured provider FAMILY profile —
//   the family's published endpoint + AI-SDK package identity (see
//   `FamilyApiProfile`); `api.id` is the real wire model id;
// - `options`/`headers` are the honest empty objects (no per-model options
//   are configured in this slice);
// - `release_date`/`status` are OMITTED: the catalog carries no vendor
//   release date and no alpha/beta/deprecated lifecycle (retired rows are
//   excluded by their pricing state instead) — a fabricated date/status
//   would be worse than the declared-required omission (documented).
//
// Pricing EXCLUSION rule (the honest rule this projection enforces):
// a model appears ONLY when its catalog row's pricing state is
// authoritative (`Known`/`ConservativeCeiling`/`LocalZero`; `Unknown` rows
// have no quote at all, and flattening them to zero cost is exactly the
// fabrication `PricingState::Unknown` exists to prevent) AND the family
// profile attests the endpoint:
//
// | family | included authorities |
// |---|---|
// | openai | `Known` with `BuiltIn` provenance only (the official builtin row); a `ConservativeCeiling`/user-override row cannot attest the endpoint (a custom OpenAI-compatible proxy shares the family id) |
// | anthropic | `Known` / `ConservativeCeiling` (the endpoint is fixed by construction — the config has no base-URL override) |
// | google | `Known` / `ConservativeCeiling` (fixed endpoint, same as anthropic) |
// | ollama | `LocalZero` only (the adapter's measured local zero; the profile's endpoint is the family default) |
// | any other family | none (no authoritative endpoint/package identity) |
//
// `Unknown`-priced models — including every model whose family has no
// profile — are omitted, never emitted with zero/partial costs.

/// One provider family's published API identity (endpoint + AI-SDK npm
/// package): the `api.{url,npm}` source of the SDK `Model` objects. Values
/// are the family's canonical published defaults (the adapter crates' own
/// defaults); they are family-level identities, never per-instance claims.
struct FamilyApiProfile {
    url: &'static str,
    npm: &'static str,
    /// Official billing origin of the family's builtin documented rows
    /// (`None` for the local family). Used ONLY to enumerate the documented
    /// model names; pricing still comes from each provider's real catalog
    /// row.
    origin: Option<faktor_core::model::BillingOrigin>,
}

fn family_api_profile(family: &str) -> Option<FamilyApiProfile> {
    use faktor_core::model::BillingOrigin;
    match family {
        "openai" => Some(FamilyApiProfile {
            url: "https://api.openai.com/v1",
            npm: "@ai-sdk/openai",
            origin: Some(BillingOrigin::OfficialOpenAi),
        }),
        "anthropic" => Some(FamilyApiProfile {
            url: "https://api.anthropic.com",
            npm: "@ai-sdk/anthropic",
            origin: Some(BillingOrigin::OfficialAnthropic),
        }),
        "google" => Some(FamilyApiProfile {
            url: "https://generativelanguage.googleapis.com",
            npm: "@ai-sdk/google",
            origin: Some(BillingOrigin::OfficialGoogle),
        }),
        "ollama" => Some(FamilyApiProfile {
            url: "http://127.0.0.1:11434/v1",
            npm: "ollama-ai-provider",
            origin: None,
        }),
        _ => None,
    }
}

/// microUSD per million tokens -> the SDK's USD-per-million-token number
/// (the published list-price unit; exact for every table row, which stores
/// whole microUSD).
fn usd_per_million(line: faktor_core::model::MicroUsdPerMillionTokens) -> f64 {
    line.0 as f64 / 1_000_000.0
}

fn sdk_model_capabilities(caps: &faktor_core::model::ModelCapabilities) -> serde_json::Value {
    serde_json::json!({
        "temperature": false,
        "reasoning": caps.thinking || caps.reasoning,
        "attachment": caps.vision,
        "toolcall": caps.tools,
        "input": {
            "text": true,
            "audio": false,
            "image": caps.vision,
            "video": false,
            "pdf": false,
        },
        "output": {
            "text": true,
            "audio": false,
            "image": false,
            "video": false,
            "pdf": false,
        },
        "interleaved": false,
    })
}

/// One catalog row -> the SDK `Model` object, or `None` under the exclusion
/// rule (Unknown pricing, no family profile, or an endpoint the family
/// profile cannot attest).
pub(crate) fn sdk_model_json(
    entry: &faktor_provider::catalog::ModelCatalogEntry,
    family: &str,
) -> Option<serde_json::Value> {
    use faktor_core::model::PriceAuthority;
    use faktor_provider::catalog::Provenance;

    let profile = family_api_profile(family)?;
    let quote = entry.pricing.quote()?; // None == Unknown: never a zero
    let authority = entry.pricing.authority();
    match (family, authority, entry.provenance) {
        // Local runtimes only under the local family, and never elsewhere.
        ("ollama", PriceAuthority::LocalZero, _) => {}
        (_, PriceAuthority::LocalZero, _) => return None,
        // The official OpenAI endpoint: only a builtin exact row attests it
        // (a proxy/custom endpoint shares the family id and its rows carry
        // UserOverride/Composite provenance or Unknown pricing).
        ("openai", PriceAuthority::Exact, Provenance::BuiltIn) => {}
        // Anthropic/Google endpoints are fixed by construction: any
        // authoritative row (exact or a declared ceiling) is against the
        // official endpoint.
        (
            "anthropic" | "google",
            PriceAuthority::Exact | PriceAuthority::ConservativeCeiling,
            _,
        ) => {}
        _ => return None,
    }
    let provider_id = entry.provider.clone();
    Some(serde_json::json!({
        "id": entry.model,
        "name": entry.model,
        "providerID": provider_id,
        "api": {
            "id": entry.model,
            "url": profile.url,
            "npm": profile.npm,
        },
        "capabilities": sdk_model_capabilities(&entry.capabilities),
        "cost": {
            "input": usd_per_million(quote.input),
            "output": usd_per_million(quote.output),
            "cache": {
                "read": usd_per_million(quote.cache_read),
                "write": usd_per_million(quote.cache_write),
            },
        },
        "limit": {
            "context": entry.capabilities.context,
            "output": entry.capabilities.max_output,
        },
        "options": {},
        "headers": {},
    }))
}

/// The candidate model names of one provider: its REAL reported models plus
/// the documented builtin rows of the family's official origin (names only —
/// every row's pricing/capabilities still come from the provider's own
/// `catalog_entry`).
fn sdk_model_candidates(
    provider: &std::sync::Arc<dyn faktor_provider::Provider>,
) -> std::collections::BTreeSet<String> {
    let mut candidates: std::collections::BTreeSet<String> =
        provider.known_models().into_iter().collect();
    if let Some(profile) = family_api_profile(provider.id()) {
        if let Some(origin) = profile.origin {
            for row in faktor_provider::catalog::builtin::TABLE {
                if row.origin == origin && !row.is_retired() {
                    candidates.insert(row.model.to_string());
                }
            }
        }
    }
    candidates
}

/// The whole `provider.list` response body, projected from the registry's
/// REAL providers (pure function; the handler only wraps it).
pub(crate) fn sdk_provider_list(registry: &faktor_provider::ProviderRegistry) -> serde_json::Value {
    let mut all: Vec<serde_json::Value> = Vec::new();
    let mut connected: Vec<String> = Vec::new();
    for id in registry.ids() {
        let Some(provider) = registry.get(&id) else {
            continue;
        };
        // The daemon keeps no provider-credential store, so "connected" is
        // exactly the registry's registered provider instance ids (its
        // usable set) — never a credential claim. A registered provider
        // whose family has no attested endpoint contributes no models.
        connected.push(provider.identity().instance_id);
        for model in sdk_model_candidates(&provider) {
            let entry = provider.catalog_entry(&model);
            if let Some(value) = sdk_model_json(&entry, provider.id()) {
                all.push(value);
            }
        }
    }
    // Deterministic order (providerID, model): the projection is a stable
    // list for every replay.
    all.sort_by(|a, b| {
        let key = |v: &serde_json::Value| {
            (
                v["providerID"].as_str().unwrap_or("").to_string(),
                v["id"].as_str().unwrap_or("").to_string(),
            )
        };
        key(a).cmp(&key(b))
    });
    serde_json::json!({
        "all": all,
        // No provider-qualified default model exists in this slice's runtime
        // config (the session default model is a bare name; provider routing
        // is resolved per turn), so the map is truthfully empty.
        "default": {},
        "connected": connected,
        // No failed-provider tracking is exposed by the registry: only
        // successfully registered providers exist here.
        "failed": [],
    })
}

/// `GET /provider` — the SDK's `provider.list` route (bare
/// `{all,default,connected,failed}` object, never the legacy
/// `{providers:[…]}` envelope; the legacy route stays at `/provider/list`).
pub(crate) async fn provider_list_sdk(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    Json(sdk_provider_list(&state.deps.agent.deps().providers)).into_response()
}

// ------------------------------------------------------------------ native v1
// Faktor Native Protocol v1 (docs/native-protocol.md): the daemon's own
// HTTP surface. UI compatibility is the target; these handlers map to
// durable runtime state only (row, journal, ledger, turn records,
// tool-run rows) and never fabricate v7.5.6 frames.

pub(crate) async fn events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<MessagesQuery>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let sid = match parse_session_id(&id) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::from_u16(e.http_status).unwrap(),
                Json(e.to_json()),
            )
                .into_response()
        }
    };
    // The SSE cursor is the raw sequence (0 = from the beginning); the
    // journal is queried as `seq > cursor` via events_range.
    let cursor: i64 = q.events_after.unwrap_or(0).max(0);
    let handle = match state.deps.session.get_session(sid) {
        Ok(Some(h)) => h,
        Ok(None) => {
            let e = ApiError {
                code: "not_found",
                message: format!("session {sid}"),
                http_status: 404,
                retryable: false,
            };
            return (StatusCode::NOT_FOUND, Json(e.to_json())).into_response();
        }
        Err(e) => return api_err(&e),
    };

    // Catch-up frames from the journal, then poll for new ones. The journal
    // is the source of truth; a reconnect resumes exactly from the cursor.
    let stream = journal_stream(handle, cursor);
    Sse::new(stream)
        .keep_alive(
            axum::response::sse::KeepAlive::new()
                .interval(Duration::from_secs(HEARTBEAT_SECS))
                .text("keep-alive"),
        )
        .into_response()
}

pub(crate) fn journal_stream(
    handle: faktor_session::SessionHandle,
    cursor: i64,
) -> impl Stream<Item = Result<Event, std::convert::Infallible>> + Send + 'static {
    // Journal catch-up is PAGED (bounded everything): each poll loads at
    // most EVENT_CATCHUP_PAGE frames into the in-memory queue, so a
    // reconnect against a huge journal can never balloon RAM. Every frame
    // carries its `id:` sequence — the resume cursor — and the stream
    // simply continues across pages, so "more pages" is implicit and
    // deterministic: no duplicate and no gap on replay from any cursor.
    // State: (handle, next cursor, queue of ready frames).
    // Poll the journal; the journal is the source of truth and the cursor is
    // the SSE resume point. Heartbeats keep proxies alive.
    futures_util::stream::unfold(
        (handle, cursor, VecDeque::<Event>::new()),
        move |(handle, mut cursor, mut queue)| async move {
            if let Some(ev) = queue.pop_front() {
                return Some((
                    Ok::<Event, std::convert::Infallible>(ev),
                    (handle, cursor, queue),
                ));
            }
            // seq > cursor (cursor 0 = everything from seq 1). One bounded
            // page per poll; the next poll continues exactly after it.
            let events = handle
                .events_range(cursor.saturating_add(1) as u64, Some(EVENT_CATCHUP_PAGE))
                .unwrap_or_default();
            let mut batch = VecDeque::new();
            let mut advanced = false;
            for e in events {
                if let Some((event, _)) = faktor_protocol::sse::project_event(&e) {
                    batch.push_back(sse_event(faktor_session::JournalFrame {
                        seq: e.seq,
                        event,
                    }));
                }
                cursor = e.seq.raw() as i64;
                advanced = true;
            }
            if advanced {
                if let Some(ev) = batch.pop_front() {
                    return Some((
                        Ok::<Event, std::convert::Infallible>(ev),
                        (handle, cursor, batch),
                    ));
                }
            }
            tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
            Some((
                Ok::<Event, std::convert::Infallible>(sse_event_heartbeat()),
                (handle, cursor, queue),
            ))
        },
    )
}

pub(crate) fn sse_event(frame: faktor_session::JournalFrame) -> Event {
    let seq = frame.seq.raw();
    let json = serde_json::to_string(&frame.event).unwrap_or_else(|_| "{}".into());
    Event::default()
        .event(frame.event.event_type())
        .id(seq.to_string())
        .data(json)
}

pub(crate) fn sse_event_heartbeat() -> Event {
    Event::default().event("heartbeat").data("{}")
}

// ------------------------------------------------------------ SDK global SSE
// `GET /global/event?after=<n>` streams the SDK `GlobalEvent` union frames
// (`id: <global-sequence>\ndata: <json>\n\n`; no `event:` field — the
// payload's `type` discriminates). The projection source is the DURABLE
// session journal (session lifecycle, turn open/close, failures) plus the
// durable message/part rows (message.updated / message.part.updated) and the
// live chunk sink (session.next.*.delta); the bounded ring is the replay
// cache for the `after` resume cursor. Frame ids are the ring's global
// sequence numbers, so resume is exact and idempotent.
//
// Union coverage (the SDK union has ~100 members; only frames with a REAL
// daemon source are projected — the remaining members stay locked, each
// with its missing primitive named in docs/wire-compat.md):
//
// - projected: session.created (journal SessionCreated + durable row),
//   session.updated (durable row metadata mutation: title/provider/model
//   changed between polls; the title-update path journals no event, the row
//   is the trigger), session.deleted (journal SessionEnded + durable row),
//   session.turn.open (journal PromptReceived), session.turn.close (journal
//   TurnCompleted on a park/terminal state), session.error (journal Failed),
//   message.updated (durable message rows: user + assistant),
//   message.part.updated (durable text/reasoning part rows, plus the durable
//   user prompt text), session.next.text.delta /
//   session.next.reasoning.delta (the live chunk sink, assistant messages
//   only);
// - locked: tool parts (the daemon persists bounded `excerpt` + artifact
//   REFERENCES, never the full inline `output`/`title`/`error` the SDK
//   `ToolState` declares, and stores tool_call/tool_result as separate rows
//   rather than one stateful `ToolPart`), and every union member whose
//   subsystem this daemon does not implement (pty.*, permission.*,
//   question.*, sandbox.*, indexing.*, installation.*, MCP, worktree, …).

/// Default replay depth of the SDK frame ring (bounded everything).
pub(crate) const SDK_RING_CAPACITY: usize = 4096;
/// Message rows scanned per poll per session (bounded read).
const SDK_MESSAGE_PAGE: u64 = 100;
/// Bounded page cap per poll per session. The ring is a LIVE replay cache:
/// a pathological backlog beyond this bound advances the message cursor
/// (documented) instead of growing memory; clients recover exact state
/// through `session.messages`.
const SDK_MESSAGE_MAX_PAGES: usize = 8;

/// One SDK `GlobalEvent` envelope: `{directory, project?, workspace?,
/// payload:{id,type,properties}}` (the vendored v7.5.6 union shape).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct SdkGlobalEvent {
    pub directory: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    pub payload: SdkGlobalPayload,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct SdkGlobalPayload {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub properties: serde_json::Value,
}

/// The daemon-side SDK global event projector: lazily polled by the
/// connected SSE streams (the same no-background-task pattern as the
/// per-session journal stream), replaying from a bounded ring.
pub(crate) struct SdkGlobalProjector {
    session: Arc<faktor_session::SessionManager>,
    directory: Option<String>,
    version: String,
    capacity: usize,
    next_id: std::sync::atomic::AtomicU64,
    state: std::sync::Mutex<SdkProjectorState>,
}

#[derive(Default)]
struct SdkProjectorState {
    ring: VecDeque<(u64, SdkGlobalEvent)>,
    /// Per-session journal sequence already projected.
    journal_cursors: HashMap<u64, u64>,
    /// Per-session newest message sequence already projected.
    message_cursors: HashMap<u64, i64>,
    /// Durable message ROW id -> durable message sequence (the wire message
    /// identity); filled as messages are projected and on chunk misses.
    message_seq_by_row: HashMap<i64, i64>,
    /// Per-session SDK-visible metadata fingerprint (title, provider, model).
    /// A change between polls is the durable trigger for `session.updated`
    /// (the title-update path journals no event; the row itself is polled).
    session_fingerprints: HashMap<u64, (String, String, String)>,
}

impl SdkGlobalProjector {
    pub(crate) fn new(
        session: Arc<faktor_session::SessionManager>,
        directory: Option<String>,
        version: String,
    ) -> Self {
        Self::with_capacity(session, directory, version, SDK_RING_CAPACITY)
    }

    pub(crate) fn with_capacity(
        session: Arc<faktor_session::SessionManager>,
        directory: Option<String>,
        version: String,
        capacity: usize,
    ) -> Self {
        assert!(capacity > 0, "ring capacity must be positive");
        Self {
            session,
            directory,
            version,
            capacity,
            next_id: std::sync::atomic::AtomicU64::new(0),
            state: std::sync::Mutex::new(SdkProjectorState::default()),
        }
    }

    /// Push ONE live chunk frame (the agent's chunk sink). `tool` chunks are
    /// not projected here: their structured call/result reaches clients
    /// through the durable part rows; the chunk text is a preformatted
    /// announcement, not the SDK `session.next.tool.called` properties.
    pub(crate) fn push_chunk(&self, chunk: faktor_agent::ChunkEvent) {
        self.push_chunk_at(chunk, faktor_core::model::unix_now_ms());
    }

    fn push_chunk_at(&self, chunk: faktor_agent::ChunkEvent, ts_ms: u64) {
        let Some(row_id) = chunk.message_id else {
            return;
        };
        let sid = chunk.session_id;
        let dir = self.directory_for(sid);
        let session_label = sid.to_string();
        let mut st = self.state.lock().unwrap();
        let Some(seq) = self.message_seq_of_row(&mut st, sid, row_id) else {
            return;
        };
        let label = seq.to_string();
        let (kind, properties) = match chunk.kind {
            "text" => (
                "session.next.text.delta",
                serde_json::json!({
                    "timestamp": ts_ms,
                    "sessionID": session_label,
                    "assistantMessageID": label,
                    "textID": format!("{label}:text"),
                    "delta": chunk.text,
                }),
            ),
            "reasoning" => (
                "session.next.reasoning.delta",
                serde_json::json!({
                    "timestamp": ts_ms,
                    "sessionID": session_label,
                    "assistantMessageID": label,
                    "reasoningID": format!("{label}:reasoning"),
                    "delta": chunk.text,
                }),
            ),
            _ => return,
        };
        self.emit_payload(&mut st, dir, kind, properties);
    }

    /// One lazy poll: scan every session's journal from its cursor (one
    /// bounded page per session) then project newly durable messages/parts.
    /// Idempotent: cursors make re-polling a no-op.
    pub(crate) fn poll_once(&self) {
        let mut st = self.state.lock().unwrap();
        let mut rows = self.session.list_sessions(None).unwrap_or_default();
        rows.sort_by_key(|r| r.id().raw());
        for listed in rows {
            let sid = listed.id();
            let Ok(Some(handle)) = self.session.get_session(sid) else {
                continue;
            };
            let Ok(session_row) = handle.row() else {
                continue;
            };
            // Durable session-row mutation -> `session.updated`. The
            // title-update path journals no event, but the row itself is
            // polled every cycle, so a change to its SDK-visible metadata
            // (title/provider/model) between polls emits exactly one frame.
            // The FIRST observation never emits (`session.created` covers
            // creation; pre-existing rows are read via `session.list`), and
            // internal `updated_ms` bumps from state transitions are not
            // metadata events, so they emit nothing — the frame carries the
            // row's real `time.updated`.
            let directory = self.directory_for(sid);
            let fingerprint = (
                session_row.title.clone(),
                session_row.provider.clone(),
                session_row.model.clone(),
            );
            let changed = st
                .session_fingerprints
                .insert(sid.raw(), fingerprint.clone())
                .map(|previous| previous != fingerprint)
                .unwrap_or(false);
            if changed {
                let info = self.session_info(&session_row, &directory);
                self.emit_payload(
                    &mut st,
                    directory,
                    "session.updated",
                    serde_json::json!({"sessionID": sid.to_string(), "info": info}),
                );
            }
            let last = st.journal_cursors.get(&sid.raw()).copied().unwrap_or(0);
            if let Ok(events) =
                handle.events_range(last.saturating_add(1), Some(EVENT_CATCHUP_PAGE))
            {
                for e in events {
                    self.project_event(&mut st, &handle, &e);
                    st.journal_cursors.insert(sid.raw(), e.seq.raw());
                }
            }
            self.project_messages(&mut st, &handle);
        }
    }

    fn project_event(
        &self,
        st: &mut SdkProjectorState,
        handle: &faktor_session::SessionHandle,
        e: &faktor_core::event::Event,
    ) {
        use faktor_core::event::EventKind;
        let sid = e.session_id;
        let dir = self.directory_for(sid);
        let session_label = sid.to_string();
        match e.kind {
            EventKind::SessionCreated => {
                if let Ok(row) = handle.row() {
                    let info = self.session_info(&row, &dir);
                    self.emit_payload(
                        st,
                        dir,
                        "session.created",
                        serde_json::json!({"sessionID": session_label, "info": info}),
                    );
                }
            }
            EventKind::SessionEnded => {
                if let Ok(row) = handle.row() {
                    let info = self.session_info(&row, &dir);
                    self.emit_payload(
                        st,
                        dir,
                        "session.deleted",
                        serde_json::json!({"sessionID": session_label, "info": info}),
                    );
                }
            }
            EventKind::PromptReceived => {
                self.emit_payload(
                    st,
                    dir,
                    "session.turn.open",
                    serde_json::json!({"sessionID": session_label}),
                );
            }
            EventKind::TurnCompleted => {
                // Interior TurnCompleted hops (Validating/UpdatingMemory) do
                // not close a turn; the SDK close fires only when the turn
                // actually parks/terminates.
                if !turn_machine_busy(e.state) {
                    self.emit_payload(
                        st,
                        dir,
                        "session.turn.close",
                        serde_json::json!({"sessionID": session_label, "reason": "completed"}),
                    );
                }
            }
            EventKind::Failed => {
                let message = e
                    .payload
                    .as_ref()
                    .and_then(|p| p.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("agent failed")
                    .to_string();
                self.emit_payload(
                    st,
                    dir,
                    "session.error",
                    serde_json::json!({
                        "sessionID": session_label,
                        "error": {"name": "UnknownError", "data": {"message": message}},
                    }),
                );
            }
            _ => {}
        }
    }

    /// Project every message newer than the session's cursor: the
    /// `message.updated` info (user/assistant; system rows have no SDK
    /// Message variant) and one `message.part.updated` per durable
    /// text/reasoning part row (plus the durable user prompt text, mirroring
    /// the wire page). Bounded: at most `SDK_MESSAGE_MAX_PAGES` pages per
    /// poll.
    fn project_messages(&self, st: &mut SdkProjectorState, handle: &faktor_session::SessionHandle) {
        let sid = handle.id();
        let last = st.message_cursors.get(&sid.raw()).copied().unwrap_or(0);
        let store = self.session.store();
        let mut fetched: Vec<faktor_store::MessageRow> = Vec::new();
        let mut before: Option<i64> = None;
        for _ in 0..SDK_MESSAGE_MAX_PAGES {
            let Ok(page) = store.messages_before(sid, before, SDK_MESSAGE_PAGE) else {
                break;
            };
            if page.is_empty() {
                break;
            }
            let page_len = page.len() as u64;
            let mut saw_old = false;
            for m in &page {
                if m.seq <= last {
                    saw_old = true;
                    break;
                }
                fetched.push(m.clone());
            }
            if saw_old || page_len < SDK_MESSAGE_PAGE {
                break;
            }
            before = page.last().map(|m| m.seq);
        }
        fetched.sort_by_key(|m| m.seq);
        for m in fetched {
            self.project_message(st, handle, &m);
            st.message_cursors.insert(sid.raw(), m.seq);
        }
    }

    fn project_message(
        &self,
        st: &mut SdkProjectorState,
        handle: &faktor_session::SessionHandle,
        m: &faktor_store::MessageRow,
    ) {
        let sid = handle.id();
        let dir = self.directory_for(sid);
        let session_label = sid.to_string();
        let label = m.seq.to_string();
        st.message_seq_by_row.insert(m.id, m.seq);
        let (provider, model) = match handle.row() {
            Ok(row) => (row.provider, row.model),
            Err(_) => (String::new(), String::new()),
        };
        let info = match m.role.as_str() {
            "user" => serde_json::json!({
                "id": label,
                "sessionID": session_label,
                "role": "user",
                "time": {"created": m.created_ms},
                "agent": "default",
                "model": {"providerID": provider, "modelID": model},
            }),
            "assistant" => {
                // The SDK AssistantMessage requires parentID: it is the
                // durable user-message sequence that produced this row. A
                // row without a resolvable parent is skipped (the frame is
                // omitted, never a fabricated parent id).
                let parent = self
                    .session
                    .store()
                    .messages_before(sid, Some(m.seq), 1)
                    .ok()
                    .and_then(|page| page.into_iter().next())
                    .filter(|p| p.role == "user")
                    .map(|p| p.seq.to_string());
                let Some(parent_id) = parent else {
                    return;
                };
                serde_json::json!({
                    "id": label,
                    "sessionID": session_label,
                    "role": "assistant",
                    "time": {"created": m.created_ms},
                    "parentID": parent_id,
                    "modelID": model,
                    "providerID": provider,
                    "mode": "default",
                    "agent": "default",
                    "path": {"cwd": dir, "root": dir},
                    // The frozen surface does not model per-message usage
                    // (documented); zeros are its projection, never guesses.
                    "cost": 0.0,
                    "tokens": {
                        "input": 0,
                        "output": 0,
                        "reasoning": 0,
                        "cache": {"read": 0, "write": 0},
                    },
                })
            }
            _ => return,
        };
        self.emit_payload(
            st,
            dir.clone(),
            "message.updated",
            serde_json::json!({"sessionID": session_label, "info": info}),
        );
        let Ok(parts) = handle.parts_of(m.id) else {
            return;
        };
        for (index, p) in parts.iter().enumerate() {
            if let Some(part) = sdk_part_json(p, &session_label, &label, index) {
                self.emit_payload(
                    st,
                    dir.clone(),
                    "message.part.updated",
                    serde_json::json!({
                        "sessionID": session_label,
                        "part": part,
                        "time": p.created_ms,
                    }),
                );
            }
        }
        // The durable user prompt text is row DATA (no part rows), exactly
        // like the wire page projects it.
        if m.role == "user" && parts.is_empty() {
            if let Some(text) = m
                .data
                .get("text")
                .and_then(|v| v.as_str())
                .filter(|t| !t.is_empty())
            {
                let part = serde_json::json!({
                    "id": format!("{label}:0"),
                    "sessionID": session_label,
                    "messageID": label,
                    "type": "text",
                    "text": text,
                });
                self.emit_payload(
                    st,
                    dir,
                    "message.part.updated",
                    serde_json::json!({
                        "sessionID": session_label,
                        "part": part,
                        "time": m.created_ms,
                    }),
                );
            }
        }
    }

    fn session_info(&self, row: &faktor_store::SessionRow, directory: &str) -> serde_json::Value {
        serde_json::json!({
            "id": row.id.to_string(),
            "slug": row.id.to_string(),
            "projectID": row.workspace_id.to_string(),
            "workspaceID": row.workspace_id.to_string(),
            "directory": directory,
            "title": row.title,
            "model": {"id": row.model, "providerID": row.provider},
            "version": self.version,
            "time": {"created": row.created_ms, "updated": row.updated_ms},
        })
    }

    /// The directory an SDK frame carries: the session's durable workspace
    /// root when resolvable, else the daemon's configured directory, else
    /// the empty string (the SDK field is a required string; nothing is
    /// invented).
    fn directory_for(&self, sid: faktor_core::id::SessionId) -> String {
        self.session
            .resolve_workspace_root(sid)
            .ok()
            .flatten()
            .map(|p| p.to_string_lossy().to_string())
            .or_else(|| self.directory.clone())
            .unwrap_or_default()
    }

    fn message_seq_of_row(
        &self,
        st: &mut SdkProjectorState,
        sid: faktor_core::id::SessionId,
        row_id: i64,
    ) -> Option<i64> {
        if let Some(seq) = st.message_seq_by_row.get(&row_id).copied() {
            return Some(seq);
        }
        // A live chunk can race the message projection: resolve the row id
        // from one bounded newest page of the session's own messages.
        let page = self.session.store().messages_before(sid, None, 16).ok()?;
        for row in page {
            st.message_seq_by_row.insert(row.id, row.seq);
        }
        st.message_seq_by_row.get(&row_id).copied()
    }

    fn emit_payload(
        &self,
        st: &mut SdkProjectorState,
        directory: String,
        kind: &str,
        properties: serde_json::Value,
    ) {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        let frame = SdkGlobalEvent {
            directory,
            project: None,
            workspace: None,
            payload: SdkGlobalPayload {
                id: id.to_string(),
                kind: kind.to_string(),
                properties,
            },
        };
        st.ring.push_back((id, frame));
        while st.ring.len() > self.capacity {
            st.ring.pop_front();
        }
    }

    /// All frames with id > `after` the ring can still serve (oversized
    /// cursors are clamped: the client gets everything the ring has, never
    /// an error).
    pub(crate) fn frames_after(&self, after: u64) -> Vec<(u64, SdkGlobalEvent)> {
        let st = self.state.lock().unwrap();
        st.ring
            .iter()
            .filter(|(id, _)| *id > after)
            .cloned()
            .collect()
    }
}

/// One durable part row -> its SDK part, for text/reasoning only. Tool
/// call/result/summary rows are deliberately NOT projected: the SDK tool
/// part is ONE stateful object (input+output+times), while this daemon
/// stores the call and its result as separate rows — projecting either
/// alone would misstate the tool state (locked; documented).
fn sdk_part_json(
    p: &faktor_store::PartRow,
    session_label: &str,
    message_label: &str,
    index: usize,
) -> Option<serde_json::Value> {
    let id = format!("{message_label}:{index}");
    match p.kind.as_str() {
        "text" => p.data.get("text").and_then(|v| v.as_str()).map(|text| {
            serde_json::json!({
                "id": id,
                "sessionID": session_label,
                "messageID": message_label,
                "type": "text",
                "text": text,
            })
        }),
        "reasoning" => p.data.get("text").and_then(|v| v.as_str()).map(|text| {
            serde_json::json!({
                "id": id,
                "sessionID": session_label,
                "messageID": message_label,
                "type": "reasoning",
                "text": text,
                "time": {"start": p.created_ms},
            })
        }),
        _ => None,
    }
}

pub(crate) async fn global_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<GlobalEventsQuery>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let after = q.after.unwrap_or(0);
    let stream = sdk_global_stream(state.projector.clone(), after);
    Sse::new(stream)
        .keep_alive(
            axum::response::sse::KeepAlive::new()
                .interval(Duration::from_secs(HEARTBEAT_SECS))
                .text("keep-alive"),
        )
        .into_response()
}

pub(crate) fn sdk_global_stream(
    projector: Arc<SdkGlobalProjector>,
    after: u64,
) -> impl Stream<Item = Result<Event, std::convert::Infallible>> + Send + 'static {
    // Poll the projector for new frames (id > cursor), emit, then wait
    // before the next poll; heartbeats keep proxies alive.
    futures_util::stream::unfold(
        (projector, after, VecDeque::<(u64, SdkGlobalEvent)>::new()),
        |(projector, mut cursor, mut queue)| async move {
            if let Some((id, frame)) = queue.pop_front() {
                return Some((
                    Ok::<Event, std::convert::Infallible>(sdk_global_frame(id, frame)),
                    (projector, cursor, queue),
                ));
            }
            projector.poll_once();
            let frames = projector.frames_after(cursor);
            let mut batch = VecDeque::new();
            let mut advanced = false;
            for (id, frame) in frames {
                batch.push_back((id, frame));
                cursor = id;
                advanced = true;
            }
            if advanced {
                if let Some((id, frame)) = batch.pop_front() {
                    return Some((
                        Ok::<Event, std::convert::Infallible>(sdk_global_frame(id, frame)),
                        (projector, cursor, batch),
                    ));
                }
            }
            tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
            Some((
                Ok::<Event, std::convert::Infallible>(sse_event_heartbeat()),
                (projector, cursor, queue),
            ))
        },
    )
}

pub(crate) fn sdk_global_frame(id: u64, frame: SdkGlobalEvent) -> Event {
    let json = serde_json::to_string(&frame).unwrap_or_else(|_| "{}".into());
    Event::default().id(id.to_string()).data(json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use faktor_core::event::EventKind;
    use faktor_core::id::OpId;
    use faktor_core::state::AgentState;
    use faktor_provider::{
        GenericAgentRequest, Provider, ProviderError, ProviderErrorKind, ProviderStream,
    };
    use faktor_session::{SessionHandle, SessionManager};

    /// Catalog-only probe provider (family `openai`): its `known_models`
    /// carry one documented Exact row and one Unknown model. Streaming is
    /// never exercised.
    struct CatalogProbe;

    impl Provider for CatalogProbe {
        fn id(&self) -> &str {
            "openai"
        }

        fn capabilities(&self, _model: &str) -> faktor_core::model::ModelCapabilities {
            faktor_core::model::ModelCapabilities {
                context: 128_000,
                max_output: 16_384,
                tools: true,
                parallel_tools: true,
                thinking: true,
                vision: true,
                json_schema: true,
                streaming: true,
                embeddings: false,
                reasoning: false,
            }
        }

        fn known_models(&self) -> Vec<String> {
            vec!["gpt-4o".into(), "mystery-proxy-model".into()]
        }

        fn stream(&self, _req: GenericAgentRequest) -> ProviderStream {
            faktor_provider::provider_error_stream(ProviderError::with_code(
                ProviderErrorKind::BadRequest,
                "catalog_only",
                "the catalog probe never streams",
            ))
        }
    }

    #[test]
    fn provider_list_excludes_unknown_priced_models_and_serves_exact_sibling_byte_exact() {
        let mut registry = faktor_provider::ProviderRegistry::new();
        registry.try_register(Arc::new(CatalogProbe)).unwrap();
        let body = sdk_provider_list(&registry);

        // Only the documented, authoritative rows of the configured family
        // are served: the Unknown-priced `mystery-proxy-model` never
        // appears (flattening it to zero cost is the fabrication the
        // catalog's Unknown state exists to prevent).
        let all = body["all"].as_array().unwrap();
        let ids: Vec<&str> = all.iter().map(|m| m["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["gpt-4o", "gpt-4o-mini", "o3", "o4-mini"]);
        assert!(!ids.contains(&"mystery-proxy-model"));
        assert_eq!(body["connected"], serde_json::json!(["openai"]));
        assert_eq!(body["default"], serde_json::json!({}));
        assert_eq!(body["failed"], serde_json::json!([]));

        // The Exact sibling is served byte-exact: real quote lines in the
        // SDK's USD-per-million unit, real capabilities limits, the family's
        // published api identity, and NO fabricated release_date/status.
        let gpt = all.iter().find(|m| m["id"] == "gpt-4o").unwrap();
        assert_eq!(
            gpt.clone(),
            serde_json::json!({
                "id": "gpt-4o",
                "name": "gpt-4o",
                "providerID": "openai",
                "api": {
                    "id": "gpt-4o",
                    "url": "https://api.openai.com/v1",
                    "npm": "@ai-sdk/openai",
                },
                "capabilities": {
                    "temperature": false,
                    "reasoning": true,
                    "attachment": true,
                    "toolcall": true,
                    "input": {"text": true, "audio": false, "image": true, "video": false, "pdf": false},
                    "output": {"text": true, "audio": false, "image": false, "video": false, "pdf": false},
                    "interleaved": false,
                },
                // Builtin row gpt-4o: 2_500_000 / 10_000_000 / 1_250_000
                // microUSD per million -> $2.5 / $10 / $1.25; cache write is
                // NotBilled -> 0.
                "cost": {
                    "input": 2.5,
                    "output": 10.0,
                    "cache": {"read": 1.25, "write": 0.0},
                },
                "limit": {"context": 128_000, "output": 16_384},
                "options": {},
                "headers": {},
            }),
            "the Exact sibling must be served byte-exact"
        );
        // The declared-required lifecycle metadata is omitted, never
        // fabricated: the daemon catalog carries no vendor release date and
        // no alpha/beta/deprecated status.
        assert!(gpt.get("release_date").is_none());
        assert!(gpt.get("status").is_none());
    }

    fn manager(dir: &std::path::Path) -> Arc<SessionManager> {
        SessionManager::open(dir.join("store"), dir.join("cas"), true).unwrap()
    }

    fn scenario_session(m: &Arc<SessionManager>) -> SessionHandle {
        let ws = m.create_workspace("/w").unwrap();
        m.create_session(ws, "t", "fake", "m").unwrap()
    }

    /// Every dotted `type:` literal declared in the VENDORED v7.5.6 SDK
    /// types (the upstream fixture this projection is checked against).
    fn vendored_union_types() -> Vec<String> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../compat/kilo-v756/upstream-sdk/src/v2/gen/types.gen.ts");
        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("vendored SDK types at {path:?}: {e}"));
        let mut out = std::collections::BTreeSet::new();
        for line in src.lines() {
            let Some(idx) = line.find("type: \"") else {
                continue;
            };
            let rest = &line[idx + 7..];
            let Some(end) = rest.find('"') else {
                continue;
            };
            let literal = &rest[..end];
            if literal.contains('.')
                && literal
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
            {
                out.insert(literal.to_string());
            }
        }
        out.into_iter().collect()
    }

    fn corpus_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../compat/kilo-v756/sdk-global-frames.json")
    }

    /// Structural template match: `"@int"` matches any number; objects and
    /// arrays must have EXACTLY the template's key/index set (no extra
    /// frames/fields). Volatile timestamps are the only wildcard.
    fn assert_template(expected: &serde_json::Value, actual: &serde_json::Value, path: &str) {
        match expected {
            serde_json::Value::String(s) if s == "@int" => {
                // The captured side is already normalized (`template_frames`),
                // so a matched wildcard is the literal `"@int"`; a raw
                // capture may still carry the number.
                assert!(
                    actual.is_number() || actual.as_str() == Some("@int"),
                    "{path}: expected number, got {actual}"
                );
            }
            serde_json::Value::Object(exp) => {
                let act = actual
                    .as_object()
                    .unwrap_or_else(|| panic!("{path}: expected object, got {actual}"));
                assert_eq!(
                    exp.len(),
                    act.len(),
                    "{path}: key set mismatch (expected {:?}, got {:?})",
                    exp.keys().collect::<Vec<_>>(),
                    act.keys().collect::<Vec<_>>()
                );
                for (k, v) in exp {
                    assert_template(v, act.get(k).unwrap(), &format!("{path}.{k}"));
                }
            }
            serde_json::Value::Array(exp) => {
                let act = actual
                    .as_array()
                    .unwrap_or_else(|| panic!("{path}: expected array, got {actual}"));
                assert_eq!(exp.len(), act.len(), "{path}: array length mismatch");
                for (i, v) in exp.iter().enumerate() {
                    assert_template(v, &act[i], &format!("{path}[{i}]"));
                }
            }
            _ => assert_eq!(expected, actual, "{path}"),
        }
    }

    /// Walk a freshly captured frame and replace volatile wall-clock
    /// numbers with the `"@int"` template (all corpus timestamps are
    /// wall-clock; every other value is pinned).
    fn template_frames(value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Number(n) => {
                if n.as_u64().map(|v| v >= 1_000_000_000_000).unwrap_or(false)
                    || n.as_i64().map(|v| v >= 1_000_000_000_000).unwrap_or(false)
                {
                    serde_json::Value::String("@int".into())
                } else {
                    value.clone()
                }
            }
            serde_json::Value::Object(o) => serde_json::Value::Object(
                o.iter()
                    .map(|(k, v)| (k.clone(), template_frames(v)))
                    .collect(),
            ),
            serde_json::Value::Array(a) => {
                serde_json::Value::Array(a.iter().map(template_frames).collect())
            }
            _ => value.clone(),
        }
    }

    const GOLDEN_REGEN_ENV: &str = "FAKTOR_COMPAT_REGEN_GLOBAL_FRAMES";

    /// The replayable frame corpus: a real journal/message/part/chunk
    /// scenario replayed through the REAL projector, checked frame-by-frame
    /// against the checked-in golden and against the vendored SDK union
    /// (every frame type must be a declared upstream literal).
    #[test]
    fn sdk_global_projection_matches_golden_frame_corpus() {
        let dir = tempfile::tempdir().unwrap();
        let m = manager(dir.path());
        let s = scenario_session(&m);
        assert_eq!(s.id().raw(), 1, "deterministic fixture session id");
        let projector = SdkGlobalProjector::new(m.clone(), Some("/w".into()), "7.5.6".into());

        // 1. Journal: prompt -> context -> model started, then a first poll.
        s.force_append_event(
            EventKind::PromptReceived,
            AgentState::Preparing,
            Some(OpId::new(7)),
            Some(serde_json::json!({"queued": false})),
        )
        .unwrap();
        s.force_append_event(
            EventKind::ContextPrepared,
            AgentState::BuildingContext,
            None,
            None,
        )
        .unwrap();
        s.force_append_event(
            EventKind::ModelStarted,
            AgentState::WaitingForModel,
            None,
            None,
        )
        .unwrap();
        projector.poll_once();

        // 2. Durable messages: user prompt (row data) + assistant text part,
        // then a second poll projects message.updated/message.part.updated.
        let _user_mid = s
            .put_message(2, "user", serde_json::json!({"text": "hi"}))
            .unwrap();
        let assistant_mid = s
            .put_message(3, "assistant", serde_json::json!({"parts": []}))
            .unwrap();
        s.put_text_part(assistant_mid, "pong").unwrap();
        projector.poll_once();

        // 3. A live text chunk: the projector resolves the durable row id to
        // the wire message sequence and emits the SDK delta.
        projector.push_chunk_at(
            faktor_agent::ChunkEvent {
                session_id: s.id(),
                message_id: Some(assistant_mid),
                kind: "text",
                text: "po".into(),
            },
            1_500,
        );

        // 4. Durable session-row metadata mutation (the title-update path
        // journals no event): the row fingerprint is the trigger, and the
        // frame must be emitted exactly once.
        s.update_session_title("renamed").unwrap();
        projector.poll_once();

        // 5. Legal turn completion chain + session end.
        s.force_append_event(
            EventKind::ModelChunkReceived,
            AgentState::Streaming,
            None,
            None,
        )
        .unwrap();
        for state in [
            AgentState::Validating,
            AgentState::UpdatingMemory,
            AgentState::ReadyForNextTurn,
        ] {
            s.force_append_event(EventKind::TurnCompleted, state, None, None)
                .unwrap();
        }
        s.end_session().unwrap();
        projector.poll_once();

        let frames = projector.frames_after(0);
        let captured: Vec<serde_json::Value> = frames
            .iter()
            .map(|(_, f)| template_frames(&serde_json::to_value(f).unwrap()))
            .collect();
        let types: Vec<&str> = frames
            .iter()
            .map(|(_, f)| f.payload.kind.as_str())
            .collect();
        assert_eq!(
            types,
            [
                "session.created",
                "session.turn.open",
                "message.updated",
                "message.part.updated",
                "message.updated",
                "message.part.updated",
                "session.next.text.delta",
                "session.updated",
                "session.turn.close",
                "session.deleted",
            ],
            "journal-order projection"
        );
        // The rename frame carries the REAL durable row metadata, never a
        // synthesized title.
        let updated = frames
            .iter()
            .find(|(_, f)| f.payload.kind == "session.updated")
            .expect("session.updated frame");
        assert_eq!(updated.1.payload.properties["info"]["title"], "renamed");
        assert_eq!(updated.1.payload.properties["sessionID"], "1");
        // Idempotence: a second poll emits nothing new.
        let latest = frames.last().map(|(id, _)| *id).unwrap();
        projector.poll_once();
        assert!(projector.frames_after(latest).is_empty());

        let corpus_path = corpus_path();
        if std::env::var(GOLDEN_REGEN_ENV).as_deref() == Ok("1") {
            let corpus = serde_json::json!({
                "schema": "faktor.compat.sdk-global-frames/v1",
                "sdk": {"package": "kilo", "version": "7.5.6"},
                "union_types": vendored_union_types(),
                "scenarios": [{
                    "id": "journal-lifecycle-message-parts",
                    "frames": captured,
                }],
            });
            std::fs::write(
                &corpus_path,
                format!("{}\n", serde_json::to_string_pretty(&corpus).unwrap()),
            )
            .unwrap();
            eprintln!("regenerated {corpus_path:?} ({GOLDEN_REGEN_ENV}=1)");
            return;
        }

        let raw = std::fs::read_to_string(&corpus_path)
            .unwrap_or_else(|e| panic!("frame corpus at {corpus_path:?}: {e}"));
        let corpus: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(corpus["schema"], "faktor.compat.sdk-global-frames/v1");
        let declared = vendored_union_types();
        assert_eq!(
            corpus["union_types"],
            serde_json::json!(declared),
            "the corpus union list must be the vendored SDK's declared literals"
        );
        for target in [
            "session.created",
            "session.updated",
            "session.deleted",
            "message.updated",
            "message.part.updated",
        ] {
            assert!(
                declared.iter().any(|t| t == target),
                "{target} must be declared by the vendored SDK union"
            );
        }
        let scenario = &corpus["scenarios"][0];
        assert_eq!(scenario["id"], "journal-lifecycle-message-parts");
        let golden = scenario["frames"].as_array().unwrap();
        assert_eq!(golden.len(), captured.len(), "frame count changed");
        for (i, (expected, actual)) in golden.iter().zip(captured.iter()).enumerate() {
            let kind = actual["payload"]["type"].as_str().unwrap();
            assert!(
                declared.iter().any(|t| t == kind),
                "frame {i} type {kind:?} is not declared by the vendored SDK union"
            );
            assert_template(expected, actual, &format!("frames[{i}]"));
        }
    }
}
