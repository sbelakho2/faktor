//! Native API module split from foundation_ext.rs.

use super::foundation_ext::{conflict, resolve_handle, unavailable};
use super::*;

// ---------------------------------------------------------------- worktrees
// F205/F206/F207: the daemon's ONE WorktreeManager surfaced over the native
// protocol. Every mutation goes through the manager's per-repository
// mutation lease; no route spawns git directly.

pub(crate) fn git_error(e: faktor_core::error::Error) -> Response {
    let (code, status) = match e.kind {
        faktor_core::error::ErrorKind::NotFound => ("not_found", StatusCode::NOT_FOUND),
        faktor_core::error::ErrorKind::Conflict => ("conflict", StatusCode::CONFLICT),
        faktor_core::error::ErrorKind::Permission => ("permission_denied", StatusCode::FORBIDDEN),
        faktor_core::error::ErrorKind::Malformed => ("malformed", StatusCode::BAD_REQUEST),
        _ => ("git_error", StatusCode::INTERNAL_SERVER_ERROR),
    };
    wire_status(ApiError {
        code,
        message: e.message,
        http_status: status.as_u16(),
        retryable: e.retryable,
    })
}

#[allow(clippy::result_large_err)]
fn worktrees(state: &AppState) -> Result<&std::sync::Arc<faktor_git::WorktreeManager>, Response> {
    state
        .deps
        .worktrees
        .as_ref()
        .ok_or_else(|| unavailable("the git worktree authority is not wired in this host"))
}

#[allow(clippy::result_large_err)]
pub(crate) fn session_workspace_root(
    state: &AppState,
    handle: &faktor_session::SessionHandle,
) -> Result<std::path::PathBuf, Response> {
    let row = handle.row().map_err(|e| {
        wire_status(ApiError {
            code: "internal",
            message: format!("session row: {e}"),
            http_status: 500,
            retryable: false,
        })
    })?;
    state
        .deps
        .session
        .store()
        .workspace_root(row.workspace_id)
        .map_err(|e| {
            wire_status(ApiError {
                code: "internal",
                message: format!("workspace root: {e}"),
                http_status: 500,
                retryable: false,
            })
        })?
        .map(std::path::PathBuf::from)
        .ok_or_else(|| {
            conflict(
                "workspace_unavailable",
                "the session's workspace has no durable root",
            )
        })
}

fn worktree_json(
    wt: &faktor_git::Worktree,
    validation: Option<&faktor_git::Validation>,
) -> serde_json::Value {
    serde_json::json!({
        "id": wt.id.raw().to_string(),
        "path": wt.path.to_string_lossy(),
        "branch": wt.branch,
        "workspaceRoot": wt.workspace_root.to_string_lossy(),
        "ownerSession": wt.owner_session.map(|s| s.raw()),
        "validation": validation.map(|v| serde_json::json!({
            "exists": v.exists,
            "hasGitFile": v.has_git_file,
            "branch": v.branch,
            "clean": v.clean,
        })),
    })
}

#[allow(clippy::result_large_err)]
async fn find_worktree(
    manager: &faktor_git::WorktreeManager,
    root: &std::path::Path,
    worktree_id: u64,
) -> Result<faktor_git::Worktree, Response> {
    let rows = manager.discover(root).await.map_err(git_error)?;
    rows.into_iter()
        .find(|wt| wt.id.raw() == worktree_id)
        .ok_or_else(|| {
            wire_status(ApiError {
                code: "worktree_not_found",
                message: format!("worktree {worktree_id} is not registered in this workspace"),
                http_status: 404,
                retryable: false,
            })
        })
}

