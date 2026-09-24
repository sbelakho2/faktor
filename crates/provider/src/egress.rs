//! Egress enforcement seam for outbound HTTP (audits 36-37).
//!
//! Every app-level outbound call must decide on a **parsed** destination
//! before the connection is attempted. [`CheckedHttpClient`] wraps a
//! `reqwest::Client` built HERE with an installed [`DestinationPolicy`]
//! (security crate): the decision runs against the `reqwest::Url` of the
//! *exact request object* that will be sent — scheme, host and port pulled
//! from the parsed URL, never from strings.
//!
//! # DNS rebinding is closed by the central resolver
//!
//! The host-text allowlist alone cannot stop a permitted hostname from
//! RESOLVING to `127.0.0.1`, `169.254.169.254`, an RFC1918 address, `::1`
//! or `fe80::1`. Every production client installed by this module pins
//! [`crate::resolver::EgressResolver`] as its `dns_resolver`: the host is
//! resolved EXACTLY ONCE, the answer set is bounded, EVERY address is
//! classified (see [`crate::resolver::AddressClass`]) and the whole
//! resolution is refused when any answer is outside the permitted classes
//! (external-only by default; a local provider entry may carry the explicit
//! `allow_loopback` rule). The connector then connects to those vetted
//! `SocketAddr`s — there is no second resolution. Literal-IP URLs never
//! reach a resolver, so [`check_url`] classifies a literal host directly.
//!
//! Semantics (identical to the security crate, with the policy REQUIRED in
//! production):
//!
//! | policy state                              | outcome |
//! |-------------------------------------------|---------|
//! | production constructors (policy required) | allowlist governs |
//! | installed allowlist, rule matches         | allow |
//! | installed allowlist (even empty)          | deny before connect |
//! | no policy = test-only `permissive()`      | allow (never in production) |
//!
//! A denied destination returns a typed [`EgressError::Denied`] carrying
//! which rule fired and how far its match got — as a credential-free
//! [`SafeUrlDiagnostic`] (scheme, canonical host, effective port, path
//! SEGMENT COUNT; raw path bytes, userinfo, query and fragment are never
//! retained), never a raw URL. The authoritative check runs at `execute` time on the final request
//! object (so no caller can construct a request that bypasses the gate);
//! `get`/`post` validate the URL shape up front with typed errors, and
//! [`CheckedHttpClient::check`] exposes the same decision for callers that
//! want to refuse early. Production constructors build the client
//! internally (redirect policy applied last, resolver pinned) and never
//! accept or expose an arbitrary `reqwest::Client`; only
//! `cfg(test)`/`test-utils` constructors wrap an injected client.
//!
//! Provider egress is config-derived (a configured `base_url` is trusted
//! config, not prompt-derived data); [`validate_provider_base_url`] is the
//! config-load validator (URL must parse, scheme http(s), host present, no
//! userinfo). As defense in depth, provider adapters that adopt
//! `CheckedHttpClient` get the same request-time gate on every send.
//!
//! Provider adapter egress (P0-36): every adapter executes HTTP through the
//! [`HttpTransport`] seam — a `reqwest::Client` exists ONLY inside this
//! module ([`PolicyCheckedHttpTransport`] is the production implementation;
//! [`MockHttpTransport`] is the no-network test seam). Adapters receive an
//! `Arc<dyn HttpTransport>` at construction and never construct or execute
//! a raw client themselves; request building / raw `Client::execute` happen
//! only in [`execute_get`]/[`execute_post_json`] here, so the request-time
//! destination gate applies to every adapter send identically to wave-11
//! semantics (parsed scheme/host/port, deny before connect).
//!
//! # Redirects are hop-checked, never delegated to `reqwest`
//!
//! Every production client built here installs
//! [`reqwest::redirect::Policy::none()`]; [`CheckedHttpClient::execute`]
//! follows redirects itself, one request at a time, re-running the FULL
//! request-time gate — parsed destination policy plus the outbound secret
//! scan — on each hop's own URL/body before that hop is sent. A `Location`
//! pointing outside the allowlist (or at a non-http(s) scheme or a URL with
//! userinfo) is a typed [`EgressError`] refusal and the next hop is never
//! requested. At most [`MAX_REDIRECT_HOPS`] hops are followed; the next
//! redirect after that is [`EgressError::TooManyRedirects`]. Method/body
//! semantics mirror the previous reqwest-internal follower exactly:
//! 301/302 turn POST into GET (other methods keep method+body), 303 turns
//! everything but HEAD into GET (body and payload headers dropped), and
//! 307/308 preserve method+body (a streamed body cannot be replayed and is
//! refused typed rather than re-sent body-less). Credentials
//! (`authorization`, `cookie`, `cookie2`, `proxy-authorization`,
//! `www-authenticate`) are stripped whenever scheme, host or port changes;
//! they are kept on same-origin hops. Response bodies of intermediate hops
//! are dropped, so the response bound is per final hop. An injected client
//! that still follows redirects internally is detected on every response
//! (its final URL differs from the checked hop URL) and fails closed with
//! [`EgressError::UncheckedRedirectFollowed`].
//!
//! # Every response body is budgeted
//!
//! No production consumer reads a response body directly. [`BudgetedBody`]
//! enforces a caller-supplied [`ResponseBudget`] (head/idle/total deadlines
//! plus byte and frame caps) on every chunk; a stalled, dripping, oversized
//! or over-framed body is the typed
//! [`EgressError::ResponseBudgetExceeded`] naming the component that fired,
//! and [`execute_raw`] materializes only under a budget (its byte cap is
//! never above [`MAX_RAW_RESPONSE_BYTES`]). The provider streaming path, the
//! wire adapters, SCM, cloud OIDC, the updater, semantic and the commerce
//! connectors all pass one; the `static-authority` scan refuses direct
//! `.bytes()`/`.text()`/`.chunk()`/`bytes_stream()` reads in those trees.
//!
//! # Error size
//!
//! [`EgressError`] carries bounded, pre-redacted [`SafeUrlDiagnostic`]
//! values (scheme/host/port + a path SEGMENT COUNT, never path bytes)
//! rather than raw URLs, and the
//! diagnostic payload itself lives behind one `Box`, so the error — and
//! every `Result<_, EgressError>` in the transport seam — stays small
//! enough for `clippy::result_large_err` without any lint allowance.
//! Redaction is carried by the payload type, not by its inline size; no
//! error route is allowed to fall back to echoing a raw URL.

use futures::future::BoxFuture;
use futures::Stream;
use reqwest::header::{
    HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_ENCODING, CONTENT_LENGTH,
    CONTENT_TYPE, COOKIE, LOCATION, PROXY_AUTHORIZATION, TRANSFER_ENCODING, WWW_AUTHENTICATE,
};
use reqwest::{Body, Method, Request, RequestBuilder, Response, ResponseBuilderExt, Url};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub use crate::resolver::{AddressClass, EgressAddressPolicy};
use crate::resolver::{EgressResolveError, EgressResolver, ReqwestDnsResolver};
use crate::{ProviderError, ProviderErrorKind};
use faktor_security::destination::{Decision, DeniedReason, DestinationPolicy, RequestTarget};
use faktor_security::payload::{scan_payload, ScanOutcome, ScanPolicy};
use faktor_security::registry::SecretRegistry;

/// A known-safe label for one production egress route.
///
/// Labels exist so an error/log line can name WHICH route a call hit without
/// ever deriving text from an untrusted URL: every label is a compile-time
/// constant chosen by the calling code, while the URL contributes at most
/// the credential-free [`SafeUrlDiagnostic`] shape. A label can therefore
/// never smuggle a path, query, userinfo or fragment — even when the
/// request's URL is fully attacker-influenced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteLabel {
    /// GitHub App installation-token minting.
    GithubInstallationToken,
    /// The GitHub REST API surface.
    GithubApi,
    /// OIDC discovery document fetch.
    OidcDiscovery,
    /// OIDC JWKS fetch.
    OidcJwks,
    /// OIDC authorization-code token exchange.
    OidcTokenExchange,
    /// Billing vendor usage-report page.
    BillingVendorReport,
    /// Updater artifact/manifest download.
    UpdaterArtifact,
    /// An external semantic provider call.
    SemanticProvider,
    /// A commerce source connector call.
    CommerceSource,
    /// A worker's control-plane call.
    WorkerControlPlane,
    /// A provider chat/embedding stream.
    ProviderStream,
}

impl std::fmt::Display for RouteLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            RouteLabel::GithubInstallationToken => "github_installation_token",
            RouteLabel::GithubApi => "github_api",
            RouteLabel::OidcDiscovery => "oidc_discovery",
            RouteLabel::OidcJwks => "oidc_jwks",
            RouteLabel::OidcTokenExchange => "oidc_token_exchange",
            RouteLabel::BillingVendorReport => "billing_vendor_report",
            RouteLabel::UpdaterArtifact => "updater_artifact",
            RouteLabel::SemanticProvider => "semantic_provider",
            RouteLabel::CommerceSource => "commerce_source",
            RouteLabel::WorkerControlPlane => "worker_control_plane",
            RouteLabel::ProviderStream => "provider_stream",
        })
    }
}

/// Bounded, credential-free rendering of a URL for diagnostics.
///
/// Carries ONLY the scheme, the canonical parsed host, the effective port
/// (scheme default resolved), the NUMBER of path segments and whether a
/// query was present. Raw path bytes are never retained: a path routinely
/// carries credentials (`/token/<secret>`), and percent-encoding makes any
/// character-level masking heuristic unsafe. Userinfo is dropped, the query
/// is reduced to a presence marker (`?<redacted>`) so no query name or
/// value — plain or percent-encoded — can ever be echoed, and the fragment
/// is dropped. [`SafeUrlDiagnostic::from_raw`] never returns unparseable
/// input either: it reports `<unparseable>` instead. Both `Debug` and
/// `Display` produce this form, so no error route can regress into echoing
/// a credential-bearing URL.
///
/// An optional [`RouteLabel`] may be attached by the caller: it is a
/// compile-time constant that describes the route in words, so diagnostics
/// never have to derive a human label from (untrusted) URL text.
///
/// The fields live behind one `Box` so the value carried in every URL-bearing
/// [`EgressError`] variant (and therefore every `Result<_, EgressError>`)
/// stays small; diagnostics are constructed only on rejection paths, so the
/// single allocation is never on a success path. `Serialize`/`Deserialize`
/// are transparent over the box (the wire shape is the flat field object),
/// and `From`/`Display`/`Debug` ergonomics are untouched.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SafeUrlDiagnostic {
    fields: Box<SafeUrlFields>,
}

/// The boxed payload of a [`SafeUrlDiagnostic`].
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SafeUrlFields {
    scheme: String,
    host: String,
    port: u16,
    /// How many path segments the URL had. NEVER the segment bytes.
    path_segments: u16,
    query_present: bool,
    /// The caller-chosen known-safe route label, when one was attached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    route: Option<RouteLabel>,
}

impl SafeUrlDiagnostic {
    /// Diagnostic of an already-parsed URL (canonical host, port resolved).
    pub fn from_url(url: &Url) -> Self {
        // A lone `/` is the empty path: an absolute URL always carries one,
        // and only NON-EMPTY segments describe a real path shape.
        let segments = url
            .path_segments()
            .map(|segments| segments.filter(|segment| !segment.is_empty()).count())
            .unwrap_or(0);
        Self {
            fields: Box::new(SafeUrlFields {
                scheme: url.scheme().to_string(),
                host: url.host_str().unwrap_or("<no-host>").to_string(),
                port: url.port_or_known_default().unwrap_or(0),
                path_segments: segments.min(u16::MAX as usize) as u16,
                query_present: url.query().is_some(),
                route: None,
            }),
        }
    }

    /// Diagnostic of an already-parsed URL carrying a caller-chosen
    /// known-safe route label (display/Debug only; never derived from the
    /// URL itself).
    pub fn from_url_route(url: &Url, route: RouteLabel) -> Self {
        let mut diagnostic = Self::from_url(url);
        diagnostic.fields.route = Some(route);
        diagnostic
    }

    /// Diagnostic of a raw URL string. Parse failure yields a fixed
    /// `<unparseable>` marker — the hostile input is never echoed.
    pub fn from_raw(raw: &str) -> Self {
        match Url::parse(raw) {
            Ok(url) => Self::from_url(&url),
            Err(_) => Self {
                fields: Box::new(SafeUrlFields {
                    scheme: String::new(),
                    host: "<unparseable>".to_string(),
                    port: 0,
                    path_segments: 0,
                    query_present: false,
                    route: None,
                }),
            },
        }
    }

    /// Diagnostic of a raw URL string carrying a caller-chosen known-safe
    /// route label. Parse failure yields the same fixed `<unparseable>`
    /// marker as [`SafeUrlDiagnostic::from_raw`] (the label is still
    /// rendered: it is a compile-time constant, not URL text).
    pub fn from_raw_route(raw: &str, route: RouteLabel) -> Self {
        let mut diagnostic = Self::from_raw(raw);
        diagnostic.fields.route = Some(route);
        diagnostic
    }

    pub fn scheme(&self) -> &str {
        &self.fields.scheme
    }

    /// The canonical host (no userinfo, no brackets for IPv6).
    pub fn host(&self) -> &str {
        &self.fields.host
    }

    /// The effective connection port (`0` when unknown).
    pub fn port(&self) -> u16 {
        self.fields.port
    }

    /// How many path segments the URL carried (0 = no path). Never the
    /// segment bytes.
    pub fn path_segments(&self) -> u16 {
        self.fields.path_segments
    }

    pub fn query_present(&self) -> bool {
        self.fields.query_present
    }

    /// The caller-chosen known-safe route label, when one was attached.
    pub fn route(&self) -> Option<RouteLabel> {
        self.fields.route
    }
}

impl From<&str> for SafeUrlDiagnostic {
    fn from(raw: &str) -> Self {
        Self::from_raw(raw)
    }
}

impl From<String> for SafeUrlDiagnostic {
    fn from(raw: String) -> Self {
        Self::from_raw(&raw)
    }
}

impl std::fmt::Display for SafeUrlDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let fields = &self.fields;
        if fields.scheme.is_empty() {
            return write!(f, "{}", fields.host);
        }
        let host = if fields.host.contains(':') {
            format!("[{}]", fields.host)
        } else {
            fields.host.clone()
        };
        write!(f, "{}://{}:{}", fields.scheme, host, fields.port)?;
        // The path contributes only its shape: whether one existed, never a
        // byte of it (a path is a routine credential carrier).
        if fields.path_segments > 0 {
            write!(f, "/<redacted-path>")?;
        }
        if fields.query_present {
            write!(f, "?<redacted>")?;
        }
        if let Some(route) = fields.route {
            write!(f, " [{route}]")?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for SafeUrlDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}

/// Why a URL was rejected at the egress boundary. A typed reason only — the
/// hostile input, its query, userinfo or path text are NEVER carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UrlRejectReason {
    /// `Url::parse` refused the string.
    ParseError,
    /// The parsed URL has no host.
    MissingHost,
    /// The parsed URL has an empty host.
    EmptyHost,
    /// The URL carries userinfo (`user:pass@host`), which would smuggle
    /// credentials.
    UserinfoNotAllowed,
    /// Explicit port 0.
    PortZero,
    /// A redirect `Location` header is not valid header text.
    RedirectLocationNotText,
    /// A redirect `Location` does not resolve to a valid absolute URL.
    RedirectTargetUnparseable,
    /// A redirect target carries userinfo.
    RedirectUserinfoNotAllowed,
}

impl std::fmt::Display for UrlRejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            UrlRejectReason::ParseError => "not a parseable URL",
            UrlRejectReason::MissingHost => "URL has no host",
            UrlRejectReason::EmptyHost => "URL has an empty host",
            UrlRejectReason::UserinfoNotAllowed => "URL must not carry userinfo",
            UrlRejectReason::PortZero => "URL port 0 is invalid",
            UrlRejectReason::RedirectLocationNotText => {
                "redirect Location is not valid header text"
            }
            UrlRejectReason::RedirectTargetUnparseable => {
                "redirect Location does not resolve to a valid absolute URL"
            }
            UrlRejectReason::RedirectUserinfoNotAllowed => {
                "redirect target must not carry userinfo"
            }
        })
    }
}

/// Outbound full-body secret-scan configuration (audit P0-37/P0-38).
/// `None` on a client/transport = no secret scanning (historical
/// behaviour). When installed, every request body is scanned — the whole
/// payload via the security crate's streaming scanner plus the exact
/// configured-secret registry, when one is attached — BEFORE the
/// connection is attempted:
///
/// | scan outcome                              | behaviour |
/// |-------------------------------------------|-----------|
/// | `Clean`                                   | send |
/// | `TooLargeForPolicy` (absolute cap set)    | [`EgressError::BodyTooLarge`], no bytes sent |
/// | hits + `block_on_secret: true`            | [`EgressError::SecretBlocked`], no bytes sent |
/// | hits + `block_on_secret: false`           | warn and send (deny-allowed-by-policy stays) |
/// | body is a stream (not materialized)       | [`EgressError::BodyNotMaterialized`] — the full payload cannot be inspected; fail closed |
#[derive(Debug, Clone)]
pub struct OutboundScanConfig {
    /// The scan policy: overlap window plus optional absolute payload cap
    /// (`max_payload_bytes: Some(n)` makes an oversized body a typed
    /// `BodyTooLarge` denial — never silently clean).
    pub policy: ScanPolicy,
    /// `true` (default): any detected secret hard-blocks the send with
    /// [`EgressError::SecretBlocked`]. `false`: warn and send.
    pub block_on_secret: bool,
    /// Exact configured-secret registry scanned over the same body
    /// (fingerprints only; the registry never stores or prints plaintext).
    pub registry: Option<Arc<SecretRegistry>>,
}

impl Default for OutboundScanConfig {
    fn default() -> Self {
        OutboundScanConfig {
            policy: ScanPolicy::default(),
            block_on_secret: true,
            registry: None,
        }
    }
}

/// How adapter-level failures map onto provider errors. A policy/builder
/// refusal (denied destination, unsupported scheme, unparseable URL, bad
/// request object) is a NON-retryable `BadRequest` — retrying a denied
/// destination can never succeed and must not hammer the runtime. Only a
/// genuine transport failure (`Transport`) is a retryable `Network` error.
impl From<EgressError> for ProviderError {
    fn from(e: EgressError) -> Self {
        let message = e.to_string();
        match e {
            EgressError::Transport(_) => ProviderError::new(ProviderErrorKind::Network, message),
            // A stalled head/idle/overall read is a transient peer problem:
            // retryable like a transport failure. A byte/frame breach is a
            // hostile or broken response shape: never retried unchanged.
            EgressError::ResponseBudgetExceeded {
                component: BudgetComponent::Head | BudgetComponent::Idle | BudgetComponent::Total,
                ..
            } => ProviderError::new(ProviderErrorKind::Network, message),
            _ => ProviderError::new(ProviderErrorKind::BadRequest, message),
        }
    }
}

/// The single outbound-HTTP seam every adapter must execute through.
///
/// An implementation takes an already-built [`reqwest::Request`] and runs it
/// (usually through the parsed-destination gate first) and hands back the
/// response *with its body still streaming* — adapters then consume the
/// response body exactly as they consumed a `reqwest::Client::execute`
/// response before, so SSE/NDJSON streaming behavior is unchanged.
///
/// Implementations MUST be `Send + Sync` (providers are shared across
/// runtime tasks). `reqwest::Client` construction, request building and raw
/// `Client::execute` calls live ONLY inside this module; adapter code never
/// names a `reqwest::Client` (certified by the source-level test
/// `no_raw_client_execute_outside_egress`).
pub trait HttpTransport: Send + Sync {
    /// Execute one built request. The response body is a live stream; call
    ///ers consume it (never buffer it here).
    fn execute(&self, req: Request) -> BoxFuture<'_, Result<Response, EgressError>>;
}

/// The policy-checked production transport: [`CheckedHttpClient`] behind the
/// [`HttpTransport`] seam. `policy: None` = no destination policy installed
/// = default-allow (documented, wave-11 semantics); `Some(policy)` = the
/// allowlist governs every request on its parsed URL before any connect.
#[derive(Debug, Clone)]
pub struct PolicyCheckedHttpTransport {
    inner: CheckedHttpClient,
}

impl PolicyCheckedHttpTransport {
    /// TEST-ONLY: explicit allowlist + loopback address rule.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn with_policy_for_tests(policy: DestinationPolicy) -> Self {
        Self {
            inner: CheckedHttpClient::with_policy_for_tests(policy),
        }
    }

    /// TEST-ONLY: explicit allowlist + scan + loopback address rule.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn with_policy_and_scan_for_tests(
        policy: DestinationPolicy,
        outbound_scan: Option<OutboundScanConfig>,
    ) -> Self {
        Self {
            inner: CheckedHttpClient::with_policy_and_scan_for_tests(policy, outbound_scan),
        }
    }

    /// The test-only permissive transport: no destination allowlist
    /// installed and the explicit loopback address rule (mock servers are
    /// loopback). Gated behind `cfg(test)` or the `test-utils` feature
    /// (enabled solely by dependent crates' dev-dependencies), so a
    /// production build has no default-allow constructor to call — the
    /// daemon must inject the policy-checked transport.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn permissive() -> Self {
        Self {
            inner: CheckedHttpClient::permissive(),
        }
    }

    /// A transport with the given allowlist installed. The policy is
    /// REQUIRED: there is no production default-allow constructor, and the
    /// address-class rule defaults to external-only (loopback refused).
    /// Construction is fallible: a failed client build surfaces typed
    /// ([`EgressError::ClientBuild`]) and the daemon propagates it as a
    /// startup failure instead of running a degraded client.
    pub fn try_with_policy(policy: DestinationPolicy) -> Result<Self, EgressError> {
        Ok(Self {
            inner: CheckedHttpClient::try_with_policy(policy)?,
        })
    }

    /// A transport with the given allowlist AND an installed outbound
    /// secret scan (audit P0-37/P0-38): every request body passes the
    /// full-payload scan before any connect (see [`OutboundScanConfig`]).
    pub fn try_with_policy_and_scan(
        policy: DestinationPolicy,
        outbound_scan: Option<OutboundScanConfig>,
    ) -> Result<Self, EgressError> {
        Ok(Self {
            inner: CheckedHttpClient::try_with_policy_and_scan(policy, outbound_scan)?,
        })
    }

    /// A transport with an explicit address-class rule in addition to the
    /// allowlist and scan. The only production use is a LOCAL provider
    /// (Ollama) whose typed config carries `allow_loopback`: every other
    /// special class stays refused, so no address class is ever allowed
    /// globally.
    pub fn try_with_policy_scan_and_addresses(
        policy: DestinationPolicy,
        outbound_scan: Option<OutboundScanConfig>,
        addresses: EgressAddressPolicy,
    ) -> Result<Self, EgressError> {
        Ok(Self {
            inner: CheckedHttpClient::try_with_policy_scan_and_addresses(
                policy,
                outbound_scan,
                addresses,
            )?,
        })
    }

    /// The installed outbound secret scan (`None` = no secret scanning).
    pub fn outbound_scan(&self) -> Option<&OutboundScanConfig> {
        self.inner.outbound_scan()
    }

    /// The installed allowlist (`None` only for test-only permissive
    /// construction).
    pub fn policy(&self) -> Option<&DestinationPolicy> {
        self.inner.policy()
    }

    /// The installed address-class rule.
    pub fn address_policy(&self) -> EgressAddressPolicy {
        self.inner.address_policy()
    }
}

