//! Production-wiring certification for the marketplace API authentication
//! path: the REAL Alibaba/1688 Open API signing code, registered by the
//! production `register_commerce_connectors` onto the DAEMON'S commerce
//! service, exercised against a contract mock that independently recomputes
//! the documented AOP signature from the actual wire bytes and refuses any
//! mismatch.
//!
//! The only substituted seam is the network transport (the connector's
//! injected `HttpTransport`): the connector, the registration, the identity
//! injection, the credential resolution and the tool/service path are the
//! production ones built over `wiring::build_production_graph`. A
//! missing/expired access token must be the typed `AuthenticationRequired`
//! with zero requests and zero browser consultations.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use faktor_commerce::connector::ProfileIdentity;
use faktor_commerce::text::{CanonicalUrl, SourceId};
use faktor_commerce::SourceError;
use faktor_commerce_connectors::http::{
    HttpRequest, HttpResponse, HttpTransport, ResponseBudget, TransportError,
};
use faktor_commerce_connectors::testing::MapCredentials;
use faktor_commerce_connectors::{
    BrowserExtraction, CaptureBundle, SecretGuard, SharedBrowserExtraction,
};
use faktor_core::ErrorKind;
use faktor_tests_production_wiring::wiring::CommerceSeams;
use hmac::Mac as _;
use serde_json::json;

use faktor_tests_production_wiring::wiring::{self, CommerceCfg, Config};

const NOW_MS: u64 = 1_700_000_000_000;

fn real_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock")
        .as_millis() as u64
}
const HOUR_MS: u64 = 3_600_000;
const APP_KEY: &str = "sanitized-app-key";
const APP_SECRET: &str = "sanitized-app-secret";
const ACCESS_TOKEN: &str = "sanitized-access-token";
const KEY_ENV: &str = "PW_ALIBABA_APP_KEY";
const SECRET_ENV: &str = "PW_ALIBABA_APP_SECRET";
const TOKEN_ENV: &str = "PW_ALIBABA_ACCESS_TOKEN";
const EXPIRES_ENV: &str = "PW_ALIBABA_ACCESS_TOKEN_EXPIRES_AT";
const PRODUCT_URL: &str = "https://www.alibaba.com/product-detail/_1600123456789.html";

// ------------------------------------------------------------- contract mock

#[derive(Default)]
struct MockState {
    responses: VecDeque<Vec<u8>>,
    requests: Vec<(String, String)>,
}

/// A transport that validates the documented AOP signature before serving a
/// recorded sanitized fixture — independently of the production signer.
struct ContractMock {
    secret: &'static str,
    state: Mutex<MockState>,
    validated: AtomicUsize,
    refused: AtomicUsize,
}

impl ContractMock {
    fn new(secret: &'static str, fixture: &str) -> Arc<Self> {
        let mut state = MockState::default();
        state.responses.push_back(fixture.as_bytes().to_vec());
        Arc::new(Self {
            secret,
            state: Mutex::new(state),
            validated: AtomicUsize::new(0),
            refused: AtomicUsize::new(0),
        })
    }

    fn validated(&self) -> usize {
        self.validated.load(Ordering::SeqCst)
    }

    fn refused(&self) -> usize {
        self.refused.load(Ordering::SeqCst)
    }

    fn request_count(&self) -> usize {
        self.state.lock().unwrap().requests.len()
    }

    fn last_body(&self) -> String {
        self.state
            .lock()
            .unwrap()
            .requests
            .last()
            .map(|(_, body)| body.clone())
            .unwrap_or_default()
    }
}

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
    let mut mac =
        <hmac::Hmac<sha1::Sha1> as hmac::Mac>::new_from_slice(secret.as_bytes()).expect("hmac key");
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

#[async_trait]
impl HttpTransport for ContractMock {
    async fn execute(
        &self,
        request: HttpRequest,
        _budget: ResponseBudget,
    ) -> Result<HttpResponse, TransportError> {
        let body = String::from_utf8_lossy(request.body().unwrap_or_default()).into_owned();
        let params = parse_form(&body);
        let received = params
            .iter()
            .find(|(name, _)| name == "_aop_signature")
            .map(|(_, value)| value.clone());
        self.state
            .lock()
            .unwrap()
            .requests
            .push((request.url().as_str().to_string(), body));
        if received.as_deref() != Some(expected_signature(self.secret, &params).as_str()) {
            self.refused.fetch_add(1, Ordering::SeqCst);
            return HttpResponse::new(401, Vec::new(), b"{}".to_vec())
                .map_err(|_| TransportError::Protocol);
        }
        self.validated.fetch_add(1, Ordering::SeqCst);
        match self.state.lock().unwrap().responses.pop_front() {
            Some(body) => {
                HttpResponse::new(200, Vec::new(), body).map_err(|_| TransportError::Protocol)
            }
            None => Err(TransportError::EgressUnavailable),
        }
    }
}

