//! faktor-commerce — the canonical commerce domain of Faktor Acquire.
//!
//! This crate is pure domain: no HTTP client, no browser, no model, no
//! agent, no database. It is the vocabulary the acquisition runtime,
//! connectors and the `source_market` tool share, and it is where every
//! commercial invariant lives:
//!
//! - [`money`]: exact integer money ([`money::Money`]) with a decimal-string
//!   wire format and no floating-point representation anywhere;
//! - [`quantity`]: bounded purchase quantities;
//! - [`text`]: bounded, validated text/identity/URL newtypes for hostile
//!   input;
//! - [`packaging`]: first-class packaging kinds;
//! - [`identity`]: product identity and the deterministic
//!   [`identity::PartNumberNormalizer`] that preserves meaningful suffixes;
//! - [`offer`]: [`offer::CommercialOffer`] and its parts (tiers, variants,
//!   packaging options, stock, lead time, supplier, provenance, freshness);
//! - [`quote`]: [`quote::price_at_quantity`] and the deterministic quote
//!   resolution (MOQ, order multiple, package quantity, packaging, variant,
//!   tiers, explicit promotions, account pricing);
//! - [`matching`]: [`matching::MatchCertainty`] plus machine-readable
//!   signals, never a bare score;
//! - [`ranking`]: a transparent deterministic weighted ranking;
//! - [`reconcile`]: multi-extractor [`reconcile::Observation`] /
//!   [`reconcile::ResolvedField`] reconciliation with documented authority
//!   ordering (conflicts are recorded, never averaged);
//! - [`error`]: the typed [`error::SourceError`] and
//!   [`error::ConnectorHealth`] of the connector contract;
//! - [`query`]: typed per-operation requests and their bounds;
//! - [`bom`]: bounded, restart-stable BOM lines;
//! - [`cache`]: commercial cache identity, field-level freshness and
//!   request coalescing;
//! - [`connector`]: the §10 connector contract, the injected transport and
//!   browser seams, and the capability/health/circuit-breaker registry;
//! - [`result`]: the forbidden-material scanner and the compact job-result /
//!   CAS-artifact contract;
//! - [`store`]: the durable `<data-dir>/commerce/commerce.db` store;
//! - [`jobs`]: deterministic, restart-safe jobs;
//! - [`service`]: the daemon-lifetime [`service::CommerceSourceService`].

pub mod bom;
pub mod cache;
pub mod connector;
pub mod error;
pub mod identity;
pub mod jobs;
pub mod matching;
pub mod money;
pub mod offer;
pub mod packaging;
pub mod quantity;
pub mod query;
pub mod quote;
pub mod ranking;
pub mod reconcile;
pub mod result;
pub mod service;
pub mod store;
pub mod text;

pub use error::{
    ConnectorHealth, SourceError, SourceRetryClass, VerificationKind, SOURCE_ERROR_LABELS,
    VERIFICATION_KIND_LABELS,
};
pub use identity::IdentityError;
pub use identity::{
    NormalizedPartNumber, PartNumberError, PartNumberNormalizer, ProductIdentity, SuffixKind,
    SuffixToken,
};
pub use identity::{MAX_PART_NUMBER_BYTES, MAX_SUFFIX_TOKENS};
pub use matching::MatchError;
pub use matching::{
    match_offer, match_offers, MatchCertainty, MatchOutcome, MatchSignals, OfferMatch, PartQuery,
};
pub use money::{BasisPoints, Currency, Money, MoneyError};
pub use money::{BasisPointsError, CurrencyError};
pub use offer::{
    validate_price_breaks, CommercialOffer, Freshness, LeadTime, LifecycleStatus, Manufacturer,
    ObservationOrigin, OfferError, OfferProvenance, PackagingOption, PriceBreak, PriceRange,
    PriceVisibility, Promotion, PromotionKind, StockState, Supplier, VariantAttribute,
    VariantOffer, MAX_ATTRIBUTES_PER_VARIANT, MAX_PACKAGING_OPTIONS, MAX_PRICE_BREAKS_PER_LIST,
    MAX_VARIANTS_PER_OFFER,
};
pub use offer::{ConfidenceError, PriceRangeError};
pub use packaging::PackagingType;
pub use quantity::{NonZeroQuantity, Quantity, QuantityError, MAX_ORDER_QUANTITY};
pub use quote::QuoteResolutionError;
pub use quote::{
    price_at_quantity, resolve_quote, AppliedPromotion, PricingContext, Quote, QuoteError,
    QuoteNote, QuoteResolution, QuoteStatus, VariantRequest,
};
pub use ranking::{rank_offers, RankScore, RankedOffer, RankingContext, ScoreComponent, ScoreKey};
pub use reconcile::{
    classify_schema_drift, resolve_field, ExtractionOutcome, Observation, ReconciliationMode,
    ReconciliationPolicy, ResolvedField, SchemaDrift, SchemaFingerprint, DOMAIN_SCHEMA_FINGERPRINT,
};
pub use text::{
    AccountScope, CanonicalUrl, SourceId, Text, TextError, UrlError, VariantId,
    MAX_ACCOUNT_SCOPE_BYTES, MAX_SOURCE_ID_BYTES, MAX_URL_BYTES, MAX_VARIANT_ID_BYTES,
};

