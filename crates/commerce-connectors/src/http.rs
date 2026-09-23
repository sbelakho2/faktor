//! The injected HTTP seam and its bounds.
//!
//! Connectors never create an HTTP client (spec §10): every byte leaves
//! through an `Arc<dyn HttpTransport>` injected into the [`crate::context::AcquireCtx`],
//! which the daemon supplies as the policy-checked egress transport
//! (`faktor-provider`'s `PolicyCheckedHttpTransport`): destination
//! validation, sandbox network policy, secret scanning and response bounds
//! happen *inside* that transport, below this seam.
//!
//! What this module adds is the connector-side half of the contract:
//!
//! * requests are built through a bounded builder (URL length, header count
//!   and size, body size, timeout, response bound) — a hostile connector
//!   cannot ask the transport for an unbounded body;
//! * [`HttpRequest`] and [`Header`] render **redacted** under `Debug`:
//!   credential header values and credential-named query parameters become
//!   `<redacted>`, bodies render as a length — a credential can never leak
//!   through a `{:?}`, a log line or a panic message;
//! * [`TransportError`] is typed and carries no free text (no URL, no
//!   upstream message), and maps onto the domain [`SourceError`] variants.
//!
//! The seam itself (replacing `HttpTransport` with the acquire runtime's
//! checked transport) is documented in the crate root.

use std::fmt;

use async_trait::async_trait;
use faktor_commerce::text::CanonicalUrl;
use faktor_commerce::SourceError;
use serde::de::DeserializeOwned;

use crate::context::AcquireCtx;
use crate::secrets::SecretString;

/// Hard bound for a request URL (matches [`CanonicalUrl`]'s own bound).
pub const MAX_URL_BYTES: usize = 2048;
/// Hard bound on request headers.
pub const MAX_HEADERS: usize = 32;
/// Hard bound for one header name.
pub const MAX_HEADER_NAME_BYTES: usize = 64;
/// Hard bound for one header value.
pub const MAX_HEADER_VALUE_BYTES: usize = 1024;
/// Hard bound for a request body (API request bodies are small).
pub const MAX_REQUEST_BODY_BYTES: usize = 64 * 1024;
/// Hard bound for a response body accepted by the connectors (defense in
/// depth on top of the transport's own bound).
pub const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
/// Default request timeout handed to the transport.
pub const DEFAULT_TIMEOUT_MS: u64 = 20_000;

/// The HTTP methods the connectors use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    /// `GET`.
    Get,
    /// `POST`.
    Post,
}

impl HttpMethod {
    /// The wire label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
        }
    }
}

/// Header names whose values are credentials and are never rendered.
const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "api-key",
    "apikey",
    "x-client-secret",
    "client_secret",
    "x-digikey-client-id",
];

/// Query parameter names whose values are credentials and are never
/// rendered.
const SENSITIVE_QUERY_PARAMS: &[&str] = &[
    "apikey",
    "api_key",
    "key",
    "token",
    "access_token",
    "client_secret",
    "password",
    "secret",
    "signature",
    "sig",
];

/// True when a header name carries a credential.
pub fn is_sensitive_header(name: &str) -> bool {
    SENSITIVE_HEADERS
        .iter()
        .any(|sensitive| name.eq_ignore_ascii_case(sensitive))
}

/// Render a URL with credential-named query parameters redacted. Never
/// panics, never returns the credential.
pub fn redact_url(url: &str) -> String {
    let Some((head, query)) = url.split_once('?') else {
        return url.to_string();
    };
    let mut out = String::with_capacity(url.len());
    out.push_str(head);
    out.push('?');
    for (index, pair) in query.split('&').enumerate() {
        if index > 0 {
            out.push('&');
        }
        match pair.split_once('=') {
            Some((name, _value))
                if SENSITIVE_QUERY_PARAMS
                    .iter()
                    .any(|sensitive| name.eq_ignore_ascii_case(sensitive)) =>
            {
                out.push_str(name);
                out.push_str("=<redacted>");
            }
            _ => out.push_str(pair),
        }
    }
    out
}

