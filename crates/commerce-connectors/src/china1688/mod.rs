//! 1688 connector — build order step 11 (`docs/acquire.md` §10, §17):
//! Open Platform adapter first, then browser acquisition in the documented
//! order (browser-network JSON → embedded application state → structural DOM
//! → rendered text), with human verification always last and never bypassed.
//!
//! * **Path 1 (API)** is used when an Open Platform credential is configured
//!   *and* its granted scope covers the requested fields. Scope coverage is
//!   advertised honestly per field ([`ApiScopes`]); when a scope does not
//!   cover a field the connector says so (`api_scope_missing` diagnostic) and
//!   the browser strategies supply that field — it never guesses from the
//!   API.
//! * **Paths 2-4 (browser)** go through the injected
//!   [`BrowserExtraction`](crate::contract::capture::BrowserExtraction)
//!   authority. The profile identity (account, profile, egress) is stable:
//!   it comes from configuration plus the account scope, never per request.
//! * **Challenge last, always typed.** A login form, verification
//!   interstitial, CAPTCHA container, security slider, access-denied page or
//!   rate-limit page stops the profile (`profile_stop` diagnostic) and
//!   surfaces the typed [`SourceError`]; there is no bypass path.
//! * **Variant matrix first-class.** A requested variant resolves exactly or
//!   fails `VariantAmbiguous`; the cheapest variant is never a quote.
//! * **RFQ / inquiry pricing** is `price: null` +
//!   [`PriceVisibility::InquiryRequired`], never a fabricated number.
//! * The request interception policy is connector-maintained: only
//!   first-party payloads reach the extractors.

pub mod auth;
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
use crate::context::{AccessVisibility, AcquireCtx, ConnectorEventKind};
use crate::contract::capture::{BrowserExtraction, CaptureBundle, CaptureKind, FirstPartyPolicy};
use crate::contract::extract::{ExtractionHealth, Field, Fingerprint, Strategy, StrategyOutcome};
use crate::contract::{
    reject_cache_only, CapabilityLevel, ConnectorCapabilities, Discovery, Mechanism,
    ProductReference, ProductRequest, QuoteCandidate, QuoteRequest, SearchRequest, SiteConnector,
};
use crate::http::{self, HttpRequest};
use crate::secrets::{CredentialProvider, SecretGuard};

use auth::{AccessToken, China1688Auth, TokenRefresher};

/// The Open Platform product-detail endpoint.
pub const OPEN_PLATFORM_PRODUCT_URL: &str =
    "https://gw.open.1688.com/openapi/param2/1/com.alibaba.product/alibaba.product.get";
/// The Open Platform keyword-search endpoint.
pub const OPEN_PLATFORM_SEARCH_URL: &str =
    "https://gw.open.1688.com/openapi/param2/1/com.alibaba.product/alibaba.product.search";
/// The browser search endpoint.
pub const BROWSER_SEARCH_URL: &str = "https://s.1688.com/selloffer/offer_search.htm";
/// The browser detail endpoint prefix.
pub const BROWSER_DETAIL_PREFIX: &str = "https://detail.1688.com/offer/";
/// The stable egress label of the 1688 profile. Egress changes only for
/// operational reasons, never per request.
pub const EGRESS_LABEL: &str = "1688-cn";

/// The first-party host policy of this connector.
pub const FIRST_PARTY: FirstPartyPolicy = FirstPartyPolicy::new(&["1688.com", "alibaba.com"]);

/// What an Open Platform credential is granted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ApiScopes {
    /// Buyer-visible search/discovery.
    pub discovery: bool,
    /// Product detail (id, title, model, specification).
    pub product: bool,
    /// Prices and price ranges.
    pub price: bool,
    /// Stock.
    pub stock: bool,
    /// Supplier/factory data.
    pub supplier: bool,
    /// Variant (SKU) matrix.
    pub variants: bool,
    /// MOQ / order multiple.
    pub moq: bool,
}

impl ApiScopes {
    /// Every scope, for a credential documented to cover the whole buyer
    /// surface.
    pub const fn all() -> Self {
        Self {
            discovery: true,
            product: true,
            price: true,
            stock: true,
            supplier: true,
            variants: true,
            moq: true,
        }
    }

    /// True when this scope covers one semantic field.
    pub const fn covers(&self, field: Field) -> bool {
        match field {
            Field::OfferId
            | Field::Title
            | Field::Model
            | Field::Specification
            | Field::LeadTime => self.product,
            Field::Price
            | Field::PriceRangeLow
            | Field::PriceRangeHigh
            | Field::Currency
            | Field::TierPrice => self.price,
            Field::MinimumOrder | Field::OrderMultiple => self.moq,
            Field::Stock => self.stock,
            Field::Supplier | Field::Manufacturer => self.supplier,
            Field::Variant => self.variants,
            Field::InquiryOnly => true,
        }
    }

    /// True when every field is covered.
    pub fn covers_all(&self, fields: &[Field]) -> bool {
        fields.iter().all(|field| self.covers(*field))
    }

    /// The labels of the granted scopes (deterministic order).
    pub fn labels(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        for (flag, label) in [
            (self.discovery, "discovery"),
            (self.product, "product"),
            (self.price, "price"),
            (self.stock, "stock"),
            (self.supplier, "supplier"),
            (self.variants, "variants"),
            (self.moq, "moq"),
        ] {
            if flag {
                out.push(label);
            }
        }
        out
    }
}

/// An Open Platform credential configuration. Every field is an environment
/// variable *name*, never a value; values are resolved through the injected
/// credential provider and registered with the secret scanner, and never
/// enter the model context.
///
/// The app key and app secret are mandatory for any Open Platform call; the
/// access-token and refresh-token names are optional. An account-scoped call
/// without a valid unexpired access token is typed
/// [`SourceError::AuthenticationRequired`], never an anonymous request and
/// never a silent browser fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenPlatformConfig {
    app_key_env: Text<64>,
    app_secret_env: Text<64>,
    access_token_env: Option<Text<64>>,
    access_token_expires_at_env: Option<Text<64>>,
    refresh_token_env: Option<Text<64>>,
    scopes: ApiScopes,
}

impl OpenPlatformConfig {
    /// A configuration with the mandatory env-var names and scopes.
    pub fn new(
        app_key_env: &str,
        app_secret_env: &str,
        scopes: ApiScopes,
    ) -> Result<Self, ConfigError> {
        Ok(Self {
            app_key_env: env_name("1688", "app_key_env", app_key_env)?,
            app_secret_env: env_name("1688", "app_secret_env", app_secret_env)?,
            access_token_env: None,
            access_token_expires_at_env: None,
            refresh_token_env: None,
            scopes,
        })
    }

    /// Name the env vars holding the access token and its absolute expiry
    /// (unix milliseconds). Both are configured together or not at all.
    pub fn with_access_token(
        mut self,
        access_token_env: &str,
        access_token_expires_at_env: &str,
    ) -> Result<Self, ConfigError> {
        self.access_token_env = Some(env_name("1688", "access_token_env", access_token_env)?);
        self.access_token_expires_at_env = Some(env_name(
            "1688",
            "access_token_expires_at_env",
            access_token_expires_at_env,
        )?);
        Ok(self)
    }

