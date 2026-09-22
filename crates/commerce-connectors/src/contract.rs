//! The connector contract (spec §10) and its request/result types.
//!
//! There is exactly one registry-facing, object-safe trait in the workspace:
//! `faktor_commerce::connector::CommerceConnector` (re-exported here). Its
//! outcome vocabulary is the domain one (`Discovery`, `CommercialOffer`,
//! `QuoteCandidate`, `ConnectorCapabilities`, ...). `faktor-commerce-connectors`
//! is the only crate that implements it in production; [`crate::contract::bridge`]
//! adapts a site adapter (the richer [`SiteConnector`]) onto it, and
//! `faktor-commerce`'s `ConnectorRegistry` accepts the resulting
//! `Arc<dyn CommerceConnector>` / `Box<dyn CommerceConnector>`.
//!
//! The site-implementation trait keeps the acquisition-runtime context
//! (`AcquireCtx` with injected transport, quota, secrets, diagnostics, clock
//! and browser), which is what the site adapters' extraction logic needs:
//!
//! ```ignore
//! #[async_trait]
//! pub trait SiteConnector {
//!     fn source(&self) -> SourceId;
//!     fn capabilities(&self) -> ConnectorCapabilities;
//!     async fn discover(&self, ctx: &AcquireCtx, req: SearchRequest) -> Result<Vec<Discovery>, SourceError>;
//!     async fn product(&self, ctx: &AcquireCtx, req: ProductRequest) -> Result<CommercialOffer, SourceError>;
//!     async fn quote(&self, ctx: &AcquireCtx, req: QuoteRequest) -> Result<Vec<QuoteCandidate>, SourceError>;
//! }
//! ```
//!
//! Connectors never create their own HTTP client or Chromium process; both
//! are injected through [`AcquireCtx`](crate::context::AcquireCtx). Adding a
//! new marketplace means adding a module and a `SiteConnector`
//! implementation — the planner is untouched (spec §6).
//!
//! [`SourceId`] is re-exported from `faktor-commerce`; [`CapabilityLevel`]
//! is defined here because the capability vocabulary is connector-facing.

pub mod bridge;
pub mod capture;
pub mod dom;
pub mod extract;

use async_trait::async_trait;
use faktor_commerce::offer::CommercialOffer;
use faktor_commerce::quote::QuoteResolution;
use faktor_commerce::text::{CanonicalUrl, Text, VariantId};
use faktor_commerce::{
    Freshness, Money, NonZeroQuantity, PackagingType, PriceVisibility, ProductIdentity,
    SourceError, SourceId, StockState,
};

use crate::context::AcquireCtx;

pub use faktor_commerce::connector::CommerceConnector;

/// How strongly a connector supports one capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CapabilityLevel {
    /// Not supported at all.
    Unsupported,
    /// Supported through a fallback mechanism (browser/DOM) rather than the
    /// primary interface.
    Fallback,
    /// Supported through the primary interface.
    Supported,
    /// The source is authoritative for this datum.
    Authoritative,
}

impl CapabilityLevel {
    /// The stable label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unsupported => "unsupported",
            Self::Fallback => "fallback",
            Self::Supported => "supported",
            Self::Authoritative => "authoritative",
        }
    }

    /// True when the connector can serve the capability at all.
    pub const fn is_supported(self) -> bool {
        matches!(self, Self::Fallback | Self::Supported | Self::Authoritative)
    }
}

/// An acquisition mechanism (spec §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mechanism {
    /// The marketplace's official API.
    OfficialApi,
    /// A stable first-party JSON/HTTP response.
    DirectHttp,
    /// Browser network/XHR capture.
    BrowserNetwork,
    /// Embedded page/application state.
    EmbeddedState,
    /// Semantic DOM extraction.
    Dom,
}

impl Mechanism {
    /// The stable label.
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

/// One advertised capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    /// Discovery / search.
    Discovery,
    /// Exact product lookup.
    ExactProduct,
    /// Quantity-aware live pricing.
    QuantityPricing,
    /// Stock data.
    Stock,
    /// Packaging data.
    Packaging,
    /// Account-specific pricing.
    AccountPricing,
    /// Supplier data.
    SupplierData,
    /// Bulk acquisition.
    Bulk,
}

impl Capability {
    /// The stable label.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Discovery => "discovery",
            Self::ExactProduct => "exact_product",
            Self::QuantityPricing => "quantity_pricing",
            Self::Stock => "stock",
            Self::Packaging => "packaging",
            Self::AccountPricing => "account_pricing",
            Self::SupplierData => "supplier_data",
            Self::Bulk => "bulk",
        }
    }
}

