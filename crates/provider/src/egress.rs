//! Egress enforcement seam for outbound HTTP (audits 36-37).
//!
//! Every app-level outbound call must decide on a **parsed** destination
//! before the connection is attempted. [`CheckedHttpClient`] wraps a
//! `reqwest::Client` with an optional installed [`DestinationPolicy`]
//! (security crate): the decision runs against the `reqwest::Url` of the
//! *exact request object* that will be sent — scheme, host and port pulled
//! from the parsed URL, never from strings — and the connection goes to
//! that same request (`execute` sends the checked `reqwest::Request`
//! itself: there is no resolve-then-connect split, so a DNS rebinding that
//! changes what a hostname resolves to between decision and connect cannot
//! bypass the gate; the host rule already compared the URL's host text).
//!
//! Semantics (documented, identical to the security crate):
//!
//! | policy state                          | outcome |
//! |---------------------------------------|---------|
//! | `None` (no policy installed)          | allow (default-allow) |
//! | installed allowlist, rule matches     | allow |
//! | installed allowlist (even empty)      | deny before connect |
//!
//! A denied destination returns a typed [`EgressError::Denied`] carrying
//! which rule fired and how far its match got. The authoritative check runs
//! at `execute` time on the final request object (so no caller can
//! construct a request that bypasses the gate); `get`/`post` validate the
//! URL shape up front with typed errors, and [`CheckedHttpClient::check`]
//! exposes the same decision for callers that want to refuse early.
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

use futures::future::BoxFuture;
use reqwest::header::{
    HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_ENCODING, CONTENT_LENGTH,
    CONTENT_TYPE, COOKIE, LOCATION, PROXY_AUTHORIZATION, TRANSFER_ENCODING, WWW_AUTHENTICATE,
};
use reqwest::{Body, Method, Request, RequestBuilder, Response, ResponseBuilderExt, Url};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::{ProviderError, ProviderErrorKind};
use faktor_security::destination::{Decision, DeniedReason, DestinationPolicy, RequestTarget};
use faktor_security::payload::{scan_payload, ScanOutcome, ScanPolicy};
use faktor_security::registry::SecretRegistry;

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
    /// Wrap an explicit client with an optional allowlist. The `inner`
    /// client MUST be built with [`reqwest::redirect::Policy::none()`] (the
    /// constructors here all are): [`CheckedHttpClient::execute`] follows
    /// redirects itself so each hop is re-validated; a client that still
    /// follows internally fails closed with
    /// [`EgressError::UncheckedRedirectFollowed`].
    pub fn new(inner: reqwest::Client, policy: Option<DestinationPolicy>) -> Self {
        Self {
            inner: CheckedHttpClient::new(inner, policy),
        }
    }

    /// The default-allow transport with the adapter-standard connect
    /// timeout and NO installed policy. TEST-ONLY: gated behind `cfg(test)`
    /// or the `test-utils` feature (enabled solely by dependent crates'
    /// dev-dependencies), so a production build has no default-allow
    /// constructor to call — the daemon must inject the policy-checked
    /// transport ([`PolicyCheckedHttpTransport::with_policy`]).
    #[cfg(any(test, feature = "test-utils"))]
    pub fn permissive() -> Self {
        Self::new(default_timeout_client(), None)
    }

    /// A transport with the given allowlist installed (`None` keeps
    /// default-allow; `Some(DestinationPolicy::empty())` denies everything
    /// before connect).
    pub fn with_policy(policy: Option<DestinationPolicy>) -> Self {
        Self::new(default_timeout_client(), policy)
    }

    /// A transport with the given allowlist AND an installed outbound
    /// secret scan (audit P0-37/P0-38): every request body passes the
    /// full-payload scan before any connect (see [`OutboundScanConfig`]).
    pub fn with_policy_and_scan(
        policy: Option<DestinationPolicy>,
        outbound_scan: Option<OutboundScanConfig>,
    ) -> Self {
        Self {
            inner: CheckedHttpClient::with_policy_and_scan(policy, outbound_scan),
        }
    }

    /// The installed outbound secret scan (`None` = no secret scanning).
    pub fn outbound_scan(&self) -> Option<&OutboundScanConfig> {
        self.inner.outbound_scan()
    }

    /// The installed allowlist (`None` = default-allow).
    pub fn policy(&self) -> Option<&DestinationPolicy> {
        self.inner.policy()
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
/// controls live in the adapter transport guards, never here) and
/// redirects DISABLED — [`CheckedHttpClient::execute`] follows redirects
/// itself so every hop passes the destination/scan gate.
fn default_timeout_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_else(|_| redirect_disabled_client())
}

