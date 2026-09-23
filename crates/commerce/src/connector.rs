//! The connector contract (`docs/acquire.md` §10), the acquisition context,
//! and the connector registry.
//!
//! Connectors never create their own HTTP client or Chromium process: the
//! checked egress authority ([`HttpTransport`]) and the browser authority
//! ([`BrowserAuthority`]) are injected seams. All external bytes are bounded
//! before they cross the seam, and [`TransportResponse`] exposes only an
//! allowlist of headers (status, content type, ETag, Last-Modified,
//! Retry-After) — a raw header map is structurally unavailable, so auth
//! headers and cookies cannot leak into commerce data by construction.
//!
//! The [`ConnectorRegistry`] owns per-source policy and *independent* circuit
//! breakers for the API path and the browser-fallback path. Browser fallback
//! is for legitimate field availability/resilience, never quota evasion: a
//! rate-limited or quota-exhausted API may not be bypassed with the browser.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::cache::ConditionalValidators;
use crate::error::{ConnectorHealth, SourceError, VerificationKind};
use crate::identity::ProductIdentity;
use crate::offer::{CommercialOffer, OfferProvenance, PriceRange, StockState, Supplier};
use crate::packaging::PackagingType;
use crate::quantity::NonZeroQuantity;
use crate::query::{FreshnessMode, ProductRequest, QuoteRequest, SearchRequest, SourceSet};
use crate::quote::QuoteResolution;
use crate::text::{AccountScope, CanonicalUrl, SourceId, Text};

/// A stable browser/profile identity (`docs/acquire.md` §9).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileIdentity {
    /// The source this profile belongs to.
    pub source: SourceId,
    /// The profile name.
    pub profile: Text<64>,
    /// The account the profile is logged into, when known.
    pub account: Option<AccountScope>,
    /// The egress identity.
    pub egress: Option<Text<64>>,
}

impl ProfileIdentity {
    /// Validate and construct.
    pub fn new(source: SourceId, profile: &str) -> Result<Self, SourceError> {
        let profile = Text::<64>::new(profile).map_err(|_| SourceError::InvalidRequest)?;
        Ok(Self {
            source,
            profile,
            account: None,
            egress: None,
        })
    }

    /// A stable identity key for job digests.
    pub fn identity_key(&self) -> String {
        format!(
            "{}:{}:{}:{}",
            self.source,
            self.profile,
            self.account
                .as_ref()
                .map(AccountScope::as_str)
                .unwrap_or("-"),
            self.egress.as_ref().map(Text::as_str).unwrap_or("-"),
        )
    }
}

/// A planner-facing quota snapshot for one source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuotaState {
    /// The source.
    pub source: SourceId,
    /// Remaining requests in the window, when known.
    pub remaining: Option<u64>,
    /// When the window resets, when known.
    pub reset_ms: Option<u64>,
}

/// Cooperative cancellation. Cloneable; `cancel` wakes every waiter.
#[derive(Clone)]
pub struct Cancellation {
    tx: Arc<tokio::sync::watch::Sender<bool>>,
    rx: tokio::sync::watch::Receiver<bool>,
}

impl Default for Cancellation {
    fn default() -> Self {
        Self::new()
    }
}

impl Cancellation {
    /// A fresh, uncancelled token.
    pub fn new() -> Self {
        let (tx, rx) = tokio::sync::watch::channel(false);
        Self {
            tx: Arc::new(tx),
            rx,
        }
    }

    /// Cancel; idempotent.
    pub fn cancel(&self) {
        let _ = self.tx.send(true);
    }

    /// True once cancelled.
    pub fn is_cancelled(&self) -> bool {
        *self.rx.borrow()
    }

