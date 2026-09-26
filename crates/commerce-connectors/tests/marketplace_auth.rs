//! Marketplace API authentication tests (`docs/acquire.md` §10, §16).
//!
//! These tests exercise the Alibaba-family Open API contract end to end,
//! offline: a [`ContractMock`] transport recomputes the documented AOP
//! common-parameter signature from the actual wire bytes (independently of
//! the production signer) and refuses any request whose signature does not
//! match — so a tampered or unsigned request cannot pass. Responses are the
//! recorded sanitized fixtures.
//!
//! Also pinned here: an account-scoped call without a valid unexpired access
//! token is typed `AuthenticationRequired` and never consults the browser;
//! the app secret never becomes a wire value; the access token never enters
//! the URL; an expired token refreshes through the injected seam and the
//! fresh token is registered with the secret scanner.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use hmac::{Hmac, Mac};
use sha1::Sha1;

use faktor_commerce_connectors::alibaba::auth::{RefreshedToken, TokenRefresher};
use faktor_commerce_connectors::alibaba::OpenApiConfig;
use faktor_commerce_connectors::china1688::OpenPlatformConfig;
use faktor_commerce_connectors::contract::{
    Mechanism, ProductReference, ProductRequest, SearchRequest, SiteConnector,
};
use faktor_commerce_connectors::http::{
    HttpRequest, HttpResponse, HttpTransport, ResponseBudget, TransportError,
};
use faktor_commerce_connectors::testing::{MapCredentials, RecordingBrowser};
use faktor_commerce_connectors::{
    AcquireCtx, AlibabaApiScopes, AlibabaConnector, China1688ApiScopes, China1688Connector,
    ManualClock, QuotaState, SecretGuard, SecretString, SourceError, Text,
};

const NOW_MS: u64 = 1_700_000_000_000;
const HOUR_MS: u64 = 3_600_000;

const ALIBABA_KEY: &str = "alibaba-sanitized-key";
const ALIBABA_SECRET: &str = "alibaba-sanitized-app-secret";
const ALIBABA_TOKEN: &str = "alibaba-sanitized-access-token";

const CN_KEY: &str = "1688-sanitized-key";
const CN_SECRET: &str = "1688-sanitized-app-secret";
const CN_TOKEN: &str = "1688-sanitized-access-token";

fn text<const MAX: usize>(raw: &str) -> Text<MAX> {
    Text::<MAX>::new(raw).expect("text")
}

fn fixture(relative: &str) -> String {
    faktor_commerce_connectors::testing::fixture(relative).expect("fixture")
}

/// The documented AOP signature over the raw decoded form parameters.
fn expected_signature(secret: &str, params: &[(String, String)]) -> String {
    let mut canonical_params: Vec<&(String, String)> = params
        .iter()
        .filter(|(name, _)| name != "_aop_signature")
        .collect();
    canonical_params.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
    let mut canonical = String::new();
    for (name, value) in canonical_params {
        canonical.push_str(name);
        canonical.push_str(value);
    }
    let mut mac = <Hmac<Sha1> as Mac>::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(canonical.as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect()
}

fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        index += 3;
                    }
                    Err(_) => {
                        out.push(b'%');
                        index += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_form(body: &str) -> Vec<(String, String)> {
    body.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((name, value)) => (percent_decode(name), percent_decode(value)),
            None => (percent_decode(pair), String::new()),
        })
        .collect()
}

/// A transport that validates the documented signature of every request
/// before answering from a queued recorded-response fixture.
struct ContractMock {
    app_secret: &'static str,
    responses: Mutex<VecDeque<Vec<u8>>>,
    requests: Mutex<Vec<HttpRequest>>,
    /// When set, the mock corrupts `_aop_signature` in flight (an on-path
    /// tamper) and must then refuse the request.
    tamper_signature: bool,
    validated: AtomicUsize,
    refused: AtomicUsize,
}

impl ContractMock {
    fn new(app_secret: &'static str) -> Self {
        Self {
            app_secret,
            responses: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
            tamper_signature: false,
            validated: AtomicUsize::new(0),
            refused: AtomicUsize::new(0),
        }
    }

