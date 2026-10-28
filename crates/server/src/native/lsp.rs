// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! Workspace-scoped language-server diagnostics and code actions (F225/F226).
//!
//! The daemon serves the SAME [`faktor_lsp::LspManager`] the semantic
//! fallback uses: one server per workspace, rooted at the durable workspace
//! root. A missing toolchain is a typed 409 (`lsp_unavailable`), a disabled
//! host is a typed 409 (`lsp_disabled`), and a publish that never arrives
//! inside the explicit wait is a typed 504 (`lsp_diagnostics_timeout`). No
//! route fabricates an empty diagnostic list for a server that never spoke.

use std::path::{Component, Path, PathBuf};

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use faktor_lsp::{config_for_root, file_uri, Diagnostic, LspClient, Position, Range};

use super::*;
use crate::api::AppState;

/// Hard cap on a document read for diagnostics/code actions.
const MAX_DOCUMENT_BYTES: u64 = 2 * 1024 * 1024;
/// Default diagnostics wait; bounded by the request query.
const DEFAULT_WAIT_MS: u64 = 10_000;
const MAX_WAIT_MS: u64 = 60_000;
/// Code-action request deadline.
const CODE_ACTION_TIMEOUT_MS: u64 = 15_000;

fn conflict(code: &str, message: String) -> Response {
    conflict_with(code, message, false)
}

fn conflict_with(code: &str, message: String, retryable: bool) -> Response {
    (
        StatusCode::CONFLICT,
        Json(json!({"error": {"code": code, "message": message, "retryable": retryable}})),
    )
        .into_response()
}

fn lsp_disabled() -> Response {
    conflict(
        "lsp_disabled",
        "the daemon hosts no language-server manager".into(),
    )
}

fn lsp_unavailable(reason: String) -> Response {
    conflict("lsp_unavailable", reason)
}

/// A start refusal whose underlying typed error says whether a retry can
/// succeed (a server that crashed/OOM-died at initialize is transient; a
/// protocol refusal is permanent). The wire flag mirrors that verdict so
/// clients never retry a permanent verdict and never give up on a transient
/// one.
fn lsp_unavailable_with(reason: String, retryable: bool) -> Response {
    conflict_with("lsp_unavailable", reason, retryable)
}

/// A language-server START failure is retryable exactly when its typed kind
/// says so (the LSP crate marks a server process that exited at initialize
/// as `Provider { code: "lsp", retryable: true }`; protocol refusals stay
/// false). The wire flag mirrors that verdict — the single place a start
/// refusal becomes an HTTP body.
fn start_failure_retryable(error: &faktor_core::Error) -> bool {
    matches!(
        &error.kind,
        faktor_core::error::ErrorKind::Provider { retryable, .. } if *retryable
    )
}

#[cfg(test)]
mod start_failure_tests {
    use faktor_core::error::{Error, ErrorKind};

    use super::start_failure_retryable;

    /// The wire's retryable flag must mirror the typed verdict in BOTH
    /// directions: a crashed server (transient) is retryable; a protocol
    /// refusal and a timeout are never silently retried.
    #[test]
    fn start_failure_retryable_mirrors_the_typed_verdict() {
        let crashed = Error::new(
            ErrorKind::Provider {
                code: "lsp".into(),
                retryable: true,
            },
            "lsp initialize: exited",
        );
        assert!(start_failure_retryable(&crashed));

        let refused = Error::new(
            ErrorKind::Provider {
                code: "lsp".into(),
                retryable: false,
            },
            "lsp initialize: -32603",
        );
        assert!(!start_failure_retryable(&refused));

        let missing = Error::new(ErrorKind::NotFound, "lsp spawn: ENOENT");
        assert!(!start_failure_retryable(&missing));

        let timed_out = Error::timeout("lsp initialize exceeded 30000ms");
        assert!(!start_failure_retryable(&timed_out));
    }
}

fn lsp_timeout(uri: &str, wait_ms: u64) -> Response {
    (
        StatusCode::GATEWAY_TIMEOUT,
        Json(json!({"error": {
            "code": "lsp_diagnostics_timeout",
            "message": format!("no publishDiagnostics for {uri} within {wait_ms}ms"),
            "retryable": true,
        }})),
    )
        .into_response()
}

fn bad_request(message: &str) -> Response {
    wire_status(malformed_body(message))
}

fn clean_component(component: Component<'_>) -> bool {
    matches!(component, Component::Normal(_) | Component::CurDir)
}