pub use bom::{item_key, Bom, BomItem, MAX_BOM_LINES};
pub use cache::{
    decide as decide_freshness, CacheClass, CacheConfigError, CacheDecision, CacheEntry,
    CacheIdentity, CacheTtls, ConditionalValidators, PricingScope, SingleFlight,
    DEFAULT_DISCOVERY_TTL_S, DEFAULT_PRICE_TTL_S, DEFAULT_PRODUCT_TTL_S, DEFAULT_STOCK_TTL_S,
    DEFAULT_SUPPLIER_TTL_S,
};
pub use connector::{
    AcquisitionMechanism, AcquisitionPath, BrowserAuthority, BrowserCapture, Capability,
    CommerceConnector, ConnectorCapabilities, ConnectorPolicy, ConnectorRegistry, Discovery,
    ExtractionPriority, HttpTransport, ProfileIdentity, QuoteCandidate, RegistryError,
    TransportBody, TransportRequest, TransportResponse, UnavailableBrowser, UnavailableTransport,
};
pub use jobs::{
    advance_job, bom_line_key, bom_request, job_digest, submit_job, terminal_outcome, CommerceJob,
    CommerceJobRequest, ItemOutcome, JobId, JobItemExecutor, JobItemResult, JobItemState, JobLine,
    JobOutcome, JobState, JobSubmitOutcome, JobWork, MAX_JOB_ITEM_ATTEMPTS,
};
pub use query::{
    canonical_query, DetailLevel, FreshnessMode, Op, ProductRef, ProductRequest, QuoteRequest,
    RequestError, SearchRequest, SourceSet, MAX_BOM_LINES as MAX_BOM_LINES_QUERY, MAX_LIMIT,
    MAX_QUERY_BYTES, MAX_REF_BYTES, MAX_SOURCES,
};
pub use result::{
    artifact_digest, scan_forbidden, scan_forbidden_bytes, scrub_discoveries, scrub_offer,
    scrub_quote_candidates, scrub_secrets, validate_digest, ArtifactRef, ArtifactStore,
    CompactResult, CompactStatus, ForbiddenMaterial, ImportantEntry, ResultCounts,
    COMPACT_RESULT_HARD_MAX_BYTES, COMPACT_RESULT_TARGET_MAX_BYTES,
    COMPACT_RESULT_TARGET_MIN_BYTES, MAX_ARTIFACT_BYTES,
};
pub use service::{
    CommerceSourceService, JobStatus, ProductOutcome, QuoteOutcome, RefusingArtifacts,
    SearchOutcome, ServiceConfig, ServiceError, COMMERCE_DB_NAME, COMMERCE_DIR_NAME,
};
pub use store::{
    BrowserProfileStateRow, CachedRow, CommerceStore, CommerceStoreError, ConnectorStateRow,
    EgressStateRow, GcPolicy, GcReport, NormalizedPayload, SnapshotDiagnostics, SnapshotVersions,
    COMMERCE_MIGRATIONS, COMMERCE_SCHEMA_VERSION, MAX_PAYLOAD_BYTES,
};

#[cfg(test)]
mod no_float_scan;