/// The capabilities a connector advertises to the planner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectorCapabilities {
    /// The source these capabilities belong to.
    pub source: SourceId,
    /// Discovery support.
    pub discovery: CapabilityLevel,
    /// Exact product support.
    pub exact_product: CapabilityLevel,
    /// Quantity pricing support.
    pub quantity_pricing: CapabilityLevel,
    /// Stock support.
    pub stock: CapabilityLevel,
    /// Packaging support.
    pub packaging: CapabilityLevel,
    /// Account pricing support.
    pub account_pricing: CapabilityLevel,
    /// Supplier data support.
    pub supplier_data: CapabilityLevel,
    /// Bulk support.
    pub bulk: CapabilityLevel,
    /// The mechanisms this connector may use.
    pub mechanisms: Vec<Mechanism>,
}

impl ConnectorCapabilities {
    /// Everything unsupported, no mechanisms (a disabled/follow-up stub).
    pub fn unsupported(source: SourceId) -> Self {
        Self {
            source,
            discovery: CapabilityLevel::Unsupported,
            exact_product: CapabilityLevel::Unsupported,
            quantity_pricing: CapabilityLevel::Unsupported,
            stock: CapabilityLevel::Unsupported,
            packaging: CapabilityLevel::Unsupported,
            account_pricing: CapabilityLevel::Unsupported,
            supplier_data: CapabilityLevel::Unsupported,
            bulk: CapabilityLevel::Unsupported,
            mechanisms: Vec::new(),
        }
    }

    /// The level of one capability.
    pub fn level(&self, capability: Capability) -> CapabilityLevel {
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

    /// True when the connector can serve the capability.
    pub fn supports(&self, capability: Capability) -> bool {
        self.level(capability).is_supported()
    }
}

/// The freshness the caller asked for (spec §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RequestedFreshness {
    /// Prefer a fresh cache entry, fall back to the network.
    #[default]
    PreferCache,
    /// Live data only.
    Live,
    /// Cache only — the connector has no cache, so it refuses this.
    CacheOnly,
}

/// One discovery request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRequest {
    query: Text<512>,
    limit: u16,
    manufacturer: Option<Text<128>>,
    freshness: RequestedFreshness,
}

impl SearchRequest {
    /// The tool-level maximum (spec §5: `limit` 1..=50).
    pub const MAX_LIMIT: u16 = 50;
    /// The tool-level minimum.
    pub const MIN_LIMIT: u16 = 1;

    /// Validate and construct.
    pub fn new(
        query: Text<512>,
        limit: u16,
        manufacturer: Option<Text<128>>,
    ) -> Result<Self, SourceError> {
        if !(Self::MIN_LIMIT..=Self::MAX_LIMIT).contains(&limit) {
            return Err(SourceError::InvalidRequest);
        }
        Ok(Self {
            query,
            limit,
            manufacturer,
            freshness: RequestedFreshness::PreferCache,
        })
    }

    /// Set the requested freshness.
    pub fn with_freshness(mut self, freshness: RequestedFreshness) -> Self {
        self.freshness = freshness;
        self
    }

    /// The query.
    pub fn query(&self) -> &Text<512> {
        &self.query
    }

    /// The requested result limit.
    pub fn limit(&self) -> u16 {
        self.limit
    }

    /// The manufacturer filter, when any.
    pub fn manufacturer(&self) -> Option<&Text<128>> {
        self.manufacturer.as_ref()
    }

    /// The requested freshness.
    pub fn freshness(&self) -> RequestedFreshness {
        self.freshness
    }
}

/// A product reference the caller already knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProductReference {
    /// A manufacturer part number.
    ManufacturerPartNumber(Text<256>),
    /// The source's own part number / SKU.
    SourcePartNumber(Text<256>),
    /// The source's offer/listing id.
    OfferId(Text<512>),
    /// A canonical product URL.
    Url(CanonicalUrl),
}

impl ProductReference {
    /// The stable label of the reference kind.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::ManufacturerPartNumber(_) => "mpn",
            Self::SourcePartNumber(_) => "source_part_number",
            Self::OfferId(_) => "offer_id",
            Self::Url(_) => "url",
        }
    }

    /// The reference text (the URL form for [`ProductReference::Url`]).
    pub fn as_str(&self) -> &str {
        match self {
            Self::ManufacturerPartNumber(value) | Self::SourcePartNumber(value) => value.as_str(),
            Self::OfferId(value) => value.as_str(),
            Self::Url(url) => url.as_str(),
        }
    }
}

/// One exact-product request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductRequest {
    reference: ProductReference,
    quantity: Option<NonZeroQuantity>,
    live_pricing: bool,
    freshness: RequestedFreshness,
}

impl ProductRequest {
    /// A request for one reference.
    pub fn new(reference: ProductReference) -> Self {
        Self {
            reference,
            quantity: None,
            live_pricing: false,
            freshness: RequestedFreshness::PreferCache,
        }
    }

    /// Request live quantity pricing for `quantity`.
    pub fn with_live_pricing(mut self, live_pricing: bool) -> Self {
        self.live_pricing = live_pricing;
        self
    }

    /// Attach the intended quantity.
    pub fn with_quantity(mut self, quantity: NonZeroQuantity) -> Self {
        self.quantity = Some(quantity);
        self
    }