    /// Name the env var holding the OAuth refresh token.
    pub fn with_refresh_token(mut self, refresh_token_env: &str) -> Result<Self, ConfigError> {
        self.refresh_token_env = Some(env_name("1688", "refresh_token_env", refresh_token_env)?);
        Ok(self)
    }

    /// The granted scopes.
    pub fn scopes(&self) -> ApiScopes {
        self.scopes
    }

    /// The app-key env-var name.
    pub fn app_key_env(&self) -> &str {
        self.app_key_env.as_str()
    }

    /// The app-secret env-var name.
    pub fn app_secret_env(&self) -> &str {
        self.app_secret_env.as_str()
    }
}

/// Validate one environment-variable name field.
fn env_name(
    connector: &'static str,
    field: &'static str,
    raw: &str,
) -> Result<Text<64>, ConfigError> {
    Text::<64>::new(raw).map_err(|_| ConfigError::MissingEnvName { connector, field })
}

/// Resolve the configured access token and its absolute expiry.
///
/// Both env names are configured together (see
/// [`OpenPlatformConfig::with_access_token`]); a value that resolves while
/// its expiry is unparseable installs no token, so account-scoped calls
/// refuse typed rather than run with an unbounded credential.
fn resolve_access_token(
    config: &OpenPlatformConfig,
    credentials: &dyn CredentialProvider,
    secrets: &SecretGuard,
) -> Result<Option<AccessToken>, ConfigError> {
    let (Some(token_env), Some(expires_env)) = (
        config.access_token_env.as_ref(),
        config.access_token_expires_at_env.as_ref(),
    ) else {
        return Ok(None);
    };
    let token = crate::config::credential(credentials, secrets, token_env.as_str())?;
    let expires_raw = crate::config::credential(credentials, secrets, expires_env.as_str())?;
    Ok(crate::aop::parse_expiry_ms(expires_raw.expose())
        .map(|expires_at_ms| AccessToken::new(token, expires_at_ms)))
}

struct OpenPlatform {
    auth: China1688Auth,
    scopes: ApiScopes,
}

/// The 1688 connector.
pub struct China1688Connector {
    source: SourceId,
    profile: Text<64>,
    api: Option<OpenPlatform>,
    policy: FirstPartyPolicy,
    health: Mutex<ExtractionHealth>,
    fingerprints: Mutex<BTreeMap<Strategy, Fingerprint>>,
}

impl China1688Connector {
    /// Build the browser-only connector. Disabled config is refused before
    /// any state exists.
    pub fn new(config: &ProfileConnectorConfig) -> Result<Self, ConfigError> {
        if !config.enabled {
            return Err(ConfigError::Disabled { connector: "1688" });
        }
        let profile = config
            .profile
            .clone()
            .unwrap_or_else(|| Text::<64>::new("procurement-cn").expect("literal"));
        Ok(Self {
            source: source_id(),
            profile,
            api: None,
            policy: FIRST_PARTY,
            health: Mutex::new(ExtractionHealth::new()),
            fingerprints: Mutex::new(BTreeMap::new()),
        })
    }

    /// Attach an Open Platform credential.
    ///
    /// Every named value is resolved through the injected provider and
    /// registered with the secret scanner. This constructor is fallible: a
    /// missing mandatory app key/app secret, or a configured value that
    /// cannot be resolved, is a typed [`ConfigError`] before any request
    /// exists. `refresher` is the optional token-refresh seam (production
    /// wiring rotates tokens via the environment and passes `None`).
    pub fn with_open_platform(
        mut self,
        config: &OpenPlatformConfig,
        credentials: Arc<dyn CredentialProvider>,
        secrets: Arc<SecretGuard>,
        refresher: Option<Arc<dyn TokenRefresher>>,
    ) -> Result<Self, ConfigError> {
        let app_key = crate::config::credential(
            credentials.as_ref(),
            secrets.as_ref(),
            config.app_key_env.as_str(),
        )?;
        let app_secret = crate::config::credential(
            credentials.as_ref(),
            secrets.as_ref(),
            config.app_secret_env.as_str(),
        )?;
        let access_token = resolve_access_token(config, credentials.as_ref(), secrets.as_ref())?;
        let refresh_token = match config.refresh_token_env.as_ref() {
            Some(env) => Some(crate::config::credential(
                credentials.as_ref(),
                secrets.as_ref(),
                env.as_str(),
            )?),
            None => None,
        };
        self.api = Some(OpenPlatform {
            auth: China1688Auth::new(
                app_key,
                app_secret,
                access_token,
                refresh_token,
                refresher,
                secrets,
            ),
            scopes: config.scopes,
        });
        Ok(self)
    }

    /// The installed access token's expiry, when a token is configured.
    pub fn access_token_expires_at_ms(&self) -> Option<u64> {
        self.api
            .as_ref()
            .and_then(|api| api.auth.token_expires_at_ms())
    }

    /// True when a valid, unexpired access token is installed now (with the
    /// documented skew).
    pub fn has_valid_access_token(&self, now_ms: u64) -> bool {
        self.api
            .as_ref()
            .is_some_and(|api| api.auth.has_valid_token(now_ms))
    }

    /// The advertised scope coverage (honest per-credential advertisement).
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

    /// The stable profile identity of this acquisition. It comes from the
    /// runtime wrapper's injected [`ConnectorIdentity`](crate::context::ConnectorIdentity)
    /// (P0 item 2), never from a request field; the connector's own configured
    /// profile name is only the fallback when the wrapper injected none.
    fn profile_identity(&self, ctx: &AcquireCtx) -> ProfileIdentity {
        let identity = ctx.identity();
        let configured = identity.profile();
        ProfileIdentity {
            source: self.source.clone(),
            profile: configured
                .map(|profile| profile.profile.clone())
                .unwrap_or_else(|| self.profile.clone()),
            account: identity.account_scope().cloned(),
            egress: configured
                .and_then(|profile| profile.egress.clone())
                .or_else(|| Text::<64>::new(EGRESS_LABEL).ok()),
        }
    }