    /// Resolve when cancelled.
    pub async fn cancelled(&self) {
        let mut rx = self.rx.clone();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

/// The acquisition context every connector call receives: deadline,
/// cancellation, account scope, profile identity, freshness and quota.
#[derive(Clone)]
pub struct AcquireCtx {
    /// The absolute deadline, when the caller set one.
    pub deadline: Option<tokio::time::Instant>,
    /// Cooperative cancellation.
    pub cancel: Cancellation,
    /// The account scope of this call.
    pub account_scope: Option<AccountScope>,
    /// The browser/profile identity for this source, when one exists.
    pub profile: Option<ProfileIdentity>,
    /// The requested freshness.
    pub freshness: FreshnessMode,
    /// The quota snapshot the planner handed in.
    pub quota: Option<QuotaState>,
}

impl Default for AcquireCtx {
    fn default() -> Self {
        Self::new()
    }
}

impl AcquireCtx {
    /// A context with no deadline and no account scope.
    pub fn new() -> Self {
        Self {
            deadline: None,
            cancel: Cancellation::new(),
            account_scope: None,
            profile: None,
            freshness: FreshnessMode::PreferCache,
            quota: None,
        }
    }

    /// Set a relative deadline budget.
    pub fn with_deadline(mut self, budget: Duration) -> Self {
        self.deadline = Some(tokio::time::Instant::now() + budget);
        self
    }

    /// Set an absolute deadline.
    pub fn with_deadline_at(mut self, deadline: tokio::time::Instant) -> Self {
        self.deadline = Some(deadline);
        self
    }

    /// Set the account scope.
    pub fn with_account(mut self, scope: AccountScope) -> Self {
        self.account_scope = Some(scope);
        self
    }

    /// Set the profile identity.
    pub fn with_profile(mut self, profile: ProfileIdentity) -> Self {
        self.profile = Some(profile);
        self
    }

    /// Set the requested freshness.
    pub fn with_freshness(mut self, freshness: FreshnessMode) -> Self {
        self.freshness = freshness;
        self
    }

    /// The remaining budget, when a deadline is set.
    pub fn remaining(&self) -> Option<Duration> {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(tokio::time::Instant::now()))
    }

    /// Fail typed when the call is already cancelled or past its deadline.
    pub fn check(&self) -> Result<(), SourceError> {
        if self.cancel.is_cancelled() {
            return Err(SourceError::Cancelled);
        }
        match self.deadline {
            Some(deadline) if tokio::time::Instant::now() >= deadline => Err(SourceError::Deadline),
            _ => Ok(()),
        }
    }

    /// Resolve when the call is cancelled or its deadline expires. Returns
    /// `Ok(())` when the caller may continue.
    pub async fn aborted(&self) -> Result<(), SourceError> {
        self.check()?;
        match self.deadline {
            None => {
                self.cancel.cancelled().await;
                Err(SourceError::Cancelled)
            }
            Some(deadline) => tokio::select! {
                _ = self.cancel.cancelled() => Err(SourceError::Cancelled),
                _ = tokio::time::sleep_until(deadline) => Err(SourceError::Deadline),
            },
        }
    }

    /// Run one future under the caller's deadline and cancellation. The
    /// future is dropped when the budget expires.
    pub async fn run_bounded<F, T>(&self, future: F) -> Result<T, SourceError>
    where
        F: std::future::Future<Output = Result<T, SourceError>>,
    {
        match self.deadline {
            None => tokio::select! {
                result = future => result,
                _ = self.cancel.cancelled() => Err(SourceError::Cancelled),
            },
            Some(deadline) => tokio::select! {
                result = future => result,
                _ = self.cancel.cancelled() => Err(SourceError::Cancelled),
                _ = tokio::time::sleep_until(deadline) => Err(SourceError::Deadline),
            },
        }
    }
}

/// How a connector may acquire data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcquisitionMechanism {
    /// An official marketplace API.
    OfficialApi,
    /// A first-party JSON/HTTP response.
    DirectHttp,
    /// A captured browser network response.
    BrowserNetwork,
    /// Embedded page/application state.
    EmbeddedState,
    /// Semantic DOM extraction.
    Dom,
}

impl AcquisitionMechanism {
    /// The stable wire label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OfficialApi => "official_api",
            Self::DirectHttp => "direct_http",
            Self::BrowserNetwork => "browser_network",
            Self::EmbeddedState => "embedded_state",
            Self::Dom => "dom",
        }
    }
}

/// The capabilities a connector advertises (`docs/acquire.md` §6).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorCapabilities {
    /// Discovery/search.
    pub discovery: bool,
    /// Exact product inspection.
    pub exact_product: bool,
    /// Quantity-dependent pricing.
    pub quantity_pricing: bool,
    /// Stock.
    pub stock: bool,
    /// Packaging data.
    pub packaging: bool,
    /// Account-specific pricing.
    pub account_pricing: bool,
    /// Supplier data.
    pub supplier_data: bool,
    /// Bulk/BOM acquisition.
    pub bulk: bool,
    /// The mechanisms this connector can use.
    pub mechanisms: Vec<AcquisitionMechanism>,
}

/// One selectable capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    /// Discovery/search.
    Discovery,
    /// Exact product inspection.
    ExactProduct,
    /// Quantity-dependent pricing.
    QuantityPricing,
    /// Stock.
    Stock,
    /// Packaging data.
    Packaging,
    /// Account-specific pricing.
    AccountPricing,
    /// Supplier data.
    SupplierData,
    /// Bulk/BOM acquisition.
    Bulk,
}

impl ConnectorCapabilities {
    /// Whether one capability is advertised.
    pub const fn has(&self, capability: Capability) -> bool {
        match capability {
            Capability::Discovery => self.discovery,
            Capability::ExactProduct => self.exact_product,
            Capability::QuantityPricing => self.quantity_pricing,
            Capability::Stock => self.stock,
            Capability::Packaging => self.packaging,
            Capability::AccountPricing => self.account_pricing,
            Capability::SupplierData => self.supplier_data,
            Capability::Bulk => self.bulk,
        }
    }

    /// The advertised capability labels (deterministic order).
    pub fn labels(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        for (capability, label) in [
            (Capability::Discovery, "discovery"),
            (Capability::ExactProduct, "exact_product"),
            (Capability::QuantityPricing, "quantity_pricing"),
            (Capability::Stock, "stock"),
            (Capability::Packaging, "packaging"),
            (Capability::AccountPricing, "account_pricing"),
            (Capability::SupplierData, "supplier_data"),
            (Capability::Bulk, "bulk"),
        ] {
            if self.has(capability) {
                out.push(label);
            }
        }
        out
    }

