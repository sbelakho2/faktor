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
//! * **Request framing** — the forward path frames exactly one HTTP/1
//!   request body per connection (fixed-length or chunked), refuses
//!   ambiguous or conflicting framing (CL+TE, unequal duplicate CL,
//!   unsupported codings, pipeline residue), and re-emits one canonical
//!   `Content-Length` so no framing metadata can desync an upstream.
//! * **Owned teardown** — every connection is owned by the accept loop's
//!   `JoinSet`; `shutdown().await` returns only once all broker-owned
//!   sockets and authorized tunnels are closed (`BrokerState::Stopped`
//!   is never reported with live connections).
//!
//! The broker is not an anti-bot subsystem and does not inspect payloads
//! beyond the request head and framing it must route.

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;
use tokio::task::JoinSet;

use faktor_core::cancellation::CancellationToken;
use faktor_security::destination::RequestTarget;
use faktor_security::secret::SecretValue;
use zeroize::Zeroizing;

/// The address-class rule for one egress leg, re-exported from the single
/// [`faktor_security::network`] authority (never defined locally).
pub use faktor_security::network::EgressAddressPolicy;

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
    /// The policy does not permit CONNECT tunnels at all (no HTTPS/WSS
    /// support, or an explicit denial).
    ConnectNotAllowed,
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
            BlockReason::ConnectNotAllowed => "connect_not_allowed",
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

/// Derive the CONNECT dimension from scheme support: a tunnel is only
/// meaningful when HTTPS or WSS is permitted. The derived value is stored
/// explicitly in the policy (never re-derived at decision time).
fn schemes_allow_connect(schemes: &[String]) -> bool {
    schemes
        .iter()
        .any(|scheme| scheme.eq_ignore_ascii_case("https") || scheme.eq_ignore_ascii_case("wss"))
}

/// Which wire shape one proxied request takes. The wire paths decide with
/// [`DestinationPolicy::decide_proxy_target`], so the request kind is part of
/// the policy decision and never bypassed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProxyRequestKind {
    /// One absolute-form HTTP request forwarded by the broker.
    ForwardHttp,
    /// A CONNECT tunnel (the wire shape HTTPS/WSS uses through a proxy).
    ConnectTunnel,
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
    /// Whether CONNECT tunnels are permitted at all. This is a first-class
    /// policy dimension (a CONNECT to `example.com:443` is not an HTTP
    /// request to that host): a policy that only supports HTTP must deny it.
    /// Constructors derive it from HTTPS/WSS support and the explicit result
    /// is persisted here; a deserialized policy that omits the field denies
    /// CONNECT (fail closed).
    #[serde(default)]
    pub allow_connect: bool,
    /// The explicit loopback address-class rule for this connector: the
    /// broker resolves every destination hostname exactly ONCE, classifies
    /// every answer and refuses the whole set when any address is outside
    /// the permitted classes (global-only by default). `true` opts into
    /// loopback (a local first-party service); every other special class —
    /// private, link-local (cloud metadata), CGNAT, documentation,
    /// multicast, unspecified, reserved — stays refused either way. Default
    /// `false`: a remote first-party host resolving onto loopback is a
    /// DNS-rebinding signal.
    #[serde(default)]
    pub allow_loopback: bool,
}

impl DestinationPolicy {
    /// The strict default: only the listed first-party hosts, http/https,
    /// the 80/443 port allowlist, with the aggressive default resource-type
    /// drop set. CONNECT is derived from the https scheme support and
    /// persisted as an explicit `allow_connect: true`.
    pub fn first_party_only(hosts: Vec<HostPattern>) -> Self {
        let allow_schemes = vec!["http".to_string(), "https".to_string()];
        Self {
            first_party: hosts,
            blocked_hosts: Vec::new(),
            blocked_resource_types: ResourceType::default_blocked(),
            allow_connect: schemes_allow_connect(&allow_schemes),
            allow_loopback: false,
            allow_schemes,
            allowed_ports: default_allowed_ports(),
        }
    }