/// `GET|POST /native/session/{id}/worktrees` — discover or create a linked
/// worktree for the session's workspace. Creation validates the branch/name
/// and serializes on the repository mutation lease inside the manager.
pub(crate) async fn native_worktrees(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<NativeWorktreeCreateBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let handle = match resolve_handle(&state, &id) {
        Ok(handle) => handle,
        Err(r) => return r,
    };
    let manager = match worktrees(&state) {
        Ok(manager) => manager,
        Err(r) => return r,
    };
    let root = match session_workspace_root(&state, &handle) {
        Ok(root) => root,
        Err(r) => return r,
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid worktree body")),
    };
    match manager
        .create(&root, &body.branch, &body.name, handle.id())
        .await
    {
        Ok(wt) => Json(serde_json::json!({
            "worktree": worktree_json(&wt, None),
            "mutationLock": "repository lease acquired for the create",
        }))
        .into_response(),
        Err(e) => git_error(e),
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NativeWorktreeCreateBody {
    pub branch: String,
    pub name: String,
}

/// `GET /native/session/{id}/worktrees` is the discover route; this is the
/// same handler split for the router's get/post pair.
pub(crate) async fn native_worktrees_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let handle = match resolve_handle(&state, &id) {
        Ok(handle) => handle,
        Err(r) => return r,
    };
    let manager = match worktrees(&state) {
        Ok(manager) => manager,
        Err(r) => return r,
    };
    let root = match session_workspace_root(&state, &handle) {
        Ok(root) => root,
        Err(r) => return r,
    };
    let rows = match manager.discover(&root).await {
        Ok(rows) => rows,
        Err(e) => return git_error(e),
    };
    let mut out = Vec::with_capacity(rows.len());
    for wt in rows {
        let validation = manager.validate(&wt).await.ok();
        out.push(worktree_json(&wt, validation.as_ref()));
    }
    Json(serde_json::json!({ "worktrees": out })).into_response()
}

/// `DELETE /native/session/{id}/worktrees/{worktree_id}` — remove a linked
/// worktree through the manager (git worktree remove + metadata, leased).
pub(crate) async fn native_worktree_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, worktree_id)): Path<(String, u64)>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let handle = match resolve_handle(&state, &id) {
        Ok(handle) => handle,
        Err(r) => return r,
    };
    let manager = match worktrees(&state) {
        Ok(manager) => manager,
        Err(r) => return r,
    };
    let root = match session_workspace_root(&state, &handle) {
        Ok(root) => root,
        Err(r) => return r,
    };
    let wt = match find_worktree(manager, &root, worktree_id).await {
        Ok(wt) => wt,
        Err(r) => return r,
    };
    match manager.remove(&wt).await {
        Ok(()) => {
            Json(serde_json::json!({ "removed": true, "worktreeId": worktree_id })).into_response()
        }
        Err(e) => git_error(e),
    }
}

/// `POST /native/session/{id}/worktrees/{worktree_id}/validate` — live
/// validation (existence, `.git`, branch, cleanliness).
pub(crate) async fn native_worktree_validate(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, worktree_id)): Path<(String, u64)>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let handle = match resolve_handle(&state, &id) {
        Ok(handle) => handle,
        Err(r) => return r,
    };
    let manager = match worktrees(&state) {
        Ok(manager) => manager,
        Err(r) => return r,
    };
    let root = match session_workspace_root(&state, &handle) {
        Ok(root) => root,
        Err(r) => return r,
    };
    let wt = match find_worktree(manager, &root, worktree_id).await {
        Ok(wt) => wt,
        Err(r) => return r,
    };
    match manager.validate(&wt).await {
        Ok(v) => Json(serde_json::json!({
            "worktreeId": worktree_id,
            "valid": v.exists && v.has_git_file,
            "validation": {
                "exists": v.exists,
                "hasGitFile": v.has_git_file,
                "branch": v.branch,
                "clean": v.clean,
            }
        }))
        .into_response(),
        Err(e) => git_error(e),
    }
}

