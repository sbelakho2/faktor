//! The acquisition context a site adapter is invoked with (spec §10).
//!
//! `AcquireCtx` is the adapter-facing context the bridge
//! ([`crate::contract::bridge::Registered`]) builds for one call: it carries
//! the injected transport, the shared quota state, the secret guard, the
//! diagnostics sink, the clock, the request deadline, the cancellation token
//! and the per-request commercial context (account scope, market, locale).
//!
//! The runtime authority is `faktor_commerce::connector::AcquireCtx` plus the
//! `faktor-acquire` `AcquisitionPlanner`: the service plans first and hands
//! the chosen [`Mechanism`] in through this context. A connector executes the
//! mechanism it was given ([`AcquireCtx::mechanism`]) and never selects one
//! itself; mechanism selection is the planner's job alone.
//!
//! Every method here is bounded and panic-free; a poisoned lock degrades to
//! the inner value rather than aborting acquisition.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use faktor_commerce::connector::ProfileIdentity;
use faktor_commerce::text::{AccountScope, Text};
use faktor_commerce::{PriceVisibility, PricingScope, SourceError, SourceId};

use crate::browser::BrowserFallback;
use crate::contract::capture::BrowserExtraction;
use crate::contract::Mechanism;
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
///
/// Besides its own flag it can mirror an external cancellation source
/// through an observed probe (used by the registry bridge to propagate the
/// commerce context's cancellation without adding a runtime dependency
/// here), so `is_cancelled()`/`check_alive()` reflect the caller's real
/// token at every check.
#[derive(Clone, Default)]
pub struct Cancellation {
    cancelled: Arc<AtomicBool>,
    observed: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

impl Cancellation {
    /// A fresh token.
    pub fn new() -> Self {
        Self::default()
    }

    /// A token that is cancelled whenever `probe` reports cancellation.
    /// `probe` must be cheap and non-blocking (it is consulted on every
    /// liveness check).
    pub fn observed(probe: impl Fn() -> bool + Send + Sync + 'static) -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            observed: Some(Arc::new(probe)),
        }
    }

    /// Request cancellation.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    /// Whether cancellation was requested (locally or by the observed
    /// source).
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst) || self.observed.as_ref().is_some_and(|probe| probe())
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
    /// A browser mechanism was executed (stable wire label: `fallback`).
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

/// The access authority one observation was made under (P0 item 8).
///
/// This is acquisition provenance, not site knowledge: the browser
/// authority reports which profile/account produced a capture, and the
/// normalizer derives the domain [`PriceVisibility`] from it. A numeric
/// price never implies `Public` by itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessVisibility {
    /// No login or dedicated profile was involved.
    Anonymous,
    /// A logged-in profile observed the value; it requires authentication.
    Authenticated,
    /// The value was observed under one explicit account scope.
    AccountScoped(AccountScope),
}

/// A refused cache identity: an observation authority may never be stored
/// under a weaker pricing scope than the one it was observed under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum VisibilityScopeError {
    /// An authenticated observation must never be stored under
    /// [`PricingScope::Public`].
    #[error("an authenticated observation must never be stored under the public pricing scope")]
    AuthenticatedNeverPublic,
    /// An account-scoped observation must never be stored under
    /// [`PricingScope::Public`].
    #[error("an account-scoped observation must never be stored under the public pricing scope")]
    AccountScopedNeverPublic,
    /// The requested pricing scope contradicts the capture authority.
    #[error("the requested cache pricing scope does not match the capture authority")]
    AuthorityMismatch,
}

impl AccessVisibility {
    /// The authority a configured connector identity implies when no browser
    /// capture reported one: an account-scoped identity means account-scoped
    /// data; everything else is anonymous.
    pub fn from_identity(identity: &ConnectorIdentity) -> Self {
        match identity.account_scope() {
            Some(scope) => Self::AccountScoped(scope.clone()),
            None => Self::Anonymous,
        }
    }

    /// The stable label.
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Anonymous => "anonymous",
            Self::Authenticated => "authenticated",
            Self::AccountScoped(_) => "account_scoped",
        }
    }

    /// True for [`AccessVisibility::Anonymous`].
    pub const fn is_anonymous(&self) -> bool {
        matches!(self, Self::Anonymous)
    }

    /// The account scope, when the observation is account-scoped.
    pub fn account_scope(&self) -> Option<&AccountScope> {
        match self {
            Self::AccountScoped(scope) => Some(scope),
            _ => None,
        }
    }

    /// The offer price visibility this authority implies.
    pub const fn price_visibility(&self) -> PriceVisibility {
        match self {
            Self::Anonymous => PriceVisibility::Public,
            Self::Authenticated => PriceVisibility::Authenticated,
            Self::AccountScoped(_) => PriceVisibility::AccountSpecific,
        }
    }

    /// The account scope every [`PriceVisibility::AccountSpecific`] price
    /// break must carry, and which no other visibility may carry.
    pub fn price_account_scope(&self) -> Option<AccountScope> {
        self.account_scope().cloned()
    }

    /// The cache pricing scope of this authority. Authenticated observations
    /// are never `Public`.
    pub const fn pricing_scope(&self) -> PricingScope {
        match self {
            Self::Anonymous => PricingScope::Public,
            Self::Authenticated => PricingScope::Authenticated,
            Self::AccountScoped(_) => PricingScope::Account,
        }
    }

    /// Admit one cache pricing scope for this authority. `Public` for an
    /// authenticated or account-scoped observation is a typed refusal, never
    /// a silent promotion to public.
    pub fn admit_cache_scope(
        &self,
        requested: PricingScope,
    ) -> Result<PricingScope, VisibilityScopeError> {
        let implied = self.pricing_scope();
        if requested == implied {
            return Ok(implied);
        }
        Err(match self {
            Self::Authenticated => VisibilityScopeError::AuthenticatedNeverPublic,
            Self::AccountScoped(_) => VisibilityScopeError::AccountScopedNeverPublic,
            Self::Anonymous => VisibilityScopeError::AuthorityMismatch,
        })
    }
}