    /// The explicit loopback address-class rule (wire config): when set,
    /// the broker's connect vetting admits loopback answers for this
    /// connector. Default false — a remote first-party host resolving onto
    /// loopback is refused.
    pub fn with_allow_loopback(mut self, allow: bool) -> Self {
        self.allow_loopback = allow;
        self
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

    /// Replace the permitted schemes, deriving `allow_connect` from
    /// HTTPS/WSS support (the derived result is stored explicitly).
    pub fn with_allow_schemes(mut self, schemes: Vec<String>) -> Self {
        self.allow_connect = schemes_allow_connect(&schemes);
        self.allow_schemes = schemes;
        self
    }

    /// Set the CONNECT dimension explicitly. [`DestinationPolicy::validate`]
    /// refuses a policy that allows CONNECT without HTTPS/WSS support.
    pub fn with_allow_connect(mut self, allow: bool) -> Self {
        self.allow_connect = allow;
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
        if self.allow_connect && !schemes_allow_connect(&self.allow_schemes) {
            return Err(BrowserError::invalid_config(
                "destination policy allows CONNECT but permits no HTTPS/WSS scheme; \
                 CONNECT is only meaningful for TLS destinations",
            ));
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
    /// path always uses [`DestinationPolicy::decide_proxy_target`] with the
    /// real port and request kind.
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
    /// DNS-rebinding closure: the broker resolves the permitted hostname
    /// exactly once at connect time ([`connect_destination`]), bounds the
    /// answer set, classifies EVERY address — literal or resolved — through
    /// the single [`faktor_security::network`] authority and refuses the
    /// whole set when any answer is outside the permitted classes. A stale
    /// "resolved by name later" bypass no longer exists: the connect uses
    /// the vetted [`std::net::SocketAddr`] list only.
    pub fn decide_destination(&self, host: &str, port: u16) -> DestinationDecision {
        let Some(canonical) = canonical_host(host) else {
            return DestinationDecision::Blocked {
                reason: BlockReason::Malformed,
            };
        };
        self.decide_canonical(&canonical, Some(port))
    }

    /// The single decision function for the broker's wire paths. Both the
    /// forward path and the CONNECT tunnel path call this, so the request
    /// kind (a first-class policy dimension) can never be bypassed: a policy
    /// that does not allow CONNECT refuses `ConnectTunnel` even when the
    /// host and port are first-party.
    pub fn decide_proxy_target(
        &self,
        kind: ProxyRequestKind,
        host: &str,
        port: u16,
    ) -> DestinationDecision {
        let Some(canonical) = canonical_host(host) else {
            return DestinationDecision::Blocked {
                reason: BlockReason::Malformed,
            };
        };
        if kind == ProxyRequestKind::ConnectTunnel && !self.allow_connect {
            return DestinationDecision::Blocked {
                reason: BlockReason::ConnectNotAllowed,
            };
        }
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
/// fragment are dropped. An authority carrying userinfo (`user@host`) is
/// rejected outright — a hostile ambiguous target is never transformed into
/// a valid one. The authority is returned verbatim; canonicalization is the
/// shared authority's job.
pub(crate) fn split_absolute_target(url: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty() {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    Some((scheme, authority))
}

/// Parse a CONNECT authority (`host:port`, `[v6]:port`) into a canonical
/// destination. The port defaults to 443. A URL-shaped, userinfo-carrying or
/// unparseable target is refused (fail closed); userinfo is never stripped
/// into a valid destination.
pub(crate) fn parse_connect_target(target: &str) -> Option<(String, u16)> {
    if target.contains('@') {
        return None;
    }
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

/// Hard bound on one upstream-proxy username, in bytes.
pub const MAX_PROXY_USERNAME_BYTES: usize = 1024;

/// Hard bound on one upstream-proxy password, in bytes.
pub const MAX_PROXY_PASSWORD_BYTES: usize = 4096;

/// Refuse credential text that could inject header structure or overshoot
/// the bound, before any credential value exists.
fn validate_credential_field(name: &str, value: &str, limit: usize) -> Result<(), BrowserError> {
    if value.len() > limit {
        return Err(BrowserError::invalid_config(format!(
            "{name} exceeds the {limit}-byte bound"
        )));
    }
    if value
        .bytes()
        .any(|byte| byte == b'\r' || byte == b'\n' || byte == 0)
    {
        return Err(BrowserError::invalid_config(format!(
            "{name} must not contain CR, LF or NUL"
        )));
    }
    Ok(())
}

/// Upstream proxy credentials. The password is a zeroizing
/// [`SecretValue`]; Debug redacts BOTH fields (a username can itself carry
/// a token), there is no Display, no Serialize, and no accessor for the
/// password outside the broker's own `Proxy-Authorization` writer.
#[derive(Clone, PartialEq, Eq)]
pub struct ProxyCredentials {
    username: String,
    password: SecretValue,
}

impl ProxyCredentials {
    /// Bounded construction: enforces the byte bounds and rejects CR/LF/NUL
    /// header-injection material in either field.
    pub fn try_new(
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> Result<Self, BrowserError> {
        let username = username.into();
        let password = password.into();
        validate_credential_field("proxy username", &username, MAX_PROXY_USERNAME_BYTES)?;
        validate_credential_field("proxy password", &password, MAX_PROXY_PASSWORD_BYTES)?;
        Ok(Self {
            username,
            password: SecretValue::new(password),
        })
    }

    /// Panicking convenience for tests and compile-time-known configuration;
    /// fallible configuration must use [`ProxyCredentials::try_new`].
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self::try_new(username, password).expect("proxy credentials within bounds")
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    /// The Basic `Proxy-Authorization` value, built entirely from zeroizing
    /// temporaries: the `username:password` material and the base64 encoding
    /// are wiped when they drop. Only ever written to the broker→upstream
    /// socket, straight from this buffer (never copied into an ordinary
    /// request-head `String`).
    fn basic_header(&self) -> Zeroizing<String> {
        use base64::Engine as _;
        let mut raw = Zeroizing::new(String::with_capacity(
            self.username.len() + 1 + self.password.len(),
        ));
        raw.push_str(&self.username);
        raw.push(':');
        raw.push_str(self.password.expose());
        let encoded =
            Zeroizing::new(base64::engine::general_purpose::STANDARD.encode(raw.as_bytes()));
        Zeroizing::new(format!("Basic {}", encoded.as_str()))
    }
}

impl fmt::Debug for ProxyCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProxyCredentials")
            .field("username", &"[redacted]")
            .field("password", &"[redacted]")
            .finish()
    }
}

/// One upstream HTTP proxy. The host is stored CANONICALIZED through the
/// shared [`RequestTarget`] authority (lowercase, UTS-46 punycode, one
/// trailing dot stripped, canonical IP literal text) and carries no scheme,
/// userinfo or embedded port. The address policy applied to the broker→proxy
/// leg is the proxy's OWN rule — default [`EgressAddressPolicy::EXTERNAL`] —
/// never inherited from the destination policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamProxy {
    pub host: String,
    pub port: u16,
    pub credentials: Option<ProxyCredentials>,
    pub address_policy: EgressAddressPolicy,
}

impl UpstreamProxy {
    /// Fallible construction: canonicalizes `host` through the shared
    /// authority and refuses hostile/ambiguous forms — userinfo, embedded
    /// port, scheme, whitespace/control characters, malformed IDN,
    /// zone-scoped IPv6 and port 0.
    pub fn try_new(host: impl Into<String>, port: u16) -> Result<Self, BrowserError> {
        let host = host.into();
        if port == 0 {
            return Err(BrowserError::invalid_config(
                "upstream proxy port must not be 0",
            ));
        }
        if host.is_empty() {
            return Err(BrowserError::invalid_config(
                "upstream proxy host must not be empty",
            ));
        }
        if host.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(BrowserError::invalid_config(
                "upstream proxy host must not contain whitespace or control characters",
            ));
        }
        let target = RequestTarget::parse(&host).map_err(|error| {
            BrowserError::invalid_config(format!("invalid upstream proxy host {host:?}: {error}"))
        })?;
        if !target.scheme.is_empty() {
            return Err(BrowserError::invalid_config(
                "upstream proxy host must not carry a scheme",
            ));
        }
        if target.port.is_some() {
            return Err(BrowserError::invalid_config(
                "upstream proxy host must not carry an embedded port; pass the port separately",
            ));
        }
        Ok(Self {
            host: target.host,
            port,
            credentials: None,
            address_policy: EgressAddressPolicy::EXTERNAL,
        })
    }

    /// Panicking convenience for tests and compile-time-known configuration;
    /// fallible configuration must use [`UpstreamProxy::try_new`].
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self::try_new(host, port).expect("valid upstream proxy host")
    }

    pub fn with_credentials(mut self, credentials: ProxyCredentials) -> Self {
        self.credentials = Some(credentials);
        self
    }

    /// The explicit local rule for THIS proxy leg. Only an operator naming a
    /// local proxy may set it; it never widens the destination policy.
    pub fn with_address_policy(mut self, policy: EgressAddressPolicy) -> Self {
        self.address_policy = policy;
        self
    }

    fn validate(&self) -> Result<(), BrowserError> {
        // `host` is a public field: re-run the canonical gate so a hand-built
        // struct cannot smuggle an unvetted host into the connect path.
        let canonical = UpstreamProxy::try_new(self.host.clone(), self.port)?;
        if canonical.host != self.host {
            return Err(BrowserError::invalid_config(format!(
                "upstream proxy host {:?} is not canonical ({:?})",
                self.host, canonical.host
            )));
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
    /// Request-head byte cap (headers only; bodies are framed and bounded
    /// separately by [`BrokerConfig::max_request_body_bytes`]).
    pub max_request_bytes: usize,
    /// Maximum decoded request body the forward path admits. A fixed-length
    /// or chunked body over this bound is refused with a typed 413 before
    /// any destination socket is opened.
    pub max_request_body_bytes: usize,
    /// Concurrent connection ceiling.
    pub max_connections: usize,
    /// TCP connect timeout to destinations/upstreams.
    pub connect_timeout_ms: u64,
    /// Idle timeout for one direction of a tunneled/forwarded copy (and for
    /// one request-body read/write): a peer that stalls for this long loses
    /// its connection (and its `max_connections` permit).
    pub copy_idle_timeout_ms: u64,
    /// Total lifetime of one tunneled/forwarded copy (and of one request
    /// body read).
    pub copy_max_ms: u64,
    /// Bounded grace the broker gives its live connection tasks after
    /// shutdown is requested, before aborting and reaping stragglers.
    pub shutdown_grace_ms: u64,
}

impl Default for BrokerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:0".parse().expect("loopback address literal"),
            policy: DestinationPolicy::first_party_only(Vec::new()),
            upstream: UpstreamSelector::default(),
            max_request_bytes: 32 * 1024,
            max_request_body_bytes: 8 * 1024 * 1024,
            max_connections: 64,
            connect_timeout_ms: 10_000,
            copy_idle_timeout_ms: 30_000,
            copy_max_ms: 600_000,
            shutdown_grace_ms: 2_000,
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
        if self.max_request_bytes == 0
            || self.max_request_body_bytes == 0
            || self.max_connections == 0
        {
            return Err(BrowserError::invalid_config(
                "broker request/body/connection bounds must be > 0",
            ));
        }
        if self.connect_timeout_ms == 0
            || self.copy_idle_timeout_ms == 0
            || self.copy_max_ms == 0
            || self.shutdown_grace_ms == 0
        {
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

/// The broker lifecycle state. Terminal states are final: `Stopped` is only
/// ever set by the accept loop after every broker-owned connection task has
/// ended, so health can never report `Stopped` with live connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokerState {
    /// Accepting connections and serving them.
    Running,
    /// Shutdown requested: the listener is closing and live connection
    /// tasks are ending. New connections are refused.
    Draining,
    /// Clean terminal state: every owned connection task is joined and every
    /// broker-owned socket/tunnel is closed (`active_connections == 0`).
    Stopped,
    /// Terminal state without a completed graceful drain (the owner dropped
    /// or aborted the broker): connection tasks were aborted.
    Failed,
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
    max_request_body_bytes: usize,
    connect_timeout: Duration,
    copy_idle_timeout: Duration,
    copy_max: Duration,
    shutdown_grace: Duration,
    accounting: Mutex<AccountingCounters>,
    active: AtomicUsize,
    started_ms: i64,
    state: Mutex<BrokerState>,
    last_error: Mutex<Option<String>>,
    shutdown: CancellationToken,
    /// Fired once when the state reaches a terminal value, so concurrent
    /// `shutdown()` callers can wait for the drain they did not own.
    terminal: Notify,
}

/// Decrements the active-connection counter exactly once, on every exit path
/// of the owning connection task (including panics).
struct ActiveConnectionGuard(Arc<BrokerInner>);

impl Drop for ActiveConnectionGuard {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::SeqCst);
    }
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

    fn state(&self) -> BrokerState {
        *self.state.lock().unwrap()
    }

    /// Move to `next` unless the state is already terminal (terminal is
    /// final: a clean stop is never downgraded to `Failed`, and `Failed`
    /// never becomes `Stopped`). Reaching a terminal state wakes every
    /// concurrent `shutdown()` waiter.
    fn transition(&self, next: BrokerState) {
        let mut state = self.state.lock().unwrap();
        if matches!(*state, BrokerState::Stopped | BrokerState::Failed) {
            return;
        }
        *state = next;
        if matches!(next, BrokerState::Stopped | BrokerState::Failed) {
            self.terminal.notify_waiters();
        }
    }
}

/// A running broker. The handle owns the accept task, and the accept task
/// owns every connection task it accepted (via a `JoinSet`), so there is no
/// detached connection work: [`BrokerHandle::shutdown`] returns only after
/// all broker-owned sockets and tunnels are closed. Dropping the handle is
/// the last-resort path: it aborts the accept task (which aborts its
/// connection tasks) and records [`BrokerState::Failed`] (the drain was not
/// awaited).
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
            state: self.inner.state(),
            addr: self.inner.addr,
            active_connections: self.inner.active.load(Ordering::SeqCst),
            accounting: self.inner.snapshot(),
            last_error: self.inner.last_error.lock().unwrap().clone(),
            uptime_ms: now_ms().saturating_sub(self.inner.started_ms),
        }
    }

    /// Stop the broker and own the teardown: cancel, stop accepting, let
    /// live copies observe cancellation and end, drain the connection
    /// `JoinSet` within the configured grace, abort/reap stragglers, and only
    /// then report `Stopped`. When this future returns:
    ///
    /// * no connection task is live (`health().active_connections == 0`),
    /// * every broker-owned client/destination socket is closed, and
    /// * no authorized tunnel is still transferring.
    ///
    /// Idempotent and concurrency-safe: sequential repeats are no-ops, and a
    /// concurrent caller waits for the same terminal state.
    pub async fn shutdown(&self) {
        self.inner.shutdown.cancel();
        self.inner.transition(BrokerState::Draining);
        let task = self.task.lock().unwrap().take();
        if let Some(task) = task {
            if task.await.is_err() {
                // The accept task panicked or was aborted: the drain never
                // completed and terminal state must say so.
                self.inner.transition(BrokerState::Failed);
            }
        } else {
            // Another caller owns (or already finished) the join. Wait for
            // the terminal state it will record.
            loop {
                let terminal = self.inner.terminal.notified();
                if matches!(
                    self.inner.state(),
                    BrokerState::Stopped | BrokerState::Failed
                ) {
                    break;
                }
                terminal.await;
            }
        }
    }
}