    /// Refuse a malformed advertisement (duplicate mechanisms, empty
    /// mechanism list).
    pub fn validate(&self, source: &SourceId) -> Result<(), RegistryError> {
        if self.mechanisms.is_empty() {
            return Err(RegistryError::InvalidCapabilities {
                source_id: source.clone(),
                reason: "no acquisition mechanism advertised".to_string(),
            });
        }
        let mut seen: Vec<AcquisitionMechanism> = Vec::new();
        for mechanism in &self.mechanisms {
            if seen.contains(mechanism) {
                return Err(RegistryError::InvalidCapabilities {
                    source_id: source.clone(),
                    reason: format!("duplicate mechanism {}", mechanism.as_str()),
                });
            }
            seen.push(*mechanism);
        }
        Ok(())
    }
}

/// One discovery result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Discovery {
    /// The source.
    pub source: SourceId,
    /// The product identity.
    pub identity: ProductIdentity,
    /// The listing title.
    pub title: Text<512>,
    /// The canonical URL, when known.
    pub url: Option<CanonicalUrl>,
    /// The headline price range (never a quote).
    pub price_range: Option<PriceRange>,
    /// The stock state.
    pub stock: StockState,
    /// The supplier, when known.
    pub supplier: Option<Supplier>,
    /// The packaging, when known.
    pub packaging: Option<PackagingType>,
    /// The MOQ, when known.
    pub moq: Option<NonZeroQuantity>,
    /// When the observation was made.
    pub observed_at_ms: u64,
    /// The provenance of the observation.
    pub provenance: OfferProvenance,
}

/// One quote candidate: a normalized offer plus its deterministic
/// resolution at the requested quantity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuoteCandidate {
    /// The normalized offer.
    pub offer: CommercialOffer,
    /// The deterministic quote resolution.
    pub resolution: QuoteResolution,
    /// How fresh the underlying observation is.
    pub freshness: crate::offer::Freshness,
}

/// HTTP method of a transport request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HttpMethod {
    /// GET.
    Get,
    /// POST.
    Post,
}

/// Hard bound of a transport request/response body.
pub const MAX_TRANSPORT_BODY_BYTES: usize = 256 * 1024;
/// Hard bound of a browser capture payload.
pub const MAX_BROWSER_CAPTURE_BYTES: usize = 2 * 1024 * 1024;

/// A bounded byte body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportBody(Vec<u8>);

impl TransportBody {
    /// Validate and construct.
    pub fn new(bytes: Vec<u8>) -> Result<Self, SourceError> {
        if bytes.len() > MAX_TRANSPORT_BODY_BYTES {
            return Err(SourceError::ResponseTooLarge);
        }
        Ok(Self(bytes))
    }

    /// Take a bounded prefix of a body.
    pub fn bounded(bytes: Vec<u8>, cap: usize) -> Self {
        let cap = cap.min(MAX_TRANSPORT_BODY_BYTES);
        if bytes.len() <= cap {
            Self(bytes)
        } else {
            Self(bytes[..cap].to_vec())
        }
    }

    /// The bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// The length.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// True when empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The bytes as lossy UTF-8.
    pub fn as_text(&self) -> String {
        String::from_utf8_lossy(&self.0).into_owned()
    }
}

/// One transport request. Credentials never appear here: the transport
/// resolves authentication from its own registered secret store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportRequest {
    /// The HTTP method.
    pub method: HttpMethod,
    /// The validated destination URL.
    pub url: CanonicalUrl,
    /// The accepted content type, when the connector cares.
    pub accept: Option<Text<64>>,
    /// Conditional validators.
    pub conditional: ConditionalValidators,
    /// The request body, when any.
    pub body: Option<TransportBody>,
    /// The maximum response body the caller will accept.
    pub max_response_bytes: u32,
}

impl TransportRequest {
    /// A bounded GET with the default response bound.
    pub fn get(url: CanonicalUrl) -> Self {
        Self {
            method: HttpMethod::Get,
            url,
            accept: None,
            conditional: ConditionalValidators::default(),
            body: None,
            max_response_bytes: MAX_TRANSPORT_BODY_BYTES as u32,
        }
    }

    /// Set a bounded request body.
    pub fn with_body(mut self, body: TransportBody) -> Self {
        self.body = Some(body);
        self
    }

    /// Set conditional validators.
    pub fn with_conditional(mut self, conditional: ConditionalValidators) -> Self {
        self.conditional = conditional;
        self
    }
}

/// One transport response with an allowlisted header surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportResponse {
    /// The HTTP status.
    pub status: u16,
    /// The content type.
    pub content_type: Option<Text<64>>,
    /// The ETag.
    pub etag: Option<Text<128>>,
    /// The Last-Modified timestamp.
    pub last_modified_ms: Option<u64>,
    /// The Retry-After hint.
    pub retry_after_ms: Option<u64>,
    /// The bounded response body.
    pub body: TransportBody,
    /// The number of redirects followed.
    pub redirects: u8,
    /// True when the body was truncated at the bound.
    pub truncated: bool,
}

impl TransportResponse {
    /// 304 Not Modified: no parse, no new snapshot.
    pub fn is_not_modified(&self) -> bool {
        self.status == 304
    }

    /// 2xx.
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The conditional validators carried by this response.
    pub fn validators(&self) -> ConditionalValidators {
        ConditionalValidators {
            etag: self.etag.as_ref().map(|etag| etag.as_str().to_string()),
            last_modified_ms: self.last_modified_ms,
        }
    }
}

