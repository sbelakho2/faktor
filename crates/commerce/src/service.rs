//! `CommerceSourceService` — the daemon-lifetime authority of Faktor Acquire
//! (spec §14/§16, build step 16).
//!
//! The service owns the connector registry (and therefore every connector,
//! health tracker and circuit breaker), the durable [`CommerceStore`], the
//! injected CAS [`ArtifactStore`], the field-level TTL policy and the
//! request coalescer. The `source_market` tool is a thin gateway in front of
//! it; nothing here is created inside a tool factory.
//!
//! # Disabled parity
//!
//! A service constructed disabled (`ServiceConfig::enabled == false`, or
//! [`CommerceSourceService::disabled`]) never opens the database, never
//! creates the commerce directory, never registers a connector and never
//! constructs a client or browser state. Every operation returns the typed
//! [`ServiceError::Disabled`] before any side effect.
//!
//! # Freshness
//!
//! `prefer_cache`: a fresh cache row answers; otherwise live acquisition,
//! and a stale row is returned only as an explicit
//! [`Freshness::StaleFallback`] when the live path fails. `live`: never
//! answer from cache (a failure surfaces; stale data is never presented as
//! current). `cache_only`: fresh cache, else stale fallback, else a typed
//! cache miss.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::cache::{
    combine_freshness, CacheClass, CacheDecision, CacheIdentity, CacheTtls, PricingScope,
    SingleFlight,
};
use crate::connector::{
    AcquireCtx, Capability, CommerceConnector, ConnectorPolicy, ConnectorRegistry, Discovery,
    ProfileIdentity, QuoteCandidate, RegistryError,
};
use crate::error::{ConnectorHealth, SourceError};
use crate::jobs::{
    advance_job, bom_request, job_digest, running_outcome, terminal_outcome, CommerceJobRequest,
    ItemOutcome, JobItemExecutor, JobItemState, JobLine, JobOutcome, JobState,
};
use crate::offer::{CommercialOffer, Freshness};
use crate::query::{
    normalize_query, DetailLevel, FreshnessMode, ProductRef, ProductRequest, QuoteRequest,
    SearchRequest, SourceSet, MAX_REF_BYTES,
};
use crate::result::{ArtifactRef, ArtifactStore, CompactResult, ImportantEntry, ResultCounts};
use crate::store::{
    CachedRow, CommerceStore, CommerceStoreError, GcPolicy, GcReport, NewCacheRow,
    NormalizedPayload, SnapshotDiagnostics, SnapshotVersions,
};
use crate::text::{AccountScope, SourceId, Text};

/// Directory under the data dir that owns all commerce state.
pub const COMMERCE_DIR_NAME: &str = "commerce";
/// The commerce database file name (spec §13).
pub const COMMERCE_DB_NAME: &str = "commerce.db";

/// A typed service failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ServiceError {
    /// The source-layer failure.
    #[error("source: {0:?}")]
    Source(#[from] SourceError),
    /// Commerce is disabled.
    #[error("commerce service is disabled")]
    Disabled,
    /// The durable store failed.
    #[error("commerce store: {0}")]
    Store(#[from] CommerceStoreError),
    /// `cache_only` was requested and no observation exists.
    #[error("no cached {class:?} observation for this commercial context")]
    CacheMiss {
        /// The freshness class that was missing.
        class: CacheClass,
    },
    /// The service lock is poisoned.
    #[error("commerce service lock is poisoned")]
    Poisoned,
}

impl From<ServiceError> for SourceError {
    fn from(error: ServiceError) -> Self {
        match error {
            ServiceError::Source(error) => error,
            ServiceError::Disabled => SourceError::Disabled,
            ServiceError::Store(_) => SourceError::Store,
            ServiceError::CacheMiss { .. } => SourceError::ProductNotFound,
            ServiceError::Poisoned => SourceError::Store,
        }
    }
}

/// The service policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceConfig {
    /// Whether commerce is enabled.
    pub enabled: bool,
    /// The field-level cache TTLs.
    pub ttls: CacheTtls,
    /// The retention/GC bounds.
    pub gc: GcPolicy,
}

impl Default for ServiceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ttls: CacheTtls::default(),
            gc: GcPolicy::default(),
        }
    }
}

impl ServiceConfig {
    /// The disabled configuration: no DB, no clients, no browser.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }
}

/// The result of a discovery operation.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchOutcome {
    /// The discoveries, in deterministic source order.
    pub discoveries: Vec<Discovery>,
    /// The sources that were consulted.
    pub sources: Vec<SourceId>,
    /// The combined freshness (the worst input; `None` with no data).
    pub freshness: Option<Freshness>,
    /// Matched/ambiguous/unmatched accounting.
    pub counts: ResultCounts,
}

/// The result of a product inspection.
#[derive(Debug, Clone, PartialEq)]
pub struct ProductOutcome {
    /// The normalized offer.
    pub offer: CommercialOffer,
    /// How fresh the offer is.
    pub freshness: Freshness,
}