impl HttpTransport for PolicyCheckedHttpTransport {
    fn execute(&self, req: Request) -> BoxFuture<'_, Result<Response, EgressError>> {
        let inner = self.inner.clone();
        Box::pin(async move { inner.execute(req).await })
    }
}

/// `CheckedHttpClient` itself is a valid transport (it already encapsulates
/// policy + client and can yield streaming responses) — callers that hold
/// one can hand it to adapters directly.
impl HttpTransport for CheckedHttpClient {
    fn execute(&self, req: Request) -> BoxFuture<'_, Result<Response, EgressError>> {
        let inner = self.clone();
        Box::pin(async move { inner.execute(req).await })
    }
}

/// The adapter-standard client: connect-timeout only (the streaming hang
/// controls live in the adapter transport guards, never here), the central
/// egress resolver pinned as the DNS seam (one resolution per host, every
/// answer class-checked, connect to the vetted address set), and redirects
/// DISABLED — [`CheckedHttpClient::execute`] follows redirects itself so
/// every hop passes the destination/scan gate. The redirect policy is
/// applied LAST and is not overridable by callers.
///
/// Construction is FALLIBLE and there is exactly ONE build: no fallback
/// client exists. The previous `unwrap_or_else` fallback silently dropped
/// the connect timeout (and any future builder knob) on a build failure;
/// every failure now surfaces as the typed
/// [`EgressError::ClientBuild`] and the daemon refuses to run without a
/// correctly-gated client instead of running with a degraded one.
fn try_default_timeout_client(resolver: EgressResolver) -> Result<reqwest::Client, EgressError> {
    reqwest::Client::builder()
        .dns_resolver(Arc::new(ReqwestDnsResolver::new(resolver)))
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| EgressError::ClientBuild {
            // `without_url` keeps even the build error URL-free; the
            // message carries only the builder's typed reason.
            detail: e.without_url().to_string(),
        })
}

/// Execute a GET through a transport. Request building and the raw
/// `Client::execute` call happen only here (adapter code never constructs a
/// `reqwest::Client`). The returned response is a stream: every production
/// consumer reads it through [`BudgetedBody`] under a [`ResponseBudget`]
/// (the static-authority scan refuses direct body reads).
pub async fn execute_get(
    transport: &dyn HttpTransport,
    url: &str,
) -> Result<Response, EgressError> {
    let parsed = parse_fetch_url(url)?;
    transport.execute(Request::new(Method::GET, parsed)).await
}

/// Execute a JSON POST through a transport (no extra headers). The returned
/// response is a stream: read it through [`BudgetedBody`] under a
/// [`ResponseBudget`].
pub async fn execute_post_json(
    transport: &dyn HttpTransport,
    url: &str,
    headers: HeaderMap,
    body: &serde_json::Value,
) -> Result<Response, EgressError> {
    execute_post_json_with_extras(
        transport,
        url,
        headers,
        &crate::config::ExtraHeaders::empty(),
        body,
    )
    .await
}

/// Execute a JSON POST through a transport with validated extra headers
/// applied OVER `headers` (per-name replace, exactly the semantics the
/// gateway path relied on when it layered extra headers after the auth
/// headers). The extra headers were validated at configuration time
/// ([`crate::config::ExtraHeaders`]), so an invalid one can never be
/// silently dropped here. The content-type is forced to
/// `application/json` last, exactly like `RequestBuilder::json` did.
pub async fn execute_post_json_with_extras(
    transport: &dyn HttpTransport,
    url: &str,
    headers: HeaderMap,
    extra_headers: &crate::config::ExtraHeaders,
    body: &serde_json::Value,
) -> Result<Response, EgressError> {
    let parsed = parse_fetch_url(url)?;
    let mut request = Request::new(Method::POST, parsed);
    *request.headers_mut() = headers;
    extra_headers.apply(request.headers_mut());
    request
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    let json = serde_json::to_string(body).map_err(|e| EgressError::Build(e.to_string()))?;
    *request.body_mut() = Some(Body::from(json));
    transport.execute(request).await
}

/// Which [`ResponseBudget`] component fired on an over-budget body read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetComponent {
    /// No first body chunk inside `head_timeout`.
    Head,
    /// Silence between chunks exceeded `idle_timeout`.
    Idle,
    /// The whole read outlived `total_deadline`.
    Total,
    /// The body crossed `max_bytes`.
    Bytes,
    /// The body carried more than `max_frames` frames.
    Frames,
}

impl BudgetComponent {
    /// The limit's unit for diagnostics (`ms`, `bytes`, `frames`).
    pub const fn unit(self) -> &'static str {
        match self {
            BudgetComponent::Head | BudgetComponent::Idle | BudgetComponent::Total => "ms",
            BudgetComponent::Bytes => "bytes",
            BudgetComponent::Frames => "frames",
        }
    }
}

impl std::fmt::Display for BudgetComponent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            BudgetComponent::Head => "head-timeout",
            BudgetComponent::Idle => "idle-timeout",
            BudgetComponent::Total => "total-deadline",
            BudgetComponent::Bytes => "max-bytes",
            BudgetComponent::Frames => "max-frames",
        })
    }
}

/// The typed budget ONE response-body read must obey.
///
/// Every production [`HttpTransport`] consumer passes one to the read
/// helpers ([`BudgetedBody`], [`execute_raw`]), which enforce it against the
/// live response stream; a breach is the typed
/// [`EgressError::ResponseBudgetExceeded`] naming the component that fired:
///
/// | component        | enforced as |
/// |------------------|-------------|
/// | `head_timeout`   | time from read start to the FIRST body chunk |
/// | `idle_timeout`   | maximum silence between consecutive chunks |
/// | `total_deadline` | overall wall clock for the whole read |
/// | `max_bytes`      | hard byte cap; the chunk that would cross it is refused |
/// | `max_frames`     | hard frame (network chunk) cap when `Some` |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseBudget {
    pub head_timeout: Duration,
    pub idle_timeout: Duration,
    pub total_deadline: Duration,
    pub max_bytes: u64,
    pub max_frames: Option<u64>,
}

impl ResponseBudget {
    /// An explicit budget.
    pub const fn new(
        head_timeout: Duration,
        idle_timeout: Duration,
        total_deadline: Duration,
        max_bytes: u64,
        max_frames: Option<u64>,
    ) -> Self {
        Self {
            head_timeout,
            idle_timeout,
            total_deadline,
            max_bytes,
            max_frames,
        }
    }

    /// A one-wall-clock budget: head, idle and total all equal `timeout`
    /// (the shape a single-`timeout` consumer such as an OIDC round trip
    /// needs), with an explicit byte cap and no frame cap.
    pub const fn for_timeout(timeout: Duration, max_bytes: u64) -> Self {
        Self::new(timeout, timeout, timeout, max_bytes, None)
    }

    /// The millisecond spelling of [`ResponseBudget::new`].
    pub const fn from_millis(
        head_ms: u64,
        idle_ms: u64,
        total_ms: u64,
        max_bytes: u64,
        max_frames: Option<u64>,
    ) -> Self {
        Self::new(
            Duration::from_millis(head_ms),
            Duration::from_millis(idle_ms),
            Duration::from_millis(total_ms),
            max_bytes,
            max_frames,
        )
    }
}

/// A response body wrapper that enforces its [`ResponseBudget`] on every
/// chunk: the ONE way production consumers read a body.
///
/// `next_chunk` is the choke point: it bounds the wait for each chunk by the
/// head/idle and remaining-total budgets (a `tokio::time::timeout` per
/// chunk, so a stalled peer cannot park the read), refuses the chunk that
/// would cross `max_bytes` or `max_frames` BEFORE it is handed out, and maps
/// a transport failure to [`EgressError::Transport`] with `without_url()`.
/// After any terminal error the wrapper is dead: the next call returns
/// `Ok(None)` and a wrapped stream ends, so exactly one typed error is
/// emitted.
pub struct BudgetedBody {
    response: Response,
    budget: ResponseBudget,
    started: Instant,
    last_activity: Option<Instant>,
    bytes: u64,
    frames: u64,
    finished: bool,
}

impl BudgetedBody {
    /// Wrap a streaming response under `budget`.
    pub fn new(response: Response, budget: ResponseBudget) -> Self {
        Self {
            response,
            budget,
            started: Instant::now(),
            last_activity: None,
            bytes: 0,
            frames: 0,
            finished: false,
        }
    }

    /// The response's parsed URL (for diagnostics).
    pub fn url(&self) -> &Url {
        self.response.url()
    }

    /// The installed budget.
    pub fn budget(&self) -> ResponseBudget {
        self.budget
    }

    /// Bytes handed out so far.
    pub fn bytes_seen(&self) -> u64 {
        self.bytes
    }

    /// Frames handed out so far.
    pub fn frames_seen(&self) -> u64 {
        self.frames
    }

    fn exceeded(&self, component: BudgetComponent, limit: u64) -> EgressError {
        EgressError::ResponseBudgetExceeded {
            url: SafeUrlDiagnostic::from_url(self.response.url()),
            component,
            limit,
        }
    }

    /// The next budget-checked chunk (`None` = body ended). See the type
    /// docs for the enforcement contract.
    pub async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, EgressError> {
        if self.finished {
            return Ok(None);
        }
        let remaining = self
            .budget
            .total_deadline
            .saturating_sub(self.started.elapsed());
        let total_limit_ms = self.budget.total_deadline.as_millis() as u64;
        if remaining.is_zero() {
            self.finished = true;
            return Err(self.exceeded(BudgetComponent::Total, total_limit_ms));
        }
        let (phase, component) = match self.last_activity {
            Some(_) => (self.budget.idle_timeout, BudgetComponent::Idle),
            None => (self.budget.head_timeout, BudgetComponent::Head),
        };
        let wait = phase.min(remaining);
        match tokio::time::timeout(wait, self.response.chunk()).await {
            Err(_) => {
                self.finished = true;
                if remaining <= wait {
                    Err(self.exceeded(BudgetComponent::Total, total_limit_ms))
                } else {
                    Err(self.exceeded(component, phase.as_millis() as u64))
                }
            }
            Ok(Err(e)) => {
                self.finished = true;
                Err(EgressError::Transport(e.without_url().to_string()))
            }
            Ok(Ok(None)) => {
                self.finished = true;
                Ok(None)
            }
            Ok(Ok(Some(chunk))) => {
                let len = chunk.len() as u64;
                if self.bytes.saturating_add(len) > self.budget.max_bytes {
                    self.finished = true;
                    return Err(self.exceeded(BudgetComponent::Bytes, self.budget.max_bytes));
                }
                self.frames = self.frames.saturating_add(1);
                if let Some(max_frames) = self.budget.max_frames {
                    if self.frames > max_frames {
                        self.finished = true;
                        return Err(self.exceeded(BudgetComponent::Frames, max_frames));
                    }
                }
                self.bytes = self.bytes.saturating_add(len);
                self.last_activity = Some(Instant::now());
                Ok(Some(chunk.to_vec()))
            }
        }
    }

    /// Read the whole body under the budget (the materializing helper).
    pub async fn read_all(&mut self) -> Result<Vec<u8>, EgressError> {
        let mut body: Vec<u8> = Vec::new();
        while let Some(chunk) = self.next_chunk().await? {
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    /// Turn the budgeted reader into a stream of budget-checked chunks. The
    /// stream yields exactly one terminal error (if any) and then ends.
    pub fn into_stream(self) -> impl Stream<Item = Result<Vec<u8>, EgressError>> + Send + 'static {
        futures::stream::unfold(Some(self), |state| async move {
            let mut body = state?;
            match body.next_chunk().await {
                Ok(Some(chunk)) => Some((Ok(chunk), Some(body))),
                Ok(None) => None,
                Err(e) => Some((Err(e), None)),
            }
        })
    }
}

/// Hard bound on one materialized [`RawResponse`] body: an adapter that
/// needs a verb/header shape beyond the JSON helpers still must not buffer
/// an unbounded remote body in RAM (bounded everything). The effective read
/// budget is `min(caller max_bytes, this)`; the read aborts with the typed
/// [`EgressError::ResponseBudgetExceeded`] (`Bytes`) at the bound.
pub const MAX_RAW_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// One neutral request description for adapters (e.g. the SCM GitHub-App
/// adapter) that need verbs, headers and bodies beyond the JSON POST
/// helpers. Building and the raw execution happen ONLY here, so a
/// non-provider adapter never names a `reqwest` type and every send still
/// passes the request-time destination gate of the transport it is handed.
///
/// `Debug` is REDACTING, and the policy is deny-by-default: the URL renders
/// through [`SafeUrlDiagnostic`] (path bytes, userinfo, query values and
/// fragment are never echoed), EVERY header value is `<redacted; N bytes>`
/// except a tiny known-safe allowlist (`content-type`, `content-length`,
/// `accept`, `user-agent` — none of which can carry a credential), and the
/// body is reported by length only. A name-based "looks sensitive" heuristic
/// is deliberately NOT used: an arbitrary custom header (`X-Session-ID`,
/// `X-Signature`, ...) must be redacted without having to guess its name.
/// `Clone`/`PartialEq` carry the real values unchanged.
#[derive(Clone, PartialEq, Eq)]
pub struct RawRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    /// Optional caller-chosen known-safe route label (never derived from
    /// the URL) for diagnostics.
    pub route: Option<RouteLabel>,
}

/// The tiny allowlist of header names whose values may render verbatim in a
/// [`RawRequest`]'s `Debug`: request-shape metadata that carries no user or
/// machine credential. Everything else is redacted by DEFAULT — the
/// inverted policy means a genuinely credential-bearing custom header
/// (`x-signature`, `x-session-id`, `x-anything`) cannot leak just because no
/// heuristic named it.
const SAFE_HEADER_VALUE_NAMES: [&str; 4] =
    ["content-type", "content-length", "accept", "user-agent"];

/// True when the header value may render verbatim (see
/// [`SAFE_HEADER_VALUE_NAMES`]).
fn header_value_is_safe(name: &str) -> bool {
    SAFE_HEADER_VALUE_NAMES
        .iter()
        .any(|safe| safe.eq_ignore_ascii_case(name))
}

/// One redacted header rendering: safe-named values verbatim, every other
/// value as `<redacted; N bytes>` (length only, never bytes).
fn render_header(name: &str, value: &str) -> String {
    if header_value_is_safe(name) {
        format!("{name}: {value}")
    } else {
        format!("{name}: <redacted; {} bytes>", value.len())
    }
}

impl std::fmt::Debug for RawRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let headers: Vec<String> = self
            .headers
            .iter()
            .map(|(name, value)| render_header(name, value))
            .collect();
        let url = match self.route {
            Some(route) => SafeUrlDiagnostic::from_raw_route(&self.url, route),
            None => SafeUrlDiagnostic::from_raw(&self.url),
        };
        let mut debug = f.debug_struct("RawRequest");
        debug
            .field("method", &self.method)
            .field("url", &url)
            .field("headers", &headers)
            .field(
                "body",
                &self
                    .body
                    .as_ref()
                    .map(|body| format!("<{} bytes>", body.len())),
            );
        if let Some(route) = self.route {
            debug.field("route", &route);
        }
        debug.finish()
    }
}

impl RawRequest {
    pub fn new(method: &str, url: impl Into<String>) -> Self {
        Self {
            method: method.to_string(),
            url: url.into(),
            headers: Vec::new(),
            body: None,
            route: None,
        }
    }

    /// Add one header (invalid names/values are a typed build refusal when
    /// the request executes).
    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    /// Attach the known-safe route label for diagnostics (a compile-time
    /// constant; never derived from the URL).
    pub fn route(mut self, route: RouteLabel) -> Self {
        self.route = Some(route);
        self
    }

    /// Attach an application/json body. Encoding failure is a typed build
    /// refusal: the request is NEVER silently sent with a defaulted empty
    /// body (the old `unwrap_or_default` sent `{}`-less bytes under a
    /// content-type that promised JSON).
    pub fn json_body(mut self, body: &serde_json::Value) -> Result<Self, EgressError> {
        let encoded = serde_json::to_vec(body)
            .map_err(|e| EgressError::Build(format!("json body encoding failed: {e}")))?;
        self.headers
            .push(("content-type".to_string(), "application/json".to_string()));
        self.body = Some(encoded);
        Ok(self)
    }

    /// Attach raw bytes.
    pub fn bytes_body(mut self, body: Vec<u8>) -> Self {
        self.body = Some(body);
        self
    }
}

/// One materialized response: status, headers (lossy-ASCII names/values
/// only, others dropped), bounded body bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl RawResponse {
    /// The first header value with this (case-insensitive) name.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The body decoded as UTF-8 (lossy) — for error excerpts only.
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// Execute one [`RawRequest`] through the injected transport and materialize
/// the response under the caller's [`ResponseBudget`] (its `max_bytes` is
/// itself capped by [`MAX_RAW_RESPONSE_BYTES`]). Adapter code never touches
/// `reqwest` directly; this is the ONLY execution site for non-JSON-POST
/// shapes, and the budget is a REQUIRED parameter so no production consumer
/// can read a body without one.
pub async fn execute_raw(
    transport: &dyn HttpTransport,
    request: RawRequest,
    budget: &ResponseBudget,
) -> Result<RawResponse, EgressError> {
    let method = Method::from_bytes(request.method.as_bytes())
        .map_err(|e| EgressError::Build(format!("invalid method {:?}: {e}", request.method)))?;
    let url = parse_fetch_url(&request.url)?;
    let mut built = Request::new(method, url);
    for (name, value) in &request.headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|e| EgressError::Build(format!("invalid header name: {e}")))?;
        let value = HeaderValue::from_str(value)
            .map_err(|e| EgressError::Build(format!("invalid header value: {e}")))?;
        built.headers_mut().insert(name, value);
    }
    if let Some(body) = request.body {
        *built.body_mut() = Some(Body::from(body));
    }
    let response = transport.execute(built).await?;
    let status = response.status().as_u16();
    let headers: Vec<(String, String)> = response
        .headers()
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str().to_string(), value.to_string()))
        })
        .collect();
    // The hard materialization cap always applies: a caller can lower its
    // own byte bound, never raise the seam's absolute one.
    let effective = ResponseBudget {
        max_bytes: budget.max_bytes.min(MAX_RAW_RESPONSE_BYTES as u64),
        ..*budget
    };
    let body = BudgetedBody::new(response, effective).read_all().await?;
    Ok(RawResponse {
        status,
        headers,
        body,
    })
}

/// Test-seam transport: never touches the network. Returns a canned
/// response body (status + text) for every executed request and records the
/// executed (method, url) pairs, so an adapter's parser can be driven
/// without real HTTP. Optional deny mode returns a canned [`EgressError`]
/// before any response exists.
pub struct MockHttpTransport {
    status: reqwest::StatusCode,
    body: String,
    deny: Option<EgressError>,
    requests: std::sync::Mutex<Vec<(String, String)>>,
}

