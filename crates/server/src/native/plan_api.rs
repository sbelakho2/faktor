//! Native API module split from foundation_ext.rs.

use super::foundation_ext::{conflict, resolve_handle};
use super::worktrees_api::session_workspace_root;
use super::*;

// ------------------------------------------------------------ plan artifact

const PLAN_ARTIFACT_MAX_BYTES: usize = 1024 * 1024;

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct NativePlanArtifactBody {
    pub content: String,
    #[serde(default)]
    pub title: Option<String>,
}

#[allow(clippy::result_large_err)]
fn plan_artifact_dir(
    state: &AppState,
    handle: &faktor_session::SessionHandle,
) -> Result<std::path::PathBuf, Response> {
    let root = session_workspace_root(state, handle)?;
    Ok(root
        .join(".faktor")
        .join("plans")
        .join(handle.id().raw().to_string()))
}

/// `POST /native/session/{id}/plan` — freeze ONE named/versioned plan
/// artifact (F087). This route writes ONLY the computed artifact path; it
/// never runs tools, never edits source and never widens the session's
/// authority. The artifact is a durable file plus a fact carrying its
/// version/digest/approval state.
pub(crate) async fn native_plan_artifact_write(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<NativePlanArtifactBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid plan artifact body")),
    };
    if body.content.len() > PLAN_ARTIFACT_MAX_BYTES {
        return wire_status(ApiError {
            code: "oversized",
            message: format!("plan artifact exceeds {PLAN_ARTIFACT_MAX_BYTES} bytes"),
            http_status: 413,
            retryable: false,
        });
    }
    if body.content.trim().is_empty() {
        return wire_status(malformed_body("plan artifact content is empty"));
    }
    let handle = match resolve_handle(&state, &id) {
        Ok(handle) => handle,
        Err(r) => return r,
    };
    let dir = match plan_artifact_dir(&state, &handle) {
        Ok(dir) => dir,
        Err(r) => return r,
    };
    let previous = plan_latest(&handle);
    let version = previous
        .as_ref()
        .and_then(|p| p.get("version"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        + 1;
    let file = dir.join(format!("v{version}.md"));
    let write_result = std::fs::create_dir_all(&dir)
        .map_err(|e| e.to_string())
        .and_then(|_| {
            faktor_fs::atomic::atomic_replace(&file, body.content.as_bytes())
                .map(|_| ())
                .map_err(|e| e.to_string())
        });
    if let Err(e) = write_result {
        return wire_status(ApiError {
            code: "internal",
            message: format!("plan artifact write {}: {e}", file.display()),
            http_status: 500,
            retryable: false,
        });
    }
    let digest = faktor_foundation::hash_hex(body.content.as_bytes());
    let meta = serde_json::json!({
        "version": version,
        "title": body.title,
        "path": file.to_string_lossy(),
        "relativePath": format!(".faktor/plans/{}/v{version}.md", handle.id().raw()),
        "digest": digest,
        "bytes": body.content.len(),
        "frozen": true,
        "approved": false,
        "approvedAt": serde_json::Value::Null,
    });
    if let Err(e) = handle.upsert_memory_fact("plan_artifact", "latest", &meta.to_string()) {
        return api_err(&e);
    }
    Json(serde_json::json!({
        "artifactId": format!("plan-{}-v{version}", handle.id().raw()),
        "sessionId": id,
        "artifact": meta,
        "authorityUnchanged": true,
        "onlyNamedArtifact": true,
    }))
    .into_response()
}

fn plan_latest(handle: &faktor_session::SessionHandle) -> Option<serde_json::Value> {
    handle
        .memory_fact_get("plan_artifact", "latest")
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str(&raw).ok())
}

/// `GET /native/session/{id}/plan` — the frozen latest plan artifact and
/// its approval state (the exact bytes are returned bounded).
pub(crate) async fn native_plan_artifact_get(
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
    let Some(meta) = plan_latest(&handle) else {
        return wire_status(ApiError {
            code: "plan_artifact_absent",
            message: "this session has no frozen plan artifact".into(),
            http_status: 404,
            retryable: false,
        });
    };
    let path = meta
        .get("path")
        .and_then(|p| p.as_str())
        .unwrap_or_default()
        .to_string();
    let content = std::fs::read_to_string(&path).ok().map(|text| {
        if text.len() <= PLAN_ARTIFACT_MAX_BYTES {
            text
        } else {
            text.chars().take(PLAN_ARTIFACT_MAX_BYTES).collect()
        }
    });
    Json(serde_json::json!({
        "sessionId": id,
        "artifactId": format!("plan-{}-v{}", handle.id().raw(), meta.get("version").and_then(|v| v.as_u64()).unwrap_or(0)),
        "artifact": meta,
        "content": content,
    }))
    .into_response()
}

/// `POST /native/session/{id}/plan/{version}/approve` — record approval of
/// the frozen version. Approval is a receipt, NOT an authority grant: the
/// response states that the tool/preset envelope is unchanged.
pub(crate) async fn native_plan_artifact_approve(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, version)): Path<(String, u64)>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let handle = match resolve_handle(&state, &id) {
        Ok(handle) => handle,
        Err(r) => return r,
    };
    let Some(mut meta) = plan_latest(&handle) else {
        return wire_status(ApiError {
            code: "plan_artifact_absent",
            message: "this session has no frozen plan artifact".into(),
            http_status: 404,
            retryable: false,
        });
    };
    let current = meta.get("version").and_then(|v| v.as_u64()).unwrap_or(0);
    if current != version {
        return conflict(
            "plan_version_mismatch",
            format!("the frozen plan version is {current}, not {version}"),
        );
    }
    meta["approved"] = serde_json::json!(true);
    meta["approvedAt"] = serde_json::json!(now_ms());
    if let Err(e) = handle.upsert_memory_fact("plan_artifact", "latest", &meta.to_string()) {
        return api_err(&e);
    }
    Json(serde_json::json!({
        "sessionId": id,
        "version": version,
        "approved": true,
        "authorityUnchanged": true,
        "note": "approval is a receipt over the frozen artifact; it does not add tool, filesystem or preset authority",
    }))
    .into_response()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub(crate) async fn native_session_lineage(
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
    let edge = match handle.memory_fact_get("session_lineage", "parent") {
        Ok(Some(raw)) => {
            let raw: String = raw;
            match serde_json::from_str::<serde_json::Value>(&raw) {
                Ok(value) => Some(value),
                Err(e) => {
                    return wire_status(ApiError {
                        code: "corrupt_lineage",
                        message: format!("durable lineage fact is malformed: {e}"),
                        http_status: 500,
                        retryable: false,
                    })
                }
            }
        }
        Ok(None) => None,
        Err(e) => return api_err(&e),
    };
    Json(serde_json::json!({
        "sessionId": id,
        "isFork": edge.is_some(),
        "parentSessionId": edge.as_ref().and_then(|e| e.get("parentSessionId")).cloned(),
        "forkedAt": edge.as_ref().and_then(|e| e.get("forkedAt")).cloned(),
        "sourceHeadSeq": edge.as_ref().and_then(|e| e.get("sourceHeadSeq")).cloned(),
        "forkedFrom": edge.as_ref().and_then(|e| e.get("parentSessionId")).cloned(),
    }))
    .into_response()
}