/// The result of a quote.
#[derive(Debug, Clone, PartialEq)]
pub struct QuoteOutcome {
    /// The quote candidates.
    pub candidates: Vec<QuoteCandidate>,
    /// The combined freshness.
    pub freshness: Freshness,
    /// The account scope the prices belong to, when any.
    pub account: Option<AccountScope>,
}

/// The deterministic state/result of a job.
#[derive(Debug, Clone, PartialEq)]
pub struct JobStatus {
    /// The job id.
    pub job_id: String,
    /// The job state.
    pub state: JobState,
    /// The compact result (running or terminal).
    pub compact: CompactResult,
}

/// An artifact store that refuses every write; the disabled-mode default.
pub struct RefusingArtifacts;

#[async_trait::async_trait]
impl ArtifactStore for RefusingArtifacts {
    async fn put(&self, _bytes: &[u8]) -> Result<ArtifactRef, SourceError> {
        Err(SourceError::Store)
    }

    async fn get(&self, _digest: &str) -> Result<Option<Vec<u8>>, SourceError> {
        Err(SourceError::Store)
    }
}

/// The internals of one coalesced acquisition.
#[derive(Debug, Clone, PartialEq)]
enum Acquired {
    Discoveries(Vec<Discovery>),
    Offer(Box<CommercialOffer>),
    Quotes(Vec<QuoteCandidate>),
}

/// What a cache consultation produced.
enum CacheLookup {
    /// A fresh row (or a stale row under `cache_only`).
    Fresh(CachedRow, Freshness),
    /// A stale row, usable only as an explicit fallback.
    Stale(CachedRow, Freshness),
    /// Nothing.
    Miss,
}

/// The daemon-lifetime commerce authority.
pub struct CommerceSourceService {
    enabled: bool,
    data_dir: Option<PathBuf>,
    store: Option<Arc<CommerceStore>>,
    registry: Mutex<ConnectorRegistry>,
    artifacts: Arc<dyn ArtifactStore>,
    ttls: CacheTtls,
    gc: GcPolicy,
    coalescer: SingleFlight<String, Acquired>,
}

impl CommerceSourceService {
    /// A disabled service: no store, no connectors, no browser, no clients.
    /// Constructing it touches nothing.
    pub fn disabled() -> Arc<Self> {
        Arc::new(Self {
            enabled: false,
            data_dir: None,
            store: None,
            registry: Mutex::new(ConnectorRegistry::new()),
            artifacts: Arc::new(RefusingArtifacts),
            ttls: CacheTtls::default(),
            gc: GcPolicy::default(),
            coalescer: SingleFlight::new(),
        })
    }

    /// Construct the service. When `config.enabled` is false this is exactly
    /// [`Self::disabled`]: the data directory is never created and the
    /// database is never opened.
    pub fn open(
        data_dir: impl AsRef<Path>,
        config: ServiceConfig,
        artifacts: Arc<dyn ArtifactStore>,
    ) -> Result<Arc<Self>, CommerceStoreError> {
        if !config.enabled {
            return Ok(Self::disabled());
        }
        let commerce_dir = data_dir.as_ref().join(COMMERCE_DIR_NAME);
        let db_path = commerce_dir.join(COMMERCE_DB_NAME);
        let store = Arc::new(CommerceStore::open(&db_path)?);
        Ok(Arc::new(Self {
            enabled: true,
            data_dir: Some(commerce_dir),
            store: Some(store),
            registry: Mutex::new(ConnectorRegistry::new()),
            artifacts,
            ttls: config.ttls,
            gc: config.gc,
            coalescer: SingleFlight::new(),
        }))
    }

    /// True when commerce is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// The commerce directory, when enabled.
    pub fn data_dir(&self) -> Option<&Path> {
        self.data_dir.as_deref()
    }

    /// The durable store, when enabled.
    pub fn store(&self) -> Option<&Arc<CommerceStore>> {
        self.store.as_ref()
    }

    /// Register one connector. Registration only advertises capabilities;
    /// no client is created here.
    pub fn register(
        &self,
        connector: Arc<dyn CommerceConnector>,
        policy: ConnectorPolicy,
    ) -> Result<(), RegistryError> {
        let source = connector.source();
        let mut registry =
            self.registry
                .lock()
                .map_err(|_| RegistryError::InvalidCapabilities {
                    source_id: source,
                    reason: "commerce registry is poisoned".to_string(),
                })?;
        registry.register_with_policy(connector, policy)
    }

    /// Register a source that is enabled by configuration but cannot serve
    /// (missing credentials, browser disabled, …). Both acquisition paths
    /// are policy-disabled and the planner-facing health is set to the
    /// error's health immediately, so `status`/`doctor`/the planner see the
    /// typed state BEFORE any request is attempted instead of discovering it
    /// at request time. Registration itself creates no client; the caller
    /// passes a connector that refuses typed.
    pub fn register_disabled(
        &self,
        connector: Arc<dyn CommerceConnector>,
        error: &SourceError,
    ) -> Result<(), RegistryError> {
        let source = connector.source();
        self.register(
            connector,
            ConnectorPolicy {
                api_enabled: false,
                browser_enabled: false,
                ..ConnectorPolicy::default()
            },
        )?;
        if let Ok(registry) = self.registry.lock() {
            registry.note_failure(
                &source,
                crate::connector::AcquisitionPath::Api,
                error,
                now_ms(),
            );
        }
        Ok(())
    }

