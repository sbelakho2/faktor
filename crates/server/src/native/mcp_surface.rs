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
use axum::response::{Html, IntoResponse, Response};
use axum::Json;
use faktor_core::error::{Error, ErrorKind};
use faktor_foundation::customization::{McpServerDraft, McpServerRecord};
use faktor_foundation::Foundation;
use faktor_mcp::oauth::{
    OAuthClient, OAUTH_DENIED_CODE, OAUTH_DISCOVERY_CODE, OAUTH_EXPIRED_CODE,
    OAUTH_UNCONFIGURED_CODE,
};
use faktor_mcp::remote::{
    RemoteCapabilities, RemoteMcpConfig, RemoteMcpServer, AUTH_FORBIDDEN_CODE, AUTH_REQUIRED_CODE,
    CAPABILITY_UNSUPPORTED_CODE, JSONRPC_ERROR_CODE, PROTOCOL_CODE, SESSION_EXPIRED_CODE,
    TRANSPORT_CODE,
};
use faktor_mcp::{McpConfig, McpConnection, McpServer};
use faktor_terminal::ProcessSupervisor;
use tokio::sync::watch;

use super::foundation::foundation_error;
use super::foundation_ext::{conflict, foundation};
use super::*;

// ------------------------------------------------------------------- MCP

/// One server's lifecycle state. `Starting` is observed by every caller that
/// joins the same in-flight connect (single-flight); the map lock is only
/// held to reserve, observe or publish this value, never across I/O.
#[derive(Clone)]
enum SlotState {
    Starting,
    Ready(Arc<McpConnection>),
    Failed(Error),
}

struct ServerSlot {
    state: watch::Sender<SlotState>,
}

impl ServerSlot {
    fn new() -> Arc<Self> {
        let (state, _receiver) = watch::channel(SlotState::Starting);
        Arc::new(Self { state })
    }
}

fn ready_connection(slot: &ServerSlot) -> Option<Arc<McpConnection>> {
    match &*slot.state.borrow() {
        SlotState::Ready(connection) => Some(connection.clone()),
        _ => None,
    }
}

