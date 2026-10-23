//! Remote MCP over Streamable HTTP.
//!
//! The client speaks JSON-RPC 2.0 over HTTP POST. A response may be a single
//! `application/json` document or a `text/event-stream` (SSE) stream; both
//! are bounded by the call deadline, the configured output cap and a frame
//! cap. Session handling follows the MCP Streamable HTTP contract:
//!
//! * `initialize` negotiates the protocol version and capabilities and may
//!   return an `Mcp-Session-Id` header;
//! * every later request carries that session id (when the server issued
//!   one) and the negotiated `MCP-Protocol-Version`;
//! * a `404` for a request that carried a session id is the server's session
//!   expiry signal — the client re-initializes once and retries the request;
//! * `DELETE` terminates the session best-effort.
//!
//! # Checked egress, never a raw client
//!
//! Every send goes through [`faktor_provider::egress::CheckedHttpClient`]:
//! the endpoint is the ONLY allowlisted destination, environment proxies are
//! disabled by the checked client builder, the central resolver refuses
//! special-purpose answers, and the outbound full-body secret scan runs
//! before any connect. Bearer tokens live in [`SecretToken`] (redacted
//! `Debug`, zeroized on drop) and are only ever written into the
//! `Authorization` header; they never enter a URL, error message or log.
//!
//! # Capability negotiation is real
//!
//! The server's `initialize` result is decoded into [`RemoteCapabilities`].
//! A discovery/invoke method whose capability was not advertised returns the
//! typed [`CAPABILITY_UNSUPPORTED_CODE`] error without sending the request —
//! there is no fabricated empty catalog or fake success.
//!
//! # Cancellation and bounds
//!
//! Every call has one wall deadline covering connect, request write, response
//! head and body/SSE reads. A timed-out request sends
//! `notifications/cancelled` best-effort (bounded separately) and returns a
//! typed timeout. Response bytes, SSE events, pagination pages and item
//! counts are all bounded.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use faktor_core::error::{Error, ErrorKind};
use faktor_provider::egress::{
    CheckedHttpClient, CheckedResponse, EgressAddressPolicy, EgressClientOptions, EgressError,
    OutboundScanConfig, ResponseBudget,
};
use faktor_security::destination::DestinationPolicy;
use faktor_security::payload::ScanPolicy;
use futures::future::BoxFuture;
use futures::StreamExt;
use reqwest::header::{HeaderName, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use reqwest::Url;
use serde_json::{json, Value};
use zeroize::{Zeroize, Zeroizing};

/// Negotiated protocol version sent by this client.
pub const CLIENT_PROTOCOL_VERSION: &str = "2025-06-18";
/// Versions this client accepts from a server. A server that answers with
/// anything else is refused typed instead of pretending compatibility.
pub const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

pub const CAPABILITY_UNSUPPORTED_CODE: &str = "mcp_capability_unsupported";
pub const AUTH_REQUIRED_CODE: &str = "mcp_auth_required";
pub const AUTH_FORBIDDEN_CODE: &str = "mcp_auth_forbidden";
pub const SESSION_EXPIRED_CODE: &str = "mcp_session_expired";
pub const PROTOCOL_CODE: &str = "mcp_remote_protocol";
pub const TRANSPORT_CODE: &str = "mcp_remote_transport";
pub const JSONRPC_ERROR_CODE: &str = "mcp_jsonrpc_error";

const SESSION_HEADER: &str = "mcp-session-id";
const PROTOCOL_HEADER: &str = "mcp-protocol-version";
const MAX_PAGES: usize = 64;
const MAX_ITEMS: usize = 8192;
const MAX_ERROR_BODY: u64 = 64 * 1024;
/// Bound on one serialized outbound JSON-RPC request. Tool arguments are
/// caller-supplied; the checked transport's scan sees the same full body.
const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;
/// Hard bound on one remote response body, independent of caller config.
const MAX_OUTPUT_CAP_BYTES: u64 = 64 * 1024 * 1024;
/// Hard bound on the negotiated per-request timeout.
const MAX_REMOTE_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_SSE_EVENTS: usize = 8192;
const MAX_MESSAGE_CHARS: usize = 512;
const CANCEL_TIMEOUT: Duration = Duration::from_millis(750);

/// A bearer token that can never be serialized or debug-printed. The bytes
/// are zeroized when the last clone is dropped.
#[derive(Clone)]
pub struct SecretToken {
    value: std::sync::Arc<Zeroizing<String>>,
}

impl SecretToken {
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            value: std::sync::Arc::new(Zeroizing::new(value.into())),
        }
    }

    pub fn from_bytes(mut bytes: Vec<u8>) -> Result<Self, Error> {
        let valid = std::str::from_utf8(&bytes).is_ok_and(|text| {
            !text.is_empty() && text.len() <= 8_192 && !text.chars().any(char::is_control)
        });
        if !valid {
            bytes.zeroize();
            return Err(Error::new(
                ErrorKind::Malformed,
                "MCP token must be 1..=8192 UTF-8 non-control bytes",
            ));
        }
        let text = String::from_utf8(bytes).map_err(|error| {
            let mut bytes = error.into_bytes();
            bytes.zeroize();
            Error::new(ErrorKind::Malformed, "MCP token is not UTF-8")
        })?;
        Ok(Self::new(text))
    }

    /// Crate-internal access for the OAuth form encoder and state checks.
    /// Never public: no caller outside this crate can read a bearer token.
    pub(crate) fn expose(&self) -> &str {
        self.value.as_str()
    }

    /// Replace this exact credential in a server-supplied message. The
    /// plaintext is only used for the replacement; the result is bounded by
    /// the caller and the secret itself is never returned or logged.
    pub fn scrub(&self, text: &str) -> String {
        if self.value.is_empty() {
            return text.to_string();
        }
        text.replace(self.value.as_str(), "[redacted]")
    }

    fn authorization_header(&self) -> Result<HeaderValue, Error> {
        let value = Zeroizing::new(format!("Bearer {}", self.value.as_str()));
        HeaderValue::from_str(value.as_str()).map_err(|_| {
            Error::new(
                ErrorKind::Malformed,
                "MCP token is not a valid header value",
            )
        })
    }

    #[cfg(test)]
    pub(crate) fn expose_for_test(&self) -> &str {
        self.value.as_str()
    }
}