impl MockHttpTransport {
    /// A transport that answers every request with `status` + `body`.
    pub fn new(status: u16, body: impl Into<String>) -> Self {
        Self {
            status: reqwest::StatusCode::from_u16(status).unwrap_or(reqwest::StatusCode::OK),
            body: body.into(),
            deny: None,
            requests: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// A transport that refuses every request with `deny` (before any
    /// response could exist — exercises the adapters' error mapping with no
    /// HTTP involved).
    pub fn denying(deny: EgressError) -> Self {
        Self {
            status: reqwest::StatusCode::OK,
            body: String::new(),
            deny: Some(deny),
            requests: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// How many requests were executed through this transport.
    pub fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    /// Executed (method, url) pairs in order.
    pub fn requests(&self) -> Vec<(String, String)> {
        self.requests.lock().unwrap().clone()
    }
}

impl HttpTransport for MockHttpTransport {
    fn execute(&self, req: Request) -> BoxFuture<'_, Result<Response, EgressError>> {
        self.requests
            .lock()
            .unwrap()
            .push((req.method().to_string(), req.url().to_string()));
        let status = self.status;
        let body = self.body.clone();
        let deny = self.deny.clone();
        let url = req.url().clone();
        Box::pin(async move {
            if let Some(err) = deny {
                return Err(err);
            }
            let response = http::Response::builder()
                .status(status)
                .url(url)
                .body(Body::from(body))
                .map_err(|e| EgressError::Build(e.to_string()))?;
            Ok(Response::from(response))
        })
    }
}

/// Why an outbound call refused to leave the process.
///
/// Every URL-bearing variant stores a [`SafeUrlDiagnostic`] (scheme,
/// canonical host, effective port, bounded path; query masked, userinfo and
/// fragment dropped) — never a raw URL string. Transport errors are
/// stringified only after `reqwest::Error::without_url()`, so a credential
/// planted in a query, userinfo, path or `Location` header can never leak
/// through `Debug`, `Display` or the `ProviderError` conversion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EgressError {
    /// The installed allowlist denied the destination before any connect.
    /// `url` is the credential-free parsed-target diagnostic.
    Denied {
        url: SafeUrlDiagnostic,
        reason: DeniedReason,
    },
    /// The outbound secret scan found hits and `block_on_secret` is set.
    /// `kinds` carries only canonical kind labels (never snippets or
    /// candidate bytes), so logs cannot leak secret material.
    SecretBlocked { kinds: Vec<String> },
    /// The request body exceeded the scan policy's `max_payload_bytes`;
    /// the unscanned suffix must never be treated as clean (fail closed
    /// before any connect).
    BodyTooLarge { limit_bytes: u64 },
    /// Secret scanning is configured but the request body is a stream that
    /// is not fully materialized, so the FULL payload cannot be inspected.
    /// Fail closed: refused before any connect.
    BodyNotMaterialized(SafeUrlDiagnostic),
    /// The URL scheme is not fetchable through this seam (only http/https
    /// are; a `ws`/`wss` policy may exist for websocket tools, but this
    /// client does not speak them).
    UnsupportedScheme(String),
    /// The URL (or provider base URL) does not parse / is not a valid
    /// absolute http(s) URL. Carries the typed reason only — never the
    /// hostile input.
    UnparseableUrl(UrlRejectReason),
    /// The destination's IP sits in a class the [`EgressAddressPolicy`]
    /// refuses (loopback/private/link-local/CGNAT/documentation/...).
    /// A literal-IP URL is classified at check time; a hostname is
    /// classified by the resolver at connect time. Refused before any
    /// connect.
    AddressClassRefused {
        url: SafeUrlDiagnostic,
        class: AddressClass,
    },
    /// The resolver answered with more addresses than the configured bound
    /// (a hostile or broken DNS answer). Refused before any connect.
    DnsAnswerSetTooLarge {
        url: SafeUrlDiagnostic,
        count: usize,
        limit: usize,
    },
    /// Building the request object failed (URL-free message).
    Build(String),
    /// Building the checked HTTP client itself failed (TLS backend, resolver
    /// pinning, ...). A typed STARTUP failure: the daemon refuses to run
    /// rather than fall back to a degraded client that could skip the
    /// redirect or resolver gate. URL-free by construction.
    ClientBuild { detail: String },
    /// One response body read exceeded its [`ResponseBudget`]. The typed
    /// component distinguishes a stalled head (`Head`), a stalled body
    /// (`Idle`), an overall deadline (`Total`), a byte cap (`Bytes`) and a
    /// frame cap (`Frames`); `limit` is in ms, bytes or frames respectively.
    ResponseBudgetExceeded {
        url: SafeUrlDiagnostic,
        component: BudgetComponent,
        limit: u64,
    },
    /// The transport itself failed (connect/io); the request was allowed
    /// by the policy. The message is URL-free by construction (reqwest
    /// errors are stripped with `without_url()` before stringifying).
    Transport(String),
    /// A materialized response exceeded an absolute materialization bound
    /// (e.g. [`MAX_RAW_RESPONSE_BYTES`]); the adapter refuses to buffer it
    /// (bounded everything), and the partial body is discarded rather than
    /// parsed. Budget-driven reads raise
    /// [`EgressError::ResponseBudgetExceeded`] instead.
    ResponseTooLarge { limit_bytes: u64 },
    /// A redirect chain exceeded [`MAX_REDIRECT_HOPS`]. Each hop is a fresh
    /// policy-checked request, so a chain that long is refused typed; the
    /// hop that would exceed the bound is never sent.
    TooManyRedirects {
        limit: usize,
        url: SafeUrlDiagnostic,
    },
    /// A 307/308 redirect (or a non-POST 301/302) would have to replay a
    /// body that is not materialized (a stream); refused typed instead of
    /// silently re-sending it body-less or converting it to GET.
    RedirectBodyNotReplayable { url: SafeUrlDiagnostic },
    /// The wrapped `reqwest::Client` followed a redirect itself (its
    /// builder installed something other than
    /// [`reqwest::redirect::Policy::none()`]), so the final hop never
    /// passed the per-hop destination gate. Fail closed: the caller must
    /// not consume a response obtained through an unchecked redirect.
    UncheckedRedirectFollowed {
        from: SafeUrlDiagnostic,
        to: SafeUrlDiagnostic,
    },
}

impl std::fmt::Display for EgressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EgressError::Denied { url, reason } => {
                write!(f, "egress to {url} denied: {reason}")
            }
            EgressError::SecretBlocked { kinds } => {
                write!(
                    f,
                    "egress denied before connect: secret scan blocked the request body ({})",
                    kinds.join(", ")
                )
            }
            EgressError::BodyTooLarge { limit_bytes } => {
                write!(
                    f,
                    "egress denied before connect: request body exceeds the secret-scan \
                     limit of {limit_bytes} bytes; the unscanned suffix would be sent \
                     unverified (raise the policy cap explicitly to allow)"
                )
            }
            EgressError::BodyNotMaterialized(url) => {
                write!(
                    f,
                    "egress denied before connect: cannot scan the full body of {url} \
                     (streamed bodies are not materialized); full-payload scanning \
                     requires a materialized body"
                )
            }
            EgressError::UnsupportedScheme(s) => {
                write!(
                    f,
                    "egress denied: unsupported scheme {s:?} (http/https only)"
                )
            }
            EgressError::UnparseableUrl(reason) => write!(f, "{reason}"),
            EgressError::AddressClassRefused { url, class } => write!(
                f,
                "egress denied before connect: {url} is a {class} address, which the egress \
                 address policy refuses"
            ),
            EgressError::DnsAnswerSetTooLarge { url, count, limit } => write!(
                f,
                "egress denied before connect: DNS for {url} answered with {count} addresses, \
                 over the bound of {limit}"
            ),
            EgressError::Build(s) => write!(f, "request build failed: {s}"),
            EgressError::ClientBuild { detail } => {
                write!(f, "egress client construction failed: {detail}")
            }
            EgressError::ResponseBudgetExceeded {
                url,
                component,
                limit,
            } => write!(
                f,
                "egress response read of {url} exceeded the {component} budget \
                 ({limit} {})",
                component.unit()
            ),
            EgressError::Transport(s) => write!(f, "transport error: {s}"),
            EgressError::ResponseTooLarge { limit_bytes } => write!(
                f,
                "response body exceeds the adapter materialization bound of {limit_bytes} bytes"
            ),
            EgressError::TooManyRedirects { limit, url } => write!(
                f,
                "egress denied: {url} exceeded the checked redirect bound of {limit} hops \
                 (every hop is re-validated, so the chain is refused)"
            ),
            EgressError::RedirectBodyNotReplayable { url } => write!(
                f,
                "egress denied: redirect from {url} would replay a streamed (non-materialized) \
                 request body; the body cannot be re-sent and is not silently dropped"
            ),
            EgressError::UncheckedRedirectFollowed { from, to } => write!(
                f,
                "egress denied: the wrapped HTTP client followed a redirect itself \
                 ({from} -> {to}) without the checked hop gate; install a \
                 Policy::none() client"
            ),
        }
    }
}

impl std::error::Error for EgressError {}

/// The request-time destination gate, shared by the checked client and any
/// adapter that wants the defense-in-depth check before its own send.
///
/// `policy` is REQUIRED in production (the explicit allowlist governs;
/// default-deny on no full scheme+host+port match) — the test-only
/// permissive constructors install no allowlist.
///
/// Literal-IP hosts and hostnames are handled differently, deliberately:
///
/// - A literal-IP host is not rebindable — the URL itself names the address
///   — so the allowlist decision is the operator's explicit choice and is
///   authoritative. `http://127.0.0.1:11434` is reachable only when a rule
///   allows exactly that destination (a hostile name cannot become it).
/// - A hostname is checked by name here and class-checked at connect time
///   by the central resolver (see [`crate::resolver`]): the name must
///   resolve through the ONE vetted resolution, every answer must be in a
///   permitted class (external-only by default; the explicit
///   `allow_loopback` rule for local endpoints), and the connect uses only
///   those vetted addresses. That is where loopback/private/link-local/
///   CGNAT/documentation answers are refused.
pub fn check_url(policy: &DestinationPolicy, url: &Url) -> Result<(), EgressError> {
    check_url_with(Some(policy), url)
}

/// The shared implementation behind [`check_url`] and
/// [`CheckedHttpClient::check`] (the latter may hold no allowlist when built
/// by a test-only permissive constructor).
fn check_url_with(policy: Option<&DestinationPolicy>, url: &Url) -> Result<(), EgressError> {
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(EgressError::UnsupportedScheme(scheme.to_string()));
    }
    let Some(host) = url.host_str() else {
        return Err(EgressError::UnparseableUrl(UrlRejectReason::MissingHost));
    };
    if host.is_empty() {
        return Err(EgressError::UnparseableUrl(UrlRejectReason::EmptyHost));
    }
    // Pull scheme/host/port from the PARSED url — never from strings. When
    // the URL layer already parsed an IPv4 address, its octets beat any
    // textual spelling, so `127.0.0.01`-style variants compare canonical.
    // (The URL crate emits canonical dotted text for IPv4 hosts, so a
    // parse of the host text is an equivalent detector here.)
    let (is_ipv4, ip) = match host.parse::<std::net::Ipv4Addr>() {
        Ok(v4) => (true, Some(v4.octets())),
        Err(_) => (false, None),
    };
    let explicit_port = url.port();
    let target = RequestTarget::from_parts(Some(scheme), host, explicit_port, is_ipv4, ip)
        .map_err(|_| EgressError::UnparseableUrl(UrlRejectReason::ParseError))?;
    if let Some(policy) = policy {
        if let Decision::Denied(reason) = target.check_against(policy) {
            return Err(EgressError::Denied {
                url: SafeUrlDiagnostic::from_url(url),
                reason,
            });
        }
    }
    Ok(())
}

/// `reqwest::Client` whose every send first passes the parsed destination
/// gate — and, when an [`OutboundScanConfig`] is installed, a full-body
/// secret scan of the FINAL request object (audit P0-37/P0-38). This is
/// the enforcement point a fetch/web tool (the genuinely user-prompt-
/// derived egress) must use for its outbound calls.
///
/// Redirects are followed here, not by `reqwest`: the wrapped client is
/// built with [`reqwest::redirect::Policy::none()`] and
/// [`CheckedHttpClient::execute`] re-runs the destination + scan gate on
/// every hop before that hop is sent (see the module docs). Production
/// constructors take an EXPLICIT [`DestinationPolicy`] and build the client
/// internally (redirect policy applied last, central resolver pinned); the
/// wrapped client is never injectable in production and never exposed.
#[derive(Debug, Clone)]
pub struct CheckedHttpClient {
    inner: reqwest::Client,
    /// `None` only for test-only permissive construction; every production
    /// constructor installs `Some(policy)`.
    policy: Option<DestinationPolicy>,
    outbound_scan: Option<OutboundScanConfig>,
    /// The central resolver pinned into `inner` (connect-time address-class
    /// gate and one-resolution rule).
    resolver: EgressResolver,
}

/// The hard bound on redirect hops one checked request may follow. A
/// redirect chain longer than this is [`EgressError::TooManyRedirects`]
/// (the refusal arrives before the hop that would exceed the bound is
/// sent); a redirect is only ever followed because the gate allowed the
/// next URL.
///
/// Restored to 10: this checker replaced `reqwest`'s internal follower
/// (whose effective default was `Policy::limited(10)`), and a value of 5
/// silently tightened every adapter's behavior instead of preserving the
/// documented compatibility surface. There is no bypass either way — every
/// hop still re-runs the parsed destination gate and the secret scan — but
/// 10 is the documented, adapter-compatible bound.
pub const MAX_REDIRECT_HOPS: usize = 10;

/// The method (and whether the payload must be dropped) of the next hop for
/// one redirect status, `None` when the status is not followed. Exactly the
/// semantics of the reqwest/tower-http follower this replaced: 301/302 turn
/// POST into GET (other methods keep method+body), 303 turns everything but
/// HEAD into GET (body + payload headers dropped), 307/308 preserve
/// method+body. 300/304/305/306 and non-3xx statuses are returned as-is.
fn next_hop(status: reqwest::StatusCode, method: &Method) -> Option<(Method, bool)> {
    use reqwest::StatusCode as S;
    match status {
        S::MOVED_PERMANENTLY | S::FOUND => {
            if *method == Method::POST {
                Some((Method::GET, true))
            } else {
                Some((method.clone(), false))
            }
        }
        S::SEE_OTHER => {
            if *method == Method::HEAD {
                Some((Method::HEAD, true))
            } else {
                Some((Method::GET, true))
            }
        }
        S::TEMPORARY_REDIRECT | S::PERMANENT_REDIRECT => Some((method.clone(), false)),
        _ => None,
    }
}

/// True when the two parsed URLs are not the same origin (scheme, host or
/// effective port differs) — the exact condition the previous reqwest
/// follower used to strip credentials.
fn cross_origin(previous: &Url, next: &Url) -> bool {
    next.host_str() != previous.host_str()
        || next.port_or_known_default() != previous.port_or_known_default()
        || next.scheme() != previous.scheme()
}

/// Drop the credential-bearing request headers on a cross-origin hop: the
/// same set reqwest's internal follower removed.
fn strip_credentials(headers: &mut HeaderMap) {
    for name in [AUTHORIZATION, COOKIE, PROXY_AUTHORIZATION, WWW_AUTHENTICATE] {
        headers.remove(name);
    }
    headers.remove("cookie2");
}

/// URL equality ignoring the fragment (never sent on the wire), used to
/// detect a wrapped client that followed a redirect itself.
fn same_url(a: &Url, b: &Url) -> bool {
    let (mut a, mut b) = (a.clone(), b.clone());
    a.set_fragment(None);
    b.set_fragment(None);
    a == b
}

impl CheckedHttpClient {
    /// TEST-ONLY: wrap an arbitrary pre-built client with an optional
    /// allowlist. `None` = no allowlist (permissive); the client MUST be
    /// built with [`reqwest::redirect::Policy::none()`] or every response
    /// fails closed with [`EgressError::UncheckedRedirectFollowed`], and it
    /// carries no central resolver (the OS resolver applies). Production
    /// construction never accepts an external client.
    #[cfg(test)]
    pub(crate) fn new(
        inner: reqwest::Client,
        policy: Option<DestinationPolicy>,
    ) -> CheckedHttpClient {
        CheckedHttpClient {
            inner,
            policy,
            outbound_scan: None,
            resolver: EgressResolver::system(EgressAddressPolicy::LOCAL),
        }
    }

    /// The test-only permissive client: no allowlist and the explicit
    /// loopback address rule (mock servers are loopback). Gated behind
    /// `cfg(test)` or the `test-utils` feature; a production build must
    /// construct with an explicit [`DestinationPolicy`].
    #[cfg(any(test, feature = "test-utils"))]
    pub fn permissive() -> CheckedHttpClient {
        let resolver = EgressResolver::system(EgressAddressPolicy::LOCAL);
        CheckedHttpClient {
            inner: try_default_timeout_client(resolver.clone())
                .expect("test-only egress client build"),
            policy: None,
            outbound_scan: None,
            resolver,
        }
    }

    /// A redirect-disabled, resolver-pinned client with the given REQUIRED
    /// allowlist (external address-class rule: loopback refused).
    /// Construction is FALLIBLE — a failed build is a typed
    /// [`EgressError::ClientBuild`], never a silent fallback client.
    pub fn try_with_policy(policy: DestinationPolicy) -> Result<CheckedHttpClient, EgressError> {
        CheckedHttpClient::try_with_policy_scan_and_addresses(
            policy,
            None,
            EgressAddressPolicy::EXTERNAL,
        )
    }

    /// A client with the given allowlist AND an installed outbound secret
    /// scan (see [`OutboundScanConfig`]).
    pub fn try_with_policy_and_scan(
        policy: DestinationPolicy,
        outbound_scan: Option<OutboundScanConfig>,
    ) -> Result<CheckedHttpClient, EgressError> {
        CheckedHttpClient::try_with_policy_scan_and_addresses(
            policy,
            outbound_scan,
            EgressAddressPolicy::EXTERNAL,
        )
    }

    /// TEST-ONLY: explicit allowlist + the loopback address rule (mock
    /// servers are loopback). Production construction goes through
    /// [`CheckedHttpClient::with_policy`] / [`CheckedHttpClient::with_policy_scan_and_addresses`].
    /// `test-utils` exposes it to dependent crates' dev-dependencies only.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn with_policy_for_tests(policy: DestinationPolicy) -> CheckedHttpClient {
        CheckedHttpClient::try_with_policy_scan_and_addresses(
            policy,
            None,
            EgressAddressPolicy::LOCAL,
        )
        .expect("test-only egress client build")
    }

    /// TEST-ONLY: production-part-equivalent client with an INJECTED
    /// resolver, so malicious DNS answers (an allowlisted hostname resolving
    /// to loopback/metadata/private addresses) can be driven without real
    /// DNS. Everything else matches production construction: redirect
    /// policy applied last, resolver pinned.
    #[cfg(test)]
    pub(crate) fn with_injected_resolver_for_tests(
        policy: DestinationPolicy,
        addresses: EgressAddressPolicy,
        resolver: Arc<dyn crate::resolver::HostResolver>,
        max_answers: usize,
    ) -> CheckedHttpClient {
        let resolver = EgressResolver::with_resolver(resolver, addresses, max_answers);
        CheckedHttpClient {
            inner: try_default_timeout_client(resolver.clone())
                .expect("test-only injected-resolver client build"),
            policy: Some(policy),
            outbound_scan: None,
            resolver,
        }
    }

    /// TEST-ONLY twin of [`CheckedHttpClient::with_policy_for_tests`] with
    /// an installed outbound scan.
    #[cfg(any(test, feature = "test-utils"))]
    pub fn with_policy_and_scan_for_tests(
        policy: DestinationPolicy,
        outbound_scan: Option<OutboundScanConfig>,
    ) -> CheckedHttpClient {
        CheckedHttpClient::try_with_policy_scan_and_addresses(
            policy,
            outbound_scan,
            EgressAddressPolicy::LOCAL,
        )
        .expect("test-only egress client build")
    }

    /// A client with an explicit address-class rule in addition to the
    /// allowlist and scan (the local-provider `allow_loopback` seam). The
    /// client is built HERE with the central resolver and
    /// [`reqwest::redirect::Policy::none()`] applied last — neither is
    /// overridable by any caller. A failed build is the typed
    /// [`EgressError::ClientBuild`]; there is no fallback client.
    pub fn try_with_policy_scan_and_addresses(
        policy: DestinationPolicy,
        outbound_scan: Option<OutboundScanConfig>,
        addresses: EgressAddressPolicy,
    ) -> Result<CheckedHttpClient, EgressError> {
        let resolver = EgressResolver::system(addresses);
        let inner = try_default_timeout_client(resolver.clone())?;
        Ok(CheckedHttpClient {
            inner,
            policy: Some(policy),
            outbound_scan,
            resolver,
        })
    }

    /// Install (or clear) the outbound secret scan on this client.
    pub fn set_outbound_scan(&mut self, outbound_scan: Option<OutboundScanConfig>) {
        self.outbound_scan = outbound_scan;
    }

    /// The installed outbound secret scan (`None` = no secret scanning).
    pub fn outbound_scan(&self) -> Option<&OutboundScanConfig> {
        self.outbound_scan.as_ref()
    }

    /// The installed allowlist (`None` only for test-only permissive
    /// construction).
    pub fn policy(&self) -> Option<&DestinationPolicy> {
        self.policy.as_ref()
    }

    /// The installed address-class rule.
    pub fn address_policy(&self) -> EgressAddressPolicy {
        self.resolver.policy()
    }

    /// The typed pre-send check for one parsed request URL: parsed
    /// allowlist (when installed) plus the literal-IP class gate.
    pub fn check(&self, url: &Url) -> Result<(), EgressError> {
        check_url_with(self.policy.as_ref(), url)
    }

    /// Start a GET on a raw URL string. The URL is parsed strictly here
    /// (typed errors for non-http(s) schemes, missing hosts, userinfo,
    /// port 0); the authoritative destination-policy check runs at send
    /// time on the final request object ([`CheckedHttpClient::execute`]).
    pub fn get(&self, url: &str) -> Result<RequestBuilder, EgressError> {
        let parsed = parse_fetch_url(url)?;
        Ok(self.inner.get(parsed))
    }

    /// Start a POST on a raw URL string (see [`CheckedHttpClient::get`]).
    pub fn post(&self, url: &str) -> Result<RequestBuilder, EgressError> {
        let parsed = parse_fetch_url(url)?;
        Ok(self.inner.post(parsed))
    }

    /// Build the request and send it through the gate. The gate runs on the
    /// request's OWN parsed URL immediately before `Client::execute`, and
    /// that same request object is what gets sent (no resolve-then-connect
    /// split, no re-parsing of strings anywhere).
    pub async fn send_checked(&self, builder: RequestBuilder) -> Result<Response, EgressError> {
        let request = builder
            .build()
            .map_err(|e| EgressError::Build(e.without_url().to_string()))?;
        self.execute(request).await
    }

    /// Execute an already-built request through the gate. This is the
    /// single choke point: any `reqwest::Request` (however it was built)
    /// is checked against the policy on its parsed URL — and against the
    /// installed outbound secret scan on its FULL body — before `execute`.
    ///
    /// Redirects are followed here, one request at a time, up to
    /// [`MAX_REDIRECT_HOPS`]: every hop re-runs the destination gate and
    /// the outbound secret scan on its own URL/body BEFORE the hop is
    /// sent, credentials never cross an origin change, and the wrapped
    /// client must not follow redirects itself (detected and refused).
    pub async fn execute(&self, request: Request) -> Result<Response, EgressError> {
        let mut request = request;
        let mut hops: usize = 0;
        loop {
            // Per-hop gate: parsed destination policy, then the full-body
            // secret scan of THIS hop's exact request object.
            self.check(request.url())?;
            if let Some(cfg) = &self.outbound_scan {
                self.gate_outbound_body(&request, cfg)?;
            }
            let hop_url = request.url().clone();
            let hop_method = request.method().clone();
            // A materialized body is cheap to replay across a preserving
            // redirect (`try_clone` shares the refcounted bytes); `None`
            // means the body is a stream and cannot be replayed.
            let replay = request.try_clone();
            let headers = request.headers().clone();

            let response = self
                .inner
                .execute(request)
                .await
                .map_err(|e| self.transport_failure(&hop_url, e))?;

            // Fail closed when the wrapped client followed a redirect
            // itself: its final URL differs from the hop that was checked,
            // so the bypass would otherwise skip every per-hop gate.
            if !same_url(response.url(), &hop_url) {
                return Err(EgressError::UncheckedRedirectFollowed {
                    from: SafeUrlDiagnostic::from_url(&hop_url),
                    to: SafeUrlDiagnostic::from_url(response.url()),
                });
            }

            let Some((next_method, drop_body)) = next_hop(response.status(), &hop_method) else {
                return Ok(response);
            };
            // A redirect status without a Location is not a redirect: the
            // response is surfaced exactly as the previous follower did.
            let Some(location) = response.headers().get(LOCATION) else {
                return Ok(response);
            };
            if hops >= MAX_REDIRECT_HOPS {
                return Err(EgressError::TooManyRedirects {
                    limit: MAX_REDIRECT_HOPS,
                    url: SafeUrlDiagnostic::from_url(&hop_url),
                });
            }
            let location = location.to_str().map_err(|_| {
                EgressError::UnparseableUrl(UrlRejectReason::RedirectLocationNotText)
            })?;
            let next_url = hop_url.join(location).map_err(|_| {
                EgressError::UnparseableUrl(UrlRejectReason::RedirectTargetUnparseable)
            })?;
            // A Location carrying userinfo would smuggle embedded
            // credentials past the credential-stripping rule.
            if !next_url.username().is_empty() || next_url.password().is_some() {
                return Err(EgressError::UnparseableUrl(
                    UrlRejectReason::RedirectUserinfoNotAllowed,
                ));
            }

            let mut next = match replay {
                Some(replayed) => replayed,
                None if !drop_body => {
                    return Err(EgressError::RedirectBodyNotReplayable {
                        url: SafeUrlDiagnostic::from_url(&hop_url),
                    });
                }
                None => {
                    // Method conversion drops the streamed body; keep every
                    // other header from the checked request.
                    let mut fresh = Request::new(next_method.clone(), next_url.clone());
                    *fresh.headers_mut() = headers;
                    fresh
                }
            };
            *next.url_mut() = next_url;
            if drop_body {
                *next.method_mut() = next_method;
                *next.body_mut() = None;
                for name in [
                    CONTENT_TYPE,
                    CONTENT_LENGTH,
                    CONTENT_ENCODING,
                    TRANSFER_ENCODING,
                ] {
                    next.headers_mut().remove(name);
                }
            }
            if cross_origin(&hop_url, next.url()) {
                strip_credentials(next.headers_mut());
            }
            hops += 1;
            request = next;
        }
    }

    /// Full-payload secret gate on the FINAL request body (audit P0-37 /
    /// P0-38): generic pattern scan plus the exact configured-secret
    /// registry scan over every byte, decided BEFORE any connect. A
    /// streamed (non-materialized) body under a scan config is refused —
    /// the full payload cannot be inspected, so the send must not happen.
    fn gate_outbound_body(
        &self,
        request: &Request,
        cfg: &OutboundScanConfig,
    ) -> Result<(), EgressError> {
        let Some(body) = request.body() else {
            return Ok(()); // no body (e.g. GET): nothing to scan
        };
        let Some(bytes) = body.as_bytes() else {
            return Err(EgressError::BodyNotMaterialized(
                SafeUrlDiagnostic::from_url(request.url()),
            ));
        };
        let outcome = scan_payload(bytes, &cfg.policy);
        let too_large = outcome == ScanOutcome::TooLargeForPolicy;
        let mut kinds: Vec<String> = match outcome {
            ScanOutcome::Found(hits) => {
                let mut seen = Vec::new();
                for kind in hits.into_iter().map(|h| h.kind) {
                    if !seen.contains(&kind) {
                        seen.push(kind);
                    }
                }
                seen
            }
            _ => Vec::new(),
        };
        if let Some(registry) = &cfg.registry {
            for hit in registry.scan_exact(bytes) {
                let kind = hit.kind;
                if !kinds.contains(&kind) {
                    kinds.push(kind);
                }
            }
        }
        if too_large {
            return Err(EgressError::BodyTooLarge {
                limit_bytes: cfg.policy.max_payload_bytes.unwrap_or(0),
            });
        }
        if !kinds.is_empty() {
            if cfg.block_on_secret {
                return Err(EgressError::SecretBlocked { kinds });
            }
            tracing::warn!(
                "outbound body contains a detected secret ({}); policy allows, sending",
                kinds.join(", ")
            );
        }
        Ok(())
    }

    /// Map one `reqwest` send failure onto a typed refusal. A resolver
    /// class refusal survives the hyper/reqwest error chain as the source
    /// of the connect error and is re-typed here (non-retryable); every
    /// other failure is a `Transport` error stringified only AFTER
    /// `without_url()`, so the failing URL (which may carry credentials)
    /// never enters the message.
    fn transport_failure(&self, hop_url: &Url, e: reqwest::Error) -> EgressError {
        if let Some(refusal) = find_resolve_refusal(&e) {
            match refusal {
                EgressResolveError::AddressClassRefused { class, .. } => {
                    return EgressError::AddressClassRefused {
                        url: SafeUrlDiagnostic::from_url(hop_url),
                        class: *class,
                    };
                }
                EgressResolveError::TooManyAnswers { count, limit } => {
                    return EgressError::DnsAnswerSetTooLarge {
                        url: SafeUrlDiagnostic::from_url(hop_url),
                        count: *count,
                        limit: *limit,
                    };
                }
                EgressResolveError::EmptyHost
                | EgressResolveError::HostTooLong { .. }
                | EgressResolveError::LookupFailed
                | EgressResolveError::NoAddresses => {}
            }
        }
        EgressError::Transport(e.without_url().to_string())
    }
}