/// The checked egress authority seam. Implementations live outside this
/// crate (the acquire engine); connectors only ever see this trait.
#[async_trait::async_trait]
pub trait HttpTransport: Send + Sync {
    /// Execute one bounded request through the checked egress authority.
    async fn execute(
        &self,
        ctx: &AcquireCtx,
        request: TransportRequest,
    ) -> Result<TransportResponse, SourceError>;
}

/// The extraction priority a browser capture should follow (§9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionPriority {
    /// XHR/fetch JSON first.
    NetworkJson,
    /// Embedded application state.
    EmbeddedState,
    /// Structured markup.
    StructuredMarkup,
    /// Semantic DOM.
    SemanticDom,
    /// Rendered text last.
    RenderedText,
}

/// One bounded, sanitized browser capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserCapture {
    /// The captured URL.
    pub url: CanonicalUrl,
    /// The bounded payload (already sanitized by the browser authority).
    pub payload: TransportBody,
    /// Where the payload came from.
    pub origin: crate::offer::ObservationOrigin,
    /// The strategy that produced it.
    pub strategy: ExtractionPriority,
    /// When the capture was made.
    pub observed_at_ms: u64,
}

/// The browser authority seam. Connectors never launch Chromium themselves.
#[async_trait::async_trait]
pub trait BrowserAuthority: Send + Sync {
    /// Whether the browser authority can serve a capture right now.
    fn available(&self) -> bool;
    /// Capture one URL under a profile and extraction priority, bounded by
    /// the caller's context.
    async fn capture(
        &self,
        ctx: &AcquireCtx,
        profile: &ProfileIdentity,
        url: &CanonicalUrl,
        priority: ExtractionPriority,
    ) -> Result<BrowserCapture, SourceError>;
}

/// The connector contract (`docs/acquire.md` §10).
#[async_trait::async_trait]
pub trait CommerceConnector: Send + Sync {
    /// The source this connector serves.
    fn source(&self) -> SourceId;
    /// The advertised capabilities.
    fn capabilities(&self) -> ConnectorCapabilities;
    /// Discovery.
    async fn discover(
        &self,
        ctx: &AcquireCtx,
        req: SearchRequest,
    ) -> Result<Vec<Discovery>, SourceError>;
    /// Inspect one known reference.
    async fn product(
        &self,
        ctx: &AcquireCtx,
        req: ProductRequest,
    ) -> Result<CommercialOffer, SourceError>;
    /// Quote candidates at a quantity.
    async fn quote(
        &self,
        ctx: &AcquireCtx,
        req: QuoteRequest,
    ) -> Result<Vec<QuoteCandidate>, SourceError>;
}

/// Which acquisition path a call takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AcquisitionPath {
    /// The official/API path.
    Api,
    /// The browser-fallback path.
    Browser,
}

impl AcquisitionPath {
    /// The stable label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Api => "api",
            Self::Browser => "browser",
        }
    }
}

/// Per-source acquisition policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectorPolicy {
    /// Whether the API path is enabled.
    pub api_enabled: bool,
    /// Whether the browser fallback is enabled.
    pub browser_enabled: bool,
    /// Consecutive transient failures before the path's breaker opens.
    pub max_consecutive_failures: u32,
    /// How long a breaker stays open after tripping.
    pub cooldown_ms: u64,
    /// The source's first-party destination allowlist (docs/acquire.md §10):
    /// one strict `scheme://host[:port]` rule per admitted destination. The
    /// daemon parses these rules into the shared parsed-destination policy
    /// installed on the checked transport every connector request executes
    /// through, so scheme and (when present) port are part of the rule —
    /// there is no scheme/port widening. The DEFAULT is the empty slice: a
    /// policy that declares no destination admits NOTHING (fail closed,
    /// never a permissive default).
    pub destinations: &'static [&'static str],
}

impl Default for ConnectorPolicy {
    fn default() -> Self {
        Self {
            api_enabled: true,
            browser_enabled: true,
            max_consecutive_failures: 3,
            cooldown_ms: 60_000,
            destinations: &[],
        }
    }
}

/// A registry refusal.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    /// A connector for this source already exists.
    #[error("connector for source {0} is already registered")]
    DuplicateSource(SourceId),
    /// The advertised capabilities are malformed.
    #[error("capabilities for source {source_id} are invalid: {reason}")]
    InvalidCapabilities {
        /// The source.
        source_id: SourceId,
        /// Why.
        reason: String,
    },
    /// The source is not registered.
    #[error("no connector is registered for source {0}")]
    UnknownSource(SourceId),
}

impl From<RegistryError> for SourceError {
    fn from(error: RegistryError) -> Self {
        tracing::debug!("commerce connector registry refused: {error}");
        SourceError::InvalidRequest
    }
}

/// One path's independent circuit breaker.
#[derive(Debug, Clone, Default)]
struct Breaker {
    failures: u32,
    open_until_ms: Option<u64>,
    last_error: Option<SourceError>,
}

impl Breaker {
    fn open_until(&self, now_ms: u64) -> Option<u64> {
        match self.open_until_ms {
            Some(until) if until > now_ms => Some(until),
            Some(_) => None,
            None => None,
        }
    }

    fn note_success(&mut self) {
        self.failures = 0;
        self.open_until_ms = None;
        self.last_error = None;
    }