/// The immutable identity of one registered connector (P0 item 2).
///
/// The daemon configuration owns it and the runtime wrapper injects it into
/// [`AcquireCtx`]; a tool/model request can never manufacture or override it.
/// It carries the stable browser profile (source, profile name, account,
/// egress) plus the commercial scoping the runtime acquired under.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ConnectorIdentity {
    profile: Option<ProfileIdentity>,
    account_scope: Option<AccountScope>,
    market: Option<Text<32>>,
    locale: Option<Text<32>>,
}

impl ConnectorIdentity {
    /// An empty identity (unauthenticated, unscoped).
    pub fn anonymous() -> Self {
        Self::default()
    }

    /// Set the stable browser/profile identity.
    pub fn with_profile(mut self, profile: ProfileIdentity) -> Self {
        self.profile = Some(profile);
        self
    }

    /// Set the explicit account scope.
    pub fn with_account_scope(mut self, account_scope: AccountScope) -> Self {
        self.account_scope = Some(account_scope);
        self
    }

    /// Set the market.
    pub fn with_market(mut self, market: Text<32>) -> Self {
        self.market = Some(market);
        self
    }

    /// Set the locale.
    pub fn with_locale(mut self, locale: Text<32>) -> Self {
        self.locale = Some(locale);
        self
    }

    /// The stable browser profile, when this connector has one.
    pub fn profile(&self) -> Option<&ProfileIdentity> {
        self.profile.as_ref()
    }

    /// The account scope: the explicit scope when configured, otherwise the
    /// scope carried by the profile identity.
    pub fn account_scope(&self) -> Option<&AccountScope> {
        self.account_scope.as_ref().or_else(|| {
            self.profile
                .as_ref()
                .and_then(|profile| profile.account.as_ref())
        })
    }

    /// The market, when configured.
    pub fn market(&self) -> Option<&Text<32>> {
        self.market.as_ref()
    }

    /// The locale, when configured.
    pub fn locale(&self) -> Option<&Text<32>> {
        self.locale.as_ref()
    }

    /// True when no component is configured.
    pub fn is_empty(&self) -> bool {
        self.profile.is_none()
            && self.account_scope.is_none()
            && self.market.is_none()
            && self.locale.is_none()
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
    mechanism: Option<Mechanism>,
    browser: Option<Arc<dyn BrowserFallback>>,
    browser_extraction: Option<Arc<dyn BrowserExtraction>>,
    identity: ConnectorIdentity,
}

impl fmt::Debug for AcquireCtx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never renders transport/browser internals or any credential.
        f.debug_struct("AcquireCtx")
            .field("operation_id", &self.operation_id)
            .field("session_id", &self.session_id)
            .field("deadline_ms", &self.deadline_ms)
            .field("mechanism", &self.mechanism)
            .field("browser", &self.browser.is_some())
            .field("account_scoped", &self.identity.account_scope().is_some())
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
            mechanism: None,
            browser: None,
            browser_extraction: None,
            identity: ConnectorIdentity::anonymous(),
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

    /// The mechanism the runtime planner chose for this call, when the
    /// bridge set one. Connectors execute exactly this mechanism.
    pub fn mechanism(&self) -> Option<Mechanism> {
        self.mechanism
    }

    /// The immutable connector identity the runtime wrapper injected.
    pub fn identity(&self) -> &ConnectorIdentity {
        &self.identity
    }

    /// The account scope of this acquisition, when any.
    pub fn account_scope(&self) -> Option<&AccountScope> {
        self.identity.account_scope()
    }

    /// The market of this acquisition, when known.
    pub fn market(&self) -> Option<&Text<32>> {
        self.identity.market()
    }

