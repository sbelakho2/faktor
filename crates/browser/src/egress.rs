//! The Faktor Browser Egress Broker (spec §9): the mandatory local HTTP
//! proxy that is Chromium's only configured egress path.
//!
//! The broker is where destination policy, request accounting, upstream
//! proxy selection, credential isolation, redacted logging and health live:
//!
//! * **Destination filtering** — a per-connector [`DestinationPolicy`]
//!   (first-party only by default) decides every `CONNECT`/absolute-URI
//!   request before a socket to the destination is opened. Unknown hosts are
//!   refused; the refusal is counted and logged (redacted).
//! * **Credential isolation** — upstream proxy credentials live only in this
//!   process. Chromium never receives them; client-supplied
//!   `Proxy-Authorization` headers are stripped, and the broker adds the
//!   upstream credential only on the broker→upstream leg.
//! * **Accounting** — requests, blocked requests, tunnels, bytes each way,
//!   per-destination counts, all atomic and monotonic.
//! * **Health** — a typed snapshot the daemon can surface.
//!
//! The broker is not an anti-bot subsystem and does not inspect payloads
//! beyond the request head it must route.

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use faktor_core::cancellation::CancellationToken;

use crate::error::BrowserError;
use crate::interception::ResourceType;
use crate::timeutil::now_ms;

/// A host allow/deny pattern.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostPattern {
    /// Exact host match (`example.com`).
    Exact(String),
    /// Domain-suffix match: `.example.com` matches `example.com` and any
    /// subdomain. `*` matches every host (explicit, never implicit).
    Suffix(String),
}

impl HostPattern {
    /// Parse a pattern: `*`, `example.com`, `.example.com`, `*.example.com`.
    pub fn parse(raw: &str) -> Result<Self, BrowserError> {
        let raw = raw.trim().to_ascii_lowercase();
        if raw.is_empty() {
            return Err(BrowserError::invalid_config("empty host pattern"));
        }
        if raw == "*" {
            return Ok(HostPattern::Suffix(String::new()));
        }
        let normalized = raw.trim_start_matches("*.").trim_start_matches('.');
        if normalized.is_empty()
            || !normalized
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_')
        {
            return Err(BrowserError::invalid_config(format!(
                "invalid host pattern {raw:?}"
            )));
        }
        if raw.starts_with("*.") || raw.starts_with('.') {
            Ok(HostPattern::Suffix(format!(".{normalized}")))
        } else {
            Ok(HostPattern::Exact(normalized.to_string()))
        }
    }

    pub fn matches(&self, host: &str) -> bool {
        let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
        match self {
            HostPattern::Exact(exact) => host == *exact,
            HostPattern::Suffix(suffix) => {
                if suffix.is_empty() {
                    return true;
                }
                host.ends_with(suffix.as_str()) || host == suffix.trim_start_matches('.')
            }
        }
    }
}

/// Why a destination was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockReason {
    /// Not in the connector's first-party set.
    NotFirstParty,
    /// Matches an explicit deny pattern (tracking/ads/marketing).
    ExplicitlyBlocked,
    /// Scheme is not permitted by the policy.
    SchemeNotAllowed,
    /// The request target could not be parsed.
    Malformed,
    /// Blocked by resource type (interception), carried through accounting.
    ResourceTypeBlocked,
}

impl BlockReason {
    pub fn as_str(self) -> &'static str {
        match self {
            BlockReason::NotFirstParty => "not_first_party",
            BlockReason::ExplicitlyBlocked => "explicitly_blocked",
            BlockReason::SchemeNotAllowed => "scheme_not_allowed",
            BlockReason::Malformed => "malformed",
            BlockReason::ResourceTypeBlocked => "resource_type_blocked",
        }
    }
}

impl fmt::Display for BlockReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The outcome of a policy decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DestinationDecision {
    Allowed,
    Blocked { reason: BlockReason },
}

impl DestinationDecision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, DestinationDecision::Allowed)
    }
}

/// The per-connector destination policy (spec §9). Default posture: deny
/// everything that is not explicitly first-party; block video/audio/images/
/// fonts/tracking resource types unless the connector says otherwise.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DestinationPolicy {
    /// Hosts the connector considers first-party (and its permitted CDNs).
    pub first_party: Vec<HostPattern>,
    /// Explicit deny patterns (generic ad/tracking hosts, or connector
    /// additions). Deny wins over allow.
    pub blocked_hosts: Vec<HostPattern>,
    /// Resource types that request interception drops by default.
    pub blocked_resource_types: Vec<ResourceType>,
    /// Permitted URL schemes (lowercase). Defaults to http/https.
    pub allow_schemes: Vec<String>,
}