/// Percent-encode one query parameter value (RFC 3986 unreserved set kept
/// verbatim). Deterministic and bounded by the input length.
pub fn query_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        let keep = byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~');
        if keep {
            out.push(*byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

/// A rejected request shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum HttpError {
    /// The URL is not a valid absolute `http(s)` URL.
    #[error("invalid request url")]
    InvalidUrl,
    /// The header name is not an ASCII token.
    #[error("invalid header name")]
    InvalidHeaderName,
    /// The header value contains control characters or exceeds its bound.
    #[error("invalid header value")]
    InvalidHeaderValue,
    /// Too many headers.
    #[error("too many headers")]
    TooManyHeaders,
    /// The request body exceeds its bound.
    #[error("request body exceeds the bound")]
    RequestBodyTooLarge,
    /// The response body exceeds the bound.
    #[error("response body exceeds the bound")]
    ResponseTooLarge,
    /// The response body does not match the expected schema.
    #[error("response body does not match the expected schema")]
    MalformedBody,
}

/// One validated header. `Debug` redacts credential carriers.
#[derive(Clone, PartialEq, Eq)]
pub struct Header {
    name: String,
    value: String,
}

impl Header {
    /// Validate and construct.
    pub fn new(name: &str, value: &str) -> Result<Self, HttpError> {
        let name = name.trim();
        if name.is_empty()
            || name.len() > MAX_HEADER_NAME_BYTES
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'*' | b'+'))
        {
            return Err(HttpError::InvalidHeaderName);
        }
        if value.len() > MAX_HEADER_VALUE_BYTES
            || value
                .bytes()
                .any(|b| b == b'\r' || b == b'\n' || b == b'\0' || b.is_ascii_control())
        {
            return Err(HttpError::InvalidHeaderValue);
        }
        Ok(Self {
            name: name.to_string(),
            value: value.to_string(),
        })
    }

    /// A header whose value is a credential: the plaintext is used on the
    /// wire and never rendered.
    pub fn secret(name: &str, value: &SecretString) -> Result<Self, HttpError> {
        Header::new(name, value.expose())
    }

    /// The header name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The header value.
    pub fn value(&self) -> &str {
        &self.value
    }
}

impl fmt::Debug for Header {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if is_sensitive_header(&self.name) {
            write!(f, "Header({}: <redacted>)", self.name)
        } else {
            write!(f, "Header({}: {:?})", self.name, self.value)
        }
    }
}

/// A bounded, validated outbound request.
#[derive(Clone, PartialEq, Eq)]
pub struct HttpRequest {
    method: HttpMethod,
    url: CanonicalUrl,
    headers: Vec<Header>,
    body: Option<Vec<u8>>,
    timeout_ms: u64,
    max_response_bytes: usize,
    /// A credential the connector intentionally placed in the URL query
    /// (the documented Mouser `apiKey` parameter). The transport boundary
    /// requires it registered with the outbound scanner and refuses any
    /// *other* registered secret found in the URL.
    url_secret: Option<SecretString>,
}

impl HttpRequest {
    /// A `GET` with the default bounds.
    pub fn get(url: &str) -> Result<Self, HttpError> {
        Ok(Self {
            method: HttpMethod::Get,
            url: CanonicalUrl::parse(url).map_err(|_| HttpError::InvalidUrl)?,
            headers: Vec::new(),
            body: None,
            timeout_ms: DEFAULT_TIMEOUT_MS,
            max_response_bytes: MAX_RESPONSE_BYTES,
            url_secret: None,
        })
    }

    /// A `POST` with a JSON body and the default bounds.
    pub fn post_json(url: &str, body: &[u8]) -> Result<Self, HttpError> {
        if body.len() > MAX_REQUEST_BODY_BYTES {
            return Err(HttpError::RequestBodyTooLarge);
        }
        let mut request = Self {
            method: HttpMethod::Post,
            url: CanonicalUrl::parse(url).map_err(|_| HttpError::InvalidUrl)?,
            headers: Vec::new(),
            body: Some(body.to_vec()),
            timeout_ms: DEFAULT_TIMEOUT_MS,
            max_response_bytes: MAX_RESPONSE_BYTES,
            url_secret: None,
        };
        request.push_header(Header::new("content-type", "application/json")?)?;
        request.push_header(Header::new("accept", "application/json")?)?;
        Ok(request)
    }

    /// A `POST` with a form body and the default bounds.
    pub fn post_form(url: &str, body: &[u8]) -> Result<Self, HttpError> {
        if body.len() > MAX_REQUEST_BODY_BYTES {
            return Err(HttpError::RequestBodyTooLarge);
        }
        let mut request = Self {
            method: HttpMethod::Post,
            url: CanonicalUrl::parse(url).map_err(|_| HttpError::InvalidUrl)?,
            headers: Vec::new(),
            body: Some(body.to_vec()),
            timeout_ms: DEFAULT_TIMEOUT_MS,
            max_response_bytes: MAX_RESPONSE_BYTES,
            url_secret: None,
        };
        request.push_header(Header::new(
            "content-type",
            "application/x-www-form-urlencoded",
        )?)?;
        request.push_header(Header::new("accept", "application/json")?)?;
        Ok(request)
    }

