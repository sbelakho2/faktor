//! Native MCP lifecycle surface: stdio children and Streamable HTTP
//! sessions, catalog discovery, OAuth/token binding and typed transport
//! failures. Remote sessions ride the ONE checked egress authority; no local
//! process exists for them. Byte-for-byte protocol rules and typed errors
//! live in `faktor_mcp::remote`.
#![allow(clippy::result_large_err)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use faktor_core::error::{Error, ErrorKind};
use faktor_foundation::customization::{McpServerDraft, McpServerRecord};
use faktor_foundation::Foundation;
use faktor_mcp::remote::{
    RemoteCapabilities, RemoteMcpConfig, RemoteMcpServer, SecretToken, AUTH_FORBIDDEN_CODE,
    AUTH_REQUIRED_CODE, CAPABILITY_UNSUPPORTED_CODE, JSONRPC_ERROR_CODE, PROTOCOL_CODE,
    SESSION_EXPIRED_CODE, TRANSPORT_CODE,
};
use faktor_mcp::{McpConfig, McpConnection, McpServer};
use faktor_terminal::ProcessSupervisor;

use super::foundation::foundation_error;
use super::foundation_ext::{conflict, foundation};
use super::*;

// ------------------------------------------------------------------- MCP

/// Owns the live MCP connections for this daemon. Stdio starts are
/// serialized and explicit; stopping kills the child's process group through
/// the daemon's ONE supervisor. Streamable HTTP connections are supervised by
/// the checked egress client (destination allowlist, no environment proxy,
/// full-body secret scan) and carry no local process.
pub struct McpRuntime {
    supervisor: Option<Arc<ProcessSupervisor>>,
    live: tokio::sync::Mutex<HashMap<String, Arc<McpConnection>>>,
    /// TEST-ONLY loopback client (the production constructor always builds
    /// the checked external-address client inside `RemoteMcpServer`).
    #[cfg(test)]
    remote_client: Option<faktor_provider::egress::CheckedHttpClient>,
}