impl DestinationPolicy {
    /// The strict default: only the listed first-party hosts, http/https,
    /// with the aggressive default resource-type drop set.
    pub fn first_party_only(hosts: Vec<HostPattern>) -> Self {
        Self {
            first_party: hosts,
            blocked_hosts: Vec::new(),
            blocked_resource_types: ResourceType::default_blocked(),
            allow_schemes: vec!["http".to_string(), "https".to_string()],
        }
    }

    /// Add explicit deny patterns (deny always wins).
    pub fn with_blocked_hosts(mut self, hosts: Vec<HostPattern>) -> Self {
        self.blocked_hosts = hosts;
        self
    }

    pub fn validate(&self) -> Result<(), BrowserError> {
        for host in &self.first_party {
            if let HostPattern::Suffix(suffix) = host {
                if suffix.is_empty() {
                    return Err(BrowserError::invalid_config(
                        "destination policy may not allow every host (`*`); name the connector's first-party hosts",
                    ));
                }
            }
        }
        if self.allow_schemes.is_empty() {
            return Err(BrowserError::invalid_config(
                "destination policy must allow at least one scheme",
            ));
        }
        for scheme in &self.allow_schemes {
            if !["http", "https", "ws", "wss"].contains(&scheme.as_str()) {
                return Err(BrowserError::invalid_config(format!(
                    "unsupported destination scheme {scheme:?}"
                )));
            }
        }
        Ok(())
    }

    /// Decide a bare host (no port). Deny wins; unknown hosts are blocked.
    pub fn decide_host(&self, host: &str) -> DestinationDecision {
        if self.blocked_hosts.iter().any(|p| p.matches(host)) {
            return DestinationDecision::Blocked {
                reason: BlockReason::ExplicitlyBlocked,
            };
        }
        if self.first_party.iter().any(|p| p.matches(host)) {
            return DestinationDecision::Allowed;
        }
        DestinationDecision::Blocked {
            reason: BlockReason::NotFirstParty,
        }
    }

    /// Decide a full URL (scheme + host + optional port).
    pub fn decide_url(&self, url: &str) -> DestinationDecision {
        let Some((scheme, host)) = split_scheme_host(url) else {
            return DestinationDecision::Blocked {
                reason: BlockReason::Malformed,
            };
        };
        if !self
            .allow_schemes
            .iter()
            .any(|s| s.eq_ignore_ascii_case(scheme))
        {
            return DestinationDecision::Blocked {
                reason: BlockReason::SchemeNotAllowed,
            };
        }
        self.decide_host(&host)
    }

    /// Is this resource type dropped by default interception?
    pub fn blocks_resource_type(&self, resource_type: ResourceType) -> bool {
        self.blocked_resource_types.contains(&resource_type)
    }
}

/// Split `scheme://host[:port]/...` into (scheme, host) — no URL library
/// needed for the two forms this proxy accepts.
pub(crate) fn split_scheme_host(url: &str) -> Option<(&str, String)> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty() {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit('@').next()?; // strip userinfo if hostile
    if authority.is_empty() {
        return None;
    }
    let host = if authority.starts_with('[') {
        // IPv6 literal: [::1]:8080
        let end = authority.find(']')?;
        authority[1..end].to_string()
    } else {
        authority.split(':').next()?.to_string()
    };
    if host.is_empty() {
        return None;
    }
    Some((scheme, host))
}

/// Upstream proxy credentials. Debug is redacted; there is no Display, no
/// Serialize, and no accessor for the password outside the broker's own
/// `Proxy-Authorization` writer.
#[derive(Clone, PartialEq, Eq)]
pub struct ProxyCredentials {
    username: String,
    password: String,
}

impl ProxyCredentials {
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            password: password.into(),
        }
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    /// The Basic `Proxy-Authorization` value. Only ever written to the
    /// broker→upstream socket.
    fn basic_header(&self) -> String {
        use base64::Engine as _;
        let raw = format!("{}:{}", self.username, self.password);
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(raw)
        )
    }
}

impl fmt::Debug for ProxyCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyCredentials")
            .field("username", &self.username)
            .field("password", &"[redacted]")
            .finish()
    }
}

