//! Cross-site integration tests: one contract, credential discipline and
//! the acceptance rule that live pricing never answers from stale discovery
//! data (`docs/acquire.md` §16).
//!
//! Every test is offline: the injected fixture transport replays sanitized
//! JSON from `fixtures/`.

use std::sync::Arc;

use faktor_commerce_connectors::config::{
    ConfigError, DigiKeyConnectorConfig, LcscConnectorConfig, MouserConnectorConfig,
    ProfileConnectorConfig,
};
use faktor_commerce_connectors::contract::{
    Capability, CapabilityLevel, ConnectorCapabilities, Mechanism, ProductReference,
    ProductRequest, QuoteRequest, SearchRequest, SiteConnector,
};
use faktor_commerce_connectors::testing::{
    CannedResponse, FixtureTransport, MapCredentials, RecordingBrowser,
};
use faktor_commerce_connectors::MemoryDiagnostics;
use faktor_commerce_connectors::{
    AcquireCtx, AlibabaConnector, China1688Connector, DigiKeyConnector, LcscConnector, ManualClock,
    MouserConnector, QuotaState, SecretGuard, SourceError, Text,
};

const MOUSER_KEY: &str = "mouser-sanitized-key-0123456789abcdef";
const DIGIKEY_CLIENT_ID: &str = "digikey-sanitized-client-id-0001";
const DIGIKEY_CLIENT_SECRET: &str = "digikey-sanitized-client-secret-0001";
const LCSC_KEY: &str = "lcsc-sanitized-key-0123456789abcdef";

fn text<const MAX: usize>(raw: &str) -> Text<MAX> {
    Text::<MAX>::new(raw).expect("valid test text")
}

struct Harness {
    transport: Arc<FixtureTransport>,
    quota: Arc<QuotaState>,
    secrets: Arc<SecretGuard>,
    diagnostics: Arc<MemoryDiagnostics>,
    browser: Arc<RecordingBrowser>,
}

impl Harness {
    fn new() -> Self {
        Self {
            transport: Arc::new(FixtureTransport::new()),
            quota: Arc::new(QuotaState::new()),
            secrets: Arc::new(SecretGuard::new()),
            diagnostics: Arc::new(MemoryDiagnostics::new()),
            browser: Arc::new(RecordingBrowser::new()),
        }
    }

    /// The default test context: the runtime planner chose the API mechanism.
    fn ctx(&self) -> AcquireCtx {
        self.ctx_mechanism(Mechanism::OfficialApi)
    }

    /// A context carrying the mechanism the runtime planner chose.
    fn ctx_mechanism(&self, mechanism: Mechanism) -> AcquireCtx {
        AcquireCtx::builder(
            self.transport.clone(),
            self.quota.clone(),
            self.secrets.clone(),
        )
        .clock(Arc::new(ManualClock::new(1_700_000_000_000)))
        .diagnostics(self.diagnostics.clone())
        .browser(self.browser.clone())
        .mechanism(mechanism)
        .build()
    }

    fn credentials() -> Arc<MapCredentials> {
        Arc::new(
            MapCredentials::new()
                .with("FAKTOR_MOUSER_KEY", MOUSER_KEY)
                .with("FAKTOR_DIGIKEY_CLIENT_ID", DIGIKEY_CLIENT_ID)
                .with("FAKTOR_DIGIKEY_CLIENT_SECRET", DIGIKEY_CLIENT_SECRET)
                .with("FAKTOR_LCSC_KEY", LCSC_KEY),
        )
    }

    fn search(&self, query: &str) -> SearchRequest {
        SearchRequest::new(text(query), 5, None).expect("search request")
    }
}

fn mouser_connector(secrets: &SecretGuard) -> MouserConnector {
    MouserConnector::new(
        &MouserConnectorConfig {
            enabled: true,
            api_key_env: Some(text("FAKTOR_MOUSER_KEY")),
        },
        Harness::credentials(),
        secrets,
    )
    .expect("mouser connector")
}

fn digikey_connector(secrets: &Arc<SecretGuard>) -> DigiKeyConnector {
    DigiKeyConnector::new(
        &DigiKeyConnectorConfig {
            enabled: true,
            client_id_env: Some(text("FAKTOR_DIGIKEY_CLIENT_ID")),
            client_secret_env: Some(text("FAKTOR_DIGIKEY_CLIENT_SECRET")),
        },
        Harness::credentials(),
        secrets.clone(),
    )
    .expect("digikey connector")
}

