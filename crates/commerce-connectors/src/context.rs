//! The acquisition context a connector is invoked with (spec §10).
//!
//! `AcquireCtx` is the connector-facing half of the acquire runtime: it
//! carries the injected transport, the shared quota state, the secret guard,
//! the diagnostics sink, the clock, the request deadline and the
//! cancellation token, plus the per-request commercial context (account
//! scope, market, locale) and the browser-fallback policy.
//!
//! The crate root documents the seam: `faktor-acquire` (build order step 4)
//! owns the production context and re-exports these types; today the
//! connectors crate defines them so the adapters are complete and testable
//! without the runtime.
//!
//! Every method here is bounded and panic-free; a poisoned lock degrades to
//! the inner value rather than aborting acquisition.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use faktor_commerce::text::{AccountScope, Text};
use faktor_commerce::{SourceError, SourceId};

use crate::browser::{BrowserFallback, FallbackPolicy};
use crate::contract::capture::BrowserExtraction;
use crate::http::{HttpResponse, HttpTransport};
use crate::quota::QuotaState;
use crate::secrets::SecretGuard;

/// At most this many retained diagnostics events (bounded memory).
const MAX_RETAINED_EVENTS: usize = 1024;
/// At most this many characters of rendered event detail.
const MAX_DETAIL_CHARS: usize = 256;

/// A clock seam: connectors never read the wall clock directly, so tests
/// and the runtime can drive time deterministically.
pub trait Clock: Send + Sync {
    /// Milliseconds since the Unix epoch.
    fn now_ms(&self) -> u64;
}

/// The production clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        crate::normalize::system_now_ms()
    }
}

/// A manually advanced clock (tests, deterministic replays).
#[derive(Debug, Default)]
pub struct ManualClock {
    now_ms: AtomicU64,
}

impl ManualClock {
    /// A clock starting at `now_ms`.
    pub fn new(now_ms: u64) -> Self {
        Self {
            now_ms: AtomicU64::new(now_ms),
        }
    }

    /// Advance by `delta_ms`.
    pub fn advance(&self, delta_ms: u64) {
        self.now_ms.fetch_add(delta_ms, Ordering::SeqCst);
    }

    /// Set the clock.
    pub fn set(&self, now_ms: u64) {
        self.now_ms.store(now_ms, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.now_ms.load(Ordering::SeqCst)
    }
}

/// A cooperative cancellation token.
#[derive(Clone, Default)]
pub struct Cancellation {
    cancelled: Arc<AtomicBool>,
}

impl Cancellation {
    /// A fresh token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// Whether cancellation was requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

impl fmt::Debug for Cancellation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cancellation")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

/// The kind of one connector event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectorEventKind {
    /// A request is about to be sent.
    Request,
    /// A response was received.
    Response,
    /// A retryable condition occurred.
    Retry,
    /// A fallback decision was made.
    Fallback,
    /// Quota state changed.
    Quota,
    /// A schema/normalization observation.
    Schema,
    /// A free-form note (still scrubbed).
    Note,
}

impl ConnectorEventKind {
    /// The stable label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Request => "request",
            Self::Response => "response",
            Self::Retry => "retry",
            Self::Fallback => "fallback",
            Self::Quota => "quota",
            Self::Schema => "schema",
            Self::Note => "note",
        }
    }
}

/// One bounded, scrubbed connector event.
///
/// `detail` has already passed through [`SecretGuard::scrub`]; nothing in
/// this struct can carry a credential, a request body or a raw upstream
/// message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectorEvent {
    /// The source id.
    pub source: String,
    /// The operation label.
    pub operation: &'static str,
    /// The event kind.
    pub kind: ConnectorEventKind,
    /// The HTTP status, when there is one.
    pub status: Option<u16>,
    /// A bounded, scrubbed detail.
    pub detail: Option<String>,
    /// Elapsed milliseconds, when known.
    pub elapsed_ms: Option<u64>,
}