impl fmt::Debug for SecretToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretToken(<redacted>)")
    }
}

/// The one refresh seam a remote MCP session uses when its credential
/// expires. The OAuth implementation lives outside this transport (the
/// daemon wires the vault-backed provider); static tokens simply have no
/// provider. `refresh` is called ONLY after a 401, so a healthy session never
/// pays for or races a refresh.
pub trait RemoteAuth: Send + Sync {
    /// The current access token, refreshed proactively when its recorded
    /// expiry has passed. A failure is a typed OAuth error.
    fn token<'a>(&'a self) -> BoxFuture<'a, Result<SecretToken, Error>>;

    /// Force one refresh after the server rejected the current token.
    fn refresh<'a>(&'a self) -> BoxFuture<'a, Result<SecretToken, Error>>;

    /// Optional reflection scrubber for a server-supplied error message.
    /// Implementations hold the current token and replace it; the default
    /// (static-token sessions) is handled by the transport itself.
    fn scrub(&self, text: &str) -> String {
        text.to_string()
    }
}

/// The server capabilities observed during `initialize`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteCapabilities {
    pub tools: bool,
    pub resources: bool,
    pub prompts: bool,
    pub logging: bool,
    pub completions: bool,
    pub tools_list_changed: bool,
    pub resources_subscribe: bool,
    pub resources_list_changed: bool,
    pub prompts_list_changed: bool,
    pub experimental: bool,
}

impl RemoteCapabilities {
    pub fn from_value(value: &Value) -> Self {
        let caps = value.get("capabilities").unwrap_or(value);
        let feature = |name: &str| caps.get(name).is_some_and(|value| !value.is_null());
        let flag = |feature_name: &str, flag: &str| {
            caps.get(feature_name)
                .and_then(|value| value.get(flag))
                .and_then(|value| value.as_bool())
                .unwrap_or(false)
        };
        Self {
            tools: feature("tools"),
            resources: feature("resources"),
            prompts: feature("prompts"),
            logging: feature("logging"),
            completions: feature("completions"),
            tools_list_changed: flag("tools", "listChanged"),
            resources_subscribe: flag("resources", "subscribe"),
            resources_list_changed: flag("resources", "listChanged"),
            prompts_list_changed: flag("prompts", "listChanged"),
            experimental: feature("experimental"),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "tools": { "supported": self.tools, "listChanged": self.tools_list_changed },
            "resources": {
                "supported": self.resources,
                "subscribe": self.resources_subscribe,
                "listChanged": self.resources_list_changed,
            },
            "prompts": { "supported": self.prompts, "listChanged": self.prompts_list_changed },
            "logging": self.logging,
            "completions": self.completions,
            "experimental": self.experimental,
        })
    }

    fn require(&self, capability: &str) -> Result<(), Error> {
        let supported = match capability {
            "tools" => self.tools,
            "resources" => self.resources,
            "prompts" => self.prompts,
            _ => false,
        };
        if supported {
            return Ok(());
        }
        Err(Error::new(
            ErrorKind::Provider {
                code: CAPABILITY_UNSUPPORTED_CODE.into(),
                retryable: false,
            },
            format!("MCP server did not advertise the {capability} capability during initialize"),
        ))
    }
}

/// Connection configuration for one remote MCP server.
pub struct RemoteMcpConfig {
    pub name: String,
    /// Absolute http(s) endpoint. Query/fragment/userinfo are refused.
    pub endpoint: String,
    pub token: Option<SecretToken>,
    /// Vault-backed OAuth credential with expiry-aware refresh. Mutually
    /// exclusive in practice with `token`; OAuth wins when both are present.
    pub auth: Option<std::sync::Arc<dyn RemoteAuth>>,
    pub timeout: Duration,
    pub output_cap_bytes: usize,
    pub protocol_version: String,
}

impl RemoteMcpConfig {
    pub fn new(name: impl Into<String>, endpoint: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            endpoint: endpoint.into(),
            token: None,
            auth: None,
            timeout: Duration::from_secs(30),
            output_cap_bytes: 16 * 1024 * 1024,
            protocol_version: CLIENT_PROTOCOL_VERSION.into(),
        }
    }
}

impl fmt::Debug for RemoteMcpConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteMcpConfig")
            .field("name", &self.name)
            .field("endpoint", &self.endpoint)
            .field("token", &self.token)
            .field("oauth", &self.auth.as_ref().map(|_| "<configured>"))
            .field("timeout", &self.timeout)
            .field("output_cap_bytes", &self.output_cap_bytes)
            .field("protocol_version", &self.protocol_version)
            .finish()
    }
}

#[derive(Debug, Clone)]
struct Session {
    id: Option<String>,
    protocol_version: String,
    capabilities: RemoteCapabilities,
    server_info: Value,
    instructions: Option<String>,
}

struct Reply {
    value: Value,
    /// The server may rotate the session id on any response.
    session_id: Option<String>,
}

enum SendOutcome {
    Reply(Reply),
    Accepted,
    SessionExpired,
}

/// One initialized Streamable HTTP MCP connection.
pub struct RemoteMcpServer {
    name: String,
    endpoint: Url,
    token: Option<SecretToken>,
    auth: Option<std::sync::Arc<dyn RemoteAuth>>,
    default_timeout: Duration,
    output_cap: usize,
    protocol_version: String,
    client: CheckedHttpClient,
    session: tokio::sync::RwLock<Option<Session>>,
    initialize_lock: tokio::sync::Mutex<()>,
    next_id: AtomicU64,
    closed: AtomicBool,
}

impl fmt::Debug for RemoteMcpServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteMcpServer")
            .field("name", &self.name)
            .field("endpoint", &self.endpoint)
            .field("token", &self.token)
            .field("oauth", &self.auth.as_ref().map(|_| "<configured>"))
            .finish_non_exhaustive()
    }
}