/// A client with automatic redirects disabled (no connect timeout). The
/// fallback for [`default_timeout_client`] keeps `Policy::none()` even when
/// the preferred build fails: a plain `reqwest::Client::new()` would
/// silently reintroduce the unchecked internal follower.
fn redirect_disabled_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("redirect-disabled HTTP client build")
}

/// Execute a GET through a transport. Request building and the raw
/// `Client::execute` call happen only here (adapter code never constructs a
/// `reqwest::Client`).
pub async fn execute_get(
    transport: &dyn HttpTransport,
    url: &str,
) -> Result<Response, EgressError> {
    let parsed = Url::parse(url).map_err(|e| EgressError::UnparseableUrl(e.to_string()))?;
    transport.execute(Request::new(Method::GET, parsed)).await
}

/// Execute a JSON POST through a transport (no extra headers).
pub async fn execute_post_json(
    transport: &dyn HttpTransport,
    url: &str,
    headers: HeaderMap,
    body: &serde_json::Value,
) -> Result<Response, EgressError> {
    execute_post_json_with_extras(transport, url, headers, &[], body).await
}

/// Execute a JSON POST through a transport with extra name/value headers
/// applied OVER `headers` (per-name replace, exactly the semantics the
/// gateway path relied on when it layered extra headers after the auth
/// headers). The content-type is forced to `application/json` last, exactly
/// like `RequestBuilder::json` did.
pub async fn execute_post_json_with_extras(
    transport: &dyn HttpTransport,
    url: &str,
    headers: HeaderMap,
    extra_headers: &[(String, String)],
    body: &serde_json::Value,
) -> Result<Response, EgressError> {
    let parsed = Url::parse(url).map_err(|e| EgressError::UnparseableUrl(e.to_string()))?;
    let mut request = Request::new(Method::POST, parsed);
    *request.headers_mut() = headers;
    for (name, value) in extra_headers {
        if let (Ok(k), Ok(v)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            request.headers_mut().insert(k, v);
        }
    }
    request
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    let json = serde_json::to_string(body).map_err(|e| EgressError::Build(e.to_string()))?;
    *request.body_mut() = Some(Body::from(json));
    transport.execute(request).await
}

/// Hard bound on one materialized [`RawResponse`] body: an adapter that
/// needs a verb/header shape beyond the JSON helpers still must not buffer
/// an unbounded remote body in RAM (bounded everything). The read aborts
/// with a typed [`EgressError::ResponseTooLarge`] at the bound.
pub const MAX_RAW_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// One neutral request description for adapters (e.g. the SCM GitHub-App
/// adapter) that need verbs, headers and bodies beyond the JSON POST
/// helpers. Building and the raw execution happen ONLY here, so a
/// non-provider adapter never names a `reqwest` type and every send still
/// passes the request-time destination gate of the transport it is handed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

impl RawRequest {
    pub fn new(method: &str, url: impl Into<String>) -> Self {
        Self {
            method: method.to_string(),
            url: url.into(),
            headers: Vec::new(),
            body: None,
        }
    }

