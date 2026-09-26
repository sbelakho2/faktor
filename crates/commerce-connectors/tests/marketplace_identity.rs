//! Release-blocking semantics for P0 item 2 (connector identity seam) and
//! P1 item 8 (capture access visibility) of the marketplace audit.
//!
//! Every test here is adversarial: it tries to make an authenticated or
//! account-scoped observation look public, to smuggle an account identity in
//! through a model/tool request, or to read an account-specific price with
//! the wrong (or no) account. All of them must fail closed.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use faktor_commerce::cache::{CacheClass, CacheIdentity, PricingScope};
use faktor_commerce::connector::{
    AcquireCtx as CommerceCtx, AcquisitionMechanism, ConnectorRegistry, ProfileIdentity,
};
use faktor_commerce::query::{
    DetailLevel, FreshnessMode, ProductRef, ProductRequest as CommerceProductRequest,
};
use faktor_commerce::text::AccountScope;
use faktor_commerce::{PriceVisibility, SourceId};

use faktor_commerce_connectors::config::ProfileConnectorConfig;
use faktor_commerce_connectors::contract::{
    Mechanism, ProductReference, ProductRequest, QuoteRequest, SiteConnector,
};
use faktor_commerce_connectors::testing::{fixture, NoopTransport};
use faktor_commerce_connectors::{
    AccessVisibility, AcquireCtx, BrowserExtraction, CaptureBundle, CaptureKind, CapturedPayload,
    China1688Connector, ConnectorIdentity, ConnectorRuntime, ManualClock, MemoryDiagnostics,
    QuotaState, Registered, SecretGuard, SourceError, Text, VisibilityScopeError, CN_EGRESS_LABEL,
};

fn text<const MAX: usize>(raw: &str) -> Text<MAX> {
    Text::<MAX>::new(raw).expect("bounded test text")
}

fn scope(raw: &str) -> AccountScope {
    AccountScope::new(raw).expect("scope")
}

fn detail_url() -> faktor_commerce_connectors::CanonicalUrl {
    faktor_commerce_connectors::CanonicalUrl::parse(
        "https://detail.1688.com/offer/678901234567.html",
    )
    .expect("url")
}

fn reference() -> ProductReference {
    ProductReference::Url(detail_url())
}

fn browser_connector() -> China1688Connector {
    China1688Connector::new(&ProfileConnectorConfig {
        enabled: true,
        profile: Some(text("procurement-cn")),
    })
    .expect("browser-only connector")
}

/// A scripted browser authority that records the exact `ProfileIdentity` the
/// connector handed to the seam before replaying one queued bundle.
struct RecordingCapture {
    state: Mutex<CaptureState>,
}

struct CaptureState {
    queue: VecDeque<CaptureBundle>,
    profiles: Vec<ProfileIdentity>,
}

impl RecordingCapture {
    fn new(bundle: CaptureBundle) -> Self {
        Self::with_bundles(vec![bundle])
    }

    fn with_bundles(bundles: Vec<CaptureBundle>) -> Self {
        Self {
            state: Mutex::new(CaptureState {
                queue: VecDeque::from(bundles),
                profiles: Vec::new(),
            }),
        }
    }

    fn profiles(&self) -> Vec<ProfileIdentity> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .profiles
            .clone()
    }
}

#[async_trait]
impl BrowserExtraction for RecordingCapture {
    async fn capture(
        &self,
        _ctx: &AcquireCtx,
        _source: &SourceId,
        profile: &ProfileIdentity,
        url: &faktor_commerce_connectors::CanonicalUrl,
    ) -> Result<CaptureBundle, SourceError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.profiles.push(profile.clone());
        match state.queue.pop_front() {
            Some(mut bundle) => {
                bundle.url = url.clone();
                Ok(bundle)
            }
            None => Err(SourceError::BrowserUnavailable),
        }
    }
}

/// One captured payload from a fixture.
fn payload(kind: CaptureKind, url: &str, fixture_path: &str) -> CapturedPayload {
    CapturedPayload::new(
        kind,
        faktor_commerce_connectors::CanonicalUrl::parse(url).expect("payload url"),
        None,
        fixture(fixture_path).expect("fixture").into_bytes(),
        1_700_000_000_000,
    )
    .expect("payload")
}

fn dom_bundle(access: AccessVisibility) -> CaptureBundle {
    let url = detail_url();
    CaptureBundle::new_with_access(
        url.clone(),
        vec![payload(
            CaptureKind::StructuredMarkup,
            url.as_str(),
            "china1688/offer_dom.html",
        )],
        None,
        1_700_000_000_000,
        access,
    )
    .expect("bundle")
}