/// Resolve a caller path inside the durable workspace root. Absolute paths
/// are accepted only when they already live under the root; `..`, rooted
/// re-entry and symlink escapes are refused.
fn resolve_workspace_path(root: &Path, raw: &str) -> Result<PathBuf, String> {
    if raw.is_empty() || raw.len() > 8192 {
        return Err("path is empty or exceeds the 8192 byte bound".into());
    }
    let candidate = Path::new(raw);
    if candidate.components().any(|c| !clean_component(c)) {
        return Err(
            "path must stay inside the workspace (no `..`, root or prefix components)".into(),
        );
    }
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    let canonical_root =
        std::fs::canonicalize(root).map_err(|e| format!("workspace root is unreadable: {e}"))?;
    let canonical = std::fs::canonicalize(&joined).map_err(|e| {
        format!(
            "file {} does not resolve inside the workspace: {e}",
            joined.display()
        )
    })?;
    if !canonical.starts_with(&canonical_root) {
        return Err("path escapes the workspace root".into());
    }
    if !canonical.is_file() {
        return Err(format!("{} is not a file", canonical.display()));
    }
    Ok(canonical)
}

type LspResult<T> = Result<T, Box<Response>>;

async fn read_document(path: &Path) -> LspResult<String> {
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|e| Box::new(bad_request(&format!("file metadata failed: {e}"))))?;
    if metadata.len() > MAX_DOCUMENT_BYTES {
        return Err(Box::new(wire_status(ApiError {
            code: "document_too_large",
            message: format!(
                "document is {} bytes; the {} byte bound applies",
                metadata.len(),
                MAX_DOCUMENT_BYTES
            ),
            http_status: 413,
            retryable: false,
        })));
    }
    tokio::fs::read_to_string(path)
        .await
        .map_err(|e| Box::new(bad_request(&format!("file read failed: {e}"))))
}

struct ResolvedLsp {
    client: std::sync::Arc<LspClient>,
    uri: String,
    text: String,
    source: String,
}

