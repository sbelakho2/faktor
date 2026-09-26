//! Cross-layer production-wiring certification: Faktor Acquire
//! (`source_market`) from the DAEMON'S graph.
//!
//! Every test builds the graph with `wiring::build_production_graph` (the
//! executable's own `build_daemon` -> `build_daemon_core`) and then drives
//! the tool the daemon's OWN registry holds (`wiring::daemon_tool`) against
//! the daemon's OWN `CommerceSourceService`. The only substitution is the
//! external site adapter, registered through the production
//! `Registered<SiteConnector>` bridge with the production identity injection
//! (`ConnectorRuntime::with_identity`) — so these tests fail if the daemon
//! stops supplying the service, the cache, the identity or the authority,
//! even when the adapter itself is a fixture.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use faktor_agent::{Tool, ToolCallMode, ToolOutcome, ToolRunCtx};
use faktor_commerce::connector::{AcquireCtx, ConnectorPolicy};
use faktor_commerce::offer::{
    CommercialOffer, LifecycleStatus, ObservationOrigin, OfferProvenance, PriceBreak,
    PriceVisibility, StockState,
};
use faktor_commerce::query::FreshnessMode;
use faktor_commerce::quote::{price_at_quantity, VariantRequest};
use faktor_commerce::service::CommerceSourceService;
use faktor_commerce::text::AccountScope;
use faktor_commerce::{
    CacheClass, CacheIdentity, Currency, Freshness, Money, NonZeroQuantity, PackagingType,
    PricingScope, ProductIdentity, Quantity, SourceId, Text, VariantId,
};
use faktor_commerce_connectors::contract::{
    CapabilityLevel, ConnectorCapabilities, Discovery, Mechanism, ProductRequest, QuoteCandidate,
    QuoteRequest, SearchRequest, SiteConnector,
};
use faktor_commerce_connectors::testing::NoopTransport;
use faktor_commerce_connectors::{
    AccessVisibility, ConnectorIdentity, ConnectorRuntime, ManualClock, NoopDiagnostics,
    QuotaState, Registered, SecretGuard,
};
use faktor_core::id::{OpId, SessionId, TaskId, WorkspaceId, WorktreeId};
use faktor_core::WorkspaceIdentity;
use serde_json::{json, Value};

use faktor_tests_production_wiring::wiring::{self, CommerceCfg, Config};

const NOW_MS: u64 = 1_700_000_000_000;
const SOURCE: &str = "mouser";
const REF: &str = "TPS5430DDAR";

// ------------------------------------------------------------------ fixture

/// What one fake acquisition observed.
#[derive(Default)]
struct Calls {
    quote: u64,
    product: u64,
    captured_quotes: Vec<QuoteRequest>,
    mechanisms: Vec<Option<Mechanism>>,
}

/// The fixture site adapter: it advertises the DirectHttp API-first
/// mechanism, derives its offer visibility from the CONFIGURED identity the
/// production bridge injects into the rich context (exactly like the real
/// alibaba/1688 adapters via `AccessVisibility::from_identity`), and records
/// every observation the daemon's service handed it. The handle is shared
/// with the registered clone so the test observes the exact instance the
/// bridge runs.
#[derive(Clone, Default)]
struct FixtureSite {
    calls: Arc<Mutex<Calls>>,
    capabilities_calls: Arc<AtomicU64>,
}

impl FixtureSite {
    fn new() -> Self {
        Self::default()
    }

    fn capabilities_calls(&self) -> u64 {
        self.capabilities_calls.load(Ordering::SeqCst)
    }

    fn quote_calls(&self) -> u64 {
        self.calls.lock().unwrap().quote
    }

    fn captured_quotes(&self) -> Vec<QuoteRequest> {
        self.calls.lock().unwrap().captured_quotes.clone()
    }
}

