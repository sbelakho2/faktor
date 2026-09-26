//! The bridge between the site adapters and the single registry-facing
//! `faktor_commerce::connector::CommerceConnector`.
//!
//! `faktor-commerce` owns the planner/registry contract; the connectors
//! crate owns the site implementation trait ([`SiteConnector`]) with the
//! acquisition-runtime context (injected transport, quota, secrets,
//! diagnostics, clock and browser). [`Registered`] adapts one onto the
//! other:
//!
//! * the commerce context contributes deadline, cancellation, account scope
//!   and freshness; the injected [`ConnectorRuntime`] contributes the egress
//!   transport, quota, secret guard, diagnostics, clock and browser — so a
//!   site adapter keeps its full extraction logic and still registers as
//!   `Arc<dyn CommerceConnector>` / `Box<dyn CommerceConnector>` in
//!   `faktor-commerce`'s `ConnectorRegistry`;
//! * requests and results are converted once, at the seam: the commerce
//!   `Discovery` shape is the planner vocabulary, the site `Discovery` shape
//!   is the extraction vocabulary (price hint, staleness bound, origin).
//!
//! Conflicts are not resolved here: the site adapter already reconciled its
//! observations (`crate::contract::extract`) before returning.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use faktor_commerce::connector::{
    AcquireCtx as CommerceCtx, AcquisitionMechanism, CommerceConnector,
    ConnectorCapabilities as CommerceCapabilities, Discovery as CommerceDiscovery,
    QuoteCandidate as CommerceQuoteCandidate,
};
use faktor_commerce::offer::PriceRange;
use faktor_commerce::query::{
    FreshnessMode, ProductRef, ProductRequest as CommerceProductRequest,
    QuoteRequest as CommerceQuoteRequest, SearchRequest as CommerceSearchRequest,
};
use faktor_commerce::text::{CanonicalUrl, Text};
use faktor_commerce::{SourceError, SourceId};

use crate::context::{AcquireCtx, Cancellation, Clock, ConnectorIdentity, Diagnostics};
use crate::contract::capture::SharedBrowserExtraction;
use crate::contract::{
    CapabilityLevel, ConnectorCapabilities, Discovery, Mechanism, ProductReference, ProductRequest,
    QuoteCandidate, QuoteRequest, RequestedFreshness, SearchRequest, SiteConnector,
};
use crate::http::HttpTransport;
use crate::quota::QuotaState;
use crate::secrets::SecretGuard;

/// Everything a site adapter needs from the acquisition runtime, injected
/// once. Nothing here creates I/O: every handle is supplied by the daemon.
pub struct ConnectorRuntime {
    /// The checked egress transport.
    pub transport: Arc<dyn HttpTransport>,
    /// The shared quota state.
    pub quota: Arc<QuotaState>,
    /// The shared secret guard.
    pub secrets: Arc<SecretGuard>,
    /// The diagnostics sink.
    pub diagnostics: Arc<dyn Diagnostics>,
    /// The clock.
    pub clock: Arc<dyn Clock>,
    /// The browser-fallback authority, when one is available.
    pub browser: Option<Arc<dyn crate::browser::BrowserFallback>>,
    /// The browser-acquisition authority, when one is available.
    pub browser_extraction: Option<SharedBrowserExtraction>,
    /// The market, when the runtime scopes acquisition to one.
    pub market: Option<Text<32>>,
    /// The locale, when the runtime scopes acquisition to one.
    pub locale: Option<Text<32>>,
    /// The immutable configured connector identity (P0 item 2). The daemon
    /// configuration owns it; the tool/model request can never manufacture
    /// it. When an explicit account scope is configured here it wins over
    /// whatever the commerce request carried.
    pub identity: ConnectorIdentity,
}