impl RemoteMcpServer {
    /// Production connection through the checked egress client: the endpoint
    /// is the only allowlisted destination, environment proxies are disabled,
    /// and the full request body is secret-scanned before any connect.
    pub async fn connect(cfg: RemoteMcpConfig) -> Result<std::sync::Arc<Self>, Error> {
        let endpoint = parse_endpoint(&cfg.endpoint)?;
        // Production remote sessions carry bearer credentials and tool
        // payloads; plaintext HTTP is refused. The local-fixture/test
        // constructor is the only http path.
        if endpoint.scheme() != "https" {
            return Err(Error::new(
                ErrorKind::Malformed,
                "remote MCP endpoint must use https",
            ));
        }
        let policy = endpoint_policy(&endpoint)?;
        let scan = OutboundScanConfig {
            policy: ScanPolicy {
                max_payload_bytes: Some(MAX_REQUEST_BYTES as u64),
                ..ScanPolicy::default()
            },
            block_on_secret: true,
            registry: None,
        };
        let options = EgressClientOptions {
            connect_timeout: Some(
                cfg.timeout
                    .min(Duration::from_secs(10))
                    .max(Duration::from_millis(100)),
            ),
            ..Default::default()
        };
        let client = CheckedHttpClient::try_with_policy_scan_addresses_and_options(
            policy,
            Some(scan),
            EgressAddressPolicy::EXTERNAL,
            options,
        )
        .map_err(map_egress)?;
        Self::from_client(cfg, client).await
    }

    /// Test/utility constructor with an explicit checked client (the tests
    /// install a loopback allowlist). Production never accepts an injected
    /// client: [`RemoteMcpServer::connect`] builds the checked one itself.
    pub async fn connect_with_client(
        cfg: RemoteMcpConfig,
        client: CheckedHttpClient,
    ) -> Result<std::sync::Arc<Self>, Error> {
        Self::from_client(cfg, client).await
    }

