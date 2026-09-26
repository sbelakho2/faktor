//! faktor-commerce-connectors — the site adapters of Faktor Acquire
//! (spec §3: site knowledge lives only in connector modules).
//!
//! # What this crate is
//!
//! Official-API-first connectors for the Faktor Acquire marketplaces, each
//! normalizing one site's API responses into the canonical
//! `faktor-commerce` domain (`CommercialOffer`, `ProductIdentity`,
//! `PriceBreak`, `PackagingOption`, `VariantOffer`, `QuoteResolution`):
//!
//! | Source | Primary | Live price | Browser fallback |
//! |--------|---------|-----------|------------------|
//! | [`mouser`] | Mouser Search API | search price breaks | policy-gated, default off |
//! | [`digikey`] | Product Information V4 | ProductDetails + PricingOptionsByQuantity | policy-gated, default off |
//! | [`lcsc`] | LCSC first-party JSON API | product detail + discounted tiers | policy-gated, default off |
//! | [`alibaba`] | *follow-up step* (spec §17 step 12) | — | — |
//! | [`china1688`] | *follow-up step* (spec §17 step 11) | — | — |
//!
//! Non-negotiables encoded here:
//!
//! * **No self-created clients.** Every byte leaves through the injected
//!   `Arc<dyn HttpTransport>` ([`http::HttpTransport`]); this crate has no
//!   HTTP client dependency and never launches Chromium. The browser
//!   fallback is a seam ([`browser::BrowserFallback`]) implemented by the
//!   acquisition runtime over `faktor-browser`.
//! * **Official API first.** Discovery and exact product lookup go to the
//!   documented API endpoints. A live-pricing request never answers from
//!   discovery/cached data: DigiKey `quote` uses ProductDetails pricing and
//!   PricingOptionsByQuantity, never the KeywordSearch price.
//! * **Documented limits.** Each connector registers its documented quota
//!   (Mouser 30/min + 1000/day; LCSC 200/min + 1000/day baseline) with the
//!   shared [`quota::QuotaState`], and DigiKey additionally consumes
//!   `X-RateLimit-Limit`/`X-RateLimit-Remaining`/`Retry-After` headers, so
//!   the hard-coded numbers are never the only limit.
//! * **The planner selects, the adapter executes.** The runtime
//!   `AcquisitionPlanner` (with its default no-substitution policy for an
//!   exhausted API quota) chooses the mechanism; adapters execute exactly
//!   the [`contract::Mechanism`] handed in through the context and can
//!   never switch API and browser themselves.
//! * **Exact money only.** Prices are captured as raw JSON tokens and
//!   parsed with integer arithmetic ([`normalize::money_from_raw`]); there
//!   is no `f64` anywhere in this crate (enforced by [`self_scan`]).
//! * **Secrets stay out of the model.** Config carries environment variable
//!   *names* ([`config`]); values are wrapped in [`secrets::SecretString`]
//!   (no `Display`, no `Serialize`, redacted `Debug`) and registered with
//!   the existing Faktor secret scanner ([`secrets::SecretGuard`] over
//!   `faktor-security`'s `SecretRegistry`). Every diagnostic detail is
//!   scrubbed before it reaches a sink.
//!
//! # The acquire seam
//!
//! Spec §10's signature takes an `AcquireCtx` owned by the acquisition
//! runtime. The in-tree `faktor-acquire` crate is deliberately domain-free
//! (spec §3) and `faktor-commerce` is pure domain, so the adapter-facing
//! seam lives with the adapters until the runtime re-exports it: this crate
//! defines [`context::AcquireCtx`] (transport, quota, secrets, diagnostics,
//! clock, deadline, cancellation, policy), [`http::HttpTransport`],
//! [`quota::QuotaState`], [`browser::BrowserFallback`], and the
//! request/result types in [`contract`]. Nothing in the seam creates I/O:
//! the transport and the browser are injected, the clock is a trait, and the
//! fixture transport in [`testing`] replays sanitized responses offline.
//!
//! # Adversarial testing
//!
//! Every test in this crate is fixture-based and offline: sanitized JSON per
//! endpoint under `fixtures/`, replayed through [`testing::FixtureTransport`].
//! Hostile fixtures (huge arrays, deep nesting, wrong types, unicode,
//! truncated bodies, oversized bodies) must produce typed
//! [`SourceError`]s — never a panic and never a misparsed price.

mod aop;