impl McpRuntime {
    pub fn new(supervisor: Option<Arc<ProcessSupervisor>>) -> Self {
        Self {
            supervisor,
            live: tokio::sync::Mutex::new(HashMap::new()),
            #[cfg(test)]
            remote_client: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_remote_client(
        supervisor: Option<Arc<ProcessSupervisor>>,
        client: faktor_provider::egress::CheckedHttpClient,
    ) -> Self {
        Self {
            supervisor,
            live: tokio::sync::Mutex::new(HashMap::new()),
            remote_client: Some(client),
        }
    }

    pub(crate) async fn start(
        &self,
        record: &McpServerRecord,
        env: Vec<(String, String)>,
        vault: Option<&faktor_foundation::SecretVault>,
    ) -> Result<Vec<serde_json::Value>, Error> {
        if !record.trusted {
            return Err(mcp_provider_error(
                "mcp_untrusted",
                "this MCP server is untrusted; review executable/origin and trust it before starting",
            ));
        }
        let mut live = self.live.lock().await;
        if let Some(connection) = live.get(&record.id) {
            if connection.is_alive() {
                let tools = connection.list_tools().await?;
                let tools = tools_json_bounded(tools, record.output_cap_bytes)
                    .map_err(|message| mcp_provider_error("mcp_tools_failed", message))?;
                return Ok(tools);
            }
            live.remove(&record.id);
        }
        let connection = match record.transport.as_str() {
            "stdio" => {
                let supervisor = self.supervisor.clone().ok_or_else(|| {
                    mcp_provider_error(
                        "mcp_supervisor_unavailable",
                        "the daemon process supervisor is not wired",
                    )
                })?;
                let server = McpServer::connect(
                    McpConfig {
                        name: record.name.clone(),
                        command: record.command.clone(),
                        args: record.args.clone(),
                        env,
                    },
                    supervisor,
                )
                .await?;
                McpConnection::Stdio(server)
            }
            "streamable_http" => {
                let endpoint = record.endpoint.clone().ok_or_else(|| {
                    mcp_provider_error(
                        "mcp_endpoint_missing",
                        "this streamable_http MCP server has no endpoint; save the spec again",
                    )
                })?;
                let token = match record.auth_token_ref.as_deref() {
                    Some(handle) => {
                        let vault = vault.ok_or_else(|| {
                            mcp_provider_error(
                                AUTH_REQUIRED_CODE,
                                "the server has a stored credential but the vault is not wired",
                            )
                        })?;
                        let bytes = vault.reveal(handle).map_err(|error| {
                            mcp_provider_error(
                                AUTH_REQUIRED_CODE,
                                format!(
                                    "the stored MCP credential is unavailable; reconnect OAuth: {error}"
                                ),
                            )
                        })?;
                        Some(SecretToken::from_bytes(bytes).map_err(|error| {
                            mcp_provider_error(AUTH_REQUIRED_CODE, error.message)
                        })?)
                    }
                    None => None,
                };
                let mut config = RemoteMcpConfig::new(record.name.clone(), endpoint);
                config.token = token;
                // Re-clamp even though save validates 100..=600000: a
                // tampered durable row must not create an unbounded deadline.
                config.timeout =
                    Duration::from_millis(record.timeout_ms.unwrap_or(30_000).clamp(100, 600_000));
                config.output_cap_bytes = record
                    .output_cap_bytes
                    .unwrap_or(16 * 1024 * 1024)
                    .clamp(1_024, 64 * 1024 * 1024)
                    as usize;
                #[cfg(test)]
                let remote = match &self.remote_client {
                    Some(client) => {
                        RemoteMcpServer::connect_with_client(config, client.clone()).await?
                    }
                    None => RemoteMcpServer::connect(config).await?,
                };
                #[cfg(not(test))]
                let remote = RemoteMcpServer::connect(config).await?;
                McpConnection::Remote(remote)
            }
            other => {
                return Err(mcp_provider_error(
                    "mcp_transport_invalid",
                    format!("unsupported MCP transport {other:?}"),
                ))
            }
        };
        let connection = Arc::new(connection);
        let tools = match connection.list_tools().await {
            Ok(tools) => tools,
            Err(error) => {
                connection.close().await;
                return Err(error);
            }
        };
        let tools = match tools_json_bounded(tools, record.output_cap_bytes) {
            Ok(tools) => tools,
            Err(message) => {
                connection.close().await;
                return Err(mcp_provider_error("mcp_tools_failed", message));
            }
        };
        live.insert(record.id.clone(), connection);
        Ok(tools)
    }

    pub(crate) async fn stop(&self, id: &str) -> bool {
        let connection = {
            let mut live = self.live.lock().await;
            live.remove(id)
        };
        if let Some(connection) = connection {
            connection.close().await;
            true
        } else {
            false
        }
    }

    pub(crate) async fn tools(
        &self,
        id: &str,
        cap: Option<u64>,
    ) -> Result<Option<Vec<serde_json::Value>>, Error> {
        let connection = {
            let live = self.live.lock().await;
            live.get(id).cloned()
        };
        match connection {
            Some(connection) if connection.is_alive() => {
                let tools = connection.list_tools().await?;
                let values = tools_json(tools)
                    .map_err(|message| mcp_provider_error("mcp_tools_failed", message))?;
                if let Some(cap) = cap {
                    let bytes = serde_json::to_vec(&values)
                        .map_err(|e| {
                            mcp_provider_error(
                                "mcp_tools_failed",
                                format!("tools/list encoding: {e}"),
                            )
                        })?
                        .len() as u64;
                    if bytes > cap {
                        return Err(Error::oversized(format!(
                            "tools/list output is {bytes} bytes, over the configured {cap}-byte cap"
                        )));
                    }
                }
                Ok(Some(values))
            }
            _ => Ok(None),
        }
    }

    pub(crate) async fn catalogs(
        &self,
        id: &str,
        cap: Option<u64>,
    ) -> Result<Option<serde_json::Value>, Error> {
        let connection = {
            let live = self.live.lock().await;
            live.get(id).cloned()
        };
        let Some(connection) = connection.filter(|connection| connection.is_alive()) else {
            return Ok(None);
        };
        let mut notes = Vec::new();
        let resources = match connection.list_resources().await {
            Ok(resources) => resources,
            Err(error) => {
                notes.push(format!("resources/list: {error}"));
                Vec::new()
            }
        };
        let resource_templates = match connection.list_resource_templates().await {
            Ok(templates) => templates,
            Err(error) => {
                notes.push(format!("resources/templates/list: {error}"));
                Vec::new()
            }
        };
        let prompts = match connection.list_prompts().await {
            Ok(prompts) => prompts,
            Err(error) => {
                notes.push(format!("prompts/list: {error}"));
                Vec::new()
            }
        };
        let value = serde_json::json!({
            "serverId": id,
            "transport": connection.transport(),
            "capabilities": connection.capabilities().await.map(|caps| caps.to_json()),
            "resources": resources,
            "resourceTemplates": resource_templates,
            "prompts": prompts,
            "notes": notes,
        });
        if let Some(cap) = cap {
            let bytes = serde_json::to_vec(&value)
                .map_err(|error| {
                    mcp_provider_error(
                        "mcp_resources_failed",
                        format!("resources encoding: {error}"),
                    )
                })?
                .len() as u64;
            if bytes > cap {
                return Err(Error::oversized(format!(
                    "resources/prompts output is {bytes} bytes, over the configured {cap}-byte cap; the catalog was not exposed"
                )));
            }
        }
        Ok(Some(value))
    }

    pub(crate) async fn health(&self, id: &str) -> Option<bool> {
        let live = self.live.lock().await;
        live.get(id).map(|connection| connection.is_alive())
    }

    pub(crate) async fn capabilities(&self, id: &str) -> Option<RemoteCapabilities> {
        let connection = {
            let live = self.live.lock().await;
            live.get(id).cloned()
        };
        match connection {
            Some(connection) if connection.is_alive() => connection.capabilities().await,
            _ => None,
        }
    }
}

fn mcp_provider_error(code: &str, message: impl Into<String>) -> Error {
    Error::new(
        ErrorKind::Provider {
            code: code.to_string(),
            retryable: false,
        },
        message,
    )
}

/// Map one remote/stdio MCP failure onto the frozen native error surface.
/// The stable `mcp_*` code is preserved for every typed remote outcome; no
/// token or vault material ever enters the message (the client sanitizes
/// server-reflected bearer values before this point).
fn mcp_failure(error: Error) -> Response {
    let retryable = error.retryable;
    let (code, status) = match &error.kind {
        ErrorKind::Provider { code, .. } => match code.as_str() {
            AUTH_REQUIRED_CODE => ("mcp_auth_required", StatusCode::CONFLICT),
            AUTH_FORBIDDEN_CODE => ("mcp_auth_forbidden", StatusCode::FORBIDDEN),
            CAPABILITY_UNSUPPORTED_CODE => ("mcp_capability_unsupported", StatusCode::CONFLICT),
            SESSION_EXPIRED_CODE => ("mcp_session_expired", StatusCode::CONFLICT),
            PROTOCOL_CODE => ("mcp_remote_protocol", StatusCode::BAD_GATEWAY),
            TRANSPORT_CODE | JSONRPC_ERROR_CODE => {
                ("mcp_remote_transport", StatusCode::BAD_GATEWAY)
            }
            "mcp_untrusted" => ("mcp_untrusted", StatusCode::CONFLICT),
            "mcp_endpoint_missing" | "mcp_transport_invalid" | "mcp_supervisor_unavailable" => {
                ("mcp_not_configured", StatusCode::CONFLICT)
            }
            "mcp_tools_failed" => ("mcp_tools_failed", StatusCode::CONFLICT),
            _ => ("mcp_start_failed", StatusCode::CONFLICT),
        },
        ErrorKind::Timeout => ("mcp_timeout", StatusCode::GATEWAY_TIMEOUT),
        ErrorKind::Oversized => ("mcp_oversized", StatusCode::PAYLOAD_TOO_LARGE),
        ErrorKind::Permission => ("mcp_egress_denied", StatusCode::FORBIDDEN),
        ErrorKind::Malformed => ("mcp_malformed", StatusCode::BAD_REQUEST),
        ErrorKind::Network => ("mcp_network", StatusCode::BAD_GATEWAY),
        _ => ("mcp_start_failed", StatusCode::CONFLICT),
    };
    (
        status,
        Json(serde_json::json!({
            "error": {
                "code": code,
                "message": error.message,
                "retryable": retryable,
            }
        })),
    )
        .into_response()
}

fn tools_json(tools: Vec<faktor_mcp::McpTool>) -> Result<Vec<serde_json::Value>, String> {
    Ok(tools
        .into_iter()
        .map(|t| {
            serde_json::json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": t.input_schema,
            })
        })
        .collect())
}