impl ConnectorRuntime {
    /// The mandatory seams; everything optional defaults off/absent.
    pub fn new(
        transport: Arc<dyn HttpTransport>,
        quota: Arc<QuotaState>,
        secrets: Arc<SecretGuard>,
        diagnostics: Arc<dyn Diagnostics>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            transport,
            quota,
            secrets,
            diagnostics,
            clock,
            browser: None,
            browser_extraction: None,
            market: None,
            locale: None,
            identity: ConnectorIdentity::anonymous(),
        }
    }

    /// Inject the immutable configured connector identity (P0 item 2). The
    /// registration site resolves it from daemon configuration; the tool
    /// request never supplies it.
    pub fn with_identity(mut self, identity: ConnectorIdentity) -> Self {
        self.identity = identity;
        self
    }

    /// Inject the browser fallback authority.
    pub fn with_browser(mut self, browser: Arc<dyn crate::browser::BrowserFallback>) -> Self {
        self.browser = Some(browser);
        self
    }

    /// Inject the browser acquisition authority.
    pub fn with_browser_extraction(mut self, browser: SharedBrowserExtraction) -> Self {
        self.browser_extraction = Some(browser);
        self
    }

    /// Set the market/locale scope (also folded into the injected identity).
    pub fn with_market(mut self, market: Text<32>, locale: Option<Text<32>>) -> Self {
        self.identity = self.identity.with_market(market.clone());
        if let Some(locale) = &locale {
            self.identity = self.identity.with_locale(locale.clone());
        }
        self.market = Some(market);
        self.locale = locale;
        self
    }
}

/// A site adapter registered under the registry-facing contract.
pub struct Registered<C: SiteConnector + 'static> {
    inner: C,
    runtime: ConnectorRuntime,
}