impl ConnectorEvent {
    /// A bounded single-line rendering.
    pub fn render(&self) -> String {
        let detail = self
            .detail
            .as_deref()
            .map(|detail| crate::normalize::sanitize_excerpt(detail, MAX_DETAIL_CHARS))
            .unwrap_or_default();
        let status = self
            .status
            .map(|status| status.to_string())
            .unwrap_or_else(|| "-".to_string());
        let elapsed = self
            .elapsed_ms
            .map(|elapsed| elapsed.to_string())
            .unwrap_or_else(|| "-".to_string());
        format!(
            "source={} operation={} kind={} status={status} elapsed_ms={elapsed} detail={detail}",
            self.source,
            self.operation,
            self.kind.as_str()
        )
    }
}

/// The diagnostics sink. Connectors never format free text themselves:
/// every event passes through [`AcquireCtx::record`], which scrubs it.
pub trait Diagnostics: Send + Sync {
    /// Record one event.
    fn record(&self, event: &ConnectorEvent);
}

/// Discards every event (the default; the daemon substitutes a tracing
/// adapter).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoopDiagnostics;

impl Diagnostics for NoopDiagnostics {
    fn record(&self, _event: &ConnectorEvent) {}
}

/// Retains bounded rendered events in memory (tests, `doctor` surfaces).
#[derive(Debug, Default)]
pub struct MemoryDiagnostics {
    events: Mutex<Vec<String>>,
}

impl MemoryDiagnostics {
    /// An empty sink.
    pub fn new() -> Self {
        Self::default()
    }

    /// The retained rendered events.
    pub fn rendered(&self) -> Vec<String> {
        self.lock().clone()
    }

    /// All retained events joined by newlines.
    pub fn joined(&self) -> String {
        self.lock().join("\n")
    }

    /// The number of retained events.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// True when nothing was recorded.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<String>> {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Diagnostics for MemoryDiagnostics {
    fn record(&self, event: &ConnectorEvent) {
        let mut events = self.lock();
        if events.len() >= MAX_RETAINED_EVENTS {
            events.remove(0);
        }
        events.push(event.render());
    }
}

/// The acquisition context one connector call runs in.
pub struct AcquireCtx {
    operation_id: u64,
    session_id: u64,
    transport: Arc<dyn HttpTransport>,
    quota: Arc<QuotaState>,
    secrets: Arc<SecretGuard>,
    diagnostics: Arc<dyn Diagnostics>,
    clock: Arc<dyn Clock>,
    cancel: Cancellation,
    deadline_ms: Option<u64>,
    fallback_policy: FallbackPolicy,
    browser: Option<Arc<dyn BrowserFallback>>,
    browser_extraction: Option<Arc<dyn BrowserExtraction>>,
    account_scope: Option<AccountScope>,
    market: Option<Text<32>>,
    locale: Option<Text<32>>,
}

impl fmt::Debug for AcquireCtx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never renders transport/browser internals or any credential.
        f.debug_struct("AcquireCtx")
            .field("operation_id", &self.operation_id)
            .field("session_id", &self.session_id)
            .field("deadline_ms", &self.deadline_ms)
            .field("fallback_enabled", &self.fallback_policy.is_enabled())
            .field("browser", &self.browser.is_some())
            .field("cancelled", &self.cancel.is_cancelled())
            .finish()
    }
}

impl AcquireCtx {
    /// Start building a context with the mandatory seams.
    pub fn builder(
        transport: Arc<dyn HttpTransport>,
        quota: Arc<QuotaState>,
        secrets: Arc<SecretGuard>,
    ) -> AcquireCtxBuilder {
        AcquireCtxBuilder {
            operation_id: 0,
            session_id: 0,
            transport,
            quota,
            secrets,
            diagnostics: Arc::new(NoopDiagnostics),
            clock: Arc::new(SystemClock),
            cancel: Cancellation::new(),
            deadline_ms: None,
            fallback_policy: FallbackPolicy::default(),
            browser: None,
            browser_extraction: None,
            account_scope: None,
            market: None,
            locale: None,
        }
    }

    /// The runtime operation id.
    pub fn operation_id(&self) -> u64 {
        self.operation_id
    }

    /// The session id.
    pub fn session_id(&self) -> u64 {
        self.session_id
    }

    /// The injected transport.
    pub fn transport(&self) -> &Arc<dyn HttpTransport> {
        &self.transport
    }

    /// The shared quota state.
    pub fn quota(&self) -> &Arc<QuotaState> {
        &self.quota
    }