fn lcsc_connector(secrets: &SecretGuard) -> LcscConnector {
    LcscConnector::new(
        &LcscConnectorConfig {
            enabled: true,
            api_key_env: Some(text("FAKTOR_LCSC_KEY")),
        },
        Harness::credentials(),
        secrets,
    )
    .expect("lcsc connector")
}

#[test]
fn every_connector_implements_the_one_object_safe_contract() {
    let secrets = Arc::new(SecretGuard::new());
    let connectors: Vec<Arc<dyn SiteConnector>> = vec![
        Arc::new(mouser_connector(&secrets)),
        Arc::new(digikey_connector(&secrets)),
        Arc::new(lcsc_connector(&secrets)),
        Arc::new(
            AlibabaConnector::new(&ProfileConnectorConfig {
                enabled: true,
                profile: Some(text("procurement-global")),
            })
            .expect("alibaba stub"),
        ),
        Arc::new(
            China1688Connector::new(&ProfileConnectorConfig {
                enabled: true,
                profile: Some(text("procurement-cn")),
            })
            .expect("1688 stub"),
        ),
    ];
    let mut sources: Vec<String> = connectors
        .iter()
        .map(|connector| connector.source().as_str().to_string())
        .collect();
    sources.sort();
    assert_eq!(
        sources,
        vec!["1688", "alibaba", "digikey", "lcsc", "mouser"]
    );

    for connector in &connectors {
        let capabilities: ConnectorCapabilities = connector.capabilities();
        match connector.source().as_str() {
            "mouser" | "digikey" | "lcsc" => {
                assert_eq!(
                    capabilities.level(Capability::Discovery),
                    CapabilityLevel::Supported
                );
                assert_eq!(
                    capabilities.level(Capability::QuantityPricing),
                    CapabilityLevel::Supported
                );
            }
            // The marketplaces are browser-first: their API scopes are
            // configuration-dependent, so the default advertisement is the
            // browser fallback, and capabilities they genuinely do not have
            // stay Unsupported.
            "alibaba" | "1688" => {
                assert_eq!(
                    capabilities.level(Capability::Discovery),
                    CapabilityLevel::Fallback
                );
                assert_eq!(
                    capabilities.level(Capability::ExactProduct),
                    CapabilityLevel::Fallback
                );
                assert_eq!(
                    capabilities.level(Capability::AccountPricing),
                    CapabilityLevel::Unsupported
                );
                assert_eq!(
                    capabilities.level(Capability::Bulk),
                    CapabilityLevel::Unsupported
                );
                assert!(capabilities.mechanisms.contains(&Mechanism::BrowserNetwork));
                assert!(capabilities.mechanisms.contains(&Mechanism::Dom));
            }
            other => panic!("unexpected source {other}"),
        }
    }
}