fn variant_bundle(access: AccessVisibility) -> CaptureBundle {
    let url = detail_url();
    CaptureBundle::new_with_access(
        url.clone(),
        vec![payload(
            CaptureKind::NetworkJson,
            "https://detail.1688.com/api/offer.json",
            "china1688/network_offer_detail.json",
        )],
        None,
        1_700_000_000_000,
        access,
    )
    .expect("bundle")
}

fn ctx_with_identity(capture: Arc<RecordingCapture>, identity: ConnectorIdentity) -> AcquireCtx {
    AcquireCtx::builder(
        Arc::new(NoopTransport),
        Arc::new(QuotaState::new()),
        Arc::new(SecretGuard::new()),
    )
    .clock(Arc::new(ManualClock::new(1_700_000_000_000)))
    .diagnostics(Arc::new(MemoryDiagnostics::new()))
    .browser_extraction(capture)
    .mechanism(Mechanism::BrowserNetwork)
    .identity(identity)
    .build()
}

#[tokio::test]
async fn anonymous_capture_is_public_but_never_inferred_for_a_numeric_price_alone() {
    let connector = browser_connector();

    // Anonymous: a numeric price is public.
    let capture = Arc::new(RecordingCapture::new(variant_bundle(
        AccessVisibility::Anonymous,
    )));
    let offer = connector
        .product(
            &ctx_with_identity(capture, ConnectorIdentity::anonymous()),
            ProductRequest::new(reference()),
        )
        .await
        .expect("anonymous offer");
    assert_eq!(offer.price_visibility, PriceVisibility::Public);
    for variant in &offer.variants {
        for price_break in &variant.price_breaks {
            assert_eq!(price_break.visibility, PriceVisibility::Public);
            assert_eq!(price_break.account_scope, None);
        }
    }

    // The SAME numeric price (¥36.00) observed through a logged-in profile
    // must never be public just because the number exists.
    let capture = Arc::new(RecordingCapture::new(variant_bundle(
        AccessVisibility::Authenticated,
    )));
    let offer = connector
        .product(
            &ctx_with_identity(capture, ConnectorIdentity::anonymous()),
            ProductRequest::new(reference()),
        )
        .await
        .expect("authenticated offer");
    assert_eq!(offer.price_visibility, PriceVisibility::Authenticated);
    assert!(
        offer.cheapest_unit_price().is_some(),
        "the test fixture must contain a numeric price"
    );
    for variant in &offer.variants {
        for price_break in &variant.price_breaks {
            assert_eq!(
                price_break.visibility,
                PriceVisibility::Authenticated,
                "a numeric price under an authenticated capture is never Public"
            );
            assert_eq!(price_break.account_scope, None);
        }
    }
}

#[tokio::test]
async fn account_scoped_capture_carries_its_scope_and_refuses_to_be_read_without_it() {
    let connector = browser_connector();
    let buyer = scope("buyer-a");
    let access = AccessVisibility::AccountScoped(buyer.clone());
    let capture = Arc::new(RecordingCapture::with_bundles(vec![
        dom_bundle(access.clone()),
        dom_bundle(access),
    ]));
    let anonymous_ctx = ctx_with_identity(capture.clone(), ConnectorIdentity::anonymous());

    let offer = connector
        .product(&anonymous_ctx, ProductRequest::new(reference()))
        .await
        .expect("account-scoped offer");
    assert_eq!(offer.price_visibility, PriceVisibility::AccountSpecific);
    assert!(
        !offer.variants.is_empty(),
        "fixture is variant-priced; scoping must reach every break"
    );
    for variant in &offer.variants {
        for price_break in &variant.price_breaks {
            assert_eq!(price_break.visibility, PriceVisibility::AccountSpecific);
            assert_eq!(price_break.account_scope, Some(buyer.clone()));
        }
    }

    // The account-specific price read with account=None must not resolve.
    let variant =
        faktor_commerce_connectors::VariantId::new("长度=1米;颜色=黑色").expect("variant id");
    let anonymous_quote = connector
        .quote(
            &anonymous_ctx,
            QuoteRequest::new(
                reference(),
                faktor_commerce_connectors::NonZeroQuantity::new(2).expect("qty"),
            )
            .with_variant(variant.clone()),
        )
        .await
        .expect("quote executes");
    assert_eq!(
        anonymous_quote[0].resolution.status,
        faktor_commerce::QuoteStatus::NoPrice,
        "an account-specific price is never readable without the account"
    );
    assert_eq!(anonymous_quote[0].resolution.unit_price, None);

    // With the configured identity's account the quote resolves.
    let capture = Arc::new(RecordingCapture::new(dom_bundle(
        AccessVisibility::AccountScoped(buyer.clone()),
    )));
    let scoped_ctx = ctx_with_identity(
        capture,
        ConnectorIdentity::anonymous().with_account_scope(buyer.clone()),
    );
    let scoped_quote = connector
        .quote(
            &scoped_ctx,
            QuoteRequest::new(
                reference(),
                faktor_commerce_connectors::NonZeroQuantity::new(2).expect("qty"),
            )
            .with_variant(variant),
        )
        .await
        .expect("quote executes");
    assert_eq!(
        scoped_quote[0].resolution.status,
        faktor_commerce::QuoteStatus::Resolved
    );
    let applied = scoped_quote[0]
        .resolution
        .tier
        .as_ref()
        .expect("applied tier");
    assert_eq!(applied.account_scope, Some(buyer));
}