// ----------------------------------------------------------------- seaming

/// Counts browser consult attempts; the API path must never fall back to it
/// silently.
#[derive(Default)]
struct CountingBrowser {
    calls: AtomicUsize,
}

#[async_trait]
impl BrowserExtraction for CountingBrowser {
    async fn capture(
        &self,
        _ctx: &faktor_commerce_connectors::AcquireCtx,
        _source: &SourceId,
        _profile: &ProfileIdentity,
        _url: &CanonicalUrl,
    ) -> Result<CaptureBundle, SourceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(SourceError::BrowserUnavailable)
    }
}

fn seams_with(
    mock: Arc<ContractMock>,
    credentials: Arc<dyn faktor_commerce_connectors::CredentialProvider>,
    browser: Arc<CountingBrowser>,
) -> CommerceSeams {
    let browser: SharedBrowserExtraction = browser;
    CommerceSeams {
        transport: mock,
        browser: Some(browser),
        credentials,
        diagnostics: None,
        secrets: Arc::new(SecretGuard::new()),
    }
}

fn alibaba_config(api: Option<serde_json::Value>) -> CommerceCfg {
    let mut connector = json!({"enabled": true});
    if let Some(api) = api {
        connector["api"] = api;
    }
    serde_json::from_value(json!({
        "enabled": true,
        "connectors": {"alibaba": connector},
    }))
    .expect("the production commerce config must accept the fixture")
}

fn credentials(with_token: bool) -> MapCredentials {
    let base = MapCredentials::new()
        .with(KEY_ENV, APP_KEY)
        .with(SECRET_ENV, APP_SECRET);
    if with_token {
        base.with(TOKEN_ENV, ACCESS_TOKEN)
            .with(EXPIRES_ENV, &(real_now_ms() + HOUR_MS).to_string())
    } else {
        base
    }
}

fn bare_commerce_config() -> Config {
    let commerce: CommerceCfg = serde_json::from_value(json!({"enabled": true})).unwrap();
    Config {
        commerce,
        ..Default::default()
    }
}

fn tool_ctx() -> faktor_agent::ToolRunCtx {
    use faktor_agent::{ToolCallMode, ToolRunCtx};
    use faktor_core::cancellation::CancellationToken;
    use faktor_core::id::{OpId, SessionId, TaskId, WorkspaceId, WorktreeId};
    use faktor_core::WorkspaceIdentity;
    ToolRunCtx {
        session_id: SessionId::new(1),
        op_id: OpId::new(1),
        identity: WorkspaceIdentity::new(WorkspaceId::new(1), WorktreeId::new(1), TaskId::new(1)),
        cancellation: CancellationToken::new(),
        artifacts: Arc::new(faktor_agent::ToolArtifactSink::Null),
        tool_call_mode: ToolCallMode::Native,
        workspace: None,
        edit: None,
        snapshots: None,
        sandbox: None,
        supervisor: None,
        deadline_ms: 30_000,
        permission_granted: true,
    }
}

fn product_args() -> serde_json::Value {
    json!({
        "op": "product",
        "ref": PRODUCT_URL,
        "sources": ["alibaba"],
        "freshness": "live",
    })
}

fn api_block(with_token: bool) -> serde_json::Value {
    let mut value = json!({
        "app_key_env": KEY_ENV,
        "app_secret_env": SECRET_ENV,
        "scopes": ["seller_product", "buyer_discovery", "trade_terms", "supplier_profile"],
    });
    if with_token {
        value["access_token_env"] = json!(TOKEN_ENV);
        value["access_token_expires_at_env"] = json!(EXPIRES_ENV);
    }
    value
}

// ------------------------------------------------------------------- tests

