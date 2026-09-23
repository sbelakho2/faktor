//! LCSC connector — first-party JSON API, API-first
//! (`docs/acquire.md` §10, build order step 8).
//!
//! * `KeywordSearch` (`/wmsc/search/global`) discovers products; an MPN
//!   lookup resolves through search to the LCSC product code and then
//!   `ProductDetails` (`/wmsc/product/detail`).
//! * Both regular and **discounted** price lists are supported: a discount
//!   list is the applicable price and the offer is `promotional`; a
//!   malformed discount list is a typed error, never silently replaced by
//!   the regular list.
//! * The documented baseline quota (200/min, 1000/day) is installed into
//!   the shared [`QuotaState`](crate::quota::QuotaState) on every operation.
//! * The API key's environment variable **name** comes from config; the
//!   value is resolved and registered with the secret scanner before use.

pub mod normalize;

use std::sync::Arc;

use async_trait::async_trait;
use faktor_commerce::{CommercialOffer, Freshness, PricingContext, SourceError, SourceId};

use crate::config::{ConfigError, LcscConnectorConfig};
use crate::context::AcquireCtx;
use crate::contract::{
    reject_cache_only, CapabilityLevel, ConnectorCapabilities, Discovery, Mechanism,
    ProductReference, ProductRequest, QuoteCandidate, QuoteRequest, SearchRequest, SiteConnector,
};
use crate::http::{self, HttpRequest};
use crate::quota::QuotaLimits;
use crate::secrets::{CredentialProvider, SecretGuard, SecretString};

/// The documented LCSC keyword search endpoint.
pub const SEARCH_URL: &str = "https://wmsc.lcsc.com/wmsc/search/global";
/// The documented LCSC product detail endpoint.
pub const PRODUCT_DETAIL_URL: &str = "https://wmsc.lcsc.com/wmsc/product/detail";

/// The documented LCSC baseline rate limit (per minute).
pub const DOCUMENTED_PER_MINUTE: u32 = 200;
/// The documented LCSC baseline rate limit (per day).
pub const DOCUMENTED_PER_DAY: u32 = 1000;
/// The documented baseline limits, installed into the shared quota state.
pub const DOCUMENTED_LIMITS: QuotaLimits =
    QuotaLimits::new(Some(DOCUMENTED_PER_MINUTE), Some(DOCUMENTED_PER_DAY));

/// The LCSC connector.
pub struct LcscConnector {
    source: SourceId,
    api_key: SecretString,
}

impl LcscConnector {
    /// Build the connector from the configured env-var *name*.
    pub fn new(
        config: &LcscConnectorConfig,
        credentials: Arc<dyn CredentialProvider>,
        secrets: &SecretGuard,
    ) -> Result<Self, ConfigError> {
        if !config.enabled {
            return Err(ConfigError::Disabled { connector: "lcsc" });
        }
        let env_name = config.require_api_key_env()?;
        let api_key = crate::config::credential(credentials.as_ref(), secrets, env_name)?;
        Ok(Self {
            source: source_id(),
            api_key,
        })
    }

    /// The documented baseline limits.
    pub const fn documented_limits(&self) -> QuotaLimits {
        DOCUMENTED_LIMITS
    }

    fn ensure_quota(&self, ctx: &AcquireCtx) {
        ctx.quota().register_limits(&self.source, DOCUMENTED_LIMITS);
    }

    /// The API key travels in the documented `key` query parameter; it is
    /// registered with the outbound scanner and declared on the request, so
    /// the transport boundary permits exactly this credential and
    /// [`crate::http::redact_url`] removes it from every rendered form.
    fn with_key(&self, url: &str) -> String {
        format!(
            "{url}{}key={}",
            if url.contains('?') { '&' } else { '?' },
            crate::http::query_escape(self.api_key.expose())
        )
    }

    fn search_url(&self, query: &str, limit: u16) -> String {
        self.with_key(&format!(
            "{SEARCH_URL}?keyword={}&currentPage=1&pageSize={limit}",
            crate::http::query_escape(query)
        ))
    }

    fn detail_url(&self, product_code: &str) -> String {
        self.with_key(&format!(
            "{PRODUCT_DETAIL_URL}?productCode={}",
            crate::http::query_escape(product_code)
        ))
    }