    fn push_header(&mut self, header: Header) -> Result<(), HttpError> {
        if self.headers.len() >= MAX_HEADERS {
            return Err(HttpError::TooManyHeaders);
        }
        self.headers.push(header);
        Ok(())
    }

    /// Add one header (bounded).
    pub fn with_header(mut self, header: Header) -> Result<Self, HttpError> {
        self.push_header(header)?;
        Ok(self)
    }

    /// Add a credential-bearing header.
    pub fn with_secret_header(self, name: &str, value: &SecretString) -> Result<Self, HttpError> {
        self.with_header(Header::secret(name, value)?)
    }

    /// Declare that the URL query intentionally carries `secret` (the
    /// documented API-key query parameter). The transport boundary scans the
    /// URL with the outbound secret scanner: the declared credential is
    /// permitted, any *other* registered secret refuses before dispatch.
    pub fn with_url_credential(mut self, secret: &SecretString) -> Self {
        self.url_secret = Some(secret.clone());
        self
    }

    /// The declared URL credential, when one was set.
    pub(crate) fn url_credential(&self) -> Option<&SecretString> {
        self.url_secret.as_ref()
    }

    /// Override the timeout (bounded by the transport's own policy).
    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }

    /// Override the response bound (never above [`MAX_RESPONSE_BYTES`]).
    pub fn with_max_response_bytes(mut self, max: usize) -> Self {
        self.max_response_bytes = max.min(MAX_RESPONSE_BYTES);
        self
    }

    /// The method.
    pub fn method(&self) -> HttpMethod {
        self.method
    }

    /// The validated URL.
    pub fn url(&self) -> &CanonicalUrl {
        &self.url
    }

    /// The headers (credential values included: this is the wire form).
    pub fn headers(&self) -> &[Header] {
        &self.headers
    }

    /// The body, when there is one.
    pub fn body(&self) -> Option<&[u8]> {
        self.body.as_deref()
    }

    /// The timeout in milliseconds.
    pub fn timeout_ms(&self) -> u64 {
        self.timeout_ms
    }

    /// The response bound.
    pub fn max_response_bytes(&self) -> usize {
        self.max_response_bytes
    }
}

impl fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpRequest")
            .field("method", &self.method)
            .field("url", &redact_url(self.url.as_str()))
            .field("headers", &self.headers)
            .field("body_bytes", &self.body.as_ref().map(Vec::len))
            .field("timeout_ms", &self.timeout_ms)
            .field("max_response_bytes", &self.max_response_bytes)
            .finish()
    }
}

/// A bounded response.
#[derive(Clone, PartialEq, Eq)]
pub struct HttpResponse {
    status: u16,
    headers: Vec<Header>,
    body: Vec<u8>,
}

impl HttpResponse {
    /// Validate and construct (bounded body, validated headers).
    pub fn new(status: u16, headers: Vec<Header>, body: Vec<u8>) -> Result<Self, HttpError> {
        if body.len() > MAX_RESPONSE_BYTES {
            return Err(HttpError::ResponseTooLarge);
        }
        if headers.len() > MAX_HEADERS {
            return Err(HttpError::TooManyHeaders);
        }
        Ok(Self {
            status,
            headers,
            body,
        })
    }

    /// The HTTP status.
    pub fn status(&self) -> u16 {
        self.status
    }

    /// The first header with this name (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|header| header.name.eq_ignore_ascii_case(name))
            .map(Header::value)
    }

    /// The raw body.
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// Parse the body as JSON into the expected schema.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, HttpError> {
        serde_json::from_slice::<T>(&self.body).map_err(|_| HttpError::MalformedBody)
    }

    /// A bounded, control-free excerpt of the body for diagnostics. The
    /// caller still scrubs it through the secret guard.
    pub fn excerpt(&self, max_chars: usize) -> String {
        crate::normalize::sanitize_excerpt(std::str::from_utf8(&self.body).unwrap_or(""), max_chars)
    }
}

impl fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

/// A typed transport failure. Carries no free text: no URL, no upstream
/// message, nothing that could smuggle a credential into a log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum TransportError {
    /// The destination policy denied the request before any connect.
    #[error("destination denied by egress policy")]
    DestinationDenied,
    /// The request timed out.
    #[error("network timeout")]
    Timeout,
    /// The egress transport is unavailable.
    #[error("egress transport unavailable")]
    EgressUnavailable,
    /// The response exceeded the bound.
    #[error("response exceeded the bound")]
    ResponseTooLarge,
    /// The request was cancelled.
    #[error("request cancelled")]
    Cancelled,
    /// A protocol-level failure.
    #[error("transport protocol error")]
    Protocol,
}