    /// The planner-facing health of every registered source.
    pub fn health_snapshot(&self, now_ms: u64) -> Vec<(SourceId, ConnectorHealth)> {
        match self.registry.lock() {
            Ok(registry) => registry.health_snapshot(now_ms),
            Err(_) => Vec::new(),
        }
    }

    /// The registered sources, sorted.
    pub fn sources(&self) -> Vec<SourceId> {
        match self.registry.lock() {
            Ok(registry) => registry.sources(),
            Err(_) => Vec::new(),
        }
    }

    /// Run one bounded GC pass.
    pub fn gc(&self, now_ms: u64) -> Result<GcReport, ServiceError> {
        let Some(store) = &self.store else {
            return Err(ServiceError::Disabled);
        };
        Ok(store.gc(&self.gc, now_ms)?)
    }

    /// Drop the whole cache (admin `clear-cache`).
    pub fn clear_cache(&self) -> Result<u64, ServiceError> {
        let Some(store) = &self.store else {
            return Err(ServiceError::Disabled);
        };
        Ok(store.clear_cache()?)
    }

    fn ensure_enabled(&self) -> Result<(), ServiceError> {
        if self.enabled {
            Ok(())
        } else {
            Err(ServiceError::Disabled)
        }
    }

    fn store_ref(&self) -> Result<&Arc<CommerceStore>, ServiceError> {
        self.store.as_ref().ok_or(ServiceError::Disabled)
    }

    /// Resolve the requested source set for a capability against the
    /// registry.
    fn resolve_sources(
        &self,
        requested: &SourceSet,
        capability: Capability,
        now_ms: u64,
    ) -> Result<Vec<SourceId>, ServiceError> {
        let registry = self.registry.lock().map_err(|_| ServiceError::Poisoned)?;
        Ok(registry.resolve(requested, capability, now_ms)?)
    }

    /// The stable "enabled sources" list for a capability: every registered
    /// source that advertises it, sorted and never health-filtered (a digest
    /// must not change because a breaker opened).
    fn enabled_sources(&self, capability: Capability) -> Vec<SourceId> {
        let Ok(registry) = self.registry.lock() else {
            return Vec::new();
        };
        let mut sources: Vec<SourceId> = registry
            .sources()
            .into_iter()
            .filter(|source| {
                registry
                    .capabilities(source)
                    .map(|cap| cap.has(capability))
                    .unwrap_or(false)
            })
            .collect();
        sources.sort();
        sources
    }

    #[allow(clippy::too_many_arguments)]
    fn cache_identity(
        &self,
        class: CacheClass,
        source: &SourceId,
        product: &str,
        ctx: &AcquireCtx,
        pricing_scope: PricingScope,
        quantity: Option<crate::quantity::NonZeroQuantity>,
        packaging: Option<crate::packaging::PackagingType>,
        variant: Option<crate::text::VariantId>,
    ) -> Result<CacheIdentity, ServiceError> {
        let product =
            Text::<MAX_REF_BYTES>::new(product).map_err(|_| SourceError::InvalidRequest)?;
        Ok(CacheIdentity {
            class,
            source: source.clone(),
            product,
            account_scope: ctx.account_scope.clone(),
            locale: None,
            market: None,
            currency: None,
            quantity,
            packaging,
            variant,
            pricing_scope,
        })
    }

    /// Consult the cache for one identity under the requested mode.
    fn consult_cache(
        &self,
        identity: &CacheIdentity,
        ttl_ms: u64,
        mode: FreshnessMode,
        now_ms: u64,
    ) -> Result<CacheLookup, ServiceError> {
        let store = self.store_ref()?;
        let Some(row) = store.cache_get(identity, ttl_ms, now_ms)? else {
            return Ok(CacheLookup::Miss);
        };
        // Defense in depth on top of the keyed identity: an account-scoped
        // row is never returned to a different scope.
        if row.account_scope != identity.account_scope {
            return Ok(CacheLookup::Miss);
        }
        match row.decision {
            CacheDecision::Fresh(freshness) => Ok(CacheLookup::Fresh(row, freshness)),
            CacheDecision::Stale(freshness) => {
                if mode == FreshnessMode::CacheOnly {
                    Ok(CacheLookup::Fresh(row, freshness))
                } else {
                    Ok(CacheLookup::Stale(row, freshness))
                }
            }
            CacheDecision::Miss => Ok(CacheLookup::Miss),
        }
    }

    fn write_cache(
        &self,
        identity: &CacheIdentity,
        payload: &NormalizedPayload,
        observed_at_ms: u64,
    ) -> Result<(), ServiceError> {
        let store = self.store_ref()?;
        store.cache_put(&NewCacheRow {
            identity: identity.clone(),
            payload: payload.clone(),
            validators: Default::default(),
            observed_at_ms,
        })?;
        Ok(())
    }