/// Owns the live MCP connections for this daemon. Stdio starts are explicit
/// and single-flight per server; stopping kills the child's process group
/// through the daemon's ONE supervisor. Streamable HTTP connections are
/// supervised by the checked egress client (destination allowlist, no
/// environment proxy, full-body secret scan) and carry no local process.
/// Each server has its own slot, so a slow remote handshake never blocks a
/// different server's start, stop or catalog read.
pub struct McpRuntime {
    supervisor: Option<Arc<ProcessSupervisor>>,
    live: tokio::sync::Mutex<HashMap<String, Arc<ServerSlot>>>,
    /// The OAuth engine used for discovery/login/token refresh/revocation.
    /// Production is external-address only; the local-fixture constructor
    /// installs the explicit loopback rule.
    oauth_client: OAuthClient,
    /// Bounded, single-use authorization-code flows awaiting the operator's
    /// loopback callback. Nothing here is durable: a daemon restart expires
    /// an unfinished flow and the operator starts again.
    pub(crate) oauth_flows: tokio::sync::Mutex<HashMap<String, super::mcp_oauth::PendingLogin>>,
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
            oauth_client: OAuthClient::production(),
            oauth_flows: tokio::sync::Mutex::new(HashMap::new()),
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
            oauth_client: OAuthClient::with_address_policy(
                faktor_provider::egress::EgressAddressPolicy::LOCAL,
            ),
            oauth_flows: tokio::sync::Mutex::new(HashMap::new()),
            remote_client: Some(client),
        }
    }

    /// The OAuth engine clone for a login/status/revocation request. The
    /// engine is configuration-only; each request builds its own checked
    /// client for the exact discovered origin.
    pub(crate) fn oauth_client(&self) -> OAuthClient {
        self.oauth_client.clone()
    }

    pub(crate) async fn start(
        &self,
        record: &McpServerRecord,
        env: Vec<(String, String)>,
        foundation: Option<&faktor_foundation::Foundation>,
    ) -> Result<Vec<serde_json::Value>, Error> {
        if !record.trusted {
            return Err(mcp_provider_error(
                "mcp_untrusted",
                "this MCP server is untrusted; review executable/origin and trust it before starting",
            ));
        }
        enum Role {
            Reuse(Arc<McpConnection>),
            Wait(watch::Receiver<SlotState>),
            Connect(Arc<ServerSlot>),
        }
        // Reserve or observe under the map lock, then release it before any
        // connect/initialize/discovery I/O. Different servers never share a
        // slot; concurrent starts of the same server share one connection.
        let role = {
            let mut live = self.live.lock().await;
            match live.get(&record.id).cloned() {
                Some(slot) => {
                    let state = slot.state.borrow().clone();
                    match state {
                        SlotState::Ready(connection) if connection.is_alive() => {
                            Role::Reuse(connection)
                        }
                        SlotState::Starting => Role::Wait(slot.state.subscribe()),
                        _ => {
                            live.remove(&record.id);
                            let slot = ServerSlot::new();
                            live.insert(record.id.clone(), slot.clone());
                            Role::Connect(slot)
                        }
                    }
                }
                None => {
                    let slot = ServerSlot::new();
                    live.insert(record.id.clone(), slot.clone());
                    Role::Connect(slot)
                }
            }
        };
        match role {
            Role::Reuse(connection) => self.discover(&connection, record).await,
            Role::Wait(mut receiver) => loop {
                if receiver.changed().await.is_err() {
                    return Err(Error::internal(format!(
                        "MCP server {} start coordinator disappeared",
                        record.id
                    )));
                }
                let state = receiver.borrow_and_update().clone();
                match state {
                    SlotState::Starting => continue,
                    SlotState::Ready(connection) => {
                        return self.discover(&connection, record).await
                    }
                    SlotState::Failed(error) => return Err(error),
                }
            },
            Role::Connect(slot) => {
                let connection = match self.connect(record, env, foundation).await {
                    Ok(connection) => Arc::new(connection),
                    Err(error) => {
                        self.fail_slot(&record.id, &slot, error.clone()).await;
                        return Err(error);
                    }
                };
                if !self.is_current(&record.id, &slot).await {
                    connection.close().await;
                    return Err(Error::cancelled());
                }
                let tools = match self.discover(&connection, record).await {
                    Ok(tools) => tools,
                    Err(error) => {
                        connection.close().await;
                        self.fail_slot(&record.id, &slot, error.clone()).await;
                        return Err(error);
                    }
                };
                if !self
                    .publish_ready(&record.id, &slot, connection.clone())
                    .await
                {
                    // stop() won the race: never publish a connection the
                    // runtime no longer tracks.
                    connection.close().await;
                    return Err(Error::cancelled());
                }
                Ok(tools)
            }
        }
    }

    async fn connect(
        &self,
        record: &McpServerRecord,
        env: Vec<(String, String)>,
        foundation: Option<&faktor_foundation::Foundation>,
    ) -> Result<McpConnection, Error> {
        match record.transport.as_str() {
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
                Ok(McpConnection::Stdio(server))
            }
            "streamable_http" => {
                let endpoint = record.endpoint.clone().ok_or_else(|| {
                    mcp_provider_error(
                        "mcp_endpoint_missing",
                        "this streamable_http MCP server has no endpoint; save the spec again",
                    )
                })?;
                let credential = if record.auth_token_ref.is_some() {
                    let foundation = foundation.ok_or_else(|| {
                        mcp_provider_error(
                            AUTH_REQUIRED_CODE,
                            "the server has a stored credential but the vault is not wired",
                        )
                    })?;
                    super::mcp_oauth::load_credential(foundation, record, self.oauth_client())?
                } else {
                    super::mcp_oauth::LoadedCredential {
                        token: None,
                        auth: None,
                    }
                };
                let mut config = RemoteMcpConfig::new(record.name.clone(), endpoint);
                config.token = credential.token;
                config.auth = credential.auth;
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
                Ok(McpConnection::Remote(remote))
            }
            other => Err(mcp_provider_error(
                "mcp_transport_invalid",
                format!("unsupported MCP transport {other:?}"),
            )),
        }
    }

    async fn discover(
        &self,
        connection: &McpConnection,
        record: &McpServerRecord,
    ) -> Result<Vec<serde_json::Value>, Error> {
        let tools = connection.list_tools().await?;
        tools_json_bounded(tools, record.output_cap_bytes)
            .map_err(|message| mcp_provider_error("mcp_tools_failed", message))
    }

    async fn is_current(&self, id: &str, slot: &Arc<ServerSlot>) -> bool {
        let live = self.live.lock().await;
        live.get(id)
            .is_some_and(|current| Arc::ptr_eq(current, slot))
    }

    async fn publish_ready(
        &self,
        id: &str,
        slot: &Arc<ServerSlot>,
        connection: Arc<McpConnection>,
    ) -> bool {
        let live = self.live.lock().await;
        if live
            .get(id)
            .is_some_and(|current| Arc::ptr_eq(current, slot))
        {
            slot.state.send_replace(SlotState::Ready(connection));
            true
        } else {
            false
        }
    }

    async fn fail_slot(&self, id: &str, slot: &Arc<ServerSlot>, error: Error) {
        let mut live = self.live.lock().await;
        if live
            .get(id)
            .is_some_and(|current| Arc::ptr_eq(current, slot))
        {
            slot.state.send_replace(SlotState::Failed(error));
            live.remove(id);
        }
    }

    pub(crate) async fn stop(&self, id: &str) -> bool {
        let slot = {
            let mut live = self.live.lock().await;
            live.remove(id)
        };
        let Some(slot) = slot else {
            return false;
        };
        let state = slot.state.borrow().clone();
        match state {
            SlotState::Ready(connection) => connection.close().await,
            SlotState::Starting => {
                // Wake single-flight waiters immediately. The owner sees that
                // its reservation is gone and closes the connection it built.
                slot.state
                    .send_replace(SlotState::Failed(Error::cancelled()));
            }
            SlotState::Failed(_) => {}
        }
        true
    }

    pub(crate) async fn tools(
        &self,
        id: &str,
        cap: Option<u64>,
    ) -> Result<Option<Vec<serde_json::Value>>, Error> {
        let connection = {
            let live = self.live.lock().await;
            live.get(id).and_then(|slot| ready_connection(slot))
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
            live.get(id).and_then(|slot| ready_connection(slot))
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
        let slot = live.get(id)?;
        let state = slot.state.borrow().clone();
        match state {
            SlotState::Ready(connection) => Some(connection.is_alive()),
            SlotState::Failed(_) => Some(false),
            SlotState::Starting => None,
        }
    }

    pub(crate) async fn capabilities(&self, id: &str) -> Option<RemoteCapabilities> {
        let connection = {
            let live = self.live.lock().await;
            live.get(id).and_then(|slot| ready_connection(slot))
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
            OAUTH_UNCONFIGURED_CODE => (OAUTH_UNCONFIGURED_CODE, StatusCode::CONFLICT),
            OAUTH_DENIED_CODE => (OAUTH_DENIED_CODE, StatusCode::FORBIDDEN),
            OAUTH_EXPIRED_CODE => (OAUTH_EXPIRED_CODE, StatusCode::CONFLICT),
            OAUTH_DISCOVERY_CODE => (OAUTH_DISCOVERY_CODE, StatusCode::BAD_GATEWAY),
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
        let mut view = mcp_view(&record, health, tools, capabilities);
        if record.transport == "streamable_http" {
            let status = super::mcp_oauth::auth_status(foundation, &record);
            view["auth"] = serde_json::json!({
                "state": status["state"],
                "account": status["account"],
                "scopes": status["scopes"],
                "hasToken": status["hasToken"],
                "method": status["method"],
                "issuer": status.get("issuer"),
                "expiresAtMs": status.get("expiresAtMs"),
                "hasRefresh": status.get("hasRefresh"),
                "renewable": status.get("renewable"),
            });
        }
        out.push(view);
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
        state.deps.mcp_runtime.start(record, env, Some(foundation)),
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

fn default_mcp_auth_action() -> String {
    "login".into()
}

// SECRET-FIELD-GATE-WIRE-DTO: MCP OAuth wire body; access tokens, client
// secrets and authorization codes are wrapped/moved into the vault in the
// same handler and never persisted or echoed in plaintext.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct NativeMcpAuthBody {
    /// The panel's selected server id. The PATH id is authoritative; this is
    /// accepted only so the generic UI command body validates.
    #[serde(default)]
    id: Option<String>,
    /// `connect` | `reconnect` | `revoke` | `login` | `complete` | `logout`
    #[serde(default = "default_mcp_auth_action")]
    action: String,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    account: Option<String>,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    expected_revision: Option<u64>,
    /// `authorization_code` (default) or `client_credentials`.
    #[serde(default)]
    mode: Option<String>,
    /// A pre-registered OAuth client id; omitted triggers RFC 7591 dynamic
    /// registration when the authorization server supports it.
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    client_secret: Option<String>,
    /// The operator's loopback callback (http://127.0.0.1:<port>/...).
    #[serde(default)]
    redirect_uri: Option<String>,
    #[serde(default)]
    flow_id: Option<String>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
}

/// `GET|POST /native/mcp/servers/{id}/oauth/status` — the non-secret OAuth
/// lifecycle view: durable state, account, granted scopes, issuer and
/// expiry. Never a token, refresh token or client secret.
pub(crate) async fn native_mcp_server_oauth_status(
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
        Ok(Some(record)) => {
            Json(super::mcp_oauth::auth_status(foundation, &record)).into_response()
        }
        Ok(None) => wire_status(not_found("MCP server")),
        Err(e) => foundation_error(e),
    }
}

/// `POST /native/mcp/servers/{id}/oauth/login` (alias `/oauth/connect`) —
/// start the operator flow. In `authorization_code` mode the response is the
/// authorization URL plus a single-use flow id; the CLI/IDE owns the
/// loopback listener and posts the callback to `/oauth/complete`. In
/// `client_credentials` mode the exchange happens immediately. No token is
/// ever returned.
pub(crate) async fn native_mcp_server_oauth_login(
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
        Err(_) => return wire_status(malformed_body("invalid MCP OAuth login body")),
    };
    if let Some(body_id) = body.id.as_deref() {
        if body_id != id {
            return wire_status(malformed_body(
                "the OAuth login body id does not match the path server id",
            ));
        }
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
    if let Err(error) = require_remote_oauth_record(&record) {
        return mcp_failure(error);
    }
    let mode = body.mode.as_deref().unwrap_or("authorization_code");
    match super::mcp_oauth::begin_login(
        &state.deps.mcp_runtime,
        foundation,
        &record,
        mode,
        body.client_id.as_deref(),
        body.client_secret.as_deref(),
        body.redirect_uri.as_deref(),
        headers.get("host").and_then(|value| value.to_str().ok()),
        body.scopes,
        body.account,
    )
    .await
    {
        Ok(value) => Json(value).into_response(),
        Err(error) => {
            super::mcp_oauth::project_login_error(foundation, &record, &error);
            mcp_failure(error)
        }
    }
}

/// `POST /native/mcp/servers/{id}/oauth/complete` — exchange the loopback
/// callback code exactly once and persist the vault bundle. A replayed flow
/// id, wrong state or PKCE mismatch is the typed `oauth_denied`; an expired
/// flow is `oauth_expired`; missing metadata is `oauth_unconfigured`.
pub(crate) async fn native_mcp_server_oauth_complete(
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
        Err(_) => return wire_status(malformed_body("invalid MCP OAuth callback body")),
    };
    if let Some(body_id) = body.id.as_deref() {
        if body_id != id {
            return wire_status(malformed_body(
                "the OAuth callback body id does not match the path server id",
            ));
        }
    }
    let (Some(flow_id), Some(code), Some(state_value)) = (
        body.flow_id.as_deref(),
        body.code.as_deref(),
        body.state.as_deref(),
    ) else {
        return wire_status(malformed_body(
            "complete requires flowId, code and state from the loopback callback",
        ));
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let record = match foundation.customization.mcp_server(&id) {
        Ok(Some(record)) => record,
        Ok(None) => return wire_status(not_found("MCP server")),
        Err(e) => return foundation_error(e),
    };
    if let Err(error) = require_remote_oauth_record(&record) {
        return mcp_failure(error);
    }
    match super::mcp_oauth::complete_login(
        &state.deps.mcp_runtime,
        foundation,
        &record,
        flow_id,
        code,
        state_value,
    )
    .await
    {
        Ok(value) => Json(value).into_response(),
        Err(error) => {
            super::mcp_oauth::project_login_error(foundation, &record, &error);
            mcp_failure(error)
        }
    }
}

/// `GET /native/mcp/servers/{id}/oauth/callback/{flowId}` — the
/// daemon-owned loopback redirect for a UI-started login. The browser
/// carries only the one-shot flow id plus the AS `code`/`state`; state is
/// verified by the pending flow, and the page never renders a credential.
pub(crate) async fn native_mcp_server_oauth_callback(
    State(state): State<AppState>,
    Path((id, flow_id)): Path<(String, String)>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let (Some(code), Some(state_value)) = (params.get("code"), params.get("state")) else {
        let detail = params
            .get("error")
            .map(|error| format!("authorization server returned {error}"))
            .unwrap_or_else(|| "the callback carried no code/state".into());
        return (
            StatusCode::BAD_REQUEST,
            Html(callback_page("Login failed", &detail, false)),
        )
            .into_response();
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(response) => return response,
    };
    let record = match foundation.customization.mcp_server(&id) {
        Ok(Some(record)) => record,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Html(callback_page("Login failed", "MCP server not found", false)),
            )
                .into_response()
        }
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html(callback_page(
                    "Login failed",
                    &format!("MCP server read failed: {error}"),
                    false,
                )),
            )
                .into_response()
        }
    };
    match super::mcp_oauth::complete_login(
        &state.deps.mcp_runtime,
        foundation,
        &record,
        &flow_id,
        code,
        state_value,
    )
    .await
    {
        Ok(_) => (
            StatusCode::OK,
            Html(callback_page(
                "Login complete",
                "The MCP server credential is stored. You can close this window and return to Faktor.",
                true,
            )),
        )
            .into_response(),
        Err(error) => {
            super::mcp_oauth::project_login_error(foundation, &record, &error);
            let detail = format!(
                "{}: {}",
                match &error.kind {
                    ErrorKind::Provider { code, .. } => code.as_str(),
                    _ => "oauth_login_failed",
                },
                error.message
            );
            (StatusCode::BAD_REQUEST, Html(callback_page("Login failed", &detail, false)))
                .into_response()
        }
    }
}