impl Drop for BrokerHandle {
    fn drop(&mut self) {
        self.inner.shutdown.cancel();
        // Cannot await the drain here: abort the accept task, which drops
        // its JoinSet and aborts every connection task (sockets close at
        // their next poll). Terminal state records that no graceful drain
        // was owned.
        self.inner.transition(BrokerState::Failed);
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
            max_request_body_bytes: config.max_request_body_bytes,
            connect_timeout: Duration::from_millis(config.connect_timeout_ms),
            copy_idle_timeout: Duration::from_millis(config.copy_idle_timeout_ms),
            copy_max: Duration::from_millis(config.copy_max_ms),
            shutdown_grace: Duration::from_millis(config.shutdown_grace_ms),
            accounting: Mutex::new(AccountingCounters::default()),
            active: AtomicUsize::new(0),
            started_ms: now_ms(),
            state: Mutex::new(BrokerState::Running),
            last_error: Mutex::new(None),
            shutdown: CancellationToken::new(),
            terminal: Notify::new(),
        });
        let max_connections = config.max_connections;
        let accept_inner = inner.clone();
        let task = tokio::spawn(async move {
            let permits = Arc::new(tokio::sync::Semaphore::new(max_connections));
            // Every accepted connection is owned here, never detached.
            let mut connections: JoinSet<Result<(), String>> = JoinSet::new();
            loop {
                tokio::select! {
                    biased;
                    _ = accept_inner.shutdown.cancelled() => break,
                    joined = connections.join_next(), if !connections.is_empty() => {
                        match joined {
                            Some(Ok(Ok(()))) => {}
                            Some(Ok(Err(error))) => accept_inner.account_error(error),
                            Some(Err(join_error)) if join_error.is_panic() => {
                                accept_inner.account_error("connection task panicked");
                            }
                            Some(Err(_)) => {}
                            None => {}
                        }
                    }
                    accepted = listener.accept() => {
                        match accepted {
                            Ok((stream, _peer)) => {
                                let Ok(permit) = permits.clone().try_acquire_owned() else {
                                    accept_inner.account_error("connection ceiling reached; refusing");
                                    drop(stream);
                                    continue;
                                };
                                accept_inner.active.fetch_add(1, Ordering::SeqCst);
                                let conn_inner = accept_inner.clone();
                                connections.spawn(async move {
                                    let _guard = ActiveConnectionGuard(conn_inner.clone());
                                    let result = serve_connection(stream, conn_inner.clone()).await;
                                    drop(permit);
                                    result
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
            // Stop accepting first: dropping the listener closes the bound
            // socket before the drain so no new connection can arrive.
            drop(listener);
            accept_inner.transition(BrokerState::Draining);
            // Bounded graceful drain: connection tasks observe the cancel
            // token and end promptly; anything still live at the deadline is
            // aborted and reaped. Only after the set is empty (i.e. every
            // guard decremented `active` and every socket was dropped) is the
            // terminal state recorded.
            let deadline = tokio::time::Instant::now() + accept_inner.shutdown_grace;
            while !connections.is_empty() {
                match tokio::time::timeout_at(deadline, connections.join_next()).await {
                    Ok(Some(_)) => {}
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
            connections.abort_all();
            while connections.join_next().await.is_some() {}
            accept_inner.transition(BrokerState::Stopped);
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
///
/// The bound is never exceeded, not even transiently: a read is capped at
/// the exact remaining allowance, and once the allowance is exhausted
/// without a terminator the head is typed [`HeadError::TooLarge`] (the
/// terminator necessarily lies beyond the bound). A head whose terminator
/// ends exactly at `max_bytes` is accepted.
async fn read_head(
    stream: &mut TcpStream,
    max_bytes: usize,
) -> Result<(String, Vec<u8>), HeadError> {
    let mut buf: Vec<u8> = Vec::with_capacity(max_bytes.min(1024).saturating_add(4));
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(pos) = find_head_end(&buf) {
            if pos > max_bytes {
                return Err(HeadError::TooLarge(pos));
            }
            let head = String::from_utf8_lossy(&buf[..pos]).to_string();
            let leftover = buf[pos..].to_vec();
            return Ok((head, leftover));
        }
        if buf.len() >= max_bytes {
            // No terminator within the allowance: any terminator would end
            // beyond it. Never read the overage.
            return Err(HeadError::TooLarge(buf.len()));
        }
        let remaining = max_bytes - buf.len();
        let want = remaining.min(chunk.len());
        let read = stream
            .read(&mut chunk[..want])
            .await
            .map_err(|e| HeadError::Io(e.to_string()))?;
        if read == 0 {
            if buf.is_empty() {
                return Err(HeadError::Closed);
            }
            return Err(HeadError::Io("connection closed mid-head".to_string()));
        }
        buf.extend_from_slice(&chunk[..read]);
        // The loop re-checks both the terminator and the bound after every
        // extend, so an over-bound terminator can never be accepted.
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

/// How one request carries its body, decided strictly from the head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyFraming {
    /// No `Content-Length` and no `Transfer-Encoding`: the request has no
    /// body, and any byte beyond the head is pipeline residue (refused).
    None,
    /// One authoritative decimal `Content-Length` (equal duplicates are
    /// canonicalized to this value).
    Fixed(u64),
    /// A single `chunked` transfer coding: the body is decoded into a
    /// canonical, bounded byte string.
    Chunked,
}

/// A typed request-body refusal. Every variant maps to one explicit status
/// code and is refused before any destination socket is opened.
#[derive(Debug, Clone, PartialEq, Eq)]
enum BodyError {
    /// Contradictory or malformed framing metadata (`400`).
    Malformed(String),
    /// A transfer coding other than a single `chunked` (`501`).
    UnsupportedEncoding(String),
    /// The framed body exceeds `max_request_body_bytes` (`413`).
    TooLarge,
    /// The client closed or stalled before the declared body arrived (`400`).
    Incomplete(String),
    /// Bytes beyond the single framed request body (HTTP pipelining). The
    /// broker forwards exactly one logical request, so residue is refused
    /// before the destination socket opens (`400`). Bytes that arrive after
    /// the body was forwarded are never relayed to the destination either:
    /// the client→destination direction ends at the framed body.
    PipelineResidue,
    /// Shutdown was requested while reading; the connection ends quietly.
    Shutdown,
}

impl BodyError {
    fn status(&self) -> &'static str {
        match self {
            BodyError::Malformed(_) | BodyError::Incomplete(_) | BodyError::PipelineResidue => {
                "400 Bad Request"
            }
            BodyError::UnsupportedEncoding(_) => "501 Not Implemented",
            BodyError::TooLarge => "413 Payload Too Large",
            BodyError::Shutdown => "503 Service Unavailable",
        }
    }

    fn detail(&self) -> String {
        match self {
            BodyError::Malformed(detail)
            | BodyError::UnsupportedEncoding(detail)
            | BodyError::Incomplete(detail) => detail.clone(),
            BodyError::TooLarge => "request body exceeds the configured bound".to_string(),
            BodyError::PipelineResidue => {
                "bytes beyond the single framed request body were sent (HTTP pipeline \
                 residue); refusing"
                    .to_string()
            }
            BodyError::Shutdown => "broker is shutting down".to_string(),
        }
    }
}

/// Decide the body framing of one request head, strictly:
///
/// * `Transfer-Encoding` and `Content-Length` together are ambiguous and
///   refused;
/// * the only accepted transfer coding is a single `chunked` (final and
///   only); any other coding list is refused with a typed `501`;
/// * duplicate `Content-Length` values must be numerically equal (they
///   canonicalize to one value); unequal duplicates are refused;
/// * a non-decimal or overflowing `Content-Length` is refused.
fn request_body_framing(headers: &[(String, String)]) -> Result<BodyFraming, BodyError> {
    let mut content_lengths: Vec<u64> = Vec::new();
    let mut transfer_encodings: Vec<String> = Vec::new();
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("content-length") {
            if value.contains(',') {
                return Err(BodyError::Malformed(format!(
                    "Content-Length must be one decimal value, got {value:?}"
                )));
            }
            let trimmed = value.trim();
            if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
                return Err(BodyError::Malformed(format!(
                    "invalid Content-Length {value:?}"
                )));
            }
            let parsed: u64 = trimmed.parse().map_err(|_| {
                BodyError::Malformed(format!("Content-Length overflows u64: {value:?}"))
            })?;
            content_lengths.push(parsed);
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            for token in value.split(',') {
                let token = token.trim();
                if token.is_empty() {
                    return Err(BodyError::Malformed(
                        "empty transfer coding in Transfer-Encoding".to_string(),
                    ));
                }
                transfer_encodings.push(token.to_ascii_lowercase());
            }
        }
    }
    if !transfer_encodings.is_empty() {
        if !content_lengths.is_empty() {
            return Err(BodyError::Malformed(
                "Content-Length and Transfer-Encoding present together; ambiguous framing \
                 refused"
                    .to_string(),
            ));
        }
        if transfer_encodings.len() != 1 || transfer_encodings[0] != "chunked" {
            return Err(BodyError::UnsupportedEncoding(format!(
                "unsupported transfer coding list {:?}; only a single `chunked` is accepted",
                transfer_encodings.join(", ")
            )));
        }
        return Ok(BodyFraming::Chunked);
    }
    let mut lengths = content_lengths.iter().copied();
    if let Some(first) = lengths.next() {
        if lengths.any(|other| other != first) {
            return Err(BodyError::Malformed(
                "conflicting duplicate Content-Length values; refused".to_string(),
            ));
        }
        return Ok(BodyFraming::Fixed(first));
    }
    Ok(BodyFraming::None)
}

/// `Expect: 100-continue` is answered locally (the broker is the only
/// endpoint the client talks to) and never forwarded. Any other Expect value
/// is refused rather than guessed at.
fn expects_continue(headers: &[(String, String)]) -> Result<bool, BodyError> {
    let mut found = false;
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("expect") {
            if !value.eq_ignore_ascii_case("100-continue") {
                return Err(BodyError::Malformed(format!(
                    "unsupported Expect value {value:?}; only 100-continue is accepted"
                )));
            }
            found = true;
        }
    }
    Ok(found)
}

fn find_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == b"\r\n")
}

/// Bytes one chunk-size line (including extensions) or one trailer line may
/// occupy.
const MAX_CHUNK_LINE: usize = 1024;
/// Total trailer-section bound (validated, then discarded; `Trailer` is
/// hop-by-hop and never forwarded).
const MAX_TRAILER_BYTES: usize = 16 * 1024;

/// A bounded, cancellation-aware reader over the client's framed body. It
/// owns the post-head leftover, never reads more from the wire than the raw
/// bound, and applies the copy idle/total bounds to body reads too.
struct BodyReader<'a> {
    stream: &'a mut TcpStream,
    shutdown: &'a CancellationToken,
    buf: Vec<u8>,
    start: usize,
    wire: u64,
    wire_max: u64,
    idle: Duration,
    deadline: tokio::time::Instant,
    closed: bool,
}

impl<'a> BodyReader<'a> {
    fn new(
        stream: &'a mut TcpStream,
        leftover: Vec<u8>,
        wire_max: u64,
        idle: Duration,
        total: Duration,
        shutdown: &'a CancellationToken,
    ) -> Self {
        Self {
            stream,
            shutdown,
            buf: leftover,
            start: 0,
            wire: 0,
            wire_max,
            idle,
            deadline: tokio::time::Instant::now() + total,
            closed: false,
        }
    }

    fn buffered(&self) -> &[u8] {
        &self.buf[self.start..]
    }

    fn consume(&mut self, n: usize) {
        self.start += n;
        if self.start == self.buf.len() {
            self.buf.clear();
            self.start = 0;
        } else if self.start >= 64 * 1024 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
    }

    /// Read one more wire chunk into the buffer, bounded by the idle/total
    /// budget and by the raw wire allowance.
    async fn fill(&mut self) -> Result<(), BodyError> {
        if self.closed {
            return Err(BodyError::Incomplete(
                "client closed before the declared request body arrived".to_string(),
            ));
        }
        let now = tokio::time::Instant::now();
        if now >= self.deadline {
            return Err(BodyError::Incomplete(
                "request body deadline exceeded".to_string(),
            ));
        }
        let wait = self.idle.min(self.deadline - now);
        let mut chunk = [0u8; 16 * 1024];
        let read = tokio::select! {
            biased;
            _ = self.shutdown.cancelled() => return Err(BodyError::Shutdown),
            read = tokio::time::timeout(wait, self.stream.read(&mut chunk)) => read,
        };
        match read {
            Err(_) => Err(BodyError::Incomplete(
                "idle timeout reading the request body".to_string(),
            )),
            Ok(Err(e)) => Err(BodyError::Malformed(format!(
                "request body read failed: {e}"
            ))),
            Ok(Ok(0)) => {
                self.closed = true;
                Err(BodyError::Incomplete(
                    "client closed before the declared request body arrived".to_string(),
                ))
            }
            Ok(Ok(n)) => {
                self.wire = self.wire.saturating_add(n as u64);
                if self.wire > self.wire_max {
                    return Err(BodyError::TooLarge);
                }
                self.buf.extend_from_slice(&chunk[..n]);
                Ok(())
            }
        }
    }

    async fn ensure(&mut self, n: usize) -> Result<(), BodyError> {
        while self.buffered().len() < n {
            self.fill().await?;
        }
        Ok(())
    }

    /// Read through one CRLF, bounded by `max_line` (excluding the CRLF).
    /// A bare LF is not a line terminator and makes the line malformed at
    /// the caller's parser, never a split point.
    async fn read_line(&mut self, max_line: usize) -> Result<Vec<u8>, BodyError> {
        loop {
            if let Some(pos) = find_crlf(self.buffered()) {
                if pos > max_line {
                    return Err(BodyError::Malformed(
                        "chunk framing line exceeds its bound".to_string(),
                    ));
                }
                let line = self.buffered()[..pos].to_vec();
                self.consume(pos + 2);
                return Ok(line);
            }
            if self.buffered().len() > max_line {
                return Err(BodyError::Malformed(
                    "chunk framing line exceeds its bound".to_string(),
                ));
            }
            self.fill().await?;
        }
    }

    async fn read_exact_into(&mut self, n: usize, out: &mut Vec<u8>) -> Result<(), BodyError> {
        while self.buffered().len() < n {
            self.fill().await?;
        }
        out.extend_from_slice(&self.buffered()[..n]);
        self.consume(n);
        Ok(())
    }
}

/// Decode one chunked body into `body`, enforcing the decoded bound before
/// each append and the raw (wire) bound inside [`BodyReader`]. Extensions
/// are validated and ignored; trailers are validated and discarded.
async fn read_chunked_body(
    reader: &mut BodyReader<'_>,
    max_body: usize,
) -> Result<Vec<u8>, BodyError> {
    let mut body = Vec::new();
    loop {
        let line = reader.read_line(MAX_CHUNK_LINE).await?;
        let size_part = match line.iter().position(|&b| b == b';') {
            Some(pos) => {
                let extension = &line[pos + 1..];
                if extension
                    .iter()
                    .any(|&b| b == 0x00 || b == 0x7f || !(0x20..=0x7e).contains(&b))
                {
                    return Err(BodyError::Malformed(
                        "control character in a chunk extension".to_string(),
                    ));
                }
                &line[..pos]
            }
            None => &line[..],
        };
        let size_text = std::str::from_utf8(size_part)
            .map_err(|_| BodyError::Malformed("non-ASCII chunk size".to_string()))?
            .trim();
        if size_text.is_empty()
            || size_text.len() > 16
            || !size_text.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(BodyError::Malformed(format!(
                "invalid chunk size {:?}",
                String::from_utf8_lossy(size_part)
            )));
        }
        let size = u64::from_str_radix(size_text, 16)
            .map_err(|_| BodyError::Malformed("chunk size overflow".to_string()))?;
        if size == 0 {
            read_trailers(reader).await?;
            return Ok(body);
        }
        if body.len() as u64 + size > max_body as u64 {
            return Err(BodyError::TooLarge);
        }
        reader.read_exact_into(size as usize, &mut body).await?;
        reader.ensure(2).await?;
        if &reader.buffered()[..2] != b"\r\n" {
            return Err(BodyError::Malformed(
                "chunk data is not followed by CRLF".to_string(),
            ));
        }
        reader.consume(2);
    }
}

async fn read_trailers(reader: &mut BodyReader<'_>) -> Result<(), BodyError> {
    let mut total = 0usize;
    loop {
        let line = reader.read_line(MAX_CHUNK_LINE).await?;
        if line.is_empty() {
            return Ok(());
        }
        total = total.saturating_add(line.len() + 2);
        if total > MAX_TRAILER_BYTES {
            return Err(BodyError::Malformed(
                "trailer section exceeds its bound".to_string(),
            ));
        }
        let text = std::str::from_utf8(&line)
            .map_err(|_| BodyError::Malformed("non-ASCII trailer line".to_string()))?;
        let Some((name, value)) = text.split_once(':') else {
            return Err(BodyError::Malformed(format!(
                "malformed trailer line {text:?}"
            )));
        };
        if !is_token(name) {
            return Err(BodyError::Malformed(format!(
                "invalid trailer name {name:?}"
            )));
        }
        let value = value.trim_matches(|c| c == ' ' || c == '\t');
        if !is_header_value(value) {
            return Err(BodyError::Malformed(
                "trailer value carries control characters".to_string(),
            ));
        }
    }
}

/// Read exactly the one framed request body, then require that nothing else
/// is buffered: the broker forwards exactly one logical request, so
/// pipelined residue is refused rather than appended to the body.
async fn read_request_body(
    client: &mut TcpStream,
    leftover: Vec<u8>,
    framing: BodyFraming,
    inner: &BrokerInner,
) -> Result<Vec<u8>, BodyError> {
    let max_body = inner.max_request_body_bytes;
    // Raw wire allowance: the decoded bound plus an equal allowance for
    // chunk framing/headers plus a fixed slack. Exceeding either bound is a
    // typed `TooLarge`, so framing overhead can never grow without bound.
    let wire_max = (max_body as u64).saturating_mul(2).saturating_add(4096);
    let mut reader = BodyReader::new(
        client,
        leftover,
        wire_max,
        inner.copy_idle_timeout,
        inner.copy_max,
        &inner.shutdown,
    );
    let body = match framing {
        BodyFraming::None => Vec::new(),
        BodyFraming::Fixed(length) => {
            if length > max_body as u64 {
                return Err(BodyError::TooLarge);
            }
            let mut body = Vec::with_capacity(length as usize);
            reader.read_exact_into(length as usize, &mut body).await?;
            body
        }
        BodyFraming::Chunked => read_chunked_body(&mut reader, max_body).await?,
    };
    if !reader.buffered().is_empty() {
        return Err(BodyError::PipelineResidue);
    }
    Ok(body)
}

async fn write_refusal(stream: &mut TcpStream, error: &BodyError) {
    write_error(stream, error.status(), &error.detail()).await;
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
    // A connection parked in the head read must still end promptly when the
    // broker is shutting down.
    let head = tokio::select! {
        biased;
        _ = inner.shutdown.cancelled() => return Ok(()),
        head = read_head(&mut client, inner.max_request_bytes) => head,
    };
    let (head, leftover) = match head {
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
            return Err(format!("head read reported malformed input: {detail}"));
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
    let decision = inner
        .policy
        .decide_proxy_target(ProxyRequestKind::ConnectTunnel, &host, port);
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
    let endpoint = connect_endpoint(upstream, &host, port, &inner);
    let mut target = match connect_destination(endpoint, &inner).await {
        Ok(stream) => stream,
        Err(_detail) if inner.shutdown.is_cancelled() => return Ok(()),
        Err(detail) => {
            inner.account_error(detail.clone());
            write_error(&mut client, "502 Bad Gateway", &detail).await;
            return Ok(());
        }
    };
    // The upstream proxy expects its own CONNECT; credentials are added here
    // and never travel to the client. Any post-header bytes the upstream
    // sent are early tunnel bytes and must be relayed to the client intact.
    let early = match send_upstream_connect(
        &mut target,
        upstream,
        &host,
        port,
        inner.copy_idle_timeout,
        &inner.shutdown,
    )
    .await
    {
        Ok(early) => early,
        Err(_detail) if inner.shutdown.is_cancelled() => return Ok(()),
        Err(detail) => {
            inner.account_error(detail.clone());
            write_error(&mut client, "502 Bad Gateway", &detail).await;
            return Ok(());
        }
    };
    inner.account_tunnel();
    let mut established = b"HTTP/1.1 200 Connection Established\r\n\r\n".to_vec();
    established.extend_from_slice(&early);
    client
        .write_all(&established)
        .await
        .map_err(|e| format!("client write failed: {e}"))?;
    match copy_bidirectional_bounded(
        &mut client,
        &mut target,
        inner.copy_idle_timeout,
        inner.copy_max,
        &inner.shutdown,
    )
    .await
    {
        Ok(stats) => {
            inner.account_bytes(stats.up, stats.down);
            Ok(())
        }
        Err(CopyAbort::Cancelled { up, down }) => {
            inner.account_bytes(up, down);
            Ok(())
        }
        Err(CopyAbort::Failed(detail)) => Err(format!("tunnel copy failed: {detail}")),
    }
}

/// Upstream CONNECT response head bound.
const UPSTREAM_HEAD_LIMIT: usize = 32 * 1024;

/// Strict status-line parse: `HTTP/1.0|HTTP/1.1 SP 3DIGIT [SP reason]`, where
/// the reason (if present) is visible ASCII. Substring heuristics like
/// `" 200"` are deliberately not used: `HTTP/1.1 2000 OK` or `XD 200 OK`
/// must never pass. Returns `(code, sanitized reason)`.
fn parse_status_line(line: &str) -> Option<(u16, String)> {
    let (version, rest) = line.split_once(' ')?;
    if version != "HTTP/1.0" && version != "HTTP/1.1" {
        return None;
    }
    let (code_text, reason) = match rest.split_once(' ') {
        Some((code, reason)) => (code, reason),
        None => (rest, ""),
    };
    if code_text.len() != 3 || !code_text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if reason.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return None;
    }
    let code: u16 = code_text.parse().ok()?;
    Some((code, reason.to_string()))
}

/// Replace non-visible bytes and cap the length, so an adversarial upstream
/// status line can never inject control characters into a broker response.
fn sanitize_status_line(line: &str) -> String {
    line.chars()
        .take(120)
        .map(|c| {
            if (' '..='\u{7e}').contains(&c) {
                c
            } else {
                '?'
            }
        })
        .collect()
}

/// Send the broker→upstream CONNECT and require a strict, exact `200` status
/// line. Returns any bytes already read past the upstream response head so
/// the caller can inject them into the downstream tunnel (never dropped).
async fn send_upstream_connect(
    upstream_stream: &mut TcpStream,
    upstream: Option<&UpstreamProxy>,
    host: &str,
    port: u16,
    idle: Duration,
    shutdown: &CancellationToken,
) -> Result<Vec<u8>, String> {
    let Some(upstream) = upstream else {
        return Ok(Vec::new());
    };
    // CONNECT authority form always carries a port; IPv6 literals are
    // bracketed (the canonical host is unbracketed).
    let authority = if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    // One head, one write: the credential (when present) is appended from
    // its zeroizing value into a zeroizing byte buffer, so the secret never
    // lands in an ordinary request-head String and the head stays one
    // contiguous write (peers may parse a single read).
    let seed = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    let credential = upstream
        .credentials
        .as_ref()
        .map(ProxyCredentials::basic_header);
    let mut head = Zeroizing::new(Vec::<u8>::with_capacity(
        seed.len() + credential.as_ref().map_or(0, |value| value.len() + 22) + 40,
    ));
    head.extend_from_slice(seed.as_bytes());
    if let Some(value) = &credential {
        head.extend_from_slice(b"Proxy-Authorization: ");
        head.extend_from_slice(value.as_bytes());
        head.extend_from_slice(b"\r\n");
    }
    head.extend_from_slice(b"Proxy-Connection: keep-alive\r\n\r\n");
    upstream_stream
        .write_all(&head)
        .await
        .map_err(|e| format!("upstream CONNECT write failed: {e}"))?;
    let read = tokio::select! {
        biased;
        _ = shutdown.cancelled() => return Err("broker shutting down".to_string()),
        read = tokio::time::timeout(idle, read_head(upstream_stream, UPSTREAM_HEAD_LIMIT)) => read,
    };
    let (response, early) = match read {
        Err(_) => {
            return Err("upstream CONNECT response timed out".to_string());
        }
        Ok(Err(error)) => {
            return Err(format!("upstream CONNECT response failed: {error:?}"));
        }
        Ok(Ok(pair)) => pair,
    };
    let status_line = response.split("\r\n").next().unwrap_or_default();
    let Some((code, reason)) = parse_status_line(status_line) else {
        return Err(format!(
            "upstream proxy sent a malformed status line: {:?}",
            sanitize_status_line(status_line)
        ));
    };
    if code != 200 {
        return Err(format!(
            "upstream proxy refused CONNECT with status {code} {}",
            sanitize_status_line(&reason)
        ));
    }
    Ok(early)
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
    let decision = inner
        .policy
        .decide_proxy_target(ProxyRequestKind::ForwardHttp, &host, port);
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
    // Frame exactly one request body from the head. Contradictory metadata
    // (CL+TE, unequal duplicate CL), unsupported transfer codings, and
    // malformed framing are typed refusals issued before any destination
    // socket exists.
    let framing = match request_body_framing(&request.headers) {
        Ok(framing) => framing,
        Err(error) => {
            inner.account_blocked();
            tracing::info!(detail = %error.detail(), "egress: request framing refused");
            write_refusal(&mut client, &error).await;
            return Ok(());
        }
    };
    let expect_continue = match expects_continue(&request.headers) {
        Ok(value) => value,
        Err(error) => {
            inner.account_blocked();
            tracing::info!(detail = %error.detail(), "egress: Expect header refused");
            write_refusal(&mut client, &error).await;
            return Ok(());
        }
    };
    if expect_continue {
        // The broker is the only endpoint the client talks to, so answer
        // locally; the Expect header itself is never forwarded.
        client
            .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
            .await
            .map_err(|e| format!("client write failed: {e}"))?;
    }
    // Read exactly the framed body (bounded, canonical, cancellation-aware);
    // pipelined residue is refused, never appended to the body.
    let body = match read_request_body(&mut client, leftover, framing, &inner).await {
        Ok(body) => body,
        Err(BodyError::Shutdown) => return Ok(()),
        Err(error) => {
            inner.account_blocked();
            tracing::info!(detail = %error.detail(), "egress: request body refused");
            write_refusal(&mut client, &error).await;
            return Ok(());
        }
    };
    let upstream = inner.upstream.select(&host);
    let endpoint = connect_endpoint(upstream, &host, port, &inner);
    let mut target = match connect_destination(endpoint, &inner).await {
        Ok(stream) => stream,
        Err(_detail) if inner.shutdown.is_cancelled() => return Ok(()),
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
    // The broker owns the Host header: every incoming Host is stripped
    // (conflicting/duplicate spellings included) and exactly one canonical
    // Host is synthesized from the policy-checked destination. Default ports
    // are omitted and IPv6 literals are bracketed.
    let host_header = canonical_authority(&host, port, &scheme);
    for (name, value) in &request.headers {
        if is_hop_by_hop(name, &connection_listed)
            || name.eq_ignore_ascii_case("content-length")
            || name.eq_ignore_ascii_case("transfer-encoding")
            || name.eq_ignore_ascii_case("expect")
            || name.eq_ignore_ascii_case("host")
        {
            continue;
        }
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str(&format!("Host: {host_header}\r\n"));
    let credential = upstream
        .and_then(|upstream| upstream.credentials.as_ref())
        .map(ProxyCredentials::basic_header);
    // Exactly one authoritative Content-Length describes the canonical body,
    // whatever framing the client used; `Connection: close` keeps the single
    // logical request unambiguous on the upstream leg. The
    // Proxy-Authorization value (if any) is appended from its zeroizing
    // buffer into a zeroizing head buffer — never copied into an ordinary
    // String — and the head stays one contiguous write.
    let tail = format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let mut wire_head = Zeroizing::new(Vec::<u8>::with_capacity(
        out.len() + credential.as_ref().map_or(0, |value| value.len() + 22) + tail.len(),
    ));
    wire_head.extend_from_slice(out.as_bytes());
    if let Some(value) = &credential {
        wire_head.extend_from_slice(b"Proxy-Authorization: ");
        wire_head.extend_from_slice(value.as_bytes());
        wire_head.extend_from_slice(b"\r\n");
    }
    wire_head.extend_from_slice(tail.as_bytes());
    tracing::debug!(
        method = %request.method,
        host = %host,
        path = %crate::capture::redact_url(&origin_form),
        "egress: request allowed"
    );
    let write = async {
        target.write_all(&wire_head).await?;
        if !body.is_empty() {
            target.write_all(&body).await?;
        }
        Ok::<(), std::io::Error>(())
    };
    match tokio::select! {
        biased;
        _ = inner.shutdown.cancelled() => return Ok(()),
        result = tokio::time::timeout(inner.copy_idle_timeout, write) => result,
    } {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(format!("upstream write failed: {e}")),
        Err(_) => return Err("idle timeout writing the request to the target leg".to_string()),
    }
    // Exactly one logical request was forwarded. Only the response is
    // relayed back; the client→destination direction ended at the framed
    // body, so pipelined bytes arriving later can never reach the
    // destination.
    match relay_response_bounded(
        &mut target,
        &mut client,
        inner.copy_idle_timeout,
        inner.copy_max,
        &inner.shutdown,
    )
    .await
    {
        Ok(down) => {
            inner.account_bytes(body.len() as u64, down);
            Ok(())
        }
        Err(CopyAbort::Cancelled { down, .. }) => {
            inner.account_bytes(body.len() as u64, down);
            Ok(())
        }
        Err(CopyAbort::Failed(detail)) => Err(format!("forward copy failed: {detail}")),
    }
}

// ------------------------------------------------- destination-connect vetting
//
// Every broker connect — a direct destination OR an upstream proxy leg — is
// vetted through the single `faktor_security::network` address authority.
// There is no locally maintained class table: the hand-mirrored classifier
// that used to live here was deleted (and the static-authority scan fails if
// a local redefinition reappears).

/// Default hard bound on the number of addresses one broker connect may
/// consume.
pub const MAX_RESOLVED_ADDRESSES: usize = 16;

/// One connect endpoint. Each leg carries its OWN address policy: the
/// destination uses the connector policy's explicit `allow_loopback` rule,
/// the upstream proxy uses [`UpstreamProxy::address_policy`] (default
/// EXTERNAL). The upstream policy is NEVER inherited from the destination,
/// so a loopback-tolerant destination policy cannot turn the proxy host into
/// a loopback/metadata connect.
enum ConnectEndpoint<'a> {
    Destination {
        host: String,
        port: u16,
        address_policy: EgressAddressPolicy,
    },
    UpstreamProxy {
        proxy: &'a UpstreamProxy,
    },
}

impl ConnectEndpoint<'_> {
    fn host(&self) -> &str {
        match self {
            ConnectEndpoint::Destination { host, .. } => host,
            ConnectEndpoint::UpstreamProxy { proxy } => &proxy.host,
        }
    }

    fn port(&self) -> u16 {
        match self {
            ConnectEndpoint::Destination { port, .. } => *port,
            ConnectEndpoint::UpstreamProxy { proxy } => proxy.port,
        }
    }

    fn address_policy(&self) -> EgressAddressPolicy {
        match self {
            ConnectEndpoint::Destination { address_policy, .. } => *address_policy,
            ConnectEndpoint::UpstreamProxy { proxy } => proxy.address_policy,
        }
    }
}

/// The destination-leg policy installed by one connector policy.
fn destination_address_policy(policy: &DestinationPolicy) -> EgressAddressPolicy {
    EgressAddressPolicy::from_allow_loopback(policy.allow_loopback)
}

/// The endpoint for one broker connect: the selected upstream proxy leg when
/// one applies, otherwise the destination itself (with the connector's
/// explicit loopback rule).
fn connect_endpoint<'a>(
    upstream: Option<&'a UpstreamProxy>,
    host: &str,
    port: u16,
    inner: &BrokerInner,
) -> ConnectEndpoint<'a> {
    match upstream {
        Some(proxy) => ConnectEndpoint::UpstreamProxy { proxy },
        None => ConnectEndpoint::Destination {
            host: host.to_string(),
            port,
            address_policy: destination_address_policy(&inner.policy),
        },
    }
}

/// Bound the answer set and vet EVERY address through the shared authority;
/// any refused class refuses the WHOLE set (a mixed public/private answer
/// set is a rebinding signal, never filtered into a racing subset).
fn vet_connect_answers(
    host: &str,
    answers: Vec<SocketAddr>,
    address_policy: EgressAddressPolicy,
) -> Result<Vec<SocketAddr>, String> {
    if answers.is_empty() {
        return Err(format!("DNS for {host} returned no addresses"));
    }
    if answers.len() > MAX_RESOLVED_ADDRESSES {
        return Err(format!(
            "DNS for {host} answered with {} addresses, over the bound of \
             {MAX_RESOLVED_ADDRESSES}",
            answers.len()
        ));
    }
    faktor_security::network::vet_resolved_answers(&answers, address_policy).map_err(
        |refusal| {
            format!(
                "DNS for {host} resolved to a {} address ({}), which the egress \
             address policy refuses",
                refusal.class, refusal.address
            )
        },
    )?;
    Ok(answers)
}

async fn connect_destination(
    endpoint: ConnectEndpoint<'_>,
    inner: &Arc<BrokerInner>,
) -> Result<TcpStream, String> {
    let host = endpoint.host().to_string();
    let port = endpoint.port();
    let address_policy = endpoint.address_policy();
    // A literal-IP host is NOT a bypass: it is vetted through the SAME
    // classification + policy function as a resolved name, as a
    // single-address answer set (no DNS involved). A NAME is resolved
    // EXACTLY ONCE, its full answer set is vetted, and the connect uses only
    // those `SocketAddr`s — no second resolution between the decision and
    // the socket. Both the direct and the upstream-proxy legs use this one
    // path, each with its own `address_policy`.
    let attempt = async {
        let vetted = match host.parse::<std::net::IpAddr>() {
            Ok(ip) => vet_connect_answers(&host, vec![SocketAddr::new(ip, port)], address_policy)?,
            Err(_) => {
                let answers = tokio::net::lookup_host((host.as_str(), port))
                    .await
                    .map(|addrs| addrs.collect::<Vec<SocketAddr>>())
                    .map_err(|_| format!("cannot resolve {host}:{port}"))?;
                vet_connect_answers(&host, answers, address_policy)?
            }
        };
        let mut last_error: Option<std::io::Error> = None;
        for addr in vetted {
            match TcpStream::connect(addr).await {
                Ok(stream) => return Ok(stream),
                Err(e) => last_error = Some(e),
            }
        }
        Err(format!(
            "cannot connect {host}:{port}: {}",
            last_error
                .map(|e| e.to_string())
                .unwrap_or_else(|| "no vetted address".to_string())
        ))
    };
    let result = tokio::select! {
        biased;
        _ = inner.shutdown.cancelled() => return Err("broker shutting down".to_string()),
        result = tokio::time::timeout(inner.connect_timeout, attempt) => result,
    };
    match result {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(detail)) => Err(detail),
        Err(_) => Err(format!("connect to {host}:{port} timed out")),
    }
}