/// One upstream HTTP proxy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamProxy {
    pub host: String,
    pub port: u16,
    pub credentials: Option<ProxyCredentials>,
}

impl UpstreamProxy {
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            credentials: None,
        }
    }

    pub fn with_credentials(mut self, credentials: ProxyCredentials) -> Self {
        self.credentials = Some(credentials);
        self
    }

    fn validate(&self) -> Result<(), BrowserError> {
        if self.host.is_empty() || self.port == 0 {
            return Err(BrowserError::invalid_config(
                "upstream proxy needs a non-empty host and non-zero port",
            ));
        }
        Ok(())
    }
}

/// Upstream proxy selection: a default plus per-destination overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpstreamSelector {
    pub default: Option<UpstreamProxy>,
    pub by_host: Vec<(HostPattern, UpstreamProxy)>,
}

impl UpstreamSelector {
    pub fn new(default: Option<UpstreamProxy>) -> Self {
        Self {
            default,
            by_host: Vec::new(),
        }
    }

    pub fn with_route(mut self, pattern: HostPattern, proxy: UpstreamProxy) -> Self {
        self.by_host.push((pattern, proxy));
        self
    }

    pub fn select(&self, host: &str) -> Option<&UpstreamProxy> {
        self.by_host
            .iter()
            .find(|(pattern, _)| pattern.matches(host))
            .map(|(_, proxy)| proxy)
            .or(self.default.as_ref())
    }

    fn validate(&self) -> Result<(), BrowserError> {
        if let Some(default) = &self.default {
            default.validate()?;
        }
        for (_, proxy) in &self.by_host {
            proxy.validate()?;
        }
        Ok(())
    }
}

/// Broker configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerConfig {
    /// Loopback bind address (`127.0.0.1:0` by default).
    pub bind: SocketAddr,
    pub policy: DestinationPolicy,
    pub upstream: UpstreamSelector,
    /// Request-head byte cap (headers only; bodies stream through).
    pub max_request_bytes: usize,
    /// Concurrent connection ceiling.
    pub max_connections: usize,
    /// TCP connect timeout to destinations/upstreams.
    pub connect_timeout_ms: u64,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:0".parse().expect("loopback address literal"),
            policy: DestinationPolicy::first_party_only(Vec::new()),
            upstream: UpstreamSelector::default(),
            max_request_bytes: 32 * 1024,
            max_connections: 64,
            connect_timeout_ms: 10_000,
        }
    }
}

impl BrokerConfig {
    pub fn validate(&self) -> Result<(), BrowserError> {
        if !self.bind.ip().is_loopback() {
            return Err(BrowserError::invalid_config(format!(
                "egress broker must bind loopback, not {}",
                self.bind
            )));
        }
        if self.max_request_bytes == 0 || self.max_connections == 0 {
            return Err(BrowserError::invalid_config(
                "broker request/connection bounds must be > 0",
            ));
        }
        self.policy.validate()?;
        self.upstream.validate()?;
        Ok(())
    }
}

/// Monotonic broker accounting.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EgressAccounting {
    pub requests_total: u64,
    pub blocked_total: u64,
    pub tunnels_total: u64,
    pub errors_total: u64,
    pub bytes_up: u64,
    pub bytes_down: u64,
    pub per_host: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokerState {
    Running,
    Stopped,
}

/// A typed health snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerHealth {
    pub state: BrokerState,
    pub addr: SocketAddr,
    pub active_connections: usize,
    pub accounting: EgressAccounting,
    pub last_error: Option<String>,
    pub uptime_ms: i64,
}

struct BrokerInner {
    addr: SocketAddr,
    policy: DestinationPolicy,
    upstream: UpstreamSelector,
    max_request_bytes: usize,
    connect_timeout: Duration,
    accounting: Mutex<AccountingCounters>,
    active: AtomicUsize,
    started_ms: i64,
    running: AtomicBool,
    last_error: Mutex<Option<String>>,
    shutdown: CancellationToken,
}

#[derive(Default)]
struct AccountingCounters {
    requests_total: u64,
    blocked_total: u64,
    tunnels_total: u64,
    errors_total: u64,
    bytes_up: u64,
    bytes_down: u64,
    per_host: BTreeMap<String, u64>,
}

impl BrokerInner {
    fn account_request(&self, host: &str) {
        let mut counters = self.accounting.lock().unwrap();
        counters.requests_total = counters.requests_total.saturating_add(1);
        let entry = counters.per_host.entry(host.to_string()).or_insert(0);
        *entry = entry.saturating_add(1);
    }