/// Walk a `reqwest` error's source chain for a typed resolver refusal.
/// `reqwest::dns::Resolve` errors are boxed through hyper's connector, so
/// the class decision arrives as a nested source; the chain walk is
/// depth-bounded (a hostile error chain cannot spin here).
fn find_resolve_refusal<'a>(
    err: &'a (dyn std::error::Error + 'static),
) -> Option<&'a EgressResolveError> {
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(err);
    for _ in 0..16 {
        let e = current?;
        if let Some(refusal) = e.downcast_ref::<EgressResolveError>() {
            return Some(refusal);
        }
        current = e.source();
    }
    None
}

/// Parse one fetch URL strictly: absolute, http/https only, host present,
/// no userinfo, no port 0. Every refusal is a typed reason — the raw input
/// (which may itself carry a planted secret) is never echoed.
fn parse_fetch_url(raw: &str) -> Result<Url, EgressError> {
    let url =
        Url::parse(raw).map_err(|_| EgressError::UnparseableUrl(UrlRejectReason::ParseError))?;
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(EgressError::UnsupportedScheme(scheme.to_string()));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(EgressError::UnparseableUrl(UrlRejectReason::MissingHost));
    }
    if url.username() != "" || url.password().is_some() {
        return Err(EgressError::UnparseableUrl(
            UrlRejectReason::UserinfoNotAllowed,
        ));
    }
    if url.port() == Some(0) {
        return Err(EgressError::UnparseableUrl(UrlRejectReason::PortZero));
    }
    Ok(url)
}