/// Bytes copied by one finished copy.
struct CopyStats {
    up: u64,
    down: u64,
}

/// Why a copy ended before EOF on both legs.
enum CopyAbort {
    /// The broker was shutting down: the copy observed the cancel token and
    /// ended at an arbitrary transfer point. Bytes copied so far are
    /// reported for accounting.
    Cancelled { up: u64, down: u64 },
    /// A real failure (timeout, I/O error, deadline): the reason the permit
    /// is being released.
    Failed(String),
}

/// Bounded bidirectional copy (CONNECT tunnels): one idle timeout per
/// read/write, a total copy deadline, and prompt termination when the
/// broker's shutdown token fires. Both directions half-close on EOF. On
/// timeout/error/cancellation the sockets are left for the caller to drop,
/// so a stalled peer can never pin a `max_connections` permit indefinitely.
async fn copy_bidirectional_bounded(
    client: &mut TcpStream,
    target: &mut TcpStream,
    idle: Duration,
    total: Duration,
    shutdown: &CancellationToken,
) -> Result<CopyStats, CopyAbort> {
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
            return Err(CopyAbort::Failed(format!(
                "copy deadline of {}ms exceeded",
                total.as_millis()
            )));
        }
        let wait = idle.min(deadline - now);
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => return Err(CopyAbort::Cancelled { up, down }),
            read = tokio::time::timeout(wait, client.read(&mut client_buf)), if client_open => {
                match read {
                    Err(_) => return Err(CopyAbort::Failed("idle timeout on the client leg".to_string())),
                    Ok(Err(e)) => return Err(CopyAbort::Failed(format!("client read failed: {e}"))),
                    Ok(Ok(0)) => {
                        client_open = false;
                        let _ = target.shutdown().await;
                    }
                    Ok(Ok(n)) => {
                        match tokio::time::timeout(wait, target.write_all(&client_buf[..n])).await {
                            Err(_) => return Err(CopyAbort::Failed("idle timeout writing to the target leg".to_string())),
                            Ok(Err(e)) => return Err(CopyAbort::Failed(format!("target write failed: {e}"))),
                            Ok(Ok(())) => up = up.saturating_add(n as u64),
                        }
                    }
                }
            }
            read = tokio::time::timeout(wait, target.read(&mut target_buf)), if target_open => {
                match read {
                    Err(_) => return Err(CopyAbort::Failed("idle timeout on the target leg".to_string())),
                    Ok(Err(e)) => return Err(CopyAbort::Failed(format!("target read failed: {e}"))),
                    Ok(Ok(0)) => {
                        target_open = false;
                        let _ = client.shutdown().await;
                    }
                    Ok(Ok(n)) => {
                        match tokio::time::timeout(wait, client.write_all(&target_buf[..n])).await {
                            Err(_) => return Err(CopyAbort::Failed("idle timeout writing to the client leg".to_string())),
                            Ok(Err(e)) => return Err(CopyAbort::Failed(format!("client write failed: {e}"))),
                            Ok(Ok(())) => down = down.saturating_add(n as u64),
                        }
                    }
                }
            }
        }
    }
    Ok(CopyStats { up, down })
}