    fn account_blocked(&self) {
        let mut counters = self.accounting.lock().unwrap();
        counters.blocked_total = counters.blocked_total.saturating_add(1);
    }

    fn account_tunnel(&self) {
        let mut counters = self.accounting.lock().unwrap();
        counters.tunnels_total = counters.tunnels_total.saturating_add(1);
    }

    fn account_error(&self, error: impl Into<String>) {
        let error = error.into();
        {
            let mut counters = self.accounting.lock().unwrap();
            counters.errors_total = counters.errors_total.saturating_add(1);
        }
        *self.last_error.lock().unwrap() = Some(error);
    }

    fn account_bytes(&self, up: u64, down: u64) {
        let mut counters = self.accounting.lock().unwrap();
        counters.bytes_up = counters.bytes_up.saturating_add(up);
        counters.bytes_down = counters.bytes_down.saturating_add(down);
    }

    fn snapshot(&self) -> EgressAccounting {
        let counters = self.accounting.lock().unwrap();
        EgressAccounting {
            requests_total: counters.requests_total,
            blocked_total: counters.blocked_total,
            tunnels_total: counters.tunnels_total,
            errors_total: counters.errors_total,
            bytes_up: counters.bytes_up,
            bytes_down: counters.bytes_down,
            per_host: counters.per_host.clone(),
        }
    }
}

/// A running broker. Dropping the handle aborts the accept loop; explicit
/// [`BrokerHandle::shutdown`] awaits it.
pub struct BrokerHandle {
    inner: Arc<BrokerInner>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl std::fmt::Debug for BrokerHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BrokerHandle")
            .field("addr", &self.inner.addr)
            .field("policy", &self.inner.policy)
            .finish()
    }
}

impl BrokerHandle {
    pub fn addr(&self) -> SocketAddr {
        self.inner.addr
    }

    /// The proxy URL Chromium is launched with — always loopback.
    pub fn proxy_url(&self) -> String {
        format!("http://{}", self.inner.addr)
    }

    pub fn policy(&self) -> &DestinationPolicy {
        &self.inner.policy
    }

    pub fn accounting(&self) -> EgressAccounting {
        self.inner.snapshot()
    }

    pub fn health(&self) -> BrokerHealth {
        BrokerHealth {
            state: if self.inner.running.load(Ordering::SeqCst) {
                BrokerState::Running
            } else {
                BrokerState::Stopped
            },
            addr: self.inner.addr,
            active_connections: self.inner.active.load(Ordering::SeqCst),
            accounting: self.inner.snapshot(),
            last_error: self.inner.last_error.lock().unwrap().clone(),
            uptime_ms: now_ms().saturating_sub(self.inner.started_ms),
        }
    }