/// Config-load validation for provider base URLs (provider egress is
/// config-derived and trusted, but the URL must still parse and be
/// http(s)). Erroring at config load keeps a bad base URL from surfacing
/// as a confusing runtime failure.
pub fn validate_provider_base_url(raw: &str) -> Result<Url, EgressError> {
    parse_fetch_url(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{MockAction, MockServer};
    use faktor_security::destination::{DestinationPolicy, RuleMatch};
    use std::sync::Arc;

    fn policy_for(port: u16) -> DestinationPolicy {
        DestinationPolicy::parse_lines([&format!("http://127.0.0.1:{port}")]).unwrap()
    }

    /// The audit row set against a live mock server. `allowed_port` is the
    /// mock's real port; the denied rows use a neighbouring port, a
    /// different IP, https, and a rebinding-style hostname.
    async fn audit_rows(server: Arc<MockServer>, allowed_port: u16) {
        let wrong_port = if allowed_port == u16::MAX {
            1
        } else {
            allowed_port + 1
        };
        let client = CheckedHttpClient::with_policy_for_tests(policy_for(allowed_port));
        let base = format!("http://127.0.0.1:{allowed_port}");

        // 1. Policy allows only http://127.0.0.1:<allowed>: allowed and
        //    reaches the mock exactly once.
        let resp = client
            .send_checked(client.get(&format!("{base}/probe")).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "ok");
        assert_eq!(server.request_count(), 1);

        // 2. http://127.0.0.1:<wrong port>: port mismatch denied BEFORE
        //    connect (the mock never sees it); the reason names the rule
        //    and the depth (host matched, port missed).
        let err = client
            .send_checked(
                client
                    .get(&format!("http://127.0.0.1:{wrong_port}/probe"))
                    .unwrap(),
            )
            .await
            .unwrap_err();
        match &err {
            EgressError::Denied { reason, .. } => {
                assert_eq!(
                    reason.rule_fired.as_deref(),
                    Some(format!("http://127.0.0.1:{allowed_port}")).as_deref(),
                    "{err}"
                );
                assert_eq!(reason.matched, RuleMatch::Host);
            }
            other => panic!("expected Denied, got {other:?}"),
        }
        assert_eq!(server.request_count(), 1, "no connect on port mismatch");

        // 3. http://127.0.0.2:<allowed port>: host mismatch denied before
        //    connect.
        let err = client
            .send_checked(
                client
                    .get(&format!("http://127.0.0.2:{allowed_port}/probe"))
                    .unwrap(),
            )
            .await
            .unwrap_err();
        match &err {
            EgressError::Denied { reason, .. } => {
                assert_eq!(reason.rule_fired, None, "{err}");
                assert_eq!(reason.matched, RuleMatch::None);
            }
            other => panic!("expected Denied, got {other:?}"),
        }
        assert_eq!(server.request_count(), 1, "no connect on host mismatch");

        // 4. https://127.0.0.1:<allowed port> against an http-only rule:
        //    scheme mismatch denied before connect; the reason names the
        //    rule with host+port matched.
        let err = client
            .send_checked(
                client
                    .get(&format!("https://127.0.0.1:{allowed_port}/probe"))
                    .unwrap(),
            )
            .await
            .unwrap_err();
        match &err {
            EgressError::Denied { reason, .. } => {
                assert_eq!(
                    reason.rule_fired.as_deref(),
                    Some(format!("http://127.0.0.1:{allowed_port}")).as_deref()
                );
                assert_eq!(reason.matched, RuleMatch::HostAndPort);
            }
            other => panic!("expected Denied, got {other:?}"),
        }
        assert_eq!(server.request_count(), 1, "no connect on scheme mismatch");

        // 5. DNS-rebinding class: `localhost` resolves to the same address
        //    family as 127.0.0.1 on this host, but the rule names the IP
        //    literal; the gate compares the URL host TEXT, so the request
        //    is denied before connect. A rebinding attacker pointing an
        //    evil hostname at the allowed IP gains nothing, and because the
        //    decision and the connect share one URL object (execute sends
        //    the checked request), no second resolution can slip in.
        let err = client
            .send_checked(
                client
                    .get(&format!("http://localhost:{allowed_port}/probe"))
                    .unwrap(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, EgressError::Denied { .. }), "{err:?}");
        assert_eq!(
            server.request_count(),
            1,
            "no connect on host-text mismatch"
        );
    }

    /// Serializes the two tests that want the literal audit port 9911:
    /// parallel tokio tests would otherwise race bind() on the same port.
    static PORT_9911_GUARD: std::sync::OnceLock<tokio::sync::Mutex<()>> =
        std::sync::OnceLock::new();
    async fn port_9911_guard() -> tokio::sync::MutexGuard<'static, ()> {
        PORT_9911_GUARD
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await
    }

    #[tokio::test]
    async fn checked_client_denies_port_host_scheme_before_connect() {
        let _guard = port_9911_guard().await;
        let server = MockServer::new();
        server.route(
            "GET",
            "/probe",
            MockAction::Respond {
                status: 200,
                body: "ok".into(),
            },
        );
        // Prefer the audit's literal port 9911 (with 9912 as the denied
        // mismatch port); fall back to an ephemeral port when 9911 is busy
        // (identical semantics, no CI flake).
        let (addr, _handle) = match server.serve_on(9911).await {
            Ok(pair) => pair,
            Err(_) => server.serve().await,
        };
        audit_rows(server, addr.port()).await;
    }

    #[tokio::test]
    async fn audit_rows_on_literal_9911_when_free() {
        // The audit's own numbers: policy allows only 127.0.0.1:9911;
        // 127.0.0.1:9912 (port mismatch) must be denied before connect.
        let _guard = port_9911_guard().await;
        let server = MockServer::new();
        server.route(
            "GET",
            "/probe",
            MockAction::Respond {
                status: 200,
                body: "ok".into(),
            },
        );
        let Ok((addr, _handle)) = server.serve_on(9911).await else {
            return; // port busy: covered by the ephemeral-port matrix
        };
        assert_eq!(addr.port(), 9911);
        audit_rows(server, 9911).await;
    }
    #[tokio::test]
    async fn default_allow_with_no_policy_and_deny_all_with_empty_policy() {
        let server = MockServer::new();
        server.route(
            "GET",
            "/probe",
            MockAction::Respond {
                status: 200,
                body: "ok".into(),
            },
        );
        let (addr, _handle) = server.serve().await;
        let url = format!("http://{addr}/probe");

        // No policy installed => default-allow (documented).
        let open = CheckedHttpClient::permissive();
        let resp = open.send_checked(open.get(&url).unwrap()).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(server.request_count(), 1);

        // Installed empty policy => default-deny, before connect.
        let closed = CheckedHttpClient::with_policy_for_tests(DestinationPolicy::empty());
        let err = closed
            .send_checked(closed.get(&url).unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&err, EgressError::Denied { reason, .. }
                if reason.rule_fired.is_none()),
            "{err:?}"
        );
        assert_eq!(server.request_count(), 1, "deny before connect");
    }

    #[tokio::test]
    async fn execute_is_the_choke_point_even_for_hand_built_requests() {
        let server = MockServer::new();
        server.route(
            "GET",
            "/probe",
            MockAction::Respond {
                status: 200,
                body: "ok".into(),
            },
        );
        let (addr, _handle) = server.serve().await;
        let policy = DestinationPolicy::parse_lines([&format!("http://{addr}")]).unwrap();
        let client = CheckedHttpClient::with_policy_for_tests(policy);

        // A request built directly (bypassing get()/post()) is still gated
        // at execute(): the URL object of the SENT request is what runs
        // through the policy.
        let allowed_url = Url::parse(&format!("http://{addr}/probe")).unwrap();
        let request = reqwest::Request::new(reqwest::Method::GET, allowed_url);
        let resp = client.execute(request).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(server.request_count(), 1);

        // Wrong port through the raw-request path: denied before connect.
        let wrong_port = addr.port() + 1;
        let wrong = Url::parse(&format!(
            "http://{}/probe",
            std::net::SocketAddr::from(([127, 0, 0, 1], wrong_port))
        ))
        .unwrap();
        let request = reqwest::Request::new(reqwest::Method::GET, wrong);
        let err = client.execute(request).await.unwrap_err();
        assert!(matches!(err, EgressError::Denied { .. }), "{err:?}");
        assert_eq!(server.request_count(), 1);
    }

    #[test]
    fn raw_request_debug_redacts_every_url_part_and_header_value_by_default() {
        // Planted secrets live in every hostile position the Debug face can
        // reach: path, userinfo, query, fragment, a named auth header, an
        // arbitrary custom header, a signature-style header, a session
        // header and the body.
        let request = RawRequest::new(
            "POST",
            "https://user:SECRET_USERINFO@api.example.com/v1/SECRET_PATH?api_key=SECRET_QUERY&plain=SECRET_TOO#SECRET_FRAGMENT",
        )
        .header("authorization", "Bearer SECRET_HEADER")
        .header("x-api-key", "SECRET_KEY")
        .header("cookie", "session=SECRET_COOKIE")
        .header("x-signature", "SECRET_SIGNATURE")
        .header("x-session-id", "SECRET_SESSION")
        .header("x-custom", "SECRET_CUSTOM")
        .header("content-type", "application/json")
        .header("content-length", "42")
        .header("accept", "application/json")
        .header("user-agent", "faktor-test/0.1")
        .json_body(&serde_json::json!({ "password": "SECRET_BODY" }))
        .expect("json body encodes");
        // `route` is a compile-time constant: it renders in full and can
        // never be influenced by the hostile URL.
        let request = request.route(RouteLabel::GithubApi);
        let debug = format!("{request:?}");
        for secret in [
            "SECRET_USERINFO",
            "SECRET_PATH",
            "SECRET_QUERY",
            "SECRET_TOO",
            "SECRET_FRAGMENT",
            "SECRET_HEADER",
            "SECRET_KEY",
            "SECRET_COOKIE",
            "SECRET_SIGNATURE",
            "SECRET_SESSION",
            "SECRET_CUSTOM",
            "SECRET_BODY",
            "Bearer",
        ] {
            assert!(
                !debug.contains(secret),
                "planted secret {secret:?} leaked through Debug: {debug}"
            );
        }
        assert!(debug.contains("route: GithubApi"), "{debug}");
        // The URL keeps only the safe shape: no userinfo, no path bytes, no
        // query values, no fragment.
        assert!(
            debug.contains("https://api.example.com:443/<redacted-path>?<redacted>"),
            "{debug}"
        );
        // Header policy is deny-by-default: every non-allowlisted value is a
        // length marker, whatever its name.
        assert!(
            debug.contains("authorization: <redacted; 20 bytes>"),
            "{debug}"
        );
        assert!(
            debug.contains("x-signature: <redacted; 16 bytes>"),
            "{debug}"
        );
        assert!(
            debug.contains("x-session-id: <redacted; 14 bytes>"),
            "{debug}"
        );
        assert!(debug.contains("x-custom: <redacted; 13 bytes>"), "{debug}");
        // The tiny safe-value allowlist renders verbatim.
        assert!(debug.contains("content-type: application/json"), "{debug}");
        assert!(debug.contains("content-length: 42"), "{debug}");
        assert!(
            debug.contains("accept: application/json"),
            "safe headers stay visible: {debug}"
        );
        assert!(debug.contains("user-agent: faktor-test/0.1"), "{debug}");
        assert!(debug.contains("bytes>"), "body is length-only: {debug}");
        // An unparseable URL must not be echoed raw (it could itself carry a
        // planted secret); the route label survives.
        let hostile =
            RawRequest::new("GET", "not a url ?secret=RAW_SECRET").route(RouteLabel::OidcDiscovery);
        let hostile_debug = format!("{hostile:?}");
        assert!(!hostile_debug.contains("RAW_SECRET"), "{hostile_debug}");
        assert!(hostile_debug.contains("<unparseable>"), "{hostile_debug}");
        assert!(hostile_debug.contains("OidcDiscovery"), "{hostile_debug}");
        // Clone/PartialEq are untouched by the manual Debug.
        assert_eq!(request.clone(), request);
    }

    #[tokio::test]
    async fn unsupported_schemes_are_rejected_at_parse_and_at_execute() {
        let client = CheckedHttpClient::with_policy_for_tests(DestinationPolicy::empty());
        // get() rejects non-http(s) schemes at builder time.
        let err = client.get("ftp://example.com/file").unwrap_err();
        assert!(matches!(err, EgressError::UnsupportedScheme(_)), "{err:?}");
        let err = client.get("gopher://example.com/").unwrap_err();
        assert!(matches!(err, EgressError::UnsupportedScheme(_)), "{err:?}");
        let err = client.get("not-a-url").unwrap_err();
        assert!(matches!(err, EgressError::UnparseableUrl(_)), "{err:?}");
        // execute() also gates scheme: a hand-built ws:// request never
        // leaves (checked before connect).
        let ws = Url::parse("ws://example.com/").unwrap();
        let request = reqwest::Request::new(reqwest::Method::GET, ws);
        let err = client.execute(request).await.unwrap_err();
        assert!(matches!(err, EgressError::UnsupportedScheme(_)), "{err:?}");
    }

    #[test]
    fn validate_provider_base_url_enforces_config_load_contract() {
        for good in [
            "https://api.openai.com/v1",
            "http://127.0.0.1:9911",
            "https://api.deepseek.com",
        ] {
            let url = validate_provider_base_url(good).unwrap();
            assert!(url.host_str().is_some(), "{good}");
        }
        for bad in [
            "ftp://api.example.com",
            "api.example.com",               // relative: no scheme
            "https://",                      // no host
            "https://user:pass@example.com", // userinfo
            "not a url",
            "https://example.com:0",
            "https://example.com:99999",
        ] {
            let err = validate_provider_base_url(bad).unwrap_err();
            assert!(
                matches!(
                    err,
                    EgressError::UnparseableUrl(_) | EgressError::UnsupportedScheme(_)
                ),
                "{bad:?} => {err:?}"
            );
        }
    }

    #[test]
    fn checked_client_policy_is_visible_and_default_allow() {
        let p = DestinationPolicy::parse_lines(["https://example.com"]).unwrap();
        let c = CheckedHttpClient::with_policy_for_tests(p.clone());
        assert_eq!(c.policy(), Some(&p));
        let c = CheckedHttpClient::permissive();
        assert_eq!(c.policy(), None);
    }

    /// The boxed-diagnostic size contract: `Result<_, EgressError>` must
    /// stay under clippy's large-error threshold WITHOUT any lint allowance
    /// (a regression to inline diagnostics re-fires `result_large_err`), and
    /// the serde shape stays the flat field object — now WITHOUT any raw
    /// path bytes (`path_segments` only). An attached route label is a
    /// compile-time constant and is the only label text the wire can carry.
    #[test]
    fn egress_error_is_small_and_diagnostic_carries_no_path_bytes() {
        assert!(
            std::mem::size_of::<EgressError>() < 128,
            "EgressError grew to {} bytes; box oversized payloads",
            std::mem::size_of::<EgressError>()
        );
        assert_eq!(
            std::mem::size_of::<SafeUrlDiagnostic>(),
            std::mem::size_of::<Box<()>>(),
            "the diagnostic fields must live behind the box"
        );
        let url = Url::parse(
            "https://user:pass@api.example.com:8443/v1/chat/SECRET_PATH?api_key=SECRET#frag",
        )
        .unwrap();
        let diag = SafeUrlDiagnostic::from_url(&url);
        let json = serde_json::to_value(&diag).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "scheme": "https",
                "host": "api.example.com",
                "port": 8443,
                "path_segments": 3,
                "query_present": true,
            }),
            "the wire shape keeps only the segment count (no path bytes)"
        );
        assert!(!json.to_string().contains("SECRET"), "{json}");
        let back: SafeUrlDiagnostic = serde_json::from_value(json).unwrap();
        assert_eq!(back, diag);
        // Credential masking survives the boxing: neither the planted path
        // or query bytes nor the userinfo may appear in any rendering.
        for rendered in [diag.to_string(), format!("{diag:?}")] {
            assert!(!rendered.contains("SECRET"), "{rendered}");
            assert!(!rendered.contains("chat"), "{rendered}");
            assert!(!rendered.contains("pass"), "{rendered}");
            assert!(rendered.contains("api.example.com:8443"), "{rendered}");
            assert!(rendered.contains("/<redacted-path>"), "{rendered}");
            assert!(rendered.contains("?<redacted>"), "{rendered}");
        }
        // A labelled diagnostic renders the compile-time constant, never
        // URL-derived text.
        let labelled = SafeUrlDiagnostic::from_url_route(&url, RouteLabel::OidcDiscovery);
        assert_eq!(
            labelled.to_string(),
            "https://api.example.com:8443/<redacted-path>?<redacted> [oidc_discovery]"
        );
        assert_eq!(labelled.route(), Some(RouteLabel::OidcDiscovery));
    }

    // ------------------------------------------------------------ transport

    fn policy_for_port(port: u16) -> DestinationPolicy {
        DestinationPolicy::parse_lines([&format!("http://127.0.0.1:{port}")]).unwrap()
    }

    async fn execute_once(
        transport: &dyn HttpTransport,
        url: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<reqwest::Response, EgressError> {
        match body {
            Some(json) => {
                execute_post_json(transport, url, reqwest::header::HeaderMap::new(), json).await
            }
            None => execute_get(transport, url).await,
        }
    }

    #[tokio::test]
    async fn policy_checked_transport_denies_before_connect_and_records_no_bytes() {
        let server = MockServer::new();
        server.route(
            "GET",
            "/probe",
            MockAction::Respond {
                status: 200,
                body: "ok".into(),
            },
        );
        let (addr, _handle) = server.serve().await;
        let allowed: Arc<dyn HttpTransport> = Arc::new(
            PolicyCheckedHttpTransport::with_policy_for_tests(policy_for_port(addr.port())),
        );
        // Allowed: same parsed destination, streams normally.
        let resp = execute_once(
            allowed.as_ref(),
            &format!("http://127.0.0.1:{}/probe", addr.port()),
            None,
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "ok");
        assert_eq!(server.request_count(), 1);

        // Denied (installed policy, wrong port): refused BEFORE any network
        // byte — the live mock never sees the request.
        let denied: Arc<dyn HttpTransport> =
            Arc::new(PolicyCheckedHttpTransport::with_policy_for_tests(
                policy_for_port(addr.port().wrapping_add(1)),
            ));
        let err = execute_once(
            denied.as_ref(),
            &format!("http://127.0.0.1:{}/probe", addr.port()),
            None,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, EgressError::Denied { .. }), "{err:?}");
        assert_eq!(server.request_count(), 1, "deny happened before connect");

        // Permissive (no policy installed) = default-allow (documented).
        let open: Arc<dyn HttpTransport> = Arc::new(PolicyCheckedHttpTransport::permissive());
        let resp = execute_once(
            open.as_ref(),
            &format!("http://127.0.0.1:{}/probe", addr.port()),
            None,
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(server.request_count(), 2);
    }

    #[tokio::test]
    async fn helpers_build_json_posts_with_extra_headers_and_streaming_body() {
        let server = MockServer::new();
        server.route(
            "POST",
            "/json",
            MockAction::Respond {
                status: 200,
                body: "data: {\"ok\":true}\n\n".into(),
            },
        );
        let (addr, _handle) = server.serve().await;
        let transport: Arc<dyn HttpTransport> = Arc::new(PolicyCheckedHttpTransport::permissive());
        let mut auth = reqwest::header::HeaderMap::new();
        auth.insert("authorization", "Bearer base".parse().unwrap());
        let json = serde_json::json!({"a": 1});
        let resp = execute_post_json_with_extras(
            transport.as_ref(),
            &format!("http://{addr}/json"),
            auth,
            &crate::config::ExtraHeaders::try_new([("authorization", "Bearer extra")]).unwrap(),
            &json,
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), 200);
        // The body must stream: text() reads it after execute returned.
        assert_eq!(resp.text().await.unwrap(), "data: {\"ok\":true}\n\n");
        let (_, path, body) = server.last_request().unwrap();
        assert_eq!(path, "/json");
        let sent: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(sent, json);
        let headers = server.last_request_headers();
        let get = |name: &str| {
            headers
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(
            get("authorization").as_deref(),
            Some("Bearer extra"),
            "extra headers replace same-named base headers"
        );
        assert_eq!(get("content-type").as_deref(), Some("application/json"));
    }

    #[test]
    fn mock_transport_serves_canned_bodies_and_deny_mode_without_http() {
        let transport = MockHttpTransport::new(200, "canned body");
        let url = reqwest::Url::parse("http://example.invalid/p").unwrap();
        let req = reqwest::Request::new(reqwest::Method::GET, url);
        let resp = futures::executor::block_on(transport.execute(req)).unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(
            futures::executor::block_on(resp.text()).unwrap(),
            "canned body"
        );
        assert_eq!(
            transport.requests(),
            vec![("GET".to_string(), "http://example.invalid/p".to_string())]
        );
        let transport = MockHttpTransport::denying(EgressError::Denied {
            url: "http://example.invalid".into(),
            reason: DeniedReason {
                rule_fired: None,
                matched: faktor_security::destination::RuleMatch::None,
            },
        });
        let url = reqwest::Url::parse("http://example.invalid/p").unwrap();
        let req = reqwest::Request::new(reqwest::Method::POST, url);
        let err = futures::executor::block_on(transport.execute(req)).unwrap_err();
        assert!(matches!(err, EgressError::Denied { .. }), "{err:?}");
        assert_eq!(transport.request_count(), 1);
    }

    #[test]
    fn egress_error_maps_to_provider_error_retryability() {
        let denied = EgressError::Denied {
            url: "http://127.0.0.1:1".into(),
            reason: DeniedReason {
                rule_fired: None,
                matched: faktor_security::destination::RuleMatch::None,
            },
        };
        let e: ProviderError = denied.into();
        assert!(!e.retryable, "a denied destination is never retried");
        assert!(e.message.contains("denied"), "{}", e.message);
        let transport: ProviderError = EgressError::Transport("connect refused".into()).into();
        assert_eq!(transport.kind, ProviderErrorKind::Network);
        assert!(transport.retryable);
    }

    #[test]
    fn checked_http_client_is_a_transport_and_reuses_the_execute_gate() {
        let client = CheckedHttpClient::with_policy_for_tests(DestinationPolicy::empty());
        let transport: &dyn HttpTransport = &client;
        let url = reqwest::Url::parse("http://127.0.0.1:9/probe").unwrap();
        let req = reqwest::Request::new(reqwest::Method::GET, url);
        let err = futures::executor::block_on(transport.execute(req)).unwrap_err();
        assert!(
            matches!(err, EgressError::Denied { .. }),
            "the empty installed policy denies before connect: {err:?}"
        );
    }

    // ------------------------------------------------------- source scan

    /// Raw-client scan markers (module scope so the separator regression
    /// test drives the same matcher logic as the scan).
    const RAW_CLIENT_MARKERS: [&str; 3] = [".execute(", "reqwest::Client::new", "Client::builder"];

    /// Adjudicated exemptions. Empty by default; the ONLY entries are the
    /// MockServer harness self-tests (provider/src/testing.rs) which drive
    /// the mock with a raw client to prove the harness works — they are the
    /// harness testing itself, never adapter egress. Keyed by relative path
    /// + trimmed line so unrelated new offenders are still listed loudly.
    const RAW_CLIENT_ALLOWLIST: &[(&str, &str)] = &[(
        "provider/src/testing.rs",
        "let client = reqwest::Client::new();",
    )];

    /// The adapter/provider trees scanned below live under `crates/`.
    fn crates_root() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("crates/provider sits directly under crates/")
            .to_path_buf()
    }

    /// Windows runners produce `\`-separated rels; every comparison against
    /// the `/`-joined constants (`provider/src/egress.rs`, the allowlist)
    /// happens after this normalization.
    fn normalize_rel(rel: &str) -> String {
        rel.replace('\\', "/")
    }

    /// True when the attribute text above a declaration test-gates it. Only
    /// the shapes this repository uses for out-of-line test modules are
    /// recognized (`#[cfg(test)]`, `#[cfg(all(test, …))]`); an unrecognized
    /// attribute leaves the file scanned — the conservative direction can
    /// only over-report, never exempt production code.
    fn attrs_are_test_gated(attrs: &str) -> bool {
        let compact: String = attrs.split_whitespace().collect();
        compact.contains("#[cfg(test)]") || compact.contains("#[cfg(all(test")
    }

    /// Does some sibling `.rs` in `dir` include `file_name` as a
    /// test-gated out-of-line module (`mod <stem>;` or
    /// `#[path = "<file>"]`)?
    fn has_test_include(dir: &std::path::Path, file_name: &str, stem: &str) -> bool {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let sibling = entry.path();
            if sibling.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let Ok(src) = std::fs::read_to_string(&sibling) else {
                continue;
            };
            let lines: Vec<&str> = src.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                let trimmed = line.trim();
                if trimmed != format!("mod {stem};")
                    && trimmed != format!("#[path = \"{file_name}\"]")
                {
                    continue;
                }
                let mut attrs = String::new();
                let mut j = i;
                while j > 0 {
                    let prev = lines[j - 1].trim();
                    if prev.starts_with("#[") || prev.ends_with(']') {
                        attrs.insert_str(0, prev);
                        attrs.push('\n');
                        j -= 1;
                    } else {
                        break;
                    }
                }
                if attrs_are_test_gated(&attrs) {
                    return true;
                }
            }
        }
        false
    }

    /// Test-only source paths are exempt from the raw-client scan: a file
    /// under a `/tests/` component, or a `tests.rs`/`*_tests.rs` file whose
    /// name is backed by a real cfg(test)-gated include in a sibling (a
    /// name alone must never exempt production code). `rel` may arrive in
    /// either separator spelling; it is normalized first.
    fn is_test_rel_at(root: &std::path::Path, rel: &str) -> bool {
        let rel = normalize_rel(rel);
        if rel.contains("/tests/") {
            return true;
        }
        let name = rel.rsplit('/').next().unwrap_or(&rel);
        if name != "tests.rs" && !name.ends_with("_tests.rs") {
            return false;
        }
        let path = root.join(&rel);
        let Some(dir) = path.parent() else {
            return false;
        };
        has_test_include(dir, name, name.strip_suffix(".rs").unwrap_or(name))
    }

    fn is_test_rel(rel: &str) -> bool {
        is_test_rel_at(&crates_root(), rel)
    }

    /// Raw-client offenders of one file: every marker line not excused by
    /// the exact-line allowlist. `rel` is normalized first, so a Windows
    /// runner's `\` spelling matches the `/`-joined exemptions.
    fn raw_client_offenders(rel: &str, source: &str) -> Vec<String> {
        let rel = normalize_rel(rel);
        let mut offenders = Vec::new();
        for (idx, line) in source.lines().enumerate() {
            let trimmed = line.trim();
            if RAW_CLIENT_MARKERS.iter().any(|m| line.contains(m)) {
                let allowlisted = RAW_CLIENT_ALLOWLIST
                    .iter()
                    .any(|(p, text)| normalize_rel(p) == rel && *text == trimmed);
                if !allowlisted {
                    offenders.push(format!("{rel}:{}: {trimmed}", idx + 1));
                }
            }
        }
        offenders
    }

    /// The P0-36 static certification: no raw `reqwest` egress may exist
    /// outside this file. Scanning our own sources is acceptable here; the
    /// walk is bounded to the six adapter dirs + provider/src. Paths are
    /// normalized on entry, so the exemption and allowlist hold on Windows
    /// runners (the 2026-09 failure scanned `egress.rs` itself because
    /// `provider\src\egress.rs` never matched).
    #[test]
    fn no_raw_client_execute_outside_egress() {
        let crates_root = crates_root();
        let mut roots: Vec<std::path::PathBuf> = Vec::new();
        for dir in [
            "openai",
            "anthropic",
            "google",
            "deepseek",
            "gateway",
            "ollama",
        ] {
            roots.push(crates_root.join(dir).join("src"));
        }
        roots.push(crates_root.join("provider").join("src"));

        let mut offenders: Vec<String> = Vec::new();
        let mut scanned = 0usize;
        for root in roots {
            let mut stack = vec![root];
            while let Some(dir) = stack.pop() {
                let entries = match std::fs::read_dir(&dir) {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    let file_type = match entry.file_type() {
                        Ok(t) => t,
                        Err(_) => continue,
                    };
                    if file_type.is_dir() {
                        stack.push(path);
                        continue;
                    }
                    if file_type.is_file()
                        && path.extension().and_then(|e| e.to_str()) == Some("rs")
                    {
                        scanned += 1;
                        let rel = path
                            .strip_prefix(&crates_root)
                            .unwrap_or(&path)
                            .display()
                            .to_string();
                        let rel = normalize_rel(&rel);
                        if rel == "provider/src/egress.rs" || is_test_rel(&rel) {
                            continue; // the ONE allowed file; test-only sources
                        }
                        let Ok(source) = std::fs::read_to_string(&path) else {
                            continue;
                        };
                        offenders.extend(raw_client_offenders(&rel, &source));
                    }
                }
            }
        }
        assert!(scanned >= 7, "scan walked nothing: {scanned}");
        assert!(
            offenders.is_empty(),
            "raw reqwest egress outside crates/provider/src/egress.rs:\n  {}\n\
             Every adapter send must go through the HttpTransport seam in egress.rs.",
            offenders.join("\n  ")
        );
    }

    #[test]
    fn source_scan_normalizes_windows_separators_and_exempts_test_modules() {
        // Separator normalization: a Windows rel reaches the same string as
        // its POSIX twin, so `egress.rs` and the allowlist still match.
        assert_eq!(
            normalize_rel(r"provider\src\egress.rs"),
            "provider/src/egress.rs"
        );
        assert_eq!(
            normalize_rel(r"provider\src\testing.rs"),
            "provider/src/testing.rs"
        );
        // A synthetic production rel through the raw matcher fires with the
        // normalized rel (on a Windows runner this line previously matched
        // nothing when the file was `egress.rs` itself).
        let offenders =
            raw_client_offenders(r"crates\x\src\lib.rs", "let c = reqwest::Client::new();\n");
        assert!(
            offenders
                .iter()
                .any(|o| o.starts_with("crates/x/src/lib.rs:1:")
                    && o.contains("reqwest::Client::new")),
            "a raw client outside egress must fire under a Windows rel: {offenders:?}"
        );
        // The allowlist key matches under the Windows spelling too.
        let allowed = raw_client_offenders(
            r"provider\src\testing.rs",
            "        let client = reqwest::Client::new();\n",
        );
        assert!(
            allowed.is_empty(),
            "allowlisted line must match: {allowed:?}"
        );
        // `/tests/` layout is test code.
        assert!(is_test_rel_at(
            std::path::Path::new("."),
            "x/tests/helper.rs"
        ));
        // Include-verified rule: a `*_tests.rs` file included by a sibling
        // under `#[cfg(test)]` is test code…
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("x").join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("shadow_tests.rs"), "").unwrap();
        std::fs::write(
            src.join("shadow.rs"),
            "#[cfg(test)]\n#[path = \"shadow_tests.rs\"]\nmod shadow_tests;\n",
        )
        .unwrap();
        assert!(is_test_rel_at(tmp.path(), r"x\src\shadow_tests.rs"));
        assert!(!is_test_rel_at(tmp.path(), "x/src/shadow.rs"));
        // …while the same NAME with no cfg(test)-gated include stays
        // scanned (a name alone never exempts production code).
        std::fs::write(
            src.join("shadow.rs"),
            "#[path = \"shadow_tests.rs\"]\nmod shadow_tests;\n",
        )
        .unwrap();
        assert!(!is_test_rel_at(tmp.path(), "x/src/shadow_tests.rs"));
    }

    // -------------------------------------- permissive-constructor scan

    /// The wire-family adapters whose production constructors must never
    /// build the default-allow transport: the daemon injects the
    /// policy-checked one.
    const PERMISSIVE_ADAPTERS: [&str; 6] = [
        "openai",
        "anthropic",
        "google",
        "deepseek",
        "gateway",
        "ollama",
    ];

    /// Production adapter code must not construct ANY egress transport — the
    /// `Arc<dyn HttpTransport>` is injected by the daemon. Matching the type
    /// prefixes catches the permissive default (`::permissive()`,
    /// `::with_policy(None)`) and a self-built policy client alike; only
    /// test-gated code may hold these constructors.
    const TRANSPORT_CTOR_MARKERS: [&str; 2] =
        ["PolicyCheckedHttpTransport::", "CheckedHttpClient::"];

    /// Mask comments and string/char literals (raw and escaped) with spaces,
    /// preserving byte length, so braces and marker text inside them can
    /// never shift or trip the structural scan. Malformed input is left as
    /// is (the conservative direction: the scan can only over-report).
    fn mask_noncode(src: &str) -> String {
        let b = src.as_bytes();
        // Index-preserving UTF-8 check: every masked span is replaced by
        // ASCII spaces, so the result must stay valid.
        let mut out = b.to_vec();
        let mut i = 0usize;
        while i < b.len() {
            if b[i] == b'/' && b.get(i + 1) == Some(&b'/') {
                while i < b.len() && b[i] != b'\n' {
                    out[i] = b' ';
                    i += 1;
                }
                continue;
            }
            if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                out[i] = b' ';
                if i + 1 < b.len() {
                    out[i + 1] = b' ';
                }
                i += 2;
                while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                    if b[i] != b'\n' {
                        out[i] = b' ';
                    }
                    i += 1;
                }
                if i < b.len() {
                    out[i] = b' ';
                    if i + 1 < b.len() {
                        out[i + 1] = b' ';
                    }
                    i += 2;
                }
                continue;
            }
            let raw_start = {
                let mut j = i;
                if b.get(j) == Some(&b'b') {
                    j += 1;
                }
                if b.get(j) == Some(&b'r') {
                    Some(j + 1)
                } else {
                    None
                }
            };
            if let Some(mut j) = raw_start {
                let mut hashes = 0usize;
                while b.get(j) == Some(&b'#') {
                    hashes += 1;
                    j += 1;
                }
                if b.get(j) == Some(&b'"') {
                    let end = find_raw_string_end(b, j + 1, hashes).unwrap_or(b.len());
                    for k in i..end {
                        if b[k] != b'\n' {
                            out[k] = b' ';
                        }
                    }
                    i = end;
                    continue;
                }
            }
            let string_start = if b[i] == b'"' {
                Some(i + 1)
            } else if b.get(i) == Some(&b'b') && b.get(i + 1) == Some(&b'"') {
                Some(i + 2)
            } else {
                None
            };
            if let Some(mut j) = string_start {
                while j < b.len() {
                    if b[j] == b'\\' {
                        j += 2;
                        continue;
                    }
                    if b[j] == b'"' {
                        j += 1;
                        break;
                    }
                    j += 1;
                }
                let end = j.min(b.len());
                for k in i..end {
                    if b[k] != b'\n' {
                        out[k] = b' ';
                    }
                }
                i = end;
                continue;
            }
            if b[i] == b'\'' {
                if b.get(i + 1) == Some(&b'\\') {
                    let mut j = i + 2;
                    while j < b.len() && b[j] != b'\'' {
                        j += 1;
                    }
                    let end = (j + 1).min(b.len());
                    for k in i..end {
                        if b[k] != b'\n' {
                            out[k] = b' ';
                        }
                    }
                    i = end;
                    continue;
                }
                if b.get(i + 2) == Some(&b'\'') {
                    for k in i..i + 3 {
                        if b[k] != b'\n' {
                            out[k] = b' ';
                        }
                    }
                    i += 3;
                    continue;
                }
                // otherwise a lifetime like `'a` / `'_`: code, not a literal.
            }
            i += 1;
        }
        String::from_utf8(out).expect("masking preserves UTF-8")
    }

    fn find_raw_string_end(b: &[u8], mut j: usize, hashes: usize) -> Option<usize> {
        while j < b.len() {
            if b[j] == b'"' {
                let closed = (0..hashes).all(|h| b.get(j + 1 + h) == Some(&b'#'));
                if closed {
                    return Some(j + 1 + hashes);
                }
            }
            j += 1;
        }
        None
    }

    /// Byte spans of items gated by `#[cfg(...test...)]` (but never
    /// `#[cfg(not(test))]`, which is production). Spans include the
    /// attribute so a marker inside the gated item is exempt.
    fn test_gated_spans(masked: &str) -> Vec<(usize, usize)> {
        let b = masked.as_bytes();
        let mut spans = Vec::new();
        let mut search = 0usize;
        while let Some(rel) = masked[search..].find("#[cfg(") {
            let start = search + rel;
            let mut j = start + "#[cfg(".len();
            let mut depth = 1usize;
            while j < b.len() && depth > 0 {
                match b[j] {
                    b'(' => depth += 1,
                    b')' => depth -= 1,
                    _ => {}
                }
                j += 1;
            }
            let words: Vec<&str> = masked[start..j]
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .filter(|w| !w.is_empty())
                .collect();
            let gated = words.contains(&"test") && !words.contains(&"not");
            if gated {
                let mut k = j;
                while k < b.len() && b[k] != b'{' && b[k] != b';' {
                    k += 1;
                }
                if k < b.len() && b[k] == b'{' {
                    let mut depth = 0usize;
                    let mut m = k;
                    while m < b.len() {
                        match b[m] {
                            b'{' => depth += 1,
                            b'}' => {
                                depth -= 1;
                                if depth == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                        m += 1;
                    }
                    spans.push((start, (m + 1).min(b.len())));
                }
            }
            search = j;
        }
        spans
    }

    /// Offenders of one file: every transport construction outside any
    /// test-gated item (the `permissive_for_tests` helper must itself be
    /// `#[cfg(test)]`, so it is exempt through its attribute — not by name).
    fn transport_ctor_offenders(rel: &str, source: &str) -> Vec<String> {
        let masked = mask_noncode(source);
        let spans = test_gated_spans(&masked);
        let mut hits: Vec<usize> = TRANSPORT_CTOR_MARKERS
            .iter()
            .flat_map(|m| masked.match_indices(m).map(|(idx, _)| idx))
            .collect();
        hits.sort_unstable();
        let mut offenders = Vec::new();
        for idx in hits {
            if spans.iter().any(|(s, e)| idx >= *s && idx < *e) {
                continue;
            }
            let line = source[..idx].matches('\n').count() + 1;
            let text = source.lines().nth(line - 1).unwrap_or("").trim();
            offenders.push(format!("{rel}:{line}: {text}"));
        }
        offenders
    }

    /// Total marker occurrences in one file (masked), so the certification
    /// can prove its walk actually saw the test helpers instead of passing
    /// vacuously.
    fn transport_ctor_hits(source: &str) -> usize {
        let masked = mask_noncode(source);
        TRANSPORT_CTOR_MARKERS
            .iter()
            .map(|m| masked.matches(m).count())
            .sum()
    }

    /// The constructor certification: every transport construction in the
    /// six adapters must live in test-gated code. A production constructor
    /// that silently defaults to default-allow (or self-builds any egress
    /// client instead of taking the injected one) fails here.
    #[test]
    fn no_transport_ctor_outside_test_code_in_adapters() {
        let crates_root = crates_root();
        let mut offenders: Vec<String> = Vec::new();
        let mut scanned = 0usize;
        let mut hits = 0usize;
        for krate in PERMISSIVE_ADAPTERS {
            let mut stack = vec![crates_root.join(krate).join("src")];
            while let Some(dir) = stack.pop() {
                let Ok(entries) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    let Ok(file_type) = entry.file_type() else {
                        continue;
                    };
                    if file_type.is_dir() {
                        stack.push(path);
                        continue;
                    }
                    if !(file_type.is_file()
                        && path.extension().and_then(|e| e.to_str()) == Some("rs"))
                    {
                        continue;
                    }
                    scanned += 1;
                    let rel = path
                        .strip_prefix(&crates_root)
                        .unwrap_or(&path)
                        .display()
                        .to_string();
                    let rel = normalize_rel(&rel);
                    if is_test_rel_at(&crates_root, &rel) {
                        continue;
                    }
                    let Ok(source) = std::fs::read_to_string(&path) else {
                        continue;
                    };
                    hits += transport_ctor_hits(&source);
                    offenders.extend(transport_ctor_offenders(&rel, &source));
                }
            }
        }
        assert!(scanned >= 6, "constructor scan walked nothing: {scanned}");
        assert!(
            hits >= 6,
            "constructor scan found no transport constructor at all ({hits} hits): \
             the walk is not covering the adapter sources"
        );
        assert!(
            offenders.is_empty(),
            "egress transport constructed in production adapter code:\n  {}\n\
             Every adapter constructor must take the injected Arc<dyn HttpTransport>; \
             transport construction belongs in #[cfg(test)] helpers only.",
            offenders.join("\n  ")
        );
    }

    // ----------------------------- permissive-constructor reference scan

    /// The gated default-allow constructor.
    const PERMISSIVE_CTOR_MARKER: &str = "PolicyCheckedHttpTransport::permissive";

    /// Offenders of one file: every reference to the permissive constructor
    /// outside a test-gated item (and outside `/tests/`-layout files).
    fn permissive_ctor_offenders(rel: &str, source: &str) -> Vec<String> {
        let masked = mask_noncode(source);
        let spans = test_gated_spans(&masked);
        let mut offenders = Vec::new();
        for (idx, _) in masked.match_indices(PERMISSIVE_CTOR_MARKER) {
            if spans.iter().any(|(s, e)| idx >= *s && idx < *e) {
                continue;
            }
            let line = source[..idx].matches('\n').count() + 1;
            let text = source.lines().nth(line - 1).unwrap_or("").trim();
            offenders.push(format!("{rel}:{line}: {text}"));
        }
        offenders
    }

    /// P2 certification: the permissive (default-allow) transport constructor
    /// may be referenced ONLY from test-gated code, anywhere in the tree.
    /// The constructor is also compile-gated behind
    /// `#[cfg(any(test, feature = "test-utils"))]`; this scan is the belt to
    /// the manifest's braces — a future edit that removes the gate or leaks
    /// `test-utils` into a production dependency graph fails here (and the
    /// production build would fail to resolve the constructor anyway).
    #[test]
    fn no_permissive_ctor_reference_outside_test_code() {
        let workspace = crates_root()
            .parent()
            .expect("crates/ has a parent")
            .to_path_buf();
        let mut offenders: Vec<String> = Vec::new();
        let mut scanned = 0usize;
        let mut hits = 0usize;
        for (root, skip_rel) in [
            (
                workspace.join("crates"),
                Some("crates/provider/src/egress.rs".to_string()),
            ),
            (workspace.join("tests"), None),
        ] {
            let mut stack = vec![root];
            while let Some(dir) = stack.pop() {
                let Ok(entries) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    let Ok(file_type) = entry.file_type() else {
                        continue;
                    };
                    if file_type.is_dir() {
                        stack.push(path);
                        continue;
                    }
                    if !(file_type.is_file()
                        && path.extension().and_then(|e| e.to_str()) == Some("rs"))
                    {
                        continue;
                    }
                    scanned += 1;
                    let rel = path
                        .strip_prefix(&workspace)
                        .unwrap_or(&path)
                        .display()
                        .to_string();
                    let rel = normalize_rel(&rel);
                    if skip_rel.as_deref() == Some(rel.as_str()) {
                        continue; // egress.rs owns the definition
                    }
                    if rel.starts_with("tests/") || rel.contains("/tests/") {
                        continue; // test-only layout
                    }
                    let Ok(source) = std::fs::read_to_string(&path) else {
                        continue;
                    };
                    hits += mask_noncode(&source)
                        .matches(PERMISSIVE_CTOR_MARKER)
                        .count();
                    offenders.extend(permissive_ctor_offenders(&rel, &source));
                }
            }
        }
        assert!(scanned >= 10, "permissive scan walked nothing: {scanned}");
        assert!(
            hits >= 6,
            "permissive scan saw no constructor reference at all ({hits} hits): \
             the walk is not covering the test helpers"
        );
        assert!(
            offenders.is_empty(),
            "the permissive egress constructor is referenced outside test-gated code:\n  {}\n\
             Production must receive the injected policy-checked Arc<dyn HttpTransport>; \
             PolicyCheckedHttpTransport::permissive is cfg(test)/test-utils only.",
            offenders.join("\n  ")
        );

        // Adversarial self-check: a production reference fires; the same
        // reference under a test gate (or a string literal) does not.
        let production =
            "fn build(c: C) -> P { build(c, Arc::new(PolicyCheckedHttpTransport::permissive())) }";
        let found = permissive_ctor_offenders("openai/src/lib.rs", production);
        assert_eq!(found.len(), 1, "{found:?}");
        let gated = "#[cfg(test)]\nfn helper(c: C) -> P { Arc::new(PolicyCheckedHttpTransport::permissive()) }\n";
        assert!(permissive_ctor_offenders("openai/src/lib.rs", gated).is_empty());
        let literal = "const DOC: &str = \"PolicyCheckedHttpTransport::permissive\";";
        assert!(permissive_ctor_offenders("openai/src/lib.rs", literal).is_empty());
    }

    #[test]
    fn transport_ctor_scan_splits_production_from_test_helpers() {
        // A production constructor is an offender; the cfg(test) helper and
        // the cfg(test) module beside it are exempt.
        let mixed = r#"
pub fn build(config: C) -> P {
    Arc::new(PolicyCheckedHttpTransport::permissive())
}
#[cfg(test)]
pub fn permissive_for_tests(config: C) -> P {
    build(config, Arc::new(PolicyCheckedHttpTransport::permissive()))
}
#[cfg(all(test, feature = "x"))]
mod tests {
    fn t() { let _ = PolicyCheckedHttpTransport::permissive(); }
}
"#;
        let offenders = transport_ctor_offenders("openai/src/lib.rs", mixed);
        assert_eq!(offenders.len(), 1, "{offenders:?}");
        assert!(
            offenders[0].starts_with("openai/src/lib.rs:3:"),
            "{offenders:?}"
        );

        // `#[cfg(not(test))]` is production: it must NOT exempt.
        let not_test = r#"
#[cfg(not(test))]
fn build(config: C) -> P {
    Arc::new(PolicyCheckedHttpTransport::permissive())
}
"#;
        assert_eq!(
            transport_ctor_offenders("x/src/lib.rs", not_test).len(),
            1,
            "cfg(not(test)) is production code"
        );

        // A self-built policy transport (or the other default-allow spelling,
        // `with_policy(None)`) is caught by the same prefix scan.
        let self_built = r#"
pub fn build(config: C) -> P {
    let c = CheckedHttpClient::permissive();
    let t = PolicyCheckedHttpTransport::permissive();
    let _ = (c, t);
    build_over(config)
}
#[cfg(test)]
fn build_over(config: C) -> P { panic!() }
"#;
        let offenders = transport_ctor_offenders("x/src/lib.rs", self_built);
        assert_eq!(offenders.len(), 2, "{offenders:?}");
        assert!(
            offenders.iter().all(|o| o.starts_with("x/src/lib.rs:")),
            "{offenders:?}"
        );

        // Braces and marker text inside literals, raw strings and comments
        // neither trip nor desynchronize the scan.
        let tricky = r##"
pub fn ok(config: C, transport: Arc<dyn HttpTransport>) -> P {
    let _doc = "PolicyCheckedHttpTransport::permissive() inside a string {";
    let _raw = r#"{"a": "}"}"#;
    // PolicyCheckedHttpTransport::permissive() in a comment {{
    build(config, transport)
}
#[cfg(test)]
mod tests {
    fn t() {
        // the `}` above must not close this module early
        let _ = PolicyCheckedHttpTransport::permissive();
    }
}
"##;
        assert!(
            transport_ctor_offenders("x/src/lib.rs", tricky).is_empty(),
            "literals/comments must be masked: {:?}",
            transport_ctor_offenders("x/src/lib.rs", tricky)
        );
    }

    // --------------------------------- redirect-policy source scan

    /// Any builder call installing a redirect policy. The checked client is
    /// the ONE place allowed to name it (and only with `Policy::none()`);
    /// everywhere else an automatic follower would bypass the per-hop gate.
    const REDIRECT_MARKER: &str = ".redirect(";

    /// Offenders of one file: every `.redirect(` outside a test-gated item.
    fn redirect_marker_offenders(rel: &str, source: &str) -> Vec<String> {
        let masked = mask_noncode(source);
        let spans = test_gated_spans(&masked);
        let mut offenders = Vec::new();
        for (idx, _) in masked.match_indices(REDIRECT_MARKER) {
            if spans.iter().any(|(s, e)| idx >= *s && idx < *e) {
                continue;
            }
            let line = source[..idx].matches('\n').count() + 1;
            let text = source.lines().nth(line - 1).unwrap_or("").trim();
            offenders.push(format!("{rel}:{line}: {text}"));
        }
        offenders
    }

    /// Regression certification: no production code in the workspace may
    /// install a redirect policy except the checked egress client itself
    /// (which installs `Policy::none()` and follows redirects through the
    /// per-hop gate). Test-gated items and `/tests/`-layout files may use
    /// redirects freely — the scan is about production egress.
    #[test]
    fn no_redirect_policy_outside_the_checked_client() {
        let crates_root = crates_root();
        let workspace = crates_root
            .parent()
            .expect("crates/ has a parent")
            .to_path_buf();
        let mut offenders: Vec<String> = Vec::new();
        let mut scanned = 0usize;
        for root in [workspace.join("crates"), workspace.join("tests")] {
            let mut stack = vec![root];
            while let Some(dir) = stack.pop() {
                let Ok(entries) = std::fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    let Ok(file_type) = entry.file_type() else {
                        continue;
                    };
                    if file_type.is_dir() {
                        stack.push(path);
                        continue;
                    }
                    if !(file_type.is_file()
                        && path.extension().and_then(|e| e.to_str()) == Some("rs"))
                    {
                        continue;
                    }
                    scanned += 1;
                    let rel = path
                        .strip_prefix(&workspace)
                        .unwrap_or(&path)
                        .display()
                        .to_string();
                    let rel = normalize_rel(&rel);
                    if rel == "crates/provider/src/egress.rs" || is_test_rel_at(&workspace, &rel) {
                        continue; // the ONE checked client / test-only sources
                    }
                    let Ok(source) = std::fs::read_to_string(&path) else {
                        continue;
                    };
                    offenders.extend(redirect_marker_offenders(&rel, &source));
                }
            }
        }
        assert!(
            scanned >= 10,
            "redirect-policy scan walked nothing: {scanned}"
        );
        assert!(
            offenders.is_empty(),
            "a redirect policy is installed outside crates/provider/src/egress.rs:\n  {}\n\
             Automatic redirect following would bypass the per-hop destination gate; \
             production clients must use reqwest::redirect::Policy::none() and route \
             redirects through CheckedHttpClient::execute.",
            offenders.join("\n  ")
        );

        // Positive control: the checked client DOES install the redirect
        // policy (the scan above would be vacuous if the marker spelling
        // drifted); and its only spelling is `Policy::none()`.
        let own = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/egress.rs"),
        )
        .expect("egress.rs is readable");
        let own_masked = mask_noncode(&own);
        assert!(
            own_masked.matches(REDIRECT_MARKER).count() >= 1,
            "the checked client must install a redirect policy explicitly"
        );
        assert!(
            own_masked.contains(".redirect(reqwest::redirect::Policy::none())"),
            "the checked client's redirect policy must be Policy::none()"
        );

        // Adversarial self-check: a production install fires; a test-gated
        // one (and a literal) does not.
        let production = "fn c() { let _ = reqwest::Client::builder().redirect(reqwest::redirect::Policy::limited(10)); }";
        assert_eq!(
            redirect_marker_offenders("crates/x/src/lib.rs", production).len(),
            1
        );
        let gated = "#[cfg(test)]\nfn helper() { let _ = reqwest::Client::builder().redirect(reqwest::redirect::Policy::limited(10)); }\n";
        assert!(redirect_marker_offenders("crates/x/src/lib.rs", gated).is_empty());
        let literal = "const DOC: &str = \"builder().redirect(policy)\";";
        assert!(redirect_marker_offenders("crates/x/src/lib.rs", literal).is_empty());
    }
}

