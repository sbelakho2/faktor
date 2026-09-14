//! SDK-shaped legacy REST surface (`/session/...`, `/permission/...`,
//! `/global/...`, `/question/...`, `/network/...`, `/config/...`, PTY and auth).

use crate::auth::ServerPassword;
use crate::global::GlobalEventBus;
use crate::permission::PendingPermission;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{sse::Event, IntoResponse, Response, Sse};
use axum::Json;
use faktor_core::capability::PermissionDecision;
use faktor_protocol::error::ApiError;
use futures_util::stream::Stream;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use super::v756::sdk_invalid_request;
use super::{submit_and_run, COMPAT_MUTATION_MODE};
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
    // legacy surface selects the direct mutation policy: the request must
    // never wait on the executor's synchronous shadow begin.
    let receipt = match submit_and_run(
        &state,
        sid,
        &prompt_text,
        &files,
        None,
        Some(COMPAT_MUTATION_MODE),
    )
    .await
    {
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
    // legacy surface selects the direct mutation policy (see `prompt`).
    let receipt = match submit_and_run(
        &state,
        sid,
        &prompt_text,
        &files,
        None,
        Some(COMPAT_MUTATION_MODE),
    )
    .await
    {
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

// ------------------------------------------------------------------ global SSE
// `GET /global/event?after=<n>` streams GlobalEvent envelopes as
// `id: <n>\ndata: <json>\n\n` frames (no `event:` field — the payload's
// `type` carries the discriminator). `after` is the resume cursor; oversized
// values are clamped to what the bounded ring can serve.

pub(crate) async fn global_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<GlobalEventsQuery>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let after = q.after.unwrap_or(0);
    let stream = global_stream(state.bus.clone(), after);
    Sse::new(stream)
        .keep_alive(
            axum::response::sse::KeepAlive::new()
                .interval(Duration::from_secs(HEARTBEAT_SECS))
                .text("keep-alive"),
        )
        .into_response()
}

pub(crate) fn global_stream(
    bus: Arc<GlobalEventBus>,
    after: u64,
) -> impl Stream<Item = Result<Event, std::convert::Infallible>> + Send + 'static {
    // Poll the bus for new frames (id > cursor), emit, then wait before the
    // next poll; heartbeats keep proxies alive. The bus cursors make the
    // poll idempotent across concurrent connections.
    futures_util::stream::unfold(
        (bus, after, VecDeque::<(u64, GlobalEvent)>::new()),
        |(bus, mut cursor, mut queue)| async move {
            if let Some((id, ge)) = queue.pop_front() {
                return Some((
                    Ok::<Event, std::convert::Infallible>(global_frame(id, ge)),
                    (bus, cursor, queue),
                ));
            }
            bus.poll_once();
            let frames = bus.frames_after(cursor);
            let mut batch = VecDeque::new();
            let mut advanced = false;
            for (id, ge) in frames {
                batch.push_back((id, ge));
                cursor = id;
                advanced = true;
            }
            if advanced {
                if let Some((id, ge)) = batch.pop_front() {
                    return Some((
                        Ok::<Event, std::convert::Infallible>(global_frame(id, ge)),
                        (bus, cursor, batch),
                    ));
                }
            }
            tokio::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
            Some((
                Ok::<Event, std::convert::Infallible>(sse_event_heartbeat()),
                (bus, cursor, queue),
            ))
        },
    )
}

pub(crate) fn global_frame(id: u64, ge: GlobalEvent) -> Event {
    let json = serde_json::to_string(&ge).unwrap_or_else(|_| "{}".into());
    Event::default().id(id.to_string()).data(json)
}