fn tools_json_bounded(
    tools: Vec<faktor_mcp::McpTool>,
    cap: Option<u64>,
) -> Result<Vec<serde_json::Value>, String> {
    let values = tools_json(tools)?;
    if let Some(cap) = cap {
        let bytes = serde_json::to_vec(&values)
            .map_err(|e| format!("tools/list encoding: {e}"))?
            .len() as u64;
        if bytes > cap {
            return Err(format!(
                "tools/list output is {bytes} bytes, over the configured {cap}-byte cap; the catalog was not exposed"
            ));
        }
    }
    Ok(values)
}

fn resolve_mcp_env(
    foundation: &Foundation,
    entries: &[(String, String)],
) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for (name, reference) in entries {
        if reference.is_empty() {
            if let Ok(value) = std::env::var(name) {
                out.push((name.clone(), value));
            }
            continue;
        }
        if let Some(rest) = reference.strip_prefix("vault://") {
            let bytes = foundation
                .vault
                .reveal(&format!("vault://{rest}"))
                .map_err(|e| format!("environment reference for {name}: {e}"))?;
            out.push((name.clone(), String::from_utf8_lossy(&bytes).into_owned()));
        } else if let Some(env_name) = reference.strip_prefix("env:") {
            let value = std::env::var(env_name)
                .map_err(|_| format!("environment reference for {name} is unset"))?;
            out.push((name.clone(), value));
        } else {
            return Err(format!(
                "environment entry {name:?} is not an opaque secret reference"
            ));
        }
    }
    Ok(out)
}