pub mod alibaba;
pub mod browser;
pub mod china1688;
pub mod config;
pub mod context;
pub mod contract;
pub mod digikey;
pub mod http;
pub mod lcsc;
pub mod mouser;
pub mod normalize;
pub mod quota;
pub mod secrets;
pub mod testing;

#[cfg(test)]
mod self_scan;

#[cfg(test)]
mod testsupport;

pub use alibaba::{
    AlibabaConnector, ApiScopes as AlibabaApiScopes, OpenApiConfig, BROWSER_SEARCH_URL,
    EGRESS_LABEL as ALIBABA_EGRESS_LABEL,
};
pub use aop::{RefreshedToken, TokenRefresher};
pub use browser::{BrowserFallback, FallbackObservation, MAX_FALLBACK_FIELDS};
pub use china1688::{
    ApiScopes as China1688ApiScopes, China1688Connector, OpenPlatformConfig,
    BROWSER_SEARCH_URL as CN_BROWSER_SEARCH_URL, EGRESS_LABEL as CN_EGRESS_LABEL,
};
pub use config::{
    ConfigError, DigiKeyConnectorConfig, LcscConnectorConfig, MouserConnectorConfig,
    ProfileConnectorConfig,
};
pub use context::{
    AccessVisibility, AcquireCtx, AcquireCtxBuilder, Cancellation, Clock, ConnectorEvent,
    ConnectorEventKind, ConnectorIdentity, Diagnostics, ManualClock, MemoryDiagnostics,
    NoopDiagnostics, SystemClock, VisibilityScopeError,
};
pub use contract::bridge::{ConnectorRuntime, Registered};
pub use contract::capture::{
    BrowserExtraction, CaptureBundle, CaptureKind, CapturedPayload, Challenge, ChallengeKind,
    FirstPartyPolicy, SharedBrowserExtraction,
};
pub use contract::extract::{
    canonical_variant_id, parse_variant_spec, reconcile, select_variant, ExtractionHealth, Field,
    FieldObservation, FieldValue, Fingerprint, ResolvedField, SchemaDrift, Strategy,
    StrategyOutcome, StrategyStats, VariantEntry, VariantSelection,
};
pub use contract::{
    Capability, CapabilityLevel, CommerceConnector, ConnectorCapabilities, Discovery, Mechanism,
    ProductReference, ProductRequest, QuoteCandidate, QuoteRequest, RequestedFreshness,
    SearchRequest, SiteConnector,
};
pub use digikey::DigiKeyConnector;
pub use http::{
    Header, HttpError, HttpMethod, HttpRequest, HttpResponse, HttpTransport, TransportError,
    MAX_RESPONSE_BYTES,
};
pub use lcsc::LcscConnector;
pub use mouser::MouserConnector;
pub use normalize::NormalizeError;
pub use quota::{
    parse_retry_after_ms, QuotaLimits, QuotaPermit, QuotaScope, QuotaSnapshot, QuotaState, DAY_MS,
    MINUTE_MS,
};
pub use secrets::{
    resolve_registered, CredentialProvider, ProcessEnvCredentials, SecretGuard, SecretString,
};
pub use testing::{CannedResponse, FixtureTransport, MapCredentials, RecordingBrowser};

// The domain vocabulary connectors speak, re-exported so callers (the tool
// gateway, the acquire runtime) can name one crate.
pub use faktor_commerce::text::{CanonicalUrl, Text, VariantId};
pub use faktor_commerce::{
    CommercialOffer, ConnectorHealth, Freshness, Money, NonZeroQuantity, PackagingType,
    PriceVisibility, ProductIdentity, SourceError, SourceId, StockState,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_trait_is_object_safe() {
        fn assert_object_safe(_connector: &dyn SiteConnector) {}
        let _ = assert_object_safe;
    }

    #[test]
    fn capability_levels_are_ordered() {
        assert!(CapabilityLevel::Unsupported < CapabilityLevel::Fallback);
        assert!(CapabilityLevel::Fallback < CapabilityLevel::Supported);
        assert!(CapabilityLevel::Supported < CapabilityLevel::Authoritative);
        assert!(!CapabilityLevel::Unsupported.is_supported());
        assert!(CapabilityLevel::Fallback.is_supported());
    }

    #[test]
    fn cache_only_is_refused_by_connectors_without_a_cache() {
        assert_eq!(
            contract::reject_cache_only(RequestedFreshness::CacheOnly),
            Err(SourceError::InvalidRequest)
        );
        assert!(contract::reject_cache_only(RequestedFreshness::Live).is_ok());
    }
}
