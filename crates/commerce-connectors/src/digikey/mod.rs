//! DigiKey connector — Product Information V4, API-first
//! (`docs/acquire.md` §10, build order step 7).
//!
//! * OAuth2 **client credentials** through the injected transport. The
//!   config carries `client_id_env` / `client_secret_env` — environment
//!   variable **names**; the values are resolved once through the injected
//!   [`CredentialProvider`] and registered with the secret scanner before
//!   use. Neither value is ever logged, returned or serialized, and the
//!   bearer token is registered the moment it is minted.
//! * `KeywordSearch` is **discovery only**: its catalog can be up to 24 h
//!   stale (documented on every [`Discovery`]).
//! * `ProductDetails` is the authoritative product record.
//!   `PricingOptionsByQuantity` is the **live** quantity pricing: a quote
//!   (and a `live_pricing` product request) is built from it and never from
//!   the KeywordSearch price.
//! * `X-RateLimit-Limit` / `X-RateLimit-Remaining` / `Retry-After` headers
//!   are consumed into the shared quota state on every response (via
//!   [`crate::http::send`]); the connector does not rely on hard-coded
//!   limits.

pub mod normalize;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use faktor_commerce::{CommercialOffer, Freshness, PricingContext, SourceError, SourceId};
use serde_json::json;

use crate::config::{ConfigError, DigiKeyConnectorConfig};
use crate::context::AcquireCtx;
use crate::contract::{
    reject_cache_only, CapabilityLevel, ConnectorCapabilities, Discovery, Mechanism,
    ProductReference, ProductRequest, QuoteCandidate, QuoteRequest, SearchRequest, SiteConnector,
};
use crate::http::{self, HttpRequest};
use crate::quota::QuotaLimits;
use crate::secrets::{CredentialProvider, SecretGuard, SecretString};

/// The documented OAuth2 token endpoint.
pub const TOKEN_URL: &str = "https://api.digikey.com/v1/oauth2/token";
/// The documented KeywordSearch endpoint (discovery; up to 24 h stale).
pub const KEYWORD_SEARCH_URL: &str = "https://api.digikey.com/products/v4/search/keyword";
/// The ProductDetails base (`{base}/{productNumber}/productdetails`).
pub const PRODUCT_DETAILS_BASE: &str = "https://api.digikey.com/products/v4/search";
/// The PricingOptionsByQuantity base (`{base}/{productNumber}/pricing`).
pub const PRICING_BASE: &str = "https://api.digikey.com/products/v4/pricing";
/// The client-id request header required by Product Information V4.
pub const CLIENT_ID_HEADER: &str = "X-DIGIKEY-Client-Id";

/// Refresh the cached token this long before its stated expiry.
const TOKEN_EARLY_REFRESH_MS: u64 = 30_000;

struct CachedToken {
    token: SecretString,
    usable_until_ms: u64,
}

/// The DigiKey connector.
pub struct DigiKeyConnector {
    source: SourceId,
    client_id: SecretString,
    client_secret: SecretString,
    secrets: Arc<SecretGuard>,
    token: Mutex<Option<CachedToken>>,
}

impl DigiKeyConnector {
    /// Build the connector from env-var *names*; values are resolved and
    /// registered immediately.
    pub fn new(
        config: &DigiKeyConnectorConfig,
        credentials: Arc<dyn CredentialProvider>,
        secrets: Arc<SecretGuard>,
    ) -> Result<Self, ConfigError> {
        if !config.enabled {
            return Err(ConfigError::Disabled {
                connector: "digikey",
            });
        }
        let client_id_env = config.require_client_id_env()?;
        let client_secret_env = config.require_client_secret_env()?;
        let client_id = crate::config::credential(credentials.as_ref(), &secrets, client_id_env)?;
        let client_secret =
            crate::config::credential(credentials.as_ref(), &secrets, client_secret_env)?;
        Ok(Self {
            source: source_id(),
            client_id,
            client_secret,
            secrets,
            token: Mutex::new(None),
        })
    }

