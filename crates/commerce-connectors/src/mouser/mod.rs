//! Mouser Electronics connector — official Search API first
//! (`docs/acquire.md` §10, build order step 6).
//!
//! * Discovery chooses **exact part-number search** when the query looks
//!   like an MPN ([`crate::normalize::looks_like_mpn`]) and keyword search
//!   otherwise.
//! * Every request leaves through the injected
//!   [`HttpTransport`](crate::http::HttpTransport) — this connector creates
//!   no client, launches no browser and never touches the environment
//!   directly.
//! * The documented quota (30 calls/min, 1000/day) is installed into the
//!   shared [`QuotaState`](crate::quota::QuotaState) on every operation, so
//!   the limits hold no matter how the runtime wired the context.
//! * Browser fallback is policy-gated (default off) and is attempted only
//!   for a field the API did not expose or for a URL reference the Search
//!   API cannot serve. It is structurally impossible on a rate limit or an
//!   exhausted quota ([`crate::browser::fallback_decision`]).

pub mod normalize;

use std::sync::Arc;

use async_trait::async_trait;
use faktor_commerce::text::CanonicalUrl;
use faktor_commerce::{CommercialOffer, Freshness, PricingContext, SourceError, SourceId};
use serde_json::json;

use crate::browser::{self, FallbackCause};
use crate::config::{ConfigError, MouserConnectorConfig};
use crate::context::{AcquireCtx, ConnectorEventKind};
use crate::contract::{
    reject_cache_only, CapabilityLevel, ConnectorCapabilities, Discovery, Mechanism,
    ProductReference, ProductRequest, QuoteCandidate, QuoteRequest, SearchRequest, SiteConnector,
};
use crate::http::{self, HttpRequest};
use crate::quota::QuotaLimits;
use crate::secrets::{CredentialProvider, SecretGuard, SecretString};

/// The documented Mouser Search API rate limit (per minute).
pub const DOCUMENTED_PER_MINUTE: u32 = 30;
/// The documented Mouser Search API rate limit (per day).
pub const DOCUMENTED_PER_DAY: u32 = 1000;
/// The documented limits, installed into the shared quota state.
pub const DOCUMENTED_LIMITS: QuotaLimits =
    QuotaLimits::new(Some(DOCUMENTED_PER_MINUTE), Some(DOCUMENTED_PER_DAY));

/// The documented exact part-number search endpoint.
pub const SEARCH_PARTNUMBER_URL: &str = "https://api.mouser.com/api/v1/search/partnumber";
/// The documented keyword search endpoint.
pub const SEARCH_KEYWORD_URL: &str = "https://api.mouser.com/api/v1/search/keyword";

/// The documented `partSearchOptions` value for an exact lookup.
const EXACT_SEARCH_OPTION: &str = "exact";

/// The Mouser connector.
pub struct MouserConnector {
    source: SourceId,
    api_key: SecretString,
}

impl MouserConnector {
    /// Build the connector. The config carries the API key's **environment
    /// variable name**; the value is resolved through the injected
    /// [`CredentialProvider`] and registered with the secret scanner in one
    /// step, so it can never exist unregistered.
    pub fn new(
        config: &MouserConnectorConfig,
        credentials: Arc<dyn CredentialProvider>,
        secrets: &SecretGuard,
    ) -> Result<Self, ConfigError> {
        if !config.enabled {
            return Err(ConfigError::Disabled {
                connector: "mouser",
            });
        }
        let env_name = config.require_api_key_env()?;
        let api_key = crate::config::credential(credentials.as_ref(), secrets, env_name)?;
        Ok(Self {
            source: source_id(),
            api_key,
        })
    }

    /// The documented limits (for the runtime's own accounting surfaces).
    pub const fn documented_limits(&self) -> QuotaLimits {
        DOCUMENTED_LIMITS
    }

    fn ensure_quota(&self, ctx: &AcquireCtx) {
        ctx.quota().register_limits(&self.source, DOCUMENTED_LIMITS);
    }