    async fn fetch_json(
        &self,
        ctx: &AcquireCtx,
        operation: &'static str,
        url: &str,
    ) -> Result<normalize::Response, SourceError> {
        let response = http::send(
            ctx,
            &self.source,
            operation,
            HttpRequest::get(url)?.with_url_credential(&self.api_key),
        )
        .await?;
        http::map_status(&response)?;
        normalize::parse_response(response.body())
    }

    /// Resolve a product reference to an LCSC product code, then fetch the
    /// full product record.
    async fn product_from_reference(
        &self,
        ctx: &AcquireCtx,
        reference: &ProductReference,
    ) -> Result<normalize::Product, SourceError> {
        match reference {
            ProductReference::SourcePartNumber(code) => {
                let response = self
                    .fetch_json(ctx, "product", &self.detail_url(code.as_str()))
                    .await?;
                response
                    .result
                    .as_ref()
                    .and_then(|result| result.product_list.first())
                    .cloned()
                    .ok_or(SourceError::ProductNotFound)
            }
            ProductReference::OfferId(code) => {
                let response = self
                    .fetch_json(ctx, "product", &self.detail_url(code.as_str()))
                    .await?;
                response
                    .result
                    .as_ref()
                    .and_then(|result| result.product_list.first())
                    .cloned()
                    .ok_or(SourceError::ProductNotFound)
            }
            ProductReference::ManufacturerPartNumber(mpn) => {
                let response = self
                    .fetch_json(ctx, "search", &self.search_url(mpn.as_str(), 10))
                    .await?;
                let code = normalize::first_product(&response)
                    .and_then(|product| product.product_code.clone())
                    .ok_or(SourceError::ProductNotFound)?;
                let response = self
                    .fetch_json(ctx, "product", &self.detail_url(&code))
                    .await?;
                response
                    .result
                    .as_ref()
                    .and_then(|result| result.product_list.first())
                    .cloned()
                    .ok_or(SourceError::ProductNotFound)
            }
            ProductReference::Url(_) => Err(SourceError::InvalidRequest),
        }
    }
}

fn source_id() -> SourceId {
    SourceId::new("lcsc").expect("valid static source id")
}