/// Relay exactly the response leg of one forwarded request
/// (destination→client) with the same idle/total bounds and shutdown
/// observation as the tunnel copy. The client→destination direction is
/// deliberately absent: the single framed request body was already
/// forwarded, so any later client bytes stay local and are discarded when
/// the connection closes.
async fn relay_response_bounded(
    target: &mut TcpStream,
    client: &mut TcpStream,
    idle: Duration,
    total: Duration,
    shutdown: &CancellationToken,
) -> Result<u64, CopyAbort> {
    let deadline = tokio::time::Instant::now() + total;
    let mut down = 0u64;
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(CopyAbort::Failed(format!(
                "copy deadline of {}ms exceeded",
                total.as_millis()
            )));
        }
        let wait = idle.min(deadline - now);
        let read = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return Err(CopyAbort::Cancelled { up: 0, down }),
            read = tokio::time::timeout(wait, target.read(&mut buf)) => read,
        };
        match read {
            Err(_) => {
                return Err(CopyAbort::Failed(
                    "idle timeout on the target leg".to_string(),
                ));
            }
            Ok(Err(e)) => return Err(CopyAbort::Failed(format!("target read failed: {e}"))),
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => match tokio::time::timeout(wait, client.write_all(&buf[..n])).await {
                Err(_) => {
                    return Err(CopyAbort::Failed(
                        "idle timeout writing to the client leg".to_string(),
                    ));
                }
                Ok(Err(e)) => return Err(CopyAbort::Failed(format!("client write failed: {e}"))),
                Ok(Ok(())) => down = down.saturating_add(n as u64),
            },
        }
    }
    let _ = client.shutdown().await;
    Ok(down)
}