impl From<TransportError> for SourceError {
    fn from(error: TransportError) -> Self {
        match error {
            TransportError::DestinationDenied | TransportError::EgressUnavailable => {
                SourceError::EgressUnavailable
            }
            TransportError::Timeout => SourceError::NetworkTimeout,
            TransportError::ResponseTooLarge => SourceError::ResponseTooLarge,
            TransportError::Cancelled => SourceError::Cancelled,
            TransportError::Protocol => SourceError::ApiUnavailable,
        }
    }
}

/// The injected transport seam (spec §10).
///
/// The production implementation is the daemon's policy-checked egress
/// transport; connectors only ever see this trait, and the fixture transport
/// in the test suite implements it for offline replay.
#[async_trait]
pub trait HttpTransport: Send + Sync {
    /// Execute one built request. The transport enforces destination
    /// policy, its own response bound, secret scanning and cancellation.
    async fn execute(&self, request: HttpRequest) -> Result<HttpResponse, TransportError>;
}

/// Map a request-shape failure onto the typed domain error. These are
/// connector-side construction bugs or hostile inputs, never transport
/// failures; they carry no free text.
impl From<HttpError> for SourceError {
    fn from(error: HttpError) -> Self {
        match error {
            HttpError::InvalidUrl
            | HttpError::InvalidHeaderName
            | HttpError::InvalidHeaderValue
            | HttpError::TooManyHeaders
            | HttpError::RequestBodyTooLarge => SourceError::InvalidRequest,
            HttpError::ResponseTooLarge => SourceError::ResponseTooLarge,
            HttpError::MalformedBody => SourceError::ExtractionIncomplete,
        }
    }
}

/// True when a transport failure means the request never reached the
/// network (safe to refund a quota permit for).
pub(crate) fn never_left_the_machine(error: TransportError) -> bool {
    matches!(
        error,
        TransportError::DestinationDenied
            | TransportError::EgressUnavailable
            | TransportError::Cancelled
    )
}

/// Map an HTTP status onto the typed domain error. `429` carries the
/// observed `Retry-After` window (defaulting to one minute, documented).
pub(crate) fn map_status(response: &HttpResponse) -> Result<(), SourceError> {
    match response.status() {
        200..=299 => Ok(()),
        400 | 422 => Err(SourceError::InvalidRequest),
        401 | 403 => Err(SourceError::AuthenticationRequired),
        404 => Err(SourceError::ProductNotFound),
        429 => Err(SourceError::RateLimited {
            retry_after_ms: response
                .header("retry-after")
                .and_then(crate::quota::parse_retry_after_ms)
                .unwrap_or(crate::quota::MINUTE_MS),
        }),
        500..=599 => Err(SourceError::ApiUnavailable),
        _ => Err(SourceError::ApiUnavailable),
    }
}

/// Enforce that the URL query is covered by the outbound secret scanner: a
/// URL may carry a registered secret only when the request declares exactly
/// that credential (e.g. the documented Mouser `apiKey` query parameter).
/// Any other registered secret refuses typed before the request leaves, so
/// a query-borne credential can never become an unscanned egress surface.
fn verify_url_secrets(
    ctx: &AcquireCtx,
    source: &faktor_commerce::SourceId,
    operation: &'static str,
    request: &HttpRequest,
) -> Result<(), SourceError> {
    let raw = request.url().as_str();
    let scrubbed = ctx.secrets().scrub(raw);
    if scrubbed == raw {
        // No registered secret in the URL: nothing to cover.
        return Ok(());
    }
    let declared_ok = request.url_credential().is_some_and(|secret| {
        let plain = secret.expose();
        if plain.is_empty() {
            return false;
        }
        // The declared credential itself must be scanner-covered...
        if ctx.secrets().scrub(plain) == plain {
            return false;
        }
        // ...and after removing exactly it, no other registered secret may
        // remain anywhere in the URL.
        let remainder = raw.replace(plain, "");
        ctx.secrets().scrub(&remainder) == remainder
    });
    if declared_ok {
        return Ok(());
    }
    ctx.record(
        source,
        operation,
        crate::context::ConnectorEventKind::Request,
        None,
        Some("secret_scan_refused_url"),
    );
    Err(SourceError::InvalidRequest)
}

