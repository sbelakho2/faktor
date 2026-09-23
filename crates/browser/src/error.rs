//! Typed browser-authority errors (spec §15). Every failure mode is a
//! distinct variant — there is no generic "scrape failed" and no stringly
//! error surface. The commerce/acquire layers map these onto `SourceError`
//! without re-parsing messages.

use std::fmt;

/// Which human-verification interstitial was detected. Detection is generic
/// (DOM landmarks and page text), never site knowledge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationKind {
    /// A login form is present: credentials are required.
    LoginForm,
    /// A CAPTCHA container (reCAPTCHA/hCaptcha/geetest/slider iframe) is
    /// present.
    Captcha,
    /// A security slider / press-and-hold challenge is present.
    SecuritySlider,
    /// An "access denied"/"forbidden" interstitial.
    AccessDenied,
    /// A rate-limit page ("too many requests", "try again later").
    RateLimit,
    /// A generic challenge/verification interstitial we cannot classify
    /// further.
    Interstitial,
    /// A verification signal was detected but did not match a known kind.
    Unknown,
}

impl VerificationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            VerificationKind::LoginForm => "login_form",
            VerificationKind::Captcha => "captcha",
            VerificationKind::SecuritySlider => "security_slider",
            VerificationKind::AccessDenied => "access_denied",
            VerificationKind::RateLimit => "rate_limit",
            VerificationKind::Interstitial => "interstitial",
            VerificationKind::Unknown => "unknown",
        }
    }
}

impl fmt::Display for VerificationKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A browser-authority failure. Typed, bounded, no hidden state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrowserError {
    /// Browser authority is disabled by config: no process, no profile, no
    /// network activity.
    Disabled,
    /// Config or caller input violates a documented bound / invariant.
    InvalidConfig { detail: String },
    /// No usable Chromium executable (or it refused to start).
    BrowserUnavailable { detail: String },
    /// The supervised Chromium child exited or its CDP socket died.
    BrowserCrashed { detail: String },
    /// The DevTools endpoint did not appear before the launch deadline.
    LaunchTimeout { waited_ms: u64 },
    /// CDP transport/protocol failure.
    Cdp { detail: String },
    /// A CDP command returned an error object.
    CdpCommand {
        method: String,
        code: i64,
        message: String,
    },
    /// The operation was cancelled by its caller token.
    Cancelled,
    /// The operation deadline expired.
    Deadline { detail: String },
    /// Human verification is required; automated work for the profile stops
    /// until a human completes it.
    VerificationRequired { kind: VerificationKind },
    /// The destination requires authentication.
    AuthenticationRequired,
    /// The destination rate-limited us; `retry_after_ms` when advertised.
    RateLimited { retry_after_ms: Option<u64> },
    /// The egress broker is not running / not reachable.
    EgressUnavailable { detail: String },
    /// The destination is not permitted by the connector's destination
    /// policy (first-party only by default).
    DestinationBlocked { host: String, reason: String },
    /// A download was refused (downloads are disabled by default). The URL is
    /// redacted at the logging boundary, never here.
    DownloadBlocked { url: String },
    /// A download failed enforcement or capture (malformed progress fields,
    /// over-bound stream, unsafe filename, storage failure). Download
    /// enforcement fails closed: anything unverifiable is refused.
    DownloadRejected { url: String, detail: String },
    /// The instance is retiring (idle shutdown or an operational retire
    /// raced this acquisition). No page was opened; a retry starts a fresh
    /// browser.
    Retiring { detail: String },
    /// A response/body exceeded a configured bound. `observed_bytes` is
    /// `None` when the size was rejected before decoding.
    ResponseTooLarge {
        limit_bytes: usize,
        observed_bytes: Option<usize>,
    },
    /// Profile storage failure (permissions, traversal, corrupt layout).
    Profile { detail: String },
    /// The CDP event stream lost events: the observation history is
    /// incomplete and no complete-history answer can be given. `skipped` is
    /// the number of events known to be missing.
    EventStreamLagged { skipped: u64 },
    /// A configured bound (pages, browsers, records) was reached.
    Bound { detail: String },
    /// Anything else, with context.
    Internal { detail: String },
}

impl BrowserError {
    pub fn cdp(detail: impl Into<String>) -> Self {
        BrowserError::Cdp {
            detail: detail.into(),
        }
    }

    pub fn profile(detail: impl Into<String>) -> Self {
        BrowserError::Profile {
            detail: detail.into(),
        }
    }

    pub fn invalid_config(detail: impl Into<String>) -> Self {
        BrowserError::InvalidConfig {
            detail: detail.into(),
        }
    }

    pub fn internal(detail: impl Into<String>) -> Self {
        BrowserError::Internal {
            detail: detail.into(),
        }
    }

    pub fn bound(detail: impl Into<String>) -> Self {
        BrowserError::Bound {
            detail: detail.into(),
        }
    }