    fn source_scope_for(reference: &ProductRef) -> Option<SourceId> {
        reference.source().cloned()
    }

    // ------------------------------------------------------------- operations

    /// Discovery (`search`): discovery only, never a quote.
    pub async fn search(
        &self,
        ctx: &AcquireCtx,
        req: SearchRequest,
    ) -> Result<SearchOutcome, ServiceError> {
        self.ensure_enabled()?;
        req.validate().map_err(SourceError::from)?;
        let now = now_ms();
        let sources = self.resolve_sources(&req.sources, Capability::Discovery, now)?;
        let ttl = self.ttls.ttl_ms(CacheClass::Discovery);
        let normalized = normalize_query(req.query.as_str());
        let mut discoveries: Vec<Discovery> = Vec::new();
        let mut marks: Vec<Freshness> = Vec::new();
        let mut consulted: Vec<SourceId> = Vec::new();

        for source in sources {
            ctx.check()?;
            consulted.push(source.clone());
            let product = format!("search:{normalized}");
            let identity = self.cache_identity(
                CacheClass::Discovery,
                &source,
                &product,
                ctx,
                PricingScope::Public,
                None,
                None,
                None,
            )?;
            let mut stale_fallback: Option<(CachedRow, Freshness)> = None;
            if req.freshness != FreshnessMode::Live {
                match self.consult_cache(&identity, ttl, req.freshness, now)? {
                    CacheLookup::Fresh(row, freshness) => {
                        let cached: Vec<Discovery> =
                            row.payload.parse().map_err(ServiceError::Store)?;
                        marks.push(freshness);
                        discoveries.extend(cached);
                        continue;
                    }
                    CacheLookup::Stale(row, freshness) => {
                        if req.freshness == FreshnessMode::CacheOnly {
                            let cached: Vec<Discovery> =
                                row.payload.parse().map_err(ServiceError::Store)?;
                            marks.push(freshness);
                            discoveries.extend(cached);
                            continue;
                        }
                        stale_fallback = Some((row, freshness));
                    }
                    CacheLookup::Miss => {}
                }
            }
            if req.freshness == FreshnessMode::CacheOnly {
                return Err(ServiceError::CacheMiss {
                    class: CacheClass::Discovery,
                });
            }

            let key = format!("discovery:{}:{}:{}", source, identity.digest(), req.limit);
            let call_ctx = ctx.clone().with_freshness(req.freshness);
            let connector = {
                let registry = self.registry.lock().map_err(|_| ServiceError::Poisoned)?;
                registry.path_allowed(&source, crate::connector::AcquisitionPath::Api, now)?;
                registry
                    .connector(&source)
                    .ok_or(ServiceError::Source(SourceError::Disabled))?
            };
            let request = req.clone();
            let acquired = self
                .coalescer
                .run(key, || {
                    let connector = connector.clone();
                    let call_ctx = call_ctx.clone();
                    async move {
                        call_ctx
                            .run_bounded(connector.discover(&call_ctx, request))
                            .await
                            .map(Acquired::Discoveries)
                    }
                })
                .await;
            match acquired {
                Ok(Acquired::Discoveries(rows)) => {
                    if let Ok(registry) = self.registry.lock() {
                        registry.note_success(&source, crate::connector::AcquisitionPath::Api);
                    }
                    // Extracted text is scrubbed before it can reach a cache
                    // payload or a tool outcome.
                    let mut rows = rows;
                    crate::result::scrub_discoveries(&mut rows);
                    let payload = NormalizedPayload::from_serializable(&rows)?;
                    self.write_cache(&identity, &payload, now)?;
                    marks.push(Freshness::Live);
                    discoveries.extend(rows);
                }
                Ok(_) => return Err(ServiceError::Source(SourceError::Store)),
                Err(error) => {
                    if let Ok(registry) = self.registry.lock() {
                        registry.note_failure(
                            &source,
                            crate::connector::AcquisitionPath::Api,
                            &error,
                            now,
                        );
                    }
                    match stale_fallback {
                        Some((row, freshness)) if req.freshness == FreshnessMode::PreferCache => {
                            let cached: Vec<Discovery> =
                                row.payload.parse().map_err(ServiceError::Store)?;
                            marks.push(freshness);
                            discoveries.extend(cached);
                        }
                        _ => return Err(ServiceError::Source(error)),
                    }
                }
            }
        }

        let freshness = combine_freshness(marks);
        let counts = ResultCounts {
            matched: discoveries.len() as u64,
            ..ResultCounts::default()
        };
        Ok(SearchOutcome {
            discoveries,
            sources: consulted,
            freshness,
            counts,
        })
    }