    /// The advertised capabilities, computed from the granted API scope and
    /// the browser mechanisms.
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
            discovery: level(scopes.discovery),
            exact_product: level(scopes.product),
            quantity_pricing: level(scopes.price),
            stock: level(scopes.stock),
            packaging: CapabilityLevel::Fallback,
            account_pricing: CapabilityLevel::Unsupported,
            supplier_data: level(scopes.supplier),
            bulk: CapabilityLevel::Unsupported,
            // Honest advertisement: the official API mechanism exists only
            // when a credential is installed. A browser-only 1688 connector
            // advertises browser mechanisms only, so the planner selects the
            // browser because that is the mechanism it has, not because the
            // adapter decided to fall back.
            mechanisms: if self.api.is_some() {
                vec![
                    Mechanism::OfficialApi,
                    Mechanism::BrowserNetwork,
                    Mechanism::EmbeddedState,
                    Mechanism::Dom,
                ]
            } else {
                vec![
                    Mechanism::BrowserNetwork,
                    Mechanism::EmbeddedState,
                    Mechanism::Dom,
                ]
            },
        }
    }

    fn search_url(&self, query: &str) -> Result<CanonicalUrl, SourceError> {
        let expanded = dictionary::expand_query(query);
        CanonicalUrl::parse(&format!(
            "{BROWSER_SEARCH_URL}?keywords={}",
            http::query_escape(&expanded)
        ))
        .map_err(|_| SourceError::InvalidRequest)
    }

    fn detail_url(&self, offer_id: &str) -> Result<CanonicalUrl, SourceError> {
        let bounded = offer_id.trim();
        if bounded.is_empty() || bounded.len() > 64 {
            return Err(SourceError::InvalidRequest);
        }
        CanonicalUrl::parse(&format!("{BROWSER_DETAIL_PREFIX}{bounded}.html"))
            .map_err(|_| SourceError::InvalidRequest)
    }

    /// Build the signed Open Platform `param2` request for one lookup, when
    /// the credential grants discovery/product on the buyer surface.
    ///
    /// An account-scoped request without a valid unexpired access token is
    /// typed [`SourceError::AuthenticationRequired`] and never becomes a
    /// browser call.
    fn api_request(
        &self,
        ctx: &AcquireCtx,
        reference: Option<&str>,
        query: Option<&str>,
    ) -> Result<Option<HttpRequest>, SourceError> {
        let Some(api) = self.api.as_ref() else {
            return Ok(None);
        };
        // The runtime planner owns mechanism selection: when it chose a
        // browser mechanism, this adapter must not build (and must not
        // demand a token for) an API request. `None` is the legacy context
        // that lets the connector pick its own primary mechanism.
        if ctx.mechanism().is_some_and(|m| m != Mechanism::OfficialApi) {
            return Ok(None);
        }
        let built = match (reference, query) {
            (Some(offer_id), _) => auth::product_request(&api.auth, offer_id, ctx.now_ms()),
            (None, Some(query)) => {
                auth::search_request(&api.auth, &dictionary::expand_query(query), ctx.now_ms())
            }
            _ => return Ok(None),
        };
        match built {
            Ok(request) => Ok(Some(request)),
            Err(error) => {
                if error == SourceError::AuthenticationRequired {
                    ctx.record(
                        &self.source,
                        "api",
                        ConnectorEventKind::Note,
                        None,
                        Some("api_auth_required_no_browser_fallback"),
                    );
                }
                Err(error)
            }
        }
    }

    async fn api_drafts(
        &self,
        ctx: &AcquireCtx,
        request: HttpRequest,
        operation: &'static str,
    ) -> Result<Vec<extract::Draft>, SourceError> {
        let response = http::send(ctx, &self.source, operation, request).await?;
        http::map_status(&response)?;
        let body = response.body();
        if body.len() > crate::contract::capture::MAX_CAPTURE_BYTES {
            return Err(SourceError::ResponseTooLarge);
        }
        let text = String::from_utf8_lossy(body).into_owned();
        let expected = self.fingerprint(Strategy::NetworkJson);
        let mut drafts = extract::network_items(&text, &self.source, ctx.now_ms(), expected);
        // The API is the official surface: its observations carry that
        // origin while keeping the strongest strategy bucket.
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

    /// Collect the drafts for one page by executing exactly the mechanism the
    /// runtime planner chose. The API mechanism never touches the browser and
    /// the browser mechanisms never touch the API: no silent switch, no
    /// outage fallback manufactured inside the adapter.
    async fn collect(
        &self,
        ctx: &AcquireCtx,
        url: &CanonicalUrl,
        api_request: Option<HttpRequest>,
        needed: &[Field],
        operation: &'static str,
    ) -> Result<(Vec<extract::Draft>, AccessVisibility), SourceError> {
        ctx.check_alive()?;
        let mechanism = ctx.mechanism().ok_or(SourceError::InvalidRequest)?;
        match mechanism {
            Mechanism::OfficialApi => {
                let Some(api) = self.api.as_ref() else {
                    // The planner only chooses the API mechanism for a
                    // connector that advertises it, which requires an
                    // installed credential.
                    return Err(SourceError::InvalidRequest);
                };
                let Some(api_request) = api_request else {
                    return Err(SourceError::InvalidRequest);
                };
                let mut drafts = self.api_drafts(ctx, api_request, operation).await?;
                // Honest scope: the API answer is only used for fields the
                // granted scope actually covers; the gap is recorded, never
                // filled by an unplanned browser call.
                for draft in &mut drafts {
                    draft
                        .observations
                        .retain(|observation| api.scopes.covers(observation.field));
                    if !api.scopes.variants {
                        draft.variants.clear();
                    }
                }
                let uncovered: Vec<Field> = needed
                    .iter()
                    .copied()
                    .filter(|field| !api.scopes.covers(*field))
                    .collect();
                for draft in &drafts {
                    self.record_draft(draft, ctx);
                }
                if !uncovered.is_empty() {
                    let labels: Vec<&str> = uncovered.iter().map(|field| field.as_str()).collect();
                    ctx.record(
                        &self.source,
                        "api_scope",
                        ConnectorEventKind::Note,
                        None,
                        Some(&format!("api_scope_missing fields={}", labels.join(","))),
                    );
                }
                Ok((drafts, AccessVisibility::from_identity(ctx.identity())))
            }
            Mechanism::BrowserNetwork | Mechanism::EmbeddedState | Mechanism::Dom => {
                self.browser_drafts(ctx, url).await
            }
            Mechanism::DirectHttp => Err(SourceError::InvalidRequest),
        }
    }

    async fn browser_drafts(
        &self,
        ctx: &AcquireCtx,
        url: &CanonicalUrl,
    ) -> Result<(Vec<extract::Draft>, AccessVisibility), SourceError> {
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
        let access = bundle.access_visibility.clone();
        let drafts = self.drafts_from_bundle(ctx, bundle)?;
        Ok((drafts, access))
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
                    let item_drafts = extract::network_items(&text, &self.source, now_ms, expected);
                    for draft in item_drafts {
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
        let (url, api_request) = match reference {
            ProductReference::OfferId(offer_id) => (
                self.detail_url(offer_id.as_str())?,
                self.api_request(ctx, Some(offer_id.as_str()), None)?,
            ),
            ProductReference::Url(url) => (
                url.clone(),
                match offer_id_from_url(url) {
                    Some(id) => self.api_request(ctx, Some(&id), None)?,
                    None => None,
                },
            ),
            ProductReference::SourcePartNumber(part)
            | ProductReference::ManufacturerPartNumber(part) => (
                self.search_url(part.as_str())?,
                self.api_request(ctx, None, Some(part.as_str()))?,
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
        let (drafts, access) = self
            .collect(ctx, &url, api_request, &needed, operation)
            .await?;
        let drafts = first_offer_drafts(drafts);
        let assembly =
            normalize::assemble(&self.source, ctx, &url, &drafts, ctx.now_ms(), &access)?;
        self.record_conflicts(&assembly, ctx);
        Ok(assembly)
    }
}

/// The numeric offer id of a `detail.1688.com/offer/<id>.html` URL, when the
/// URL has that shape.
fn offer_id_from_url(url: &CanonicalUrl) -> Option<String> {
    let path = url
        .as_str()
        .split(['?', '#'])
        .next()
        .unwrap_or(url.as_str());
    let last = path.rsplit('/').find(|segment| !segment.is_empty())?;
    let id = last.strip_suffix(".html").unwrap_or(last);
    if !id.is_empty() && id.len() <= 32 && id.bytes().all(|byte| byte.is_ascii_digit()) {
        Some(id.to_string())
    } else {
        None
    }
}

/// Restrict search-page drafts to the first offer on the page, so one
/// product lookup never mixes fields from different listings.
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

/// The fields a strategy is expected to produce (used for health accounting
/// on not-applicable/schema-mismatch outcomes).
fn primary_fields(strategy: Strategy) -> &'static [Field] {
    match strategy {
        Strategy::NetworkJson | Strategy::EmbeddedState => &[
            Field::OfferId,
            Field::Title,
            Field::Price,
            Field::MinimumOrder,
            Field::Stock,
        ],
        Strategy::StructuralDom => &[Field::OfferId, Field::Title, Field::Price],
        Strategy::RenderedText => &[Field::Price],
    }
}

fn source_id() -> SourceId {
    SourceId::new("1688").expect("valid static source id")
}

impl std::fmt::Debug for China1688Connector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("China1688Connector")
            .field("source", &self.source)
            .field("profile", &self.profile)
            .field("api", &self.api.is_some())
            .finish()
    }
}

