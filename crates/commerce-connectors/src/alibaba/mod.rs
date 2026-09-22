//! Alibaba.com connector — build order step 12 (`docs/acquire.md` §10, §17):
//! Open API capabilities first *where the credential actually grants a
//! buyer-visible surface*, then browser acquisition with its own extraction
//! logic (separate from 1688).
//!
//! Honest capability advertisement is the point of the API scope model here:
//! Alibaba's seller/ISV listing surface is **not** buyer discovery. A
//! credential granting only that surface never answers a buyer search; the
//! connector records `api_surface_not_buyer_discovery` and uses the browser
//! mechanisms instead. `CapabilityLevel::Unsupported` is reserved for
//! capabilities this connector genuinely cannot serve (account pricing,
//! bulk), never to hide a browser fallback that exists.
//!
//! Browser acquisition extracts supplier listings, product details, MOQ, tier
//! prices and supplier profiles with Alibaba's own parsers; contact-supplier /
//! RFQ pricing is `price: null` + [`PriceVisibility::InquiryRequired`].

pub mod dictionary;
pub mod extract;
pub mod normalize;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use faktor_commerce::connector::ProfileIdentity;
use faktor_commerce::money::Currency;
use faktor_commerce::offer::ObservationOrigin;
use faktor_commerce::text::{CanonicalUrl, Text, VariantId};
use faktor_commerce::{CommercialOffer, Freshness, NonZeroQuantity, SourceError, SourceId};

use crate::config::{ConfigError, ProfileConnectorConfig};
use crate::context::{AcquireCtx, ConnectorEventKind};
use crate::contract::capture::{BrowserExtraction, CaptureBundle, CaptureKind, FirstPartyPolicy};
use crate::contract::extract::{ExtractionHealth, Field, Fingerprint, Strategy, StrategyOutcome};
use crate::contract::{
    reject_cache_only, CapabilityLevel, ConnectorCapabilities, Discovery, Mechanism,
    ProductReference, ProductRequest, QuoteCandidate, QuoteRequest, SearchRequest, SiteConnector,
};
use crate::http::{self, HttpRequest};
use crate::secrets::{CredentialProvider, SecretGuard, SecretString};

/// The buyer-visible product endpoint.
pub const OPEN_API_PRODUCT_URL: &str = "https://openapi.alibaba.com/product/detail";
/// The buyer-visible discovery endpoint.
pub const OPEN_API_SEARCH_URL: &str = "https://openapi.alibaba.com/product/search";
/// The browser search endpoint.
pub const BROWSER_SEARCH_URL: &str = "https://www.alibaba.com/trade/search";
/// The browser detail endpoint prefix.
pub const BROWSER_DETAIL_PREFIX: &str = "https://www.alibaba.com/product-detail/_";
/// The stable egress label of the Alibaba profile.
pub const EGRESS_LABEL: &str = "alibaba-global";

/// The first-party host policy of this connector.
pub const FIRST_PARTY: FirstPartyPolicy = FirstPartyPolicy::new(&["alibaba.com", "alicdn.com"]);

/// What an Alibaba Open API credential is granted. The seller/ISV surface
/// (`seller_product`) is deliberately **not** buyer discovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ApiScopes {
    /// The seller/ISV listing surface (manage your own listings). Never used
    /// for buyer discovery.
    pub seller_product: bool,
    /// Buyer-visible product discovery/detail.
    pub buyer_discovery: bool,
    /// Buyer-visible trade terms (price, MOQ).
    pub trade_terms: bool,
    /// Buyer-visible supplier profile data.
    pub supplier_profile: bool,
}

impl ApiScopes {
    /// Every buyer-visible scope.
    pub const fn buyer_visible() -> Self {
        Self {
            seller_product: false,
            buyer_discovery: true,
            trade_terms: true,
            supplier_profile: true,
        }
    }

    /// True when this scope covers one semantic field.
    pub const fn covers(&self, field: Field) -> bool {
        match field {
            Field::OfferId
            | Field::Title
            | Field::Model
            | Field::Specification
            | Field::Stock
            | Field::Variant => self.buyer_discovery,
            Field::Price
            | Field::PriceRangeLow
            | Field::PriceRangeHigh
            | Field::Currency
            | Field::TierPrice => self.buyer_discovery && self.trade_terms,
            Field::MinimumOrder | Field::OrderMultiple => self.buyer_discovery && self.trade_terms,
            Field::LeadTime => self.buyer_discovery && self.trade_terms,
            Field::Supplier | Field::Manufacturer => self.buyer_discovery && self.supplier_profile,
            Field::InquiryOnly => true,
        }
    }

    /// True when no granted scope is buyer-visible (seller/ISV only).
    pub const fn seller_surface_only(&self) -> bool {
        self.seller_product && !self.buyer_discovery
    }

    /// The granted scope labels (deterministic order).
    pub fn labels(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        for (flag, label) in [
            (self.seller_product, "seller_product"),
            (self.buyer_discovery, "buyer_discovery"),
            (self.trade_terms, "trade_terms"),
            (self.supplier_profile, "supplier_profile"),
        ] {
            if flag {
                out.push(label);
            }
        }
        out
    }
}