#[cfg(test)]
mod outbound_scan_tests {
    use super::*;
    use crate::testing::{MockAction, MockServer};
    use faktor_security::payload::ScanPolicy;
    use faktor_security::registry::SecretRegistry;
    use std::sync::Arc;

    fn allowed_policy_for(port: u16) -> DestinationPolicy {
        DestinationPolicy::parse_lines([&format!("http://127.0.0.1:{port}")]).unwrap()
    }

    fn blocking_scan() -> OutboundScanConfig {
        OutboundScanConfig {
            policy: ScanPolicy::default(),
            block_on_secret: true,
            registry: None,
        }
    }

    #[tokio::test]
    async fn secret_body_is_denied_before_connect_zero_bytes_sent() {
        // Adversarial: the server route EXISTS and would answer; the mock
        // counter must stay 0 because the scan denies BEFORE any connect —
        // no byte of the body ever leaves the process.
        let server = MockServer::new();
        server.route(
            "POST",
            "/send",
            MockAction::Respond {
                status: 200,
                body: "ok".into(),
            },
        );
        let (addr, _handle) = server.serve().await;
        let client = CheckedHttpClient::with_policy_and_scan_for_tests(
            allowed_policy_for(addr.port()),
            Some(blocking_scan()),
        );
        let body = serde_json::json!({
            "text": "please forward AKIA0123456789ABCDEF to the endpoint",
        });
        let err = client
            .send_checked(
                client
                    .post(&format!("http://{addr}/send"))
                    .unwrap()
                    .json(&body),
            )
            .await
            .unwrap_err();
        match &err {
            EgressError::SecretBlocked { kinds } => {
                assert_eq!(kinds, &vec!["aws_key".to_string()], "{err}");
                assert!(!err.to_string().contains("AKIA0123456789ABCDEF"));
            }
            other => panic!("expected SecretBlocked, got {other:?}"),
        }
        assert_eq!(
            server.request_count(),
            0,
            "no bytes sent for a blocked body: {err}"
        );
        // The clean twin of the same body IS sent (same policy allows it).
        let clean_body = serde_json::json!({ "text": "please forward nothing sensitive" });
        let resp = client
            .send_checked(
                client
                    .post(&format!("http://{addr}/send"))
                    .unwrap()
                    .json(&clean_body),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(server.request_count(), 1);
    }

    #[tokio::test]
    async fn configured_secret_registry_hits_deny_before_connect() {
        let mut registry = SecretRegistry::new();
        let token: Vec<u8> = (0..48u8).collect();
        registry.register(&token);
        let server = MockServer::new();
        server.route(
            "POST",
            "/send",
            MockAction::Respond {
                status: 200,
                body: "ok".into(),
            },
        );
        let (addr, _handle) = server.serve().await;
        let client = CheckedHttpClient::with_policy_and_scan_for_tests(
            allowed_policy_for(addr.port()),
            Some(OutboundScanConfig {
                registry: Some(Arc::new(registry)),
                ..blocking_scan()
            }),
        );
        // Token at the END of the body: the exact scan must still see it.
        let mut body_bytes = b"prefix content ".to_vec();
        body_bytes.extend_from_slice(&token);
        let err = client
            .send_checked(
                client
                    .post(&format!("http://{addr}/send"))
                    .unwrap()
                    .body(body_bytes),
            )
            .await
            .unwrap_err();
        match &err {
            EgressError::SecretBlocked { kinds } => {
                assert!(kinds.iter().any(|k| k == "configured_secret"), "{err}");
            }
            other => panic!("expected SecretBlocked, got {other:?}"),
        }
        assert_eq!(
            server.request_count(),
            0,
            "registry hit denies before connect"
        );
        // The token value never appears in the error text.
        assert!(!err.to_string().contains("content"), "{err}");
        assert!(!format!("{err:?}").contains("48"));
    }

    #[tokio::test]
    async fn oversized_body_is_too_large_before_connect() {
        let server = MockServer::new();
        server.route(
            "POST",
            "/send",
            MockAction::Respond {
                status: 200,
                body: "ok".into(),
            },
        );
        let (addr, _handle) = server.serve().await;
        let client = CheckedHttpClient::with_policy_and_scan_for_tests(
            allowed_policy_for(addr.port()),
            Some(OutboundScanConfig {
                policy: ScanPolicy {
                    max_payload_bytes: Some(16),
                    ..ScanPolicy::default()
                },
                ..blocking_scan()
            }),
        );
        let body = "x".repeat(64);
        let err = client
            .send_checked(
                client
                    .post(&format!("http://{addr}/send"))
                    .unwrap()
                    .body(body),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, EgressError::BodyTooLarge { limit_bytes: 16 }),
            "{err:?}"
        );
        assert_eq!(server.request_count(), 0, "too-large denies before connect");
    }

    #[tokio::test]
    async fn deny_allowed_by_policy_sends_and_warns() {
        // block_on_secret=false: Found still sends (deny-allowed-by-policy
        // behaviour stays) — the mock receives exactly one request.
        let server = MockServer::new();
        server.route(
            "POST",
            "/send",
            MockAction::Respond {
                status: 200,
                body: "ok".into(),
            },
        );
        let (addr, _handle) = server.serve().await;
        let client = CheckedHttpClient::with_policy_and_scan_for_tests(
            allowed_policy_for(addr.port()),
            Some(OutboundScanConfig {
                block_on_secret: false,
                ..blocking_scan()
            }),
        );
        let resp = client
            .send_checked(
                client
                    .post(&format!("http://{addr}/send"))
                    .unwrap()
                    .body("sk-0123456789abcdefghijklmnopqrstuv"),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(server.request_count(), 1);
    }

    #[tokio::test]
    async fn streamed_body_under_scan_config_fails_closed_before_connect() {
        // A streamed (non-materialized) body cannot be scanned in full:
        // the send refuses with a typed error before any connect rather
        // than sending bytes that were never inspected.
        let server = MockServer::new();
        server.route(
            "POST",
            "/send",
            MockAction::Respond {
                status: 200,
                body: "ok".into(),
            },
        );
        let (addr, _handle) = server.serve().await;
        let client = CheckedHttpClient::with_policy_and_scan_for_tests(
            allowed_policy_for(addr.port()),
            Some(blocking_scan()),
        );
        // The body holds an actual secret — but as an un-materialized
        // stream, so no full-payload scan is possible. Fail closed.
        let stream =
            futures::stream::iter(vec![Ok::<_, std::io::Error>(&b"AKIA0123456789ABCDEF"[..])]);
        let req = client
            .post(&format!("http://{addr}/send"))
            .unwrap()
            .body(reqwest::Body::wrap_stream(stream))
            .build()
            .unwrap();
        let err = client.execute(req).await.unwrap_err();
        assert!(
            matches!(&err, EgressError::BodyNotMaterialized(_)),
            "{err:?}"
        );
        assert_eq!(server.request_count(), 0, "unscannable body never connects");
        // No-body request under a scan config passes (nothing to scan).
        let resp = client
            .send_checked(client.get(&format!("http://{addr}/nobody")).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), 404, "unrouted path answers loudly");
        assert_eq!(server.request_count(), 1);
    }

    #[tokio::test]
    async fn transport_seam_applies_the_same_gate() {
        // PolicyCheckedHttpTransport (the wave-11 adapter seam) carries the
        // scan config into the same choke point.
        let server = MockServer::new();
        server.route(
            "POST",
            "/send",
            MockAction::Respond {
                status: 200,
                body: "ok".into(),
            },
        );
        let (addr, _handle) = server.serve().await;
        let transport = PolicyCheckedHttpTransport::with_policy_and_scan_for_tests(
            allowed_policy_for(addr.port()),
            Some(blocking_scan()),
        );
        assert!(transport.outbound_scan().is_some());
        let url = format!("http://{addr}/send");
        let body = serde_json::json!({ "key": "ghp_0123456789abcdefghijklmnopqrstuv" });
        let err = execute_post_json(&transport, &url, HeaderMap::new(), &body)
            .await
            .unwrap_err();
        assert!(matches!(err, EgressError::SecretBlocked { .. }), "{err:?}");
        assert_eq!(server.request_count(), 0);
        // Same transport, clean body: sent.
        let ok_body = serde_json::json!({ "key": "benign" });
        let resp = execute_post_json(&transport, &url, HeaderMap::new(), &ok_body)
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(server.request_count(), 1);
    }
}

#[cfg(test)]
mod redirect_tests {
    use super::*;
    use faktor_security::destination::{DestinationPolicy, RuleMatch};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// One scripted route of the redirect test server.
    #[derive(Clone)]
    struct Route {
        status: u16,
        location: Option<String>,
        body: String,
    }

    impl Route {
        fn respond(status: u16, body: &str) -> Self {
            Self {
                status,
                location: None,
                body: body.to_string(),
            }
        }

        fn redirect(status: u16, location: &str) -> Self {
            Self {
                status,
                location: Some(location.to_string()),
                body: String::new(),
            }
        }
    }

