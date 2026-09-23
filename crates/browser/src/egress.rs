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
use faktor_security::destination::RequestTarget;

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
    /// The host part is canonicalized through the shared `faktor-security`
    /// destination authority (lowercase, trailing-dot stripped, UTS-46
    /// punycode for IDNs, numeric IPv4 canonicalized), so a pattern and a
    /// request spelling of the same destination always compare equal.
    pub fn parse(raw: &str) -> Result<Self, BrowserError> {
        let raw = raw.trim().to_ascii_lowercase();
        if raw.is_empty() {
            return Err(BrowserError::invalid_config("empty host pattern"));
        }
        if raw == "*" {
            return Ok(HostPattern::Suffix(String::new()));
        }
        let (suffix, host) = if let Some(rest) = raw.strip_prefix("*.") {
            (true, rest)
        } else if let Some(rest) = raw.strip_prefix('.') {
            (true, rest)
        } else {
            (false, raw.as_str())
        };
        if host.is_empty() || host.contains('*') || host.contains('/') {
            return Err(BrowserError::invalid_config(format!(
                "invalid host pattern {raw:?}"
            )));
        }
        let target = RequestTarget::parse(host).map_err(|error| {
            BrowserError::invalid_config(format!("invalid host pattern {raw:?}: {error}"))
        })?;
        if target.port.is_some() {
            return Err(BrowserError::invalid_config(format!(
                "host pattern {raw:?} must not carry a port; permitted ports are listed in \
                 allowed_ports"
            )));
        }
        if suffix {
            Ok(HostPattern::Suffix(format!(".{}", target.host)))
        } else {
            Ok(HostPattern::Exact(target.host))
        }
    }

    /// Match a canonical host spelling against this pattern. The argument is
    /// canonicalized through the shared authority first, so punycode,
    /// trailing dots, case and numeric IPv4 spellings cannot evade a rule;
    /// an unparseable host never matches (fail closed).
    pub fn matches(&self, host: &str) -> bool {
        let Some(host) = canonical_host(host) else {
            return false;
        };
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
    /// The destination port is not in the policy's explicit port allowlist.
    PortNotAllowed,
}

impl BlockReason {
    pub fn as_str(self) -> &'static str {
        match self {
            BlockReason::NotFirstParty => "not_first_party",
            BlockReason::ExplicitlyBlocked => "explicitly_blocked",
            BlockReason::SchemeNotAllowed => "scheme_not_allowed",
            BlockReason::Malformed => "malformed",
            BlockReason::ResourceTypeBlocked => "resource_type_blocked",
            BlockReason::PortNotAllowed => "port_not_allowed",
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

/// The documented default port allowlist: the two web ports. Every other
/// port must be listed explicitly by the connector's policy.
pub const DEFAULT_ALLOWED_PORTS: [u16; 2] = [80, 443];

fn default_allowed_ports() -> Vec<u16> {
    DEFAULT_ALLOWED_PORTS.to_vec()
}

/// The per-connector destination policy (spec §9). Default posture: deny
/// everything that is not explicitly first-party; deny every port outside
/// the explicit allowlist (80/443 unless widened); block video/audio/images/
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
    /// The explicit port allowlist. A destination whose real connection port
    /// is not listed is refused with [`BlockReason::PortNotAllowed`] even
    /// when its host is first-party. Defaults to 80/443.
    #[serde(default = "default_allowed_ports")]
    pub allowed_ports: Vec<u16>,
}

impl DestinationPolicy {
    /// The strict default: only the listed first-party hosts, http/https,
    /// the 80/443 port allowlist, with the aggressive default resource-type
    /// drop set.
    pub fn first_party_only(hosts: Vec<HostPattern>) -> Self {
        Self {
            first_party: hosts,
            blocked_hosts: Vec::new(),
            blocked_resource_types: ResourceType::default_blocked(),
            allow_schemes: vec!["http".to_string(), "https".to_string()],
            allowed_ports: default_allowed_ports(),
        }
    }