/// An Open API credential configuration (env-var *name*, never the value).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenApiConfig {
    /// The environment variable naming the app key.
    pub app_key_env: Text<64>,
    /// The granted scopes.
    pub scopes: ApiScopes,
}

impl OpenApiConfig {
    /// Construct from an env-var name and scopes.
    pub fn new(app_key_env: &str, scopes: ApiScopes) -> Result<Self, ConfigError> {
        let app_key_env =
            Text::<64>::new(app_key_env).map_err(|_| ConfigError::MissingEnvName {
                connector: "alibaba",
                field: "app_key_env",
            })?;
        Ok(Self {
            app_key_env,
            scopes,
        })
    }
}

struct OpenApi {
    app_key: SecretString,
    scopes: ApiScopes,
}

/// The Alibaba.com connector.
pub struct AlibabaConnector {
    source: SourceId,
    profile: Text<64>,
    api: Option<OpenApi>,
    policy: FirstPartyPolicy,
    health: Mutex<ExtractionHealth>,
    fingerprints: Mutex<BTreeMap<Strategy, Fingerprint>>,
}

impl AlibabaConnector {
    /// Build the browser-only connector.
    pub fn new(config: &ProfileConnectorConfig) -> Result<Self, ConfigError> {
        if !config.enabled {
            return Err(ConfigError::Disabled {
                connector: "alibaba",
            });
        }
        let profile = config
            .profile
            .clone()
            .unwrap_or_else(|| Text::<64>::new("procurement-global").expect("literal"));
        Ok(Self {
            source: source_id(),
            profile,
            api: None,
            policy: FIRST_PARTY,
            health: Mutex::new(ExtractionHealth::new()),
            fingerprints: Mutex::new(BTreeMap::new()),
        })
    }

    /// Attach an Open API credential.
    pub fn with_open_api(
        mut self,
        config: &OpenApiConfig,
        credentials: Arc<dyn CredentialProvider>,
        secrets: &SecretGuard,
    ) -> Result<Self, ConfigError> {
        let app_key =
            crate::config::credential(credentials.as_ref(), secrets, config.app_key_env.as_str())?;
        self.api = Some(OpenApi {
            app_key,
            scopes: config.scopes,
        });
        Ok(self)
    }

    /// The granted API scopes.
    pub fn scopes(&self) -> ApiScopes {
        self.api.as_ref().map(|api| api.scopes).unwrap_or_default()
    }

    /// The per-field extraction health recorded so far.
    pub fn extraction_health(&self) -> ExtractionHealth {
        self.health
            .lock()
            .map(|health| health.clone())
            .unwrap_or_default()
    }