    /// One recorded request: method, path (query stripped, like MockServer),
    /// lowercased headers and the exact body bytes.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct SeenRequest {
        method: String,
        path: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl SeenRequest {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }
    }

    /// Minimal scripted HTTP server for redirect behavior: routes
    /// `(method, path)` to one status/Location/body and records every
    /// request including headers and body. The stock `MockServer` cannot
    /// emit `Location`, and redirects are exactly what must be adversarial.
    #[derive(Default)]
    struct RedirectServer {
        routes: Mutex<HashMap<(String, String), Route>>,
        seen: Mutex<Vec<SeenRequest>>,
    }

    impl RedirectServer {
        fn new() -> Arc<Self> {
            Arc::new(Self::default())
        }

        fn route(&self, method: &str, path: &str, route: Route) {
            self.routes
                .lock()
                .unwrap()
                .insert((method.to_string(), path.to_string()), route);
        }

        fn seen(&self) -> Vec<SeenRequest> {
            self.seen.lock().unwrap().clone()
        }

        fn count(&self) -> usize {
            self.seen.lock().unwrap().len()
        }

        async fn serve(self: &Arc<Self>) -> std::net::SocketAddr {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .expect("redirect server bind");
            let addr = listener.local_addr().expect("local addr");
            let me = self.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((socket, _)) = listener.accept().await else {
                        break;
                    };
                    let me = me.clone();
                    tokio::spawn(async move { handle_conn(socket, me).await });
                }
            });
            addr
        }
    }

    async fn handle_conn(mut socket: tokio::net::TcpStream, server: Arc<RedirectServer>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        let header_end;
        loop {
            let n = match socket.read(&mut tmp).await {
                Ok(n) => n,
                Err(_) => return,
            };
            if n == 0 {
                return;
            }
            buf.extend_from_slice(&tmp[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                header_end = pos + 4;
                break;
            }
            if buf.len() > 64 * 1024 {
                return;
            }
        }
        let header = String::from_utf8_lossy(&buf[..header_end]).to_string();
        let mut lines = header.lines();
        let request_line = lines.next().unwrap_or("");
        let mut parts = request_line.split(' ');
        let method = parts.next().unwrap_or("").to_string();
        let path = parts
            .next()
            .unwrap_or("")
            .split('?')
            .next()
            .unwrap_or("")
            .to_string();
        let mut content_length = 0usize;
        let mut headers: Vec<(String, String)> = Vec::new();
        for line in lines {
            if let Some(v) = line.to_lowercase().strip_prefix("content-length:") {
                content_length = v.trim().parse().unwrap_or(0);
            }
            if let Some((name, value)) = line.split_once(':') {
                headers.push((name.trim().to_lowercase(), value.trim().to_string()));
            }
        }
        while buf.len() < header_end + content_length {
            let n = socket.read(&mut tmp).await.unwrap_or_default();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
        }
        let body = buf[header_end..(header_end + content_length).min(buf.len())].to_vec();
        server.seen.lock().unwrap().push(SeenRequest {
            method: method.clone(),
            path: path.clone(),
            headers,
            body,
        });

        let route = server.routes.lock().unwrap().get(&(method, path)).cloned();
        let response = match route {
            None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .to_string(),
            Some(route) => {
                let mut head = format!(
                    "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                    route.status,
                    route.body.len()
                );
                if let Some(location) = &route.location {
                    head.push_str(&format!("Location: {location}\r\n"));
                }
                head.push_str("\r\n");
                head.push_str(&route.body);
                head
            }
        };
        let _ = socket.write_all(response.as_bytes()).await;
    }

    fn policy_for<I, S>(urls: I) -> DestinationPolicy
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        DestinationPolicy::parse_lines(urls).expect("test policy parses")
    }

    /// The audit defect: a cross-host `Location` target must be refused by
    /// the SAME allowlist logic before the next hop connects. The target
    /// differs only by HOST TEXT (`localhost` vs the allowlisted `127.0.0.1`
    /// literal), the rebinding-style bypass class; the second server must
    /// record ZERO requests.
    #[tokio::test]
    async fn cross_host_redirect_is_denied_and_the_second_hop_never_connects() {
        let first = RedirectServer::new();
        let second = RedirectServer::new();
        let first_addr = first.serve().await;
        let second_addr = second.serve().await;
        first.route(
            "GET",
            "/start",
            Route::redirect(
                302,
                &format!("http://localhost:{}/exfil", second_addr.port()),
            ),
        );
        second.route("GET", "/exfil", Route::respond(200, "stolen"));
        let policy = policy_for([
            format!("http://127.0.0.1:{}", first_addr.port()),
            format!("http://127.0.0.1:{}", second_addr.port()),
        ]);
        let client = CheckedHttpClient::with_policy_for_tests(policy);

        let err = client
            .send_checked(
                client
                    .get(&format!("http://127.0.0.1:{}/start", first_addr.port()))
                    .unwrap(),
            )
            .await
            .unwrap_err();
        match &err {
            EgressError::Denied { url, reason } => {
                assert_eq!(reason.matched, RuleMatch::None, "{err}");
                assert_eq!(url.host(), "localhost", "{err}");
            }
            other => panic!("expected a typed Denied, got {other:?}"),
        }
        assert_eq!(first.count(), 1, "the allowed first hop was sent");
        assert_eq!(
            second.count(),
            0,
            "the denied redirect target must never be requested"
        );
    }

    /// Same-host path redirects are followed, in order, each hop passing the
    /// policy; the final response streams back to the caller.
    #[tokio::test]
    async fn same_host_redirects_are_followed_hop_by_hop() {
        let server = RedirectServer::new();
        let addr = server.serve().await;
        server.route("GET", "/a", Route::redirect(302, "/b"));
        server.route("GET", "/b", Route::redirect(301, "/c"));
        server.route("GET", "/c", Route::respond(200, "done"));
        let client = CheckedHttpClient::with_policy_for_tests(policy_for([format!(
            "http://127.0.0.1:{}",
            addr.port()
        )]));

        let resp = client
            .send_checked(
                client
                    .get(&format!("http://127.0.0.1:{}/a", addr.port()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.text().await.unwrap(), "done");
        let paths: Vec<String> = server.seen().into_iter().map(|r| r.path).collect();
        assert_eq!(paths, ["/a", "/b", "/c"]);
    }

    /// The policy is re-enforced on EVERY hop, not just the first: an
    /// allowed first and second hop redirecting to a denied third target is
    /// refused before that target is contacted.
    #[tokio::test]
    async fn policy_is_re_enforced_after_several_hops() {
        let first = RedirectServer::new();
        let second = RedirectServer::new();
        let first_addr = first.serve().await;
        let second_addr = second.serve().await;
        first.route("GET", "/start", Route::redirect(302, "/middle"));
        first.route(
            "GET",
            "/middle",
            Route::redirect(
                302,
                &format!("http://localhost:{}/exfil", second_addr.port()),
            ),
        );
        second.route("GET", "/exfil", Route::respond(200, "stolen"));
        let policy = policy_for([
            format!("http://127.0.0.1:{}", first_addr.port()),
            format!("http://127.0.0.1:{}", second_addr.port()),
        ]);
        let client = CheckedHttpClient::with_policy_for_tests(policy);

        let err = client
            .send_checked(
                client
                    .get(&format!("http://127.0.0.1:{}/start", first_addr.port()))
                    .unwrap(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, EgressError::Denied { .. }), "{err:?}");
        assert_eq!(first.count(), 2, "both allowed hops were sent");
        assert_eq!(second.count(), 0, "the denied third hop never connected");
    }

    /// The hop bound is small, typed, and off-by-one adversarial: exactly
    /// `MAX_REDIRECT_HOPS` hops are followed (`limit + 1` requests sent) and
    /// the next redirect is refused.
    #[tokio::test]
    async fn redirect_hop_limit_is_typed_and_bounded() {
        let server = RedirectServer::new();
        let addr = server.serve().await;
        server.route("GET", "/loop", Route::redirect(302, "/loop"));
        let client = CheckedHttpClient::with_policy_for_tests(policy_for([format!(
            "http://127.0.0.1:{}",
            addr.port()
        )]));

        let err = client
            .send_checked(
                client
                    .get(&format!("http://127.0.0.1:{}/loop", addr.port()))
                    .unwrap(),
            )
            .await
            .unwrap_err();
        assert_eq!(
            err,
            EgressError::TooManyRedirects {
                limit: MAX_REDIRECT_HOPS,
                url: SafeUrlDiagnostic::from_raw(&format!("http://127.0.0.1:{}/loop", addr.port())),
            }
        );
        assert_eq!(
            server.count(),
            MAX_REDIRECT_HOPS + 1,
            "the refusal lands before the hop that would exceed the bound"
        );
    }

    /// Method/body semantics match the reqwest follower this replaced:
    /// 303 => GET (HEAD stays HEAD), 301/302 POST => GET, other 301/302
    /// methods preserved, 307/308 preserve method+body; payload headers are
    /// dropped on GET conversion.
    #[tokio::test]
    async fn redirect_method_and_body_semantics_are_preserved() {
        let server = RedirectServer::new();
        let addr = server.serve().await;
        for (path, status) in [
            ("/see-other", 303),
            ("/moved-post", 301),
            ("/found-post", 302),
            ("/temporary", 307),
            ("/permanent", 308),
        ] {
            server.route("POST", path, Route::redirect(status, "/done"));
        }
        server.route("POST", "/done", Route::respond(200, "ok"));
        // GET conversions (303 / POST 301 / POST 302) land on the same path.
        server.route("GET", "/done", Route::respond(200, "ok"));
        server.route("GET", "/moved-get", Route::redirect(301, "/done-get"));
        server.route("GET", "/done-get", Route::respond(200, "ok"));
        server.route(
            "HEAD",
            "/see-other-head",
            Route::redirect(303, "/done-head"),
        );
        server.route("HEAD", "/done-head", Route::respond(200, ""));
        let client = CheckedHttpClient::with_policy_for_tests(policy_for([format!(
            "http://127.0.0.1:{}",
            addr.port()
        )]));
        let base = format!("http://127.0.0.1:{}", addr.port());

        for path in [
            "/see-other",
            "/moved-post",
            "/found-post",
            "/temporary",
            "/permanent",
        ] {
            let request = client
                .post(&format!("{base}{path}"))
                .unwrap()
                .header("content-type", "text/plain")
                .body("payload")
                .build()
                .unwrap();
            let resp = client.execute(request).await.unwrap();
            assert_eq!(resp.status(), 200, "{path}");
        }
        // GET 301 keeps GET (no conversion, no body).
        let resp = client
            .send_checked(client.get(&format!("{base}/moved-get")).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        // 303 from HEAD stays HEAD (tower-http semantics).
        let request = reqwest::Request::new(
            reqwest::Method::HEAD,
            Url::parse(&format!("{base}/see-other-head")).unwrap(),
        );
        let resp = client.execute(request).await.unwrap();
        assert_eq!(resp.status(), 200);

        let seen = server.seen();
        let pair = |i: usize| (&seen[i], &seen[i + 1]);
        for (start, start_path, follow_status, want_method) in [
            (0usize, "/see-other", 303u16, "GET"),
            (2, "/moved-post", 301, "GET"),
            (4, "/found-post", 302, "GET"),
            (6, "/temporary", 307, "POST"),
            (8, "/permanent", 308, "POST"),
        ] {
            let (first, second) = pair(start);
            assert_eq!(first.path, start_path, "script order");
            assert_eq!(
                second.method, want_method,
                "status {follow_status} from {}",
                first.path
            );
            assert_eq!(second.path, "/done");
            if want_method == "GET" {
                assert!(second.body.is_empty(), "GET conversion drops the body");
                assert_eq!(
                    second.header("content-type"),
                    None,
                    "payload headers dropped"
                );
                assert_eq!(second.header("content-length"), None);
            } else {
                assert_eq!(second.body, b"payload", "307/308 replay the exact body");
            }
        }
        let (moved_get, done_get) = pair(10);
        assert_eq!(
            (moved_get.method.as_str(), done_get.method.as_str()),
            ("GET", "GET")
        );
        let (head_start, head_follow) = pair(12);
        assert_eq!(
            (head_start.method.as_str(), head_follow.method.as_str()),
            ("HEAD", "HEAD"),
            "303 from HEAD keeps HEAD"
        );
    }

    /// Credentials never cross an origin change (scheme/host/port), and are
    /// kept on same-origin hops — the documented stripping rule.
    #[tokio::test]
    async fn credentials_are_stripped_across_origins_and_kept_same_origin() {
        let first = RedirectServer::new();
        let second = RedirectServer::new();
        let first_addr = first.serve().await;
        let second_addr = second.serve().await;
        first.route(
            "GET",
            "/start",
            Route::redirect(
                302,
                &format!("http://127.0.0.1:{}/final", second_addr.port()),
            ),
        );
        first.route("GET", "/same-start", Route::redirect(302, "/same-final"));
        first.route("GET", "/same-final", Route::respond(200, "ok"));
        second.route("GET", "/final", Route::respond(200, "ok"));
        let policy = policy_for([
            format!("http://127.0.0.1:{}", first_addr.port()),
            format!("http://127.0.0.1:{}", second_addr.port()),
        ]);
        let client = CheckedHttpClient::with_policy_for_tests(policy);
        let base = format!("http://127.0.0.1:{}", first_addr.port());

        // Port change => cross-origin: credentials stripped.
        let resp = client
            .send_checked(
                client
                    .get(&format!("{base}/start"))
                    .unwrap()
                    .header("authorization", "Bearer top-secret")
                    .header("cookie", "session=1")
                    .header("accept", "application/json"),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let cross = second.seen();
        assert_eq!(cross.len(), 1);
        assert_eq!(
            cross[0].header("authorization"),
            None,
            "no auth across origins"
        );
        assert_eq!(cross[0].header("cookie"), None, "no cookie across origins");
        assert_eq!(cross[0].header("accept"), Some("application/json"));

        // Same origin (path-only change): credentials kept.
        let resp = client
            .send_checked(
                client
                    .get(&format!("{base}/same-start"))
                    .unwrap()
                    .header("authorization", "Bearer top-secret")
                    .header("cookie", "session=1"),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let same = first.seen();
        let final_hop = same.last().expect("same-origin hop recorded");
        assert_eq!(final_hop.path, "/same-final");
        assert_eq!(final_hop.header("authorization"), Some("Bearer top-secret"));
        assert_eq!(final_hop.header("cookie"), Some("session=1"));
    }

    /// A 3xx without `Location` is not a redirect: the response is surfaced
    /// exactly as the previous follower did (no error, no second request).
    #[tokio::test]
    async fn redirect_without_location_is_returned_unfollowed() {
        let server = RedirectServer::new();
        let addr = server.serve().await;
        server.route("GET", "/noloc", Route::respond(302, "moved but nowhere"));
        let client = CheckedHttpClient::with_policy_for_tests(policy_for([format!(
            "http://127.0.0.1:{}",
            addr.port()
        )]));
        let resp = client
            .send_checked(
                client
                    .get(&format!("http://127.0.0.1:{}/noloc", addr.port()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 302);
        assert_eq!(resp.text().await.unwrap(), "moved but nowhere");
        assert_eq!(server.count(), 1);
    }

    /// 307/308 preserve the body; a streamed body cannot be replayed and is
    /// refused TYPED (never silently re-sent body-less or converted to GET).
    #[tokio::test]
    async fn streamed_body_redirect_is_refused_typed() {
        let server = RedirectServer::new();
        let addr = server.serve().await;
        server.route("POST", "/stream", Route::redirect(307, "/target"));
        server.route("POST", "/target", Route::respond(200, "ok"));
        let client = CheckedHttpClient::with_policy_for_tests(policy_for([format!(
            "http://127.0.0.1:{}",
            addr.port()
        )]));
        let stream = futures::stream::iter(vec![Ok::<_, std::io::Error>(&b"chunk"[..])]);
        let request = client
            .post(&format!("http://127.0.0.1:{}/stream", addr.port()))
            .unwrap()
            .body(reqwest::Body::wrap_stream(stream))
            .build()
            .unwrap();
        let err = client.execute(request).await.unwrap_err();
        assert!(
            matches!(&err, EgressError::RedirectBodyNotReplayable { url } if url.path_segments() == 1),
            "{err:?}"
        );
        assert!(!err.to_string().contains("/stream"), "{err}");
        assert_eq!(server.count(), 1, "only the checked first hop was sent");
    }

    /// The outbound secret scan runs on every hop's body: a 307 whose
    /// follow-up would replay a secret body is refused before ANY connect,
    /// and a clean body follows the chain.
    #[tokio::test]
    async fn outbound_scan_is_enforced_before_every_hop() {
        let server = RedirectServer::new();
        let addr = server.serve().await;
        server.route("POST", "/send", Route::redirect(307, "/final"));
        server.route("POST", "/final", Route::respond(200, "ok"));
        let client = CheckedHttpClient::with_policy_and_scan_for_tests(
            policy_for([format!("http://127.0.0.1:{}", addr.port())]),
            Some(OutboundScanConfig {
                policy: faktor_security::payload::ScanPolicy::default(),
                block_on_secret: true,
                registry: None,
            }),
        );
        let url = format!("http://127.0.0.1:{}/send", addr.port());

        let resp = client
            .send_checked(client.post(&url).unwrap().body("clean payload"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let followed = server.seen();
        assert_eq!(
            followed.len(),
            2,
            "clean 307 body was replayed to the final hop"
        );
        assert_eq!(followed[1].body, b"clean payload");

        let err = client
            .send_checked(client.post(&url).unwrap().body("AKIA0123456789ABCDEF"))
            .await
            .unwrap_err();
        assert!(matches!(err, EgressError::SecretBlocked { .. }), "{err:?}");
        assert_eq!(server.count(), 2, "no hop was sent for the blocked body");
    }

    /// Constructor-level regression: the clients the checked constructors
    /// build must NOT follow redirects on their own (the exact defect was a
    /// default-policy client silently following past the gate).
    #[tokio::test]
    async fn checked_constructors_build_redirect_disabled_clients() {
        let server = RedirectServer::new();
        let addr = server.serve().await;
        server.route("GET", "/go", Route::redirect(302, "/final"));
        server.route("GET", "/final", Route::respond(200, "final"));
        let url = format!("http://127.0.0.1:{}/go", addr.port());

        let raw = try_default_timeout_client(EgressResolver::system(EgressAddressPolicy::LOCAL))
            .expect("client build");
        let resp = raw.get(&url).send().await.unwrap();
        assert_eq!(
            resp.status(),
            302,
            "try_default_timeout_client must not follow"
        );

        let checked = CheckedHttpClient::permissive();
        let resp = checked.inner.get(&url).send().await.unwrap();
        assert_eq!(
            resp.status(),
            302,
            "CheckedHttpClient::with_policy's inner client must not follow"
        );
        assert_eq!(server.count(), 2, "both probes hit the first hop only");
    }

    /// Defense in depth: an injected client that still follows redirects
    /// internally is detected (its final URL differs from the checked hop)
    /// and fails closed instead of handing back unchecked content.
    #[tokio::test]
    async fn injected_auto_following_client_fails_closed() {
        let server = RedirectServer::new();
        let addr = server.serve().await;
        server.route("GET", "/go", Route::redirect(302, "/final"));
        server.route("GET", "/final", Route::respond(200, "unchecked"));
        // A default `reqwest::Client::new()` follows redirects (the defect).
        let client = CheckedHttpClient::new(reqwest::Client::new(), None);
        let err = client
            .send_checked(
                client
                    .get(&format!("http://127.0.0.1:{}/go", addr.port()))
                    .unwrap(),
            )
            .await
            .unwrap_err();
        match &err {
            EgressError::UncheckedRedirectFollowed { from, to } => {
                // Only the path SHAPE survives: `/go` and `/final` are one
                // segment each, and no path bytes are carried anywhere.
                assert_eq!(from.path_segments(), 1, "{err}");
                assert_eq!(to.path_segments(), 1, "{err}");
                assert!(!err.to_string().contains("/go"), "{err}");
                assert!(!err.to_string().contains("/final"), "{err}");
            }
            other => panic!("expected UncheckedRedirectFollowed, got {other:?}"),
        }
        assert_eq!(
            server.count(),
            2,
            "the inner client did follow; we refuse the result"
        );
    }

    /// Planted credential in a redirect `Location`: the denial, the hop
    /// bound, the replay refusal and the unchecked-follower detection must
    /// all render without it.
    #[tokio::test]
    async fn redirect_locations_never_leak_planted_secrets() {
        const SECRET: &str = "REDIRECT-SECRET-77aa";
        let first = RedirectServer::new();
        let second = RedirectServer::new();
        let first_addr = first.serve().await;
        let second_addr = second.serve().await;
        let policy = policy_for([format!("http://127.0.0.1:{}", first_addr.port())]);
        let client = CheckedHttpClient::with_policy_for_tests(policy);
        let base = format!("http://127.0.0.1:{}", first_addr.port());
        let asserts = |err: &EgressError, route: &str| {
            for rendered in [format!("{err}"), format!("{err:?}"), {
                let converted: ProviderError = err.clone().into();
                format!("{converted} / {converted:?}")
            }] {
                assert!(
                    !rendered.contains(SECRET),
                    "{route}: Location secret leaked: {rendered}"
                );
            }
        };

        // 1. Cross-host Location with a secret query/fragment: denied on the
        //    redirect target.
        first.route(
            "GET",
            "/secret-deny",
            Route::redirect(
                302,
                &format!(
                    "http://localhost:{}/exfil?token={SECRET}#{SECRET}",
                    second_addr.port()
                ),
            ),
        );
        let err = client
            .send_checked(client.get(&format!("{base}/secret-deny")).unwrap())
            .await
            .unwrap_err();
        assert!(matches!(err, EgressError::Denied { .. }), "{err:?}");
        asserts(&err, "denied redirect target");
        assert_eq!(second.count(), 0);

        // 2. TooManyRedirects: the hop URL carries the secret query.
        first.route(
            "GET",
            "/secret-loop",
            Route::redirect(302, &format!("/secret-loop?token={SECRET}")),
        );
        let err = client
            .send_checked(client.get(&format!("{base}/secret-loop")).unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(err, EgressError::TooManyRedirects { .. }),
            "{err:?}"
        );
        asserts(&err, "too many redirects");

        // 3. Streamed-body replay refusal from a hop URL with the secret.
        first.route("POST", "/secret-stream", Route::redirect(307, "/target"));
        let stream = futures::stream::iter(vec![Ok::<_, std::io::Error>(&b"chunk"[..])]);
        let request = client
            .post(&format!("{base}/secret-stream?token={SECRET}"))
            .unwrap()
            .body(reqwest::Body::wrap_stream(stream))
            .build()
            .unwrap();
        let err = client.execute(request).await.unwrap_err();
        assert!(
            matches!(err, EgressError::RedirectBodyNotReplayable { .. }),
            "{err:?}"
        );
        asserts(&err, "redirect body not replayable");

        // 4. Unchecked follower: `from`/`to` diagnostics mask the target's
        //    query even though the injected client followed it itself.
        let server = RedirectServer::new();
        let addr = server.serve().await;
        server.route(
            "GET",
            "/go",
            Route::redirect(302, &format!("/final?token={SECRET}#{SECRET}")),
        );
        server.route("GET", "/final", Route::respond(200, "unchecked"));
        let injected = CheckedHttpClient::new(reqwest::Client::new(), None);
        let err = injected
            .send_checked(
                injected
                    .get(&format!("http://127.0.0.1:{}/go", addr.port()))
                    .unwrap(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, EgressError::UncheckedRedirectFollowed { .. }),
            "{err:?}"
        );
        asserts(&err, "unchecked redirect");
    }
}

#[cfg(test)]
mod diagnostic_leak_tests {
    use super::*;
    use faktor_security::destination::{DeniedReason, RuleMatch};

    /// One planted credential value reused across every credential-bearing
    /// URL position (query value, percent-encoded query NAME, userinfo,
    /// fragment). Every failure route must render without it.
    const SECRET: &str = "P0-SECRET-4f2a-QUERY";

    /// Run one error through every consumer-facing rendering an operator,
    /// log line or `ProviderError` conversion can hit.
    fn assert_secret_free(err: &EgressError, route: &str) {
        for (what, rendered) in [
            ("Display", format!("{err}")),
            ("Debug", format!("{err:?}")),
            ("ProviderError::from", {
                let converted: ProviderError = err.clone().into();
                format!("{converted} / {converted:?}")
            }),
        ] {
            assert!(
                !rendered.contains(SECRET),
                "{route}: planted secret leaked through {what}: {rendered}"
            );
        }
    }

    fn denied_reason() -> DeniedReason {
        DeniedReason {
            rule_fired: None,
            matched: RuleMatch::None,
        }
    }

    fn hostile_diagnostic() -> SafeUrlDiagnostic {
        // The secret is planted in EVERY hostile position: path segment,
        // userinfo, query value, percent-encoded query name and fragment.
        let raw = format!(
            "https://user:{SECRET}@allowed.example:8443/benign/{SECRET}/end?a={SECRET}&%73ecret={SECRET}#frag-{SECRET}"
        );
        SafeUrlDiagnostic::from_raw(&raw)
    }

    #[test]
    fn every_url_bearing_variant_is_secret_free_through_all_renderings() {
        let diag = hostile_diagnostic();
        for (route, err) in [
            (
                "Denied",
                EgressError::Denied {
                    url: diag.clone(),
                    reason: denied_reason(),
                },
            ),
            (
                "BodyNotMaterialized",
                EgressError::BodyNotMaterialized(diag.clone()),
            ),
            (
                "AddressClassRefused",
                EgressError::AddressClassRefused {
                    url: diag.clone(),
                    class: AddressClass::Loopback,
                },
            ),
            (
                "DnsAnswerSetTooLarge",
                EgressError::DnsAnswerSetTooLarge {
                    url: diag.clone(),
                    count: 17,
                    limit: 16,
                },
            ),
            (
                "TooManyRedirects",
                EgressError::TooManyRedirects {
                    limit: 10,
                    url: diag.clone(),
                },
            ),
            (
                "RedirectBodyNotReplayable",
                EgressError::RedirectBodyNotReplayable { url: diag.clone() },
            ),
            (
                "UncheckedRedirectFollowed",
                EgressError::UncheckedRedirectFollowed {
                    from: diag.clone(),
                    to: diag.clone(),
                },
            ),
            (
                "UnparseableUrl",
                EgressError::UnparseableUrl(UrlRejectReason::ParseError),
            ),
        ] {
            assert_secret_free(&err, route);
        }
        // The diagnostic itself keeps only the safe shape: path bytes gone
        // (segment count only), userinfo gone, query masked to a marker,
        // fragment gone.
        let rendered = format!("{diag}");
        assert_eq!(
            rendered,
            "https://allowed.example:8443/<redacted-path>?<redacted>"
        );
        assert_eq!(diag.path_segments(), 3);
        assert!(!format!("{diag:?}").contains(SECRET));
    }

    #[test]
    fn hostile_raw_inputs_never_reach_any_message() {
        // A raw string that does not parse at all: only the typed reason.
        let hostile = format!("not a url at all?{SECRET}={SECRET}");
        assert_secret_free(
            &EgressError::UnparseableUrl(UrlRejectReason::ParseError),
            "unparseable",
        );
        assert!(!hostile.is_empty());
        // A raw userinfo URL whose PATH also carries the secret: the
        // diagnostic drops the userinfo, the query and the fragment
        // entirely, and never carries a path byte (the planted
        // percent-encoded segment included).
        let diag = SafeUrlDiagnostic::from_raw(&format!(
            "https://user:{SECRET}@allowed.example/{SECRET}%2f?{SECRET}#{SECRET}"
        ));
        assert!(!diag.host().contains(SECRET));
        let rendered = format!("{diag}");
        assert!(!rendered.contains(SECRET), "{rendered}");
        assert_eq!(
            rendered,
            "https://allowed.example:443/<redacted-path>?<redacted>"
        );
        // Percent-encoded query NAMES are dropped with the whole query.
        let diag = SafeUrlDiagnostic::from_raw(&format!(
            "https://allowed.example/ok?%73ecret%2dname={SECRET}&plain={SECRET}"
        ));
        let rendered = format!("{diag}");
        assert_eq!(
            rendered,
            "https://allowed.example:443/<redacted-path>?<redacted>"
        );
    }

    #[tokio::test]
    async fn live_parse_refusals_are_secret_free() {
        let client = CheckedHttpClient::with_policy_for_tests(
            DestinationPolicy::parse_lines(["https://allowed.example"]).unwrap(),
        );
        for (route, url) in [
            ("userinfo", format!("https://u:{SECRET}@allowed.example/x")),
            ("port0", format!("https://allowed.example:0/x?q={SECRET}")),
            (
                "unparseable",
                format!("definitely not a url ?q={SECRET}&{SECRET}"),
            ),
        ] {
            let err = client.get(&url).unwrap_err();
            assert_secret_free(&err, route);
        }
    }

    #[tokio::test]
    async fn live_policy_denial_is_secret_free() {
        let client = CheckedHttpClient::with_policy_for_tests(
            DestinationPolicy::parse_lines(["https://allowed.example"]).unwrap(),
        );
        let err = client
            .send_checked(
                client
                    .get(&format!(
                        "https://evil.example/collect?token={SECRET}#{SECRET}"
                    ))
                    .unwrap(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, EgressError::Denied { .. }), "{err:?}");
        assert_secret_free(&err, "denied");
    }

    #[tokio::test]
    async fn live_body_not_materialized_is_secret_free() {
        let client = CheckedHttpClient::with_policy_and_scan_for_tests(
            DestinationPolicy::parse_lines(["http://allowed.example:80"]).unwrap(),
            Some(OutboundScanConfig::default()),
        );
        let stream = futures::stream::iter(vec![Ok::<_, std::io::Error>(&b"payload"[..])]);
        let request = client
            .post(&format!("http://allowed.example/send?token={SECRET}"))
            .unwrap()
            .body(reqwest::Body::wrap_stream(stream))
            .build()
            .unwrap();
        let err = client.execute(request).await.unwrap_err();
        assert!(
            matches!(&err, EgressError::BodyNotMaterialized(_)),
            "{err:?}"
        );
        assert_secret_free(&err, "body-not-materialized");
    }

    #[tokio::test]
    async fn live_transport_failure_is_url_free() {
        // A real connect failure: the query carries the planted secret and
        // the message must not (reqwest errors are stripped with
        // `without_url()` before stringifying).
        let client = CheckedHttpClient::with_policy_for_tests(
            DestinationPolicy::parse_lines(["http://localhost:1"]).unwrap(),
        );
        let err = client
            .send_checked(
                client
                    .get(&format!("http://localhost:1/x?token={SECRET}#{SECRET}"))
                    .unwrap(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, EgressError::Transport(_)), "{err:?}");
        assert_secret_free(&err, "transport");
    }

    #[test]
    fn safe_url_diagnostic_carries_no_path_bytes_however_long_or_hostile() {
        // A long, secret-bearing path renders as one fixed marker: nothing
        // derived from the path is retained (not even a truncation suffix
        // that could splice hostile bytes).
        let long_path: String = (0..600).map(|i| format!("/seg-{SECRET}-{i}")).collect();
        let diag = SafeUrlDiagnostic::from_raw(&format!("https://allowed.example{long_path}?x=y"));
        assert_eq!(diag.path_segments(), 600);
        let rendered = format!("{diag}");
        assert_eq!(
            rendered,
            "https://allowed.example:443/<redacted-path>?<redacted>"
        );
        assert!(!rendered.contains(SECRET), "{rendered}");
        // A path with more than u16::MAX segments saturates (never wraps).
        let huge: String = std::iter::repeat_n("/s", u16::MAX as usize + 100).collect();
        let diag = SafeUrlDiagnostic::from_raw(&format!("https://allowed.example{huge}"));
        assert_eq!(diag.path_segments(), u16::MAX);
        assert!(!format!("{diag}").contains("/s"), "{diag}");
        // A root path (no segments) omits the path marker entirely.
        let diag = SafeUrlDiagnostic::from_raw("https://allowed.example");
        assert_eq!(diag.path_segments(), 0);
        assert_eq!(format!("{diag}"), "https://allowed.example:443");
    }
}

#[cfg(test)]
mod resolver_rebinding_tests {
    use super::*;
    use crate::resolver::{HostResolver, DEFAULT_MAX_DNS_ANSWERS};
    use crate::testing::{MockAction, MockServer};
    use std::collections::VecDeque;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// Counting scripted DNS: one canned answer set per call, so a test can
    /// assert both the vetted outcome and that exactly ONE resolution
    /// happened (no re-resolution between decision and connect).
    #[derive(Debug, Default)]
    struct ScriptedResolver {
        answers: Mutex<VecDeque<Vec<SocketAddr>>>,
        calls: AtomicUsize,
    }

    impl ScriptedResolver {
        fn answering(answers: Vec<SocketAddr>) -> Arc<Self> {
            let resolver = ScriptedResolver::default();
            resolver.answers.lock().unwrap().push_back(answers);
            Arc::new(resolver)
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        /// Queue one more canned answer set (each request resolves once).
        fn queue(&self, answers: Vec<SocketAddr>) {
            self.answers.lock().unwrap().push_back(answers);
        }
    }

    impl HostResolver for ScriptedResolver {
        fn resolve(
            &self,
            _host: &str,
            _port: u16,
        ) -> BoxFuture<'_, Result<Vec<SocketAddr>, String>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let next = self.answers.lock().unwrap().pop_front().unwrap_or_default();
            Box::pin(async move { Ok(next) })
        }
    }

    fn sa(ip: &str, port: u16) -> SocketAddr {
        SocketAddr::new(ip.parse().expect("test IP"), port)
    }

    async fn probe_server() -> (Arc<MockServer>, u16) {
        let server = MockServer::new();
        server.route(
            "GET",
            "/probe",
            MockAction::Respond {
                status: 200,
                body: "ok".into(),
            },
        );
        let (addr, _handle) = server.serve().await;
        (server, addr.port())
    }

    #[tokio::test]
    async fn explicit_literal_is_admitted_while_a_name_resolving_there_is_not() {
        let (server, port) = probe_server().await;
        // The policy explicitly names BOTH the literal loopback address and
        // a hostname; only the hostname can be rebound.
        let policy = DestinationPolicy::parse_lines([
            &format!("http://127.0.0.1:{port}"),
            &format!("http://allowed.example:{port}"),
        ])
        .unwrap();
        let literal_url = format!("http://127.0.0.1:{port}/probe");
        let name = "allowed.example";

        // A literal-IP host cannot be rebound: the exact allowlist rule is
        // the operator explicitly naming that address, so it is admitted
        // even under the external-only address rule.
        let external = CheckedHttpClient::try_with_policy(policy.clone()).expect("client build");
        let resp = external
            .send_checked(external.get(&literal_url).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(server.request_count(), 1);

        // The SAME external rule refuses the hostname once it resolves onto
        // loopback: that is the rebinding vector the resolver closes.
        let resolver = ScriptedResolver::answering(vec![sa("127.0.0.1", port)]);
        let client = CheckedHttpClient::with_injected_resolver_for_tests(
            policy.clone(),
            EgressAddressPolicy::EXTERNAL,
            resolver.clone(),
            DEFAULT_MAX_DNS_ANSWERS,
        );
        let err = client
            .send_checked(client.get(&format!("http://{name}:{port}/probe")).unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(
                &err,
                EgressError::AddressClassRefused {
                    class: AddressClass::Loopback,
                    ..
                }
            ),
            "{err:?}"
        );
        assert_eq!(server.request_count(), 1, "the name never connected");
        assert_eq!(resolver.calls(), 1);

        // The explicit address-class rule admits the resolved local case
        // (one more resolution for the second request).
        resolver.queue(vec![sa("127.0.0.1", port)]);
        let local = CheckedHttpClient::with_injected_resolver_for_tests(
            policy,
            EgressAddressPolicy::LOCAL,
            resolver,
            DEFAULT_MAX_DNS_ANSWERS,
        );
        let resp = local
            .send_checked(local.get(&format!("http://{name}:{port}/probe")).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(server.request_count(), 2);
    }

    #[tokio::test]
    async fn malicious_dns_answers_never_connect_under_an_external_rule() {
        let (server, port) = probe_server().await;
        let policy =
            DestinationPolicy::parse_lines([&format!("http://allowed.example:{port}")]).unwrap();
        let url = format!("http://allowed.example:{port}/probe");

        for (label, answer) in [
            ("loopback", "127.0.0.1"),
            ("cloud metadata", "169.254.169.254"),
            ("rfc1918 private", "10.0.0.1"),
            ("ipv6 loopback", "::1"),
            ("ipv6 link-local", "fe80::1"),
        ] {
            let resolver = ScriptedResolver::answering(vec![sa(answer, port)]);
            let client = CheckedHttpClient::with_injected_resolver_for_tests(
                policy.clone(),
                EgressAddressPolicy::EXTERNAL,
                resolver.clone(),
                DEFAULT_MAX_DNS_ANSWERS,
            );
            let err = client
                .send_checked(client.get(&url).unwrap())
                .await
                .unwrap_err();
            assert!(
                matches!(err, EgressError::AddressClassRefused { .. }),
                "{label}: expected a class refusal, got {err:?}"
            );
            assert_eq!(server.request_count(), 0, "{label}: no connect");
            assert_eq!(resolver.calls(), 1, "{label}: exactly one resolution");
        }
    }

    #[tokio::test]
    async fn mixed_public_and_private_answers_are_refused_wholesale() {
        let (server, port) = probe_server().await;
        let policy =
            DestinationPolicy::parse_lines([&format!("http://allowed.example:{port}")]).unwrap();
        let url = format!("http://allowed.example:{port}/probe");
        // Public first: a filtering implementation would race the hostile
        // answer; the whole set must be refused.
        let resolver =
            ScriptedResolver::answering(vec![sa("93.184.216.34", port), sa("127.0.0.1", port)]);
        let client = CheckedHttpClient::with_injected_resolver_for_tests(
            policy,
            EgressAddressPolicy::EXTERNAL,
            resolver.clone(),
            DEFAULT_MAX_DNS_ANSWERS,
        );
        let err = client
            .send_checked(client.get(&url).unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(err, EgressError::AddressClassRefused { .. }),
            "{err:?}"
        );
        assert_eq!(server.request_count(), 0);
        assert_eq!(resolver.calls(), 1);
    }

    #[tokio::test]
    async fn explicit_allow_loopback_permits_only_the_configured_local_case() {
        let (server, port) = probe_server().await;
        let policy =
            DestinationPolicy::parse_lines([&format!("http://allowed.example:{port}")]).unwrap();
        let url = format!("http://allowed.example:{port}/probe");

        let resolver = ScriptedResolver::answering(vec![sa("127.0.0.1", port)]);
        let client = CheckedHttpClient::with_injected_resolver_for_tests(
            policy.clone(),
            EgressAddressPolicy::LOCAL,
            resolver.clone(),
            DEFAULT_MAX_DNS_ANSWERS,
        );
        let resp = client
            .send_checked(client.get(&url).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(server.request_count(), 1);
        assert_eq!(resolver.calls(), 1, "no re-resolution during connect");

        // The same explicit rule never opens link-local/metadata.
        let resolver = ScriptedResolver::answering(vec![sa("169.254.169.254", port)]);
        let client = CheckedHttpClient::with_injected_resolver_for_tests(
            policy,
            EgressAddressPolicy::LOCAL,
            resolver,
            DEFAULT_MAX_DNS_ANSWERS,
        );
        let err = client
            .send_checked(client.get(&url).unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(
                &err,
                EgressError::AddressClassRefused {
                    class: AddressClass::LinkLocal,
                    ..
                }
            ),
            "{err:?}"
        );
        assert_eq!(server.request_count(), 1, "metadata still refused locally");
    }

    #[tokio::test]
    async fn answer_set_bound_is_enforced_through_the_client() {
        let (server, port) = probe_server().await;
        let policy =
            DestinationPolicy::parse_lines([&format!("http://allowed.example:{port}")]).unwrap();
        let url = format!("http://allowed.example:{port}/probe");
        let answers: Vec<SocketAddr> = (1..=DEFAULT_MAX_DNS_ANSWERS + 1)
            .map(|i| sa(&format!("93.184.216.{i}"), port))
            .collect();
        let resolver = ScriptedResolver::answering(answers);
        let client = CheckedHttpClient::with_injected_resolver_for_tests(
            policy,
            EgressAddressPolicy::EXTERNAL,
            resolver.clone(),
            DEFAULT_MAX_DNS_ANSWERS,
        );
        let err = client
            .send_checked(client.get(&url).unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(
                &err,
                EgressError::DnsAnswerSetTooLarge {
                    count,
                    limit,
                    ..
                } if *count == DEFAULT_MAX_DNS_ANSWERS + 1 && *limit == DEFAULT_MAX_DNS_ANSWERS
            ),
            "{err:?}"
        );
        assert_eq!(server.request_count(), 0);
        assert_eq!(resolver.calls(), 1);
        // DNS refusals are security refusals: never retried.
        let converted: ProviderError = err.into();
        assert!(!converted.retryable, "{}", converted.message);
    }
}

#[cfg(test)]
mod budget_tests {
    use super::*;
    use futures::StreamExt;

    type BodyStream = futures::stream::BoxStream<'static, Result<Vec<u8>, std::io::Error>>;

    /// A body stream that never yields a chunk (the stalled-head shape).
    fn pending_body() -> BodyStream {
        Box::pin(futures::stream::pending())
    }

    /// A body that yields the scripted `(delay_ms, bytes)` chunks and then
    /// stays silent forever (the stalled-idle / dripped shapes).
    fn chunks_then_pending(chunks: Vec<(u64, Vec<u8>)>) -> BodyStream {
        let scripted = futures::stream::iter(chunks).then(|(delay_ms, bytes)| async move {
            if delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
            Ok::<Vec<u8>, std::io::Error>(bytes)
        });
        Box::pin(scripted.chain(futures::stream::pending()))
    }

    /// A body that yields the scripted chunks and then ends cleanly.
    fn chunks_then_end(chunks: Vec<Vec<u8>>) -> BodyStream {
        Box::pin(futures::stream::iter(chunks.into_iter().map(Ok)))
    }

    /// A transport serving ONE scripted body stream (no network).
    struct ScriptedBodyTransport {
        body: std::sync::Mutex<Option<BodyStream>>,
    }

    impl ScriptedBodyTransport {
        fn new(body: BodyStream) -> Self {
            Self {
                body: std::sync::Mutex::new(Some(body)),
            }
        }
    }

    impl HttpTransport for ScriptedBodyTransport {
        fn execute(&self, req: Request) -> BoxFuture<'_, Result<Response, EgressError>> {
            let body = self
                .body
                .lock()
                .unwrap()
                .take()
                .expect("one scripted body per test");
            let url = req.url().clone();
            Box::pin(async move {
                let response = http::Response::builder()
                    .status(200u16)
                    .url(url)
                    .body(Body::wrap_stream(body))
                    .map_err(|e| EgressError::Build(e.to_string()))?;
                Ok(Response::from(response))
            })
        }
    }

    fn request() -> RawRequest {
        RawRequest::new("GET", "https://allowed.example/data?token=PLANTED_SECRET")
            .route(RouteLabel::ProviderStream)
    }

    /// Run one scripted body through `execute_raw` under `budget`, with an
    /// outer test guard so a broken bound surfaces as a test timeout.
    async fn run(body: BodyStream, budget: ResponseBudget) -> Result<RawResponse, EgressError> {
        let transport = ScriptedBodyTransport::new(body);
        tokio::time::timeout(
            Duration::from_secs(10),
            execute_raw(&transport, request(), &budget),
        )
        .await
        .expect("the budget must fire without the test timeout")
    }

    #[tokio::test]
    async fn head_component_fires_typed_on_a_silent_body() {
        let budget = ResponseBudget::from_millis(60, 60, 5_000, 1024, None);
        let err = run(pending_body(), budget).await.unwrap_err();
        match &err {
            EgressError::ResponseBudgetExceeded {
                component: BudgetComponent::Head,
                limit: 60,
                url,
            } => {
                // The diagnostic carries only the credential-free shape.
                assert_eq!(url.host(), "allowed.example");
                assert!(url.query_present());
            }
            other => panic!("expected a typed Head budget error, got {other:?}"),
        }
        // The planted query secret never renders.
        assert!(!err.to_string().contains("PLANTED_SECRET"), "{err}");
        assert!(!format!("{err:?}").contains("PLANTED_SECRET"), "{err:?}");
    }

    #[tokio::test]
    async fn idle_component_fires_typed_after_a_partial_body() {
        let body = chunks_then_pending(vec![(0, b"first".to_vec())]);
        let budget = ResponseBudget::from_millis(2_000, 60, 5_000, 1024, None);
        let err = run(body, budget).await.unwrap_err();
        assert!(
            matches!(
                &err,
                EgressError::ResponseBudgetExceeded {
                    component: BudgetComponent::Idle,
                    limit: 60,
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn total_component_fires_typed_on_a_slow_drip() {
        // A chunk every 30 ms is inside the idle window, so only the overall
        // deadline can stop the drip.
        let chunks: Vec<(u64, Vec<u8>)> = (0..40).map(|_| (30, vec![b'x'])).collect();
        let budget = ResponseBudget::from_millis(5_000, 5_000, 150, 100_000, None);
        let err = run(chunks_then_pending(chunks), budget).await.unwrap_err();
        assert!(
            matches!(
                &err,
                EgressError::ResponseBudgetExceeded {
                    component: BudgetComponent::Total,
                    limit: 150,
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn bytes_component_fires_typed_before_the_crossing_chunk_is_handed_out() {
        let body = chunks_then_end(vec![vec![7u8; 64], vec![7u8; 64]]);
        let budget = ResponseBudget::from_millis(2_000, 2_000, 5_000, 100, None);
        let err = run(body, budget).await.unwrap_err();
        assert!(
            matches!(
                &err,
                EgressError::ResponseBudgetExceeded {
                    component: BudgetComponent::Bytes,
                    limit: 100,
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn frames_component_fires_typed_on_the_over_cap_frame() {
        let body = chunks_then_end(vec![vec![1], vec![2], vec![3]]);
        let budget = ResponseBudget::from_millis(2_000, 2_000, 5_000, 1024, Some(2));
        let err = run(body, budget).await.unwrap_err();
        assert!(
            matches!(
                &err,
                EgressError::ResponseBudgetExceeded {
                    component: BudgetComponent::Frames,
                    limit: 2,
                    ..
                }
            ),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn a_body_inside_its_budget_materializes() {
        let body = chunks_then_end(vec![b"ab".to_vec(), b"cd".to_vec()]);
        let budget = ResponseBudget::from_millis(2_000, 2_000, 5_000, 64, Some(4));
        let response = run(body, budget).await.unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"abcd");
    }

    #[tokio::test]
    async fn budgeted_stream_emits_exactly_one_typed_error_then_ends() {
        let transport = ScriptedBodyTransport::new(pending_body());
        let url = Url::parse("https://allowed.example/stream").unwrap();
        let response = transport
            .execute(Request::new(Method::GET, url))
            .await
            .unwrap();
        let budget = ResponseBudget::from_millis(50, 50, 5_000, 1024, None);
        let mut stream = Box::pin(BudgetedBody::new(response, budget).into_stream());
        let first = stream.next().await.expect("one error item").unwrap_err();
        assert!(
            matches!(
                first,
                EgressError::ResponseBudgetExceeded {
                    component: BudgetComponent::Head,
                    ..
                }
            ),
            "{first:?}"
        );
        assert!(stream.next().await.is_none(), "stream ends after the error");
    }

    /// The fallible-construction contract (item 9): production constructors
    /// return `Result`, a build failure is the typed `ClientBuild` (never a
    /// panicking or fallback path), and the deleted `unwrap_or_else`
    /// fallback cannot reappear unnoticed.
    #[test]
    fn checked_client_construction_is_fallible_without_a_fallback_path() {
        let client = CheckedHttpClient::try_with_policy(DestinationPolicy::empty())
            .expect("an empty policy still builds the one checked client");
        assert!(client.policy().is_some());
        let scanned = CheckedHttpClient::try_with_policy_and_scan(
            DestinationPolicy::empty(),
            Some(OutboundScanConfig::default()),
        )
        .expect("client build");
        assert!(scanned.outbound_scan().is_some());
        let transport = PolicyCheckedHttpTransport::try_with_policy_scan_and_addresses(
            DestinationPolicy::empty(),
            None,
            EgressAddressPolicy::EXTERNAL,
        )
        .expect("client build");
        assert_eq!(transport.address_policy(), EgressAddressPolicy::EXTERNAL);
        let rendered = EgressError::ClientBuild {
            detail: "tls backend unavailable".into(),
        }
        .to_string();
        assert_eq!(
            rendered,
            "egress client construction failed: tls backend unavailable"
        );
        // Source certification of the deleted fallback: the old fallback
        // client constructor must not come back. Markers are assembled at
        // runtime so this test's own text cannot satisfy them.
        let own = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/egress.rs"))
            .expect("egress.rs is readable");
        let fallback_fn = format!("fn redirect_disabled{}", "_client");
        assert!(
            !own.contains(&fallback_fn),
            "the fallback client constructor returned; construction must stay fallible"
        );
        let fallback_call = format!("unwrap_or_else(|_| default_timeout{}", "_client");
        assert!(
            !own.contains(&fallback_call),
            "the unwrap_or_else client fallback returned; start-up must fail typed"
        );
    }
}