    fn note_failure(&mut self, error: &SourceError, policy: &ConnectorPolicy, now_ms: u64) {
        self.last_error = Some(error.clone());
        match error {
            SourceError::RateLimited { retry_after_ms } => {
                self.open_until_ms = Some(now_ms.saturating_add(*retry_after_ms));
            }
            SourceError::QuotaExhausted { reset_ms } => {
                self.open_until_ms = Some(now_ms.saturating_add(*reset_ms));
            }
            SourceError::CoolingDown { until_ms } => {
                self.open_until_ms = Some(*until_ms);
            }
            SourceError::AuthenticationRequired
            | SourceError::VerificationRequired { .. }
            | SourceError::Disabled
            | SourceError::InvalidRequest
            | SourceError::ProductNotFound
            | SourceError::VariantAmbiguous
            | SourceError::Cancelled
            | SourceError::Deadline => {}
            _ => {
                self.failures = self.failures.saturating_add(1);
                if self.failures >= policy.max_consecutive_failures {
                    self.open_until_ms = Some(now_ms.saturating_add(policy.cooldown_ms));
                }
            }
        }
    }
}

#[derive(Default)]
struct RegistryState {
    health: BTreeMap<SourceId, ConnectorHealth>,
    last_error: BTreeMap<SourceId, SourceError>,
    api: BTreeMap<SourceId, Breaker>,
    browser: BTreeMap<SourceId, Breaker>,
}

/// The connector registry: connectors by source, capability advertisement,
/// per-source policy and independent API/browser circuit breakers.
pub struct ConnectorRegistry {
    connectors: BTreeMap<SourceId, Arc<dyn CommerceConnector>>,
    policies: BTreeMap<SourceId, ConnectorPolicy>,
    state: Mutex<RegistryState>,
}