fn offer_for(ctx: &faktor_commerce_connectors::AcquireCtx) -> CommercialOffer {
    let source = SourceId::new(SOURCE).unwrap();
    let access = AccessVisibility::from_identity(ctx.identity());
    let account = access.account_scope().cloned();
    let visibility = access.price_visibility();
    CommercialOffer {
        source: source.clone(),
        identity: ProductIdentity {
            manufacturer: Some(Text::<128>::new("Texas Instruments").unwrap()),
            manufacturer_part_number: Some(Text::<256>::new(REF).unwrap()),
            source_part_number: None,
            offer_id: Some(Text::<512>::new("offer-1").unwrap()),
            canonical_url: None,
            category: None,
        },
        title: Text::<512>::new("TPS5430 3A step-down converter").unwrap(),
        description: None,
        currency: Currency::USD,
        price_breaks: vec![PriceBreak {
            min_quantity: NonZeroQuantity::new(1).unwrap(),
            max_quantity: None,
            unit_price: Money::from_micros(Currency::USD, 18_200),
            visibility,
            account_scope: account.clone(),
            promotion: None,
        }],
        variants: Vec::new(),
        moq: Some(NonZeroQuantity::new(1).unwrap()),
        order_multiple: None,
        standard_pack: None,
        stock: StockState::InStock {
            quantity: NonZeroQuantity::new(5_000).unwrap(),
        },
        lead_time: None,
        packaging: Vec::new(),
        supplier: None,
        manufacturer: None,
        provenance: OfferProvenance {
            origin: ObservationOrigin::OfficialApi,
            source,
            extractor_version: None,
            connector_version: None,
            normalization_version: None,
            content_digest: None,
            account_scope: account,
            locale: None,
            market: None,
            source_confidence_bp: None,
        },
        observed_at_ms: NOW_MS,
        price_visibility: visibility,
        lifecycle: LifecycleStatus::Active,
    }
}

#[async_trait::async_trait]
impl SiteConnector for FixtureSite {
    fn source(&self) -> SourceId {
        SourceId::new(SOURCE).unwrap()
    }

    fn capabilities(&self) -> ConnectorCapabilities {
        self.capabilities_calls.fetch_add(1, Ordering::SeqCst);
        ConnectorCapabilities {
            source: self.source(),
            discovery: CapabilityLevel::Supported,
            exact_product: CapabilityLevel::Supported,
            quantity_pricing: CapabilityLevel::Supported,
            stock: CapabilityLevel::Fallback,
            packaging: CapabilityLevel::Unsupported,
            account_pricing: CapabilityLevel::Unsupported,
            supplier_data: CapabilityLevel::Unsupported,
            bulk: CapabilityLevel::Unsupported,
            mechanisms: vec![Mechanism::DirectHttp],
        }
    }

    async fn discover(
        &self,
        _ctx: &faktor_commerce_connectors::AcquireCtx,
        _req: SearchRequest,
    ) -> Result<Vec<Discovery>, faktor_commerce::SourceError> {
        Err(faktor_commerce::SourceError::ProductNotFound)
    }

    async fn product(
        &self,
        ctx: &faktor_commerce_connectors::AcquireCtx,
        _req: ProductRequest,
    ) -> Result<CommercialOffer, faktor_commerce::SourceError> {
        self.calls.lock().unwrap().product += 1;
        Ok(offer_for(ctx))
    }

    async fn quote(
        &self,
        ctx: &faktor_commerce_connectors::AcquireCtx,
        req: QuoteRequest,
    ) -> Result<Vec<QuoteCandidate>, faktor_commerce::SourceError> {
        let quantity = req.quantity().get();
        let mut calls = self.calls.lock().unwrap();
        calls.quote += 1;
        calls.captured_quotes.push(req.clone());
        calls.mechanisms.push(ctx.mechanism());
        drop(calls);
        let offer = offer_for(ctx);
        let resolution = price_at_quantity(
            &offer,
            VariantRequest::None,
            Quantity::new(quantity).unwrap(),
        );
        Ok(vec![QuoteCandidate {
            source: SourceId::new(SOURCE).unwrap(),
            offer,
            resolution,
            freshness: Freshness::Live,
        }])
    }
}