    async fn from_client(
        cfg: RemoteMcpConfig,
        client: CheckedHttpClient,
    ) -> Result<std::sync::Arc<Self>, Error> {
        let endpoint = parse_endpoint(&cfg.endpoint)?;
        if cfg.output_cap_bytes < 1024 || cfg.output_cap_bytes as u64 > MAX_OUTPUT_CAP_BYTES {
            return Err(Error::new(
                ErrorKind::Oversized,
                format!("remote MCP output cap must be within 1024..={MAX_OUTPUT_CAP_BYTES} bytes"),
            ));
        }
        if cfg.timeout.is_zero() || cfg.timeout > MAX_REMOTE_TIMEOUT {
            return Err(Error::new(
                ErrorKind::Timeout,
                format!(
                    "remote MCP timeout must be within 1..={} seconds",
                    MAX_REMOTE_TIMEOUT.as_secs()
                ),
            ));
        }
        let server = std::sync::Arc::new(Self {
            name: cfg.name,
            endpoint,
            token: cfg.token,
            auth: cfg.auth,
            default_timeout: cfg.timeout,
            output_cap: cfg.output_cap_bytes,
            protocol_version: cfg.protocol_version,
            client,
            session: tokio::sync::RwLock::new(None),
            initialize_lock: tokio::sync::Mutex::new(()),
            next_id: AtomicU64::new(1),
            closed: AtomicBool::new(false),
        });
        server.initialize().await?;
        Ok(server)
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn is_alive(&self) -> bool {
        !self.closed.load(Ordering::SeqCst) && self.session.try_read().is_ok_and(|s| s.is_some())
    }

    pub async fn session_id(&self) -> Option<String> {
        self.session
            .read()
            .await
            .as_ref()
            .and_then(|session| session.id.clone())
    }

    pub async fn capabilities(&self) -> RemoteCapabilities {
        self.session
            .read()
            .await
            .as_ref()
            .map(|session| session.capabilities.clone())
            .unwrap_or_default()
    }

    pub async fn server_info(&self) -> Value {
        self.session
            .read()
            .await
            .as_ref()
            .map(|session| session.server_info.clone())
            .unwrap_or(Value::Null)
    }

    pub async fn instructions(&self) -> Option<String> {
        self.session
            .read()
            .await
            .as_ref()
            .and_then(|session| session.instructions.clone())
    }

    async fn initialize(self: &std::sync::Arc<Self>) -> Result<(), Error> {
        let _guard = self.initialize_lock.lock().await;
        if self.session.read().await.is_some() {
            return Ok(());
        }
        let deadline = tokio::time::Instant::now() + self.default_timeout;
        self.initialize_with_deadline(deadline).await
    }

    async fn reinitialize(&self, deadline: tokio::time::Instant) -> Result<(), Error> {
        // Serialize re-initialization and re-check under the lock: concurrent
        // expired-session retries must not race two initialize handshakes
        // (duplicate sessions, mismatched ids). The first winner installs the
        // session; later callers reuse it.
        let _guard = self.initialize_lock.lock().await;
        if self.session.read().await.is_some() {
            return Ok(());
        }
        self.initialize_with_deadline(deadline).await
    }

    async fn initialize_with_deadline(&self, deadline: tokio::time::Instant) -> Result<(), Error> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "protocolVersion": self.protocol_version,
                "capabilities": {},
                "clientInfo": { "name": "faktor-plus", "version": "0.1.0" },
            },
        });
        let outcome = self.send_request(request, id, None, deadline, true).await?;
        let SendOutcome::Reply(reply) = outcome else {
            return Err(self.protocol_error("initialize returned no JSON-RPC response"));
        };
        if reply.value.get("error").is_some() {
            return Err(self.jsonrpc_error("initialize", &reply.value));
        }
        let result = reply.value.get("result").cloned().unwrap_or(Value::Null);
        let negotiated = result
            .get("protocolVersion")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                self.protocol_error("MCP initialize result did not negotiate a protocolVersion")
            })?
            .to_string();
        if !SUPPORTED_PROTOCOL_VERSIONS.contains(&negotiated.as_str()) {
            return Err(Error::new(
                ErrorKind::Provider {
                    code: PROTOCOL_CODE.into(),
                    retryable: false,
                },
                format!("MCP server negotiated unsupported protocol version {negotiated:?}"),
            ));
        }
        *self.session.write().await = Some(Session {
            id: reply.session_id,
            protocol_version: negotiated,
            capabilities: RemoteCapabilities::from_value(&result),
            server_info: result.get("serverInfo").cloned().unwrap_or(Value::Null),
            instructions: result
                .get("instructions")
                .and_then(|value| value.as_str())
                .map(str::to_string),
        });
        self.notify("notifications/initialized", json!({}), deadline)
            .await
    }

    pub async fn list_tools(&self) -> Result<Vec<super::McpTool>, Error> {
        self.capabilities().await.require("tools")?;
        let values = self
            .paginate("tools/list", "tools", self.default_timeout)
            .await?;
        Ok(values
            .into_iter()
            .map(|value| super::McpTool {
                name: value
                    .get("name")
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
                    .to_string(),
                description: value
                    .get("description")
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
                    .to_string(),
                input_schema: value.get("inputSchema").cloned().unwrap_or(Value::Null),
            })
            .collect())
    }

    pub async fn list_resources(&self) -> Result<Vec<Value>, Error> {
        self.capabilities().await.require("resources")?;
        self.paginate("resources/list", "resources", self.default_timeout)
            .await
    }

    pub async fn list_resource_templates(&self) -> Result<Vec<Value>, Error> {
        self.capabilities().await.require("resources")?;
        self.paginate(
            "resources/templates/list",
            "resourceTemplates",
            self.default_timeout,
        )
        .await
    }

    pub async fn list_prompts(&self) -> Result<Vec<Value>, Error> {
        self.capabilities().await.require("prompts")?;
        self.paginate("prompts/list", "prompts", self.default_timeout)
            .await
    }

    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        deadline: Duration,
    ) -> Result<super::McpResult, Error> {
        self.capabilities().await.require("tools")?;
        let result = self
            .rpc(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
                deadline,
            )
            .await?;
        Ok(super::McpResult {
            content: result
                .get("content")
                .and_then(|value| value.as_array())
                .cloned()
                .unwrap_or_default(),
            is_error: result
                .get("isError")
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
        })
    }

    pub async fn read_resource(&self, uri: &str) -> Result<Value, Error> {
        self.capabilities().await.require("resources")?;
        self.rpc(
            "resources/read",
            json!({ "uri": uri }),
            self.default_timeout,
        )
        .await
    }

    pub async fn get_prompt(&self, name: &str, arguments: Option<Value>) -> Result<Value, Error> {
        self.capabilities().await.require("prompts")?;
        let params = match arguments {
            Some(arguments) => json!({ "name": name, "arguments": arguments }),
            None => json!({ "name": name }),
        };
        self.rpc("prompts/get", params, self.default_timeout).await
    }

    async fn paginate(
        &self,
        method: &str,
        key: &str,
        timeout: Duration,
    ) -> Result<Vec<Value>, Error> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let result = self.rpc(method, params, timeout).await?;
            if let Some(items) = result.get(key).and_then(|value| value.as_array()) {
                for item in items.iter().take(MAX_ITEMS.saturating_sub(out.len())) {
                    out.push(item.clone());
                }
            }
            if out.len() >= MAX_ITEMS {
                break;
            }
            cursor = result
                .get("nextCursor")
                .and_then(|value| value.as_str())
                .filter(|cursor| !cursor.is_empty())
                .map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        Ok(out)
    }

    async fn rpc(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, Error> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut retried = false;
        loop {
            if self.session.read().await.is_none() {
                self.reinitialize(deadline).await?;
            }
            let session_id = self.session_id().await;
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            let request = json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            });
            match self
                .send_request(request, id, session_id.as_deref(), deadline, true)
                .await?
            {
                SendOutcome::Reply(reply) => {
                    if let Some(session_id) = reply.session_id {
                        if let Some(session) = self.session.write().await.as_mut() {
                            session.id = Some(session_id);
                        }
                    }
                    if reply.value.get("error").is_some() {
                        return Err(self.jsonrpc_error(method, &reply.value));
                    }
                    return Ok(reply.value.get("result").cloned().unwrap_or(Value::Null));
                }
                SendOutcome::SessionExpired if !retried && session_id.is_some() => {
                    // The server refused the session id: start a fresh
                    // session and retry this request exactly once. A 404 is
                    // the server's explicit "not executed" signal.
                    retried = true;
                    {
                        // Only invalidate the session this request actually
                        // used: a concurrent retry may already have installed
                        // a fresh session, and clobbering it would stampede.
                        let mut session = self.session.write().await;
                        if session.as_ref().and_then(|session| session.id.as_deref())
                            == session_id.as_deref()
                        {
                            *session = None;
                        }
                    }
                    if tokio::time::Instant::now() >= deadline {
                        return Err(Error::timeout(format!(
                            "MCP {method} session expired and the deadline elapsed"
                        )));
                    }
                }
                SendOutcome::SessionExpired => {
                    return Err(Error::new(
                        ErrorKind::Provider {
                            code: SESSION_EXPIRED_CODE.into(),
                            retryable: true,
                        },
                        format!("MCP remote session expired during {method}"),
                    ))
                }
                SendOutcome::Accepted => {
                    return Err(self.protocol_error(format!(
                        "MCP {method} was accepted without a JSON-RPC response"
                    )))
                }
            }
        }
    }

    async fn notify(
        &self,
        method: &str,
        params: Value,
        deadline: tokio::time::Instant,
    ) -> Result<(), Error> {
        let session_id = self.session_id().await;
        let request = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        match self
            .send_request(request, 0, session_id.as_deref(), deadline, false)
            .await?
        {
            SendOutcome::Accepted | SendOutcome::Reply(_) => Ok(()),
            SendOutcome::SessionExpired => Err(Error::new(
                ErrorKind::Provider {
                    code: SESSION_EXPIRED_CODE.into(),
                    retryable: false,
                },
                format!("MCP remote session expired during {method}"),
            )),
        }
    }

    async fn cancel_best_effort(&self, request_id: u64) {
        let Some(session_id) = self.session_id().await else {
            return;
        };
        let request = json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": { "requestId": request_id, "reason": "client deadline exceeded" },
        });
        let deadline = tokio::time::Instant::now() + CANCEL_TIMEOUT;
        let _ = self
            .post_notification(&request, Some(&session_id), deadline)
            .await;
    }

    /// Bounded notification POST used by cancellation. It deliberately does
    /// NOT call [`Self::send_request`] (whose timeout path sends a
    /// cancellation), so cancellation can never recurse.
    async fn post_notification(
        &self,
        request: &Value,
        session_id: Option<&str>,
        deadline: tokio::time::Instant,
    ) -> Result<(), Error> {
        let mut builder = self
            .client
            .post(self.endpoint.as_str())
            .map_err(map_egress)?;
        builder = builder
            .header(ACCEPT, "application/json, text/event-stream")
            .header(CONTENT_TYPE, "application/json");
        if let Some(token) = self.bearer().await? {
            builder = builder.header(AUTHORIZATION, token.authorization_header()?);
        }
        if let Some(session_id) = session_id {
            builder = builder.header(HeaderName::from_static(SESSION_HEADER), session_id);
        }
        builder = builder.header(
            HeaderName::from_static(PROTOCOL_HEADER),
            self.protocol_version.clone(),
        );
        builder = builder.body(bounded_request_body(request)?);
        match tokio::time::timeout_at(deadline, self.client.send_checked(builder)).await {
            Ok(Ok(response)) => {
                let _ = drain(response, self.budget(deadline), self.output_cap).await;
                Ok(())
            }
            Ok(Err(error)) => Err(map_egress(error)),
            Err(_) => Err(Error::timeout("MCP cancellation notification timed out")),
        }
    }

    /// The current bearer: OAuth (which may refresh on its recorded expiry)
    /// or the static token. A failure is the typed OAuth transport error.
    async fn bearer(&self) -> Result<Option<SecretToken>, Error> {
        if let Some(auth) = &self.auth {
            return auth.token().await.map(Some);
        }
        Ok(self.token.clone())
    }

    async fn send_request(
        &self,
        request: Value,
        id: u64,
        session_id: Option<&str>,
        deadline: tokio::time::Instant,
        expect_response: bool,
    ) -> Result<SendOutcome, Error> {
        let version = self
            .session
            .try_read()
            .ok()
            .and_then(|session| {
                session
                    .as_ref()
                    .map(|session| session.protocol_version.clone())
            })
            .unwrap_or_else(|| self.protocol_version.clone());
        let body = bounded_request_body(&request)?;
        let mut refreshed = false;
        let (response, status, token) = loop {
            let token = self.bearer().await?;
            let mut builder = self
                .client
                .post(self.endpoint.as_str())
                .map_err(map_egress)?;
            builder = builder
                .header(ACCEPT, "application/json, text/event-stream")
                .header(CONTENT_TYPE, "application/json");
            if let Some(token) = &token {
                builder = builder.header(AUTHORIZATION, token.authorization_header()?);
            }
            if let Some(session_id) = session_id {
                builder = builder.header(HeaderName::from_static(SESSION_HEADER), session_id);
            }
            builder = builder.header(HeaderName::from_static(PROTOCOL_HEADER), version.clone());
            builder = builder.body(body.clone());
            let response =
                match tokio::time::timeout_at(deadline, self.client.send_checked(builder)).await {
                    Ok(Ok(response)) => response,
                    Ok(Err(error)) => return Err(map_egress(error)),
                    Err(_) => {
                        if expect_response {
                            self.cancel_best_effort(id).await;
                        }
                        return Err(Error::timeout(format!(
                            "MCP remote request {id} exceeded its deadline"
                        )));
                    }
                };
            let status = response.status().as_u16();
            if status == 401 && !refreshed {
                if let Some(auth) = &self.auth {
                    // The server rejected the credential: force exactly ONE
                    // refresh and replay the same bounded request. The replay
                    // is safe here because the first attempt produced only a
                    // 401 (no application-level effect was accepted).
                    auth.refresh().await?;
                    refreshed = true;
                    continue;
                }
            }
            break (response, status, token);
        };
        let session_header = match header_text(response.headers(), SESSION_HEADER) {
            Some(session) if !valid_session_id(&session) => {
                return Err(self.protocol_error("MCP server sent an invalid Mcp-Session-Id header"))
            }
            session => session,
        };
        if matches!(status, 401 | 403) {
            let code = if status == 401 && self.auth.is_some() {
                // The refresh was attempted and the retried request was
                // still refused: the credential is expired/revoked.
                crate::oauth::OAUTH_EXPIRED_CODE
            } else if status == 401 {
                AUTH_REQUIRED_CODE
            } else {
                AUTH_FORBIDDEN_CODE
            };
            return Err(Error::new(
                ErrorKind::Permission,
                if status == 401 && self.auth.is_some() {
                    "MCP remote server rejected the refreshed OAuth credential (HTTP 401)"
                        .to_string()
                } else if status == 401 {
                    "MCP remote server requires authorization (HTTP 401)".to_string()
                } else {
                    "MCP remote server refused the credential (HTTP 403)".to_string()
                },
            )
            .with_code(code));
        }
        if status == 404 && session_id.is_some() {
            return Ok(SendOutcome::SessionExpired);
        }
        if matches!(status, 202 | 204) {
            return Ok(SendOutcome::Accepted);
        }
        if !(200..300).contains(&status) {
            let detail = read_error_body(response, self.output_cap).await;
            let retryable = status >= 500 || status == 429;
            let code = json_error_code(&detail).unwrap_or(TRANSPORT_CODE);
            return Err(Error::new(
                ErrorKind::Provider {
                    code: code.to_string(),
                    retryable,
                },
                format!(
                    "MCP remote HTTP {status}: {}",
                    sanitize_message(
                        &json_error_message(&detail).unwrap_or_default(),
                        token.as_ref()
                    )
                ),
            ));
        }
        if !expect_response {
            // A notification may still return a body; drain it under the
            // same bound instead of leaking an unread stream.
            let _ = drain(response, self.budget(deadline), self.output_cap).await;
            return Ok(SendOutcome::Accepted);
        }
        let content_type = header_text(response.headers(), "content-type").unwrap_or_default();
        let value = if content_type
            .to_ascii_lowercase()
            .starts_with("text/event-stream")
        {
            match read_sse_response(response, id, self.budget(deadline), self.output_cap).await {
                Ok(value) => value,
                Err(error) => {
                    if matches!(error.kind, ErrorKind::Timeout) {
                        self.cancel_best_effort(id).await;
                    }
                    return Err(error);
                }
            }
        } else {
            let bytes = match response.read_bytes(&self.budget(deadline)).await {
                Ok(bytes) => bytes,
                Err(error) => {
                    let error = map_egress(error);
                    if matches!(error.kind, ErrorKind::Timeout) {
                        self.cancel_best_effort(id).await;
                    }
                    return Err(error);
                }
            };
            let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
                self.protocol_error(format!("MCP remote response is not JSON: {error}"))
            })?;
            select_response(&value, id).ok_or_else(|| {
                self.protocol_error(format!("MCP remote response did not match request id {id}"))
            })?
        };
        Ok(SendOutcome::Reply(Reply {
            value,
            session_id: session_header,
        }))
    }

    fn budget(&self, deadline: tokio::time::Instant) -> ResponseBudget {
        let remaining = deadline
            .saturating_duration_since(tokio::time::Instant::now())
            .max(Duration::from_millis(1));
        let head = remaining.min(Duration::from_secs(10));
        ResponseBudget::new(
            head,
            remaining,
            remaining,
            self.output_cap as u64,
            Some(MAX_SSE_EVENTS as u64),
        )
    }

    fn protocol_error(&self, detail: impl Into<String>) -> Error {
        Error::new(
            ErrorKind::Provider {
                code: PROTOCOL_CODE.into(),
                retryable: false,
            },
            detail.into(),
        )
    }

    fn jsonrpc_error(&self, method: &str, value: &Value) -> Error {
        let error = value.get("error").cloned().unwrap_or(Value::Null);
        let code = error
            .get("code")
            .and_then(|code| code.as_i64())
            .map(|code| code.to_string())
            .unwrap_or_else(|| "unknown".into());
        let raw_message = error
            .get("message")
            .and_then(|message| message.as_str())
            .unwrap_or("");
        let scrubbed = self
            .auth
            .as_ref()
            .map(|auth| auth.scrub(raw_message))
            .unwrap_or_else(|| raw_message.to_string());
        let message = sanitize_message(&scrubbed, self.token.as_ref());
        Error::new(
            ErrorKind::Provider {
                code: JSONRPC_ERROR_CODE.into(),
                retryable: false,
            },
            format!("MCP {method} returned JSON-RPC error {code}: {message}"),
        )
    }

    /// Terminate the session best-effort (`DELETE`). The connection is
    /// marked closed even when the server refuses the request.
    pub async fn close(&self) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        let (session_id, protocol_version) = {
            let session = self.session.read().await;
            let Some(current) = session.as_ref() else {
                return;
            };
            (current.id.clone(), current.protocol_version.clone())
        };
        let Some(session_id) = session_id else {
            return;
        };
        if let Ok(session) = HeaderValue::from_str(&session_id) {
            let mut refreshed = false;
            loop {
                let token = self.bearer().await.ok().flatten();
                let Ok(mut builder) = self.client.delete(self.endpoint.as_str()) else {
                    break;
                };
                builder = builder.header(HeaderName::from_static(SESSION_HEADER), session.clone());
                builder = builder.header(
                    HeaderName::from_static(PROTOCOL_HEADER),
                    protocol_version.clone(),
                );
                if let Some(token) = &token {
                    if let Ok(value) = token.authorization_header() {
                        builder = builder.header(AUTHORIZATION, value);
                    }
                }
                let response =
                    tokio::time::timeout(CANCEL_TIMEOUT, self.client.send_checked(builder)).await;
                let rejected =
                    matches!(&response, Ok(Ok(response)) if response.status().as_u16() == 401);
                if rejected && !refreshed {
                    if let Some(auth) = &self.auth {
                        // A session DELETE is best-effort, but a rejected OAuth
                        // bearer still gets the same single refresh+retry.
                        if auth.refresh().await.is_ok() {
                            refreshed = true;
                            continue;
                        }
                    }
                }
                break;
            }
        }
        *self.session.write().await = None;
    }
}