    /// Inspect one known reference (`product`).
    pub async fn product(
        &self,
        ctx: &AcquireCtx,
        req: ProductRequest,
    ) -> Result<ProductOutcome, ServiceError> {
        self.ensure_enabled()?;
        req.validate().map_err(SourceError::from)?;
        let now = now_ms();
        let requested = match Self::source_scope_for(&req.reference) {
            Some(source) => SourceSet::named(vec![source]).map_err(SourceError::from)?,
            None => SourceSet::auto(),
        };
        let sources = self.resolve_sources(&requested, Capability::ExactProduct, now)?;
        let ttl = self.ttls.ttl_ms(CacheClass::Product);
        let product_key = req.reference.identity_key();
        let mut last_error: Option<SourceError> = None;

        for source in &sources {
            ctx.check()?;
            let identity = self.cache_identity(
                CacheClass::Product,
                source,
                &product_key,
                ctx,
                PricingScope::Public,
                None,
                None,
                None,
            )?;
            let mut stale_fallback: Option<(CachedRow, Freshness)> = None;
            if req.freshness != FreshnessMode::Live {
                match self.consult_cache(&identity, ttl, req.freshness, now)? {
                    CacheLookup::Fresh(row, freshness) => {
                        let offer: CommercialOffer =
                            row.payload.parse().map_err(ServiceError::Store)?;
                        return Ok(ProductOutcome { offer, freshness });
                    }
                    CacheLookup::Stale(row, freshness) => {
                        if req.freshness == FreshnessMode::CacheOnly {
                            let offer: CommercialOffer =
                                row.payload.parse().map_err(ServiceError::Store)?;
                            return Ok(ProductOutcome { offer, freshness });
                        }
                        stale_fallback = Some((row, freshness));
                    }
                    CacheLookup::Miss => {}
                }
            }
            if req.freshness == FreshnessMode::CacheOnly {
                return Err(ServiceError::CacheMiss {
                    class: CacheClass::Product,
                });
            }

            let key = format!("product:{}:{}", source, identity.digest());
            let call_ctx = ctx.clone().with_freshness(req.freshness);
            let connector = {
                let registry = self.registry.lock().map_err(|_| ServiceError::Poisoned)?;
                registry.path_allowed(source, crate::connector::AcquisitionPath::Api, now)?;
                registry
                    .connector(source)
                    .ok_or(ServiceError::Source(SourceError::Disabled))?
            };
            let request = req.clone();
            let acquired = self
                .coalescer
                .run(key, || {
                    let connector = connector.clone();
                    let call_ctx = call_ctx.clone();
                    async move {
                        call_ctx
                            .run_bounded(connector.product(&call_ctx, request))
                            .await
                            .map(|offer| Acquired::Offer(Box::new(offer)))
                    }
                })
                .await;
            match acquired {
                Ok(Acquired::Offer(offer)) => {
                    if let Ok(registry) = self.registry.lock() {
                        registry.note_success(source, crate::connector::AcquisitionPath::Api);
                    }
                    let mut offer = *offer;
                    crate::result::scrub_offer(&mut offer);
                    let payload = NormalizedPayload::from_serializable(&offer)?;
                    self.write_cache(&identity, &payload, now)?;
                    self.persist_offer_snapshot(&offer, now);
                    return Ok(ProductOutcome {
                        offer,
                        freshness: Freshness::Live,
                    });
                }
                Ok(_) => return Err(ServiceError::Source(SourceError::Store)),
                Err(error) => {
                    if let Ok(registry) = self.registry.lock() {
                        registry.note_failure(
                            source,
                            crate::connector::AcquisitionPath::Api,
                            &error,
                            now,
                        );
                    }
                    if let Some((row, freshness)) = stale_fallback {
                        if req.freshness == FreshnessMode::PreferCache {
                            let offer: CommercialOffer =
                                row.payload.parse().map_err(ServiceError::Store)?;
                            return Ok(ProductOutcome { offer, freshness });
                        }
                    }
                    last_error = Some(error);
                }
            }
        }
        Err(ServiceError::Source(
            last_error.unwrap_or(SourceError::ProductNotFound),
        ))
    }

    /// A real applicable price at a quantity (`quote`).
    pub async fn quote(
        &self,
        ctx: &AcquireCtx,
        req: QuoteRequest,
    ) -> Result<QuoteOutcome, ServiceError> {
        let requested = match Self::source_scope_for(&req.reference) {
            Some(source) => SourceSet::named(vec![source]).map_err(SourceError::from)?,
            None => SourceSet::auto(),
        };
        self.quote_with_sources(ctx, req, requested).await
    }