pub(super) fn mcp_view(
    record: &McpServerRecord,
    health: Option<bool>,
    tools: Option<Vec<serde_json::Value>>,
    capabilities: Option<RemoteCapabilities>,
) -> serde_json::Value {
    let state = match health {
        Some(true) => "ready",
        Some(false) => "failed",
        None => "stopped",
    };
    let remote_configured = record.transport == "streamable_http" && record.endpoint.is_some();
    let remote_reason = if remote_configured {
        "streamable HTTP is negotiated at start: initialize negotiates the protocol version and capabilities, and the checked egress authority (destination allowlist, no environment proxy, outbound secret scan) carries every request"
    } else {
        "this record is a stdio server; remote Streamable HTTP is available only for a configured streamable_http server"
    };
    serde_json::json!({
        "record": record,
        "health": {
            "state": state,
            "note": if health.is_none() {
                "not started; starting is an explicit action"
            } else if health == Some(true) {
                match record.transport.as_str() {
                    "streamable_http" => "initialize handshake completed over Streamable HTTP; the server answered tools/list",
                    _ => "initialize handshake completed and the child answered tools/list",
                }
            } else if record.transport == "streamable_http" {
                "the remote connection is not alive; reconnect explicitly"
            } else {
                "the child process is not alive"
            },
        },
        "tools": tools,
        "transport": record.transport,
        "capabilities": capabilities.map(|caps| caps.to_json()),
        "auth": {
            "state": record.auth_state,
            "account": record.auth_account,
            "scopes": record.auth_scopes,
            "hasToken": record.auth_token_ref.is_some(),
        },
        "bounds": {
            "timeoutMs": record.timeout_ms,
            "outputCapBytes": record.output_cap_bytes,
            "stderrCapBytes": record.stderr_cap_bytes,
        },
        "remoteTransport": {
            "supported": remote_configured,
            "configured": record.endpoint.is_some(),
            "endpoint": record.endpoint,
            "protocolVersions": faktor_mcp::remote::SUPPORTED_PROTOCOL_VERSIONS,
            "reason": remote_reason,
        },
        "logs": {
            "available": false,
            "reason": if record.transport == "streamable_http" {
                "remote MCP calls surface typed errors per request; no retained log buffer exists"
            } else {
                "stdio children run with captured pipes owned by the MCP client; no retained log buffer exists"
            },
        },
    })
}

/// `GET /native/mcp/servers` — bounded lifecycle listing with live health.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeMcpSearchQuery {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    limit: Option<u64>,
}