#[tokio::test]
async fn cache_identity_isolates_profiles_and_never_degrades_authenticated_to_public() {
    fn identity_for(access: &AccessVisibility, product: &str) -> CacheIdentity {
        CacheIdentity {
            class: CacheClass::Product,
            source: SourceId::new("1688").expect("source"),
            product: Text::new(product).expect("product key"),
            account_scope: access.price_account_scope(),
            locale: None,
            market: None,
            currency: None,
            quantity: None,
            packaging: None,
            variant: None,
            pricing_scope: access.pricing_scope(),
        }
    }

    let anonymous = AccessVisibility::Anonymous;
    let authenticated = AccessVisibility::Authenticated;
    let buyer_a = AccessVisibility::AccountScoped(scope("buyer-a"));
    let buyer_b = AccessVisibility::AccountScoped(scope("buyer-b"));
    let product = "offer:678901234567";

    // Anonymous vs authenticated must never share a cache identity.
    assert_ne!(
        identity_for(&anonymous, product).digest(),
        identity_for(&authenticated, product).digest(),
        "an authenticated observation must not collide with a public one"
    );
    // Anonymous vs account-scoped, and two account scopes, differ too.
    assert_ne!(
        identity_for(&anonymous, product).digest(),
        identity_for(&buyer_a, product).digest()
    );
    assert_ne!(
        identity_for(&buyer_a, product).digest(),
        identity_for(&buyer_b, product).digest(),
        "the same product under profile A and profile B is not one cache entry"
    );

    // The typed refusal: an authenticated observation can never be admitted
    // under the public pricing scope.
    assert_eq!(
        authenticated.admit_cache_scope(PricingScope::Public),
        Err(VisibilityScopeError::AuthenticatedNeverPublic)
    );
    assert_eq!(
        buyer_a.admit_cache_scope(PricingScope::Public),
        Err(VisibilityScopeError::AccountScopedNeverPublic)
    );
    assert_eq!(
        authenticated.admit_cache_scope(PricingScope::Authenticated),
        Ok(PricingScope::Authenticated)
    );
    assert_eq!(
        anonymous.admit_cache_scope(PricingScope::Public),
        Ok(PricingScope::Public)
    );
    assert_ne!(authenticated.pricing_scope(), PricingScope::Public);
}