    /// Quote honoring an explicit job/BOM source set.
    pub async fn quote_with_sources(
        &self,
        ctx: &AcquireCtx,
        req: QuoteRequest,
        sources: SourceSet,
    ) -> Result<QuoteOutcome, ServiceError> {
        self.ensure_enabled()?;
        req.validate().map_err(SourceError::from)?;
        let now = now_ms();
        let sources = self.resolve_sources(&sources, Capability::QuantityPricing, now)?;
        let ttl = self.ttls.ttl_ms(CacheClass::Price);
        let product_key = req.reference.identity_key();
        let pricing_scope = if ctx.account_scope.is_some() {
            PricingScope::Account
        } else {
            PricingScope::Public
        };
        let mut last_error: Option<SourceError> = None;

        for source in &sources {
            ctx.check()?;
            let identity = self.cache_identity(
                CacheClass::Price,
                source,
                &product_key,
                ctx,
                pricing_scope,
                Some(req.quantity),
                req.packaging,
                req.variant.clone(),
            )?;
            let mut stale_fallback: Option<(CachedRow, Freshness)> = None;
            if req.freshness != FreshnessMode::Live {
                match self.consult_cache(&identity, ttl, req.freshness, now)? {
                    CacheLookup::Fresh(row, freshness) => {
                        let candidates: Vec<QuoteCandidate> =
                            row.payload.parse().map_err(ServiceError::Store)?;
                        return Ok(QuoteOutcome {
                            candidates,
                            freshness,
                            account: ctx.account_scope.clone(),
                        });
                    }
                    CacheLookup::Stale(row, freshness) => {
                        if req.freshness == FreshnessMode::CacheOnly {
                            let candidates: Vec<QuoteCandidate> =
                                row.payload.parse().map_err(ServiceError::Store)?;
                            return Ok(QuoteOutcome {
                                candidates,
                                freshness,
                                account: ctx.account_scope.clone(),
                            });
                        }
                        stale_fallback = Some((row, freshness));
                    }
                    CacheLookup::Miss => {}
                }
            }
            if req.freshness == FreshnessMode::CacheOnly {
                return Err(ServiceError::CacheMiss {
                    class: CacheClass::Price,
                });
            }

            let key = format!("quote:{}:{}", source, identity.digest());
            let call_ctx = ctx.clone().with_freshness(req.freshness);
            let connector = {
                let registry = self.registry.lock().map_err(|_| ServiceError::Poisoned)?;
                registry.path_allowed(source, crate::connector::AcquisitionPath::Api, now)?;
                registry
                    .connector(source)
                    .ok_or(ServiceError::Source(SourceError::Disabled))?
            };
            let request = req.clone();
            let acquired = self
                .coalescer
                .run(key, || {
                    let connector = connector.clone();
                    let call_ctx = call_ctx.clone();
                    async move {
                        call_ctx
                            .run_bounded(connector.quote(&call_ctx, request))
                            .await
                            .map(Acquired::Quotes)
                    }
                })
                .await;
            match acquired {
                Ok(Acquired::Quotes(candidates)) => {
                    if let Ok(registry) = self.registry.lock() {
                        registry.note_success(source, crate::connector::AcquisitionPath::Api);
                    }
                    let mut candidates = candidates;
                    crate::result::scrub_quote_candidates(&mut candidates);
                    let payload = NormalizedPayload::from_serializable(&candidates)?;
                    self.write_cache(&identity, &payload, now)?;
                    for candidate in candidates.iter().take(MAX_QUOTE_SNAPSHOTS) {
                        self.persist_offer_snapshot(&candidate.offer, now);
                    }
                    return Ok(QuoteOutcome {
                        candidates,
                        freshness: Freshness::Live,
                        account: ctx.account_scope.clone(),
                    });
                }
                Ok(_) => return Err(ServiceError::Source(SourceError::Store)),
                Err(error) => {
                    if let Ok(registry) = self.registry.lock() {
                        registry.note_failure(
                            source,
                            crate::connector::AcquisitionPath::Api,
                            &error,
                            now,
                        );
                    }
                    if let Some((row, freshness)) = stale_fallback {
                        if req.freshness == FreshnessMode::PreferCache {
                            let candidates: Vec<QuoteCandidate> =
                                row.payload.parse().map_err(ServiceError::Store)?;
                            return Ok(QuoteOutcome {
                                candidates,
                                freshness,
                                account: ctx.account_scope.clone(),
                            });
                        }
                    }
                    last_error = Some(error);
                }
            }
        }
        Err(ServiceError::Source(
            last_error.unwrap_or(SourceError::ProductNotFound),
        ))
    }

    /// Submit and run a deterministic BOM job (`bom`).
    pub async fn bom(
        &self,
        ctx: &AcquireCtx,
        sources: SourceSet,
        freshness: FreshnessMode,
        detail: DetailLevel,
        bom: crate::bom::Bom,
    ) -> Result<JobOutcome, ServiceError> {
        self.ensure_enabled()?;
        let request = bom_request(bom, sources, freshness, detail, ctx.account_scope.clone());
        self.submit_and_advance(ctx, request).await
    }

    /// Submit and run any deterministic job request.
    pub async fn submit_and_advance(
        &self,
        ctx: &AcquireCtx,
        request: CommerceJobRequest,
    ) -> Result<JobOutcome, ServiceError> {
        self.ensure_enabled()?;
        let store = self.store_ref()?.clone();
        let now = now_ms();
        let enabled = self.enabled_sources(Capability::QuantityPricing);
        let profile = ctx.profile.clone();
        let (job, _attached) =
            crate::jobs::submit_job(&store, request, &enabled, profile.as_ref(), now)?;
        let executor = ServiceJobExecutor { service: self };
        Ok(advance_job(
            &store,
            ctx,
            &job.id,
            &executor,
            self.artifacts.as_ref(),
            now,
        )
        .await?)
    }