#[async_trait]
impl SiteConnector for LcscConnector {
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
            mechanisms: vec![Mechanism::OfficialApi],
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
        let response = self
            .fetch_json(
                ctx,
                "search",
                &self.search_url(req.query().as_str(), req.limit()),
            )
            .await?;
        normalize::discoveries_from_search(&response, &self.source, ctx, req.limit())
    }

    async fn product(
        &self,
        ctx: &AcquireCtx,
        req: ProductRequest,
    ) -> Result<CommercialOffer, SourceError> {
        ctx.check_alive()?;
        reject_cache_only(req.freshness())?;
        self.ensure_quota(ctx);
        let product = self.product_from_reference(ctx, req.reference()).await?;
        normalize::offer_from_product(&product, &self.source, ctx)
    }

    async fn quote(
        &self,
        ctx: &AcquireCtx,
        req: QuoteRequest,
    ) -> Result<Vec<QuoteCandidate>, SourceError> {
        ctx.check_alive()?;
        self.ensure_quota(ctx);
        let product = self.product_from_reference(ctx, req.reference()).await?;
        let offer = normalize::offer_from_product(&product, &self.source, ctx)?;
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
    use crate::context::Clock;
    use crate::contract::RequestedFreshness;
    use crate::secrets::CredentialProvider;
    use crate::testing::{CannedResponse, MapCredentials};
    use crate::testsupport::{text, Rig};
    use faktor_commerce::text::Text;
    use faktor_commerce::{NonZeroQuantity, QuoteStatus};

    const PLANTED_KEY: &str = "lcsc-sanitized-key-0123456789abcdef";

    fn lcsc_connector(secrets: &SecretGuard) -> LcscConnector {
        let config = LcscConnectorConfig {
            enabled: true,
            api_key_env: Some(text("FAKTOR_LCSC_KEY")),
        };
        let credentials: Arc<dyn CredentialProvider> =
            Arc::new(MapCredentials::new().with("FAKTOR_LCSC_KEY", PLANTED_KEY));
        LcscConnector::new(&config, credentials, secrets).expect("connector")
    }

    fn fixture(relative: &str) -> Vec<u8> {
        crate::testing::fixture(relative)
            .expect("fixture")
            .into_bytes()
    }

    fn search_request(query: &str, limit: u16) -> SearchRequest {
        SearchRequest::new(text(query), limit, None).expect("request")
    }

    #[tokio::test]
    async fn search_uses_the_documented_endpoint_and_registers_quota() {
        let rig = Rig::new();
        rig.transport.push(CannedResponse::new(
            200,
            fixture("lcsc/keyword_search.json"),
        ));
        let connector = lcsc_connector(&rig.secrets);
        let discoveries = connector
            .discover(&rig.ctx(), search_request("STM32F407VGT6", 5))
            .await
            .expect("discover");
        assert_eq!(discoveries.len(), 1);
        let url = rig.transport.request_url(0).expect("url");
        assert!(url.contains("/wmsc/search/global"), "{url}");
        assert!(url.contains("pageSize=5"), "{url}");
        assert!(url.contains(&format!("key={PLANTED_KEY}")));
        let snapshot = rig.quota.snapshot(&connector.source(), rig.clock.now_ms());
        assert_eq!(snapshot.minute_limit, Some(200));
        assert_eq!(snapshot.day_limit, Some(1000));
    }

    #[tokio::test]
    async fn mpn_resolves_through_search_then_product_details() {
        let rig = Rig::new();
        rig.transport.push(CannedResponse::new(
            200,
            fixture("lcsc/keyword_search.json"),
        ));
        rig.transport.push(CannedResponse::new(
            200,
            fixture("lcsc/product_details.json"),
        ));
        let connector = lcsc_connector(&rig.secrets);
        let offer = connector
            .product(
                &rig.ctx(),
                ProductRequest::new(ProductReference::ManufacturerPartNumber(text(
                    "STM32F407VGT6",
                ))),
            )
            .await
            .expect("product");
        assert_eq!(
            offer.identity.source_part_number.as_ref().map(Text::as_str),
            Some("C89642")
        );
        assert_eq!(offer.moq.map(NonZeroQuantity::get), Some(1));
        assert!(rig
            .transport
            .request_url(0)
            .expect("url")
            .contains("search/global"));
        assert!(rig
            .transport
            .request_url(1)
            .expect("url")
            .contains("product/detail"));
    }

    #[tokio::test]
    async fn source_part_number_goes_straight_to_product_details() {
        let rig = Rig::new();
        rig.transport.push(CannedResponse::new(
            200,
            fixture("lcsc/product_details.json"),
        ));
        let connector = lcsc_connector(&rig.secrets);
        connector
            .product(
                &rig.ctx(),
                ProductRequest::new(ProductReference::SourcePartNumber(text("C89642"))),
            )
            .await
            .expect("product");
        assert_eq!(rig.transport.request_count(), 1);
        assert!(rig
            .transport
            .request_url(0)
            .expect("url")
            .contains("productCode=C89642"));
    }

    #[tokio::test]
    async fn discounted_pricing_is_quoted_when_present() {
        let rig = Rig::new();
        rig.transport.push(CannedResponse::new(
            200,
            fixture("lcsc/product_details_discount.json"),
        ));
        let connector = lcsc_connector(&rig.secrets);
        let request = QuoteRequest::new(
            ProductReference::SourcePartNumber(text("C89642")),
            NonZeroQuantity::new(100).expect("quantity"),
        );
        let candidates = connector.quote(&rig.ctx(), request).await.expect("quote");
        let candidate = &candidates[0];
        assert_eq!(candidate.resolution.status, QuoteStatus::Resolved);
        assert_eq!(
            candidate
                .resolution
                .unit_price
                .expect("unit price")
                .to_decimal_string(),
            "9.500000"
        );
        assert_eq!(
            candidate.offer.price_visibility,
            faktor_commerce::PriceVisibility::Promotional
        );
    }

    #[tokio::test]
    async fn regular_pricing_is_quoted_when_no_discount_exists() {
        let rig = Rig::new();
        rig.transport.push(CannedResponse::new(
            200,
            fixture("lcsc/product_details.json"),
        ));
        let connector = lcsc_connector(&rig.secrets);
        let request = QuoteRequest::new(
            ProductReference::SourcePartNumber(text("C89642")),
            NonZeroQuantity::new(100).expect("quantity"),
        );
        let candidates = connector.quote(&rig.ctx(), request).await.expect("quote");
        assert_eq!(
            candidates[0]
                .resolution
                .unit_price
                .expect("unit price")
                .to_decimal_string(),
            "9.870000"
        );
    }

    #[tokio::test]
    async fn documented_baseline_limits_are_enforced() {
        let rig = Rig::new();
        for _ in 0..DOCUMENTED_PER_MINUTE {
            rig.transport.push(CannedResponse::new(
                200,
                fixture("lcsc/keyword_search.json"),
            ));
        }
        let connector = lcsc_connector(&rig.secrets);
        let ctx = rig.ctx();
        for _ in 0..DOCUMENTED_PER_MINUTE {
            connector
                .discover(&ctx, search_request("STM32F407VGT6", 1))
                .await
                .expect("within baseline");
        }
        let exhausted = connector
            .discover(&ctx, search_request("STM32F407VGT6", 1))
            .await
            .expect_err("rate limited");
        assert!(matches!(exhausted, SourceError::RateLimited { .. }));
        assert_eq!(
            rig.transport.request_count(),
            DOCUMENTED_PER_MINUTE as usize
        );
    }

    #[tokio::test]
    async fn http_and_payload_errors_are_typed_and_secrets_stay_out() {
        let cases: &[(&str, u16, SourceError)] = &[
            (
                "lcsc/error_401.json",
                401,
                SourceError::AuthenticationRequired,
            ),
            (
                "lcsc/error_429.json",
                429,
                SourceError::RateLimited {
                    retry_after_ms: 60_000,
                },
            ),
            ("lcsc/error_500.json", 500, SourceError::ApiUnavailable),
            (
                "lcsc/malformed.json",
                200,
                SourceError::ExtractionIncomplete,
            ),
            (
                "lcsc/wrong_types.json",
                200,
                SourceError::ExtractionIncomplete,
            ),
            (
                "lcsc/unicode_hostile.json",
                200,
                SourceError::ExtractionIncomplete,
            ),
            (
                "lcsc/deep_nesting.json",
                200,
                SourceError::ExtractionIncomplete,
            ),
            (
                "lcsc/huge_price_breaks.json",
                200,
                SourceError::ExtractionIncomplete,
            ),
        ];
        for (fixture_name, status, expected) in cases {
            let rig = Rig::new();
            rig.transport
                .push(CannedResponse::new(*status, fixture(fixture_name)));
            let connector = lcsc_connector(&rig.secrets);
            let error = connector
                .discover(&rig.ctx(), search_request("STM32F407VGT6", 5))
                .await
                .expect_err(fixture_name);
            assert_eq!(error, *expected, "{fixture_name}");
            let rendered = format!("{error:?} {}", error.as_str());
            assert!(!rendered.contains(PLANTED_KEY), "{fixture_name}");
            assert!(
                !rig.diagnostics.joined().contains(PLANTED_KEY),
                "{fixture_name}"
            );
            assert!(!rig.transport.rendered_requests().contains(PLANTED_KEY));
        }
    }

    #[tokio::test]
    async fn oversized_response_and_cache_only_are_typed() {
        let rig = Rig::new();
        rig.transport.push(CannedResponse::new(
            200,
            vec![b'x'; crate::http::MAX_RESPONSE_BYTES + 1],
        ));
        let connector = lcsc_connector(&rig.secrets);
        let error = connector
            .discover(&rig.ctx(), search_request("STM32F407VGT6", 5))
            .await
            .expect_err("oversized");
        assert_eq!(error, SourceError::ResponseTooLarge);

        let rig = Rig::new();
        let connector = lcsc_connector(&rig.secrets);
        let error = connector
            .discover(
                &rig.ctx(),
                search_request("STM32F407VGT6", 5).with_freshness(RequestedFreshness::CacheOnly),
            )
            .await
            .expect_err("cache only");
        assert_eq!(error, SourceError::InvalidRequest);
        assert_eq!(rig.transport.request_count(), 0);
    }
}