    /// Stop the broker: the listener closes, in-flight copies finish, and
    /// the accept task is joined.
    pub async fn shutdown(&self) {
        self.inner.shutdown.cancel();
        self.inner.running.store(false, Ordering::SeqCst);
        let task = self.task.lock().unwrap().take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}

impl Drop for BrokerHandle {
    fn drop(&mut self) {
        self.inner.shutdown.cancel();
        self.inner.running.store(false, Ordering::SeqCst);
        if let Some(task) = self.task.lock().unwrap().take() {
            task.abort();
        }
    }
}

/// The broker entry point.
pub struct EgressBroker;

impl EgressBroker {
    /// Start the broker on its configured loopback address. Nothing is
    /// accepted until the listener exists; a bind failure is typed.
    pub async fn start(config: BrokerConfig) -> Result<BrokerHandle, BrowserError> {
        config.validate()?;
        let listener =
            TcpListener::bind(config.bind)
                .await
                .map_err(|e| BrowserError::EgressUnavailable {
                    detail: format!("cannot bind egress broker on {}: {e}", config.bind),
                })?;
        let addr = listener
            .local_addr()
            .map_err(|e| BrowserError::EgressUnavailable {
                detail: format!("cannot read broker address: {e}"),
            })?;
        if !addr.ip().is_loopback() {
            return Err(BrowserError::EgressUnavailable {
                detail: format!("broker bound a non-loopback address: {addr}"),
            });
        }
        let inner = Arc::new(BrokerInner {
            addr,
            policy: config.policy.clone(),
            upstream: config.upstream.clone(),
            max_request_bytes: config.max_request_bytes,
            connect_timeout: Duration::from_millis(config.connect_timeout_ms),
            accounting: Mutex::new(AccountingCounters::default()),
            active: AtomicUsize::new(0),
            started_ms: now_ms(),
            running: AtomicBool::new(true),
            last_error: Mutex::new(None),
            shutdown: CancellationToken::new(),
        });
        let max_connections = config.max_connections;
        let accept_inner = inner.clone();
        let task = tokio::spawn(async move {
            let permits = Arc::new(tokio::sync::Semaphore::new(max_connections));
            loop {
                tokio::select! {
                    biased;
                    _ = accept_inner.shutdown.cancelled() => break,
                    accepted = listener.accept() => {
                        match accepted {
                            Ok((stream, _peer)) => {
                                let Ok(permit) = permits.clone().try_acquire_owned() else {
                                    accept_inner.account_error("connection ceiling reached; refusing");
                                    drop(stream);
                                    continue;
                                };
                                let conn_inner = accept_inner.clone();
                                tokio::spawn(async move {
                                    conn_inner.active.fetch_add(1, Ordering::SeqCst);
                                    if let Err(error) = serve_connection(stream, conn_inner.clone()).await {
                                        conn_inner.account_error(error);
                                    }
                                    conn_inner.active.fetch_sub(1, Ordering::SeqCst);
                                    drop(permit);
                                });
                            }
                            Err(e) => {
                                accept_inner.account_error(format!("accept failed: {e}"));
                                tokio::time::sleep(Duration::from_millis(20)).await;
                            }
                        }
                    }
                }
            }
        });
        Ok(BrokerHandle {
            inner,
            task: Mutex::new(Some(task)),
        })
    }
}

#[derive(Debug)]
enum HeadError {
    Closed,
    TooLarge(usize),
    Io(String),
}

/// Read an HTTP request/response head (through `\r\n\r\n`), returning the
/// head text and any leftover bytes already read past it.
async fn read_head(
    stream: &mut TcpStream,
    max_bytes: usize,
) -> Result<(String, Vec<u8>), HeadError> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(pos) = find_head_end(&buf) {
            let head = String::from_utf8_lossy(&buf[..pos]).to_string();
            let leftover = buf[pos..].to_vec();
            return Ok((head, leftover));
        }
        if buf.len() > max_bytes {
            return Err(HeadError::TooLarge(buf.len()));
        }
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|e| HeadError::Io(e.to_string()))?;
        if read == 0 {
            if buf.is_empty() {
                return Err(HeadError::Closed);
            }
            return Err(HeadError::Io("connection closed mid-head".to_string()));
        }
        buf.extend_from_slice(&chunk[..read]);
    }
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

struct ParsedRequest {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
}

fn parse_head(head: &str) -> Result<ParsedRequest, String> {
    let mut lines = head.split("\r\n");
    let request_line = lines.next().ok_or("empty request")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or("missing method")?.to_string();
    let target = parts.next().ok_or("missing target")?.to_string();
    let version = parts.next().unwrap_or("HTTP/1.1");
    if !version.starts_with("HTTP/1.") {
        return Err(format!("unsupported http version {version:?}"));
    }
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(format!("malformed header line {line:?}"));
        };
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }
    Ok(ParsedRequest {
        method,
        target,
        headers,
    })
}

fn is_hop_by_hop(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "proxy-authorization" | "proxy-connection" | "connection" | "keep-alive"
    )
}

