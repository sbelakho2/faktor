//! Opt-in live canaries for the marketplace connectors (audit item 8).
//!
//! The CI suites in this crate are all fixture-based and offline. These
//! canaries are the ONE exception and the reason the live path is never
//! unpinned: each canary is `#[ignore]`-gated, assigned to the key-gated
//! `live-paid` lane in `scripts/certification/ignored-tests.json`, and makes
//! ZERO vendor calls unless the operator explicitly provides that source's
//! environment (`faktor_commerce_connectors::canary::LIVE_CANARY_SOURCES`;
//! the same registry the daemon's doctor reports as configured/absent).
//!
//! What a canary records, per operation:
//!
//! * **Structural validity, never golden constants.** Live vendor data
//!   changes; the assertions are the domain's own shape invariants
//!   ([`structural_discovery_check`], [`structural_offer_check`]) — source
//!   consistency, non-empty identity, bounded price ranges, offer
//!   cross-field invariants. No captured vendor value is compared.
//! * **Evidence digests.** A BLAKE3 digest over the secret-scrubbed
//!   structural summary, plus a bounded `faktor-live-canary/v1` line; with
//!   `FAKTOR_LIVE_CANARY_EVIDENCE_DIR` set, the same record is written as
//!   JSON (never contains a credential: the guard scrubs first).
//!
//! Browser canaries (1688/Alibaba) drive the REAL browser authority over the
//! operator's stored logged-in profile: a Chromium launch under the profile
//! directory in `FAKTOR_LIVE_DATA_DIR`, navigation to the connector's own
//! search URL, verification detection, and a bounded DOM capture handed back
//! through the connector's `BrowserExtraction` seam. A detected
//! human-verification state is a PASSING outcome (the profile stops, the
//! connector surfaces the typed error, nothing is bypassed); the legal /
//! compliance posture is documented in
//! `docs/legal/marketplace-acquisition.md`.
//!
//! Optional operator knobs: `FAKTOR_LIVE_QUERY` (bounded query override),
//! `FAKTOR_LIVE_BROWSER_EXECUTABLE` (explicit Chromium path),
//! `FAKTOR_LIVE_NAVIGATION_TIMEOUT_MS` (default 60000, capped 300000),
//! `FAKTOR_LIVE_SETTLE_MS` (post-load settle, default 2000, capped 15000),
//! `FAKTOR_LIVE_CANARY_EVIDENCE_DIR` (evidence JSON directory).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

use faktor_commerce::text::CanonicalUrl;
use faktor_commerce::{SourceError, SourceId};

use faktor_commerce_connectors::canary::{
    canary_source, evidence_digest, evidence_line, structural_discovery_check,
    structural_offer_check, EVIDENCE_SCHEMA,
};
use faktor_commerce_connectors::config::{
    DigiKeyConnectorConfig, LcscConnectorConfig, MouserConnectorConfig, ProfileConnectorConfig,
};
use faktor_commerce_connectors::contract::{
    Mechanism, ProductReference, QuoteRequest, SearchRequest, SiteConnector,
};
use faktor_commerce_connectors::http::{
    HttpRequest, HttpResponse, HttpTransport, ResponseBudget, TransportError,
};
use faktor_commerce_connectors::testing::{MapCredentials, NoopTransport};
use faktor_commerce_connectors::{
    AccessVisibility, AcquireCtx, AlibabaConnector, BrowserExtraction, CaptureBundle, CaptureKind,
    CapturedPayload, Challenge, ChallengeKind, China1688Connector, DigiKeyConnector, LcscConnector,
    ManualClock, MemoryDiagnostics, MouserConnector, NonZeroQuantity, QuotaState, SecretGuard,
    Text,
};

const TEST_NOW_MS: u64 = 1_700_000_000_000;

fn text<const MAX: usize>(raw: &str) -> Text<MAX> {
    Text::<MAX>::new(raw).expect("bounded live canary text")
}