impl Default for ConnectorRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectorRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self {
            connectors: BTreeMap::new(),
            policies: BTreeMap::new(),
            state: Mutex::new(RegistryState::default()),
        }
    }

    /// Register a connector with the default policy. A duplicate source is a
    /// typed refusal.
    pub fn register(&mut self, connector: Arc<dyn CommerceConnector>) -> Result<(), RegistryError> {
        self.register_with_policy(connector, ConnectorPolicy::default())
    }

    /// Register one boxed connector with the default policy (the connectors
    /// crate hands its bridged `Box<dyn CommerceConnector>` in here).
    pub fn register_boxed(
        &mut self,
        connector: Box<dyn CommerceConnector>,
    ) -> Result<(), RegistryError> {
        self.register(Arc::from(connector))
    }

    /// Register one boxed connector with an explicit policy.
    pub fn register_boxed_with_policy(
        &mut self,
        connector: Box<dyn CommerceConnector>,
        policy: ConnectorPolicy,
    ) -> Result<(), RegistryError> {
        self.register_with_policy(Arc::from(connector), policy)
    }

    /// Register a connector with an explicit policy.
    pub fn register_with_policy(
        &mut self,
        connector: Arc<dyn CommerceConnector>,
        policy: ConnectorPolicy,
    ) -> Result<(), RegistryError> {
        let source = connector.source();
        connector.capabilities().validate(&source)?;
        if self.connectors.contains_key(&source) {
            return Err(RegistryError::DuplicateSource(source));
        }
        self.connectors.insert(source.clone(), connector);
        self.policies.insert(source, policy);
        Ok(())
    }

    /// The registered sources, sorted.
    pub fn sources(&self) -> Vec<SourceId> {
        self.connectors.keys().cloned().collect()
    }

    /// The connector for a source.
    pub fn connector(&self, source: &SourceId) -> Option<Arc<dyn CommerceConnector>> {
        self.connectors.get(source).cloned()
    }

    /// The advertised capabilities for a source.
    pub fn capabilities(&self, source: &SourceId) -> Option<ConnectorCapabilities> {
        self.connectors.get(source).map(|c| c.capabilities())
    }

    /// The policy for a source.
    pub fn policy(&self, source: &SourceId) -> Option<ConnectorPolicy> {
        self.policies.get(source).copied()
    }

    /// Sources that advertise `capability`, are policy-enabled for the API
    /// path and are not currently broken.
    pub fn capable_sources(&self, capability: Capability, now_ms: u64) -> Vec<SourceId> {
        let mut out = Vec::new();
        for (source, connector) in &self.connectors {
            let policy = self.policies.get(source).copied().unwrap_or_default();
            if !policy.api_enabled {
                continue;
            }
            if !connector.capabilities().has(capability) {
                continue;
            }
            if self
                .path_allowed(source, AcquisitionPath::Api, now_ms)
                .is_err()
            {
                continue;
            }
            out.push(source.clone());
        }
        out
    }

    /// Resolve an explicit source set against the registry: `auto` selects
    /// every capable, available source; an explicit set keeps the requested
    /// order and refuses unknown or incapable sources.
    pub fn resolve(
        &self,
        requested: &SourceSet,
        capability: Capability,
        now_ms: u64,
    ) -> Result<Vec<SourceId>, SourceError> {
        if requested.is_auto() {
            return Ok(self.capable_sources(capability, now_ms));
        }
        let mut out = Vec::new();
        for source in requested.as_slice() {
            let connector = self
                .connectors
                .get(source)
                .ok_or_else(|| SourceError::from(RegistryError::UnknownSource(source.clone())))?;
            if !connector.capabilities().has(capability) {
                return Err(SourceError::InvalidRequest);
            }
            out.push(source.clone());
        }
        Ok(out)
    }

    /// The planner-facing health of a source.
    pub fn health(&self, source: &SourceId, now_ms: u64) -> ConnectorHealth {
        let Ok(state) = self.state.lock() else {
            return ConnectorHealth::Unavailable;
        };
        if let Some(until) = state.api.get(source).and_then(|b| b.open_until(now_ms)) {
            return ConnectorHealth::CoolingDown { until_ms: until };
        }
        state
            .health
            .get(source)
            .cloned()
            .unwrap_or(ConnectorHealth::Healthy)
    }

    /// The last failure recorded for a source, if any.
    pub fn last_error(&self, source: &SourceId) -> Option<SourceError> {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.last_error.get(source).cloned())
    }

    /// Record a success on one path: that path's breaker closes.
    pub fn note_success(&self, source: &SourceId, path: AcquisitionPath) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        match path {
            AcquisitionPath::Api => {
                if let Some(breaker) = state.api.get_mut(source) {
                    breaker.note_success();
                }
                state
                    .health
                    .insert(source.clone(), ConnectorHealth::Healthy);
            }
            AcquisitionPath::Browser => {
                if let Some(breaker) = state.browser.get_mut(source) {
                    breaker.note_success();
                }
                if !state.health.contains_key(source) {
                    state
                        .health
                        .insert(source.clone(), ConnectorHealth::Healthy);
                }
            }
        }
        state.last_error.remove(source);
    }

    /// Record a failure on one path: only that path's breaker moves, and the
    /// planner-facing health follows the error.
    pub fn note_failure(
        &self,
        source: &SourceId,
        path: AcquisitionPath,
        error: &SourceError,
        now_ms: u64,
    ) {
        let policy = self.policies.get(source).copied().unwrap_or_default();
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let breaker = match path {
            AcquisitionPath::Api => state.api.entry(source.clone()).or_default(),
            AcquisitionPath::Browser => state.browser.entry(source.clone()).or_default(),
        };
        breaker.note_failure(error, &policy, now_ms);
        state
            .health
            .insert(source.clone(), error.clone().into_health());
        state.last_error.insert(source.clone(), error.clone());
    }

    /// May this path be used right now?
    pub fn path_allowed(
        &self,
        source: &SourceId,
        path: AcquisitionPath,
        now_ms: u64,
    ) -> Result<(), SourceError> {
        let Some(policy) = self.policies.get(source).copied() else {
            return Err(SourceError::from(RegistryError::UnknownSource(
                source.clone(),
            )));
        };
        match path {
            AcquisitionPath::Api if !policy.api_enabled => return Err(SourceError::Disabled),
            AcquisitionPath::Browser if !policy.browser_enabled => {
                return Err(SourceError::BrowserUnavailable)
            }
            _ => {}
        }
        let Ok(state) = self.state.lock() else {
            return Err(SourceError::Store);
        };
        let breaker = match path {
            AcquisitionPath::Api => state.api.get(source),
            AcquisitionPath::Browser => state.browser.get(source),
        };
        if let Some(until) = breaker.and_then(|b| b.open_until(now_ms)) {
            return Err(SourceError::CoolingDown { until_ms: until });
        }
        match state.health.get(source) {
            Some(ConnectorHealth::AuthenticationRequired) => {
                return Err(SourceError::AuthenticationRequired)
            }
            Some(ConnectorHealth::VerificationRequired { .. }) => {
                return Err(SourceError::VerificationRequired {
                    kind: VerificationKind::Unknown,
                })
            }
            Some(ConnectorHealth::QuotaExhausted { reset_ms }) => {
                return Err(SourceError::QuotaExhausted {
                    reset_ms: *reset_ms,
                })
            }
            _ => {}
        }
        Ok(())
    }

    /// Browser fallback is legitimate only for availability/resilience
    /// failures. A rate-limited, quota-exhausted, unauthenticated or
    /// verification-blocked API is never bypassed with the browser — that
    /// would be quota evasion (`docs/acquire.md` §10).
    pub fn browser_fallback_allowed(
        &self,
        source: &SourceId,
        last_api_error: &SourceError,
        now_ms: u64,
    ) -> bool {
        match last_api_error {
            SourceError::RateLimited { .. }
            | SourceError::QuotaExhausted { .. }
            | SourceError::CoolingDown { .. }
            | SourceError::AuthenticationRequired
            | SourceError::VerificationRequired { .. }
            | SourceError::Disabled => return false,
            _ => {}
        }
        self.path_allowed(source, AcquisitionPath::Browser, now_ms)
            .is_ok()
    }

    /// The independent API and browser breaker states for one source
    /// (`(open_until_api, open_until_browser)`), for tests and diagnostics.
    pub fn breaker_state(&self, source: &SourceId, now_ms: u64) -> (Option<u64>, Option<u64>) {
        let Ok(state) = self.state.lock() else {
            return (None, None);
        };
        (
            state.api.get(source).and_then(|b| b.open_until(now_ms)),
            state.browser.get(source).and_then(|b| b.open_until(now_ms)),
        )
    }

    /// Every source's health, sorted by source.
    pub fn health_snapshot(&self, now_ms: u64) -> Vec<(SourceId, ConnectorHealth)> {
        self.sources()
            .into_iter()
            .map(|source| {
                let health = self.health(&source, now_ms);
                (source, health)
            })
            .collect()
    }
}