    /// The shared secret guard.
    pub fn secrets(&self) -> &Arc<SecretGuard> {
        &self.secrets
    }

    /// The diagnostics sink.
    pub fn diagnostics(&self) -> &Arc<dyn Diagnostics> {
        &self.diagnostics
    }

    /// The injected browser authority, when one is available.
    pub fn browser(&self) -> Option<&Arc<dyn BrowserFallback>> {
        self.browser.as_ref()
    }

    /// The injected browser-acquisition authority, when one is available.
    pub fn browser_extraction(&self) -> Option<&Arc<dyn BrowserExtraction>> {
        self.browser_extraction.as_ref()
    }

    /// The browser-fallback policy flag.
    pub fn fallback_policy(&self) -> FallbackPolicy {
        self.fallback_policy
    }

    /// The account scope of this acquisition, when any.
    pub fn account_scope(&self) -> Option<&AccountScope> {
        self.account_scope.as_ref()
    }

    /// The market of this acquisition, when known.
    pub fn market(&self) -> Option<&Text<32>> {
        self.market.as_ref()
    }

    /// The locale of this acquisition, when known.
    pub fn locale(&self) -> Option<&Text<32>> {
        self.locale.as_ref()
    }

    /// The current time.
    pub fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    /// The absolute deadline, when one was set.
    pub fn deadline_ms(&self) -> Option<u64> {
        self.deadline_ms
    }

    /// The cancellation token.
    pub fn cancellation(&self) -> &Cancellation {
        &self.cancel
    }

    /// Whether cancellation was requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Fail with [`SourceError::Cancelled`] or [`SourceError::Deadline`]
    /// when the call must stop. Connectors call this before and after every
    /// await point.
    pub fn check_alive(&self) -> Result<(), SourceError> {
        if self.is_cancelled() {
            return Err(SourceError::Cancelled);
        }
        if let Some(deadline_ms) = self.deadline_ms {
            if self.now_ms() >= deadline_ms {
                return Err(SourceError::Deadline);
            }
        }
        Ok(())
    }

    /// Record one scrubbed diagnostic event.
    pub fn record(
        &self,
        source: &SourceId,
        operation: &'static str,
        kind: ConnectorEventKind,
        status: Option<u16>,
        detail: Option<&str>,
    ) {
        self.record_timed(source, operation, kind, status, detail, None);
    }

    /// Record one scrubbed diagnostic event with a timing.
    pub fn record_timed(
        &self,
        source: &SourceId,
        operation: &'static str,
        kind: ConnectorEventKind,
        status: Option<u16>,
        detail: Option<&str>,
        elapsed_ms: Option<u64>,
    ) {
        let detail = detail.map(|detail| self.secrets.scrub(detail));
        let event = ConnectorEvent {
            source: source.as_str().to_string(),
            operation,
            kind,
            status,
            detail,
            elapsed_ms,
        };
        self.diagnostics.record(&event);
    }

    /// Consume `X-RateLimit-*` / `Retry-After` headers into the quota state
    /// (spec §10: never rely only on hard-coded limits).
    pub fn observe_rate_limit_headers(
        &self,
        source: &SourceId,
        response: &HttpResponse,
        now_ms: u64,
    ) {
        let limit = response
            .header("x-ratelimit-limit")
            .and_then(parse_bounded_u32);
        let remaining = response
            .header("x-ratelimit-remaining")
            .and_then(parse_bounded_u32);
        let retry_after = response
            .header("retry-after")
            .and_then(crate::quota::parse_retry_after_ms);
        if response.status() == 429 {
            self.quota.observe_rate_limited(source, retry_after, now_ms);
            self.record(
                source,
                "quota",
                ConnectorEventKind::Quota,
                Some(response.status()),
                Some("rate_limited"),
            );
            return;
        }
        if limit.is_some() || remaining.is_some() || retry_after.is_some() {
            let reset_at = retry_after.map(|ms| now_ms.saturating_add(ms));
            self.quota
                .observe_headers(source, limit, remaining, reset_at, now_ms);
            self.record(
                source,
                "quota",
                ConnectorEventKind::Quota,
                Some(response.status()),
                Some("headers_observed"),
            );
        }
    }
}