/// `POST /native/session/{id}/worktrees/repair` — prune vanished worktrees
/// and repair metadata for the workspace.
pub(crate) async fn native_worktrees_repair(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let handle = match resolve_handle(&state, &id) {
        Ok(handle) => handle,
        Err(r) => return r,
    };
    let manager = match worktrees(&state) {
        Ok(manager) => manager,
        Err(r) => return r,
    };
    let root = match session_workspace_root(&state, &handle) {
        Ok(root) => root,
        Err(r) => return r,
    };
    match manager.repair(&root).await {
        Ok(notes) => Json(serde_json::json!({ "repaired": true, "notes": notes })).into_response(),
        Err(e) => git_error(e),
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NativeWorktreeTransferBody {
    pub session_id: u64,
}

/// `POST /native/session/{id}/worktrees/{worktree_id}/transfer` — the
/// deliberate durable ownership move to another session.
pub(crate) async fn native_worktree_transfer(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, worktree_id)): Path<(String, u64)>,
    body: Result<Json<NativeWorktreeTransferBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let handle = match resolve_handle(&state, &id) {
        Ok(handle) => handle,
        Err(r) => return r,
    };
    let manager = match worktrees(&state) {
        Ok(manager) => manager,
        Err(r) => return r,
    };
    let root = match session_workspace_root(&state, &handle) {
        Ok(root) => root,
        Err(r) => return r,
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid worktree transfer body")),
    };
    let new_owner = faktor_core::id::SessionId::new(body.session_id);
    if state
        .deps
        .session
        .get_session(new_owner)
        .ok()
        .flatten()
        .is_none()
    {
        return wire_status(ApiError {
            code: "session_not_found",
            message: format!("transfer target session {} does not exist", body.session_id),
            http_status: 404,
            retryable: false,
        });
    }
    let wt = match find_worktree(manager, &root, worktree_id).await {
        Ok(wt) => wt,
        Err(r) => return r,
    };
    match manager.transfer(&wt, new_owner).await {
        Ok(()) => Json(serde_json::json!({
            "transferred": true,
            "worktreeId": worktree_id,
            "ownerSession": body.session_id,
        }))
        .into_response(),
        Err(e) => git_error(e),
    }
}

/// `POST /native/session/{id}/repo/lock` — acquire the repository's durable
/// mutation lease and hold it until the matching DELETE (or daemon
/// shutdown). Same-repo acquisitions serialize; unrelated repos stay
/// concurrent (the lease is per common git dir).
pub(crate) async fn native_repo_lock(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let handle = match resolve_handle(&state, &id) {
        Ok(handle) => handle,
        Err(r) => return r,
    };
    let manager = match worktrees(&state) {
        Ok(manager) => manager,
        Err(r) => return r,
    };
    let root = match session_workspace_root(&state, &handle) {
        Ok(root) => root,
        Err(r) => return r,
    };
    match manager
        .acquire_mutation_guard(&root, faktor_terminal::ProcessOwner::Session(handle.id()))
        .await
    {
        Ok(guard) => {
            let token = guard.lease().owner.clone();
            let lease = serde_json::json!({
                "token": token,
                "repo": guard.repo().to_string_lossy(),
                "pid": guard.lease().pid,
                "startedMs": guard.lease().started_ms,
                "purpose": guard.lease().purpose,
            });
            state
                .deps
                .repo_locks
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(token.clone(), guard);
            Json(serde_json::json!({ "acquired": true, "lease": lease })).into_response()
        }
        Err(e) => git_error(e),
    }
}

/// `DELETE /native/session/{id}/repo/lock/{token}` — release a held lease.
pub(crate) async fn native_repo_unlock(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, token)): Path<(String, String)>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    if let Err(r) = resolve_handle(&state, &id) {
        return r;
    }
    let removed = state
        .deps
        .repo_locks
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&token);
    match removed {
        Some(guard) => {
            let repo = guard.repo().to_string_lossy().into_owned();
            drop(guard);
            Json(serde_json::json!({ "released": true, "repo": repo })).into_response()
        }
        None => wire_status(ApiError {
            code: "lock_not_found",
            message: format!("no held repository lease {token}"),
            http_status: 404,
            retryable: false,
        }),
    }
}