    /// Build one bounded request. The API key travels in the documented
    /// `apiKey` query parameter; [`crate::http::redact_url`] removes it from
    /// every rendered form.
    fn request(&self, endpoint: &str, body: &[u8]) -> Result<HttpRequest, SourceError> {
        let url = format!(
            "{endpoint}?apiKey={}",
            crate::http::query_escape(self.api_key.expose())
        );
        Ok(HttpRequest::post_json(&url, body)?)
    }

    /// The exact part-number search body.
    fn part_number_body(reference: &str) -> Result<Vec<u8>, SourceError> {
        serde_json::to_vec(&json!({
            "SearchByPartRequest": {
                "mouserPartNumber": reference,
                "partSearchOptions": EXACT_SEARCH_OPTION,
            }
        }))
        .map_err(|_| SourceError::InvalidRequest)
    }

    /// Fill fields the API did not expose from a policy-gated browser
    /// observation. Existing API values are never overwritten and a browser
    /// failure never hides the API result.
    async fn fill_absent_fields(&self, ctx: &AcquireCtx, offer: &mut CommercialOffer) {
        if offer.stock.is_known() && offer.lead_time.is_some() {
            return;
        }
        let Some(url) = offer.identity.canonical_url.clone() else {
            return;
        };
        let wanted: &[&'static str] = &["availability", "lead_time"];
        match browser::attempt(
            ctx,
            &self.source,
            FallbackCause::FieldAbsentFromApi,
            &url,
            wanted,
        )
        .await
        {
            Ok(Some(observation)) => normalize::merge_observation(offer, &observation),
            Ok(None) => {}
            Err(_) => ctx.record(
                &self.source,
                "browser_fallback",
                ConnectorEventKind::Note,
                None,
                Some("fallback_failed"),
            ),
        }
    }

    /// A URL reference cannot be served by the Search API: only the browser
    /// (when the policy enables it) can inspect it.
    async fn product_from_url(
        &self,
        ctx: &AcquireCtx,
        url: &CanonicalUrl,
    ) -> Result<CommercialOffer, SourceError> {
        let wanted: &[&'static str] = &[
            "mpn",
            "manufacturer",
            "description",
            "availability",
            "lead_time",
        ];
        match browser::attempt(
            ctx,
            &self.source,
            FallbackCause::FieldAbsentFromApi,
            url,
            wanted,
        )
        .await?
        {
            Some(observation) => {
                normalize::offer_from_observation(&observation, url, &self.source, ctx)
            }
            None => Err(SourceError::InvalidRequest),
        }
    }
}

fn source_id() -> SourceId {
    SourceId::new("mouser").expect("valid static source id")
}

#[async_trait]
impl SiteConnector for MouserConnector {
    fn source(&self) -> SourceId {
        self.source.clone()
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        ConnectorCapabilities {
            source: self.source.clone(),
            discovery: CapabilityLevel::Supported,
            exact_product: CapabilityLevel::Supported,
            quantity_pricing: CapabilityLevel::Supported,
            stock: CapabilityLevel::Supported,
            packaging: CapabilityLevel::Fallback,
            account_pricing: CapabilityLevel::Unsupported,
            supplier_data: CapabilityLevel::Unsupported,
            bulk: CapabilityLevel::Unsupported,
            mechanisms: vec![Mechanism::OfficialApi, Mechanism::BrowserNetwork],
        }
    }

    async fn discover(
        &self,
        ctx: &AcquireCtx,
        req: SearchRequest,
    ) -> Result<Vec<Discovery>, SourceError> {
        ctx.check_alive()?;
        reject_cache_only(req.freshness())?;
        self.ensure_quota(ctx);
        let query = req.query().as_str();
        let (endpoint, body) = if crate::normalize::looks_like_mpn(query) {
            (SEARCH_PARTNUMBER_URL, Self::part_number_body(query)?)
        } else {
            let body = serde_json::to_vec(&json!({
                "SearchByKeywordRequest": {
                    "keyword": query,
                    "records": req.limit(),
                    "startingRecord": 0,
                    "searchOptions": "",
                    "searchWithYourSignUpLanguage": "",
                }
            }))
            .map_err(|_| SourceError::InvalidRequest)?;
            (SEARCH_KEYWORD_URL, body)
        };
        let response =
            http::send(ctx, &self.source, "search", self.request(endpoint, &body)?).await?;
        http::map_status(&response)?;
        let parsed = normalize::parse_search_response(response.body())?;
        let offers = normalize::offers_from_response(&parsed, &self.source, ctx, req.limit())?;
        Ok(offers
            .iter()
            .map(|offer| crate::normalize::discovery_from_offer(offer, None))
            .collect())
    }