pub(crate) async fn native_mcp_servers(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<NativeMcpSearchQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(q) => q,
        Err(_) => return wire_status(malformed_body("invalid query parameters")),
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let records = match foundation.customization.mcp_servers() {
        Ok(records) => records,
        Err(e) => return foundation_error(e),
    };
    let needle = q.q.as_deref().unwrap_or("").to_ascii_lowercase();
    let limit = q.limit.unwrap_or(500).clamp(1, 500) as usize;
    let mut out = Vec::new();
    for record in records {
        if !needle.is_empty() {
            let haystack =
                format!("{} {} {}", record.id, record.name, record.command).to_ascii_lowercase();
            if !haystack.contains(&needle) {
                continue;
            }
        }
        let health = state.deps.mcp_runtime.health(&record.id).await;
        let tools = if health == Some(true) {
            state
                .deps
                .mcp_runtime
                .tools(&record.id, record.output_cap_bytes)
                .await
                .unwrap_or_default()
        } else {
            None
        };
        let capabilities = if health == Some(true) {
            state.deps.mcp_runtime.capabilities(&record.id).await
        } else {
            None
        };
        out.push(mcp_view(&record, health, tools, capabilities));
        if out.len() >= limit {
            break;
        }
    }
    Json(serde_json::json!({ "servers": out })).into_response()
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeMcpSaveBody {
    #[serde(flatten)]
    draft: McpServerDraft,
    #[serde(default, rename = "expectedRevision")]
    expected_revision: Option<u64>,
}

/// `POST /native/mcp/servers` — validated stdio spec; never starts.
pub(crate) async fn native_mcp_server_save(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<NativeMcpSaveBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid MCP server body")),
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation
        .customization
        .save_mcp_server(&req.draft, req.expected_revision)
    {
        Ok(record) => Json(mcp_view(&record, None, None, None)).into_response(),
        Err(e) => foundation_error(e),
    }
}

/// Start or reconnect one saved MCP record. Stdio starts the child through
/// the ONE supervisor; streamable_http resolves the bearer credential from
/// the vault and opens the checked-egress session. Both are explicit actions.
async fn start_mcp_record(
    state: &AppState,
    foundation: &Foundation,
    record: &McpServerRecord,
) -> Result<Vec<serde_json::Value>, Response> {
    if !record.enabled {
        return Err(conflict(
            "mcp_disabled",
            "this MCP server is saved but disabled; enable it before starting",
        ));
    }
    let env = if record.transport == "stdio" {
        resolve_mcp_env(foundation, &record.env)
            .map_err(|message| conflict("mcp_environment", message))?
    } else {
        Vec::new()
    };
    let timeout = Duration::from_millis(record.timeout_ms.unwrap_or(10_000).clamp(100, 600_000));
    match tokio::time::timeout(
        timeout,
        state
            .deps
            .mcp_runtime
            .start(record, env, Some(&foundation.vault)),
    )
    .await
    {
        Ok(Ok(tools)) => Ok(tools),
        Ok(Err(error)) => Err(mcp_failure(error)),
        Err(_) => {
            let _ = state.deps.mcp_runtime.stop(&record.id).await;
            Err(mcp_failure(Error::timeout(format!(
                "start exceeded the configured {} ms bound; the connection was closed (no tool call was admitted)",
                timeout.as_millis()
            ))))
        }
    }
}

/// `POST /native/mcp/servers/{id}/start` — explicit start; untrusted or
/// unconfigured servers refuse typed, never a fake ready.
pub(crate) async fn native_mcp_server_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let record = match foundation.customization.mcp_server(&id) {
        Ok(Some(record)) => record,
        Ok(None) => return wire_status(not_found("MCP server")),
        Err(e) => return foundation_error(e),
    };
    match start_mcp_record(&state, foundation, &record).await {
        Ok(tools) => {
            let capabilities = state.deps.mcp_runtime.capabilities(&id).await;
            Json(mcp_view(&record, Some(true), Some(tools), capabilities)).into_response()
        }
        Err(response) => response,
    }
}

/// `POST /native/mcp/servers/{id}/stop`
pub(crate) async fn native_mcp_server_stop(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let record = match foundation.customization.mcp_server(&id) {
        Ok(Some(record)) => record,
        Ok(None) => return wire_status(not_found("MCP server")),
        Err(e) => return foundation_error(e),
    };
    let stopped = state.deps.mcp_runtime.stop(&id).await;
    Json(serde_json::json!({
        "record": record,
        "stopped": stopped,
        "health": { "state": "stopped" },
    }))
    .into_response()
}