#[async_trait]
impl SiteConnector for China1688Connector {
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
        let api_request = if self.scopes().discovery {
            self.api_request(ctx, None, Some(req.query().as_str()))?
        } else {
            None
        };
        let (drafts, access) = self
            .collect(ctx, &url, api_request, &[Field::Title], "search")
            .await?;
        let mut discoveries: Vec<Discovery> = Vec::new();
        for draft in &drafts {
            if let Ok(assembly) = normalize::assemble(
                &self.source,
                ctx,
                &url,
                std::slice::from_ref(draft),
                ctx.now_ms(),
                &access,
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
                        // Ambiguous or missing variant mappings never fall
                        // back to a cheaper variant.
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
            // Account-specific prices are only applicable to the identity the
            // runtime configured; without it the quote resolves NoPrice.
            account: ctx.account_scope(),
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

/// The currency this connector prices in.
pub const CURRENCY: Currency = Currency::CNY;

/// A quantity of one (helper for offer construction in tests).
pub fn one() -> NonZeroQuantity {
    NonZeroQuantity::new(1).expect("one")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::capture::test_support::{self as capture_support, ScriptedCapture};
    use crate::contract::capture::{CaptureKind as Kind, ChallengeKind};
    use crate::testing::{CannedResponse, MapCredentials};
    use crate::testsupport::{text, Rig};

    fn fixture(relative: &str) -> String {
        crate::testing::fixture(relative).expect("fixture")
    }

    fn browser_only() -> China1688Connector {
        China1688Connector::new(&ProfileConnectorConfig {
            enabled: true,
            profile: Some(text("procurement-cn")),
        })
        .expect("connector")
    }

    fn ctx_with_mechanism(
        rig: &Rig,
        capture: Arc<ScriptedCapture>,
        mechanism: Mechanism,
    ) -> AcquireCtx {
        AcquireCtx::builder(
            rig.transport.clone(),
            rig.quota.clone(),
            rig.secrets.clone(),
        )
        .clock(rig.clock.clone())
        .diagnostics(rig.diagnostics.clone())
        .browser_extraction(capture)
        .mechanism(mechanism)
        .build()
    }

    /// Browser-mechanism context (the planner chose browser extraction).
    fn ctx_with(rig: &Rig, capture: Arc<ScriptedCapture>) -> AcquireCtx {
        ctx_with_mechanism(rig, capture, Mechanism::BrowserNetwork)
    }

    fn bundle_with(fixture_path: &str, kind: Kind, url: &str) -> CaptureBundle {
        capture_support::bundle(
            url,
            vec![capture_support::payload(kind, url, &fixture(fixture_path))],
        )
    }

    fn access_bundle(fixture_path: &str, kind: Kind, access: AccessVisibility) -> CaptureBundle {
        capture_support::bundle_with_access(
            "https://detail.1688.com/offer/678901234567.html",
            vec![capture_support::payload(
                kind,
                "https://detail.1688.com/offer/678901234567.html",
                &fixture(fixture_path),
            )],
            access,
        )
    }

    #[tokio::test]
    async fn capture_authority_decides_price_visibility_never_the_number() {
        use faktor_commerce::PriceVisibility;

        let rig = Rig::new();
        let capture = Arc::new(ScriptedCapture::new());
        capture.push(access_bundle(
            "china1688/network_offer_detail.json",
            Kind::NetworkJson,
            AccessVisibility::Authenticated,
        ));
        let buyer = faktor_commerce::text::AccountScope::new("buyer-a").expect("scope");
        capture.push(access_bundle(
            "china1688/network_offer_detail.json",
            Kind::NetworkJson,
            AccessVisibility::AccountScoped(buyer.clone()),
        ));
        let connector = browser_only();
        let ctx = ctx_with(&rig, capture);

        let authenticated = connector
            .product(&ctx, ProductRequest::new(url_reference()))
            .await
            .expect("authenticated offer");
        assert!(authenticated.cheapest_unit_price().is_some());
        assert_eq!(
            authenticated.price_visibility,
            PriceVisibility::Authenticated,
            "a numeric price under a logged-in capture is never Public"
        );
        for variant in &authenticated.variants {
            for price_break in &variant.price_breaks {
                assert_eq!(price_break.visibility, PriceVisibility::Authenticated);
                assert_eq!(price_break.account_scope, None);
            }
        }

        let scoped = connector
            .product(&ctx, ProductRequest::new(url_reference()))
            .await
            .expect("account-scoped offer");
        assert_eq!(scoped.price_visibility, PriceVisibility::AccountSpecific);
        let mut breaks = 0usize;
        for variant in &scoped.variants {
            for price_break in &variant.price_breaks {
                assert_eq!(price_break.visibility, PriceVisibility::AccountSpecific);
                assert_eq!(price_break.account_scope, Some(buyer.clone()));
                breaks += 1;
            }
        }
        for price_break in &scoped.price_breaks {
            assert_eq!(price_break.visibility, PriceVisibility::AccountSpecific);
            assert_eq!(price_break.account_scope, Some(buyer.clone()));
            breaks += 1;
        }
        assert!(breaks > 0, "the fixture must carry at least one price");
    }

    #[tokio::test]
    async fn an_inquiry_stays_inquiry_required_under_any_capture_authority() {
        use faktor_commerce::PriceVisibility;

        for access in [
            AccessVisibility::Anonymous,
            AccessVisibility::Authenticated,
            AccessVisibility::AccountScoped(
                faktor_commerce::text::AccountScope::new("buyer-a").expect("scope"),
            ),
        ] {
            let rig = Rig::new();
            let capture = Arc::new(ScriptedCapture::new());
            capture.push(access_bundle(
                "china1688/network_inquiry.json",
                Kind::NetworkJson,
                access,
            ));
            let connector = browser_only();
            let ctx = ctx_with(&rig, capture);
            let offer = connector
                .product(&ctx, ProductRequest::new(url_reference()))
                .await
                .expect("inquiry offer");
            assert_eq!(offer.price_visibility, PriceVisibility::InquiryRequired);
            assert!(offer.price_breaks.is_empty());
            assert!(offer.variants.iter().all(|v| v.price_breaks.is_empty()));
        }
    }

    fn detail_url() -> CanonicalUrl {
        CanonicalUrl::parse("https://detail.1688.com/offer/678901234567.html").expect("url")
    }

    fn url_reference() -> ProductReference {
        ProductReference::Url(detail_url())
    }

    #[tokio::test]
    async fn network_extraction_wins_and_reconciliation_records_the_conflict() {
        let rig = Rig::new();
        let capture = Arc::new(ScriptedCapture::new());
        capture.push(capture_support::bundle(
            "https://detail.1688.com/offer/678901234567.html",
            vec![
                capture_support::payload(
                    Kind::NetworkJson,
                    "https://detail.1688.com/api/offer.json",
                    &fixture("china1688/network_offer_detail.json"),
                ),
                capture_support::payload(
                    Kind::StructuredMarkup,
                    "https://detail.1688.com/offer/678901234567.html",
                    &fixture("china1688/offer_dom_stale.html"),
                ),
            ],
        ));
        let connector = browser_only();
        let ctx = ctx_with(&rig, capture.clone());
        let offer = connector
            .product(&ctx, ProductRequest::new(url_reference()))
            .await
            .expect("offer");
        // The network value wins; the stale DOM ¥2.00 is never selected and
        // never averaged: the expensive variant still carries ¥36.00 and the
        // offer-level price stays empty (variant-priced).
        assert!(offer.price_breaks.is_empty());
        let black = offer
            .variants
            .iter()
            .find(|variant| variant.variant_id.as_str() == "长度=1米;颜色=黑色")
            .expect("variant");
        assert_eq!(
            black.price_breaks[0].unit_price.to_decimal_string(),
            "36.000000"
        );
        assert_eq!(offer.title.as_str(), "USB 3.0 数据线 编织款");
        assert_eq!(offer.variants.len(), 3);
        let diagnostics = rig.diagnostics.joined();
        assert!(
            diagnostics.contains("extraction_conflict field=price authority=network_json"),
            "{diagnostics}"
        );
        let health = connector.extraction_health();
        assert!(health.stats(Field::Price, Strategy::StructuralDom).conflict >= 1);
    }

    #[tokio::test]
    async fn acceptance_variant_price_is_resolved_and_the_floor_is_never_quoted() {
        let rig = Rig::new();
        let capture = Arc::new(ScriptedCapture::new());
        for _ in 0..3 {
            capture.push(bundle_with(
                "china1688/network_offer_detail.json",
                Kind::NetworkJson,
                "https://detail.1688.com/offer/678901234567.html",
            ));
        }
        let connector = browser_only();
        let ctx = ctx_with(&rig, capture);

        // Requested variant ¥36.00 resolves to ¥36.00 (MOQ 2).
        let requested = VariantId::new("颜色:黑色;长度:1米").expect("requested variant id");
        let two = NonZeroQuantity::new(2).expect("quantity");
        let candidates = connector
            .quote(
                &ctx,
                QuoteRequest::new(url_reference(), two).with_variant(requested),
            )
            .await
            .expect("quote");
        assert_eq!(candidates.len(), 1);
        let resolution = &candidates[0].resolution;
        assert_eq!(resolution.status, faktor_commerce::QuoteStatus::Resolved);
        assert_eq!(
            resolution
                .unit_price
                .expect("unit price")
                .to_decimal_string(),
            "36.000000"
        );

        // Without a variant the resolution is ambiguous — never ¥2.00.
        let candidates = connector
            .quote(&ctx, QuoteRequest::new(url_reference(), two))
            .await
            .expect("quote");
        assert_eq!(
            candidates[0].resolution.status,
            faktor_commerce::QuoteStatus::VariantAmbiguous
        );
        assert_eq!(candidates[0].resolution.unit_price, None);

        // An unknown variant is typed, and never the cheapest SKU.
        let unknown = VariantId::new("颜色:蓝色").expect("variant id");
        let error = connector
            .quote(
                &ctx,
                QuoteRequest::new(url_reference(), two).with_variant(unknown),
            )
            .await
            .expect_err("unknown variant");
        assert_eq!(error, SourceError::VariantAmbiguous);
    }

    #[tokio::test]
    async fn strategies_fall_through_network_then_embedded_then_dom_then_text() {
        let rig = Rig::new();
        let capture = Arc::new(ScriptedCapture::new());
        capture.push(bundle_with(
            "china1688/embed_state.html",
            Kind::EmbeddedState,
            "https://detail.1688.com/offer/678901234567.html",
        ));
        capture.push(bundle_with(
            "china1688/offer_dom.html",
            Kind::StructuredMarkup,
            "https://detail.1688.com/offer/678901234567.html",
        ));
        capture.push(bundle_with(
            "china1688/offer_rendered.txt",
            Kind::RenderedText,
            "https://detail.1688.com/offer/678901234567.html",
        ));
        let connector = browser_only();
        let ctx = ctx_with(&rig, capture.clone());

        for _ in 0..3 {
            let offer = connector
                .product(&ctx, ProductRequest::new(url_reference()))
                .await
                .expect("offer from a single strategy");
            assert_eq!(offer.title.as_str(), "USB 3.0 数据线 编织款");
            assert!(offer.cheapest_unit_price().is_some());
        }
        assert_eq!(capture.call_count(), 3);
        let health = connector.extraction_health();
        assert!(health.stats(Field::Title, Strategy::EmbeddedState).success >= 1);
        assert!(health.stats(Field::Title, Strategy::StructuralDom).success >= 1);
        assert!(health.stats(Field::Price, Strategy::RenderedText).success >= 1);
    }

    #[tokio::test]
    async fn a_payload_that_does_not_apply_is_not_applicable() {
        let rig = Rig::new();
        let capture = Arc::new(ScriptedCapture::new());
        capture.push(capture_support::bundle(
            "https://detail.1688.com/offer/678901234567.html",
            vec![
                capture_support::payload(
                    Kind::NetworkJson,
                    "https://detail.1688.com/api/offer.json",
                    &fixture("china1688/network_offer_detail.json"),
                ),
                capture_support::payload(
                    Kind::StructuredMarkup,
                    "https://detail.1688.com/offer/678901234567.html",
                    "<html><body><div class=\"search-page\">not an offer page</div></body></html>",
                ),
            ],
        ));
        let connector = browser_only();
        let ctx = ctx_with(&rig, capture);
        let offer = connector
            .product(&ctx, ProductRequest::new(url_reference()))
            .await
            .expect("offer from the network payload");
        assert_eq!(offer.title.as_str(), "USB 3.0 数据线 编织款");
        let health = connector.extraction_health();
        assert!(
            health
                .stats(Field::Title, Strategy::StructuralDom)
                .not_applicable
                >= 1,
            "a non-applying payload is a strategy outcome, not an error"
        );
    }

    #[tokio::test]
    async fn challenge_stops_the_profile_with_a_typed_error() {
        let rig = Rig::new();
        let capture = Arc::new(ScriptedCapture::new());
        capture.push(capture_support::challenged(
            "https://detail.1688.com/offer/678901234567.html",
            ChallengeKind::Captcha,
            "验证码",
        ));
        let connector = browser_only();
        let ctx = ctx_with(&rig, capture.clone());
        let error = connector
            .product(&ctx, ProductRequest::new(url_reference()))
            .await
            .expect_err("challenge");
        assert_eq!(
            error,
            SourceError::VerificationRequired {
                kind: faktor_commerce::error::VerificationKind::Captcha
            }
        );
        assert_eq!(capture.call_count(), 1, "a challenge is never retried");
        assert!(rig
            .diagnostics
            .joined()
            .contains("profile_stop kind=captcha"));
    }

    #[tokio::test]
    async fn login_and_rate_limit_challenges_map_to_their_typed_states() {
        let rig = Rig::new();
        let capture = Arc::new(ScriptedCapture::new());
        capture.push(capture_support::bundle(
            "https://detail.1688.com/offer/678901234567.html",
            vec![capture_support::payload(
                Kind::StructuredMarkup,
                "https://detail.1688.com/offer/678901234567.html",
                &fixture("china1688/challenge_login.html"),
            )],
        ));
        capture.push(capture_support::bundle(
            "https://detail.1688.com/offer/678901234567.html",
            vec![capture_support::payload(
                Kind::RenderedText,
                "https://detail.1688.com/offer/678901234567.html",
                &fixture("china1688/challenge_rate_limit.txt"),
            )],
        ));
        let connector = browser_only();
        let ctx = ctx_with(&rig, capture);
        let login = connector
            .product(&ctx, ProductRequest::new(url_reference()))
            .await
            .expect_err("login");
        assert_eq!(
            login,
            SourceError::VerificationRequired {
                kind: faktor_commerce::error::VerificationKind::Login
            }
        );
        let rate = connector
            .product(&ctx, ProductRequest::new(url_reference()))
            .await
            .expect_err("rate limit");
        assert!(matches!(rate, SourceError::RateLimited { .. }));
    }

    #[tokio::test]
    async fn third_party_payloads_never_become_prices() {
        let rig = Rig::new();
        let capture = Arc::new(ScriptedCapture::new());
        capture.push(capture_support::bundle(
            "https://detail.1688.com/offer/678901234567.html",
            vec![capture_support::payload(
                Kind::NetworkJson,
                "https://tracker.test/price.json",
                r#"{"data":{"offerId":"1","subject":"fake","price":"2.00"}}"#,
            )],
        ));
        let connector = browser_only();
        let ctx = ctx_with(&rig, capture);
        let error = connector
            .product(&ctx, ProductRequest::new(url_reference()))
            .await
            .expect_err("no first-party data");
        assert_eq!(error, SourceError::ExtractionIncomplete);
        assert!(rig.diagnostics.joined().contains("third_party_dropped"));
    }

    #[tokio::test]
    async fn schema_drift_is_typed_and_never_misparsed() {
        let rig = Rig::new();
        let capture = Arc::new(ScriptedCapture::new());
        capture.push(bundle_with(
            "china1688/network_offer_detail.json",
            Kind::NetworkJson,
            "https://detail.1688.com/offer/678901234567.html",
        ));
        capture.push(bundle_with(
            "china1688/schema_drift.json",
            Kind::NetworkJson,
            "https://detail.1688.com/offer/678901234567.html",
        ));
        let connector = browser_only();
        let ctx = ctx_with(&rig, capture);
        connector
            .product(&ctx, ProductRequest::new(url_reference()))
            .await
            .expect("first parse");
        let error = connector
            .product(&ctx, ProductRequest::new(url_reference()))
            .await
            .expect_err("drift");
        assert_eq!(error, SourceError::ExtractionIncomplete);
        assert!(rig.diagnostics.joined().contains("schema_drift"));
        let health = connector.extraction_health();
        assert!(
            health
                .stats(Field::Price, Strategy::NetworkJson)
                .schema_mismatch
                >= 1
        );
    }

    #[tokio::test]
    async fn inquiry_pricing_is_null_and_typed() {
        let rig = Rig::new();
        let capture = Arc::new(ScriptedCapture::new());
        for _ in 0..2 {
            capture.push(bundle_with(
                "china1688/network_inquiry.json",
                Kind::NetworkJson,
                "https://detail.1688.com/offer/678901234567.html",
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
        assert!(offer.cheapest_unit_price().is_none());
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

    const CN_KEY_ENV: &str = "FAKTOR_1688_APP_KEY";
    const CN_SECRET_ENV: &str = "FAKTOR_1688_APP_SECRET";
    const CN_TOKEN_ENV: &str = "FAKTOR_1688_ACCESS_TOKEN";
    const CN_EXPIRES_ENV: &str = "FAKTOR_1688_ACCESS_TOKEN_EXPIRES_AT";
    const CN_KEY: &str = "1688-sanitized-key";
    const CN_SECRET: &str = "1688-sanitized-app-secret";
    const CN_TOKEN: &str = "1688-sanitized-access-token";

    fn golden_cn_credentials(expires_at_ms: u64) -> (Arc<MapCredentials>, Arc<SecretGuard>) {
        (
            Arc::new(
                MapCredentials::new()
                    .with(CN_KEY_ENV, CN_KEY)
                    .with(CN_SECRET_ENV, CN_SECRET)
                    .with(CN_TOKEN_ENV, CN_TOKEN)
                    .with(CN_EXPIRES_ENV, &expires_at_ms.to_string()),
            ),
            Arc::new(SecretGuard::new()),
        )
    }

    fn cn_config(scopes: ApiScopes) -> OpenPlatformConfig {
        OpenPlatformConfig::new(CN_KEY_ENV, CN_SECRET_ENV, scopes)
            .expect("config")
            .with_access_token(CN_TOKEN_ENV, CN_EXPIRES_ENV)
            .expect("access token config")
    }

    #[tokio::test]
    async fn open_platform_scope_is_advertised_honestly() {
        let (credentials, secrets) =
            golden_cn_credentials(crate::testsupport::TEST_NOW_MS + 3_600_000);
        let connector = browser_only()
            .with_open_platform(
                &cn_config(ApiScopes {
                    discovery: true,
                    product: true,
                    price: true,
                    variants: true,
                    moq: true,
                    ..ApiScopes::default()
                }),
                credentials,
                secrets,
                None,
            )
            .expect("api connector");
        let capabilities = connector.capabilities();
        assert_eq!(
            capabilities.level(crate::contract::Capability::Discovery),
            CapabilityLevel::Supported
        );
        assert_eq!(
            capabilities.level(crate::contract::Capability::Stock),
            CapabilityLevel::Fallback
        );
        assert_eq!(
            capabilities.supplier_data,
            CapabilityLevel::Fallback,
            "uncovered scope is never advertised as supported"
        );
        assert_eq!(
            connector.scopes().labels(),
            vec!["discovery", "product", "price", "variants", "moq"]
        );
    }

    #[tokio::test]
    async fn open_platform_with_full_scope_answers_alone() {
        let rig = Rig::new();
        let (credentials, secrets) =
            golden_cn_credentials(crate::testsupport::TEST_NOW_MS + 3_600_000);
        let connector = browser_only()
            .with_open_platform(&cn_config(ApiScopes::all()), credentials, secrets, None)
            .expect("api connector");
        assert!(connector.has_valid_access_token(crate::testsupport::TEST_NOW_MS));
        rig.transport.push(CannedResponse::new(
            200,
            fixture("china1688/api_offer_detail.json").into_bytes(),
        ));
        let capture = Arc::new(ScriptedCapture::new());
        let ctx = ctx_with_mechanism(&rig, capture.clone(), Mechanism::OfficialApi);
        let offer = connector
            .product(&ctx, ProductRequest::new(url_reference()))
            .await
            .expect("api offer");
        assert_eq!(offer.title.as_str(), "USB 3.0 数据线 编织款");
        assert_eq!(offer.variants.len(), 3);
        assert!(
            offer.supplier.is_some(),
            "a covered field comes from the API"
        );
        assert_eq!(capture.call_count(), 0, "no browser when scope suffices");
        let url = rig.transport.request_url(0).expect("request url");
        assert!(url.contains("gw.open.1688.com"), "{url}");
        assert!(url.ends_with("/1688-sanitized-key"), "{url}");
        assert!(!url.contains(CN_TOKEN), "the token must not be URL-visible");
        assert!(!url.contains(CN_SECRET));
        let body = rig.transport.request_body(0).expect("body");
        assert!(
            !body.contains(CN_SECRET),
            "the app secret is never transmitted"
        );
        // The wire signature is the independently generated golden vector
        // (`fixtures/china1688/signing_golden.json`, vector `offer_detail`).
        assert!(
            body.contains("_aop_signature=3F6B0600A0D4BD6E9FA45F9871FB9A757C96F578"),
            "{body}"
        );
    }

    #[tokio::test]
    async fn account_scoped_call_without_a_token_never_falls_back_to_the_browser() {
        let rig = Rig::new();
        let credentials = Arc::new(
            MapCredentials::new()
                .with(CN_KEY_ENV, CN_KEY)
                .with(CN_SECRET_ENV, CN_SECRET),
        );
        let connector = browser_only()
            .with_open_platform(
                &OpenPlatformConfig::new(CN_KEY_ENV, CN_SECRET_ENV, ApiScopes::all())
                    .expect("config"),
                credentials,
                rig.secrets.clone(),
                None,
            )
            .expect("api connector");
        let capture = Arc::new(ScriptedCapture::new());
        capture.push(bundle_with(
            "china1688/network_offer_detail.json",
            Kind::NetworkJson,
            "https://detail.1688.com/offer/678901234567.html",
        ));
        let ctx = ctx_with_mechanism(&rig, capture.clone(), Mechanism::OfficialApi);
        assert_eq!(
            connector
                .product(&ctx, ProductRequest::new(url_reference()))
                .await
                .expect_err("no token"),
            SourceError::AuthenticationRequired
        );
        assert_eq!(capture.call_count(), 0);
        assert_eq!(rig.transport.request_count(), 0);
        assert!(rig
            .diagnostics
            .joined()
            .contains("api_auth_required_no_browser_fallback"));
    }

    #[tokio::test]
    async fn a_planner_browser_mechanism_never_demands_api_auth() {
        let rig = Rig::new();
        let credentials = Arc::new(
            MapCredentials::new()
                .with(CN_KEY_ENV, CN_KEY)
                .with(CN_SECRET_ENV, CN_SECRET),
        );
        let connector = browser_only()
            .with_open_platform(
                &OpenPlatformConfig::new(CN_KEY_ENV, CN_SECRET_ENV, ApiScopes::all())
                    .expect("config"),
                credentials,
                rig.secrets.clone(),
                None,
            )
            .expect("api connector");
        // No access token is configured, but the planner chose browser
        // extraction: the adapter must not demand API auth for it.
        let capture = Arc::new(ScriptedCapture::new());
        capture.push(bundle_with(
            "china1688/network_offer_detail.json",
            Kind::NetworkJson,
            "https://detail.1688.com/offer/678901234567.html",
        ));
        let ctx = ctx_with(&rig, capture.clone());
        let offer = connector
            .product(&ctx, ProductRequest::new(url_reference()))
            .await
            .expect("browser offer without an API token");
        assert_eq!(offer.title.as_str(), "USB 3.0 数据线 编织款");
        assert_eq!(capture.call_count(), 1);
        assert_eq!(rig.transport.request_count(), 0);
        assert!(!rig.diagnostics.joined().contains("api_auth_required"));
    }

    #[tokio::test]
    async fn an_expired_token_is_typed_and_never_falls_back_to_the_browser() {
        let rig = Rig::new();
        let (credentials, secrets) = golden_cn_credentials(crate::testsupport::TEST_NOW_MS - 1);
        let connector = browser_only()
            .with_open_platform(&cn_config(ApiScopes::all()), credentials, secrets, None)
            .expect("api connector");
        assert!(!connector.has_valid_access_token(crate::testsupport::TEST_NOW_MS));
        assert_eq!(
            connector.access_token_expires_at_ms(),
            Some(crate::testsupport::TEST_NOW_MS - 1)
        );
        let capture = Arc::new(ScriptedCapture::new());
        capture.push(bundle_with(
            "china1688/network_offer_detail.json",
            Kind::NetworkJson,
            "https://detail.1688.com/offer/678901234567.html",
        ));
        let ctx = ctx_with_mechanism(&rig, capture.clone(), Mechanism::OfficialApi);
        assert_eq!(
            connector
                .product(&ctx, ProductRequest::new(url_reference()))
                .await
                .expect_err("expired token"),
            SourceError::AuthenticationRequired
        );
        assert_eq!(capture.call_count(), 0);
        assert_eq!(rig.transport.request_count(), 0);
    }

    #[tokio::test]
    async fn an_uncovered_scope_field_is_not_guessed() {
        let rig = Rig::new();
        let (credentials, secrets) =
            golden_cn_credentials(crate::testsupport::TEST_NOW_MS + 3_600_000);
        let connector = browser_only()
            .with_open_platform(
                &cn_config(ApiScopes {
                    discovery: true,
                    product: true,
                    price: true,
                    ..ApiScopes::default()
                }),
                credentials,
                secrets,
                None,
            )
            .expect("api connector");
        rig.transport.push(CannedResponse::new(
            200,
            fixture("china1688/api_offer_detail.json").into_bytes(),
        ));
        let capture = Arc::new(ScriptedCapture::new());
        let ctx = ctx_with_mechanism(&rig, capture.clone(), Mechanism::OfficialApi);
        let offer = connector
            .product(&ctx, ProductRequest::new(url_reference()))
            .await
            .expect("partial api offer");
        // The API payload carries a seller name, but the scope does not
        // cover supplier data: the field is said to be missing instead.
        assert_eq!(offer.supplier, None);
        assert!(offer.variants.is_empty(), "variant scope is not granted");
        assert!(rig.diagnostics.joined().contains("api_scope_missing"));
        assert!(offer.cheapest_unit_price().is_some());
    }

    #[tokio::test]
    async fn api_mechanism_never_falls_back_to_the_browser() {
        // A 500 under the API mechanism surfaces typed; the browser capture
        // is never consulted even though it is injected.
        for status in [500u16, 401, 429] {
            let rig = Rig::new();
            let (credentials, secrets) =
                golden_cn_credentials(crate::testsupport::TEST_NOW_MS + 3_600_000);
            let connector = browser_only()
                .with_open_platform(&cn_config(ApiScopes::all()), credentials, secrets, None)
                .expect("api connector");
            rig.transport
                .push(CannedResponse::new(status, b"{}".to_vec()));
            let capture = Arc::new(ScriptedCapture::new());
            capture.push(bundle_with(
                "china1688/network_offer_detail.json",
                Kind::NetworkJson,
                "https://detail.1688.com/offer/678901234567.html",
            ));
            let ctx = ctx_with_mechanism(&rig, capture.clone(), Mechanism::OfficialApi);
            let error = connector
                .product(&ctx, ProductRequest::new(url_reference()))
                .await
                .expect_err("api failure surfaces");
            assert!(
                matches!(
                    error,
                    SourceError::ApiUnavailable
                        | SourceError::AuthenticationRequired
                        | SourceError::RateLimited { .. }
                ),
                "status {status}: {error:?}"
            );
            assert_eq!(capture.call_count(), 0, "status {status}: no browsing");
        }

        // The browser mechanism executes the browser and never the API.
        let rig = Rig::new();
        let (credentials, secrets) =
            golden_cn_credentials(crate::testsupport::TEST_NOW_MS + 3_600_000);
        let connector = browser_only()
            .with_open_platform(&cn_config(ApiScopes::all()), credentials, secrets, None)
            .expect("api connector");
        let capture = Arc::new(ScriptedCapture::new());
        capture.push(bundle_with(
            "china1688/network_offer_detail.json",
            Kind::NetworkJson,
            "https://detail.1688.com/offer/678901234567.html",
        ));
        let ctx = ctx_with(&rig, capture.clone());
        let offer = connector
            .product(&ctx, ProductRequest::new(url_reference()))
            .await
            .expect("browser offer");
        assert_eq!(offer.title.as_str(), "USB 3.0 数据线 编织款");
        assert_eq!(capture.call_count(), 1);
        assert_eq!(rig.transport.request_count(), 0, "no API request");
    }

    #[tokio::test]
    async fn discovery_expands_deterministically_and_finds_items() {
        let rig = Rig::new();
        let capture = Arc::new(ScriptedCapture::new());
        capture.push(capture_support::bundle(
            "https://s.1688.com/selloffer/offer_search.htm?keywords=USB",
            vec![capture_support::payload(
                Kind::NetworkJson,
                "https://s.1688.com/api/search.json",
                &fixture("china1688/network_search.json"),
            )],
        ));
        let connector = browser_only();
        let ctx = ctx_with(&rig, capture.clone());
        let discoveries = connector
            .discover(
                &ctx,
                SearchRequest::new(text("数据线"), 5, None).expect("search"),
            )
            .await
            .expect("discover");
        assert_eq!(discoveries.len(), 2);
        assert_eq!(discoveries[0].title.as_str(), "USB 3.0 数据线 编织款");
        let (url, profile) = capture.calls()[0].clone();
        assert!(url.contains("keywords="), "{url}");
        assert_eq!(profile, "procurement-cn");
    }

    #[tokio::test]
    async fn profile_identity_is_stable_across_calls() {
        let rig = Rig::new();
        let capture = Arc::new(ScriptedCapture::new());
        for _ in 0..2 {
            capture.push(bundle_with(
                "china1688/offer_dom.html",
                Kind::StructuredMarkup,
                "https://detail.1688.com/offer/678901234567.html",
            ));
        }
        let connector = browser_only();
        let ctx = ctx_with(&rig, capture.clone());
        for _ in 0..2 {
            connector
                .product(&ctx, ProductRequest::new(url_reference()))
                .await
                .expect("offer");
        }
        let calls = capture.calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(
            calls[0].1, calls[1].1,
            "the profile never changes per request"
        );
    }

    #[tokio::test]
    async fn malformed_and_hostile_payloads_are_typed() {
        for (fixture_path, expected_incomplete) in [
            ("china1688/malformed.json", true),
            ("china1688/hostile.json", true),
        ] {
            let rig = Rig::new();
            let capture = Arc::new(ScriptedCapture::new());
            capture.push(bundle_with(
                fixture_path,
                Kind::NetworkJson,
                "https://detail.1688.com/offer/678901234567.html",
            ));
            let connector = browser_only();
            let ctx = ctx_with(&rig, capture);
            let error = connector
                .product(&ctx, ProductRequest::new(url_reference()))
                .await
                .expect_err(fixture_path);
            if expected_incomplete {
                assert!(
                    matches!(
                        error,
                        SourceError::ExtractionIncomplete | SourceError::ResponseTooLarge
                    ),
                    "{fixture_path}: {error:?}"
                );
            }
        }
    }

    #[test]
    fn disabled_config_never_constructs() {
        let config = ProfileConnectorConfig {
            enabled: false,
            profile: None,
        };
        match China1688Connector::new(&config) {
            Err(ConfigError::Disabled { connector }) => assert_eq!(connector, "1688"),
            _ => panic!("disabled config must be refused"),
        }
    }

    #[test]
    fn currency_is_cny() {
        assert_eq!(CURRENCY, Currency::CNY);
    }
}