/// The canonical `Host`/authority value for one policy-checked destination:
/// the default port for the scheme is omitted, IPv6 literals are bracketed.
fn canonical_authority(host: &str, port: u16, scheme: &str) -> String {
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    let default_port = (scheme.eq_ignore_ascii_case("http") && port == 80)
        || (scheme.eq_ignore_ascii_case("https") && port == 443)
        || (scheme.eq_ignore_ascii_case("ws") && port == 80)
        || (scheme.eq_ignore_ascii_case("wss") && port == 443);
    if default_port {
        host
    } else {
        format!("{host}:{port}")
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
        "allowed_ports": policy.allowed_ports,
        "allow_connect": policy.allow_connect,
        "allow_loopback": policy.allow_loopback,
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
    fn credentials_are_bounded_and_never_render_their_secrets() {
        let credentials = ProxyCredentials::new("PLANTED-USER-NAME", "PLANTED-PASSWORD-2f6c");
        let proxy = UpstreamProxy::new("proxy.test", 8080).with_credentials(credentials.clone());
        let selector = UpstreamSelector::new(Some(proxy.clone()));
        let config = BrokerConfig {
            upstream: selector.clone(),
            ..BrokerConfig::default()
        };
        // Debug is redacted through every nesting level; there is no Display
        // and no Serialize on credentials at all.
        for rendered in [
            format!("{credentials:?}"),
            format!("{proxy:?}"),
            format!("{selector:?}"),
            format!("{config:?}"),
        ] {
            assert!(!rendered.contains("PLANTED-USER-NAME"), "{rendered}");
            assert!(!rendered.contains("PLANTED-PASSWORD-2f6c"), "{rendered}");
        }
        assert!(format!("{credentials:?}").contains("[redacted]"));
        // The Basic value carries the real material (only ever written to
        // the upstream socket) and is base64 of `user:pass`.
        let header = credentials.basic_header();
        assert!(header.starts_with("Basic "));
        assert!(!header.contains("PLANTED-PASSWORD-2f6c"));
        use base64::Engine as _;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(header.strip_prefix("Basic ").unwrap())
            .unwrap();
        assert_eq!(decoded, b"PLANTED-USER-NAME:PLANTED-PASSWORD-2f6c");
        // Bounds and header-injection material, refused with a typed error.
        assert!(matches!(
            ProxyCredentials::try_new("u".repeat(MAX_PROXY_USERNAME_BYTES + 1), "p"),
            Err(BrowserError::InvalidConfig { .. })
        ));
        assert!(ProxyCredentials::try_new("u", "p".repeat(MAX_PROXY_PASSWORD_BYTES + 1)).is_err());
        assert!(ProxyCredentials::try_new(
            "u".repeat(MAX_PROXY_USERNAME_BYTES),
            "p".repeat(MAX_PROXY_PASSWORD_BYTES)
        )
        .is_ok());
        for bad in ["bad\ruser", "bad\nuser", "bad\0user"] {
            assert!(ProxyCredentials::try_new(bad, "p").is_err(), "{bad:?}");
            assert!(ProxyCredentials::try_new("u", bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn upstream_proxy_host_is_canonicalized_and_hostile_forms_are_refused() {
        assert_eq!(
            UpstreamProxy::try_new("EXAMPLE.COM.", 8080).unwrap().host,
            "example.com"
        );
        assert_eq!(
            UpstreamProxy::try_new("bücher.example", 8080).unwrap().host,
            "xn--bcher-kva.example"
        );
        assert_eq!(
            UpstreamProxy::try_new("[0:0:0:0:0:0:0:1]", 8080)
                .unwrap()
                .host,
            "::1"
        );
        assert_eq!(
            UpstreamProxy::try_new("127.000.000.001", 8080)
                .unwrap()
                .host,
            "127.0.0.1"
        );
        for hostile in [
            "",
            " ",
            "user@proxy.test",
            "user:pass@proxy.test",
            "proxy.test:8080",
            "http://proxy.test",
            "bad host",
            "proxy.test\n",
            "proxy.test\0",
            "fe80::1%eth0",
            "[fe80::1%25eth0]",
            "2130706433",
            "*",
        ] {
            let error = UpstreamProxy::try_new(hostile, 8080)
                .expect_err(&format!("{hostile:?} must be refused"));
            assert!(
                matches!(error, BrowserError::InvalidConfig { .. }),
                "{hostile:?}: typed refusal expected, got {error:?}"
            );
        }
        assert!(UpstreamProxy::try_new("proxy.test", 0).is_err());
        // A hand-built non-canonical struct is refused by validate().
        let hand_built = UpstreamProxy {
            host: "PROXY.TEST".to_string(),
            port: 8080,
            credentials: None,
            address_policy: EgressAddressPolicy::EXTERNAL,
        };
        assert!(hand_built.validate().is_err());
        assert!(UpstreamProxy::try_new("proxy.test", 8080)
            .unwrap()
            .validate()
            .is_ok());
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
        // A hostile userinfo target is rejected outright: it is never
        // stripped into a valid destination that the policy then allows.
        assert_eq!(
            policy.decide_url("https://evil.test@example.com/x"),
            DestinationDecision::Blocked {
                reason: BlockReason::Malformed
            }
        );
        assert_eq!(
            policy.decide_url("https://example.com@evil.test/x"),
            DestinationDecision::Blocked {
                reason: BlockReason::Malformed
            }
        );
        assert_eq!(split_absolute_target("http://user@example.com:80/x"), None);
        assert_eq!(
            split_absolute_target("http://user:pass@example.com:80"),
            None
        );
        assert_eq!(parse_connect_target("user@example.com:443"), None);
        assert_eq!(parse_connect_target("user:pass@example.com:443"), None);
        assert!(parse_connect_target("example.com:443").is_some());
    }

    #[test]
    fn canonical_authority_omits_default_ports_and_brackets_ipv6() {
        assert_eq!(
            canonical_authority("example.com", 80, "http"),
            "example.com"
        );
        assert_eq!(
            canonical_authority("example.com", 443, "https"),
            "example.com"
        );
        assert_eq!(
            canonical_authority("example.com", 8443, "https"),
            "example.com:8443"
        );
        assert_eq!(
            canonical_authority("example.com", 8080, "http"),
            "example.com:8080"
        );
        assert_eq!(canonical_authority("::1", 80, "http"), "[::1]");
        assert_eq!(canonical_authority("::1", 8443, "http"), "[::1]:8443");
    }

    #[test]
    fn connect_is_a_first_class_policy_dimension() {
        // A policy that only ever needed HTTP denies CONNECT outright, even
        // for an allowed host on an allowed port: a tunnel is not a forward
        // request to that host.
        let http_only =
            DestinationPolicy::first_party_only(vec![HostPattern::parse("example.com").unwrap()])
                .with_allow_schemes(vec!["http".to_string()]);
        assert!(!http_only.allow_connect, "derived from the scheme set");
        assert!(http_only.validate().is_ok());
        assert_eq!(
            http_only.decide_proxy_target(ProxyRequestKind::ConnectTunnel, "example.com", 443),
            DestinationDecision::Blocked {
                reason: BlockReason::ConnectNotAllowed
            }
        );
        assert!(http_only
            .decide_proxy_target(ProxyRequestKind::ForwardHttp, "example.com", 80)
            .is_allowed());
        // The default https-capable policy allows the tunnel explicitly.
        let https =
            DestinationPolicy::first_party_only(vec![HostPattern::parse("example.com").unwrap()]);
        assert!(https.allow_connect);
        assert!(https
            .decide_proxy_target(ProxyRequestKind::ConnectTunnel, "example.com", 443)
            .is_allowed());
        // A malformed CONNECT authority is still malformed, not "not
        // allowed".
        assert_eq!(
            https.decide_proxy_target(ProxyRequestKind::ConnectTunnel, "bad host", 443),
            DestinationDecision::Blocked {
                reason: BlockReason::Malformed
            }
        );
        // Explicit denial wins even with https support.
        let denied = https.clone().with_allow_connect(false);
        assert_eq!(
            denied.decide_proxy_target(ProxyRequestKind::ConnectTunnel, "example.com", 443),
            DestinationDecision::Blocked {
                reason: BlockReason::ConnectNotAllowed
            }
        );
        // An inconsistent policy (CONNECT allowed without HTTPS/WSS) is a
        // configuration error, never a silent allow.
        let inconsistent = DestinationPolicy {
            allow_connect: true,
            ..http_only
        };
        assert!(inconsistent.validate().is_err());
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

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn request_body_framing_is_strict() {
        assert_eq!(request_body_framing(&[]).unwrap(), BodyFraming::None);
        assert_eq!(
            request_body_framing(&headers(&[("Content-Length", "4")])).unwrap(),
            BodyFraming::Fixed(4)
        );
        // Numerically equal duplicates canonicalize to one value.
        assert_eq!(
            request_body_framing(&headers(&[
                ("Content-Length", "4"),
                ("Content-Length", "04")
            ]))
            .unwrap(),
            BodyFraming::Fixed(4)
        );
        assert!(matches!(
            request_body_framing(&headers(&[
                ("Content-Length", "4"),
                ("Content-Length", "5")
            ])),
            Err(BodyError::Malformed(_))
        ));
        assert!(matches!(
            request_body_framing(&headers(&[
                ("Content-Length", "4"),
                ("Transfer-Encoding", "chunked")
            ])),
            Err(BodyError::Malformed(_))
        ));
        assert!(matches!(
            request_body_framing(&headers(&[("Transfer-Encoding", "gzip, chunked")])),
            Err(BodyError::UnsupportedEncoding(_))
        ));
        assert!(matches!(
            request_body_framing(&headers(&[("Transfer-Encoding", "chunked, chunked")])),
            Err(BodyError::UnsupportedEncoding(_))
        ));
        assert_eq!(
            request_body_framing(&headers(&[("Transfer-Encoding", "Chunked")])).unwrap(),
            BodyFraming::Chunked
        );
        assert!(matches!(
            request_body_framing(&headers(&[("Content-Length", "4x")])),
            Err(BodyError::Malformed(_))
        ));
        assert!(matches!(
            request_body_framing(&headers(&[("Content-Length", "18446744073709551616")])),
            Err(BodyError::Malformed(_))
        ));
        assert!(matches!(
            request_body_framing(&headers(&[("Content-Length", "4, 4")])),
            Err(BodyError::Malformed(_))
        ));
    }

    #[test]
    fn status_line_parse_is_strict() {
        assert_eq!(
            parse_status_line("HTTP/1.1 200 OK"),
            Some((200, "OK".to_string()))
        );
        assert_eq!(
            parse_status_line("HTTP/1.0 200"),
            Some((200, String::new()))
        );
        assert_eq!(
            parse_status_line("HTTP/1.1 200 "),
            Some((200, String::new()))
        );
        assert_eq!(
            parse_status_line("HTTP/1.1 407 Proxy Authentication Required").map(|(code, _)| code),
            Some(407)
        );
        for bad in [
            "HTTP/1.1 2000 OK",
            "XD 200 OK",
            "HTTP/9 200 OK",
            "HTTP/1.1 20 OK",
            "HTTP/1.1 200OK",
            "HTTP/1.1  200 OK",
            "HTTP/2 200 OK",
            "HTTP/1.1\t200 OK",
            "",
        ] {
            assert!(parse_status_line(bad).is_none(), "{bad:?}");
        }
    }

    // --- connect vetting through the single faktor-security authority ---

    fn ip(text: &str) -> std::net::IpAddr {
        text.parse().expect("test IP")
    }

    fn sa(text: &str, port: u16) -> SocketAddr {
        SocketAddr::new(ip(text), port)
    }

    fn test_inner(policy: DestinationPolicy) -> Arc<BrokerInner> {
        Arc::new(BrokerInner {
            addr: "127.0.0.1:0".parse().unwrap(),
            policy,
            upstream: UpstreamSelector::default(),
            max_request_bytes: 32 * 1024,
            max_request_body_bytes: 1024 * 1024,
            connect_timeout: Duration::from_millis(500),
            copy_idle_timeout: Duration::from_millis(500),
            copy_max: Duration::from_millis(500),
            shutdown_grace: Duration::from_millis(100),
            accounting: Mutex::new(AccountingCounters::default()),
            active: AtomicUsize::new(0),
            started_ms: 0,
            state: Mutex::new(BrokerState::Running),
            last_error: Mutex::new(None),
            shutdown: CancellationToken::new(),
            terminal: Notify::new(),
        })
    }

    #[test]
    fn classify_ip_comes_from_the_shared_authority() {
        use faktor_security::network::{classify_ip, AddressClass};
        assert_eq!(classify_ip(ip("127.0.0.1")), AddressClass::Loopback);
        assert_eq!(classify_ip(ip("169.254.169.254")), AddressClass::LinkLocal);
        // Ranges the deleted mirror missed, plus transition/tunneling forms
        // that are never classified by their embedded IPv4 address.
        for text in [
            "64:ff9b:1::1",
            "100:0:0:1::1",
            "3fff::1",
            "5f00::1",
            "::ffff:8.8.8.8",
            "64:ff9b::7f00:1",
            "2002:7f00:1::",
        ] {
            assert_ne!(classify_ip(ip(text)), AddressClass::Global, "{text}");
        }
    }

    #[test]
    fn answer_vetting_bounds_the_set_and_refuses_wholesale() {
        for (label, answer) in [
            ("loopback", "127.0.0.1"),
            ("metadata", "169.254.169.254"),
            ("private", "10.0.0.1"),
            ("v6 loopback", "::1"),
            ("v6 link-local", "fe80::1"),
        ] {
            let err = vet_connect_answers(
                "allowed.test",
                vec![sa(answer, 443)],
                EgressAddressPolicy::EXTERNAL,
            )
            .expect_err(label);
            assert!(err.contains("refuses"), "{label}: {err}");
        }
        // Mixed public/private: the whole set is refused, never filtered.
        let err = vet_connect_answers(
            "allowed.test",
            vec![sa("93.184.216.34", 443), sa("127.0.0.1", 443)],
            EgressAddressPolicy::EXTERNAL,
        )
        .expect_err("mixed set");
        assert!(err.contains("refuses"), "{err}");
        // The explicit loopback rule admits loopback and nothing else.
        assert_eq!(
            vet_connect_answers(
                "local.test",
                vec![sa("127.0.0.1", 11434)],
                EgressAddressPolicy::LOCAL
            )
            .unwrap(),
            vec![sa("127.0.0.1", 11434)]
        );
        let err = vet_connect_answers(
            "local.test",
            vec![sa("169.254.169.254", 80)],
            EgressAddressPolicy::LOCAL,
        )
        .unwrap_err();
        assert!(err.contains("link_local"), "{err}");
        // Answer-set bound, empty set.
        let flood: Vec<SocketAddr> = (1..=MAX_RESOLVED_ADDRESSES + 1)
            .map(|i| sa(&format!("93.184.216.{i}"), 443))
            .collect();
        let err =
            vet_connect_answers("flood.test", flood, EgressAddressPolicy::EXTERNAL).unwrap_err();
        assert!(err.contains("over the bound"), "{err}");
        let err = vet_connect_answers("empty.test", Vec::new(), EgressAddressPolicy::EXTERNAL)
            .unwrap_err();
        assert!(err.contains("no addresses"), "{err}");
    }

    #[tokio::test]
    async fn literal_ip_destinations_are_class_vetted_before_any_dial() {
        // The P0 bypass: a literal-IP host used to skip classification. Bind
        // a listener so a skipped vet would actually connect.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let loopback_port = listener.local_addr().unwrap().port();
        let inner = test_inner(DestinationPolicy::first_party_only(Vec::new()));
        for (literal, port) in [
            ("127.0.0.1", loopback_port),
            ("169.254.169.254", 9),
            ("10.0.0.1", 9),
            ("172.16.0.1", 9),
            ("192.168.1.1", 9),
            ("100.64.0.1", 9),
            ("::1", loopback_port),
            ("fe80::1", 9),
            ("fc00::1", 9),
        ] {
            let endpoint = ConnectEndpoint::Destination {
                host: literal.to_string(),
                port,
                address_policy: EgressAddressPolicy::EXTERNAL,
            };
            let err = connect_destination(endpoint, &inner)
                .await
                .expect_err(literal);
            assert!(err.contains("refuses"), "{literal}: {err}");
        }
        // The explicit local rule connects loopback and only loopback.
        let endpoint = ConnectEndpoint::Destination {
            host: "127.0.0.1".to_string(),
            port: loopback_port,
            address_policy: EgressAddressPolicy::LOCAL,
        };
        assert!(
            connect_destination(endpoint, &inner).await.is_ok(),
            "local loopback must connect"
        );
    }

    #[tokio::test]
    async fn upstream_proxy_leg_policy_is_independent_of_the_destination() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        // The destination policy tolerates loopback...
        let inner =
            test_inner(DestinationPolicy::first_party_only(Vec::new()).with_allow_loopback(true));
        // ...but the proxy's own default (EXTERNAL) still refuses a loopback
        // proxy instead of inheriting the destination's rule.
        let external_proxy = UpstreamProxy::try_new("127.0.0.1", port).unwrap();
        let err = connect_destination(
            ConnectEndpoint::UpstreamProxy {
                proxy: &external_proxy,
            },
            &inner,
        )
        .await
        .expect_err("loopback proxy under EXTERNAL");
        assert!(err.contains("loopback"), "{err}");
        // Metadata on the proxy leg is refused even under LOCAL.
        let metadata_proxy = UpstreamProxy::try_new("169.254.169.254", 80)
            .unwrap()
            .with_address_policy(EgressAddressPolicy::LOCAL);
        let err = connect_destination(
            ConnectEndpoint::UpstreamProxy {
                proxy: &metadata_proxy,
            },
            &inner,
        )
        .await
        .unwrap_err();
        assert!(err.contains("refuses"), "{err}");
        // An operator explicitly naming a local proxy installs LOCAL on the
        // proxy leg; it never widens the destination leg.
        let local_proxy = UpstreamProxy::try_new("127.0.0.1", port)
            .unwrap()
            .with_address_policy(EgressAddressPolicy::LOCAL);
        assert!(connect_destination(
            ConnectEndpoint::UpstreamProxy {
                proxy: &local_proxy
            },
            &inner
        )
        .await
        .is_ok());
        let endpoint = ConnectEndpoint::Destination {
            host: "127.0.0.1".to_string(),
            port,
            address_policy: EgressAddressPolicy::EXTERNAL,
        };
        assert!(connect_destination(endpoint, &inner).await.is_err());
    }

    #[tokio::test]
    async fn broker_wire_path_refuses_literal_ip_without_the_explicit_rule() {
        // End-to-end over the real broker wire path: the CONNECT target is a
        // literal IP, which used to skip class vetting entirely.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin_port = listener.local_addr().unwrap().port();
        let policy =
            DestinationPolicy::first_party_only(vec![HostPattern::parse("127.0.0.1").unwrap()])
                .with_allowed_ports(vec![origin_port]);
        let broker = EgressBroker::start(BrokerConfig {
            policy: policy.clone(),
            ..BrokerConfig::default()
        })
        .await
        .unwrap();
        let connect = format!(
            "CONNECT 127.0.0.1:{origin_port} HTTP/1.1\r\nHost: 127.0.0.1:{origin_port}\r\n\r\n"
        );
        let response = send_raw_request(broker.addr(), &connect).await;
        assert!(response.starts_with("HTTP/1.1 502"), "{response}");
        assert!(response.contains("loopback"), "{response}");
        broker.shutdown().await;
        // With the explicit loopback rule the same literal connects.
        let broker = EgressBroker::start(BrokerConfig {
            policy: policy.with_allow_loopback(true),
            ..BrokerConfig::default()
        })
        .await
        .unwrap();
        let response = send_raw_request(broker.addr(), &connect).await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        broker.shutdown().await;
    }

    async fn send_raw_request(addr: SocketAddr, request: &str) -> String {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut buffer = [0u8; 4096];
        match tokio::time::timeout(Duration::from_millis(1500), stream.read(&mut buffer)).await {
            Ok(Ok(n)) => String::from_utf8_lossy(&buffer[..n]).to_string(),
            _ => String::new(),
        }
    }

    #[test]
    fn destination_policy_loopback_rule_defaults_to_external_only() {
        let policy =
            DestinationPolicy::first_party_only(vec![HostPattern::parse("example.com").unwrap()]);
        assert!(!policy.allow_loopback);
        assert!(policy.clone().with_allow_loopback(true).allow_loopback);
        // The rule is a first-class snapshot field: config round-trips it.
        let snapshot = policy_snapshot(&policy.clone().with_allow_loopback(true));
        assert_eq!(snapshot["allow_loopback"], serde_json::json!(true));
        let http_only =
            DestinationPolicy::first_party_only(vec![HostPattern::parse("example.com").unwrap()])
                .with_allow_schemes(vec!["http".to_string()])
                .with_allow_loopback(true);
        assert!(
            http_only.validate().is_ok(),
            "the rule never weakens validation"
        );
    }
}