#[tokio::test]
async fn configured_identity_reaches_the_profile_and_cannot_be_overridden_by_the_request() {
    let buyer = scope("buyer-configured");
    let profile = ProfileIdentity::new(SourceId::new("1688").expect("source"), "procurement-cn")
        .expect("profile");
    let identity = ConnectorIdentity::anonymous()
        .with_profile(profile)
        .with_account_scope(buyer.clone())
        .with_market(text("CN"))
        .with_locale(text("zh-CN"));
    let capture = Arc::new(RecordingCapture::new(dom_bundle(
        AccessVisibility::AccountScoped(buyer.clone()),
    )));
    let runtime = ConnectorRuntime::new(
        Arc::new(NoopTransport),
        Arc::new(QuotaState::new()),
        Arc::new(SecretGuard::new()),
        Arc::new(MemoryDiagnostics::new()),
        Arc::new(ManualClock::new(1_700_000_000_000)),
    )
    .with_browser_extraction(capture.clone())
    .with_identity(identity);
    let registered = Registered::new(browser_connector(), runtime);
    let mut registry = ConnectorRegistry::new();
    registry
        .register(Arc::new(registered))
        .expect("registration");
    let connector = registry
        .connector(&SourceId::new("1688").expect("source"))
        .expect("registered connector");

    // The model/tool request does NOT set an account scope; a hostile one
    // would still lose to the configured identity.
    let request = CommerceProductRequest::new(
        ProductRef::Url { url: detail_url() },
        FreshnessMode::Live,
        DetailLevel::Normal,
    )
    .expect("product request");
    let commerce = CommerceCtx::new().with_mechanism(AcquisitionMechanism::BrowserNetwork);
    let offer = connector
        .product(&commerce, request)
        .await
        .expect("product through the registry");

    let profiles = capture.profiles();
    assert_eq!(profiles.len(), 1);
    assert_eq!(profiles[0].account, Some(buyer.clone()));
    assert_eq!(profiles[0].profile.as_str(), "procurement-cn");
    assert_eq!(
        profiles[0].egress.as_ref().map(Text::as_str),
        Some(CN_EGRESS_LABEL),
        "the connector-owned stable egress label is preserved"
    );

    assert_eq!(offer.price_visibility, PriceVisibility::AccountSpecific);
    for price_break in &offer.price_breaks {
        assert_eq!(price_break.account_scope, Some(buyer.clone()));
    }
    assert_eq!(offer.provenance.account_scope, Some(buyer.clone()));
    assert_eq!(
        offer.provenance.market.as_ref().map(Text::as_str),
        Some("CN")
    );
    assert_eq!(
        offer.provenance.locale.as_ref().map(Text::as_str),
        Some("zh-CN")
    );

    // A request that tries to manufacture a different identity must not
    // change the profile the browser authority sees.
    let capture = Arc::new(RecordingCapture::new(dom_bundle(
        AccessVisibility::AccountScoped(buyer.clone()),
    )));
    let runtime = ConnectorRuntime::new(
        Arc::new(NoopTransport),
        Arc::new(QuotaState::new()),
        Arc::new(SecretGuard::new()),
        Arc::new(MemoryDiagnostics::new()),
        Arc::new(ManualClock::new(1_700_000_000_000)),
    )
    .with_browser_extraction(capture.clone())
    .with_identity(
        ConnectorIdentity::anonymous()
            .with_profile(
                ProfileIdentity::new(SourceId::new("1688").expect("source"), "procurement-cn")
                    .expect("profile"),
            )
            .with_account_scope(buyer.clone()),
    );
    let mut registry = ConnectorRegistry::new();
    registry
        .register(Arc::new(Registered::new(browser_connector(), runtime)))
        .expect("registration");
    let connector = registry
        .connector(&SourceId::new("1688").expect("source"))
        .expect("registered connector");
    let hostile = CommerceCtx::new()
        .with_mechanism(AcquisitionMechanism::BrowserNetwork)
        .with_account(scope("buyer-requested"));
    connector
        .product(
            &hostile,
            CommerceProductRequest::new(
                ProductRef::Url { url: detail_url() },
                FreshnessMode::Live,
                DetailLevel::Normal,
            )
            .expect("product request"),
        )
        .await
        .expect("product through the registry");
    assert_eq!(
        capture.profiles()[0].account,
        Some(buyer),
        "the configured identity always wins over a request-supplied scope"
    );
}

#[tokio::test]
async fn an_unconfigured_identity_still_lets_an_embedded_caller_scope_through() {
    // The embedded-caller fallback (no daemon identity configured) is the
    // only path where the caller's account scope is honored; it is still a
    // typed identity, never a public one.
    let requested = scope("buyer-embedded");
    let capture = Arc::new(RecordingCapture::new(dom_bundle(
        AccessVisibility::AccountScoped(requested.clone()),
    )));
    let runtime = ConnectorRuntime::new(
        Arc::new(NoopTransport),
        Arc::new(QuotaState::new()),
        Arc::new(SecretGuard::new()),
        Arc::new(MemoryDiagnostics::new()),
        Arc::new(ManualClock::new(1_700_000_000_000)),
    )
    .with_browser_extraction(capture.clone());
    let mut registry = ConnectorRegistry::new();
    registry
        .register(Arc::new(Registered::new(browser_connector(), runtime)))
        .expect("registration");
    let connector = registry
        .connector(&SourceId::new("1688").expect("source"))
        .expect("registered connector");
    let commerce = CommerceCtx::new()
        .with_mechanism(AcquisitionMechanism::BrowserNetwork)
        .with_account(requested.clone());
    let offer = connector
        .product(
            &commerce,
            CommerceProductRequest::new(
                ProductRef::Url { url: detail_url() },
                FreshnessMode::Live,
                DetailLevel::Normal,
            )
            .expect("product request"),
        )
        .await
        .expect("product through the registry");
    assert_eq!(capture.profiles()[0].account, Some(requested.clone()));
    assert_eq!(offer.price_visibility, PriceVisibility::AccountSpecific);
    let accounted = offer
        .variants
        .iter()
        .flat_map(|variant| &variant.price_breaks)
        .chain(offer.price_breaks.iter())
        .next()
        .expect("at least one price break");
    assert_eq!(accounted.visibility, PriceVisibility::AccountSpecific);
    assert_eq!(accounted.account_scope, Some(requested));
}