/// The signed request produced by the production Alibaba connector passes
/// the independently-verifying contract mock and replays its fixture through
/// the daemon's `source_market` tool; the browser seam is never consulted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signed_alibaba_request_is_verified_by_the_contract_mock() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = alibaba_config(Some(api_block(true)));
    let graph = wiring::build_production_graph(dir.path(), bare_commerce_config())
        .expect("production daemon graph");
    let service = wiring::commerce_service(&graph).expect("commerce enabled");

    let fixture = faktor_commerce_connectors::testing::fixture("alibaba/api_product.json")
        .expect("sanitized alibaba fixture");
    let mock = ContractMock::new(APP_SECRET, &fixture);
    let browser = Arc::new(CountingBrowser::default());
    let seams = seams_with(mock.clone(), Arc::new(credentials(true)), browser.clone());
    let registration = wiring::register_production_connectors(&service, &cfg, &seams);
    assert_eq!(registration.registered(), 1, "{registration:?}");

    let tool = wiring::daemon_tool(&graph, "source_market");
    let outcome = (tool.execute)(tool_ctx(), product_args())
        .await
        .expect("the signed product request must validate and replay");
    assert_eq!(mock.validated(), 1, "the mock must validate one signature");
    assert_eq!(mock.refused(), 0);
    assert_eq!(
        browser.calls.load(Ordering::SeqCst),
        0,
        "the API path must never consult the browser"
    );
    let body = mock.last_body();
    assert!(body.contains("access_token="), "token missing: {body}");
    assert!(!body.contains(APP_SECRET), "app secret leaked: {body}");
    assert!(
        outcome.text.contains("USB 3.0 Braided Data Cable"),
        "{}",
        outcome.text
    );
}

/// With the app credential but NO access token, the same daemon tool path is
/// a typed `AuthenticationRequired` (permission class): zero requests reach
/// the transport, zero browser calls happen, and nothing is fabricated.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_access_token_is_typed_with_no_request_and_no_browser() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = alibaba_config(Some(api_block(false)));
    let graph = wiring::build_production_graph(dir.path(), bare_commerce_config())
        .expect("production daemon graph");
    let service = wiring::commerce_service(&graph).expect("commerce enabled");

    let mock = ContractMock::new(APP_SECRET, "{}");
    let browser = Arc::new(CountingBrowser::default());
    let seams = seams_with(mock.clone(), Arc::new(credentials(false)), browser.clone());
    let registration = wiring::register_production_connectors(&service, &cfg, &seams);
    assert_eq!(registration.registered(), 1, "{registration:?}");

    let tool = wiring::daemon_tool(&graph, "source_market");
    let error = (tool.execute)(tool_ctx(), product_args())
        .await
        .expect_err("a missing access token must refuse typed");
    assert_eq!(error.kind, ErrorKind::Permission, "{error:?}");
    let rendered = format!("{error:?}");
    assert!(
        rendered.to_ascii_lowercase().contains("authentication"),
        "{rendered}"
    );
    assert_eq!(mock.request_count(), 0, "no request may be built");
    assert_eq!(mock.validated(), 0);
    assert_eq!(mock.refused(), 0);
    assert_eq!(
        browser.calls.load(Ordering::SeqCst),
        0,
        "no silent browser fallback for an auth failure"
    );
}

/// An EXPIRED staged token is the same typed refusal without any request:
/// the connector must not sign with a token it knows is invalid.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_access_token_is_typed_with_no_request() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = alibaba_config(Some(api_block(true)));
    let graph = wiring::build_production_graph(dir.path(), bare_commerce_config())
        .expect("production daemon graph");
    let service = wiring::commerce_service(&graph).expect("commerce enabled");

    let expired = MapCredentials::new()
        .with(KEY_ENV, APP_KEY)
        .with(SECRET_ENV, APP_SECRET)
        .with(TOKEN_ENV, ACCESS_TOKEN)
        .with(EXPIRES_ENV, &(NOW_MS - 1).to_string());
    let mock = ContractMock::new(APP_SECRET, "{}");
    let browser = Arc::new(CountingBrowser::default());
    let seams = seams_with(mock.clone(), Arc::new(expired), browser.clone());
    let registration = wiring::register_production_connectors(&service, &cfg, &seams);
    assert_eq!(registration.registered(), 1, "{registration:?}");

    let tool = wiring::daemon_tool(&graph, "source_market");
    let error = (tool.execute)(tool_ctx(), product_args())
        .await
        .expect_err("an expired token must refuse typed");
    assert_eq!(error.kind, ErrorKind::Permission, "{error:?}");
    assert_eq!(mock.request_count(), 0, "no request may be built");
    assert_eq!(browser.calls.load(Ordering::SeqCst), 0);
}