    async fn product(
        &self,
        ctx: &AcquireCtx,
        req: ProductRequest,
    ) -> Result<CommercialOffer, SourceError> {
        ctx.check_alive()?;
        reject_cache_only(req.freshness())?;
        self.ensure_quota(ctx);
        if let ProductReference::Url(url) = req.reference() {
            return self.product_from_url(ctx, url).await;
        }
        let body = Self::part_number_body(req.reference().as_str())?;
        let response = http::send(
            ctx,
            &self.source,
            "product",
            self.request(SEARCH_PARTNUMBER_URL, &body)?,
        )
        .await?;
        http::map_status(&response)?;
        let parsed = normalize::parse_search_response(response.body())?;
        let mut offer = normalize::first_offer(&parsed, &self.source, ctx)?;
        self.fill_absent_fields(ctx, &mut offer).await;
        Ok(offer)
    }

    async fn quote(
        &self,
        ctx: &AcquireCtx,
        req: QuoteRequest,
    ) -> Result<Vec<QuoteCandidate>, SourceError> {
        let product = ProductRequest::new(req.reference().clone());
        let offer = self.product(ctx, product).await?;
        let resolution = faktor_commerce::resolve_quote(
            &offer,
            &PricingContext {
                packaging: req.packaging(),
                now_ms: ctx.now_ms(),
                ..PricingContext::default()
            },
            req.quantity().as_quantity(),
        );
        Ok(vec![QuoteCandidate {
            source: self.source.clone(),
            offer,
            resolution,
            freshness: Freshness::Live,
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::FallbackPolicy;
    use crate::config::MouserConnectorConfig;
    use crate::contract::RequestedFreshness;
    use crate::normalize::validate_tiers;
    use crate::secrets::SecretGuard;
    use crate::testing::{CannedResponse, MapCredentials, RecordingBrowser};
    use crate::testsupport::{text, Rig};
    use faktor_commerce::text::Text;
    use faktor_commerce::QuoteStatus;
    use std::sync::Arc;

    const PLANTED_KEY: &str = "mouser-sanitized-key-0123456789abcdef";

    fn mouser_connector(secrets: &SecretGuard) -> MouserConnector {
        let config = MouserConnectorConfig {
            enabled: true,
            api_key_env: Some(text("FAKTOR_MOUSER_KEY")),
        };
        let credentials: Arc<dyn CredentialProvider> =
            Arc::new(MapCredentials::new().with("FAKTOR_MOUSER_KEY", PLANTED_KEY));
        MouserConnector::new(&config, credentials, secrets).expect("connector")
    }

    fn search_request(query: &str, limit: u16) -> SearchRequest {
        SearchRequest::new(text(query), limit, None).expect("request")
    }

    fn fixture_body(relative: &str) -> Vec<u8> {
        crate::testing::fixture(relative)
            .expect("fixture")
            .into_bytes()
    }

    #[tokio::test]
    async fn mpn_shaped_query_uses_the_exact_partnumber_endpoint() {
        let rig = Rig::new();
        rig.transport.push(CannedResponse::new(
            200,
            fixture_body("mouser/partnumber_search.json"),
        ));
        let connector = mouser_connector(&rig.secrets);
        let discoveries = connector
            .discover(&rig.ctx(), search_request("TPS5430DDAR", 10))
            .await
            .expect("discover");
        assert_eq!(discoveries.len(), 1);
        assert_eq!(
            discoveries[0]
                .identity
                .manufacturer_part_number
                .as_ref()
                .map(Text::as_str),
            Some("TPS5430DDAR")
        );
        let url = rig.transport.request_url(0).expect("request");
        assert!(url.contains("/search/partnumber"), "{url}");
        assert!(url.contains(&format!("apiKey={PLANTED_KEY}")));
        // The connector-side quota registration is observable and exact.
        let snapshot = rig.quota.snapshot(&connector.source(), 0);
        assert_eq!(snapshot.minute_limit, Some(30));
        assert_eq!(snapshot.day_limit, Some(1000));
    }

    #[tokio::test]
    async fn keyword_query_uses_the_keyword_endpoint_and_bounds_records() {
        let rig = Rig::new();
        rig.transport.push(CannedResponse::new(
            200,
            fixture_body("mouser/keyword_search.json"),
        ));
        let connector = mouser_connector(&rig.secrets);
        let discoveries = connector
            .discover(&rig.ctx(), search_request("voltage regulator 3A", 2))
            .await
            .expect("discover");
        assert_eq!(discoveries.len(), 2);
        let url = rig.transport.request_url(0).expect("request");
        assert!(url.contains("/search/keyword"), "{url}");
        let body = rig.transport.request_body(0).expect("body");
        assert!(body.contains("\"records\":2"), "{body}");
    }

    #[tokio::test]
    async fn secret_never_reaches_any_sink() {
        let rig = Rig::new();
        // A hostile 500 body echoes the key; the connector must not forward
        // it to diagnostics, rendered requests, errors or artifacts.
        rig.transport.push(CannedResponse::with_status(
            500,
            &format!(r#"{{"Errors":[{{"Message":"key {PLANTED_KEY} rejected"}}]}}"#),
        ));
        let connector = mouser_connector(&rig.secrets);
        let error = connector
            .discover(&rig.ctx(), search_request("TPS5430DDAR", 5))
            .await
            .expect_err("500");
        assert_eq!(error, SourceError::ApiUnavailable);
        let rendered = format!("{error:?} {}", error.as_str());
        assert!(!rendered.contains(PLANTED_KEY), "{rendered}");
        let diagnostics = rig.diagnostics.joined();
        assert!(!diagnostics.contains(PLANTED_KEY), "{diagnostics}");
        let requests = rig.transport.rendered_requests();
        assert!(!requests.contains(PLANTED_KEY), "{requests}");
        let serialized = serde_json::to_string(&error).map_err(|_| ()).ok();
        assert!(serialized.is_none() || !serialized.unwrap().contains(PLANTED_KEY));
    }

    #[tokio::test]
    async fn http_error_bodies_map_to_typed_errors() {
        let cases: &[(&str, u16, SourceError)] = &[
            (
                "mouser/error_401.json",
                401,
                SourceError::AuthenticationRequired,
            ),
            (
                "mouser/error_429.json",
                429,
                SourceError::RateLimited {
                    retry_after_ms: 60_000,
                },
            ),
            ("mouser/error_500.json", 500, SourceError::ApiUnavailable),
            (
                "mouser/malformed.json",
                200,
                SourceError::ExtractionIncomplete,
            ),
            (
                "mouser/wrong_types.json",
                200,
                SourceError::ExtractionIncomplete,
            ),
            (
                "mouser/unicode_hostile.json",
                200,
                SourceError::ExtractionIncomplete,
            ),
            (
                "mouser/deep_nesting.json",
                200,
                SourceError::ExtractionIncomplete,
            ),
            (
                "mouser/mixed_currencies.json",
                200,
                SourceError::ExtractionConflict,
            ),
            (
                "mouser/huge_price_breaks.json",
                200,
                SourceError::ExtractionIncomplete,
            ),
        ];
        for (fixture, status, expected) in cases {
            let rig = Rig::new();
            rig.transport
                .push(CannedResponse::new(*status, fixture_body(fixture)));
            let connector = mouser_connector(&rig.secrets);
            let error = connector
                .discover(&rig.ctx(), search_request("TPS5430DDAR", 5))
                .await
                .expect_err(fixture);
            assert_eq!(error, *expected, "{fixture}");
        }
    }

    #[tokio::test]
    async fn successful_tool_result_shape_carries_no_credential() {
        let rig = Rig::new();
        rig.transport.push(CannedResponse::new(
            200,
            fixture_body("mouser/partnumber_search.json"),
        ));
        let connector = mouser_connector(&rig.secrets);
        let discoveries = connector
            .discover(&rig.ctx(), search_request("TPS5430DDAR", 5))
            .await
            .expect("discover");
        let rendered_result = format!("{discoveries:?}");
        assert!(!rendered_result.contains(PLANTED_KEY));
        assert!(!rig.diagnostics.joined().contains(PLANTED_KEY));
    }

    #[tokio::test]
    async fn oversized_response_is_typed() {
        let rig = Rig::new();
        rig.transport.push(CannedResponse::new(
            200,
            vec![b'x'; crate::http::MAX_RESPONSE_BYTES + 1],
        ));
        let connector = mouser_connector(&rig.secrets);
        let error = connector
            .discover(&rig.ctx(), search_request("TPS5430DDAR", 5))
            .await
            .expect_err("oversized");
        assert_eq!(error, SourceError::ResponseTooLarge);
    }

    #[tokio::test]
    async fn documented_limits_are_enforced_and_fail_typed() {
        let rig = Rig::new();
        for _ in 0..DOCUMENTED_PER_MINUTE {
            rig.transport.push(CannedResponse::new(
                200,
                fixture_body("mouser/partnumber_search.json"),
            ));
        }
        let connector = mouser_connector(&rig.secrets);
        let ctx = rig.ctx();
        for _ in 0..DOCUMENTED_PER_MINUTE {
            connector
                .discover(&ctx, search_request("TPS5430DDAR", 1))
                .await
                .expect("within quota");
        }
        let exhausted = connector
            .discover(&ctx, search_request("TPS5430DDAR", 1))
            .await
            .expect_err("quota exhausted");
        assert!(matches!(exhausted, SourceError::RateLimited { .. }));
        assert_eq!(
            rig.transport.request_count(),
            DOCUMENTED_PER_MINUTE as usize
        );
    }

    #[tokio::test]
    async fn browser_fallback_is_off_by_default_and_never_evades_429() {
        let browser = Arc::new(RecordingBrowser::returning(
            crate::browser::FallbackObservation::new(vec![(
                text("availability"),
                text("99 In Stock"),
            )])
            .expect("observation"),
        ));
        let rig = Rig::new()
            .browser(browser.clone())
            .policy(FallbackPolicy::enabled());
        rig.transport.push(CannedResponse::with_status(429, "{}"));
        let connector = mouser_connector(&rig.secrets);
        let error = connector
            .product(
                &rig.ctx(),
                ProductRequest::new(ProductReference::ManufacturerPartNumber(text(
                    "TPS5430DDAR",
                ))),
            )
            .await
            .expect_err("rate limited");
        assert!(matches!(error, SourceError::RateLimited { .. }));
        assert_eq!(
            browser.calls(),
            0,
            "a 429 is never bypassed with the browser"
        );

        // Default policy (off) with an injecting browser: the refusal is the
        // policy, and the browser stays untouched too.
        let browser = Arc::new(RecordingBrowser::new());
        let rig = Rig::new().browser(browser.clone());
        rig.transport.push(CannedResponse::new(
            200,
            fixture_body("mouser/missing_fields.json"),
        ));
        let connector = mouser_connector(&rig.secrets);
        let offer = connector
            .product(
                &rig.ctx(),
                ProductRequest::new(ProductReference::ManufacturerPartNumber(text("TPS5430DDA"))),
            )
            .await
            .expect("offer");
        assert_eq!(offer.stock, faktor_commerce::StockState::Unknown);
        assert_eq!(browser.calls(), 0);
    }

    #[tokio::test]
    async fn absent_fields_are_filled_from_the_browser_only_when_enabled() {
        let browser = Arc::new(RecordingBrowser::returning(
            crate::browser::FallbackObservation::new(vec![
                (text("availability"), text("42 In Stock")),
                (text("lead_time"), text("6 Weeks")),
            ])
            .expect("observation"),
        ));
        let rig = Rig::new()
            .browser(browser.clone())
            .policy(FallbackPolicy::enabled());
        rig.transport.push(CannedResponse::new(
            200,
            fixture_body("mouser/missing_fields.json"),
        ));
        let connector = mouser_connector(&rig.secrets);
        let offer = connector
            .product(
                &rig.ctx(),
                ProductRequest::new(ProductReference::ManufacturerPartNumber(text("TPS5430DDA"))),
            )
            .await
            .expect("offer");
        assert_eq!(offer.stock.available_quantity(), Some(42));
        assert_eq!(
            offer.lead_time,
            Some(faktor_commerce::LeadTime::new(42, 42).expect("lead"))
        );
        assert_eq!(browser.calls(), 1);
        // The API-derived price list is untouched.
        assert_eq!(offer.price_breaks.len(), 1);
    }

    #[tokio::test]
    async fn url_reference_is_api_unservable_and_browser_gated() {
        let rig = Rig::new();
        let connector = mouser_connector(&rig.secrets);
        let url = CanonicalUrl::parse(
            "https://www.mouser.com/ProductDetail/Texas-Instruments/TPS5430DDAR",
        )
        .expect("url");
        let error = connector
            .product(
                &rig.ctx(),
                ProductRequest::new(ProductReference::Url(url.clone())),
            )
            .await
            .expect_err("no api path for a url");
        assert_eq!(error, SourceError::InvalidRequest);
        assert_eq!(rig.transport.request_count(), 0);

        let browser = Arc::new(RecordingBrowser::returning(
            crate::browser::FallbackObservation::new(vec![
                (text("mpn"), text("TPS5430DDAR")),
                (text("manufacturer"), text("Texas Instruments")),
                (text("availability"), text("1,234 In Stock")),
            ])
            .expect("observation"),
        ));
        let rig = Rig::new()
            .browser(browser.clone())
            .policy(FallbackPolicy::enabled());
        let connector = mouser_connector(&rig.secrets);
        let offer = connector
            .product(&rig.ctx(), ProductRequest::new(ProductReference::Url(url)))
            .await
            .expect("browser offer");
        assert_eq!(offer.stock.available_quantity(), Some(1234));
        assert_eq!(
            offer.price_visibility,
            faktor_commerce::PriceVisibility::Unknown
        );
        assert_eq!(browser.calls(), 1);
    }

    #[tokio::test]
    async fn quote_resolves_from_the_api_price_tiers() {
        let rig = Rig::new();
        rig.transport.push(CannedResponse::new(
            200,
            fixture_body("mouser/partnumber_search.json"),
        ));
        let connector = mouser_connector(&rig.secrets);
        let request = QuoteRequest::new(
            ProductReference::ManufacturerPartNumber(text("TPS5430DDAR")),
            faktor_commerce::NonZeroQuantity::new(100).expect("quantity"),
        );
        let candidates = connector.quote(&rig.ctx(), request).await.expect("quote");
        assert_eq!(candidates.len(), 1);
        let candidate = &candidates[0];
        assert_eq!(candidate.resolution.status, QuoteStatus::Resolved);
        // The documented standard pack (2500) is the order multiple used by
        // the deterministic resolution: 100 pcs round up to 2500 and select
        // the 1000+ tier.
        assert_eq!(
            candidate
                .resolution
                .billed_quantity
                .map(faktor_commerce::Quantity::as_u64),
            Some(2500)
        );
        assert_eq!(
            candidate
                .resolution
                .unit_price
                .expect("unit price")
                .to_decimal_string(),
            "2.980000"
        );
        assert!(validate_tiers(&candidate.offer.price_breaks, candidate.offer.currency).is_ok());
    }

    #[tokio::test]
    async fn cache_only_is_refused_and_disabled_config_never_constructs() {
        let rig = Rig::new();
        let connector = mouser_connector(&rig.secrets);
        let error = connector
            .discover(
                &rig.ctx(),
                search_request("TPS5430DDAR", 1).with_freshness(RequestedFreshness::CacheOnly),
            )
            .await
            .expect_err("cache only");
        assert_eq!(error, SourceError::InvalidRequest);

        let config = MouserConnectorConfig {
            enabled: false,
            api_key_env: None,
        };
        let credentials: Arc<dyn CredentialProvider> = Arc::new(MapCredentials::new());
        match MouserConnector::new(&config, credentials, &rig.secrets) {
            Err(ConfigError::Disabled { connector }) => assert_eq!(connector, "mouser"),
            _ => panic!("a disabled connector must never be constructed"),
        }
    }
}