/// `GET /native/mcp/servers/{id}/tools`
pub(crate) async fn native_mcp_server_tools(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    if !matches!(foundation.customization.mcp_server(&id), Ok(Some(_))) {
        return wire_status(not_found("MCP server"));
    }
    let cap = foundation
        .customization
        .mcp_server(&id)
        .ok()
        .flatten()
        .and_then(|record| record.output_cap_bytes);
    match state.deps.mcp_runtime.tools(&id, cap).await {
        Ok(Some(tools)) => {
            Json(serde_json::json!({ "serverId": id, "tools": tools })).into_response()
        }
        Ok(None) => conflict(
            "mcp_not_ready",
            "the server is not started; start it explicitly to discover tools",
        ),
        Err(error) => mcp_failure(error),
    }
}

// SECRET-FIELD-GATE-WIRE-DTO: MCP OAuth callback body; the token is written
// straight to the vault and only the opaque handle is persisted.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct NativeMcpAuthBody {
    /// `connect` | `reconnect` | `revoke`
    action: String,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    account: Option<String>,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    expected_revision: Option<u64>,
}

/// `GET /native/mcp/servers/{id}/auth` — the durable auth binding without
/// any token material.
pub(crate) async fn native_mcp_server_auth_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation.customization.mcp_server(&id) {
        Ok(Some(record)) => Json(serde_json::json!({
            "serverId": record.id,
            "transport": record.transport,
            "state": record.auth_state,
            "account": record.auth_account,
            "scopes": record.auth_scopes,
            "hasToken": record.auth_token_ref.is_some(),
            "revision": record.revision,
        }))
        .into_response(),
        Ok(None) => wire_status(not_found("MCP server")),
        Err(e) => foundation_error(e),
    }
}

/// `POST /native/mcp/servers/{id}/auth` — OAuth/token lifecycle. The token
/// is written to the vault FIRST and only its opaque handle is persisted.
/// A failed operation leaves the previous binding untouched; `revoke`
/// removes the vault material and returns to `signed_out`.
pub(crate) async fn native_mcp_server_auth(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<NativeMcpAuthBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid MCP auth body")),
    };
    if !matches!(body.action.as_str(), "connect" | "reconnect" | "revoke") {
        return wire_status(malformed_body(
            "action must be connect, reconnect or revoke",
        ));
    }
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let record = match foundation.customization.mcp_server(&id) {
        Ok(Some(record)) => record,
        Ok(None) => return wire_status(not_found("MCP server")),
        Err(e) => return foundation_error(e),
    };
    if record.transport != "streamable_http" {
        return conflict(
            "mcp_oauth_unsupported",
            "OAuth binding applies to streamable_http servers; stdio servers use environment secret references",
        );
    }
    if body.action == "revoke" {
        if let Some(handle) = record.auth_token_ref.clone() {
            if let Err(e) = foundation.vault.delete(&handle) {
                return foundation_error(e);
            }
        }
        return match foundation.customization.set_mcp_auth(
            &id,
            None,
            None,
            Vec::new(),
            body.expected_revision,
        ) {
            Ok(record) => Json(serde_json::json!({
                "serverId": record.id,
                "state": record.auth_state,
                "account": record.auth_account,
                "scopes": record.auth_scopes,
                "revoked": true,
            }))
            .into_response(),
            Err(e) => foundation_error(e),
        };
    }
    let Some(token) = body.access_token.as_deref() else {
        return wire_status(malformed_body(
            "connect/reconnect requires the OAuth access token from the auth callback",
        ));
    };
    if token.is_empty() || token.len() > 8_192 || token.chars().any(char::is_control) {
        return wire_status(malformed_body(
            "accessToken must be 1..=8192 non-control bytes",
        ));
    }
    let secret = match foundation.vault.store(
        "mcp_oauth",
        &format!("MCP OAuth token for {}", record.name),
        token.as_bytes(),
    ) {
        Ok(secret) => secret,
        Err(e) => return foundation_error(e),
    };
    // Reconnect rotates: the OLD material is revoked only after the new
    // binding is durably written.
    let previous = record.auth_token_ref.clone();
    match foundation.customization.set_mcp_auth(
        &id,
        Some(secret.handle.clone()),
        body.account,
        body.scopes,
        body.expected_revision,
    ) {
        Ok(record) => {
            if let Some(previous) = previous {
                if previous != secret.handle {
                    let _ = foundation.vault.delete(&previous);
                }
            }
            // A token rotation invalidates any live session built with the
            // previous credential; the next connect re-initializes.
            let _ = state.deps.mcp_runtime.stop(&id).await;
            Json(serde_json::json!({
                "serverId": record.id,
                "transport": record.transport,
                "state": record.auth_state,
                "account": record.auth_account,
                "scopes": record.auth_scopes,
                "hasToken": true,
                "revision": record.revision,
                "connected": false,
                "note": "credential authorized; call connect to open the remote session",
            }))
            .into_response()
        }
        Err(e) => {
            let _ = foundation.vault.delete(&secret.handle);
            foundation_error(e)
        }
    }
}