    /// The last accepted structural fingerprint per strategy.
    pub fn fingerprints(&self) -> Vec<(Strategy, String)> {
        self.fingerprints
            .lock()
            .map(|fingerprints| {
                fingerprints
                    .iter()
                    .map(|(strategy, fingerprint)| (*strategy, fingerprint.to_hex()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn profile_identity(&self, ctx: &AcquireCtx) -> ProfileIdentity {
        ProfileIdentity {
            source: self.source.clone(),
            profile: self.profile.clone(),
            account: ctx.account_scope().cloned(),
            egress: Text::<64>::new(EGRESS_LABEL).ok(),
        }
    }

    fn computed_capabilities(&self) -> ConnectorCapabilities {
        let scopes = self.scopes();
        let level = |covered: bool| {
            if covered {
                CapabilityLevel::Supported
            } else {
                CapabilityLevel::Fallback
            }
        };
        ConnectorCapabilities {
            source: self.source.clone(),
            discovery: level(scopes.buyer_discovery),
            exact_product: level(scopes.buyer_discovery),
            quantity_pricing: level(scopes.buyer_discovery && scopes.trade_terms),
            stock: level(scopes.buyer_discovery),
            packaging: CapabilityLevel::Fallback,
            // Buyer account pricing and bulk are not on this connector's
            // surface at all: advertised Unsupported, not hidden.
            account_pricing: CapabilityLevel::Unsupported,
            supplier_data: level(scopes.buyer_discovery && scopes.supplier_profile),
            bulk: CapabilityLevel::Unsupported,
            mechanisms: vec![
                Mechanism::OfficialApi,
                Mechanism::BrowserNetwork,
                Mechanism::EmbeddedState,
                Mechanism::Dom,
            ],
        }
    }

    fn search_url(&self, query: &str) -> Result<CanonicalUrl, SourceError> {
        CanonicalUrl::parse(&format!(
            "{BROWSER_SEARCH_URL}?SearchText={}",
            http::query_escape(&dictionary::expand_query(query))
        ))
        .map_err(|_| SourceError::InvalidRequest)
    }

    fn detail_url(&self, product_id: &str) -> Result<CanonicalUrl, SourceError> {
        let bounded = product_id.trim();
        if bounded.is_empty() || bounded.len() > 64 {
            return Err(SourceError::InvalidRequest);
        }
        CanonicalUrl::parse(&format!("{BROWSER_DETAIL_PREFIX}{bounded}.html"))
            .map_err(|_| SourceError::InvalidRequest)
    }

    fn api_url(
        &self,
        product_id: Option<&str>,
        query: Option<&str>,
        buyer_surface: bool,
    ) -> Result<Option<String>, SourceError> {
        let Some(api) = self.api.as_ref() else {
            return Ok(None);
        };
        if !buyer_surface {
            return Ok(None);
        }
        let key = http::query_escape(api.app_key.expose());
        Ok(match (product_id, query) {
            (Some(product_id), _) => Some(format!(
                "{OPEN_API_PRODUCT_URL}?apiKey={key}&productId={}",
                http::query_escape(product_id)
            )),
            (None, Some(query)) => Some(format!(
                "{OPEN_API_SEARCH_URL}?apiKey={key}&keywords={}",
                http::query_escape(&dictionary::expand_query(query))
            )),
            _ => None,
        })
    }

    async fn api_drafts(
        &self,
        ctx: &AcquireCtx,
        url: &str,
        operation: &'static str,
    ) -> Result<Vec<extract::Draft>, SourceError> {
        let request = HttpRequest::get(url)?;
        let response = http::send(ctx, &self.source, operation, request).await?;
        http::map_status(&response)?;
        let body = response.body();
        if body.len() > crate::contract::capture::MAX_CAPTURE_BYTES {
            return Err(SourceError::ResponseTooLarge);
        }
        let text = String::from_utf8_lossy(body).into_owned();
        let expected = self.fingerprint(Strategy::NetworkJson);
        let mut drafts = extract::network_items(&text, &self.source, ctx.now_ms(), expected);
        for draft in &mut drafts {
            for observation in &mut draft.observations {
                observation.origin = ObservationOrigin::OfficialApi;
            }
        }
        Ok(drafts)
    }

    fn fingerprint(&self, strategy: Strategy) -> Option<Fingerprint> {
        self.fingerprints
            .lock()
            .ok()
            .and_then(|fingerprints| fingerprints.get(&strategy).copied())
    }

    async fn collect(
        &self,
        ctx: &AcquireCtx,
        url: &CanonicalUrl,
        api_url: Option<String>,
        needed: &[Field],
        operation: &'static str,
    ) -> Result<Vec<extract::Draft>, SourceError> {
        ctx.check_alive()?;
        let mut drafts: Vec<extract::Draft> = Vec::new();
        let mut api_outage: Option<SourceError> = None;
        if let (Some(api), Some(api_url)) = (self.api.as_ref(), api_url) {
            match self.api_drafts(ctx, &api_url, operation).await {
                Ok(mut api_drafts) => {
                    for draft in &mut api_drafts {
                        draft
                            .observations
                            .retain(|observation| api.scopes.covers(observation.field));
                        if !api.scopes.covers(Field::Variant) {
                            draft.variants.clear();
                        }
                        if !api.scopes.covers(Field::TierPrice) {
                            draft.tiers.clear();
                        }
                    }
                    let uncovered: Vec<Field> = needed
                        .iter()
                        .copied()
                        .filter(|field| !api.scopes.covers(*field))
                        .collect();
                    for draft in &api_drafts {
                        self.record_draft(draft, ctx);
                    }
                    drafts.extend(api_drafts);
                    if uncovered.is_empty() {
                        return Ok(drafts);
                    }
                    let labels: Vec<&str> = uncovered.iter().map(|field| field.as_str()).collect();
                    ctx.record(
                        &self.source,
                        "api_scope",
                        ConnectorEventKind::Note,
                        None,
                        Some(&format!("api_scope_missing fields={}", labels.join(","))),
                    );
                }
                Err(error) if is_outage(&error) => {
                    api_outage = Some(error);
                    ctx.record(
                        &self.source,
                        "api",
                        ConnectorEventKind::Retry,
                        None,
                        Some("api_outage_browser_fallback"),
                    );
                }
                Err(error) => return Err(error),
            }
        }
        match self.browser_drafts(ctx, url).await {
            Ok(browser) if !browser.is_empty() => {
                drafts.extend(browser);
                Ok(drafts)
            }
            Ok(_) => match api_outage {
                Some(error) => Err(error),
                None => Err(SourceError::ExtractionIncomplete),
            },
            Err(error) => {
                if drafts.is_empty() {
                    Err(error)
                } else {
                    ctx.record(
                        &self.source,
                        "browser",
                        ConnectorEventKind::Note,
                        None,
                        Some("browser_failed_partial_api"),
                    );
                    Ok(drafts)
                }
            }
        }
    }

    async fn browser_drafts(
        &self,
        ctx: &AcquireCtx,
        url: &CanonicalUrl,
    ) -> Result<Vec<extract::Draft>, SourceError> {
        ctx.check_alive()?;
        if !self.policy.allows(url) {
            return Err(SourceError::InvalidRequest);
        }
        let extraction: Arc<dyn BrowserExtraction> = ctx
            .browser_extraction()
            .cloned()
            .ok_or(SourceError::BrowserUnavailable)?;
        let profile = self.profile_identity(ctx);
        let bundle = extraction.capture(ctx, &self.source, &profile, url).await?;
        self.drafts_from_bundle(ctx, bundle)
    }

    fn drafts_from_bundle(
        &self,
        ctx: &AcquireCtx,
        mut bundle: CaptureBundle,
    ) -> Result<Vec<extract::Draft>, SourceError> {
        if let Some(error) = bundle.challenge_error() {
            let kind = bundle
                .challenge
                .as_ref()
                .map(|challenge| challenge.kind.as_str())
                .unwrap_or("challenge");
            ctx.record(
                &self.source,
                "browser",
                ConnectorEventKind::Note,
                None,
                Some(&format!("profile_stop kind={kind}")),
            );
            return Err(error);
        }
        let dropped = bundle.retain_first_party(&self.policy);
        if dropped > 0 {
            ctx.record(
                &self.source,
                "interception",
                ConnectorEventKind::Note,
                None,
                Some("third_party_dropped"),
            );
        }
        let mut drafts: Vec<extract::Draft> = Vec::new();
        for kind in [
            CaptureKind::NetworkJson,
            CaptureKind::EmbeddedState,
            CaptureKind::StructuredMarkup,
            CaptureKind::RenderedText,
        ] {
            for payload in bundle.payloads_of(kind) {
                let text = payload.text();
                if matches!(
                    kind,
                    CaptureKind::StructuredMarkup | CaptureKind::RenderedText
                ) {
                    if let Some(challenge) = dictionary::CHALLENGE_MARKERS.detect(&text) {
                        ctx.record(
                            &self.source,
                            "browser",
                            ConnectorEventKind::Note,
                            None,
                            Some(&format!("profile_stop kind={}", challenge.kind.as_str())),
                        );
                        return Err(challenge.to_source_error());
                    }
                }
                let expected = self.fingerprint(kind.strategy());
                let now_ms = ctx.now_ms();
                if kind == CaptureKind::NetworkJson {
                    for draft in extract::network_items(&text, &self.source, now_ms, expected) {
                        self.record_draft(&draft, ctx);
                        drafts.push(draft);
                    }
                    continue;
                }
                let draft = match kind {
                    CaptureKind::NetworkJson => unreachable!(),
                    CaptureKind::EmbeddedState => {
                        extract::embedded_state(&text, &self.source, now_ms, expected)
                    }
                    CaptureKind::StructuredMarkup => {
                        extract::structural_dom(&text, &self.source, now_ms, expected)
                    }
                    CaptureKind::RenderedText => {
                        extract::rendered_text(&text, &self.source, now_ms, expected)
                    }
                };
                self.record_draft(&draft, ctx);
                drafts.push(draft);
            }
        }
        Ok(drafts)
    }

    fn record_draft(&self, draft: &extract::Draft, ctx: &AcquireCtx) {
        if let Ok(mut health) = self.health.lock() {
            for field in primary_fields(draft.strategy) {
                health.record(*field, draft.strategy, &draft.outcome);
            }
            for observation in &draft.observations {
                health.record(observation.field, draft.strategy, &draft.outcome);
            }
        }
        if let Some(fingerprint) = draft.fingerprint {
            if matches!(draft.outcome, StrategyOutcome::Success) {
                if let Ok(mut fingerprints) = self.fingerprints.lock() {
                    fingerprints.insert(draft.strategy, fingerprint);
                }
            }
        }
        match &draft.outcome {
            StrategyOutcome::SchemaMismatch(drift) => ctx.record(
                &self.source,
                "extract",
                ConnectorEventKind::Schema,
                None,
                Some(&drift.render()),
            ),
            StrategyOutcome::Conflict => ctx.record(
                &self.source,
                "extract",
                ConnectorEventKind::Schema,
                None,
                Some("extraction_conflict"),
            ),
            _ => {}
        }
    }

    fn record_conflicts(&self, assembly: &normalize::Assembly, ctx: &AcquireCtx) {
        for (field, authority, loser) in &assembly.conflicts {
            if let Ok(mut health) = self.health.lock() {
                health.record(*field, *loser, &StrategyOutcome::Conflict);
            }
            ctx.record(
                &self.source,
                "extract",
                ConnectorEventKind::Schema,
                None,
                Some(&format!(
                    "extraction_conflict field={} authority={} loser={}",
                    field.as_str(),
                    authority.as_str(),
                    loser.as_str()
                )),
            );
        }
    }

    async fn fetch(
        &self,
        ctx: &AcquireCtx,
        reference: &ProductReference,
        operation: &'static str,
    ) -> Result<normalize::Assembly, SourceError> {
        let buyer_surface = self.scopes().buyer_discovery;
        let (url, api_url) = match reference {
            ProductReference::OfferId(product_id) => (
                self.detail_url(product_id.as_str())?,
                self.api_url(Some(product_id.as_str()), None, buyer_surface)?,
            ),
            ProductReference::Url(url) => (
                url.clone(),
                match product_id_from_url(url) {
                    Some(product_id) => self.api_url(Some(&product_id), None, buyer_surface)?,
                    None => None,
                },
            ),
            ProductReference::SourcePartNumber(part)
            | ProductReference::ManufacturerPartNumber(part) => (
                self.search_url(part.as_str())?,
                self.api_url(None, Some(part.as_str()), buyer_surface)?,
            ),
        };
        let needed = [
            Field::OfferId,
            Field::Title,
            Field::Price,
            Field::Variant,
            Field::Stock,
            Field::MinimumOrder,
            Field::Supplier,
        ];
        if let Some(api) = self.api.as_ref() {
            if api.scopes.seller_surface_only() {
                ctx.record(
                    &self.source,
                    "api_surface",
                    ConnectorEventKind::Note,
                    None,
                    Some("api_surface_not_buyer_discovery seller_product_only"),
                );
            }
        }
        let drafts = self.collect(ctx, &url, api_url, &needed, operation).await?;
        let drafts = first_offer_drafts(drafts);
        let assembly = normalize::assemble(&self.source, ctx, &url, &drafts, ctx.now_ms())?;
        self.record_conflicts(&assembly, ctx);
        Ok(assembly)
    }
}

/// The numeric product id inside an Alibaba detail URL.
fn product_id_from_url(url: &CanonicalUrl) -> Option<String> {
    let tail = url
        .as_str()
        .split(['?', '#'])
        .next()
        .unwrap_or(url.as_str());
    let last = tail.rsplit('/').find(|segment| !segment.is_empty())?;
    let digits: String = last
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.len() < 6 {
        return None;
    }
    Some(digits)
}

fn first_offer_drafts(drafts: Vec<extract::Draft>) -> Vec<extract::Draft> {
    let first_offer: Option<String> = drafts.iter().find_map(|draft| {
        draft.observations.iter().find_map(|observation| {
            (observation.field == Field::OfferId)
                .then(|| observation.value.as_text().map(str::to_string))
                .flatten()
        })
    });
    let Some(first_offer) = first_offer else {
        return drafts;
    };
    drafts
        .into_iter()
        .filter(|draft| {
            draft.observations.iter().all(|observation| {
                observation.field != Field::OfferId
                    || observation.value.as_text() == Some(first_offer.as_str())
            })
        })
        .collect()
}

fn is_outage(error: &SourceError) -> bool {
    matches!(
        error,
        SourceError::ApiUnavailable | SourceError::NetworkTimeout | SourceError::EgressUnavailable
    )
}

fn primary_fields(strategy: Strategy) -> &'static [Field] {
    match strategy {
        Strategy::NetworkJson | Strategy::EmbeddedState => &[
            Field::OfferId,
            Field::Title,
            Field::Price,
            Field::MinimumOrder,
            Field::Supplier,
        ],
        Strategy::StructuralDom => &[Field::OfferId, Field::Title, Field::Price],
        Strategy::RenderedText => &[Field::Price],
    }
}

fn source_id() -> SourceId {
    SourceId::new("alibaba").expect("valid static source id")
}

impl std::fmt::Debug for AlibabaConnector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlibabaConnector")
            .field("source", &self.source)
            .field("profile", &self.profile)
            .field("api", &self.api.is_some())
            .finish()
    }
}

#[async_trait]
impl SiteConnector for AlibabaConnector {
    fn source(&self) -> SourceId {
        self.source.clone()
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        self.computed_capabilities()
    }

    async fn discover(
        &self,
        ctx: &AcquireCtx,
        req: SearchRequest,
    ) -> Result<Vec<Discovery>, SourceError> {
        ctx.check_alive()?;
        reject_cache_only(req.freshness())?;
        let url = self.search_url(req.query().as_str())?;
        let api_url = self.api_url(
            None,
            Some(req.query().as_str()),
            self.scopes().buyer_discovery,
        )?;
        if let Some(api) = self.api.as_ref() {
            if api.scopes.seller_surface_only() {
                ctx.record(
                    &self.source,
                    "api_surface",
                    ConnectorEventKind::Note,
                    None,
                    Some("api_surface_not_buyer_discovery"),
                );
            }
        }
        let drafts = self
            .collect(ctx, &url, api_url, &[Field::Title], "search")
            .await?;
        let mut discoveries: Vec<Discovery> = Vec::new();
        for draft in &drafts {
            if let Ok(assembly) = normalize::assemble(
                &self.source,
                ctx,
                &url,
                std::slice::from_ref(draft),
                ctx.now_ms(),
            ) {
                discoveries.push(normalize::discovery(&assembly));
                if discoveries.len() >= usize::from(req.limit()) {
                    break;
                }
            }
        }
        if discoveries.is_empty() {
            return Err(SourceError::ProductNotFound);
        }
        Ok(discoveries)
    }

    async fn product(
        &self,
        ctx: &AcquireCtx,
        req: ProductRequest,
    ) -> Result<CommercialOffer, SourceError> {
        ctx.check_alive()?;
        reject_cache_only(req.freshness())?;
        let assembly = self.fetch(ctx, req.reference(), "product").await?;
        Ok(assembly.offer)
    }

    async fn quote(
        &self,
        ctx: &AcquireCtx,
        req: QuoteRequest,
    ) -> Result<Vec<QuoteCandidate>, SourceError> {
        ctx.check_alive()?;
        let assembly = self.fetch(ctx, req.reference(), "quote").await?;
        let offer = assembly.offer.clone();
        let variant: Option<VariantId> = match req.variant() {
            Some(requested) => {
                if let Some(exact) = offer.variant_by_id(requested.as_str()) {
                    Some(exact.variant_id.clone())
                } else {
                    match crate::contract::extract::select_variant(
                        &assembly.variants,
                        requested.as_str(),
                    ) {
                        crate::contract::extract::VariantSelection::Exact(entry) => {
                            Some(entry.variant_id)
                        }
                        crate::contract::extract::VariantSelection::Ambiguous(_)
                        | crate::contract::extract::VariantSelection::Missing => {
                            return Err(SourceError::VariantAmbiguous)
                        }
                    }
                }
            }
            None => None,
        };
        let pricing = faktor_commerce::PricingContext {
            variant: match &variant {
                Some(variant) => faktor_commerce::quote::VariantRequest::Id(variant.as_str()),
                None => faktor_commerce::quote::VariantRequest::None,
            },
            packaging: req.packaging(),
            now_ms: ctx.now_ms(),
            ..faktor_commerce::PricingContext::default()
        };
        let resolution =
            faktor_commerce::resolve_quote(&offer, &pricing, req.quantity().as_quantity());
        Ok(vec![QuoteCandidate {
            source: self.source.clone(),
            offer,
            resolution,
            freshness: Freshness::Live,
        }])
    }
}

/// The default currency of the buyer-visible surface.
pub const DEFAULT_CURRENCY: Currency = Currency::USD;

/// A quantity of one (helper for tests).
pub fn one() -> NonZeroQuantity {
    NonZeroQuantity::new(1).expect("one")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::capture::test_support::{self as capture_support, ScriptedCapture};
    use crate::contract::capture::CaptureKind as Kind;
    use crate::testing::{CannedResponse, MapCredentials};
    use crate::testsupport::{text, Rig};

    fn fixture(relative: &str) -> String {
        crate::testing::fixture(relative).expect("fixture")
    }

    fn browser_only() -> AlibabaConnector {
        AlibabaConnector::new(&ProfileConnectorConfig {
            enabled: true,
            profile: Some(text("procurement-global")),
        })
        .expect("connector")
    }

    fn ctx_with(rig: &Rig, capture: Arc<ScriptedCapture>) -> AcquireCtx {
        AcquireCtx::builder(
            rig.transport.clone(),
            rig.quota.clone(),
            rig.secrets.clone(),
        )
        .clock(rig.clock.clone())
        .diagnostics(rig.diagnostics.clone())
        .browser_extraction(capture)
        .build()
    }

    fn detail_url() -> CanonicalUrl {
        CanonicalUrl::parse("https://www.alibaba.com/product-detail/_1600123456789.html")
            .expect("url")
    }

    fn url_reference() -> ProductReference {
        ProductReference::Url(detail_url())
    }

    fn bundle_with(fixture_path: &str, kind: Kind, url: &str) -> CaptureBundle {
        capture_support::bundle(
            url,
            vec![capture_support::payload(kind, url, &fixture(fixture_path))],
        )
    }

    #[tokio::test]
    async fn network_wins_over_stale_dom_and_records_the_conflict() {
        let rig = Rig::new();
        let capture = Arc::new(ScriptedCapture::new());
        capture.push(capture_support::bundle(
            "https://www.alibaba.com/product-detail/_1600123456789.html",
            vec![
                capture_support::payload(
                    Kind::NetworkJson,
                    "https://www.alibaba.com/api/product.json",
                    &fixture("alibaba/network_product.json"),
                ),
                capture_support::payload(
                    Kind::StructuredMarkup,
                    "https://www.alibaba.com/product-detail/_1600123456789.html",
                    &fixture("alibaba/product_dom_stale.html"),
                ),
            ],
        ));
        let connector = browser_only();
        let ctx = ctx_with(&rig, capture);
        let offer = connector
            .product(&ctx, ProductRequest::new(url_reference()))
            .await
            .expect("offer");
        assert_eq!(offer.title.as_str(), "USB 3.0 Braided Data Cable");
        assert!(offer.supplier.is_some(), "supplier profile is preserved");
        assert!(rig
            .diagnostics
            .joined()
            .contains("extraction_conflict field=price authority=network_json"));
        let health = connector.extraction_health();
        assert!(health.stats(Field::Price, Strategy::StructuralDom).conflict >= 1);
    }

    #[tokio::test]
    async fn variant_matrix_resolves_or_is_ambiguous_never_cheapest() {
        let rig = Rig::new();
        let capture = Arc::new(ScriptedCapture::new());
        for _ in 0..3 {
            capture.push(bundle_with(
                "alibaba/network_product.json",
                Kind::NetworkJson,
                "https://www.alibaba.com/product-detail/_1600123456789.html",
            ));
        }
        let connector = browser_only();
        let ctx = ctx_with(&rig, capture);
        let two = NonZeroQuantity::new(2).expect("quantity");

        let requested = VariantId::new("Color:Black;Length:1m").expect("variant");
        let candidates = connector
            .quote(
                &ctx,
                QuoteRequest::new(url_reference(), two).with_variant(requested),
            )
            .await
            .expect("quote");
        assert_eq!(
            candidates[0].resolution.status,
            faktor_commerce::QuoteStatus::Resolved
        );
        assert_eq!(
            candidates[0]
                .resolution
                .unit_price
                .expect("price")
                .to_decimal_string(),
            "1.200000"
        );

        let candidates = connector
            .quote(&ctx, QuoteRequest::new(url_reference(), two))
            .await
            .expect("quote");
        assert_eq!(
            candidates[0].resolution.status,
            faktor_commerce::QuoteStatus::VariantAmbiguous
        );
        assert_eq!(candidates[0].resolution.unit_price, None);

        let unknown = VariantId::new("Color:Blue").expect("variant");
        assert_eq!(
            connector
                .quote(
                    &ctx,
                    QuoteRequest::new(url_reference(), two).with_variant(unknown),
                )
                .await
                .expect_err("unknown variant"),
            SourceError::VariantAmbiguous
        );
    }

    #[tokio::test]
    async fn supplier_listing_and_tier_prices() {
        let rig = Rig::new();
        let capture = Arc::new(ScriptedCapture::new());
        capture.push(bundle_with(
            "alibaba/network_tiers.json",
            Kind::NetworkJson,
            "https://www.alibaba.com/product-detail/_1600123456789.html",
        ));
        let connector = browser_only();
        let ctx = ctx_with(&rig, capture);
        let offer = connector
            .product(&ctx, ProductRequest::new(url_reference()))
            .await
            .expect("offer");
        assert_eq!(offer.price_breaks.len(), 2);
        assert_eq!(
            offer.price_breaks[0].unit_price.to_decimal_string(),
            "1.200000"
        );
        assert_eq!(
            offer.price_breaks[0].min_quantity.get(),
            2,
            "the first tier starts at the MOQ ladder"
        );
        assert_eq!(
            offer.price_breaks[1].unit_price.to_decimal_string(),
            "0.800000"
        );
        assert_eq!(
            offer.supplier.as_ref().expect("supplier").name.as_str(),
            "Shenzhen Example Electronics Co., Ltd."
        );
    }

    #[tokio::test]
    async fn seller_surface_only_is_not_buyer_discovery() {
        let rig = Rig::new();
        let connector = browser_only()
            .with_open_api(
                &OpenApiConfig::new(
                    "FAKTOR_ALIBABA_APP_KEY",
                    ApiScopes {
                        seller_product: true,
                        ..ApiScopes::default()
                    },
                )
                .expect("config"),
                Arc::new(
                    MapCredentials::new().with("FAKTOR_ALIBABA_APP_KEY", "alibaba-sanitized-key"),
                ),
                &rig.secrets,
            )
            .expect("api connector");
        assert!(connector.scopes().seller_surface_only());
        assert_eq!(
            connector.capabilities().discovery,
            CapabilityLevel::Fallback,
            "a seller surface never claims buyer discovery"
        );
        assert_eq!(
            connector.capabilities().account_pricing,
            CapabilityLevel::Unsupported
        );

        let capture = Arc::new(ScriptedCapture::new());
        capture.push(bundle_with(
            "alibaba/network_product.json",
            Kind::NetworkJson,
            "https://www.alibaba.com/product-detail/_1600123456789.html",
        ));
        let ctx = ctx_with(&rig, capture.clone());
        connector
            .product(&ctx, ProductRequest::new(url_reference()))
            .await
            .expect("browser offer");
        assert_eq!(
            rig.transport.request_count(),
            0,
            "the seller surface is never used for buyer lookup"
        );
        assert_eq!(capture.call_count(), 1);
        assert!(rig
            .diagnostics
            .joined()
            .contains("api_surface_not_buyer_discovery"));
    }

    #[tokio::test]
    async fn buyer_scope_answers_alone_and_advertises_supported() {
        let rig = Rig::new();
        let connector = browser_only()
            .with_open_api(
                &OpenApiConfig::new("FAKTOR_ALIBABA_APP_KEY", ApiScopes::buyer_visible())
                    .expect("config"),
                Arc::new(
                    MapCredentials::new().with("FAKTOR_ALIBABA_APP_KEY", "alibaba-sanitized-key"),
                ),
                &rig.secrets,
            )
            .expect("api connector");
        assert_eq!(
            connector.capabilities().supplier_data,
            CapabilityLevel::Supported
        );
        rig.transport.push(CannedResponse::new(
            200,
            fixture("alibaba/api_product.json").into_bytes(),
        ));
        let capture = Arc::new(ScriptedCapture::new());
        let ctx = ctx_with(&rig, capture.clone());
        let offer = connector
            .product(&ctx, ProductRequest::new(url_reference()))
            .await
            .expect("api offer");
        assert_eq!(offer.title.as_str(), "USB 3.0 Braided Data Cable");
        assert_eq!(offer.variants.len(), 3);
        assert!(offer.supplier.is_some());
        assert_eq!(capture.call_count(), 0, "no browser when scope suffices");
        let url = rig.transport.request_url(0).expect("url");
        assert!(url.contains("openapi.alibaba.com"), "{url}");
    }

    #[tokio::test]
    async fn contact_supplier_is_inquiry_required() {
        let rig = Rig::new();
        let capture = Arc::new(ScriptedCapture::new());
        for _ in 0..2 {
            capture.push(bundle_with(
                "alibaba/network_inquiry.json",
                Kind::NetworkJson,
                "https://www.alibaba.com/product-detail/_1600123456789.html",
            ));
        }
        let connector = browser_only();
        let ctx = ctx_with(&rig, capture);
        let offer = connector
            .product(&ctx, ProductRequest::new(url_reference()))
            .await
            .expect("offer");
        assert_eq!(
            offer.price_visibility,
            faktor_commerce::PriceVisibility::InquiryRequired
        );
        assert!(offer.price_breaks.is_empty());
        let candidates = connector
            .quote(&ctx, QuoteRequest::new(url_reference(), one()))
            .await
            .expect("quote");
        assert_eq!(
            candidates[0].resolution.status,
            faktor_commerce::QuoteStatus::InquiryRequired
        );
        assert_eq!(candidates[0].resolution.unit_price, None);
    }

    #[tokio::test]
    async fn own_extraction_strategies_fall_through_independently() {
        let rig = Rig::new();
        let capture = Arc::new(ScriptedCapture::new());
        capture.push(bundle_with(
            "alibaba/embed_state.html",
            Kind::EmbeddedState,
            "https://www.alibaba.com/product-detail/_1600123456789.html",
        ));
        capture.push(bundle_with(
            "alibaba/product_dom.html",
            Kind::StructuredMarkup,
            "https://www.alibaba.com/product-detail/_1600123456789.html",
        ));
        capture.push(bundle_with(
            "alibaba/product_rendered.txt",
            Kind::RenderedText,
            "https://www.alibaba.com/product-detail/_1600123456789.html",
        ));
        let connector = browser_only();
        let ctx = ctx_with(&rig, capture.clone());
        for _ in 0..3 {
            let offer = connector
                .product(&ctx, ProductRequest::new(url_reference()))
                .await
                .expect("offer from one strategy");
            assert_eq!(offer.title.as_str(), "USB 3.0 Braided Data Cable");
        }
        let health = connector.extraction_health();
        assert!(health.stats(Field::Title, Strategy::EmbeddedState).success >= 1);
        assert!(health.stats(Field::Title, Strategy::StructuralDom).success >= 1);
        assert!(health.stats(Field::Price, Strategy::RenderedText).success >= 1);
    }

    #[tokio::test]
    async fn challenge_hostile_and_drift_are_typed() {
        let rig = Rig::new();
        let capture = Arc::new(ScriptedCapture::new());
        capture.push(capture_support::bundle(
            "https://www.alibaba.com/product-detail/_1600123456789.html",
            vec![capture_support::payload(
                Kind::StructuredMarkup,
                "https://www.alibaba.com/product-detail/_1600123456789.html",
                &fixture("alibaba/challenge_captcha.html"),
            )],
        ));
        let connector = browser_only();
        let ctx = ctx_with(&rig, capture);
        assert_eq!(
            connector
                .product(&ctx, ProductRequest::new(url_reference()))
                .await
                .expect_err("captcha"),
            SourceError::VerificationRequired {
                kind: faktor_commerce::error::VerificationKind::Captcha
            }
        );

        let rig = Rig::new();
        let capture = Arc::new(ScriptedCapture::new());
        capture.push(bundle_with(
            "alibaba/network_product.json",
            Kind::NetworkJson,
            "https://www.alibaba.com/product-detail/_1600123456789.html",
        ));
        capture.push(bundle_with(
            "alibaba/schema_drift.json",
            Kind::NetworkJson,
            "https://www.alibaba.com/product-detail/_1600123456789.html",
        ));
        capture.push(bundle_with(
            "alibaba/hostile.json",
            Kind::NetworkJson,
            "https://www.alibaba.com/product-detail/_1600123456789.html",
        ));
        let connector = browser_only();
        let ctx = ctx_with(&rig, capture);
        connector
            .product(&ctx, ProductRequest::new(url_reference()))
            .await
            .expect("first parse");
        assert_eq!(
            connector
                .product(&ctx, ProductRequest::new(url_reference()))
                .await
                .expect_err("drift"),
            SourceError::ExtractionIncomplete
        );
        let hostile = connector
            .product(&ctx, ProductRequest::new(url_reference()))
            .await
            .expect_err("hostile");
        assert!(
            matches!(
                hostile,
                SourceError::ExtractionIncomplete | SourceError::ResponseTooLarge
            ),
            "{hostile:?}"
        );
        assert!(rig.diagnostics.joined().contains("schema_drift"));
    }

    #[tokio::test]
    async fn discovery_uses_its_own_search_shape() {
        let rig = Rig::new();
        let capture = Arc::new(ScriptedCapture::new());
        capture.push(capture_support::bundle(
            "https://www.alibaba.com/trade/search?SearchText=usb",
            vec![capture_support::payload(
                Kind::NetworkJson,
                "https://www.alibaba.com/api/search.json",
                &fixture("alibaba/network_search.json"),
            )],
        ));
        let connector = browser_only();
        let ctx = ctx_with(&rig, capture.clone());
        let discoveries = connector
            .discover(
                &ctx,
                SearchRequest::new(text("usb cable"), 5, None).expect("search"),
            )
            .await
            .expect("discover");
        assert_eq!(discoveries.len(), 2);
        assert_eq!(discoveries[0].title.as_str(), "USB 3.0 Braided Data Cable");
        let (url, profile) = capture.calls()[0].clone();
        assert!(url.contains("SearchText="), "{url}");
        assert_eq!(profile, "procurement-global");
    }

    #[test]
    fn disabled_config_never_constructs() {
        let config = ProfileConnectorConfig {
            enabled: false,
            profile: None,
        };
        match AlibabaConnector::new(&config) {
            Err(ConfigError::Disabled { connector }) => assert_eq!(connector, "alibaba"),
            _ => panic!("disabled config must be refused"),
        }
        assert_eq!(DEFAULT_CURRENCY, Currency::USD);
    }
}