/// A tiny in-memory transport for tests and for connectors that need no
/// network at all. Refuses every request typed; never performs I/O.
pub struct UnavailableTransport;

#[async_trait::async_trait]
impl HttpTransport for UnavailableTransport {
    async fn execute(
        &self,
        _ctx: &AcquireCtx,
        _request: TransportRequest,
    ) -> Result<TransportResponse, SourceError> {
        Err(SourceError::EgressUnavailable)
    }
}

/// A browser authority that is never available (disabled parity).
pub struct UnavailableBrowser;

#[async_trait::async_trait]
impl BrowserAuthority for UnavailableBrowser {
    fn available(&self) -> bool {
        false
    }

    async fn capture(
        &self,
        _ctx: &AcquireCtx,
        _profile: &ProfileIdentity,
        _url: &CanonicalUrl,
        _priority: ExtractionPriority,
    ) -> Result<BrowserCapture, SourceError> {
        Err(SourceError::BrowserUnavailable)
    }
}

/// True when this error justifies a browser fallback (the inverse of the
/// quota-evasion guard, exposed for planning).
pub fn browser_fallback_justified(error: &SourceError) -> bool {
    !matches!(
        error,
        SourceError::RateLimited { .. }
            | SourceError::QuotaExhausted { .. }
            | SourceError::CoolingDown { .. }
            | SourceError::AuthenticationRequired
            | SourceError::VerificationRequired { .. }
            | SourceError::Disabled
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(id: &str) -> SourceId {
        SourceId::new(id).expect("source")
    }

    struct FakeConnector {
        source: SourceId,
        capabilities: ConnectorCapabilities,
    }

    #[async_trait::async_trait]
    impl CommerceConnector for FakeConnector {
        fn source(&self) -> SourceId {
            self.source.clone()
        }

        fn capabilities(&self) -> ConnectorCapabilities {
            self.capabilities.clone()
        }

        async fn discover(
            &self,
            _ctx: &AcquireCtx,
            _req: SearchRequest,
        ) -> Result<Vec<Discovery>, SourceError> {
            Ok(Vec::new())
        }

        async fn product(
            &self,
            _ctx: &AcquireCtx,
            _req: ProductRequest,
        ) -> Result<CommercialOffer, SourceError> {
            Err(SourceError::ProductNotFound)
        }

        async fn quote(
            &self,
            _ctx: &AcquireCtx,
            _req: QuoteRequest,
        ) -> Result<Vec<QuoteCandidate>, SourceError> {
            Ok(Vec::new())
        }
    }

    fn fake(id: &str, capabilities: ConnectorCapabilities) -> Arc<dyn CommerceConnector> {
        Arc::new(FakeConnector {
            source: source(id),
            capabilities,
        })
    }

    fn full_capabilities() -> ConnectorCapabilities {
        ConnectorCapabilities {
            discovery: true,
            exact_product: true,
            quantity_pricing: true,
            stock: true,
            packaging: true,
            account_pricing: true,
            supplier_data: true,
            bulk: true,
            mechanisms: vec![
                AcquisitionMechanism::OfficialApi,
                AcquisitionMechanism::DirectHttp,
            ],
        }
    }

    #[test]
    fn duplicate_sources_and_malformed_capabilities_are_refused() {
        let mut registry = ConnectorRegistry::new();
        registry
            .register(fake("mouser", full_capabilities()))
            .expect("first registration");
        let error = registry
            .register(fake("mouser", full_capabilities()))
            .expect_err("duplicate refused");
        assert_eq!(error, RegistryError::DuplicateSource(source("mouser")));

        let mut malformed = full_capabilities();
        malformed.mechanisms.clear();
        assert!(malformed.validate(&source("x")).is_err());
        let mut duplicated = full_capabilities();
        duplicated
            .mechanisms
            .push(AcquisitionMechanism::OfficialApi);
        assert!(duplicated.validate(&source("x")).is_err());

        assert_eq!(registry.sources(), vec![source("mouser")]);
        assert!(registry.capabilities(&source("mouser")).is_some());
        assert_eq!(
            registry.capabilities(&source("mouser")).unwrap().labels(),
            vec![
                "discovery",
                "exact_product",
                "quantity_pricing",
                "stock",
                "packaging",
                "account_pricing",
                "supplier_data",
                "bulk"
            ]
        );
    }

    #[test]
    fn api_and_browser_breakers_are_independent() {
        let mut registry = ConnectorRegistry::new();
        registry
            .register(fake("lcsc", full_capabilities()))
            .expect("registration");
        let source = source("lcsc");
        let now = 1_000_000u64;

        for _ in 0..3 {
            registry.note_failure(
                &source,
                AcquisitionPath::Api,
                &SourceError::NetworkTimeout,
                now,
            );
        }
        let (api_open, browser_open) = registry.breaker_state(&source, now);
        assert!(api_open.is_some(), "api breaker opened");
        assert!(browser_open.is_none(), "browser breaker untouched");
        assert!(matches!(
            registry.path_allowed(&source, AcquisitionPath::Api, now),
            Err(SourceError::CoolingDown { .. })
        ));
        assert!(registry
            .path_allowed(&source, AcquisitionPath::Browser, now)
            .is_ok());
        assert!(matches!(
            registry.health(&source, now),
            ConnectorHealth::CoolingDown { .. }
        ));

        // The api breaker closes on success, independently.
        registry.note_success(&source, AcquisitionPath::Api);
        assert!(registry
            .path_allowed(&source, AcquisitionPath::Api, now)
            .is_ok());
    }

    #[test]
    fn browser_fallback_never_evades_quota_or_rate_limits() {
        let mut registry = ConnectorRegistry::new();
        registry
            .register(fake("1688", full_capabilities()))
            .expect("registration");
        let source = source("1688");
        let now = 5_000u64;

        assert!(!registry.browser_fallback_allowed(
            &source,
            &SourceError::RateLimited {
                retry_after_ms: 1_000
            },
            now
        ));
        assert!(!registry.browser_fallback_allowed(
            &source,
            &SourceError::QuotaExhausted { reset_ms: 1_000 },
            now
        ));
        assert!(!registry.browser_fallback_allowed(
            &source,
            &SourceError::AuthenticationRequired,
            now
        ));
        assert!(!registry.browser_fallback_allowed(
            &source,
            &SourceError::VerificationRequired {
                kind: VerificationKind::Captcha
            },
            now
        ));
        assert!(registry.browser_fallback_allowed(&source, &SourceError::ApiUnavailable, now));
        assert!(registry.browser_fallback_allowed(&source, &SourceError::NetworkTimeout, now));
        assert!(browser_fallback_justified(&SourceError::ApiUnavailable));
        assert!(!browser_fallback_justified(&SourceError::QuotaExhausted {
            reset_ms: 1
        }));

        // A policy that disables the browser path refuses fallback too.
        let mut registry = ConnectorRegistry::new();
        registry
            .register_with_policy(
                fake("1688", full_capabilities()),
                ConnectorPolicy {
                    browser_enabled: false,
                    ..ConnectorPolicy::default()
                },
            )
            .expect("registration");
        assert!(!registry.browser_fallback_allowed(&source, &SourceError::ApiUnavailable, now));
    }

    #[test]
    fn auto_selection_skips_broken_and_incapable_sources() {
        let mut registry = ConnectorRegistry::new();
        registry
            .register(fake("mouser", full_capabilities()))
            .expect("registration");
        let mut discovery_only = ConnectorCapabilities {
            discovery: true,
            ..ConnectorCapabilities::default()
        };
        discovery_only.mechanisms = vec![AcquisitionMechanism::OfficialApi];
        registry
            .register(fake("digikey", discovery_only))
            .expect("registration");
        let now = 10_000u64;

        assert_eq!(
            registry.resolve(&SourceSet::auto(), Capability::QuantityPricing, now),
            Ok(vec![source("mouser")])
        );
        assert_eq!(
            registry.resolve(&SourceSet::auto(), Capability::Discovery, now),
            Ok(vec![source("digikey"), source("mouser")])
        );
        let explicit = SourceSet::named(vec![source("digikey")]).expect("source set");
        assert_eq!(
            registry.resolve(&explicit, Capability::QuantityPricing, now),
            Err(SourceError::InvalidRequest)
        );
        let unknown = SourceSet::named(vec![source("nope")]).expect("source set");
        assert!(registry
            .resolve(&unknown, Capability::Discovery, now)
            .is_err());

        for _ in 0..3 {
            registry.note_failure(
                &source("mouser"),
                AcquisitionPath::Api,
                &SourceError::ApiUnavailable,
                now,
            );
        }
        assert_eq!(
            registry.resolve(&SourceSet::auto(), Capability::Discovery, now),
            Ok(vec![source("digikey")])
        );
    }

    #[tokio::test(start_paused = true)]
    async fn deadline_and_cancellation_propagate_through_the_context() {
        let ctx = AcquireCtx::new().with_deadline(Duration::from_millis(100));
        assert!(ctx.remaining().is_some());
        assert!(ctx.check().is_ok());
        let aborted = ctx
            .run_bounded(async {
                tokio::time::sleep(Duration::from_secs(10)).await;
                Ok(())
            })
            .await;
        assert_eq!(aborted, Err(SourceError::Deadline));

        let ctx = AcquireCtx::new();
        let cancel = ctx.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel.cancel();
        });
        let aborted = ctx
            .run_bounded(async {
                tokio::time::sleep(Duration::from_secs(10)).await;
                Ok(())
            })
            .await;
        assert_eq!(aborted, Err(SourceError::Cancelled));
    }

    #[test]
    fn transport_bodies_are_bounded() {
        assert!(TransportBody::new(vec![0u8; MAX_TRANSPORT_BODY_BYTES]).is_ok());
        assert!(matches!(
            TransportBody::new(vec![0u8; MAX_TRANSPORT_BODY_BYTES + 1]),
            Err(SourceError::ResponseTooLarge)
        ));
        let bounded = TransportBody::bounded(vec![0u8; 64], 8);
        assert_eq!(bounded.len(), 8);
        assert!(TransportResponse {
            status: 304,
            content_type: None,
            etag: Some(Text::<128>::new("\"abc\"").expect("etag")),
            last_modified_ms: None,
            retry_after_ms: None,
            body: TransportBody::new(Vec::new()).expect("empty"),
            redirects: 0,
            truncated: false,
        }
        .is_not_modified());
    }
}