    pub fn retiring(detail: impl Into<String>) -> Self {
        BrowserError::Retiring {
            detail: detail.into(),
        }
    }

    /// Machine-readable, stable error code (snake_case) for logs and the
    /// protocol boundary.
    pub fn code(&self) -> &'static str {
        match self {
            BrowserError::Disabled => "disabled",
            BrowserError::InvalidConfig { .. } => "invalid_config",
            BrowserError::BrowserUnavailable { .. } => "browser_unavailable",
            BrowserError::BrowserCrashed { .. } => "browser_crashed",
            BrowserError::LaunchTimeout { .. } => "launch_timeout",
            BrowserError::Cdp { .. } => "cdp",
            BrowserError::CdpCommand { .. } => "cdp_command",
            BrowserError::Cancelled => "cancelled",
            BrowserError::Deadline { .. } => "deadline",
            BrowserError::VerificationRequired { .. } => "verification_required",
            BrowserError::AuthenticationRequired => "authentication_required",
            BrowserError::RateLimited { .. } => "rate_limited",
            BrowserError::EgressUnavailable { .. } => "egress_unavailable",
            BrowserError::DestinationBlocked { .. } => "destination_blocked",
            BrowserError::DownloadBlocked { .. } => "download_blocked",
            BrowserError::DownloadRejected { .. } => "download_rejected",
            BrowserError::Retiring { .. } => "retiring",
            BrowserError::ResponseTooLarge { .. } => "response_too_large",
            BrowserError::Profile { .. } => "profile",
            BrowserError::EventStreamLagged { .. } => "event_stream_lagged",
            BrowserError::Bound { .. } => "bound",
            BrowserError::Internal { .. } => "internal",
        }
    }

    /// Transient failures may be retried by the acquire layer with bounded
    /// backoff; auth/CAPTCHA/quota surface immediately (spec §15).
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            BrowserError::BrowserCrashed { .. }
                | BrowserError::Cdp { .. }
                | BrowserError::EgressUnavailable { .. }
                | BrowserError::LaunchTimeout { .. }
                | BrowserError::Deadline { .. }
                | BrowserError::Retiring { .. }
        )
    }
}

impl fmt::Display for BrowserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BrowserError::Disabled => write!(f, "browser authority disabled"),
            BrowserError::InvalidConfig { detail } => write!(f, "invalid browser config: {detail}"),
            BrowserError::BrowserUnavailable { detail } => {
                write!(f, "browser unavailable: {detail}")
            }
            BrowserError::BrowserCrashed { detail } => write!(f, "browser crashed: {detail}"),
            BrowserError::LaunchTimeout { waited_ms } => {
                write!(f, "launch timed out after {waited_ms}ms")
            }
            BrowserError::Cdp { detail } => write!(f, "cdp failure: {detail}"),
            BrowserError::CdpCommand {
                method,
                code,
                message,
            } => write!(f, "cdp command {method} failed ({code}): {message}"),
            BrowserError::Cancelled => write!(f, "cancelled"),
            BrowserError::Deadline { detail } => write!(f, "deadline exceeded: {detail}"),
            BrowserError::VerificationRequired { kind } => {
                write!(f, "verification required: {kind}")
            }
            BrowserError::AuthenticationRequired => write!(f, "authentication required"),
            BrowserError::RateLimited { retry_after_ms } => match retry_after_ms {
                Some(ms) => write!(f, "rate limited; retry after {ms}ms"),
                None => write!(f, "rate limited"),
            },
            BrowserError::EgressUnavailable { detail } => {
                write!(f, "egress broker unavailable: {detail}")
            }
            BrowserError::DestinationBlocked { host, reason } => {
                write!(f, "destination blocked: {host} ({reason})")
            }
            BrowserError::DownloadBlocked { url } => {
                write!(f, "download blocked by policy: {url}")
            }
            BrowserError::DownloadRejected { url, detail } => {
                write!(f, "download rejected: {url} ({detail})")
            }
            BrowserError::Retiring { detail } => {
                write!(f, "browser instance is retiring: {detail}")
            }
            BrowserError::ResponseTooLarge {
                limit_bytes,
                observed_bytes,
            } => match observed_bytes {
                Some(n) => write!(f, "response too large: {n} bytes > {limit_bytes}"),
                None => write!(f, "response too large: limit {limit_bytes} bytes"),
            },
            BrowserError::Profile { detail } => write!(f, "profile failure: {detail}"),
            BrowserError::EventStreamLagged { skipped } => {
                write!(f, "cdp event stream lagged: {skipped} events lost")
            }
            BrowserError::Bound { detail } => write!(f, "browser bound reached: {detail}"),
            BrowserError::Internal { detail } => write!(f, "internal browser failure: {detail}"),
        }
    }
}

impl std::error::Error for BrowserError {}

impl From<faktor_core::error::Error> for BrowserError {
    fn from(e: faktor_core::error::Error) -> Self {
        BrowserError::internal(e.to_string())
    }
}