    /// The deterministic state/result of a job (`job`).
    pub fn job_status(&self, job_id: &str) -> Result<JobStatus, ServiceError> {
        self.ensure_enabled()?;
        let store = self.store_ref()?;
        let row = store
            .job(job_id)?
            .ok_or(ServiceError::Source(SourceError::ProductNotFound))?;
        let job = crate::jobs::CommerceJob {
            id: row.job_id.clone(),
            digest: row.digest.clone(),
            state: row.state,
            request: serde_json::from_str(row.request.as_str())
                .map_err(|_| ServiceError::Source(SourceError::Store))?,
        };
        let compact = match row.compact_json {
            Some(json) => {
                serde_json::from_str(&json).map_err(|_| ServiceError::Source(SourceError::Store))?
            }
            // A terminal job without a persisted compact result reports its
            // TRUE terminal state with typed diagnostics; only a
            // non-terminal job is "running".
            None if row.state.is_terminal() => {
                let counts = ResultCounts {
                    matched: row.matched,
                    ambiguous: row.ambiguous,
                    unmatched: row.unmatched,
                };
                terminal_outcome(&job, counts, row.last_error.as_deref())
                    .map_err(ServiceError::Source)?
                    .compact
            }
            None => running_outcome(&job, now_ms()).compact,
        };
        Ok(JobStatus {
            job_id: row.job_id,
            state: row.state,
            compact,
        })
    }

    /// Resume every non-terminal job after a daemon restart. Deterministic
    /// lines settle once; the pass is bounded by the caller's context.
    pub async fn resume_pending(&self, ctx: &AcquireCtx) -> Result<Vec<JobOutcome>, ServiceError> {
        self.ensure_enabled()?;
        let store = self.store_ref()?.clone();
        let executor = ServiceJobExecutor { service: self };
        let mut outcomes = Vec::new();
        for row in store.pending_jobs()? {
            ctx.check()?;
            let outcome = advance_job(
                &store,
                ctx,
                &row.job_id,
                &executor,
                self.artifacts.as_ref(),
                now_ms(),
            )
            .await?;
            outcomes.push(outcome);
        }
        Ok(outcomes)
    }

    /// The digest a job request would receive (diagnostics/tests).
    pub fn digest_for(
        &self,
        request: &CommerceJobRequest,
        profile: Option<&ProfileIdentity>,
    ) -> String {
        job_digest(
            request,
            &self.enabled_sources(Capability::QuantityPricing),
            profile,
        )
    }

    fn persist_offer_snapshot(&self, offer: &CommercialOffer, now_ms: u64) {
        let Some(store) = &self.store else {
            return;
        };
        let versions = SnapshotVersions {
            extractor_version: offer
                .provenance
                .extractor_version
                .as_ref()
                .map(|v| v.as_str().to_string()),
            connector_version: offer
                .provenance
                .connector_version
                .as_ref()
                .map(|v| v.as_str().to_string()),
            normalization_version: offer
                .provenance
                .normalization_version
                .as_ref()
                .map(|v| v.as_str().to_string()),
        };
        let diagnostics = SnapshotDiagnostics::new(vec![(
            "origin".to_string(),
            offer.provenance.origin.as_str().to_string(),
        )])
        .unwrap_or_default();
        if let Err(error) = store.record_offer_snapshot(offer, &versions, &diagnostics, now_ms) {
            tracing::warn!("commerce snapshot not persisted: {error}");
        }
    }
}

/// At most this many offer snapshots are written per quote.
pub const MAX_QUOTE_SNAPSHOTS: usize = 8;

/// The deterministic per-line executor of BOM jobs, backed by the service's
/// connectors, cache, registry health and coalescer.
struct ServiceJobExecutor<'a> {
    service: &'a CommerceSourceService,
}