#[tokio::test]
async fn planted_secrets_never_reach_any_sink() {
    let harness = Harness::new();
    let planted = "SANITIZED-PLANTED-CREDENTIAL-9f3c";
    harness.secrets.register(planted);

    // Every connector receives an upstream failure body that echoes the
    // credential; nothing may forward it.
    harness.transport.push(CannedResponse::with_status(
        500,
        &format!(r#"{{"message":"key {planted} rejected"}}"#),
    ));
    let mouser = mouser_connector(&harness.secrets);
    mouser
        .discover(&harness.ctx(), harness.search("TPS5430DDAR"))
        .await
        .expect_err("mouser 500");

    harness.transport.push(CannedResponse::with_status(
        500,
        &format!(r#"{{"message":"key {planted} rejected"}}"#),
    ));
    let lcsc = lcsc_connector(&harness.secrets);
    lcsc.discover(&harness.ctx(), harness.search("STM32F407VGT6"))
        .await
        .expect_err("lcsc 500");

    harness.transport.push(CannedResponse::with_status(
        401,
        &format!(r#"{{"message":"secret {planted} rejected"}}"#),
    ));
    let digikey = digikey_connector(&harness.secrets);
    digikey
        .discover(&harness.ctx(), harness.search("TPS5430DDAR"))
        .await
        .expect_err("digikey token 401");

    let diagnostics = harness.diagnostics.joined();
    let rendered = harness.transport.rendered_requests();
    for leaked in [planted, MOUSER_KEY, LCSC_KEY, DIGIKEY_CLIENT_SECRET] {
        assert!(!diagnostics.contains(leaked), "diagnostics leaked {leaked}");
        assert!(
            !rendered.contains(leaked),
            "rendered requests leaked {leaked}"
        );
        assert!(
            !format!("{:?}", SourceError::ApiUnavailable).contains(leaked),
            "typed errors must not carry upstream text"
        );
    }
    assert_eq!(harness.browser.calls(), 0);
}

#[tokio::test]
async fn missing_and_disabled_credentials_never_construct_a_connector() {
    let harness = Harness::new();
    let empty: std::sync::Arc<MapCredentials> = std::sync::Arc::new(MapCredentials::new());
    match DigiKeyConnector::new(
        &DigiKeyConnectorConfig {
            enabled: true,
            client_id_env: Some(text("FAKTOR_DIGIKEY_CLIENT_ID")),
            client_secret_env: Some(text("FAKTOR_DIGIKEY_CLIENT_SECRET")),
        },
        empty,
        harness.secrets.clone(),
    ) {
        Err(ConfigError::MissingCredential { env_name }) => {
            assert_eq!(env_name, "FAKTOR_DIGIKEY_CLIENT_ID");
        }
        Ok(_) => panic!("missing credential must be typed"),
        Err(other) => panic!("unexpected error {other:?}"),
    }

    match MouserConnector::new(
        &MouserConnectorConfig {
            enabled: false,
            api_key_env: None,
        },
        Harness::credentials(),
        &harness.secrets,
    ) {
        Err(ConfigError::Disabled { connector }) => assert_eq!(connector, "mouser"),
        Ok(_) => panic!("disabled config must not construct"),
        Err(other) => panic!("unexpected error {other:?}"),
    }
    assert_eq!(harness.transport.request_count(), 0);
}

#[tokio::test]
async fn marketplaces_refuse_typed_without_an_injected_browser() {
    let harness = Harness::new();
    let alibaba = AlibabaConnector::new(&ProfileConnectorConfig {
        enabled: true,
        profile: Some(text("procurement-global")),
    })
    .expect("alibaba");
    let china1688 = China1688Connector::new(&ProfileConnectorConfig {
        enabled: true,
        profile: Some(text("procurement-cn")),
    })
    .expect("1688");
    // Browser-only connectors are planned onto a browser mechanism; with no
    // browser authority injected the refusal is typed.
    let ctx = harness.ctx_mechanism(Mechanism::BrowserNetwork);
    for connector in [
        &alibaba as &dyn SiteConnector,
        &china1688 as &dyn SiteConnector,
    ] {
        assert_eq!(
            connector.discover(&ctx, harness.search("usb cable")).await,
            Err(SourceError::BrowserUnavailable),
            "no browser authority is injected in this context"
        );
        assert_eq!(
            connector
                .product(
                    &ctx,
                    ProductRequest::new(ProductReference::ManufacturerPartNumber(text("ABC-123")))
                )
                .await,
            Err(SourceError::BrowserUnavailable)
        );
        assert_eq!(
            connector
                .quote(
                    &ctx,
                    QuoteRequest::new(
                        ProductReference::ManufacturerPartNumber(text("ABC-123")),
                        faktor_commerce_connectors::NonZeroQuantity::new(10).expect("quantity"),
                    )
                )
                .await,
            Err(SourceError::BrowserUnavailable)
        );
    }
    assert_eq!(harness.transport.request_count(), 0);
    assert_eq!(harness.browser.calls(), 0);
}

#[tokio::test]
async fn api_mechanism_never_bypasses_a_rate_limit_with_the_browser() {
    let harness = Harness::new();
    harness
        .transport
        .push(CannedResponse::with_status(429, "{}"));
    let mouser = mouser_connector(&harness.secrets);
    let error = mouser
        .product(
            &harness.ctx(),
            ProductRequest::new(ProductReference::ManufacturerPartNumber(text(
                "TPS5430DDAR",
            ))),
        )
        .await
        .expect_err("rate limited");
    assert!(matches!(error, SourceError::RateLimited { .. }));
    assert_eq!(
        harness.browser.calls(),
        0,
        "a rate limit is never bypassed with the browser"
    );
}