// ------------------------------------------------------------------ helpers

/// A spy planner: counts every `plan()` call and delegates to the production
/// default planner, so the daemon's decisions stay identical while the test
/// observes that the REAL `build_daemon_core` assembly (not a hand-built
/// service) actually ran the planner.
struct SpyPlanner {
    calls: Arc<AtomicU64>,
    inner: faktor_acquire::AcquisitionPlanner,
}

impl faktor_acquire::AcquisitionPlanning for SpyPlanner {
    fn plan(
        &self,
        state: &faktor_acquire::RuntimeAcquisitionState,
    ) -> faktor_acquire::AcquisitionPlan {
        self.calls.fetch_add(1, Ordering::SeqCst);
        faktor_acquire::RuntimeAcquisitionState::plan(state, &self.inner)
    }
}

fn commerce_config() -> Config {
    Config {
        commerce: CommerceCfg {
            enabled: true,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn runtime_with(identity: ConnectorIdentity) -> ConnectorRuntime {
    ConnectorRuntime::new(
        Arc::new(NoopTransport),
        Arc::new(QuotaState::new()),
        Arc::new(SecretGuard::new()),
        Arc::new(NoopDiagnostics),
        Arc::new(ManualClock::new(NOW_MS)),
    )
    .with_identity(identity)
}

fn register_fixture(service: &CommerceSourceService, identity: ConnectorIdentity) -> FixtureSite {
    let site = FixtureSite::new();
    service
        .register(
            Arc::new(Registered::new(site.clone(), runtime_with(identity))),
            ConnectorPolicy {
                api_enabled: true,
                browser_enabled: false,
                ..Default::default()
            },
        )
        .expect("the daemon service must accept the registered site adapter");
    site
}

fn tool_ctx(session: u64, workspace: u64) -> ToolRunCtx {
    use faktor_core::cancellation::CancellationToken;
    ToolRunCtx {
        session_id: SessionId::new(session),
        op_id: OpId::new(1),
        identity: WorkspaceIdentity::new(
            WorkspaceId::new(workspace),
            WorktreeId::new(workspace),
            TaskId::new(1),
        ),
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

async fn call_tool(
    tool: &Tool,
    ctx: ToolRunCtx,
    args: Value,
) -> Result<ToolOutcome, faktor_core::Error> {
    (tool.execute)(ctx, args).await
}

fn product_key() -> String {
    faktor_commerce::query::ProductRef::parse(REF, Some(&SourceId::new(SOURCE).unwrap()))
        .unwrap()
        .identity_key()
}

fn quote_args(freshness: &str) -> Value {
    json!({
        "op": "quote",
        "ref": REF,
        "qty": 100,
        "sources": [SOURCE],
        "freshness": freshness,
    })
}

/// The daemon graph + its ONE commerce service + the registered fixture, for
/// one test.
struct Daemon {
    _dir: tempfile::TempDir,
    graph: wiring::DaemonGraph,
    service: Arc<CommerceSourceService>,
}

fn daemon(identity: ConnectorIdentity) -> (Daemon, FixtureSite) {
    daemon_with_planner(identity, None)
}

/// The production graph built through `build_daemon_core` with an explicit
/// planner seam: the daemon's own construction opens the service with the
/// injected planner, so every later `source_market` execution plans through
/// it.
fn daemon_with_planner(
    identity: ConnectorIdentity,
    planner: Option<Arc<dyn faktor_acquire::AcquisitionPlanning>>,
) -> (Daemon, FixtureSite) {
    let dir = tempfile::tempdir().unwrap();
    let graph = wiring::build_production_graph_with_planner(dir.path(), commerce_config(), planner)
        .expect("the production daemon graph must build");
    let service = wiring::commerce_service(&graph).expect("commerce is enabled by config");
    let site = register_fixture(&service, identity);
    (
        Daemon {
            _dir: dir,
            graph,
            service,
        },
        site,
    )
}

// ------------------------------------------------------ 1. planner is live

/// The daemon's `source_market` quote executes the PLANNER'S decisions: the
/// chosen mechanism injected into the adapter context is the planner's
/// `Acquire { mechanism }`, and a fresh cache entry turns the next quote into
/// the planner's `ServeFromCache` (zero adapter calls). The graph is built
/// through the daemon's own `build_daemon_core` with a SPY planner installed
/// by that construction, so this fails if the daemon ever wired a service
/// that bypassed `AcquisitionPlanner` (the spy would never be called) or
/// stopped executing its decisions.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_tool_quote_runs_the_production_planner_decisions() {
    let calls = Arc::new(AtomicU64::new(0));
    let (daemon, site) = daemon_with_planner(
        ConnectorIdentity::anonymous(),
        Some(Arc::new(SpyPlanner {
            calls: calls.clone(),
            inner: faktor_acquire::AcquisitionPlanner::default(),
        })),
    );
    let tool = wiring::daemon_tool(&daemon.graph, "source_market");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "daemon construction alone plans nothing"
    );

    // Live quote: the adapter is dispatched with the planner-selected
    // mechanism (DirectHttp, the first advertised usable path).
    let first = call_tool(&tool, tool_ctx(1, 1), quote_args("live"))
        .await
        .expect("live quote through the daemon tool");
    assert!(first.text.contains("\"op\":\"quote\""), "{}", first.text);
    assert_eq!(site.quote_calls(), 1, "one live acquisition");
    assert!(
        calls.load(Ordering::SeqCst) >= 1,
        "the daemon's build_daemon_core commerce path must invoke the injected planner"
    );
    let mechanisms = site.calls.lock().unwrap().mechanisms.clone();
    assert_eq!(
        mechanisms,
        vec![Some(Mechanism::DirectHttp)],
        "the planner's chosen mechanism must be the one the adapter executes"
    );
    assert!(
        site.capabilities_calls() >= 1,
        "the planning path must consult the registered capabilities"
    );

    // Prefer-cache on the now-fresh row: the planner serves from cache, so
    // the adapter is NOT called again even though planning still happens.
    let planned_before = calls.load(Ordering::SeqCst);
    let capabilities_before = site.capabilities_calls();
    let second = call_tool(&tool, tool_ctx(1, 1), quote_args("prefer_cache"))
        .await
        .expect("prefer-cache quote through the daemon tool");
    assert!(
        second.text.contains("fresh_cache") || second.text.contains("FreshCache"),
        "the second quote must report cached freshness: {}",
        second.text
    );
    assert_eq!(
        site.quote_calls(),
        1,
        "the planner's ServeFromCache decision must prevent a second acquisition"
    );
    assert!(
        site.capabilities_calls() > capabilities_before,
        "the second quote must still plan (capabilities consulted)"
    );
    assert!(
        calls.load(Ordering::SeqCst) > planned_before,
        "the cached quote must still run the planner through the daemon path"
    );
}

// ------------------------------------- 2. configured identity -> cache identity

/// The marketplace account scope configured on the adapter identity is what
/// keys the daemon's cache row: the row is stored under `Account`, the
/// account scope is the configured one, and an anonymous caller (the tool
/// path, whose context carries no account) gets a typed cache miss instead
/// of the account price. The same daemon-held service served the
/// account-scoped lookup from its cache.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_account_scope_reaches_cache_identity() {
    let account = AccountScope::new("acct-a").unwrap();
    let identity = ConnectorIdentity::anonymous().with_account_scope(account.clone());
    let (daemon, site) = daemon(identity);
    let tool = wiring::daemon_tool(&daemon.graph, "source_market");

    let live = call_tool(&tool, tool_ctx(1, 1), quote_args("live"))
        .await
        .expect("live account-scoped quote");
    assert!(
        live.text.contains("\"status\":\"completed\""),
        "{}",
        live.text
    );
    assert_eq!(site.quote_calls(), 1);

    // Anonymous tool ctx (no account scope): cache_only must be a typed
    // miss — the account row may never be served to the weaker caller.
    let anon = call_tool(&tool, tool_ctx(1, 1), quote_args("cache_only")).await;
    let error = anon.expect_err("anonymous ctx must not be served the account row");
    let rendered = format!("{error:?}");
    assert!(
        rendered.to_ascii_lowercase().contains("cache"),
        "the refusal must be a typed cache miss: {rendered}"
    );
    assert_eq!(
        site.quote_calls(),
        1,
        "no acquisition may happen for the miss"
    );

    // The account-scoped lookup on the SAME daemon service hits the cache.
    let account_ctx = AcquireCtx::new()
        .with_freshness(FreshnessMode::CacheOnly)
        .with_account(account.clone());
    let request = faktor_commerce::query::QuoteRequest::new(
        faktor_commerce::query::ProductRef::parse(REF, Some(&SourceId::new(SOURCE).unwrap()))
            .unwrap(),
        100,
        None,
        None,
        None,
        FreshnessMode::CacheOnly,
    )
    .unwrap();
    let outcome = daemon
        .service
        .quote(&account_ctx, request)
        .await
        .expect("the account-scoped lookup is served from the daemon cache");
    assert!(
        matches!(outcome.freshness, Freshness::FreshCache { .. }),
        "expected a fresh cache hit, got {:?}",
        outcome.freshness
    );
    assert_eq!(outcome.account.as_ref(), Some(&account));

    // The stored row's SCOPE is account, not public (direct durable read).
    let identity = CacheIdentity {
        class: CacheClass::Price,
        source: SourceId::new(SOURCE).unwrap(),
        product: Text::<2048>::new(&product_key()).unwrap(),
        account_scope: Some(AccountScope::new("acct-a").unwrap()),
        locale: None,
        market: None,
        currency: None,
        quantity: Some(NonZeroQuantity::new(100).unwrap()),
        packaging: None,
        variant: None,
        pricing_scope: PricingScope::Account,
    };
    let row = daemon
        .service
        .store()
        .unwrap()
        .cache_get(&identity, 3_600_000, NOW_MS)
        .unwrap()
        .expect("the account identity must address the stored row");
    assert_eq!(row.account_scope, Some(account));
}

// ------------------------- 3. authenticated prices never enter public cache

/// An account-scoped observation is stored under the account pricing scope
/// and is NOT addressable as a public row: the public identity misses even
/// though the account identity hits, and the stored payload itself carries
/// the account-scoped visibility (never a public price break).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authenticated_prices_never_enter_the_public_cache() {
    let account = AccountScope::new("acct-a").unwrap();
    let identity = ConnectorIdentity::anonymous().with_account_scope(account.clone());
    let (daemon, site) = daemon(identity);
    let tool = wiring::daemon_tool(&daemon.graph, "source_market");
    let _ = call_tool(&tool, tool_ctx(1, 1), quote_args("live"))
        .await
        .expect("live account-scoped quote");
    assert_eq!(site.quote_calls(), 1);

    let store = daemon.service.store().unwrap();
    let base = CacheIdentity {
        class: CacheClass::Price,
        source: SourceId::new(SOURCE).unwrap(),
        product: Text::<2048>::new(&product_key()).unwrap(),
        account_scope: Some(account.clone()),
        locale: None,
        market: None,
        currency: None,
        quantity: Some(NonZeroQuantity::new(100).unwrap()),
        packaging: None,
        variant: None,
        pricing_scope: PricingScope::Account,
    };
    let account_row = store
        .cache_get(&base, 3_600_000, NOW_MS)
        .unwrap()
        .expect("account row stored");
    let payload: Vec<faktor_commerce::connector::QuoteCandidate> =
        account_row.payload.parse().unwrap();
    assert_eq!(
        payload[0].offer.price_visibility,
        PriceVisibility::AccountSpecific,
        "the cached payload must stay account-specific"
    );
    assert_eq!(
        payload[0].offer.price_breaks[0].visibility,
        PriceVisibility::AccountSpecific
    );

    // The PUBLIC pricing scope can never address the row: neither with the
    // account key stripped...
    let public_no_account = CacheIdentity {
        account_scope: None,
        pricing_scope: PricingScope::Public,
        ..base.clone()
    };
    assert!(
        store
            .cache_get(&public_no_account, 3_600_000, NOW_MS)
            .unwrap()
            .is_none(),
        "an account observation must never be public-cache addressable"
    );
    // ...nor by holding the account scope while claiming the public scope.
    let public_with_account = CacheIdentity {
        pricing_scope: PricingScope::Public,
        ..base.clone()
    };
    assert!(
        store
            .cache_get(&public_with_account, 3_600_000, NOW_MS)
            .unwrap()
            .is_none(),
        "the pricing scope is part of the cache identity"
    );
}

// ------------------- 4. variant/packaging survive tool -> connector

/// The flat `variant`/`packaging` tool arguments survive the daemon tool
/// handler, the commerce `QuoteRequest`, the production bridge conversion
/// (`site_quote`) and arrive at the site adapter as its typed QuoteRequest —
/// captured on the adapter seam the daemon registered.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn variant_and_packaging_survive_tool_service_connector() {
    let (daemon, site) = daemon(ConnectorIdentity::anonymous());
    let tool = wiring::daemon_tool(&daemon.graph, "source_market");
    let mut args = quote_args("live");
    args["variant"] = json!("package=tape_and_reel");
    args["packaging"] = json!("tape_and_reel");
    call_tool(&tool, tool_ctx(1, 1), args)
        .await
        .expect("quote with variant and packaging");
    assert_eq!(site.quote_calls(), 1);
    let captured = site.captured_quotes();
    assert_eq!(captured.len(), 1);
    let request = &captured[0];
    assert_eq!(request.quantity().get(), 100);
    assert_eq!(
        request.variant().map(VariantId::as_str),
        Some("package=tape_and_reel")
    );
    assert_eq!(request.packaging(), Some(PackagingType::TapeAndReel));
    assert!(
        site.quote_calls() == 1 && site.captured_quotes().len() == 1,
        "the selection must reach the adapter on the single quote call"
    );
}