/// Apply the outbound secret scanner to one response at the connector
/// boundary: a credential echoed by an upstream API (or present in scraped
/// text) can never enter a model-visible result, diagnostic or artifact.
/// Invalid UTF-8 bodies are left untouched — the typed parsers reject them
/// before any text can surface.
fn scrub_response(ctx: &AcquireCtx, mut response: HttpResponse) -> HttpResponse {
    if let Ok(text) = std::str::from_utf8(response.body()) {
        let scrubbed = ctx.secrets().scrub(text);
        if scrubbed != text {
            response.body = scrubbed.into_bytes();
        }
    }
    for header in &mut response.headers {
        let scrubbed = ctx.secrets().scrub(&header.value);
        if scrubbed != header.value {
            header.value = scrubbed;
        }
    }
    response
}

/// Send one request through the injected transport with the connector-side
/// guarantees: liveness check, URL secret scan, quota admission, typed error
/// mapping, response secret scrubbing, and rate-limit header accounting.
pub(crate) async fn send(
    ctx: &AcquireCtx,
    source: &faktor_commerce::SourceId,
    operation: &'static str,
    request: HttpRequest,
) -> Result<HttpResponse, SourceError> {
    ctx.check_alive()?;
    verify_url_secrets(ctx, source, operation, &request)?;
    let host = request.url().host().to_string();
    let permit = ctx.quota().try_acquire(source, ctx.now_ms())?;
    ctx.record(
        source,
        operation,
        crate::context::ConnectorEventKind::Request,
        None,
        Some(&host),
    );
    let started = ctx.now_ms();
    let outcome = ctx.transport().execute(request).await;
    let elapsed = ctx.now_ms().saturating_sub(started);
    match outcome {
        Ok(response) => {
            let response = scrub_response(ctx, response);
            ctx.observe_rate_limit_headers(source, &response, ctx.now_ms());
            ctx.record_timed(
                source,
                operation,
                crate::context::ConnectorEventKind::Response,
                Some(response.status()),
                None,
                Some(elapsed),
            );
            if response.body().len() > MAX_RESPONSE_BYTES {
                return Err(SourceError::ResponseTooLarge);
            }
            ctx.check_alive()?;
            Ok(response)
        }
        Err(error) => {
            if never_left_the_machine(error) {
                ctx.quota().release(permit);
            }
            if error == TransportError::Timeout {
                ctx.record(
                    source,
                    operation,
                    crate::context::ConnectorEventKind::Retry,
                    None,
                    Some("timeout"),
                );
            }
            Err(error.into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::CannedResponse;

    fn source() -> faktor_commerce::SourceId {
        faktor_commerce::SourceId::new("mouser").expect("source")
    }

    #[tokio::test]
    async fn declared_url_credentials_pass_and_undeclared_ones_refuse() {
        let rig = crate::testsupport::Rig::new();
        let ctx = rig.ctx();
        let source = source();

        // A URL with no registered secret scans clean.
        rig.transport.push(CannedResponse::json("{}"));
        let clean =
            HttpRequest::get("https://api.mouser.com/api/v1/search/partnumber?limit=1").unwrap();
        assert!(send(&ctx, &source, "probe", clean).await.is_ok());

        // A credential that is registered and declared is permitted.
        let declared = SecretString::new("declared-url-key-0123456789".to_string());
        ctx.secrets().register(declared.expose());
        let request = HttpRequest::get(&format!(
            "https://api.mouser.com/api/v1/search/partnumber?apiKey={}",
            declared.expose()
        ))
        .unwrap()
        .with_url_credential(&declared);
        rig.transport.push(CannedResponse::json("{}"));
        assert!(send(&ctx, &source, "declared", request).await.is_ok());

        // An *undeclared* registered secret in the URL refuses before dispatch.
        let planted = SecretString::new("planted-url-key-abcdef0123".to_string());
        ctx.secrets().register(planted.expose());
        let before = rig.transport.request_count();
        let hostile = HttpRequest::get(&format!(
            "https://api.mouser.com/api/v1/search/partnumber?token={}",
            planted.expose()
        ))
        .unwrap();
        let error = send(&ctx, &source, "hostile", hostile)
            .await
            .expect_err("an undeclared URL secret must refuse");
        assert_eq!(error, SourceError::InvalidRequest);
        assert_eq!(
            rig.transport.request_count(),
            before,
            "the request must never be dispatched"
        );
        assert!(
            !rig.diagnostics.joined().contains(planted.expose()),
            "the refusal must not record the value"
        );
    }
}