    /// Add explicit deny patterns (deny always wins).
    pub fn with_blocked_hosts(mut self, hosts: Vec<HostPattern>) -> Self {
        self.blocked_hosts = hosts;
        self
    }

    /// Set the explicit port allowlist (replaces the 80/443 default).
    pub fn with_allowed_ports(mut self, ports: Vec<u16>) -> Self {
        self.allowed_ports = ports;
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
        if self.allowed_ports.is_empty() {
            return Err(BrowserError::invalid_config(
                "destination policy must allow at least one port (default: 80, 443)",
            ));
        }
        if self.allowed_ports.contains(&0) {
            return Err(BrowserError::invalid_config(
                "port 0 is not a valid destination port",
            ));
        }
        Ok(())
    }

    /// Decide a bare host (no port), for host-level diagnostics. The wire
    /// path always uses [`DestinationPolicy::decide_destination`] with the
    /// real port.
    pub fn decide_host(&self, host: &str) -> DestinationDecision {
        let Some(canonical) = canonical_host(host) else {
            return DestinationDecision::Blocked {
                reason: BlockReason::Malformed,
            };
        };
        self.decide_canonical(&canonical, None)
    }

    fn decide_canonical(&self, host: &str, port: Option<u16>) -> DestinationDecision {
        if self.blocked_hosts.iter().any(|p| p.matches(host)) {
            return DestinationDecision::Blocked {
                reason: BlockReason::ExplicitlyBlocked,
            };
        }
        if !self.first_party.iter().any(|p| p.matches(host)) {
            return DestinationDecision::Blocked {
                reason: BlockReason::NotFirstParty,
            };
        }
        if let Some(port) = port {
            if !self.allowed_ports.contains(&port) {
                return DestinationDecision::Blocked {
                    reason: BlockReason::PortNotAllowed,
                };
            }
        }
        DestinationDecision::Allowed
    }

    /// Decide one destination: host and real connection port. The host is
    /// canonicalized through the shared `faktor-security` authority
    /// (lowercase, trailing dot, UTS-46 punycode, canonical numeric IPv4/
    /// IPv6) so alternate spellings cannot bypass a rule. Deny wins; an
    /// unknown host or an unlisted port is refused.
    ///
    /// Residual (honest): DNS-rebinding IP pinning is **not** implemented.
    /// The policy decides the canonical *name*; name resolution and connect
    /// happen afterwards, so a name that resolves to an unlisted address is
    /// not re-checked against an IP allowlist.
    pub fn decide_destination(&self, host: &str, port: u16) -> DestinationDecision {
        let Some(canonical) = canonical_host(host) else {
            return DestinationDecision::Blocked {
                reason: BlockReason::Malformed,
            };
        };
        self.decide_canonical(&canonical, Some(port))
    }