    /// The locale of this acquisition, when known.
    pub fn locale(&self) -> Option<&Text<32>> {
        self.identity.locale()
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
    mechanism: Option<Mechanism>,
    browser: Option<Arc<dyn BrowserFallback>>,
    browser_extraction: Option<Arc<dyn BrowserExtraction>>,
    identity: ConnectorIdentity,
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

    /// The mechanism the runtime planner chose for this call.
    pub fn mechanism(mut self, mechanism: Mechanism) -> Self {
        self.mechanism = Some(mechanism);
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

    /// The whole injected connector identity (the runtime wrapper's seam).
    /// Replaces every component; later component setters still apply.
    pub fn identity(mut self, identity: ConnectorIdentity) -> Self {
        self.identity = identity;
        self
    }

    /// The account scope (component of the identity).
    pub fn account_scope(mut self, account_scope: AccountScope) -> Self {
        self.identity.account_scope = Some(account_scope);
        self
    }

    /// The market (component of the identity).
    pub fn market(mut self, market: Text<32>) -> Self {
        self.identity.market = Some(market);
        self
    }

    /// The locale (component of the identity).
    pub fn locale(mut self, locale: Text<32>) -> Self {
        self.identity.locale = Some(locale);
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
            mechanism: self.mechanism,
            browser: self.browser,
            browser_extraction: self.browser_extraction,
            identity: self.identity,
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

    fn scope(raw: &str) -> AccountScope {
        AccountScope::new(raw).expect("scope")
    }

    #[test]
    fn access_authority_never_degrades_to_public() {
        let anonymous = AccessVisibility::Anonymous;
        assert_eq!(anonymous.price_visibility(), PriceVisibility::Public);
        assert_eq!(anonymous.price_account_scope(), None);
        assert_eq!(anonymous.pricing_scope(), PricingScope::Public);
        assert_eq!(
            anonymous.admit_cache_scope(PricingScope::Public),
            Ok(PricingScope::Public)
        );

        let authenticated = AccessVisibility::Authenticated;
        assert_eq!(
            authenticated.price_visibility(),
            PriceVisibility::Authenticated
        );
        assert_eq!(authenticated.price_account_scope(), None);
        assert_eq!(authenticated.pricing_scope(), PricingScope::Authenticated);
        assert_ne!(authenticated.pricing_scope(), PricingScope::Public);
        for requested in [
            PricingScope::Public,
            PricingScope::Account,
            PricingScope::Unknown,
        ] {
            assert_eq!(
                authenticated.admit_cache_scope(requested),
                Err(VisibilityScopeError::AuthenticatedNeverPublic),
                "an authenticated observation must never be stored as {requested:?}"
            );
        }

        let account = AccessVisibility::AccountScoped(scope("buyer-a"));
        assert_eq!(account.price_visibility(), PriceVisibility::AccountSpecific);
        assert_eq!(account.price_account_scope(), Some(scope("buyer-a")));
        assert_eq!(account.pricing_scope(), PricingScope::Account);
        assert_eq!(
            account.admit_cache_scope(PricingScope::Public),
            Err(VisibilityScopeError::AccountScopedNeverPublic)
        );
        assert_eq!(
            account.admit_cache_scope(PricingScope::Account),
            Ok(PricingScope::Account)
        );
        // Anonymous cannot claim a stronger cache scope either.
        assert_eq!(
            anonymous.admit_cache_scope(PricingScope::Account),
            Err(VisibilityScopeError::AuthorityMismatch)
        );
    }

    #[test]
    fn connector_identity_is_read_from_the_injected_seam_only() {
        let profile =
            ProfileIdentity::new(SourceId::new("1688").expect("source"), "procurement-cn")
                .expect("profile");
        let identity = ConnectorIdentity::anonymous()
            .with_profile(profile.clone())
            .with_account_scope(scope("buyer-a"))
            .with_market(Text::<32>::new("CN").expect("market"))
            .with_locale(Text::<32>::new("zh-CN").expect("locale"));
        assert_eq!(identity.profile(), Some(&profile));
        assert_eq!(identity.account_scope(), Some(&scope("buyer-a")));
        assert_eq!(identity.market().map(Text::as_str), Some("CN"));
        assert_eq!(identity.locale().map(Text::as_str), Some("zh-CN"));
        assert!(!identity.is_empty());
        assert!(ConnectorIdentity::anonymous().is_empty());

        // The profile's own account is the fallback when no explicit scope
        // was configured; the explicit scope wins when both exist.
        let mut profile_account = profile;
        profile_account.account = Some(scope("profile-account"));
        let fallback = ConnectorIdentity::anonymous().with_profile(profile_account);
        assert_eq!(fallback.account_scope(), Some(&scope("profile-account")));

        // The identity reaches the context through the builder; a caller can
        // still address components directly.
        let ctx = AcquireCtx::builder(
            Arc::new(crate::testing::NoopTransport),
            Arc::new(QuotaState::new()),
            Arc::new(SecretGuard::new()),
        )
        .identity(identity.clone())
        .build();
        assert_eq!(ctx.identity(), &identity);
        assert_eq!(ctx.account_scope(), Some(&scope("buyer-a")));
        assert_eq!(ctx.market().map(Text::as_str), Some("CN"));
        assert_eq!(ctx.locale().map(Text::as_str), Some("zh-CN"));
        assert_eq!(
            AccessVisibility::from_identity(ctx.identity()),
            AccessVisibility::AccountScoped(scope("buyer-a"))
        );
        assert_eq!(
            AccessVisibility::from_identity(&ConnectorIdentity::anonymous()),
            AccessVisibility::Anonymous
        );
    }
}