    /// Set the requested freshness.
    pub fn with_freshness(mut self, freshness: RequestedFreshness) -> Self {
        self.freshness = freshness;
        self
    }

    /// The reference.
    pub fn reference(&self) -> &ProductReference {
        &self.reference
    }

    /// The intended quantity, when stated.
    pub fn quantity(&self) -> Option<NonZeroQuantity> {
        self.quantity
    }

    /// Whether live quantity pricing was requested.
    pub fn live_pricing(&self) -> bool {
        self.live_pricing
    }

    /// The requested freshness.
    pub fn freshness(&self) -> RequestedFreshness {
        self.freshness
    }
}

/// One quote request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuoteRequest {
    reference: ProductReference,
    quantity: NonZeroQuantity,
    variant: Option<VariantId>,
    packaging: Option<PackagingType>,
}

impl QuoteRequest {
    /// A quote request for one reference and quantity.
    pub fn new(reference: ProductReference, quantity: NonZeroQuantity) -> Self {
        Self {
            reference,
            quantity,
            variant: None,
            packaging: None,
        }
    }

    /// Select a variant.
    pub fn with_variant(mut self, variant: VariantId) -> Self {
        self.variant = Some(variant);
        self
    }

    /// Select a packaging.
    pub fn with_packaging(mut self, packaging: PackagingType) -> Self {
        self.packaging = Some(packaging);
        self
    }

    /// The reference.
    pub fn reference(&self) -> &ProductReference {
        &self.reference
    }

    /// The requested quantity.
    pub fn quantity(&self) -> NonZeroQuantity {
        self.quantity
    }

    /// The selected variant, when any.
    pub fn variant(&self) -> Option<&VariantId> {
        self.variant.as_ref()
    }

    /// The selected packaging, when any.
    pub fn packaging(&self) -> Option<PackagingType> {
        self.packaging
    }
}

/// One discovery result. A discovery price is a **hint** from the source's
/// discovery endpoint; it is never a quote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    /// The source that produced the discovery.
    pub source: SourceId,
    /// The product identity.
    pub identity: ProductIdentity,
    /// The listing title.
    pub title: Text<512>,
    /// The canonical product URL, when one exists.
    pub url: Option<CanonicalUrl>,
    /// The headline price hint, when the source states one.
    pub price_hint: Option<Money>,
    /// The visibility of the price hint.
    pub price_visibility: PriceVisibility,
    /// The stock hint, when the source states one.
    pub stock_hint: Option<StockState>,
    /// The freshness of this observation.
    pub freshness: Freshness,
    /// The observation origin (which acquisition strategy produced it).
    pub origin: faktor_commerce::offer::ObservationOrigin,
    /// The documented maximum staleness of the underlying catalog data,
    /// when the source documents one (for example DigiKey KeywordSearch:
    /// up to 24 hours). Discovery data with a documented staleness bound is
    /// never presented as live.
    pub documented_max_staleness_ms: Option<u64>,
    /// When the discovery was observed.
    pub observed_at_ms: u64,
}

/// One quote candidate: a resolved (or explicitly unresolved) quote over a
/// normalized offer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuoteCandidate {
    /// The source that produced the offer.
    pub source: SourceId,
    /// The normalized offer the quote is over.
    pub offer: CommercialOffer,
    /// The deterministic resolution (status, tiers, MOQ, notes).
    pub resolution: QuoteResolution,
    /// The freshness of the price data.
    pub freshness: Freshness,
}

/// The site-adapter implementation trait (spec §10 shape, acquisition
/// runtime context). [`crate::contract::bridge::Registered`] adapts it onto
/// the single registry-facing
/// [`CommerceConnector`](faktor_commerce::connector::CommerceConnector).
#[async_trait]
pub trait SiteConnector: Send + Sync {
    /// The source this connector serves.
    fn source(&self) -> SourceId;

    /// The advertised capabilities.
    fn capabilities(&self) -> ConnectorCapabilities;

    /// Discovery (`source_market` op `search`).
    async fn discover(
        &self,
        ctx: &AcquireCtx,
        req: SearchRequest,
    ) -> Result<Vec<Discovery>, SourceError>;

    /// Exact product (`source_market` op `product`).
    async fn product(
        &self,
        ctx: &AcquireCtx,
        req: ProductRequest,
    ) -> Result<CommercialOffer, SourceError>;

    /// Real applicable price at quantity (`source_market` op `quote`).
    async fn quote(
        &self,
        ctx: &AcquireCtx,
        req: QuoteRequest,
    ) -> Result<Vec<QuoteCandidate>, SourceError>;
}

/// The default refusal for a connector that has no cache of its own: the
/// cache is owned by the acquire runtime.
pub(crate) fn reject_cache_only(freshness: RequestedFreshness) -> Result<(), SourceError> {
    if freshness == RequestedFreshness::CacheOnly {
        return Err(SourceError::InvalidRequest);
    }
    Ok(())
}