    /// Decide a full URL (scheme + host + port). The port is the URL's real
    /// connection port (explicit, else the scheme default), so an allowed
    /// host on a non-allowlisted port is refused.
    pub fn decide_url(&self, url: &str) -> DestinationDecision {
        let Some((scheme, authority)) = split_absolute_target(url) else {
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
        let destination = format!("{scheme}://{authority}");
        let Ok(target) = RequestTarget::parse(&destination) else {
            return DestinationDecision::Blocked {
                reason: BlockReason::Malformed,
            };
        };
        let Some(port) = target.port else {
            return DestinationDecision::Blocked {
                reason: BlockReason::Malformed,
            };
        };
        self.decide_canonical(&target.host, Some(port))
    }

    /// Is this resource type dropped by default interception?
    pub fn blocks_resource_type(&self, resource_type: ResourceType) -> bool {
        self.blocked_resource_types.contains(&resource_type)
    }
}

/// Split `scheme://authority/...` into (scheme, authority): path, query and
/// fragment are dropped, userinfo (hostile input) is stripped. The authority
/// is returned verbatim; canonicalization is the shared authority's job.
pub(crate) fn split_absolute_target(url: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty() {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit('@').next()?; // strip userinfo if hostile
    if authority.is_empty() {
        return None;
    }
    Some((scheme, authority))
}

/// Parse a CONNECT authority (`host:port`, `[v6]:port`) into a canonical
/// destination. The port defaults to 443. A URL-shaped or unparseable target
/// is refused (fail closed).
pub(crate) fn parse_connect_target(target: &str) -> Option<(String, u16)> {
    let target = target.rsplit('@').next()?;
    let parsed = RequestTarget::parse(target).ok()?;
    if !parsed.scheme.is_empty() {
        return None;
    }
    Some((parsed.host, parsed.port.unwrap_or(443)))
}

/// Canonicalize one bare destination host through the shared
/// `faktor-security` authority. A canonical unbracketed IPv6 literal
/// (what URL parsers produce) is canonicalized directly; a bracketed
/// literal, name, IDN or numeric IPv4 goes through the shared parser.
fn canonical_host(host: &str) -> Option<String> {
    let host = host.trim();
    if host.is_empty() {
        return None;
    }
    if host.contains(':') && !host.starts_with('[') {
        return host
            .parse::<std::net::Ipv6Addr>()
            .ok()
            .map(|address| address.to_string());
    }
    let target = RequestTarget::parse(host).ok()?;
    if target.port.is_some() {
        return None;
    }
    Some(target.host)
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
    /// Idle timeout for one direction of a tunneled/forwarded copy: a peer
    /// that stalls for this long loses its connection (and its
    /// `max_connections` permit).
    pub copy_idle_timeout_ms: u64,
    /// Total lifetime of one tunneled/forwarded copy.
    pub copy_max_ms: u64,
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
            copy_idle_timeout_ms: 30_000,
            copy_max_ms: 600_000,
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
        if self.connect_timeout_ms == 0 || self.copy_idle_timeout_ms == 0 || self.copy_max_ms == 0 {
            return Err(BrowserError::invalid_config("broker timeouts must be > 0"));
        }
        if self.copy_max_ms < self.copy_idle_timeout_ms {
            return Err(BrowserError::invalid_config(
                "broker copy_max_ms must be >= copy_idle_timeout_ms",
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
    copy_idle_timeout: Duration,
    copy_max: Duration,
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
            copy_idle_timeout: Duration::from_millis(config.copy_idle_timeout_ms),
            copy_max: Duration::from_millis(config.copy_max_ms),
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
    /// The head is malformed (strict grammar violation). Refused with a
    /// typed 400, never forwarded or re-emitted.
    Malformed(String),
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

/// RFC 7230 token characters.
fn is_token_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

fn is_token(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(is_token_char)
}

/// Strict header value grammar: visible ASCII plus SP/HTAB only. Every CTL
/// (CR, LF, NUL, DEL) is refused, so no line can be smuggled through a
/// header value.
fn is_header_value(text: &str) -> bool {
    text.bytes()
        .all(|b| b == b'\t' || (0x20..=0x7e).contains(&b))
}

/// The standard hop-by-hop set (RFC 7230 §6.1 plus the proxy header). A
/// header listed here is never forwarded, whatever its case.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

fn is_hop_by_hop(name: &str, connection_tokens: &[String]) -> bool {
    let lower = name.to_ascii_lowercase();
    HOP_BY_HOP.contains(&lower.as_str()) || connection_tokens.iter().any(|token| token == &lower)
}

/// Collect the tokens listed by every `Connection` header, so
/// `Connection: x-forwarded-for`-style lists strip the named headers too.
/// A malformed token list is refused (never silently ignored).
fn connection_tokens(headers: &[(String, String)]) -> Result<Vec<String>, HeadError> {
    let mut tokens = Vec::new();
    for (name, value) in headers {
        if !name.eq_ignore_ascii_case("connection") {
            continue;
        }
        for token in value.split(',') {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            if !is_token(token) {
                return Err(HeadError::Malformed(format!(
                    "malformed Connection token {token:?}"
                )));
            }
            tokens.push(token.to_ascii_lowercase());
        }
    }
    Ok(tokens)
}

/// Strict request-head grammar:
///
/// * request line: `METHOD SP target SP HTTP/1.x` with token method and a
///   control-free target;
/// * header lines: `token ":" value`, no obs-fold, no bare `\n` or any other
///   CTL inside a line, no empty names;
/// * anything else is a typed [`HeadError::Malformed`] (refused with 400).
fn parse_head(head: &str) -> Result<ParsedRequest, HeadError> {
    let mut lines = head.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| HeadError::Malformed("empty request".to_string()))?;
    if !is_header_value(request_line) || request_line.contains('\t') {
        return Err(HeadError::Malformed(
            "request line carries control characters".to_string(),
        ));
    }
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let version = parts.next().unwrap_or_default();
    if parts.next().is_some() {
        return Err(HeadError::Malformed(
            "request line has too many fields".to_string(),
        ));
    }
    if !is_token(method) {
        return Err(HeadError::Malformed(format!("invalid method {method:?}")));
    }
    if target.is_empty() || target.bytes().any(|b| b <= 0x20 || b == 0x7f) {
        return Err(HeadError::Malformed(format!(
            "invalid request target {target:?}"
        )));
    }
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1") {
        return Err(HeadError::Malformed(format!(
            "unsupported http version {version:?}"
        )));
    }
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(HeadError::Malformed(format!(
                "malformed header line {line:?}"
            )));
        };
        if !is_token(name) {
            return Err(HeadError::Malformed(format!(
                "invalid header name {name:?}"
            )));
        }
        let value = value.trim_matches(|c| c == ' ' || c == '\t');
        if !is_header_value(value) {
            return Err(HeadError::Malformed(format!(
                "header {name:?} carries control characters"
            )));
        }
        headers.push((name.to_string(), value.to_string()));
    }
    Ok(ParsedRequest {
        method: method.to_string(),
        target: target.to_string(),
        headers,
    })
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
        Err(HeadError::Malformed(detail)) => {
            return Err(format!("head read reported malformed input: {detail}"))
        }
    };
    let request = match parse_head(&head) {
        Ok(request) => request,
        Err(HeadError::Malformed(detail)) => {
            inner.account_blocked();
            tracing::info!(detail = %detail, "egress: malformed request head refused");
            write_denial(&mut client, "400 Bad Request", BlockReason::Malformed).await;
            return Ok(());
        }
        Err(other) => return Err(format!("head parse failed: {other:?}")),
    };

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
    let Some((host, port)) = parse_connect_target(&request.target) else {
        inner.account_blocked();
        write_denial(&mut client, "400 Bad Request", BlockReason::Malformed).await;
        return Ok(());
    };
    inner.account_request(&host);
    let decision = inner.policy.decide_destination(&host, port);
    if let DestinationDecision::Blocked { reason } = &decision {
        inner.account_blocked();
        tracing::info!(host = %host, port, reason = %reason, "egress: CONNECT blocked by policy");
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
    let (up, down) = copy_bidirectional_bounded(
        &mut client,
        &mut target,
        inner.copy_idle_timeout,
        inner.copy_max,
    )
    .await
    .map_err(|detail| format!("tunnel copy failed: {detail}"))?;
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
    let Some((scheme, authority)) = split_absolute_target(&request.target) else {
        inner.account_blocked();
        write_denial(&mut client, "400 Bad Request", BlockReason::Malformed).await;
        return Ok(());
    };
    let scheme = scheme.to_ascii_lowercase();
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
    let destination = format!("{scheme}://{authority}");
    let Ok(parsed) = RequestTarget::parse(&destination) else {
        inner.account_blocked();
        write_denial(&mut client, "400 Bad Request", BlockReason::Malformed).await;
        return Ok(());
    };
    let Some(port) = parsed.port else {
        inner.account_blocked();
        write_denial(&mut client, "400 Bad Request", BlockReason::Malformed).await;
        return Ok(());
    };
    let host = parsed.host;
    inner.account_request(&host);
    let decision = inner.policy.decide_destination(&host, port);
    if let DestinationDecision::Blocked { reason } = &decision {
        inner.account_blocked();
        tracing::info!(host = %host, port, reason = %reason, "egress: request blocked by policy");
        write_denial(&mut client, "403 Forbidden", *reason).await;
        return Ok(());
    }
    let origin_form = origin_form_of(&request.target).unwrap_or_else(|| "/".to_string());
    // Rebuild the head: hop-by-hop headers — the fixed set AND every token
    // listed by `Connection` — plus any client-supplied proxy credentials
    // are stripped; the upstream credential (if any) is added on this leg
    // only. A malformed Connection list is refused before any destination
    // socket is opened.
    let connection_listed = match connection_tokens(&request.headers) {
        Ok(tokens) => tokens,
        Err(_) => {
            inner.account_blocked();
            tracing::info!("egress: malformed Connection header refused");
            write_denial(&mut client, "400 Bad Request", BlockReason::Malformed).await;
            return Ok(());
        }
    };
    let upstream = inner.upstream.select(&host);
    let mut target = match connect_destination(upstream, &host, port, &inner).await {
        Ok(stream) => stream,
        Err(detail) => {
            inner.account_error(detail.clone());
            write_error(&mut client, "502 Bad Gateway", &detail).await;
            return Ok(());
        }
    };
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
        if is_hop_by_hop(name, &connection_listed) {
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
    let (up, down) = copy_bidirectional_bounded(
        &mut client,
        &mut target,
        inner.copy_idle_timeout,
        inner.copy_max,
    )
    .await
    .map_err(|detail| format!("forward copy failed: {detail}"))?;
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

/// Bounded bidirectional copy: one idle timeout per read/write and a total
/// copy deadline. Both directions half-close on EOF. On timeout or error the
/// sockets are left for the caller to drop (the typed reason is returned),
/// so a stalled peer can never pin a `max_connections` permit indefinitely.
async fn copy_bidirectional_bounded(
    client: &mut TcpStream,
    target: &mut TcpStream,
    idle: Duration,
    total: Duration,
) -> Result<(u64, u64), String> {
    let deadline = tokio::time::Instant::now() + total;
    let mut client_open = true;
    let mut target_open = true;
    let mut up = 0u64;
    let mut down = 0u64;
    let mut client_buf = vec![0u8; 16 * 1024];
    let mut target_buf = vec![0u8; 16 * 1024];
    while client_open || target_open {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(format!("copy deadline of {}ms exceeded", total.as_millis()));
        }
        let wait = idle.min(deadline - now);
        tokio::select! {
            biased;
            read = tokio::time::timeout(wait, client.read(&mut client_buf)), if client_open => {
                match read {
                    Err(_) => return Err("idle timeout on the client leg".to_string()),
                    Ok(Err(e)) => return Err(format!("client read failed: {e}")),
                    Ok(Ok(0)) => {
                        client_open = false;
                        let _ = target.shutdown().await;
                    }
                    Ok(Ok(n)) => {
                        match tokio::time::timeout(wait, target.write_all(&client_buf[..n])).await {
                            Err(_) => return Err("idle timeout writing to the target leg".to_string()),
                            Ok(Err(e)) => return Err(format!("target write failed: {e}")),
                            Ok(Ok(())) => up = up.saturating_add(n as u64),
                        }
                    }
                }
            }
            read = tokio::time::timeout(wait, target.read(&mut target_buf)), if target_open => {
                match read {
                    Err(_) => return Err("idle timeout on the target leg".to_string()),
                    Ok(Err(e)) => return Err(format!("target read failed: {e}")),
                    Ok(Ok(0)) => {
                        target_open = false;
                        let _ = client.shutdown().await;
                    }
                    Ok(Ok(n)) => {
                        match tokio::time::timeout(wait, client.write_all(&target_buf[..n])).await {
                            Err(_) => return Err("idle timeout writing to the client leg".to_string()),
                            Ok(Err(e)) => return Err(format!("client write failed: {e}")),
                            Ok(Ok(())) => down = down.saturating_add(n as u64),
                        }
                    }
                }
            }
        }
    }
    Ok((up, down))
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
        "allowed_ports": policy.allowed_ports,
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
        // Copy bounds are validated too.
        let bad = BrokerConfig {
            copy_idle_timeout_ms: 0,
            ..BrokerConfig::default()
        };
        assert!(bad.validate().is_err());
        let bad = BrokerConfig {
            copy_max_ms: BrokerConfig::default().copy_idle_timeout_ms - 1,
            ..BrokerConfig::default()
        };
        assert!(bad.validate().is_err());
    }

    #[test]
    fn destination_ports_are_policy_checked() {
        let policy = policy();
        assert!(policy.decide_destination("example.com", 443).is_allowed());
        assert!(policy.decide_destination("example.com", 80).is_allowed());
        assert!(policy.decide_url("https://example.com/x").is_allowed());
        assert!(policy.decide_url("http://example.com/x").is_allowed());
        // An allowlisted host on a non-policy port is refused.
        assert_eq!(
            policy.decide_destination("example.com", 8443),
            DestinationDecision::Blocked {
                reason: BlockReason::PortNotAllowed
            }
        );
        assert_eq!(
            policy.decide_url("https://example.com:8443/x"),
            DestinationDecision::Blocked {
                reason: BlockReason::PortNotAllowed
            }
        );
        // An explicit port allowlist is honored.
        let widened = policy.clone().with_allowed_ports(vec![80, 443, 8443]);
        assert!(widened.decide_destination("example.com", 8443).is_allowed());
        assert_eq!(
            widened.decide_destination("example.com", 8080),
            DestinationDecision::Blocked {
                reason: BlockReason::PortNotAllowed
            }
        );
        // A widened port never widens the host set.
        assert_eq!(
            widened.decide_destination("tracker.test", 8443),
            DestinationDecision::Blocked {
                reason: BlockReason::NotFirstParty
            }
        );
        // An empty port allowlist is a config error, not a silent deny-all.
        let mut empty = policy.clone();
        empty.allowed_ports = Vec::new();
        assert!(empty.validate().is_err());
    }

    #[test]
    fn canonicalization_blocks_spelling_tricks() {
        let policy = DestinationPolicy::first_party_only(vec![
            HostPattern::parse("example.com").unwrap(),
            HostPattern::parse("127.0.0.1").unwrap(),
            HostPattern::parse("bücher.example").unwrap(),
            HostPattern::parse("[::1]").unwrap(),
        ]);
        // Trailing dot and case.
        assert!(policy.decide_url("https://example.com./x").is_allowed());
        assert!(policy.decide_destination("EXAMPLE.COM.", 443).is_allowed());
        // UTS-46 punycode both directions.
        let puny = HostPattern::parse("xn--bcher-kva.example").unwrap();
        assert!(puny.matches("bücher.example"));
        assert!(HostPattern::parse("bücher.example")
            .unwrap()
            .matches("xn--bcher-kva.example"));
        assert!(policy
            .decide_destination("xn--bcher-kva.example", 443)
            .is_allowed());
        assert!(policy.decide_url("https://BÜCHER.example/").is_allowed());
        // Numeric IPv4 alternates canonicalize to the allowlisted literal.
        assert!(policy
            .decide_destination("127.000.000.001", 80)
            .is_allowed());
        assert!(policy.decide_url("http://127.000.000.001/").is_allowed());
        // Decimal IP shorthand and unbracketed IPv6 are ambiguous: refused.
        assert_eq!(
            policy.decide_destination("2130706433", 80),
            DestinationDecision::Blocked {
                reason: BlockReason::Malformed
            }
        );
        assert_eq!(
            policy.decide_url("http://2130706433/"),
            DestinationDecision::Blocked {
                reason: BlockReason::Malformed
            }
        );
        assert_eq!(
            policy.decide_url("http://::1/"),
            DestinationDecision::Blocked {
                reason: BlockReason::Malformed
            }
        );
        // A zero-padded IPv6 literal canonicalizes and matches.
        assert!(policy
            .decide_url("http://[0:0:0:0:0:0:0:1]:80/")
            .is_allowed());
        // Patterns carry no ports: ports live in `allowed_ports`.
        assert!(HostPattern::parse("example.com:8443").is_err());
        // A hostile userinfo never becomes the host.
        assert!(policy
            .decide_url("https://evil.test@example.com/x")
            .is_allowed());
        assert_eq!(
            policy.decide_url("https://example.com@evil.test/x"),
            DestinationDecision::Blocked {
                reason: BlockReason::NotFirstParty
            }
        );
    }

    #[test]
    fn request_head_grammar_refuses_smuggling() {
        assert!(parse_head("GET http://x.test/ HTTP/1.1\r\nHost: x.test\r\n\r\n").is_ok());
        assert!(parse_head("CONNECT x.test:443 HTTP/1.0\r\nHost: x.test\r\n\r\n").is_ok());
        // A bare \n inside a value must not survive as a new header line.
        let smuggled = "GET http://x.test/ HTTP/1.1\r\nHost: x.test\r\n\
                        X-Ignored: a\nProxy-Authorization: Basic ZQ==\r\n\r\n";
        assert!(matches!(parse_head(smuggled), Err(HeadError::Malformed(_))));
        // Empty header name.
        assert!(parse_head("GET http://x.test/ HTTP/1.1\r\n: v\r\n\r\n").is_err());
        // Obsolete line folding.
        assert!(
            parse_head("GET http://x.test/ HTTP/1.1\r\nX: v\r\n  folded\r\n\r\n").is_err(),
            "obs-fold must be refused"
        );
        // Control characters in the request line.
        assert!(parse_head("GET http://x.test/\nHTTP/1.1\r\n\r\n").is_err());
        // Non-token method/target/version.
        assert!(parse_head("G ET http://x.test/ HTTP/1.1\r\n\r\n").is_err());
        assert!(parse_head("GET http://x.test/ HTTP/2\r\n\r\n").is_err());
        assert!(parse_head("GET  HTTP/1.1\r\n\r\n").is_err());
        // DEL in a value.
        assert!(parse_head("GET http://x.test/ HTTP/1.1\r\nX: a\u{7f}b\r\n\r\n").is_err());
    }

    #[test]
    fn connection_listed_tokens_are_hop_by_hop() {
        let headers = vec![(
            "connection".to_string(),
            "Keep-Alive, X-Custom-Hop".to_string(),
        )];
        let tokens = connection_tokens(&headers).unwrap();
        assert!(is_hop_by_hop("Connection", &tokens));
        assert!(is_hop_by_hop("x-custom-hop", &tokens));
        assert!(is_hop_by_hop("Proxy-Authorization", &tokens));
        assert!(is_hop_by_hop("Transfer-Encoding", &tokens));
        assert!(!is_hop_by_hop("Content-Type", &tokens));
        // A malformed token list is refused, never guessed at.
        assert!(connection_tokens(&[("connection".to_string(), "bad token".to_string())]).is_err());
    }
}