/// `POST /native/mcp/servers/{id}/connect` — open or reconnect a
/// streamable_http session through the checked egress authority. A stdio
/// record is a typed transport mismatch, never a fabricated remote state.
pub(crate) async fn native_mcp_server_connect(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let record = match foundation.customization.mcp_server(&id) {
        Ok(Some(record)) => record,
        Ok(None) => return wire_status(not_found("MCP server")),
        Err(e) => return foundation_error(e),
    };
    if record.transport != "streamable_http" {
        return conflict(
            "mcp_transport_mismatch",
            "connect applies to a streamable_http server; stdio servers are started explicitly",
        );
    }
    match start_mcp_record(&state, foundation, &record).await {
        Ok(tools) => {
            let capabilities = state.deps.mcp_runtime.capabilities(&id).await;
            Json(mcp_view(&record, Some(true), Some(tools), capabilities)).into_response()
        }
        Err(response) => response,
    }
}

/// `POST /native/mcp/servers/{id}/oauth/revoke` — revoke the vault material
/// and durably return the server to `signed_out`.
pub(crate) async fn native_mcp_server_oauth_revoke(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let record = match foundation.customization.mcp_server(&id) {
        Ok(Some(record)) => record,
        Ok(None) => return wire_status(not_found("MCP server")),
        Err(e) => return foundation_error(e),
    };
    if record.transport != "streamable_http" {
        return conflict(
            "mcp_oauth_unsupported",
            "OAuth binding applies to streamable_http servers only",
        );
    }
    if let Some(handle) = record.auth_token_ref.clone() {
        if let Err(e) = foundation.vault.delete(&handle) {
            return foundation_error(e);
        }
    }
    // Revocation closes the live session before the durable signed_out row.
    let _ = state.deps.mcp_runtime.stop(&id).await;
    match foundation
        .customization
        .set_mcp_auth(&id, None, None, Vec::new(), None)
    {
        Ok(record) => Json(serde_json::json!({
            "serverId": record.id,
            "state": record.auth_state,
            "revoked": true,
        }))
        .into_response(),
        Err(e) => foundation_error(e),
    }
}

/// `GET /native/mcp/servers/{id}/resources` — the started server's own
/// resources, resource templates and prompts. A method the adapter does not
/// implement is reported in `notes`; an empty catalog is never invented for a
/// failing server, and a server that is not started is a typed 409.
pub(crate) async fn native_mcp_server_resources(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let cap = match foundation.customization.mcp_server(&id) {
        Ok(Some(record)) => record.output_cap_bytes,
        Ok(None) => return wire_status(not_found("MCP server")),
        Err(e) => return foundation_error(e),
    };
    match state.deps.mcp_runtime.catalogs(&id, cap).await {
        Ok(Some(catalogs)) => Json(catalogs).into_response(),
        Ok(None) => conflict(
            "mcp_not_ready",
            "the server is not started; start it explicitly to read resources and prompts",
        ),
        Err(error) => mcp_failure(error),
    }
}

/// `DELETE /native/mcp/servers/{id}` — stops the child FIRST, then removes
/// the durable spec (a removed server can never keep running).
pub(crate) async fn native_mcp_server_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let stopped = state.deps.mcp_runtime.stop(&id).await;
    match foundation.customization.delete_mcp_server(&id) {
        Ok(()) => Json(serde_json::json!({ "deleted": true, "stopped": stopped })).into_response(),
        Err(e) => foundation_error(e),
    }
}

#[cfg(test)]
#[path = "mcp_remote_tests.rs"]
mod mcp_remote_tests;