    /// The documented DigiKey limits are the response headers themselves;
    /// there is no hard-coded local cap (spec §10).
    pub const fn documented_limits(&self) -> QuotaLimits {
        QuotaLimits::unlimited()
    }

    fn lock_token(&self) -> std::sync::MutexGuard<'_, Option<CachedToken>> {
        self.token
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn cached_token(&self, now_ms: u64) -> Option<SecretString> {
        let guard = self.lock_token();
        guard
            .as_ref()
            .filter(|cached| cached.usable_until_ms > now_ms)
            .map(|cached| cached.token.clone())
    }

    /// Fetch (or reuse) an OAuth2 client-credentials token.
    async fn access_token(&self, ctx: &AcquireCtx) -> Result<SecretString, SourceError> {
        let now_ms = ctx.now_ms();
        if let Some(token) = self.cached_token(now_ms) {
            return Ok(token);
        }
        let body = format!(
            "grant_type=client_credentials&client_id={}&client_secret={}",
            crate::http::query_escape(self.client_id.expose()),
            crate::http::query_escape(self.client_secret.expose()),
        );
        let request = HttpRequest::post_form(TOKEN_URL, body.as_bytes())?;
        let response = http::send(ctx, &self.source, "token", request).await?;
        http::map_status(&response)?;
        let parsed = normalize::parse_token_response(response.body())?;
        let (token, expires_ms) = normalize::token_from_response(&parsed)?;
        // Register the minted token before it can be stored or used.
        self.secrets.register(&token);
        let token = SecretString::new(token);
        let usable_until_ms = now_ms
            .saturating_add(expires_ms)
            .saturating_sub(TOKEN_EARLY_REFRESH_MS);
        *self.lock_token() = Some(CachedToken {
            token: token.clone(),
            usable_until_ms,
        });
        Ok(token)
    }

    fn authorize(
        &self,
        request: HttpRequest,
        token: &SecretString,
    ) -> Result<HttpRequest, SourceError> {
        let bearer = SecretString::new(format!("Bearer {}", token.expose()));
        let request = request.with_secret_header("authorization", &bearer)?;
        Ok(request.with_secret_header(CLIENT_ID_HEADER, &self.client_id)?)
    }

    fn product_details_url(product_number: &str) -> String {
        format!(
            "{PRODUCT_DETAILS_BASE}/{}/productdetails",
            crate::http::query_escape(product_number)
        )
    }

    fn pricing_url(product_number: &str) -> String {
        format!(
            "{PRICING_BASE}/{}/pricing",
            crate::http::query_escape(product_number)
        )
    }

    async fn fetch_product_details(
        &self,
        ctx: &AcquireCtx,
        token: &SecretString,
        product_number: &str,
    ) -> Result<normalize::ProductRaw, SourceError> {
        let request = HttpRequest::get(&Self::product_details_url(product_number))?;
        let response = http::send(
            ctx,
            &self.source,
            "product_details",
            self.authorize(request, token)?,
        )
        .await?;
        http::map_status(&response)?;
        normalize::parse_product_details(response.body())
    }

    async fn fetch_pricing(
        &self,
        ctx: &AcquireCtx,
        token: &SecretString,
        product_number: &str,
        quantity: u64,
    ) -> Result<normalize::PricingResponse, SourceError> {
        let body = serde_json::to_vec(&json!({ "RequestedQuantity": quantity }))
            .map_err(|_| SourceError::InvalidRequest)?;
        let request = HttpRequest::post_json(&Self::pricing_url(product_number), &body)?;
        let response = http::send(
            ctx,
            &self.source,
            "pricing_by_quantity",
            self.authorize(request, token)?,
        )
        .await?;
        http::map_status(&response)?;
        normalize::parse_pricing_response(response.body())
    }