/// Small extension so auth errors carry the stable capability code without
/// reaching into `ErrorKind` construction at every call site.
trait ErrorCodeExt {
    fn with_code(self, code: &'static str) -> Self;
}

impl ErrorCodeExt for Error {
    fn with_code(mut self, code: &'static str) -> Self {
        if matches!(self.kind, ErrorKind::Permission) {
            self.kind = ErrorKind::Provider {
                code: code.into(),
                retryable: false,
            };
        }
        self
    }
}

fn parse_endpoint(raw: &str) -> Result<Url, Error> {
    let url = Url::parse(raw).map_err(|_| {
        Error::new(
            ErrorKind::Malformed,
            "MCP endpoint must be an absolute http(s) URL",
        )
    })?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(Error::new(
            ErrorKind::Malformed,
            "MCP endpoint must be an absolute http(s) URL",
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(Error::new(
            ErrorKind::Malformed,
            "MCP endpoint must not embed userinfo credentials",
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(Error::new(
            ErrorKind::Malformed,
            "MCP endpoint must not carry a query or fragment",
        ));
    }
    Ok(url)
}

fn endpoint_policy(endpoint: &Url) -> Result<DestinationPolicy, Error> {
    let host = endpoint
        .host_str()
        .ok_or_else(|| Error::new(ErrorKind::Malformed, "MCP endpoint has no host"))?;
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    let port = endpoint.port_or_known_default().ok_or_else(|| {
        Error::new(
            ErrorKind::Malformed,
            "MCP endpoint has no known effective port",
        )
    })?;
    let rule = format!("{}://{host}:{port}", endpoint.scheme());
    DestinationPolicy::parse_lines([rule.as_str()]).map_err(|error| {
        Error::new(
            ErrorKind::Malformed,
            format!("MCP endpoint policy: {error}"),
        )
    })
}

fn bounded_request_body(request: &Value) -> Result<String, Error> {
    let body = request.to_string();
    if body.len() > MAX_REQUEST_BYTES {
        return Err(Error::new(
            ErrorKind::Oversized,
            format!("MCP request body exceeded the {MAX_REQUEST_BYTES}-byte bound"),
        ));
    }
    Ok(body)
}

fn valid_session_id(session: &str) -> bool {
    !session.is_empty()
        && session.len() <= 256
        && session.bytes().all(|byte| byte.is_ascii_graphic())
}

fn header_text(headers: &reqwest::header::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

fn select_response(value: &Value, id: u64) -> Option<Value> {
    let matches_id = |candidate: &Value| {
        candidate
            .get("id")
            .and_then(|value| value.as_u64())
            .is_some_and(|value| value == id)
    };
    match value {
        Value::Array(items) => items.iter().find(|item| matches_id(item)).cloned(),
        Value::Object(_) if matches_id(value) => Some(value.clone()),
        _ => None,
    }
}

async fn read_sse_response(
    response: CheckedResponse,
    id: u64,
    budget: ResponseBudget,
    cap: usize,
) -> Result<Value, Error> {
    let mut stream = Box::pin(response.stream_frames(&budget));
    let mut parser = SseParser::new(cap);
    let mut response_value: Option<Value> = None;
    let mut events = 0usize;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(map_egress)?;
        for value in parser.feed(&chunk)? {
            events += 1;
            if events > MAX_SSE_EVENTS {
                return Err(Error::new(
                    ErrorKind::Oversized,
                    format!("MCP SSE stream exceeded the {MAX_SSE_EVENTS}-event bound"),
                ));
            }
            if let Some(found) = select_response(&value, id) {
                response_value = Some(found);
            }
        }
        if response_value.is_some() {
            break;
        }
    }
    response_value.ok_or_else(|| {
        Error::new(
            ErrorKind::Provider {
                code: PROTOCOL_CODE.into(),
                retryable: false,
            },
            format!("MCP SSE stream ended without a response for request id {id}"),
        )
    })
}

async fn drain(response: CheckedResponse, budget: ResponseBudget, cap: usize) -> Result<(), Error> {
    let bytes = response.read_bytes(&budget).await.map_err(map_egress)?;
    if bytes.len() > cap {
        return Err(Error::new(
            ErrorKind::Oversized,
            format!("MCP notification response exceeded the {cap}-byte bound"),
        ));
    }
    Ok(())
}

async fn read_error_body(response: CheckedResponse, cap: usize) -> Value {
    let limit = (cap as u64).clamp(1, MAX_ERROR_BODY);
    let budget = ResponseBudget::for_timeout(Duration::from_secs(2), limit);
    match response.read_bytes(&budget).await {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        Err(_) => Value::Null,
    }
}

fn json_error_message(value: &Value) -> Option<String> {
    value
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(|message| message.as_str())
        .map(str::to_string)
}

fn json_error_code(value: &Value) -> Option<&'static str> {
    let code = value
        .get("error")
        .and_then(|error| error.get("code"))
        .and_then(|code| code.as_i64())?;
    match code {
        -32600 => Some(JSONRPC_ERROR_CODE),
        -32601 => Some(JSONRPC_ERROR_CODE),
        -32602 => Some(JSONRPC_ERROR_CODE),
        -32603 => Some(JSONRPC_ERROR_CODE),
        _ => None,
    }
}

/// Never echo a bearer token in a diagnostic, even when a hostile server
/// reflects it into an error message. The message is length-bounded too.
fn sanitize_message(message: &str, token: Option<&SecretToken>) -> String {
    let mut out = message.replace(['\r', '\n'], " ");
    if let Some(token) = token {
        let secret = token.value.as_str();
        if !secret.is_empty() {
            out = out.replace(secret, "[redacted]");
        }
    }
    out.chars().take(MAX_MESSAGE_CHARS).collect()
}

fn map_egress(error: EgressError) -> Error {
    match error {
        EgressError::ResponseBudgetExceeded { component, .. } => {
            use faktor_provider::egress::BudgetComponent;
            match component {
                BudgetComponent::Bytes | BudgetComponent::Frames => Error::new(
                    ErrorKind::Oversized,
                    format!("MCP remote response exceeded its {component} budget"),
                ),
                BudgetComponent::Head | BudgetComponent::Idle | BudgetComponent::Total => {
                    Error::new(
                        ErrorKind::Timeout,
                        format!("MCP remote response exceeded its {component} budget"),
                    )
                }
            }
        }
        EgressError::Denied { reason, .. } => Error::new(
            ErrorKind::Permission,
            format!("MCP remote destination denied: {reason}"),
        ),
        EgressError::SecretBlocked { kinds } => Error::new(
            ErrorKind::Permission,
            format!(
                "MCP remote request blocked by the outbound secret scan ({})",
                kinds.join(", ")
            ),
        ),
        EgressError::BodyTooLarge { limit_bytes } => Error::new(
            ErrorKind::Oversized,
            format!("MCP remote request exceeds the {limit_bytes}-byte scan bound"),
        ),
        EgressError::AddressClassRefused { class, .. } => Error::new(
            ErrorKind::Permission,
            format!("MCP remote destination refused: {class} address"),
        ),
        EgressError::UnsupportedScheme(scheme) => Error::new(
            ErrorKind::Permission,
            format!("MCP remote scheme {scheme:?} is not fetchable"),
        ),
        EgressError::UnparseableUrl(reason) => {
            Error::new(ErrorKind::Malformed, format!("MCP remote URL: {reason}"))
        }
        EgressError::Transport(detail) => Error::new(
            ErrorKind::Network,
            format!("MCP remote transport: {detail}"),
        ),
        EgressError::ClientBuild { detail } => Error::new(
            ErrorKind::Internal,
            format!("MCP remote checked client: {detail}"),
        ),
        other => Error::new(
            ErrorKind::Provider {
                code: TRANSPORT_CODE.into(),
                retryable: false,
            },
            format!("MCP remote transport: {other}"),
        ),
    }
}

/// Incremental SSE parser: LF/CRLF separated lines, multi-line `data:`
/// fields joined with `\n`, comments and non-data fields ignored, and a hard
/// byte/event bound. Each complete event must be one JSON-RPC document.
struct SseParser {
    buffer: Vec<u8>,
    data: String,
    cap: usize,
    seen_bytes: usize,
}

impl SseParser {
    fn new(cap: usize) -> Self {
        Self {
            buffer: Vec::new(),
            data: String::new(),
            cap,
            seen_bytes: 0,
        }
    }

    fn feed(&mut self, chunk: &[u8]) -> Result<Vec<Value>, Error> {
        self.seen_bytes = self.seen_bytes.saturating_add(chunk.len());
        if self.seen_bytes > self.cap || self.buffer.len() + chunk.len() > self.cap {
            return Err(Error::new(
                ErrorKind::Oversized,
                format!(
                    "MCP SSE event data exceeded the {}-byte output cap",
                    self.cap
                ),
            ));
        }
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();
        // Scan forward with a cursor and compact ONCE per chunk. Draining the
        // consumed prefix per line made a many-line SSE body O(n^2) in copied
        // bytes; this keeps it O(n) while preserving event semantics.
        let mut start = 0usize;
        while let Some(offset) = self.buffer[start..].iter().position(|byte| *byte == b'\n') {
            let end = start + offset;
            let mut line_end = end;
            if line_end > start && self.buffer[line_end - 1] == b'\r' {
                line_end -= 1;
            }
            let line = std::str::from_utf8(&self.buffer[start..line_end])
                .map_err(|_| Error::new(ErrorKind::Malformed, "MCP SSE stream is not UTF-8"))?;
            start = end + 1;
            if line.is_empty() {
                if self.data.is_empty() {
                    continue;
                }
                let payload = std::mem::take(&mut self.data);
                let value: Value = serde_json::from_str(payload.trim_end()).map_err(|error| {
                    Error::new(
                        ErrorKind::Provider {
                            code: PROTOCOL_CODE.into(),
                            retryable: false,
                        },
                        format!("MCP SSE event is not JSON: {error}"),
                    )
                })?;
                out.push(value);
                continue;
            }
            if let Some(data) = line.strip_prefix("data:") {
                if self.data.len() + data.len() + 1 > self.cap {
                    return Err(Error::new(
                        ErrorKind::Oversized,
                        format!(
                            "MCP SSE event data exceeded the {}-byte output cap",
                            self.cap
                        ),
                    ));
                }
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(data.strip_prefix(' ').unwrap_or(data));
            }
            // `event:`, `id:`, `retry:` and comments carry no JSON payload.
        }
        if start > 0 {
            self.buffer.drain(..start);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_parsing_is_linear_in_lines_per_chunk() {
        let mut parser = SseParser::new(8 * 1024 * 1024);
        let mut body = String::with_capacity(1024 * 1024);
        for _ in 0..100_000 {
            body.push_str("data: {}\n\n");
        }
        let started = std::time::Instant::now();
        let events = parser.feed(body.as_bytes()).unwrap();
        assert_eq!(events.len(), 100_000);
        eprintln!(
            "perf MCP SSE: 100k lines / {} bytes parsed in {:?}",
            body.len(),
            started.elapsed()
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "SSE parsing a single 1.2 MB chunk took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn token_debug_never_reveals_the_secret() {
        let token = SecretToken::new("sk-super-secret");
        let debug = format!("{token:?}");
        assert!(!debug.contains("sk-super-secret"), "{debug}");
        assert_eq!(token.expose_for_test(), "sk-super-secret");
    }

    #[test]
    fn sanitize_redacts_reflected_tokens_and_newlines() {
        let token = SecretToken::new("sk-super-secret");
        let message = sanitize_message("bad sk-super-secret\r\nnext", Some(&token));
        assert!(!message.contains("sk-super-secret"), "{message}");
        assert!(!message.contains('\n'), "{message}");
    }

    #[test]
    fn capabilities_decode_only_advertised_features() {
        let caps = RemoteCapabilities::from_value(&json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {
                "tools": { "listChanged": true },
                "resources": {},
                "logging": {}
            }
        }));
        assert!(caps.tools);
        assert!(caps.tools_list_changed);
        assert!(caps.resources);
        assert!(!caps.resources_list_changed);
        assert!(caps.logging);
        assert!(!caps.prompts);
    }

    #[test]
    fn sse_parser_handles_fragmentation_comments_and_crlf() {
        let mut parser = SseParser::new(4096);
        let mut out = parser
            .feed(b": comment\r\nevent: message\r\ndata: {\"jsonrpc\":\"2.0\",\r\n")
            .unwrap();
        assert!(out.is_empty());
        out = parser
            .feed(b"data: \"id\":1,\"result\":{\"ok\":true}}\r\n\r\n")
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["id"], 1);
        assert_eq!(out[0]["result"]["ok"], true);
    }

    #[test]
    fn sse_parser_joins_multiline_data_and_refuses_oversize() {
        let mut parser = SseParser::new(128);
        let out = parser
            .feed(b"data: {\"jsonrpc\":\"2.0\",\"id\":1,\ndata: \"result\":{}}\n\n")
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["id"], 1);
        let mut small = SseParser::new(8);
        assert!(small.feed(&[b'x'; 16]).is_err());
    }

    #[test]
    fn session_ids_are_bounded_graphic_ascii() {
        assert!(valid_session_id("sess-0"));
        assert!(!valid_session_id(""));
        assert!(!valid_session_id("has space"));
        assert!(!valid_session_id("line\nbreak"));
        assert!(!valid_session_id(&"x".repeat(257)));
    }

    #[tokio::test]
    async fn production_connect_refuses_plaintext_http() {
        let mut cfg = RemoteMcpConfig::new("remote", "http://mcp.example.com/rpc");
        cfg.timeout = Duration::from_millis(100);
        let error = match RemoteMcpServer::connect(cfg).await {
            Ok(_) => panic!("plaintext HTTP must not produce a remote session"),
            Err(error) => error,
        };
        assert_eq!(error.kind, ErrorKind::Malformed, "{error:?}");
    }

    #[test]
    fn endpoint_policy_allows_only_the_configured_destination() {
        let endpoint = parse_endpoint("https://mcp.example.com/rpc").unwrap();
        let policy = endpoint_policy(&endpoint).unwrap();
        assert!(policy
            .check("https", "mcp.example.com", 443, false, None)
            .is_allowed());
        assert!(!policy
            .check("https", "evil.example.com", 443, false, None)
            .is_allowed());
        assert!(parse_endpoint("https://user:pass@mcp.example.com/rpc").is_err());
        assert!(parse_endpoint("https://mcp.example.com/rpc?token=x").is_err());
    }
}