fn search(query: &str) -> SearchRequest {
    SearchRequest::new(text::<512>(query), 5, None).expect("search request")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

fn bounded_env_ms(name: &str, default_ms: u64, cap_ms: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .map(|value| value.min(cap_ms))
        .unwrap_or(default_ms)
}

/// The opt-in gate: every required environment variable must be explicitly
/// provided, or the canary performs no vendor call at all.
struct LiveCanary {
    source: &'static str,
    mechanism: &'static str,
    values: Vec<(&'static str, String)>,
    secrets: Arc<SecretGuard>,
}

impl LiveCanary {
    fn from_env(source_id: &'static str) -> Option<Self> {
        let source = canary_source(source_id).expect("registered canary source");
        let mut values = Vec::new();
        for name in source.required_env {
            match std::env::var(name) {
                Ok(value) if !value.trim().is_empty() => values.push((*name, value)),
                _ => {
                    eprintln!(
                        "[live-canary] {source_id} skipped: {name} is not set (opt-in only; \
                         nothing was requested from the vendor)"
                    );
                    return None;
                }
            }
        }
        let secrets = Arc::new(SecretGuard::new());
        for (_, value) in &values {
            secrets.register(value);
        }
        Some(Self {
            source: source_id,
            mechanism: source.mechanism.as_str(),
            values,
            secrets,
        })
    }

    fn value(&self, name: &str) -> &str {
        self.values
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
            .expect("required canary env")
    }

    fn credentials(&self) -> Arc<MapCredentials> {
        let mut credentials = MapCredentials::new();
        for (name, value) in &self.values {
            credentials = credentials.with(name, value);
        }
        Arc::new(credentials)
    }

    fn query(&self) -> String {
        std::env::var("FAKTOR_LIVE_QUERY")
            .ok()
            .filter(|query| !query.trim().is_empty() && query.len() <= 256)
            .unwrap_or_else(|| match self.source {
                "1688" => "USB 数据线".to_string(),
                "alibaba" => "usb cable".to_string(),
                _ => "STM32F407VGT6".to_string(),
            })
    }
}

/// The API-mechanism context: the real vendor is reached only through the
/// curl-backed transport below, with the operator credential registered with
/// the secret scanner (the same discipline as production).
fn api_ctx(canary: &LiveCanary, transport: Arc<dyn HttpTransport>) -> AcquireCtx {
    AcquireCtx::builder(
        transport,
        Arc::new(QuotaState::new()),
        canary.secrets.clone(),
    )
    .clock(Arc::new(ManualClock::new(TEST_NOW_MS)))
    .diagnostics(Arc::new(MemoryDiagnostics::new()))
    .mechanism(Mechanism::OfficialApi)
    .build()
}

/// Record one operation structurally: a BLAKE3 evidence digest over the
/// secret-scrubbed payload plus a bounded evidence line (and JSON when the
/// operator names an evidence directory).
fn record_evidence(
    canary: &LiveCanary,
    operation: &str,
    outcome: &str,
    observed: &[String],
    payload: &str,
) {
    let scrubbed = canary.secrets.scrub(payload);
    let digest = evidence_digest(&format!("{EVIDENCE_SCHEMA}\n{scrubbed}"));
    let line = evidence_line(
        canary.source,
        canary.mechanism,
        operation,
        outcome,
        &digest,
        observed,
    );
    println!("[live-canary] {line}");
    if let Ok(dir) = std::env::var("FAKTOR_LIVE_CANARY_EVIDENCE_DIR") {
        let dir = PathBuf::from(dir);
        if std::fs::create_dir_all(&dir).is_ok() {
            let record = serde_json::json!({
                "schema": EVIDENCE_SCHEMA,
                "source": canary.source,
                "mechanism": canary.mechanism,
                "operation": operation,
                "outcome": outcome,
                "digest": digest,
                "observed": observed,
                "observed_at_ms": now_ms(),
            });
            let path = dir.join(format!("{}-{operation}.json", canary.source));
            let _ = std::fs::write(path, serde_json::to_vec_pretty(&record).unwrap_or_default());
        }
    }
}

/// Structural labels only: shapes, counts and booleans — never vendor values
/// and never anything credential-shaped.
fn discovery_labels(discoveries: &[faktor_commerce_connectors::Discovery]) -> Vec<String> {
    discoveries
        .iter()
        .take(8)
        .map(|discovery| {
            format!(
                "listing(title_bytes={},identity={},url={},price_hint={},stock_hint={},origin={:?})",
                discovery.title.as_str().len(),
                !discovery.identity.is_empty(),
                discovery.url.is_some(),
                discovery.price_hint.is_some(),
                discovery.stock_hint.is_some(),
                discovery.origin,
            )
        })
        .collect()
}

/// The structural summary a live digest hashes: shapes, counts and enum
/// labels only — never a vendor value, never a credential.
fn discovery_summary(discoveries: &[faktor_commerce_connectors::Discovery]) -> String {
    let items: Vec<serde_json::Value> = discoveries
        .iter()
        .take(8)
        .map(|discovery| {
            serde_json::json!({
                "title_bytes": discovery.title.as_str().len(),
                "identity_empty": discovery.identity.is_empty(),
                "has_url": discovery.url.is_some(),
                "has_price_hint": discovery.price_hint.is_some(),
                "price_visibility": format!("{:?}", discovery.price_visibility),
                "stock_hint": format!("{:?}", discovery.stock_hint),
                "origin": format!("{:?}", discovery.origin),
                "observed_at_ms": discovery.observed_at_ms,
            })
        })
        .collect();
    serde_json::to_string(&items).unwrap_or_default()
}

/// No operator credential may surface in any rendered live result or
/// evidence: the same discipline the offline suites prove on fixtures.
fn assert_no_secret_leak(canary: &LiveCanary, rendered: &str) {
    for (name, value) in &canary.values {
        if value.len() >= 4 {
            assert!(
                !rendered.contains(value),
                "{}: live result leaked the credential named by {name}",
                canary.source
            );
        }
    }
}

/// The strongest reference a discovery offers, for a live quote follow-up.
fn reference_of(discovery: &faktor_commerce_connectors::Discovery) -> Option<ProductReference> {
    if let Some(mpn) = &discovery.identity.manufacturer_part_number {
        return Some(ProductReference::ManufacturerPartNumber(mpn.clone()));
    }
    if let Some(offer_id) = &discovery.identity.offer_id {
        return Some(ProductReference::OfferId(offer_id.clone()));
    }
    discovery.url.clone().map(ProductReference::Url)
}

/// Run discover + quote against one API connector and assert only structural
/// validity. Shared by Mouser/DigiKey/LCSC so the three canaries cannot drift
/// in what they certify.
async fn api_canary(
    canary: &LiveCanary,
    connector: &dyn SiteConnector,
    transport: Arc<dyn HttpTransport>,
) {
    let ctx = api_ctx(canary, transport);
    let query = canary.query();
    let discoveries = connector
        .discover(&ctx, search(&query))
        .await
        .unwrap_or_else(|error| {
            panic!(
                "{}: live discover failed: {}",
                canary.source,
                error.as_str()
            )
        });
    structural_discovery_check(&connector.source(), &discoveries).unwrap_or_else(|error| {
        panic!(
            "{}: live discoveries are structurally invalid: {error}",
            canary.source
        )
    });
    assert_no_secret_leak(canary, &format!("{discoveries:?}"));
    record_evidence(
        canary,
        "discover",
        "ok",
        &discovery_labels(&discoveries),
        &discovery_summary(&discoveries),
    );

    let Some(reference) = reference_of(&discoveries[0]) else {
        record_evidence(
            canary,
            "quote",
            "skipped-no-reference",
            &["listing has no usable product reference".to_string()],
            "",
        );
        return;
    };
    let candidates = connector
        .quote(&ctx, QuoteRequest::new(reference, NonZeroQuantity::ONE))
        .await
        .unwrap_or_else(|error| panic!("{}: live quote failed: {}", canary.source, error.as_str()));
    if candidates.is_empty() {
        panic!(
            "{}: live quote returned no candidate for quantity 1",
            canary.source
        );
    }
    let mut labels = Vec::new();
    for candidate in &candidates {
        structural_offer_check(&connector.source(), &candidate.offer).unwrap_or_else(|error| {
            panic!(
                "{}: live offer is structurally invalid: {error}",
                canary.source
            )
        });
        assert_eq!(
            candidate.resolution.requested_quantity.as_u64(),
            1,
            "{}: the live resolution must echo the requested quantity",
            canary.source
        );
        labels.push(format!(
            "candidate(status={:?},unit_price={},visible={},offer_tiers={})",
            candidate.resolution.status,
            candidate.resolution.unit_price.is_some(),
            !candidate.offer.price_breaks.is_empty(),
            candidate.offer.price_breaks.len(),
        ));
    }
    let summary = serde_json::json!({
        "candidates": candidates.len(),
        "resolution_status": format!("{:?}", candidates[0].resolution.status),
        "requested_quantity": candidates[0].resolution.requested_quantity.as_u64(),
    });
    record_evidence(
        canary,
        "quote",
        "ok",
        &labels,
        &serde_json::to_string(&summary).unwrap_or_default(),
    );
}

/// `[live-paid]` Mouser: real Search API discovery + part-number quote.
#[tokio::test]
#[ignore = "[live-paid] live Mouser canary (network; set FAKTOR_LIVE_MOUSER_KEY; never part of the default CI run)"]
async fn live_paid_mouser_canary() {
    let Some(canary) = LiveCanary::from_env("mouser") else {
        return;
    };
    let connector = MouserConnector::new(
        &MouserConnectorConfig {
            enabled: true,
            api_key_env: Some(text("FAKTOR_LIVE_MOUSER_KEY")),
        },
        canary.credentials(),
        &canary.secrets,
    )
    .expect("live mouser connector");
    api_canary(&canary, &connector, Arc::new(CurlTransport)).await;
}

/// `[live-paid]` DigiKey: OAuth token + KeywordSearch discovery + live
/// ProductDetails/PricingOptionsByQuantity quote.
#[tokio::test]
#[ignore = "[live-paid] live DigiKey canary (network; set FAKTOR_LIVE_DIGIKEY_CLIENT_ID and FAKTOR_LIVE_DIGIKEY_CLIENT_SECRET; never part of the default CI run)"]
async fn live_paid_digikey_canary() {
    let Some(canary) = LiveCanary::from_env("digikey") else {
        return;
    };
    let connector = DigiKeyConnector::new(
        &DigiKeyConnectorConfig {
            enabled: true,
            client_id_env: Some(text("FAKTOR_LIVE_DIGIKEY_CLIENT_ID")),
            client_secret_env: Some(text("FAKTOR_LIVE_DIGIKEY_CLIENT_SECRET")),
        },
        canary.credentials(),
        canary.secrets.clone(),
    )
    .expect("live digikey connector");
    api_canary(&canary, &connector, Arc::new(CurlTransport)).await;
}

/// `[live-paid]` LCSC: real `wmsc` Search API discovery + ProductDetails
/// quote with the live discounted tiers.
#[tokio::test]
#[ignore = "[live-paid] live LCSC canary (network; set FAKTOR_LIVE_LCSC_KEY; never part of the default CI run)"]
async fn live_paid_lcsc_canary() {
    let Some(canary) = LiveCanary::from_env("lcsc") else {
        return;
    };
    let connector = LcscConnector::new(
        &LcscConnectorConfig {
            enabled: true,
            api_key_env: Some(text("FAKTOR_LIVE_LCSC_KEY")),
        },
        canary.credentials(),
        &canary.secrets,
    )
    .expect("live lcsc connector");
    api_canary(&canary, &connector, Arc::new(CurlTransport)).await;
}

/// `[soak]` 1688: the real browser authority over the operator's stored
/// logged-in profile, navigated to the connector's own search URL. A
/// detected human-verification state is the documented stop outcome and
/// passes with recorded evidence; anything else must be a structurally
/// valid discovery set.
#[tokio::test]
#[ignore = "[soak] live 1688 browser canary (network + stored profile; set FAKTOR_LIVE_1688_PROFILE and FAKTOR_LIVE_DATA_DIR; never part of the default CI run)"]
async fn soak_live_china1688_browser_canary() {
    let Some(canary) = LiveCanary::from_env("1688") else {
        return;
    };
    let connector = China1688Connector::new(&ProfileConnectorConfig {
        enabled: true,
        profile: Some(text(canary.value("FAKTOR_LIVE_1688_PROFILE"))),
    })
    .expect("live 1688 connector");
    browser_canary(&canary, &connector).await;
}

/// `[soak]` Alibaba: same stored-profile browser canary on Alibaba's own
/// search URL and extraction logic.
#[tokio::test]
#[ignore = "[soak] live Alibaba browser canary (network + stored profile; set FAKTOR_LIVE_ALIBABA_PROFILE and FAKTOR_LIVE_DATA_DIR; never part of the default CI run)"]
async fn soak_live_alibaba_browser_canary() {
    let Some(canary) = LiveCanary::from_env("alibaba") else {
        return;
    };
    let connector = AlibabaConnector::new(&ProfileConnectorConfig {
        enabled: true,
        profile: Some(text(canary.value("FAKTOR_LIVE_ALIBABA_PROFILE"))),
    })
    .expect("live alibaba connector");
    browser_canary(&canary, &connector).await;
}

async fn browser_canary(canary: &LiveCanary, connector: &dyn SiteConnector) {
    let extraction = Arc::new(
        LiveBrowserExtraction::new(PathBuf::from(canary.value("FAKTOR_LIVE_DATA_DIR")))
            .unwrap_or_else(|error| {
                panic!(
                    "{}: live browser harness unavailable: {error}",
                    canary.source
                )
            }),
    );
    let ctx = AcquireCtx::builder(
        Arc::new(NoopTransport),
        Arc::new(QuotaState::new()),
        canary.secrets.clone(),
    )
    .clock(Arc::new(ManualClock::new(TEST_NOW_MS)))
    .diagnostics(Arc::new(MemoryDiagnostics::new()))
    .mechanism(Mechanism::Dom)
    .browser_extraction(extraction)
    .build();
    let query = canary.query();
    match connector.discover(&ctx, search(&query)).await {
        Ok(discoveries) => {
            structural_discovery_check(&connector.source(), &discoveries).unwrap_or_else(|error| {
                panic!(
                    "{}: live discoveries are structurally invalid: {error}",
                    canary.source
                )
            });
            assert_no_secret_leak(canary, &format!("{discoveries:?}"));
            record_evidence(
                canary,
                "discover",
                "ok",
                &discovery_labels(&discoveries),
                &discovery_summary(&discoveries),
            );
        }
        Err(error)
            if matches!(
                error,
                SourceError::VerificationRequired { .. }
                    | SourceError::AuthenticationRequired
                    | SourceError::RateLimited { .. }
            ) =>
        {
            // The documented human-verification flow: the stored profile was
            // stopped by the vendor's interstitial and the connector surfaced
            // the typed state. Nothing is bypassed; a human may complete the
            // verification in the profile and re-run.
            record_evidence(
                canary,
                "discover",
                "verification-required",
                &[format!("typed_error={}", error.as_str())],
                &format!("{error:?}"),
            );
        }
        Err(error) => panic!(
            "{}: live browser canary failed: {}",
            canary.source,
            error.as_str()
        ),
    }
}

/// A minimal curl-backed transport for the opt-in API canaries only. It is
/// never compiled into the library and performs no I/O unless a canary is
/// explicitly run with an operator credential.
struct CurlTransport;

#[async_trait]
impl HttpTransport for CurlTransport {
    async fn execute(
        &self,
        request: HttpRequest,
        _budget: ResponseBudget,
    ) -> Result<HttpResponse, TransportError> {
        use std::io::Write as _;
        let method = match request.method() {
            faktor_commerce_connectors::http::HttpMethod::Get => "GET",
            faktor_commerce_connectors::http::HttpMethod::Post => "POST",
        };
        let mut command = std::process::Command::new("curl");
        command
            .arg("-sS")
            .arg("--max-time")
            .arg("30")
            .arg("-X")
            .arg(method)
            .arg("-w")
            .arg("\n%{http_code}")
            .arg(request.url().as_str());
        for header in request.headers() {
            command
                .arg("-H")
                .arg(format!("{}: {}", header.name(), header.value()));
        }
        if request.body().is_some() {
            command.arg("--data-binary").arg("@-");
            command.stdin(std::process::Stdio::piped());
        }
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|_| TransportError::EgressUnavailable)?;
        if let Some(body) = request.body() {
            if let Some(mut stdin) = child.stdin.take() {
                stdin
                    .write_all(body)
                    .map_err(|_| TransportError::EgressUnavailable)?;
            }
        }
        let output = child
            .wait_with_output()
            .map_err(|_| TransportError::EgressUnavailable)?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let (body, status) = stdout
            .rsplit_once('\n')
            .ok_or(TransportError::EgressUnavailable)?;
        let status: u16 = status
            .trim()
            .parse()
            .map_err(|_| TransportError::EgressUnavailable)?;
        HttpResponse::new(status, Vec::new(), body.as_bytes().to_vec())
            .map_err(|_| TransportError::ResponseTooLarge)
    }
}

/// The live browser extraction: the real Chromium authority over the
/// operator's stored profile, with verification detection and a bounded DOM
/// capture. This is test-harness code — the connector crate still never
/// launches Chromium.
struct LiveBrowserExtraction {
    manager: Arc<faktor_browser::BrowserManager>,
}

impl LiveBrowserExtraction {
    fn new(data_dir: PathBuf) -> Result<Self, String> {
        let cas = Arc::new(
            faktor_cas::Cas::open(data_dir.join("live-canary-cas"))
                .map_err(|error| format!("cannot open the live canary CAS: {error}"))?,
        );
        let supervisor = faktor_terminal::ProcessSupervisor::new(cas);
        let mut config = faktor_browser::BrowserConfig {
            enabled: true,
            headless: true,
            ..faktor_browser::BrowserConfig::default()
        };
        if let Ok(executable) = std::env::var("FAKTOR_LIVE_BROWSER_EXECUTABLE") {
            if !executable.trim().is_empty() {
                config.executable = Some(PathBuf::from(executable));
            }
        }
        // Keep the DOM capture inside one CapturedPayload (MAX_CAPTURE_BYTES).
        config.capture.max_dom_bytes = 512 * 1024;
        config
            .validate()
            .map_err(|error| format!("browser config refused: {error}"))?;
        let manager = faktor_browser::BrowserManager::new(supervisor, config, &data_dir)
            .map_err(|error| format!("browser authority unavailable: {error}"))?;
        Ok(Self { manager })
    }
}

#[async_trait]
impl BrowserExtraction for LiveBrowserExtraction {
    async fn capture(
        &self,
        _ctx: &AcquireCtx,
        source: &SourceId,
        profile: &faktor_commerce::connector::ProfileIdentity,
        url: &CanonicalUrl,
    ) -> Result<CaptureBundle, SourceError> {
        let timeout_ms = bounded_env_ms("FAKTOR_LIVE_NAVIGATION_TIMEOUT_MS", 60_000, 300_000);
        let settle_ms = bounded_env_ms("FAKTOR_LIVE_SETTLE_MS", 2_000, 15_000);
        let deadline =
            faktor_core::time::Deadline::now_plus(&faktor_core::time::SystemClock, timeout_ms);
        let cancel = faktor_core::cancellation::CancellationToken::new();
        let first_party: &[&str] = if source.as_str() == "1688" {
            &["1688.com"]
        } else {
            &["alibaba.com"]
        };
        let patterns = first_party
            .iter()
            .map(|host| {
                faktor_browser::HostPattern::parse(host).map_err(|_| SourceError::InvalidRequest)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let policy = faktor_browser::DestinationPolicy::first_party_only(patterns);
        let account = profile
            .account
            .as_ref()
            .map(|scope| scope.as_str().to_string())
            .unwrap_or_else(|| "live-canary".to_string());
        let identity =
            faktor_browser::BrowserIdentity::new(account, profile.profile.as_str(), "direct");
        let page = self
            .manager
            .acquire_page(
                source.as_str(),
                &identity,
                policy,
                &faktor_browser::PagePurpose::new("live-canary"),
                deadline,
                &cancel,
            )
            .await
            .map_err(browser_error)?;
        let outcome = page
            .navigate(url.as_str(), deadline, &cancel)
            .await
            .map_err(browser_error)?;
        tokio::time::sleep(Duration::from_millis(settle_ms)).await;
        let signal = page
            .detect_verification(deadline, &cancel)
            .await
            .map_err(browser_error)?;
        let mut payloads = Vec::new();
        if signal.is_none() {
            let html = page
                .document_html(deadline, &cancel)
                .await
                .map_err(browser_error)?;
            if !html.text.is_empty() {
                let final_url = CanonicalUrl::parse(&outcome.url).unwrap_or_else(|_| url.clone());
                payloads.push(
                    CapturedPayload::new(
                        CaptureKind::StructuredMarkup,
                        final_url,
                        Some(text::<64>("text/html")),
                        html.text.into_bytes(),
                        now_ms(),
                    )
                    .map_err(|_| SourceError::ResponseTooLarge)?,
                );
            }
        }
        let challenge = signal
            .map(|signal| Challenge::new(challenge_kind(signal.kind()), signal.kind().as_str()));
        let bundle = CaptureBundle::new_with_access(
            CanonicalUrl::parse(&outcome.url).unwrap_or_else(|_| url.clone()),
            payloads,
            challenge,
            now_ms(),
            // The capture was made through the operator's stored logged-in
            // profile: the authority is authenticated, never public.
            AccessVisibility::Authenticated,
        )?;
        if let Err(error) = page.close().await {
            eprintln!("[live-canary] page close failed: {error}");
        }
        Ok(bundle)
    }
}

fn challenge_kind(kind: faktor_browser::VerificationKind) -> ChallengeKind {
    match kind {
        faktor_browser::VerificationKind::LoginForm => ChallengeKind::LoginForm,
        faktor_browser::VerificationKind::Captcha => ChallengeKind::Captcha,
        faktor_browser::VerificationKind::SecuritySlider => ChallengeKind::SecuritySlider,
        faktor_browser::VerificationKind::AccessDenied => ChallengeKind::AccessDenied,
        faktor_browser::VerificationKind::RateLimit => ChallengeKind::RateLimitPage,
        faktor_browser::VerificationKind::Interstitial
        | faktor_browser::VerificationKind::Unknown => ChallengeKind::VerificationInterstitial,
    }
}

fn browser_error(error: faktor_browser::BrowserError) -> SourceError {
    use faktor_browser::BrowserError as Browser;
    match error {
        Browser::Cancelled => SourceError::Cancelled,
        Browser::Deadline { .. } | Browser::LaunchTimeout { .. } => SourceError::Deadline,
        Browser::AuthenticationRequired => SourceError::AuthenticationRequired,
        Browser::RateLimited { retry_after_ms } => SourceError::RateLimited {
            retry_after_ms: retry_after_ms.unwrap_or(60_000),
        },
        Browser::BrowserCrashed { .. } => SourceError::BrowserCrashed,
        Browser::ResponseTooLarge { .. }
        | Browser::EventStreamLagged { .. }
        | Browser::Bound { .. } => SourceError::ResponseTooLarge,
        Browser::VerificationRequired { kind } => SourceError::VerificationRequired {
            kind: match kind {
                faktor_browser::VerificationKind::Captcha => {
                    faktor_commerce::error::VerificationKind::Captcha
                }
                faktor_browser::VerificationKind::LoginForm => {
                    faktor_commerce::error::VerificationKind::Login
                }
                _ => faktor_commerce::error::VerificationKind::Manual,
            },
        },
        _ => SourceError::BrowserUnavailable,
    }
}