impl<C: SiteConnector + 'static> Registered<C> {
    /// Adapt `inner` over the injected runtime.
    pub fn new(inner: C, runtime: ConnectorRuntime) -> Self {
        Self { inner, runtime }
    }

    /// The adapted site adapter.
    pub fn inner(&self) -> &C {
        &self.inner
    }

    /// Box the adapter as the registry trait object.
    pub fn boxed(self) -> Box<dyn CommerceConnector> {
        Box::new(self)
    }

    /// Build the site context for one call: the runtime's injected seams plus
    /// the commerce context's deadline, cancellation, account scope and
    /// freshness.
    fn rich_ctx(&self, ctx: &CommerceCtx) -> AcquireCtx {
        let now = self.runtime.clock.now_ms();
        let commerce_cancel = ctx.cancel.clone();
        let mut builder = AcquireCtx::builder(
            self.runtime.transport.clone(),
            self.runtime.quota.clone(),
            self.runtime.secrets.clone(),
        )
        .clock(self.runtime.clock.clone())
        .diagnostics(self.runtime.diagnostics.clone())
        // The caller's cancellation must be observable inside every
        // connector `check_alive()`; the observed token mirrors the
        // commerce token lazily.
        .cancellation(Cancellation::observed(move || {
            commerce_cancel.is_cancelled()
        }));
        if let Some(remaining) = ctx.remaining() {
            builder = builder.deadline_ms(now.saturating_add(duration_ms(remaining)));
        }
        // The runtime planner already chose the mechanism; the connector
        // executes it. Without a choice the adapter refuses rather than
        // selecting one itself.
        if let Some(mechanism) = ctx.mechanism() {
            builder = builder.mechanism(match mechanism {
                AcquisitionMechanism::OfficialApi => Mechanism::OfficialApi,
                AcquisitionMechanism::DirectHttp => Mechanism::DirectHttp,
                AcquisitionMechanism::BrowserNetwork => Mechanism::BrowserNetwork,
                AcquisitionMechanism::EmbeddedState => Mechanism::EmbeddedState,
                AcquisitionMechanism::Dom => Mechanism::Dom,
            });
        }
        if let Some(browser) = &self.runtime.browser {
            builder = builder.browser(browser.clone());
        }
        if let Some(extraction) = &self.runtime.browser_extraction {
            builder = builder.browser_extraction(extraction.clone());
        }
        // The configured identity is the authority (P0 item 2): a configured
        // account scope always wins over whatever the commerce request
        // carried. The request is only a fallback when the daemon
        // configured no account scope at all, which keeps embedded callers
        // working without letting a tool request override configured
        // identity.
        let mut identity = self.runtime.identity.clone();
        if identity.account_scope().is_none() {
            if let Some(account) = &ctx.account_scope {
                identity = identity.with_account_scope(account.clone());
            }
        }
        if identity.market().is_none() {
            if let Some(market) = &self.runtime.market {
                identity = identity.with_market(market.clone());
            }
        }
        if identity.locale().is_none() {
            if let Some(locale) = &self.runtime.locale {
                identity = identity.with_locale(locale.clone());
            }
        }
        builder = builder.identity(identity);
        builder.build()
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// The registry-facing capabilities of a site advertisement.
pub fn registry_capabilities(capabilities: &ConnectorCapabilities) -> CommerceCapabilities {
    let mut mechanisms: Vec<AcquisitionMechanism> = Vec::new();
    for mechanism in &capabilities.mechanisms {
        let converted = match mechanism {
            Mechanism::OfficialApi => AcquisitionMechanism::OfficialApi,
            Mechanism::DirectHttp => AcquisitionMechanism::DirectHttp,
            Mechanism::BrowserNetwork => AcquisitionMechanism::BrowserNetwork,
            Mechanism::EmbeddedState => AcquisitionMechanism::EmbeddedState,
            Mechanism::Dom => AcquisitionMechanism::Dom,
        };
        if !mechanisms.contains(&converted) {
            mechanisms.push(converted);
        }
    }
    let supported = |level: CapabilityLevel| level.is_supported();
    CommerceCapabilities {
        discovery: supported(capabilities.discovery),
        exact_product: supported(capabilities.exact_product),
        quantity_pricing: supported(capabilities.quantity_pricing),
        stock: supported(capabilities.stock),
        packaging: supported(capabilities.packaging),
        account_pricing: supported(capabilities.account_pricing),
        supplier_data: supported(capabilities.supplier_data),
        bulk: supported(capabilities.bulk),
        mechanisms,
    }
}

/// Convert one site discovery into the planner vocabulary.
pub fn registry_discovery(discovery: &Discovery) -> CommerceDiscovery {
    CommerceDiscovery {
        source: discovery.source.clone(),
        identity: discovery.identity.clone(),
        title: discovery.title.clone(),
        url: discovery.url.clone(),
        price_range: discovery
            .price_hint
            .and_then(|hint| PriceRange::new(hint, hint).ok()),
        stock: discovery
            .stock_hint
            .unwrap_or(faktor_commerce::StockState::Unknown),
        supplier: None,
        packaging: None,
        moq: None,
        observed_at_ms: discovery.observed_at_ms,
        provenance: faktor_commerce::offer::OfferProvenance {
            origin: discovery.origin,
            source: discovery.source.clone(),
            extractor_version: None,
            connector_version: None,
            normalization_version: None,
            content_digest: None,
            account_scope: None,
            locale: None,
            market: None,
            source_confidence_bp: None,
        },
    }
}

/// Convert one site quote candidate (the shapes are identical).
pub fn registry_quote(candidate: &QuoteCandidate) -> CommerceQuoteCandidate {
    CommerceQuoteCandidate {
        offer: candidate.offer.clone(),
        resolution: candidate.resolution.clone(),
        freshness: candidate.freshness,
    }
}

fn rich_freshness(freshness: FreshnessMode) -> RequestedFreshness {
    match freshness {
        FreshnessMode::PreferCache => RequestedFreshness::PreferCache,
        FreshnessMode::Live => RequestedFreshness::Live,
        FreshnessMode::CacheOnly => RequestedFreshness::CacheOnly,
    }
}

/// Convert a planner search request into the site vocabulary.
pub fn site_search(req: &CommerceSearchRequest) -> Result<SearchRequest, SourceError> {
    SearchRequest::new(
        Text::<512>::new(req.query.as_str()).map_err(|_| SourceError::InvalidRequest)?,
        u16::from(req.limit),
        None,
    )
    .map(|request| request.with_freshness(rich_freshness(req.freshness)))
}

/// Convert a planner product reference into the site vocabulary.
pub fn site_reference(reference: &ProductRef) -> Result<ProductReference, SourceError> {
    match reference {
        ProductRef::Url { url } => Ok(ProductReference::Url(url.clone())),
        ProductRef::SourceSku { sku, .. } => Ok(ProductReference::SourcePartNumber(
            Text::<256>::new(sku.as_str()).map_err(|_| SourceError::InvalidRequest)?,
        )),
        ProductRef::OfferId { offer_id, .. } => Ok(ProductReference::OfferId(
            Text::<512>::new(offer_id.as_str()).map_err(|_| SourceError::InvalidRequest)?,
        )),
        ProductRef::PartNumber { part_number } => Ok(ProductReference::ManufacturerPartNumber(
            Text::<256>::new(part_number.as_str()).map_err(|_| SourceError::InvalidRequest)?,
        )),
    }
}

/// Convert a planner product request into the site vocabulary.
pub fn site_product(req: &CommerceProductRequest) -> Result<ProductRequest, SourceError> {
    let reference = site_reference(&req.reference)?;
    Ok(ProductRequest::new(reference).with_freshness(rich_freshness(req.freshness)))
}

/// Convert a planner quote request into the site vocabulary.
pub fn site_quote(req: &CommerceQuoteRequest) -> Result<QuoteRequest, SourceError> {
    let reference = site_reference(&req.reference)?;
    let quantity = req.quantity;
    let mut request = QuoteRequest::new(reference, quantity);
    if let Some(variant) = &req.variant {
        request = request.with_variant(variant.clone());
    }
    if let Some(packaging) = req.packaging {
        request = request.with_packaging(packaging);
    }
    Ok(request)
}

/// True when the commerce context still allows work.
fn ensure_live(ctx: &CommerceCtx) -> Result<(), SourceError> {
    ctx.check()
}

#[async_trait]
impl<C: SiteConnector> CommerceConnector for Registered<C> {
    fn source(&self) -> SourceId {
        self.inner.source()
    }

    fn capabilities(&self) -> CommerceCapabilities {
        registry_capabilities(&self.inner.capabilities())
    }

    async fn discover(
        &self,
        ctx: &CommerceCtx,
        req: CommerceSearchRequest,
    ) -> Result<Vec<CommerceDiscovery>, SourceError> {
        ensure_live(ctx)?;
        let rich = self.rich_ctx(ctx);
        let request = site_search(&req)?;
        let discoveries = self.inner.discover(&rich, request).await?;
        Ok(discoveries.iter().map(registry_discovery).collect())
    }

    async fn product(
        &self,
        ctx: &CommerceCtx,
        req: CommerceProductRequest,
    ) -> Result<faktor_commerce::CommercialOffer, SourceError> {
        ensure_live(ctx)?;
        let rich = self.rich_ctx(ctx);
        let request = site_product(&req)?;
        self.inner.product(&rich, request).await
    }

    async fn quote(
        &self,
        ctx: &CommerceCtx,
        req: CommerceQuoteRequest,
    ) -> Result<Vec<CommerceQuoteCandidate>, SourceError> {
        ensure_live(ctx)?;
        let rich = self.rich_ctx(ctx);
        let request = site_quote(&req)?;
        let candidates = self.inner.quote(&rich, request).await?;
        Ok(candidates.iter().map(registry_quote).collect())
    }
}

/// True when the URL host is one of `suffixes` (first-party policy helper
/// exposed for the runtime).
pub fn is_first_party(url: &CanonicalUrl, suffixes: &[&str]) -> bool {
    let host = url.host().to_ascii_lowercase();
    suffixes.iter().any(|suffix| {
        let suffix = suffix.to_ascii_lowercase();
        host == suffix || host.ends_with(&format!(".{suffix}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ManualClock;
    use crate::contract::extract::Strategy;
    use crate::contract::{Discovery, RequestedFreshness};
    use crate::testing::NoopTransport;
    use faktor_commerce::connector::{Capability as RegistryCapability, ConnectorRegistry};
    use faktor_commerce::query::{DetailLevel, SourceSet};
    use faktor_commerce::Money;

    fn source() -> SourceId {
        SourceId::new("1688").expect("source")
    }

    fn runtime() -> ConnectorRuntime {
        ConnectorRuntime::new(
            Arc::new(NoopTransport),
            Arc::new(QuotaState::new()),
            Arc::new(SecretGuard::new()),
            Arc::new(crate::context::NoopDiagnostics),
            Arc::new(ManualClock::new(1_700_000_000_000)),
        )
    }

    struct FakeSite {
        calls: std::sync::Mutex<usize>,
    }

    #[async_trait]
    impl SiteConnector for FakeSite {
        fn source(&self) -> SourceId {
            source()
        }

        fn capabilities(&self) -> ConnectorCapabilities {
            ConnectorCapabilities {
                source: source(),
                discovery: CapabilityLevel::Fallback,
                exact_product: CapabilityLevel::Supported,
                quantity_pricing: CapabilityLevel::Supported,
                stock: CapabilityLevel::Fallback,
                packaging: CapabilityLevel::Unsupported,
                account_pricing: CapabilityLevel::Unsupported,
                supplier_data: CapabilityLevel::Fallback,
                bulk: CapabilityLevel::Unsupported,
                mechanisms: vec![Mechanism::BrowserNetwork, Mechanism::EmbeddedState],
            }
        }

        async fn discover(
            &self,
            ctx: &AcquireCtx,
            req: SearchRequest,
        ) -> Result<Vec<Discovery>, SourceError> {
            *self.calls.lock().expect("lock") += 1;
            assert_eq!(req.freshness(), RequestedFreshness::Live);
            let deadline = ctx.deadline_ms().expect("deadline");
            assert!(
                (1_700_000_000_000..=1_700_000_000_000 + 5_000).contains(&deadline),
                "{deadline}"
            );
            assert_eq!(req.query().as_str(), "usb cable");
            Ok(vec![Discovery {
                source: source(),
                identity: faktor_commerce::ProductIdentity {
                    manufacturer: None,
                    manufacturer_part_number: None,
                    source_part_number: Some(Text::<256>::new("678901234567").expect("sku")),
                    offer_id: None,
                    canonical_url: Some(
                        CanonicalUrl::parse("https://detail.1688.com/offer/678.html").expect("url"),
                    ),
                    category: None,
                },
                title: Text::<512>::new("USB 3.0 数据线").expect("title"),
                url: CanonicalUrl::parse("https://detail.1688.com/offer/678.html").ok(),
                price_hint: Some(Money::from_micros(
                    faktor_commerce::Currency::CNY,
                    36_000_000,
                )),
                price_visibility: faktor_commerce::PriceVisibility::Public,
                stock_hint: None,
                freshness: faktor_commerce::Freshness::Live,
                documented_max_staleness_ms: None,
                observed_at_ms: 1_700_000_000_000,
                origin: Strategy::NetworkJson.origin(),
            }])
        }

        async fn product(
            &self,
            _ctx: &AcquireCtx,
            _req: ProductRequest,
        ) -> Result<faktor_commerce::CommercialOffer, SourceError> {
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

    #[tokio::test]
    async fn a_site_adapter_registers_under_the_one_registry_trait() {
        let registered = Registered::new(
            FakeSite {
                calls: std::sync::Mutex::new(0),
            },
            runtime(),
        );
        let mut registry = ConnectorRegistry::new();
        registry
            .register(Arc::new(registered))
            .expect("registration through the shared contract");
        assert_eq!(registry.sources(), vec![source()]);
        let capabilities = registry.capabilities(&source()).expect("capabilities");
        assert!(capabilities.has(RegistryCapability::Discovery));
        assert!(capabilities.has(RegistryCapability::QuantityPricing));
        assert!(!capabilities.has(RegistryCapability::Packaging));
        assert!(capabilities
            .mechanisms
            .contains(&AcquisitionMechanism::BrowserNetwork));

        let connector = registry.connector(&source()).expect("connector");
        let ctx = CommerceCtx::new()
            .with_deadline(Duration::from_millis(5_000))
            .with_freshness(FreshnessMode::Live);
        let request = CommerceSearchRequest::new(
            "usb cable",
            SourceSet::auto(),
            5,
            FreshnessMode::Live,
            DetailLevel::Compact,
        )
        .expect("search");
        let discoveries = connector.discover(&ctx, request).await.expect("discover");
        assert_eq!(discoveries.len(), 1);
        assert_eq!(
            discoveries[0]
                .price_range
                .expect("range")
                .min
                .to_decimal_string(),
            "36.000000"
        );
    }

    #[test]
    fn rich_ctx_reads_the_configured_identity_not_the_request() {
        use faktor_commerce::text::AccountScope;

        let configured = AccountScope::new("buyer-configured").expect("scope");
        let requested = AccountScope::new("buyer-requested").expect("scope");
        let profile = faktor_commerce::connector::ProfileIdentity::new(source(), "procurement-cn")
            .expect("profile");
        let runtime = runtime().with_identity(
            ConnectorIdentity::anonymous()
                .with_profile(profile)
                .with_account_scope(configured.clone())
                .with_market(Text::<32>::new("CN").expect("market"))
                .with_locale(Text::<32>::new("zh-CN").expect("locale")),
        );
        let registered = Registered::new(
            FakeSite {
                calls: std::sync::Mutex::new(0),
            },
            runtime,
        );
        // A request that tries to manufacture a different account scope must
        // not override the configured identity.
        let mut commerce = CommerceCtx::new();
        commerce.account_scope = Some(requested);
        let site = registered.rich_ctx(&commerce);
        assert_eq!(site.account_scope(), Some(&configured));
        assert_eq!(site.market().map(Text::as_str), Some("CN"));
        assert_eq!(site.locale().map(Text::as_str), Some("zh-CN"));
        assert_eq!(
            site.identity()
                .profile()
                .map(|profile| profile.profile.as_str()),
            Some("procurement-cn")
        );
    }

    #[test]
    fn an_unconfigured_identity_keeps_the_embedded_caller_scope() {
        use faktor_commerce::text::AccountScope;

        let requested = AccountScope::new("buyer-requested").expect("scope");
        let registered = Registered::new(
            FakeSite {
                calls: std::sync::Mutex::new(0),
            },
            runtime(),
        );
        let mut commerce = CommerceCtx::new();
        commerce.account_scope = Some(requested.clone());
        let site = registered.rich_ctx(&commerce);
        assert_eq!(site.account_scope(), Some(&requested));
        assert!(site.identity().profile().is_none());
    }

    /// The planner's mechanism reaches the site adapter verbatim; the bridge
    /// adds no selection and no default.
    #[test]
    fn rich_ctx_carries_the_planned_mechanism_verbatim() {
        let registered = Registered::new(
            FakeSite {
                calls: std::sync::Mutex::new(0),
            },
            runtime(),
        );
        let mut commerce = CommerceCtx::new();
        commerce.mechanism = Some(AcquisitionMechanism::BrowserNetwork);
        let site = registered.rich_ctx(&commerce);
        assert_eq!(site.mechanism(), Some(Mechanism::BrowserNetwork));

        let mut commerce = CommerceCtx::new();
        commerce.mechanism = Some(AcquisitionMechanism::Dom);
        assert_eq!(
            registered.rich_ctx(&commerce).mechanism(),
            Some(Mechanism::Dom)
        );

        // No planned mechanism means no mechanism: the adapter must refuse
        // rather than pick one.
        assert_eq!(registered.rich_ctx(&CommerceCtx::new()).mechanism(), None);
    }

    #[tokio::test]
    async fn rich_ctx_mirrors_the_caller_cancellation() {
        let registered = Registered::new(
            FakeSite {
                calls: std::sync::Mutex::new(0),
            },
            runtime(),
        );
        // A fresh caller is not cancelled; after cancellation the connector
        // context observes it on its very next liveness check.
        let commerce = CommerceCtx::new();
        let site = registered.rich_ctx(&commerce);
        assert!(!site.is_cancelled(), "a fresh caller is not cancelled");
        commerce.cancel.cancel();
        assert!(
            site.is_cancelled(),
            "the observed token must mirror the caller's cancellation"
        );
        // A caller already cancelled before the bridge is built starts
        // cancelled too.
        let cancelled = CommerceCtx::new();
        cancelled.cancel.cancel();
        let site = registered.rich_ctx(&cancelled);
        assert!(site.is_cancelled());
    }

    #[tokio::test]
    async fn boxed_connectors_register_too() {
        let registered = Registered::new(
            FakeSite {
                calls: std::sync::Mutex::new(0),
            },
            runtime(),
        );
        let mut registry = ConnectorRegistry::new();
        registry
            .register_boxed(registered.boxed())
            .expect("boxed registration");
        assert_eq!(registry.sources(), vec![source()]);
    }

    #[test]
    fn conversions_map_references_and_freshness() {
        let reference =
            ProductRef::parse("https://detail.1688.com/offer/678.html", None).expect("url ref");
        assert!(matches!(
            site_reference(&reference).expect("reference"),
            ProductReference::Url(_)
        ));
        let request = CommerceQuoteRequest::new(
            reference,
            10,
            None,
            Some(faktor_commerce::text::VariantId::new("颜色=黑色").expect("variant")),
            None,
            FreshnessMode::PreferCache,
        )
        .expect("quote");
        let site = site_quote(&request).expect("site quote");
        assert_eq!(site.quantity().get(), 10);
        assert_eq!(site.variant().expect("variant").as_str(), "颜色=黑色");
    }
}