// ---------------------------- 7. job status is requester-scoped

/// A durable BOM job belongs to the requesting principal (workspace +
/// session + account). The same daemon tool reads it for its own session and
/// returns a typed NotFound to another session, even in the same workspace.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn job_status_is_requester_scoped() {
    let (daemon, site) = daemon(ConnectorIdentity::anonymous());
    let tool = wiring::daemon_tool(&daemon.graph, "source_market");

    let bom = call_tool(
        &tool,
        tool_ctx(7, 1),
        json!({
            "op": "bom",
            "items": [{"q": REF, "qty": 10}],
            "sources": [SOURCE],
            "freshness": "live",
        }),
    )
    .await
    .expect("the daemon tool submits and advances the BOM job");
    let parsed: Value = serde_json::from_str(&bom.text).expect("bom result is JSON");
    let job_id = parsed["job_id"]
        .as_str()
        .expect("the bom result carries the durable job id")
        .to_string();
    assert!(site.quote_calls() + site.calls.lock().unwrap().product >= 1);

    let own = call_tool(
        &tool,
        tool_ctx(7, 1),
        json!({"op": "job", "job_id": job_id}),
    )
    .await
    .expect("the owning session reads its job");
    assert!(own.text.contains(&job_id), "{}", own.text);

    let foreign = call_tool(
        &tool,
        tool_ctx(8, 1),
        json!({"op": "job", "job_id": job_id}),
    )
    .await
    .expect_err("another session must not read the job");
    let rendered = format!("{foreign:?}");
    assert!(
        rendered.contains("not found") || rendered.contains("NotFound"),
        "the cross-session read must be a typed NotFound: {rendered}"
    );
    assert!(
        !rendered.contains("\"state\""),
        "no job state may leak cross-session"
    );
}