async fn write_denial(stream: &mut TcpStream, status: &str, reason: BlockReason) {
    let body = format!("blocked by egress policy: {reason}\n");
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

async fn write_error(stream: &mut TcpStream, status: &str, detail: &str) {
    let body = format!("egress broker error: {detail}\n");
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

async fn serve_connection(mut client: TcpStream, inner: Arc<BrokerInner>) -> Result<(), String> {
    let (head, leftover) = match read_head(&mut client, inner.max_request_bytes).await {
        Ok(pair) => pair,
        Err(HeadError::Closed) => return Ok(()),
        Err(HeadError::TooLarge(size)) => {
            write_error(
                &mut client,
                "431 Request Header Fields Too Large",
                "head too large",
            )
            .await;
            return Err(format!("request head of {size} bytes exceeds the bound"));
        }
        Err(HeadError::Io(e)) => return Err(format!("head read failed: {e}")),
    };
    let request = parse_head(&head).map_err(|e| format!("malformed request head: {e}"))?;

    if request.method.eq_ignore_ascii_case("CONNECT") {
        return serve_connect(client, request, leftover, inner).await;
    }
    serve_forward(client, request, leftover, inner).await
}

async fn serve_connect(
    mut client: TcpStream,
    request: ParsedRequest,
    leftover: Vec<u8>,
    inner: Arc<BrokerInner>,
) -> Result<(), String> {
    let (host, port) = split_authority(&request.target)
        .ok_or_else(|| format!("malformed CONNECT target {:?}", request.target))?;
    inner.account_request(&host);
    let decision = inner.policy.decide_host(&host);
    if let DestinationDecision::Blocked { reason } = &decision {
        inner.account_blocked();
        tracing::info!(host = %host, reason = %reason, "egress: CONNECT blocked by policy");
        write_denial(&mut client, "403 Forbidden", *reason).await;
        return Ok(());
    }
    if !leftover.is_empty() {
        // A CONNECT head must not carry a body; treat as malformed.
        return Err("CONNECT request carried unexpected bytes".to_string());
    }
    let upstream = inner.upstream.select(&host);
    let mut target = match connect_destination(upstream, &host, port, &inner).await {
        Ok(stream) => stream,
        Err(detail) => {
            inner.account_error(detail.clone());
            write_error(&mut client, "502 Bad Gateway", &detail).await;
            return Ok(());
        }
    };
    if upstream.is_some() {
        // The upstream proxy expects its own CONNECT; credentials are added
        // here and never travel to the client.
        if let Err(detail) = send_upstream_connect(&mut target, upstream, &host, port).await {
            inner.account_error(detail.clone());
            write_error(&mut client, "502 Bad Gateway", &detail).await;
            return Ok(());
        }
    }
    inner.account_tunnel();
    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await
        .map_err(|e| format!("client write failed: {e}"))?;
    let (up, down) = tokio::io::copy_bidirectional(&mut client, &mut target)
        .await
        .map_err(|e| format!("tunnel copy failed: {e}"))?;
    inner.account_bytes(up, down);
    Ok(())
}

async fn send_upstream_connect(
    upstream_stream: &mut TcpStream,
    upstream: Option<&UpstreamProxy>,
    host: &str,
    port: u16,
) -> Result<(), String> {
    let Some(upstream) = upstream else {
        return Ok(());
    };
    let mut head = format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n");
    if let Some(credentials) = &upstream.credentials {
        head.push_str(&format!(
            "Proxy-Authorization: {}\r\n",
            credentials.basic_header()
        ));
    }
    head.push_str("Proxy-Connection: keep-alive\r\n\r\n");
    upstream_stream
        .write_all(head.as_bytes())
        .await
        .map_err(|e| format!("upstream CONNECT write failed: {e}"))?;
    let (response, _) = read_head(upstream_stream, 32 * 1024)
        .await
        .map_err(|e| format!("upstream CONNECT response failed: {e:?}"))?;
    let status = response.lines().next().unwrap_or_default();
    if !status.contains(" 200") {
        return Err(format!("upstream proxy refused CONNECT: {status}"));
    }
    Ok(())
}

async fn serve_forward(
    mut client: TcpStream,
    request: ParsedRequest,
    leftover: Vec<u8>,
    inner: Arc<BrokerInner>,
) -> Result<(), String> {
    let Some((scheme, host)) = split_scheme_host(&request.target) else {
        inner.account_blocked();
        write_denial(&mut client, "400 Bad Request", BlockReason::Malformed).await;
        return Ok(());
    };
    let scheme = scheme.to_ascii_lowercase();
    inner.account_request(&host);
    if !inner
        .policy
        .allow_schemes
        .iter()
        .any(|s| s.eq_ignore_ascii_case(&scheme))
    {
        inner.account_blocked();
        write_denial(&mut client, "403 Forbidden", BlockReason::SchemeNotAllowed).await;
        return Ok(());
    }
    if scheme != "http" {
        write_error(
            &mut client,
            "501 Not Implemented",
            "https absolute-form is not supported; use CONNECT",
        )
        .await;
        return Ok(());
    }
    let decision = inner.policy.decide_host(&host);
    if let DestinationDecision::Blocked { reason } = &decision {
        inner.account_blocked();
        tracing::info!(host = %host, reason = %reason, "egress: request blocked by policy");
        write_denial(&mut client, "403 Forbidden", *reason).await;
        return Ok(());
    }
    let port = url_host_port(&request.target)
        .map(|(_, port)| port)
        .unwrap_or(80);
    let origin_form = origin_form_of(&request.target).unwrap_or_else(|| "/".to_string());
    let upstream = inner.upstream.select(&host);
    let mut target = match connect_destination(upstream, &host, port, &inner).await {
        Ok(stream) => stream,
        Err(detail) => {
            inner.account_error(detail.clone());
            write_error(&mut client, "502 Bad Gateway", &detail).await;
            return Ok(());
        }
    };
    // Rebuild the head: hop-by-hop and any client-supplied proxy
    // credentials are stripped; the upstream credential (if any) is added
    // on this leg only.
    let mut out = String::new();
    if upstream.is_some() {
        out.push_str(&format!(
            "{} {} HTTP/1.1\r\n",
            request.method, request.target
        ));
    } else {
        out.push_str(&format!("{} {} HTTP/1.1\r\n", request.method, origin_form));
    }
    for (name, value) in &request.headers {
        if is_hop_by_hop(name) {
            continue;
        }
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    if let Some(upstream) = upstream {
        if let Some(credentials) = &upstream.credentials {
            out.push_str(&format!(
                "Proxy-Authorization: {}\r\n",
                credentials.basic_header()
            ));
        }
    }
    out.push_str("Connection: close\r\n\r\n");
    tracing::debug!(
        method = %request.method,
        host = %host,
        path = %crate::capture::redact_url(&origin_form),
        "egress: request allowed"
    );
    target
        .write_all(out.as_bytes())
        .await
        .map_err(|e| format!("upstream write failed: {e}"))?;
    if !leftover.is_empty() {
        target
            .write_all(&leftover)
            .await
            .map_err(|e| format!("upstream body write failed: {e}"))?;
    }
    let (up, down) = tokio::io::copy_bidirectional(&mut client, &mut target)
        .await
        .map_err(|e| format!("forward copy failed: {e}"))?;
    inner.account_bytes(up, down);
    Ok(())
}

async fn connect_destination(
    upstream: Option<&UpstreamProxy>,
    host: &str,
    port: u16,
    inner: &Arc<BrokerInner>,
) -> Result<TcpStream, String> {
    let (connect_host, connect_port) = match upstream {
        Some(upstream) => (upstream.host.clone(), upstream.port),
        None => (host.to_string(), port),
    };
    let attempt = TcpStream::connect((connect_host.as_str(), connect_port));
    match tokio::time::timeout(inner.connect_timeout, attempt).await {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(e)) => Err(format!("cannot connect {connect_host}:{connect_port}: {e}")),
        Err(_) => Err(format!(
            "connect to {connect_host}:{connect_port} timed out"
        )),
    }
}

/// Split `host:port` (also `[v6]:port`) with a default port per scheme.
fn split_authority(authority: &str) -> Option<(String, u16)> {
    split_authority_default(authority, 443)
}

/// Host+port of an absolute URL (`scheme://host[:port]/...`).
fn url_host_port(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let default_port = if scheme.eq_ignore_ascii_case("https") || scheme.eq_ignore_ascii_case("wss")
    {
        443
    } else {
        80
    };
    split_authority_default(authority, default_port)
}

fn split_authority_default(authority: &str, default_port: u16) -> Option<(String, u16)> {
    let authority = authority.rsplit('@').next()?;
    if let Some(rest) = authority.strip_prefix('[') {
        let end = rest.find(']')?;
        let host = rest[..end].to_string();
        let port = rest[end + 1..]
            .strip_prefix(':')
            .and_then(|p| p.parse().ok())
            .unwrap_or(default_port);
        return Some((host, port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => match port.parse::<u16>() {
            Ok(port) => Some((host.to_string(), port)),
            Err(_) => None,
        },
        None => Some((authority.to_string(), default_port)),
    }
}

/// The origin-form path+query of an absolute URI.
fn origin_form_of(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let after = &rest[authority.len()..];
    if after.is_empty() {
        Some("/".to_string())
    } else if after.starts_with('/') {
        Some(after.to_string())
    } else {
        Some(format!("/{after}"))
    }
}

/// A policy decision for one (url, resource-type) pair — used by the
/// interception layer and unit tests.
pub fn decide_resource(
    policy: &DestinationPolicy,
    url: &str,
    resource_type: ResourceType,
) -> DestinationDecision {
    if policy.blocks_resource_type(resource_type) {
        return DestinationDecision::Blocked {
            reason: BlockReason::ResourceTypeBlocked,
        };
    }
    policy.decide_url(url)
}

/// Serialize a policy for diagnostics (never credentials: the policy holds
/// none).
pub fn policy_snapshot(policy: &DestinationPolicy) -> serde_json::Value {
    json!({
        "first_party": policy.first_party,
        "blocked_hosts": policy.blocked_hosts,
        "blocked_resource_types": policy.blocked_resource_types,
        "allow_schemes": policy.allow_schemes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> DestinationPolicy {
        DestinationPolicy::first_party_only(vec![
            HostPattern::parse("example.com").unwrap(),
            HostPattern::parse("127.0.0.1").unwrap(),
        ])
    }

    #[test]
    fn host_patterns_match_suffixes_and_reject_traversal() {
        let suffix = HostPattern::parse("*.example.com").unwrap();
        assert!(suffix.matches("example.com"));
        assert!(suffix.matches("a.example.com"));
        assert!(!suffix.matches("notexample.com"));
        assert!(HostPattern::parse("example.com.evil.test")
            .unwrap()
            .matches("example.com.evil.test"));
        assert!(HostPattern::parse("").is_err());
        assert!(HostPattern::parse("bad host").is_err());
        let wildcard = HostPattern::parse("*").unwrap();
        assert!(wildcard.matches("anything.test"));
        assert!(DestinationPolicy::first_party_only(vec![wildcard])
            .validate()
            .is_err());
    }

    #[test]
    fn policy_denies_unknown_hosts_and_deny_wins() {
        let mut policy = policy();
        assert!(policy.decide_host("example.com").is_allowed());
        assert_eq!(
            policy.decide_host("tracker.test"),
            DestinationDecision::Blocked {
                reason: BlockReason::NotFirstParty
            }
        );
        policy.blocked_hosts = vec![HostPattern::parse("*.example.com").unwrap()];
        assert_eq!(
            policy.decide_host("example.com"),
            DestinationDecision::Blocked {
                reason: BlockReason::ExplicitlyBlocked
            }
        );
        assert_eq!(
            policy.decide_url("ftp://example.com/x"),
            DestinationDecision::Blocked {
                reason: BlockReason::SchemeNotAllowed
            }
        );
        assert_eq!(
            policy.decide_url("not a url"),
            DestinationDecision::Blocked {
                reason: BlockReason::Malformed
            }
        );
    }

    #[test]
    fn default_blocked_resource_types_cover_media_images_fonts() {
        let policy =
            DestinationPolicy::first_party_only(vec![HostPattern::parse("x.test").unwrap()]);
        assert!(policy.blocks_resource_type(ResourceType::Image));
        assert!(policy.blocks_resource_type(ResourceType::Media));
        assert!(policy.blocks_resource_type(ResourceType::Font));
        assert!(!policy.blocks_resource_type(ResourceType::Xhr));
        assert!(!policy.blocks_resource_type(ResourceType::Document));
        assert_eq!(
            decide_resource(&policy, "https://x.test/a.png", ResourceType::Image),
            DestinationDecision::Blocked {
                reason: BlockReason::ResourceTypeBlocked
            }
        );
    }

    #[test]
    fn credentials_never_render_their_password() {
        let credentials = ProxyCredentials::new("user", "hunter2-secret");
        let rendered = format!("{credentials:?}");
        assert!(!rendered.contains("hunter2-secret"));
        assert!(rendered.contains("[redacted]"));
        let header = credentials.basic_header();
        assert!(header.starts_with("Basic "));
        assert!(!header.contains("hunter2-secret"));
    }

    #[test]
    fn upstream_selection_prefers_host_routes() {
        let selector = UpstreamSelector::new(Some(UpstreamProxy::new("default.proxy", 8080)))
            .with_route(
                HostPattern::parse("*.cn.test").unwrap(),
                UpstreamProxy::new("cn.proxy", 3128),
            );
        assert_eq!(selector.select("a.cn.test").unwrap().port, 3128);
        assert_eq!(selector.select("other.test").unwrap().port, 8080);
        let empty = UpstreamSelector::default();
        assert!(empty.select("other.test").is_none());
    }

    #[test]
    fn broker_config_refuses_non_loopback_bind() {
        let config = BrokerConfig {
            bind: "0.0.0.0:8080".parse().unwrap(),
            ..BrokerConfig::default()
        };
        assert!(config.validate().is_err());
        assert!(BrokerConfig::default().validate().is_ok());
    }
}