async fn resolve_lsp(state: &AppState, session: &str, raw_path: &str) -> LspResult<ResolvedLsp> {
    let handle = match native_resolve_session(state, session) {
        Ok(handle) => handle,
        Err(response) => return Err(response),
    };
    let Some(manager) = state.deps.lsp.as_ref() else {
        return Err(Box::new(lsp_disabled()));
    };
    let identity = handle
        .identity()
        .map_err(|e| Box::new(wire_status(faktor_protocol::error::from_core(&e))))?;
    let root = state
        .deps
        .session
        .workspace_root(identity.workspace_id)
        .map_err(|e| Box::new(wire_status(faktor_protocol::error::from_core(&e))))?
        .ok_or_else(|| {
            Box::new(lsp_unavailable(format!(
                "workspace {} has no durable root",
                identity.workspace_id
            )))
        })?;
    // Canonicalize the workspace root ONCE: macOS `/tmp` is a symlink to
    // `/private/tmp`, and a language server publishes URIs against the root
    // it was initialized with. Serving the coded path under one spelling and
    // waiting for publishes under the other would time out on a healthy
    // server.
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    let path = resolve_workspace_path(&root, raw_path).map_err(|e| Box::new(bad_request(&e)))?;
    let Some(config) = config_for_root(&root) else {
        return Err(Box::new(lsp_unavailable(format!(
            "no language server is configured/installed for {}",
            root.display()
        ))));
    };
    let source = config.name.clone();
    let client = tokio::time::timeout(
        std::time::Duration::from_secs(180),
        manager.start(identity.workspace_id, config),
    )
    .await
    .map_err(|_| lsp_unavailable("language-server start exceeded the 180s bound".to_string()))?
    .map_err(|e| {
        lsp_unavailable_with(
            format!("language-server start failed: {e}"),
            start_failure_retryable(&e),
        )
    })?;
    let text = read_document(&path).await?;
    Ok(ResolvedLsp {
        client,
        uri: file_uri(&path),
        text,
        source,
    })
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LspDiagnosticsQuery {
    session: String,
    path: String,
    #[serde(default)]
    wait_ms: Option<u64>,
}

/// `GET /native/lsp/diagnostics?session=&path=&wait_ms=` — open/refresh the
/// exact document version, then wait for the server's publish for THIS
/// version. Returns the bounded snapshot verbatim (empty after a stale-clear
/// publish).
pub(crate) async fn native_lsp_diagnostics(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<LspDiagnosticsQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return bad_request("invalid diagnostics query (strict DTO)"),
    };
    let wait_ms = query
        .wait_ms
        .unwrap_or(DEFAULT_WAIT_MS)
        .clamp(1, MAX_WAIT_MS);
    let resolved = match resolve_lsp(&state, &query.session, &query.path).await {
        Ok(resolved) => resolved,
        Err(response) => return *response,
    };
    let baseline = resolved.client.diagnostics_sequence();
    let version = resolved
        .client
        .document_revision(&resolved.uri)
        .map(|revision| revision.version.saturating_add(1))
        .unwrap_or(1);
    if let Err(error) = resolved
        .client
        .did_open_version(&resolved.uri, &resolved.text, version)
    {
        return lsp_unavailable(format!("didOpen/didChange failed: {error}"));
    }
    let snapshot = resolved
        .client
        .wait_diagnostics_settled(
            &resolved.uri,
            baseline,
            Some(version),
            std::time::Duration::from_millis(250),
            std::time::Duration::from_millis(wait_ms),
            None,
        )
        .await;
    let Some(snapshot) = snapshot else {
        return lsp_timeout(&resolved.uri, wait_ms);
    };
    if snapshot.sequence <= baseline {
        return lsp_timeout(&resolved.uri, wait_ms);
    }
    Json(json!({
        "ok": true,
        "source": resolved.source,
        "uri": snapshot.uri,
        "path": query.path,
        "version": snapshot.version,
        "sequence": snapshot.sequence.to_string(),
        "truncated": snapshot.truncated,
        "diagnostics": snapshot.diagnostics,
    }))
    .into_response()
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WirePosition {
    line: u32,
    character: u32,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WireRange {
    start: WirePosition,
    end: WirePosition,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LspCodeActionsBody {
    session: String,
    path: String,
    range: WireRange,
    #[serde(default)]
    diagnostics: Option<Vec<Diagnostic>>,
}

/// `POST /native/lsp/code-actions` — bounded `textDocument/codeAction` for
/// the exact range. The response is a list of PROPOSALS; applying an edit is
/// the host editor's explicit action (this route never writes a file).
pub(crate) async fn native_lsp_code_actions(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<LspCodeActionsBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return bad_request("invalid code-action body (strict DTO)"),
    };
    let resolved = match resolve_lsp(&state, &body.session, &body.path).await {
        Ok(resolved) => resolved,
        Err(response) => return *response,
    };
    let range = Range {
        start: Position::new(body.range.start.line, body.range.start.character),
        end: Position::new(body.range.end.line, body.range.end.character),
    };
    if range.start > range.end {
        return bad_request("range start must not be after range end");
    }
    let version = resolved
        .client
        .document_revision(&resolved.uri)
        .map(|revision| revision.version)
        .unwrap_or(1);
    if let Err(error) = resolved
        .client
        .did_open_version(&resolved.uri, &resolved.text, version)
    {
        return lsp_unavailable(format!("didOpen/didChange failed: {error}"));
    }
    let diagnostics = body.diagnostics.unwrap_or_default();
    let actions = resolved
        .client
        .code_actions(
            &resolved.uri,
            range,
            &diagnostics,
            std::time::Duration::from_millis(CODE_ACTION_TIMEOUT_MS),
            None,
        )
        .await;
    match actions {
        Ok(actions) => Json(json!({
            "ok": true,
            "source": resolved.source,
            "uri": resolved.uri,
            "path": body.path,
            "actions": actions,
            "autoApplied": false,
        }))
        .into_response(),
        Err(error) if faktor_lsp::is_unsupported_error(&error) => {
            lsp_unavailable("the language server does not advertise textDocument/codeAction".into())
        }
        Err(error) => lsp_unavailable(format!("code action request failed: {error}")),
    }
}

/// Bound on a request path's parent depth (defense in depth for diagnostics).
#[allow(dead_code)]
fn depth(path: &Path) -> usize {
    path.components().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_paths_cannot_escape_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn f() {}").unwrap();
        assert!(resolve_workspace_path(root, "src/lib.rs").is_ok());
        assert!(resolve_workspace_path(root, "src/../src/lib.rs").is_err());
        assert!(resolve_workspace_path(root, "../outside.rs").is_err());
        assert!(resolve_workspace_path(root, "/etc/passwd").is_err());
        assert!(resolve_workspace_path(root, "src/missing.rs").is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/passwd", root.join("escape.rs")).unwrap();
            assert!(resolve_workspace_path(root, "escape.rs").is_err());
        }
    }
}