    fn product_number(reference: &ProductReference) -> Result<String, SourceError> {
        match reference {
            ProductReference::ManufacturerPartNumber(value)
            | ProductReference::SourcePartNumber(value) => Ok(value.as_str().to_string()),
            ProductReference::OfferId(value) => Ok(value.as_str().to_string()),
            ProductReference::Url(_) => Err(SourceError::InvalidRequest),
        }
    }
}

fn source_id() -> SourceId {
    SourceId::new("digikey").expect("valid static source id")
}

#[async_trait]
impl SiteConnector for DigiKeyConnector {
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
            packaging: CapabilityLevel::Supported,
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
        let token = self.access_token(ctx).await?;
        let body = serde_json::to_vec(&json!({
            "Keywords": req.query().as_str(),
            "Limit": req.limit(),
            "Offset": 0,
        }))
        .map_err(|_| SourceError::InvalidRequest)?;
        let request = self.authorize(HttpRequest::post_json(KEYWORD_SEARCH_URL, &body)?, &token)?;
        let response = http::send(ctx, &self.source, "search", request).await?;
        http::map_status(&response)?;
        let parsed = normalize::parse_keyword_response(response.body())?;
        normalize::discoveries_from_keyword(&parsed, &self.source, ctx, req.limit())
    }

    async fn product(
        &self,
        ctx: &AcquireCtx,
        req: ProductRequest,
    ) -> Result<CommercialOffer, SourceError> {
        ctx.check_alive()?;
        reject_cache_only(req.freshness())?;
        let number = Self::product_number(req.reference())?;
        let token = self.access_token(ctx).await?;
        let product = self.fetch_product_details(ctx, &token, &number).await?;
        let pricing = match req.quantity() {
            Some(quantity) if req.live_pricing() => Some(
                self.fetch_pricing(ctx, &token, &number, quantity.get())
                    .await?,
            ),
            _ => None,
        };
        normalize::offer_from_product(&product, pricing.as_ref(), &self.source, ctx)
    }

    async fn quote(
        &self,
        ctx: &AcquireCtx,
        req: QuoteRequest,
    ) -> Result<Vec<QuoteCandidate>, SourceError> {
        ctx.check_alive()?;
        let number = Self::product_number(req.reference())?;
        let token = self.access_token(ctx).await?;
        // Live pricing is always ProductDetails + PricingOptionsByQuantity;
        // the discovery price is never consulted here.
        let product = self.fetch_product_details(ctx, &token, &number).await?;
        let pricing = self
            .fetch_pricing(ctx, &token, &number, req.quantity().get())
            .await?;
        let offer = normalize::offer_from_product(&product, Some(&pricing), &self.source, ctx)?;
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
    use faktor_commerce::{NonZeroQuantity, QuoteStatus};

    const PLANTED_CLIENT_ID: &str = "digikey-sanitized-client-id-0001";
    const PLANTED_CLIENT_SECRET: &str = "digikey-sanitized-client-secret-0001";
    const PLANTED_TOKEN: &str = "dk-sanitized-access-token-0001";

    fn digikey_connector(secrets: &Arc<SecretGuard>) -> DigiKeyConnector {
        let config = DigiKeyConnectorConfig {
            enabled: true,
            client_id_env: Some(text("FAKTOR_DIGIKEY_CLIENT_ID")),
            client_secret_env: Some(text("FAKTOR_DIGIKEY_CLIENT_SECRET")),
        };
        let credentials: Arc<dyn CredentialProvider> = Arc::new(
            MapCredentials::new()
                .with("FAKTOR_DIGIKEY_CLIENT_ID", PLANTED_CLIENT_ID)
                .with("FAKTOR_DIGIKEY_CLIENT_SECRET", PLANTED_CLIENT_SECRET),
        );
        DigiKeyConnector::new(&config, credentials, secrets.clone()).expect("connector")
    }

    fn token_response() -> CannedResponse {
        CannedResponse::new(
            200,
            crate::testing::fixture("digikey/oauth_token.json")
                .expect("fixture")
                .into_bytes(),
        )
    }

    fn search_request(query: &str, limit: u16) -> SearchRequest {
        SearchRequest::new(text(query), limit, None).expect("request")
    }

    fn mpn_request(mpn: &str, quantity: u64) -> QuoteRequest {
        QuoteRequest::new(
            ProductReference::ManufacturerPartNumber(text(mpn)),
            NonZeroQuantity::new(quantity).expect("quantity"),
        )
    }

    #[tokio::test]
    async fn token_is_minted_once_and_credentials_are_redacted() {
        let rig = Rig::new();
        rig.transport.push(token_response());
        rig.transport.push(CannedResponse::new(
            200,
            crate::testing::fixture("digikey/keyword_search.json")
                .expect("fixture")
                .into_bytes(),
        ));
        let connector = digikey_connector(&rig.secrets);
        connector
            .discover(&rig.ctx(), search_request("TPS5430DDAR", 5))
            .await
            .expect("discover");
        assert_eq!(rig.transport.request_count(), 2);
        let token_url = rig.transport.request_url(0).expect("token url");
        assert_eq!(token_url, TOKEN_URL);
        let token_body = rig.transport.request_body(0).expect("token body");
        assert!(token_body.contains("grant_type=client_credentials"));
        assert!(token_body.contains(PLANTED_CLIENT_ID));
        assert_eq!(
            rig.transport.request_header(1, "authorization"),
            Some(format!("Bearer {PLANTED_TOKEN}"))
        );
        assert_eq!(
            rig.transport.request_header(1, "x-digikey-client-id"),
            Some(PLANTED_CLIENT_ID.to_string())
        );

        // No credential may survive into any rendered sink.
        let rendered = rig.transport.rendered_requests();
        for secret in [PLANTED_CLIENT_ID, PLANTED_CLIENT_SECRET, PLANTED_TOKEN] {
            assert!(
                !rendered.contains(secret),
                "rendered request leaked {secret}"
            );
            assert!(!rig.diagnostics.joined().contains(secret));
        }
        assert!(rendered.contains("<redacted>"), "{rendered}");

        // A second operation reuses the cached token: no third request.
        rig.transport.push(CannedResponse::new(
            200,
            crate::testing::fixture("digikey/keyword_search.json")
                .expect("fixture")
                .into_bytes(),
        ));
        connector
            .discover(&rig.ctx(), search_request("TPS5430DDA", 5))
            .await
            .expect("discover again");
        assert_eq!(rig.transport.request_count(), 3);
        assert!(rig
            .transport
            .request_url(2)
            .expect("url")
            .contains("keyword"));
    }

    #[tokio::test]
    async fn live_pricing_never_uses_the_stale_keyword_price() {
        let rig = Rig::new();
        rig.transport.push(token_response());
        rig.transport.push(CannedResponse::new(
            200,
            crate::testing::fixture("digikey/keyword_search.json")
                .expect("fixture")
                .into_bytes(),
        ));
        let connector = digikey_connector(&rig.secrets);
        let discoveries = connector
            .discover(&rig.ctx(), search_request("TPS5430DDAR", 5))
            .await
            .expect("discover");
        assert_eq!(
            discoveries[0]
                .price_hint
                .expect("stale hint")
                .to_decimal_string(),
            "0.850000"
        );

        // Live pricing: ProductDetails + PricingOptionsByQuantity.
        rig.transport.push(CannedResponse::new(
            200,
            crate::testing::fixture("digikey/product_details.json")
                .expect("fixture")
                .into_bytes(),
        ));
        rig.transport.push(CannedResponse::new(
            200,
            crate::testing::fixture("digikey/pricing_by_quantity.json")
                .expect("fixture")
                .into_bytes(),
        ));
        let candidates = connector
            .quote(&rig.ctx(), mpn_request("TPS5430DDAR", 100))
            .await
            .expect("quote");
        assert_eq!(candidates.len(), 1);
        let candidate = &candidates[0];
        assert_eq!(candidate.resolution.status, QuoteStatus::Resolved);
        assert_eq!(
            candidate
                .resolution
                .unit_price
                .expect("unit price")
                .to_decimal_string(),
            "4.500000"
        );
        for stale in ["0.850000", "1.000000"] {
            assert!(
                candidate
                    .offer
                    .price_breaks
                    .iter()
                    .all(|break_| break_.unit_price.to_decimal_string() != stale),
                "the stale discovery price {stale} must never survive into a live quote"
            );
        }
        assert_eq!(rig.transport.request_count(), 4);
        assert!(rig
            .transport
            .request_url(2)
            .expect("url")
            .contains("productdetails"));
        assert!(rig
            .transport
            .request_url(3)
            .expect("url")
            .contains("pricing"));
        assert!(candidate.freshness.is_current());
    }

    #[tokio::test]
    async fn live_pricing_product_request_fetches_quantity_pricing() {
        let rig = Rig::new();
        rig.transport.push(token_response());
        rig.transport.push(CannedResponse::new(
            200,
            crate::testing::fixture("digikey/product_details.json")
                .expect("fixture")
                .into_bytes(),
        ));
        rig.transport.push(CannedResponse::new(
            200,
            crate::testing::fixture("digikey/pricing_by_quantity.json")
                .expect("fixture")
                .into_bytes(),
        ));
        let connector = digikey_connector(&rig.secrets);
        let request = ProductRequest::new(ProductReference::ManufacturerPartNumber(text(
            "TPS5430DDAR",
        )))
        .with_live_pricing(true)
        .with_quantity(NonZeroQuantity::new(100).expect("quantity"));
        let offer = connector
            .product(&rig.ctx(), request)
            .await
            .expect("product");
        assert_eq!(
            offer.price_breaks[1].unit_price.to_decimal_string(),
            "4.500000"
        );
        assert!(rig
            .transport
            .request_url(2)
            .expect("url")
            .contains("pricing"));
    }

    #[tokio::test]
    async fn rate_limit_headers_are_consumed_into_the_quota_state() {
        let rig = Rig::new();
        rig.transport.push(token_response());
        // The headers observed from a sanitized live response.
        let header_fixture: serde_json::Value = serde_json::from_str(
            &crate::testing::fixture("digikey/rate_limit_headers.json").expect("fixture"),
        )
        .expect("json");
        let mut response = CannedResponse::new(
            200,
            crate::testing::fixture("digikey/keyword_search.json")
                .expect("fixture")
                .into_bytes(),
        );
        for (name, value) in header_fixture["headers"].as_object().expect("headers") {
            response = response.with_header(name, value.as_str().expect("header value"));
        }
        rig.transport.push(response);
        let connector = digikey_connector(&rig.secrets);
        let ctx = rig.ctx();
        connector
            .discover(&ctx, search_request("TPS5430DDAR", 5))
            .await
            .expect("first discovery");
        let snapshot = rig.quota.snapshot(&connector.source(), rig.clock.now_ms());
        assert_eq!(snapshot.header_limit, Some(1000));
        assert_eq!(snapshot.header_remaining, Some(0));

        // The observed header budget pins the source without another
        // request, and the pin expires with the observed reset.
        let blocked = connector
            .discover(&ctx, search_request("TPS5430DDAR", 5))
            .await
            .expect_err("pinned");
        assert_eq!(
            blocked,
            SourceError::RateLimited {
                retry_after_ms: 60_000
            }
        );
        assert_eq!(rig.transport.request_count(), 2);
        rig.clock.advance(60_000);
        rig.transport.push(CannedResponse::new(
            200,
            crate::testing::fixture("digikey/keyword_search.json")
                .expect("fixture")
                .into_bytes(),
        ));
        connector
            .discover(&ctx, search_request("TPS5430DDAR", 5))
            .await
            .expect("reset");
        assert_eq!(rig.transport.request_count(), 3);
    }

    #[tokio::test]
    async fn http_and_payload_errors_are_typed() {
        let cases: &[(&str, u16, SourceError)] = &[
            (
                "digikey/error_401.json",
                401,
                SourceError::AuthenticationRequired,
            ),
            (
                "digikey/error_429.json",
                429,
                SourceError::RateLimited {
                    retry_after_ms: 60_000,
                },
            ),
            ("digikey/error_500.json", 500, SourceError::ApiUnavailable),
            (
                "digikey/malformed.json",
                200,
                SourceError::ExtractionIncomplete,
            ),
            (
                "digikey/wrong_types.json",
                200,
                SourceError::ExtractionIncomplete,
            ),
            (
                "digikey/unicode_hostile.json",
                200,
                SourceError::ExtractionIncomplete,
            ),
            (
                "digikey/deep_nesting.json",
                200,
                SourceError::ExtractionIncomplete,
            ),
        ];
        for (fixture, status, expected) in cases {
            let rig = Rig::new();
            rig.transport.push(token_response());
            rig.transport.push(CannedResponse::new(
                *status,
                crate::testing::fixture(fixture)
                    .expect("fixture")
                    .into_bytes(),
            ));
            let connector = digikey_connector(&rig.secrets);
            let error = connector
                .discover(&rig.ctx(), search_request("TPS5430DDAR", 5))
                .await
                .expect_err(fixture);
            assert_eq!(error, *expected, "{fixture}");
            let rendered = format!("{error:?} {}", error.as_str());
            assert!(!rendered.contains(PLANTED_CLIENT_SECRET), "{fixture}");
            assert!(
                !rig.diagnostics.joined().contains(PLANTED_TOKEN),
                "{fixture}"
            );
        }
    }

    #[tokio::test]
    async fn oversized_response_is_typed() {
        let rig = Rig::new();
        rig.transport.push(token_response());
        rig.transport.push(CannedResponse::new(
            200,
            vec![b'x'; crate::http::MAX_RESPONSE_BYTES + 1],
        ));
        let connector = digikey_connector(&rig.secrets);
        let error = connector
            .discover(&rig.ctx(), search_request("TPS5430DDAR", 5))
            .await
            .expect_err("oversized");
        assert_eq!(error, SourceError::ResponseTooLarge);
    }

    #[tokio::test]
    async fn config_requires_both_env_var_names() {
        let rig = Rig::new();
        let config = DigiKeyConnectorConfig {
            enabled: true,
            client_id_env: None,
            client_secret_env: Some(text("FAKTOR_DIGIKEY_CLIENT_SECRET")),
        };
        let credentials: Arc<dyn CredentialProvider> = Arc::new(MapCredentials::new());
        match DigiKeyConnector::new(&config, credentials, rig.secrets.clone()) {
            Err(ConfigError::MissingEnvName { connector, field }) => {
                assert_eq!(connector, "digikey");
                assert_eq!(field, "client_id_env");
            }
            _ => panic!("a missing env var name must be refused"),
        }
    }

    #[tokio::test]
    async fn cache_only_and_url_references_are_refused_without_network() {
        let rig = Rig::new();
        let connector = digikey_connector(&rig.secrets);
        let error = connector
            .discover(
                &rig.ctx(),
                search_request("TPS5430DDAR", 1).with_freshness(RequestedFreshness::CacheOnly),
            )
            .await
            .expect_err("cache only");
        assert_eq!(error, SourceError::InvalidRequest);
        let url = faktor_commerce::text::CanonicalUrl::parse(
            "https://www.digikey.com/en/products/detail/texas-instruments/TPS5430DDAR/1234567",
        )
        .expect("url");
        let error = connector
            .product(&rig.ctx(), ProductRequest::new(ProductReference::Url(url)))
            .await
            .expect_err("url");
        assert_eq!(error, SourceError::InvalidRequest);
        assert_eq!(rig.transport.request_count(), 0);
    }
}