    fn tampering(self) -> Self {
        Self {
            tamper_signature: true,
            ..self
        }
    }

    fn enqueue(self, body: &str) -> Self {
        self.responses
            .lock()
            .expect("responses")
            .push_back(body.as_bytes().to_vec());
        self
    }

    fn requests(&self) -> Vec<HttpRequest> {
        self.requests.lock().expect("requests").clone()
    }

    fn validated(&self) -> usize {
        self.validated.load(Ordering::SeqCst)
    }

    fn refused(&self) -> usize {
        self.refused.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl HttpTransport for ContractMock {
    async fn execute(
        &self,
        request: HttpRequest,
        _budget: ResponseBudget,
    ) -> Result<HttpResponse, TransportError> {
        let body = String::from_utf8_lossy(request.body().unwrap_or_default()).into_owned();
        let mut params = parse_form(&body);
        if self.tamper_signature {
            for (name, value) in &mut params {
                if name == "_aop_signature" {
                    *value = "0".repeat(value.len());
                }
            }
        }
        let received = params
            .iter()
            .find(|(name, _)| name == "_aop_signature")
            .map(|(_, value)| value.clone());
        self.requests.lock().expect("requests").push(request);
        if received.as_deref() != Some(expected_signature(self.app_secret, &params).as_str()) {
            self.refused.fetch_add(1, Ordering::SeqCst);
            return HttpResponse::new(401, Vec::new(), b"{}".to_vec())
                .map_err(|_| TransportError::Protocol);
        }
        self.validated.fetch_add(1, Ordering::SeqCst);
        match self.responses.lock().expect("responses").pop_front() {
            Some(body) => {
                HttpResponse::new(200, Vec::new(), body).map_err(|_| TransportError::Protocol)
            }
            None => Err(TransportError::EgressUnavailable),
        }
    }
}

fn ctx_with(
    mock: Arc<ContractMock>,
    secrets: Arc<SecretGuard>,
    browser: Arc<RecordingBrowser>,
) -> AcquireCtx {
    let transport: Arc<dyn HttpTransport> = mock;
    AcquireCtx::builder(transport, Arc::new(QuotaState::new()), secrets)
        .clock(Arc::new(ManualClock::new(NOW_MS)))
        .mechanism(Mechanism::OfficialApi)
        .browser(browser)
        .build()
}

fn alibaba_connector(
    mock_secret: &'static str,
    token_expires_at_ms: u64,
) -> (AlibabaConnector, Arc<SecretGuard>) {
    let secrets = Arc::new(SecretGuard::new());
    let credentials = Arc::new(
        MapCredentials::new()
            .with("FAKTOR_ALIBABA_APP_KEY", ALIBABA_KEY)
            .with("FAKTOR_ALIBABA_APP_SECRET", mock_secret)
            .with("FAKTOR_ALIBABA_ACCESS_TOKEN", ALIBABA_TOKEN)
            .with(
                "FAKTOR_ALIBABA_ACCESS_TOKEN_EXPIRES_AT",
                &token_expires_at_ms.to_string(),
            ),
    );
    let config = OpenApiConfig::new(
        "FAKTOR_ALIBABA_APP_KEY",
        "FAKTOR_ALIBABA_APP_SECRET",
        AlibabaApiScopes::buyer_visible(),
    )
    .expect("config")
    .with_access_token(
        "FAKTOR_ALIBABA_ACCESS_TOKEN",
        "FAKTOR_ALIBABA_ACCESS_TOKEN_EXPIRES_AT",
    )
    .expect("token config");
    let connector = AlibabaConnector::new(&faktor_commerce_connectors::ProfileConnectorConfig {
        enabled: true,
        profile: Some(text("procurement-global")),
    })
    .expect("connector")
    .with_open_api(&config, credentials, secrets.clone(), None)
    .expect("api connector");
    (connector, secrets)
}

fn china1688_connector(mock_secret: &'static str) -> (China1688Connector, Arc<SecretGuard>) {
    let secrets = Arc::new(SecretGuard::new());
    let credentials = Arc::new(
        MapCredentials::new()
            .with("FAKTOR_1688_APP_KEY", CN_KEY)
            .with("FAKTOR_1688_APP_SECRET", mock_secret)
            .with("FAKTOR_1688_ACCESS_TOKEN", CN_TOKEN)
            .with(
                "FAKTOR_1688_ACCESS_TOKEN_EXPIRES_AT",
                &(NOW_MS + HOUR_MS).to_string(),
            ),
    );
    let config = OpenPlatformConfig::new(
        "FAKTOR_1688_APP_KEY",
        "FAKTOR_1688_APP_SECRET",
        China1688ApiScopes::all(),
    )
    .expect("config")
    .with_access_token(
        "FAKTOR_1688_ACCESS_TOKEN",
        "FAKTOR_1688_ACCESS_TOKEN_EXPIRES_AT",
    )
    .expect("token config");
    let connector = China1688Connector::new(&faktor_commerce_connectors::ProfileConnectorConfig {
        enabled: true,
        profile: Some(text("procurement-cn")),
    })
    .expect("connector")
    .with_open_platform(&config, credentials, secrets.clone(), None)
    .expect("api connector");
    (connector, secrets)
}

fn product_url() -> ProductReference {
    ProductReference::Url(
        faktor_commerce_connectors::CanonicalUrl::parse(
            "https://www.alibaba.com/product-detail/_1600123456789.html",
        )
        .expect("url"),
    )
}

fn cn_product_url() -> ProductReference {
    ProductReference::Url(
        faktor_commerce_connectors::CanonicalUrl::parse(
            "https://detail.1688.com/offer/678901234567.html",
        )
        .expect("url"),
    )
}

#[tokio::test]
async fn alibaba_signed_request_passes_the_contract_mock_and_replays_its_fixture() {
    let mock =
        Arc::new(ContractMock::new(ALIBABA_SECRET).enqueue(&fixture("alibaba/api_product.json")));
    let (connector, _secrets) = alibaba_connector(ALIBABA_SECRET, NOW_MS + HOUR_MS);
    let browser = Arc::new(RecordingBrowser::new());
    let ctx = ctx_with(mock.clone(), Arc::new(SecretGuard::new()), browser.clone());
    let offer = connector
        .product(&ctx, ProductRequest::new(product_url()))
        .await
        .expect("offer");
    assert_eq!(offer.title.as_str(), "USB 3.0 Braided Data Cable");
    assert_eq!(mock.validated(), 1);
    assert_eq!(mock.refused(), 0);
    assert_eq!(browser.calls(), 0);
    let requests = mock.requests();
    let url = requests[0].url().as_str();
    let body = String::from_utf8_lossy(requests[0].body().unwrap_or_default()).into_owned();
    assert!(url.contains("openapi.alibaba.com"), "{url}");
    assert!(!url.contains(ALIBABA_TOKEN), "token leaked into URL: {url}");
    assert!(
        !url.contains(ALIBABA_SECRET),
        "secret leaked into URL: {url}"
    );
    assert!(
        !body.contains(ALIBABA_SECRET),
        "secret leaked into body: {body}"
    );
    assert!(body.contains("access_token="), "{body}");
}

#[tokio::test]
async fn china1688_signed_request_passes_the_contract_mock_and_replays_its_fixture() {
    let mock =
        Arc::new(ContractMock::new(CN_SECRET).enqueue(&fixture("china1688/api_offer_detail.json")));
    let (connector, _secrets) = china1688_connector(CN_SECRET);
    let browser = Arc::new(RecordingBrowser::new());
    let ctx = ctx_with(mock.clone(), Arc::new(SecretGuard::new()), browser.clone());
    let offer = connector
        .product(&ctx, ProductRequest::new(cn_product_url()))
        .await
        .expect("offer");
    assert_eq!(offer.title.as_str(), "USB 3.0 数据线 编织款");
    assert_eq!(mock.validated(), 1);
    assert_eq!(mock.refused(), 0);
    assert_eq!(browser.calls(), 0);
    let requests = mock.requests();
    let url = requests[0].url().as_str();
    assert!(url.contains("gw.open.1688.com"), "{url}");
    assert!(!url.contains(CN_TOKEN), "token leaked into URL: {url}");
    assert!(!url.contains(CN_SECRET), "secret leaked into URL: {url}");
}

#[tokio::test]
async fn an_expired_token_refreshes_through_the_seam_and_the_fresh_token_is_registered() {
    #[derive(Debug)]
    struct FixedRefresher;

    impl TokenRefresher for FixedRefresher {
        fn refresh(
            &self,
            _refresh_token: Option<&SecretString>,
            _now_ms: u64,
        ) -> Result<RefreshedToken, SourceError> {
            Ok(RefreshedToken::new(
                SecretString::new("alibaba-refreshed-token".to_string()),
                NOW_MS + HOUR_MS,
            ))
        }
    }

    let mock =
        Arc::new(ContractMock::new(ALIBABA_SECRET).enqueue(&fixture("alibaba/api_product.json")));
    let secrets = Arc::new(SecretGuard::new());
    let credentials = Arc::new(
        MapCredentials::new()
            .with("FAKTOR_ALIBABA_APP_KEY", ALIBABA_KEY)
            .with("FAKTOR_ALIBABA_APP_SECRET", ALIBABA_SECRET)
            .with("FAKTOR_ALIBABA_ACCESS_TOKEN", ALIBABA_TOKEN)
            .with(
                "FAKTOR_ALIBABA_ACCESS_TOKEN_EXPIRES_AT",
                &(NOW_MS - 1).to_string(),
            )
            .with("FAKTOR_ALIBABA_REFRESH_TOKEN", "alibaba-refresh-token"),
    );
    let config = OpenApiConfig::new(
        "FAKTOR_ALIBABA_APP_KEY",
        "FAKTOR_ALIBABA_APP_SECRET",
        AlibabaApiScopes::buyer_visible(),
    )
    .expect("config")
    .with_access_token(
        "FAKTOR_ALIBABA_ACCESS_TOKEN",
        "FAKTOR_ALIBABA_ACCESS_TOKEN_EXPIRES_AT",
    )
    .expect("token config")
    .with_refresh_token("FAKTOR_ALIBABA_REFRESH_TOKEN")
    .expect("refresh config");
    let connector = AlibabaConnector::new(&faktor_commerce_connectors::ProfileConnectorConfig {
        enabled: true,
        profile: Some(text("procurement-global")),
    })
    .expect("connector")
    .with_open_api(
        &config,
        credentials,
        secrets.clone(),
        Some(Arc::new(FixedRefresher)),
    )
    .expect("api connector");
    assert!(!connector.has_valid_access_token(NOW_MS));
    let browser = Arc::new(RecordingBrowser::new());
    let ctx = ctx_with(mock.clone(), secrets.clone(), browser.clone());
    let offer = connector
        .product(&ctx, ProductRequest::new(product_url()))
        .await
        .expect("offer after refresh");
    assert_eq!(offer.title.as_str(), "USB 3.0 Braided Data Cable");
    assert_eq!(mock.validated(), 1, "the refreshed signature must validate");
    assert_eq!(mock.refused(), 0);
    assert!(connector.has_valid_access_token(NOW_MS));
    let body = String::from_utf8_lossy(mock.requests()[0].body().unwrap_or_default()).into_owned();
    assert!(
        body.contains("access_token=alibaba-refreshed-token"),
        "{body}"
    );
    assert!(
        secrets.scrub("alibaba-refreshed-token") != "alibaba-refreshed-token",
        "the refreshed token must be scanner-registered"
    );
    assert_eq!(browser.calls(), 0);
}

#[tokio::test]
async fn a_tampered_signature_is_rejected_and_never_bypassed_by_the_browser() {
    let mock = Arc::new(
        ContractMock::new(CN_SECRET)
            .tampering()
            .enqueue(&fixture("china1688/api_offer_detail.json")),
    );
    let (connector, _secrets) = china1688_connector(CN_SECRET);
    let browser = Arc::new(RecordingBrowser::new());
    let ctx = ctx_with(mock.clone(), Arc::new(SecretGuard::new()), browser.clone());
    let error = connector
        .product(&ctx, ProductRequest::new(cn_product_url()))
        .await
        .expect_err("tampered signature");
    assert_eq!(error, SourceError::AuthenticationRequired);
    assert_eq!(mock.validated(), 0);
    assert_eq!(
        mock.refused(),
        1,
        "the mock must reject the tampered signature"
    );
    assert_eq!(browser.calls(), 0);
}

#[tokio::test]
async fn a_missing_token_is_typed_and_never_builds_a_request_or_consults_the_browser() {
    let mock =
        Arc::new(ContractMock::new(CN_SECRET).enqueue(&fixture("china1688/api_offer_detail.json")));
    let secrets = Arc::new(SecretGuard::new());
    let credentials = Arc::new(
        MapCredentials::new()
            .with("FAKTOR_1688_APP_KEY", CN_KEY)
            .with("FAKTOR_1688_APP_SECRET", CN_SECRET),
    );
    let config = OpenPlatformConfig::new(
        "FAKTOR_1688_APP_KEY",
        "FAKTOR_1688_APP_SECRET",
        China1688ApiScopes::all(),
    )
    .expect("config");
    let connector = China1688Connector::new(&faktor_commerce_connectors::ProfileConnectorConfig {
        enabled: true,
        profile: Some(text("procurement-cn")),
    })
    .expect("connector")
    .with_open_platform(&config, credentials, secrets.clone(), None)
    .expect("api connector");
    let browser = Arc::new(RecordingBrowser::new());
    let ctx = ctx_with(mock.clone(), secrets, browser.clone());
    assert_eq!(
        connector
            .product(&ctx, ProductRequest::new(cn_product_url()))
            .await
            .expect_err("no token"),
        SourceError::AuthenticationRequired
    );
    assert_eq!(mock.requests().len(), 0, "no request may be built");
    assert_eq!(mock.validated(), 0);
    assert_eq!(mock.refused(), 0);
    assert_eq!(browser.calls(), 0, "no silent browser fallback");
}

#[test]
fn a_missing_app_secret_refuses_at_construction() {
    let credentials = Arc::new(MapCredentials::new().with("FAKTOR_1688_APP_KEY", CN_KEY));
    let config = OpenPlatformConfig::new(
        "FAKTOR_1688_APP_KEY",
        "FAKTOR_1688_APP_SECRET",
        China1688ApiScopes::all(),
    )
    .expect("config");
    let error = China1688Connector::new(&faktor_commerce_connectors::ProfileConnectorConfig {
        enabled: true,
        profile: Some(text("procurement-cn")),
    })
    .expect("connector")
    .with_open_platform(&config, credentials, Arc::new(SecretGuard::new()), None)
    .expect_err("missing app secret must refuse at construction");
    assert!(matches!(
        error,
        faktor_commerce_connectors::ConfigError::MissingCredential { .. }
    ));
}

#[tokio::test]
async fn discovery_is_account_scoped_too() {
    let mock = Arc::new(
        ContractMock::new(ALIBABA_SECRET).enqueue(&fixture("alibaba/network_search.json")),
    );
    let (connector, _secrets) = alibaba_connector(ALIBABA_SECRET, NOW_MS + HOUR_MS);
    // The fixture is a browser-network payload; as an API body it simply
    // produces no discovery, which is the honest typed outcome — the point
    // here is that the signed request reached the mock and validated.
    let browser = Arc::new(RecordingBrowser::new());
    let ctx = ctx_with(mock.clone(), Arc::new(SecretGuard::new()), browser);
    let _ = connector
        .discover(
            &ctx,
            SearchRequest::new(text("usb cable"), 5, None).expect("search"),
        )
        .await;
    assert_eq!(mock.validated(), 1);
    assert_eq!(mock.refused(), 0);
}