    /// Add one header (invalid names/values are a typed build refusal when
    /// the request executes).
    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    /// Attach an application/json body.
    pub fn json_body(mut self, body: &serde_json::Value) -> Self {
        self.headers
            .push(("content-type".to_string(), "application/json".to_string()));
        self.body = Some(serde_json::to_vec(body).unwrap_or_default());
        self
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
/// the response under [`MAX_RAW_RESPONSE_BYTES`]. Adapter code never touches
/// `reqwest` directly; this is the ONLY execution site for
/// non-JSON-POST shapes.
pub async fn execute_raw(
    transport: &dyn HttpTransport,
    request: RawRequest,
) -> Result<RawResponse, EgressError> {
    let method = Method::from_bytes(request.method.as_bytes())
        .map_err(|e| EgressError::Build(format!("invalid method {:?}: {e}", request.method)))?;
    let url = Url::parse(&request.url).map_err(|e| EgressError::UnparseableUrl(e.to_string()))?;
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
    let mut stream = response.bytes_stream();
    let mut body: Vec<u8> = Vec::new();
    while let Some(chunk) = futures::StreamExt::next(&mut stream).await {
        let chunk = chunk.map_err(|e| EgressError::Transport(e.to_string()))?;
        if body.len().saturating_add(chunk.len()) > MAX_RAW_RESPONSE_BYTES {
            return Err(EgressError::ResponseTooLarge {
                limit_bytes: MAX_RAW_RESPONSE_BYTES as u64,
            });
        }
        body.extend_from_slice(&chunk);
    }
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EgressError {
    /// The installed allowlist denied the destination before any connect.
    /// `url` is the parsed request target (never a re-split string).
    Denied { url: String, reason: DeniedReason },
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
    BodyNotMaterialized(String),
    /// The URL scheme is not fetchable through this seam (only http/https
    /// are; a `ws`/`wss` policy may exist for websocket tools, but this
    /// client does not speak them).
    UnsupportedScheme(String),
    /// The URL (or provider base URL) does not parse / is not a valid
    /// absolute http(s) URL.
    UnparseableUrl(String),
    /// Building the request object failed.
    Build(String),
    /// The transport itself failed (connect/io); the request was allowed
    /// by the policy.
    Transport(String),
    /// A materialized response exceeded [`MAX_RAW_RESPONSE_BYTES`]; the
    /// adapter refuses to buffer it (bounded everything), and the partial
    /// body is discarded rather than parsed.
    ResponseTooLarge { limit_bytes: u64 },
    /// A redirect chain exceeded [`MAX_REDIRECT_HOPS`]. Each hop is a fresh
    /// policy-checked request, so a chain that long is refused typed; the
    /// hop that would exceed the bound is never sent.
    TooManyRedirects { limit: usize, url: String },
    /// A 307/308 redirect (or a non-POST 301/302) would have to replay a
    /// body that is not materialized (a stream); refused typed instead of
    /// silently re-sending it body-less or converting it to GET.
    RedirectBodyNotReplayable { url: String },
    /// The wrapped `reqwest::Client` followed a redirect itself (its
    /// builder installed something other than
    /// [`reqwest::redirect::Policy::none()`]), so the final hop never
    /// passed the per-hop destination gate. Fail closed: the caller must
    /// not consume a response obtained through an unchecked redirect.
    UncheckedRedirectFollowed { from: String, to: String },
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
            EgressError::UnparseableUrl(s) => write!(f, "not a parseable http(s) URL: {s}"),
            EgressError::Build(s) => write!(f, "request build failed: {s}"),
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
/// `policy: None` means **no policy installed → default-allow**
/// (documented); `Some(policy)` means the allowlist governs (default-deny
/// on no full scheme+host+port match).
pub fn check_url(policy: Option<&DestinationPolicy>, url: &Url) -> Result<(), EgressError> {
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(EgressError::UnsupportedScheme(scheme.to_string()));
    }
    let Some(host) = url.host_str() else {
        return Err(EgressError::UnparseableUrl(format!(
            "URL has no host: {url}"
        )));
    };
    if host.is_empty() {
        return Err(EgressError::UnparseableUrl(format!(
            "URL has an empty host: {url}"
        )));
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
        .map_err(EgressError::UnparseableUrl)?;
    match policy {
        None => Ok(()), // default-allow: no destination policy installed
        Some(policy) => match target.check_against(policy) {
            Decision::Allowed => Ok(()),
            Decision::Denied(reason) => Err(EgressError::Denied {
                url: target.describe(),
                reason,
            }),
        },
    }
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
/// every hop before that hop is sent (see the module docs).
#[derive(Debug, Clone)]
pub struct CheckedHttpClient {
    inner: reqwest::Client,
    policy: Option<DestinationPolicy>,
    outbound_scan: Option<OutboundScanConfig>,
}

/// The hard bound on redirect hops one checked request may follow. A
/// redirect chain longer than this is [`EgressError::TooManyRedirects`]
/// (the refusal arrives before the hop that would exceed the bound is
/// sent); a redirect is only ever followed because the gate allowed the
/// next URL.
pub const MAX_REDIRECT_HOPS: usize = 5;

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
    /// Wrap a client with an optional allowlist. `None` = no policy
    /// installed = default-allow (documented). No secret scanning.
    ///
    /// `inner` MUST have been built with
    /// [`reqwest::redirect::Policy::none()`]: this client follows redirects
    /// itself so every hop is re-validated. A client that still follows
    /// internally is detected on the response and refused with
    /// [`EgressError::UncheckedRedirectFollowed`] (fail closed). Prefer
    /// [`CheckedHttpClient::with_policy`] /
    /// [`CheckedHttpClient::with_policy_and_scan`], which build a compliant
    /// client.
    pub fn new(inner: reqwest::Client, policy: Option<DestinationPolicy>) -> CheckedHttpClient {
        CheckedHttpClient {
            inner,
            policy,
            outbound_scan: None,
        }
    }

    /// A default redirect-disabled client with the given allowlist.
    pub fn with_policy(policy: Option<DestinationPolicy>) -> CheckedHttpClient {
        CheckedHttpClient::new(redirect_disabled_client(), policy)
    }

    /// A client with the given allowlist AND an installed outbound secret
    /// scan (see [`OutboundScanConfig`]).
    pub fn with_policy_and_scan(
        policy: Option<DestinationPolicy>,
        outbound_scan: Option<OutboundScanConfig>,
    ) -> CheckedHttpClient {
        let mut client = CheckedHttpClient::with_policy(policy);
        client.outbound_scan = outbound_scan;
        client
    }

    /// Install (or clear) the outbound secret scan on this client.
    pub fn set_outbound_scan(&mut self, outbound_scan: Option<OutboundScanConfig>) {
        self.outbound_scan = outbound_scan;
    }

    /// The installed outbound secret scan (`None` = no secret scanning).
    pub fn outbound_scan(&self) -> Option<&OutboundScanConfig> {
        self.outbound_scan.as_ref()
    }

    /// The installed allowlist (`None` = default-allow).
    pub fn policy(&self) -> Option<&DestinationPolicy> {
        self.policy.as_ref()
    }

    /// The typed pre-send check for one parsed request URL.
    pub fn check(&self, url: &Url) -> Result<(), EgressError> {
        check_url(self.policy.as_ref(), url)
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
            .map_err(|e| EgressError::Build(e.to_string()))?;
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
                .map_err(|e| EgressError::Transport(format!("{hop_url}: {e}")))?;

            // Fail closed when the wrapped client followed a redirect
            // itself: its final URL differs from the hop that was checked,
            // so the bypass would otherwise skip every per-hop gate.
            if !same_url(response.url(), &hop_url) {
                return Err(EgressError::UncheckedRedirectFollowed {
                    from: hop_url.to_string(),
                    to: response.url().to_string(),
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
                    url: hop_url.to_string(),
                });
            }
            let location = location.to_str().map_err(|_| {
                EgressError::UnparseableUrl(format!(
                    "redirect Location is not valid header text on {hop_url}"
                ))
            })?;
            let next_url = hop_url.join(location).map_err(|e| {
                EgressError::UnparseableUrl(format!(
                    "redirect Location {location:?} on {hop_url}: {e}"
                ))
            })?;
            // A Location carrying userinfo would smuggle embedded
            // credentials past the credential-stripping rule.
            if !next_url.username().is_empty() || next_url.password().is_some() {
                return Err(EgressError::UnparseableUrl(format!(
                    "redirect target must not carry userinfo: {next_url}"
                )));
            }

            let mut next = match replay {
                Some(replayed) => replayed,
                None if !drop_body => {
                    return Err(EgressError::RedirectBodyNotReplayable {
                        url: hop_url.to_string(),
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
            return Err(EgressError::BodyNotMaterialized(format!(
                "{}",
                request.url()
            )));
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

    /// The wrapped client (for callers that need other builder families).
    pub fn inner(&self) -> &reqwest::Client {
        &self.inner
    }
}

/// Parse one fetch URL strictly: absolute, http/https only, host present,
/// no userinfo, no port 0.
fn parse_fetch_url(raw: &str) -> Result<Url, EgressError> {
    let url = Url::parse(raw).map_err(|e| EgressError::UnparseableUrl(e.to_string()))?;
    let scheme = url.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(EgressError::UnsupportedScheme(scheme.to_string()));
    }
    if url.host_str().is_none_or(str::is_empty) {
        return Err(EgressError::UnparseableUrl(format!(
            "URL has no host: {raw}"
        )));
    }
    if url.username() != "" || url.password().is_some() {
        return Err(EgressError::UnparseableUrl(format!(
            "URL must not carry userinfo: {raw}"
        )));
    }
    if url.port() == Some(0) {
        return Err(EgressError::UnparseableUrl(format!(
            "URL port 0 is invalid: {raw}"
        )));
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
        let client = CheckedHttpClient::with_policy(Some(policy_for(allowed_port)));
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
        let open = CheckedHttpClient::with_policy(None);
        let resp = open.send_checked(open.get(&url).unwrap()).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(server.request_count(), 1);

        // Installed empty policy => default-deny, before connect.
        let closed = CheckedHttpClient::with_policy(Some(DestinationPolicy::empty()));
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
        let client = CheckedHttpClient::with_policy(Some(policy));

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

    #[tokio::test]
    async fn unsupported_schemes_are_rejected_at_parse_and_at_execute() {
        let client = CheckedHttpClient::with_policy(Some(DestinationPolicy::empty()));
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
        let c = CheckedHttpClient::with_policy(Some(p.clone()));
        assert_eq!(c.policy(), Some(&p));
        let c = CheckedHttpClient::with_policy(None);
        assert_eq!(c.policy(), None);
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
        let allowed: Arc<dyn HttpTransport> = Arc::new(PolicyCheckedHttpTransport::with_policy(
            Some(policy_for_port(addr.port())),
        ));
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
        let denied: Arc<dyn HttpTransport> = Arc::new(PolicyCheckedHttpTransport::with_policy(
            Some(policy_for_port(addr.port().wrapping_add(1))),
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
            &[("authorization".into(), "Bearer extra".into())],
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
        let client = CheckedHttpClient::with_policy(Some(DestinationPolicy::empty()));
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
    let c = CheckedHttpClient::with_policy(None);
    let t = PolicyCheckedHttpTransport::with_policy(None);
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
        let client = CheckedHttpClient::with_policy_and_scan(
            Some(allowed_policy_for(addr.port())),
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
        let client = CheckedHttpClient::with_policy_and_scan(
            Some(allowed_policy_for(addr.port())),
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
        let client = CheckedHttpClient::with_policy_and_scan(
            Some(allowed_policy_for(addr.port())),
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
        let client = CheckedHttpClient::with_policy_and_scan(
            Some(allowed_policy_for(addr.port())),
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
        let client = CheckedHttpClient::with_policy_and_scan(
            Some(allowed_policy_for(addr.port())),
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
        let transport = PolicyCheckedHttpTransport::with_policy_and_scan(
            Some(allowed_policy_for(addr.port())),
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
        let client = CheckedHttpClient::with_policy(Some(policy));

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
                assert!(url.contains("localhost"), "{err}");
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
        let client = CheckedHttpClient::with_policy(Some(policy_for([format!(
            "http://127.0.0.1:{}",
            addr.port()
        )])));

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
        let client = CheckedHttpClient::with_policy(Some(policy));

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
        let client = CheckedHttpClient::with_policy(Some(policy_for([format!(
            "http://127.0.0.1:{}",
            addr.port()
        )])));

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
                url: format!("http://127.0.0.1:{}/loop", addr.port()),
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
        let client = CheckedHttpClient::with_policy(Some(policy_for([format!(
            "http://127.0.0.1:{}",
            addr.port()
        )])));
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
        let client = CheckedHttpClient::with_policy(Some(policy));
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
        let client = CheckedHttpClient::with_policy(Some(policy_for([format!(
            "http://127.0.0.1:{}",
            addr.port()
        )])));
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
        let client = CheckedHttpClient::with_policy(Some(policy_for([format!(
            "http://127.0.0.1:{}",
            addr.port()
        )])));
        let stream = futures::stream::iter(vec![Ok::<_, std::io::Error>(&b"chunk"[..])]);
        let request = client
            .post(&format!("http://127.0.0.1:{}/stream", addr.port()))
            .unwrap()
            .body(reqwest::Body::wrap_stream(stream))
            .build()
            .unwrap();
        let err = client.execute(request).await.unwrap_err();
        assert!(
            matches!(&err, EgressError::RedirectBodyNotReplayable { url } if url.ends_with("/stream")),
            "{err:?}"
        );
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
        let client = CheckedHttpClient::with_policy_and_scan(
            Some(policy_for([format!("http://127.0.0.1:{}", addr.port())])),
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

        let raw = default_timeout_client();
        let resp = raw.get(&url).send().await.unwrap();
        assert_eq!(resp.status(), 302, "default_timeout_client must not follow");

        let checked = CheckedHttpClient::with_policy(None);
        let resp = checked.inner().get(&url).send().await.unwrap();
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
                assert!(from.ends_with("/go"), "{err}");
                assert!(to.ends_with("/final"), "{err}");
            }
            other => panic!("expected UncheckedRedirectFollowed, got {other:?}"),
        }
        assert_eq!(
            server.count(),
            2,
            "the inner client did follow; we refuse the result"
        );
    }
}