fn callback_page(title: &str, detail: &str, success: bool) -> String {
    let color = if success { "#1a7f37" } else { "#b42318" };
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{title}</title></head>\
         <body style=\"font-family: system-ui, sans-serif; margin: 3rem; max-width: 42rem\">\
         <h1 style=\"color: {color}\">{title}</h1><p>{detail}</p></body></html>",
        title = escape_html(title),
        detail = escape_html(detail),
        color = color
    )
}

fn escape_html(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars().take(1024) {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            ch if ch.is_control() => {}
            ch => out.push(ch),
        }
    }
    out
}

/// `POST /native/mcp/servers/{id}/oauth/logout` — remote revocation
/// (best-effort, reported honestly), local vault deletion, live-session
/// close and durable `signed_out`.
pub(crate) async fn native_mcp_server_oauth_logout(
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
    if let Err(error) = require_remote_oauth_record(&record) {
        return mcp_failure(error);
    }
    match super::mcp_oauth::logout(&state.deps.mcp_runtime, foundation, &record).await {
        Ok(value) => Json(value).into_response(),
        Err(error) => mcp_failure(error),
    }
}

fn require_remote_oauth_record(record: &McpServerRecord) -> Result<(), Error> {
    if record.transport != "streamable_http" {
        return Err(mcp_provider_error(
            "mcp_oauth_unsupported",
            "OAuth binding applies to streamable_http servers only",
        ));
    }
    Ok(())
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
        Ok(Some(record)) => {
            Json(super::mcp_oauth::auth_status(foundation, &record)).into_response()
        }
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
    if let Some(body_id) = body.id.as_deref() {
        if body_id != id {
            return wire_status(malformed_body(
                "the auth body id does not match the path server id",
            ));
        }
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
        if let Some(expected) = body.expected_revision {
            if record.revision != expected {
                return conflict(
                    "mcp_revision",
                    "the MCP server changed before revocation; refresh and retry",
                );
            }
        }
        return match super::mcp_oauth::logout(&state.deps.mcp_runtime, foundation, &record).await {
            Ok(value) => Json(value).into_response(),
            Err(error) => mcp_failure(error),
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
    match super::mcp_oauth::logout(&state.deps.mcp_runtime, foundation, &record).await {
        Ok(value) => Json(value).into_response(),
        Err(error) => mcp_failure(error),
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