#[async_trait::async_trait]
impl JobItemExecutor for ServiceJobExecutor<'_> {
    async fn execute(
        &self,
        ctx: &AcquireCtx,
        job: &crate::jobs::CommerceJob,
        line: &JobLine,
    ) -> Result<ItemOutcome, SourceError> {
        let reference = match (&line.reference, &line.query) {
            (Some(reference), _) => reference.clone(),
            (None, Some(query)) => match ProductRef::parse(query.as_str(), None) {
                Ok(reference) => reference,
                Err(_) => return Ok(unparseable_line(line)),
            },
            (None, None) => return Ok(unparseable_line(line)),
        };
        let quantity = match line.quantity {
            Some(quantity) => quantity,
            None => crate::quantity::NonZeroQuantity::new(1).map_err(|_| SourceError::Store)?,
        };
        // A `Quote` job carries the requested packaging/variant in its work
        // (both are bound into the job digest); they must reach the quote
        // request, or a variant/packaging-scoped job would resolve at the
        // offer level or report VariantAmbiguous instead of the requested
        // price.
        let (packaging, variant) = match &job.request.work {
            crate::jobs::JobWork::Quote {
                packaging, variant, ..
            } => (*packaging, variant.clone()),
            _ => (None, None),
        };
        let quote_request = QuoteRequest::new(
            reference.clone(),
            quantity.get(),
            packaging,
            variant,
            ctx.account_scope.clone(),
            job.request.freshness,
        )
        .map_err(SourceError::from)?;
        let outcome = self
            .service
            .quote_with_sources(ctx, quote_request, job.request.sources.clone())
            .await
            .map_err(|error| match error {
                ServiceError::Source(error) => error,
                other => SourceError::from(other),
            })?;
        let candidates = outcome.candidates;
        if candidates.is_empty() {
            return Ok(ItemOutcome {
                state: JobItemState::Unmatched,
                label: Some("no candidates".to_string()),
                important: Some(
                    ImportantEntry::new(
                        &format!("line-{:04}", line.ordinal + 1),
                        &format!(
                            "{} -> no candidates",
                            reference.part_number_text().unwrap_or("reference")
                        ),
                    )
                    .map_err(|_| SourceError::Store)?,
                ),
                detail: NormalizedPayload::from_json(
                    "{\"status\":\"unmatched\",\"candidates\":0}".to_string(),
                )?,
            });
        }
        let resolved = candidates
            .iter()
            .find(|candidate| {
                candidate.resolution.status == crate::quote::QuoteStatus::Resolved
                    && candidate.resolution.quote.is_some()
            })
            .cloned();
        let ambiguous = candidates.iter().any(|candidate| {
            matches!(
                candidate.resolution.status,
                crate::quote::QuoteStatus::VariantAmbiguous
                    | crate::quote::QuoteStatus::PackagingAmbiguous
                    | crate::quote::QuoteStatus::AmbiguousTier
            )
        });
        #[derive(serde::Serialize)]
        struct LineDetail {
            status: &'static str,
            candidates: usize,
            source: Option<String>,
            unit_price: Option<String>,
            currency: Option<String>,
            variant: Option<String>,
            packaging: Option<String>,
            quantity: u64,
        }
        let (state, status, chosen) = match resolved {
            Some(candidate) => (JobItemState::Matched, "matched", Some(candidate)),
            None if ambiguous => (JobItemState::Ambiguous, "ambiguous", None),
            None => (
                JobItemState::Unmatched,
                candidates
                    .first()
                    .map(|candidate| quote_status_label(candidate.resolution.status))
                    .unwrap_or("unmatched"),
                None,
            ),
        };
        let detail = LineDetail {
            status,
            candidates: candidates.len(),
            source: chosen
                .as_ref()
                .map(|candidate| candidate.offer.source.as_str().to_string()),
            unit_price: chosen
                .as_ref()
                .and_then(|candidate| candidate.resolution.unit_price)
                .map(|money| money.to_decimal_string()),
            currency: chosen
                .as_ref()
                .map(|candidate| candidate.offer.currency.as_str().to_string()),
            variant: chosen
                .as_ref()
                .and_then(|candidate| candidate.resolution.variant_id.as_ref())
                .map(|variant| variant.as_str().to_string()),
            packaging: chosen
                .as_ref()
                .and_then(|candidate| candidate.resolution.packaging)
                .map(|packaging| packaging.as_str().to_string()),
            quantity: quantity.get(),
        };
        let label = match state {
            JobItemState::Matched => chosen.as_ref().and_then(|candidate| {
                candidate.resolution.unit_price.map(|price| {
                    format!(
                        "{} x{} {}",
                        price.to_decimal_string(),
                        quantity.get(),
                        candidate.offer.currency.as_str()
                    )
                })
            }),
            JobItemState::Ambiguous => Some(format!("{} ambiguous candidates", candidates.len())),
            _ => Some("no applicable price".to_string()),
        };
        let important = if matches!(state, JobItemState::Matched) {
            None
        } else {
            Some(
                ImportantEntry::new(
                    &format!("line-{:04}", line.ordinal + 1),
                    &label.clone().unwrap_or_else(|| status.to_string()),
                )
                .map_err(|_| SourceError::Store)?,
            )
        };
        Ok(ItemOutcome {
            state,
            label,
            important,
            detail: NormalizedPayload::from_serializable(&detail)?,
        })
    }
}

fn unparseable_line(line: &JobLine) -> ItemOutcome {
    ItemOutcome {
        state: JobItemState::Unmatched,
        label: Some("unparseable line".to_string()),
        important: ImportantEntry::new(
            &format!("line-{:04}", line.ordinal + 1),
            "unparseable line",
        )
        .ok(),
        detail: NormalizedPayload::from_json(
            "{\"status\":\"unmatched\",\"reason\":\"unparseable_line\"}".to_string(),
        )
        .unwrap_or_else(|_| NormalizedPayload::from_json("{}".to_string()).expect("literal")),
    }
}

fn quote_status_label(status: crate::quote::QuoteStatus) -> &'static str {
    use crate::quote::QuoteStatus::*;
    match status {
        Resolved => "matched",
        VariantAmbiguous | PackagingAmbiguous | AmbiguousTier => "ambiguous",
        VariantNotFound
        | PackagingNotFound
        | PackagingMismatch
        | NoPrice
        | InquiryRequired
        | MoqViolation
        | QuantityStepUnsatisfiable
        | QuantityTooLarge
        | AmountOverflow
        | CurrencyMismatch => "unmatched",
    }
}

/// The wall clock in milliseconds.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