/// Parse a bounded non-negative header integer (rejects signs, exponents,
/// whitespace and oversized values).
fn parse_bounded_u32(raw: &str) -> Option<u32> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 10 || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    trimmed.parse::<u32>().ok()
}

/// The builder for [`AcquireCtx`].
pub struct AcquireCtxBuilder {
    operation_id: u64,
    session_id: u64,
    transport: Arc<dyn HttpTransport>,
    quota: Arc<QuotaState>,
    secrets: Arc<SecretGuard>,
    diagnostics: Arc<dyn Diagnostics>,
    clock: Arc<dyn Clock>,
    cancel: Cancellation,
    deadline_ms: Option<u64>,
    fallback_policy: FallbackPolicy,
    browser: Option<Arc<dyn BrowserFallback>>,
    browser_extraction: Option<Arc<dyn BrowserExtraction>>,
    account_scope: Option<AccountScope>,
    market: Option<Text<32>>,
    locale: Option<Text<32>>,
}

impl AcquireCtxBuilder {
    /// The runtime operation/session identity.
    pub fn operation(mut self, operation_id: u64, session_id: u64) -> Self {
        self.operation_id = operation_id;
        self.session_id = session_id;
        self
    }

    /// The clock.
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// The diagnostics sink.
    pub fn diagnostics(mut self, diagnostics: Arc<dyn Diagnostics>) -> Self {
        self.diagnostics = diagnostics;
        self
    }

    /// The cancellation token.
    pub fn cancellation(mut self, cancel: Cancellation) -> Self {
        self.cancel = cancel;
        self
    }

    /// An absolute deadline in milliseconds since the Unix epoch.
    pub fn deadline_ms(mut self, deadline_ms: u64) -> Self {
        self.deadline_ms = Some(deadline_ms);
        self
    }

    /// The browser-fallback policy.
    pub fn fallback_policy(mut self, policy: FallbackPolicy) -> Self {
        self.fallback_policy = policy;
        self
    }

    /// The injected browser authority.
    pub fn browser(mut self, browser: Arc<dyn BrowserFallback>) -> Self {
        self.browser = Some(browser);
        self
    }

    /// The injected browser-acquisition authority.
    pub fn browser_extraction(mut self, browser: Arc<dyn BrowserExtraction>) -> Self {
        self.browser_extraction = Some(browser);
        self
    }

    /// The account scope.
    pub fn account_scope(mut self, account_scope: AccountScope) -> Self {
        self.account_scope = Some(account_scope);
        self
    }

    /// The market.
    pub fn market(mut self, market: Text<32>) -> Self {
        self.market = Some(market);
        self
    }

    /// The locale.
    pub fn locale(mut self, locale: Text<32>) -> Self {
        self.locale = Some(locale);
        self
    }

    /// Finish the context.
    pub fn build(self) -> AcquireCtx {
        AcquireCtx {
            operation_id: self.operation_id,
            session_id: self.session_id,
            transport: self.transport,
            quota: self.quota,
            secrets: self.secrets,
            diagnostics: self.diagnostics,
            clock: self.clock,
            cancel: self.cancel,
            deadline_ms: self.deadline_ms,
            fallback_policy: self.fallback_policy,
            browser: self.browser,
            browser_extraction: self.browser_extraction,
            account_scope: self.account_scope,
            market: self.market,
            locale: self.locale,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_parsing_rejects_hostile_values() {
        assert_eq!(parse_bounded_u32("1000"), Some(1000));
        assert_eq!(parse_bounded_u32(" 42 "), Some(42));
        for bad in ["-1", "1e3", "999999999999", "1.5", "", "abc"] {
            assert_eq!(parse_bounded_u32(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn cancellation_and_deadline_are_typed() {
        let ctx = AcquireCtx::builder(
            Arc::new(crate::testing::NoopTransport),
            Arc::new(QuotaState::new()),
            Arc::new(SecretGuard::new()),
        )
        .clock(Arc::new(ManualClock::new(1_000)))
        .deadline_ms(2_000)
        .build();
        assert!(ctx.check_alive().is_ok());
        ctx.cancellation().cancel();
        assert_eq!(ctx.check_alive(), Err(SourceError::Cancelled));
    }
}
