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

use faktor_acquire::cache::{freshness_class, CacheEntry, CacheKey, CacheState, FreshnessClassTtl};
use faktor_acquire::capability::{
    CapabilityLevel as AcquireCapabilityLevel, ConnectorCapabilities as AcquireCapabilities,
    CredentialAvailability,
};
use faktor_acquire::health::{ConnectorHealth as AcquireHealth, ExtractionHealth};
/// Re-exported so the daemon construction seam can name the planner port
/// without a direct `faktor-acquire` dependency (the service already owns the
/// port; this only widens its path, never the behavior).
pub use faktor_acquire::planner::AcquisitionPlanning;
use faktor_acquire::planner::{
    AcquisitionPlan, AcquisitionPlanner, PlanDecision, RuntimeAcquisitionState,
};
use faktor_acquire::quota::{QuotaState as AcquireQuotaState, QuotaWindow};
use faktor_acquire::request::{RequestedField, RequestedFields, RequestedFreshness};

use crate::cache::{
    combine_freshness, CacheClass, CacheDecision, CacheIdentity, CacheTtls, PricingScope,
    SingleFlight,
};
use crate::connector::{
    AcquireCtx, AcquisitionMechanism, AcquisitionPath, Capability, CommerceConnector,
    ConnectorPolicy, ConnectorRegistry, Discovery, ProfileIdentity, QuoteCandidate, RegistryError,
};
use crate::error::{ConnectorHealth, SourceError, VerificationKind};
use crate::jobs::{
    advance_job, bom_request, job_digest, running_outcome, terminal_outcome, CommerceJobRequest,
    CommercePrincipal, ItemOutcome, JobItemExecutor, JobItemState, JobLine, JobOutcome, JobState,
};
use crate::offer::{CommercialOffer, Freshness, PriceVisibility};
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
    planner: Arc<dyn AcquisitionPlanning>,
}

impl CommerceSourceService {
    /// A disabled service: no store, no connectors, no browser, no clients.
    /// Constructing it touches nothing.
    pub fn disabled() -> Arc<Self> {
        Self::disabled_with_planner(Arc::new(AcquisitionPlanner::default()))
    }

    /// A disabled service with an explicit planner seam (diagnostics/tests).
    pub fn disabled_with_planner(planner: Arc<dyn AcquisitionPlanning>) -> Arc<Self> {
        Arc::new(Self {
            enabled: false,
            data_dir: None,
            store: None,
            registry: Mutex::new(ConnectorRegistry::new()),
            artifacts: Arc::new(RefusingArtifacts),
            ttls: CacheTtls::default(),
            gc: GcPolicy::default(),
            coalescer: SingleFlight::new(),
            planner,
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
        Self::open_with_planner(
            data_dir,
            config,
            artifacts,
            Arc::new(AcquisitionPlanner::default()),
        )
    }

    /// Construct the service with an explicit planner seam. Production uses
    /// the default [`AcquisitionPlanner`]; tests inject a spy to observe the
    /// production path without changing it.
    pub fn open_with_planner(
        data_dir: impl AsRef<Path>,
        config: ServiceConfig,
        artifacts: Arc<dyn ArtifactStore>,
        planner: Arc<dyn AcquisitionPlanning>,
    ) -> Result<Arc<Self>, CommerceStoreError> {
        if !config.enabled {
            return Ok(Self::disabled_with_planner(planner));
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
            planner,
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

    /// The pricing scope the caller context implies for LOOKUPS (`Account`
    /// when it carries an account scope, else `Public`). This never decides
    /// what a row is written under: that is the observation's own scope (see
    /// [`Self::admit_cache_scope`]).
    fn ctx_pricing_scope(ctx: &AcquireCtx) -> PricingScope {
        if ctx.account_scope.is_some() {
            PricingScope::Account
        } else {
            PricingScope::Public
        }
    }

    /// Admit the cache pricing scope and account key of one observation.
    ///
    /// The scope comes from the offer's own `price_visibility`, NEVER from
    /// the commerce ctx: an authenticated or account-scoped observation is
    /// admitted as `Authenticated`/`Account` even when the ctx is anonymous,
    /// so it can never be cached under `Public`. An account-specific
    /// observation with no account scope to key it by is the typed refusal
    /// ([`SourceError::InvalidRequest`]), never a silent widening.
    ///
    /// The ctx can only strengthen the scope, never weaken it: an
    /// account-scoped ctx stores a public/promotional/unknown observation
    /// under its own account scope, preserving account isolation.
    fn admit_cache_scope(
        ctx: &AcquireCtx,
        offer: &CommercialOffer,
    ) -> Result<(PricingScope, Option<AccountScope>), SourceError> {
        let (observed, observed_account) = match offer.price_visibility {
            PriceVisibility::Public => (PricingScope::Public, None),
            PriceVisibility::Promotional => (PricingScope::Promotional, None),
            PriceVisibility::Authenticated => (PricingScope::Authenticated, None),
            PriceVisibility::InquiryRequired | PriceVisibility::Unknown => {
                (PricingScope::Unknown, None)
            }
            PriceVisibility::AccountSpecific => (
                PricingScope::Account,
                Some(Self::observed_account_scope(offer).ok_or(SourceError::InvalidRequest)?),
            ),
        };
        if observed == PricingScope::Account {
            // The observation's own account keys the row; the ctx never
            // republishes account data under a different account.
            return Ok((observed, observed_account));
        }
        match &ctx.account_scope {
            Some(account) if observed != PricingScope::Authenticated => {
                Ok((PricingScope::Account, Some(account.clone())))
            }
            _ => Ok((
                observed,
                observed_account.or_else(|| ctx.account_scope.clone()),
            )),
        }
    }

    /// Admit the scope of a whole candidate set: the strongest observation
    /// wins, so a public sibling can never mask an authenticated or
    /// account-scoped candidate, and two different account scopes under one
    /// cache key are a typed refusal (never a mixed row).
    fn admit_candidates_cache_scope(
        ctx: &AcquireCtx,
        candidates: &[QuoteCandidate],
    ) -> Result<(PricingScope, Option<AccountScope>), SourceError> {
        let mut admitted: Option<(PricingScope, Option<AccountScope>)> = None;
        for candidate in candidates {
            let scope = Self::admit_cache_scope(ctx, &candidate.offer)?;
            admitted = Some(match admitted {
                None => scope,
                Some(current) => {
                    if scope.0 == PricingScope::Account
                        && current.0 == PricingScope::Account
                        && scope.1 != current.1
                    {
                        return Err(SourceError::InvalidRequest);
                    }
                    if scope_strength(scope.0) > scope_strength(current.0) {
                        scope
                    } else {
                        current
                    }
                }
            });
        }
        Ok(admitted.unwrap_or((PricingScope::Public, None)))
    }

    /// The account scope an account-specific observation is keyed by: the
    /// first account-specific tier's scope, else the acquisition provenance
    /// scope.
    fn observed_account_scope(offer: &CommercialOffer) -> Option<AccountScope> {
        fn first_break(breaks: &[crate::offer::PriceBreak]) -> Option<AccountScope> {
            breaks
                .iter()
                .find_map(|price_break| price_break.account_scope.clone())
        }
        first_break(&offer.price_breaks)
            .or_else(|| {
                offer
                    .variants
                    .iter()
                    .find_map(|variant| first_break(&variant.price_breaks))
            })
            .or_else(|| {
                offer
                    .packaging
                    .iter()
                    .find_map(|option| first_break(&option.price_breaks))
            })
            .or_else(|| offer.provenance.account_scope.clone())
    }

    /// The cache identity one observation set is WRITTEN under. The scope
    /// and account key are the admitted observation values, never the ctx
    /// (see [`Self::admit_cache_scope`]); quantity/packaging/variant remain
    /// part of the key.
    #[allow(clippy::too_many_arguments)]
    fn observation_identity(
        &self,
        class: CacheClass,
        source: &SourceId,
        product: &str,
        ctx: &AcquireCtx,
        admitted: (PricingScope, Option<AccountScope>),
        quantity: Option<crate::quantity::NonZeroQuantity>,
        packaging: Option<crate::packaging::PackagingType>,
        variant: Option<crate::text::VariantId>,
    ) -> Result<CacheIdentity, ServiceError> {
        let (scope, account_scope) = admitted;
        let mut identity = self.cache_identity(
            class, source, product, ctx, scope, quantity, packaging, variant,
        )?;
        identity.account_scope = account_scope;
        Ok(identity)
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

    // ------------------------------------------------------------- planning

    /// The planner learns the fields each operation actually promises, never
    /// a kitchen sink: discovery returns identity/description, exact product
    /// the same, a quote quantity tiers. A mechanism is only eligible when it
    /// covers all of them.
    fn requested_fields(capability: Capability) -> RequestedFields {
        match capability {
            Capability::Discovery | Capability::ExactProduct => {
                RequestedFields::of([RequestedField::Identity, RequestedField::Descriptive])
            }
            Capability::QuantityPricing => {
                RequestedFields::of([RequestedField::Identity, RequestedField::QuantityTiers])
            }
            Capability::Stock => RequestedFields::of([RequestedField::Availability]),
            Capability::Packaging => RequestedFields::of([RequestedField::Packaging]),
            Capability::AccountPricing => RequestedFields::of([RequestedField::AccountTerms]),
            Capability::SupplierData => RequestedFields::of([RequestedField::Descriptive]),
            Capability::Bulk => RequestedFields::of([RequestedField::BulkSet]),
        }
    }

    /// The acquire field a service capability covers, when they differ.
    fn field_capability(field: RequestedField) -> Option<Capability> {
        match field {
            RequestedField::Availability => Some(Capability::Stock),
            RequestedField::QuantityTiers => Some(Capability::QuantityPricing),
            RequestedField::Packaging => Some(Capability::Packaging),
            RequestedField::AccountTerms => Some(Capability::AccountPricing),
            RequestedField::BulkSet => Some(Capability::Bulk),
            RequestedField::Identity | RequestedField::Descriptive | RequestedField::Provenance => {
                None
            }
        }
    }

    /// Translate the registry's advertised capabilities into the planner's
    /// per-mechanism/per-field levels. A mechanism the connector advertises
    /// covers the requested fields the connector's capability booleans allow;
    /// everything else is demoted to `None` so the planner refuses instead of
    /// handing the connector a mechanism it never advertised.
    fn acquire_capabilities(
        capabilities: &crate::connector::ConnectorCapabilities,
        fields: &RequestedFields,
    ) -> AcquireCapabilities {
        let mut acquire = AcquireCapabilities::none();
        for mechanism in &capabilities.mechanisms {
            let mechanism = Self::acquire_mechanism(*mechanism);
            acquire = acquire.with_mechanism(mechanism, AcquireCapabilityLevel::Full);
            for field in fields.iter() {
                if let Some(capability) = Self::field_capability(field) {
                    if !capabilities.has(capability) {
                        acquire =
                            acquire.with_field(mechanism, field, AcquireCapabilityLevel::None);
                    }
                }
            }
        }
        acquire
    }

    fn acquire_mechanism(mechanism: AcquisitionMechanism) -> faktor_acquire::AcquisitionMechanism {
        match mechanism {
            AcquisitionMechanism::OfficialApi => faktor_acquire::AcquisitionMechanism::OfficialApi,
            AcquisitionMechanism::DirectHttp => faktor_acquire::AcquisitionMechanism::DirectHttp,
            AcquisitionMechanism::BrowserNetwork => {
                faktor_acquire::AcquisitionMechanism::BrowserNetwork
            }
            AcquisitionMechanism::EmbeddedState => {
                faktor_acquire::AcquisitionMechanism::EmbeddedState
            }
            AcquisitionMechanism::Dom => faktor_acquire::AcquisitionMechanism::Dom,
        }
    }

    fn commerce_mechanism(mechanism: faktor_acquire::AcquisitionMechanism) -> AcquisitionMechanism {
        match mechanism {
            faktor_acquire::AcquisitionMechanism::OfficialApi => AcquisitionMechanism::OfficialApi,
            faktor_acquire::AcquisitionMechanism::DirectHttp => AcquisitionMechanism::DirectHttp,
            faktor_acquire::AcquisitionMechanism::BrowserNetwork => {
                AcquisitionMechanism::BrowserNetwork
            }
            faktor_acquire::AcquisitionMechanism::EmbeddedState => {
                AcquisitionMechanism::EmbeddedState
            }
            faktor_acquire::AcquisitionMechanism::Dom => AcquisitionMechanism::Dom,
        }
    }

    fn acquire_freshness(freshness: FreshnessMode) -> RequestedFreshness {
        match freshness {
            FreshnessMode::PreferCache => RequestedFreshness::PreferCache,
            FreshnessMode::Live => RequestedFreshness::Live,
            FreshnessMode::CacheOnly => RequestedFreshness::CacheOnly,
        }
    }

    fn acquire_verification(kind: faktor_acquire::VerificationKind) -> VerificationKind {
        match kind {
            faktor_acquire::VerificationKind::Challenge => VerificationKind::Captcha,
            faktor_acquire::VerificationKind::Consent => VerificationKind::Manual,
            faktor_acquire::VerificationKind::Unknown => VerificationKind::Unknown,
        }
    }

    /// Map a planner refusal onto the typed domain failure. `NotFound` has no
    /// acquire spelling, so it maps back to `ProductNotFound`; the bound
    /// errors map onto extraction failures.
    fn acquire_error(error: &faktor_acquire::AcquisitionError) -> SourceError {
        use faktor_acquire::AcquisitionError::*;
        match error {
            Disabled => SourceError::Disabled,
            InvalidRequest { .. } => SourceError::InvalidRequest,
            AuthenticationRequired => SourceError::AuthenticationRequired,
            VerificationRequired { kind } => SourceError::VerificationRequired {
                kind: Self::acquire_verification(*kind),
            },
            RateLimited { retry_after_ms } => SourceError::RateLimited {
                retry_after_ms: *retry_after_ms,
            },
            QuotaExhausted { reset_ms } => SourceError::QuotaExhausted {
                reset_ms: *reset_ms,
            },
            CoolingDown { until_ms } => SourceError::CoolingDown {
                until_ms: *until_ms,
            },
            EgressUnavailable => SourceError::EgressUnavailable,
            NetworkTimeout => SourceError::NetworkTimeout,
            ApiUnavailable => SourceError::ApiUnavailable,
            BrowserUnavailable => SourceError::BrowserUnavailable,
            BrowserCrashed => SourceError::BrowserCrashed,
            NotFound => SourceError::ProductNotFound,
            ExtractionIncomplete | NestingTooDeep { .. } | TooManyRedirects { .. } => {
                SourceError::ExtractionIncomplete
            }
            ExtractionConflict => SourceError::ExtractionConflict,
            ResponseTooLarge { .. } => SourceError::ResponseTooLarge,
            Cancelled => SourceError::Cancelled,
            Deadline => SourceError::Deadline,
            Store => SourceError::Store,
        }
    }

    /// The planner-facing quota state. The context snapshot is bounded: an
    /// exhausted snapshot becomes an exhausted window whose reset is the
    /// snapshot's reset (never an unbounded bypass), everything else is a
    /// fresh window.
    fn acquire_quota(
        quota: Option<&crate::connector::QuotaState>,
        now_ms: u64,
    ) -> AcquireQuotaState {
        let mut state = AcquireQuotaState::new(
            QuotaWindow {
                window_ms: 60_000,
                limit: u32::MAX,
            },
            now_ms,
        );
        let Some(snapshot) = quota else {
            return state;
        };
        match (snapshot.remaining, snapshot.reset_ms) {
            (Some(0), Some(reset_ms)) if reset_ms > now_ms => {
                state.window.window_ms = reset_ms - now_ms;
                state.window.limit = 1;
                state.used = 1;
                state.exhausted_until_ms = Some(reset_ms);
            }
            (Some(remaining), _) => {
                state.window.limit = u32::try_from(remaining.max(1)).unwrap_or(u32::MAX);
                state.used = 0;
            }
            _ => {}
        }
        state
    }

    /// The planner-facing health: the path that could execute the connector's
    /// strongest advertised mechanism. A browser-only connector is planned
    /// against its browser state; an API-capable connector against its API
    /// state, so a browser outcome can never masquerade as API health.
    fn planner_health(
        registry: &ConnectorRegistry,
        source: &SourceId,
        mechanisms: &[AcquisitionMechanism],
        now_ms: u64,
    ) -> AcquireHealth {
        let api = mechanisms
            .iter()
            .find(|mechanism| AcquisitionPath::of_mechanism(**mechanism) == AcquisitionPath::Api);
        match api.or_else(|| mechanisms.first()) {
            Some(mechanism) => registry.mechanism_health(source, *mechanism, now_ms),
            None => AcquireHealth::Healthy,
        }
    }

    /// Build the acquire cache state for one identity from the service's own
    /// cache lookup, so the planner (not the connector) owns the
    /// serve-cache/acquire decision.
    fn acquire_cache_state(
        key: &str,
        ctx: &AcquireCtx,
        fields: &RequestedFields,
        lookup: &CacheLookup,
    ) -> CacheState {
        let mut cache = CacheState::new();
        let row = match lookup {
            CacheLookup::Fresh(row, _) | CacheLookup::Stale(row, _) => row,
            CacheLookup::Miss => return cache,
        };
        let account_scope = ctx.account_scope.as_ref().map(|a| a.as_str().to_string());
        for field in fields.iter() {
            cache.insert(
                CacheKey::new(key, account_scope.clone(), field),
                CacheEntry {
                    observed_at_ms: row.observed_at_ms,
                    class: freshness_class(field),
                    etag: row.etag.clone(),
                    last_modified: row.last_modified_ms.map(|ms| ms.to_string()),
                    content_digest: None,
                    content: None,
                },
            );
        }
        cache
    }

    /// Run the production planner for one source and request.
    #[allow(clippy::too_many_arguments)]
    fn plan_source(
        &self,
        source: &SourceId,
        class: CacheClass,
        fields: RequestedFields,
        freshness: FreshnessMode,
        ctx: &AcquireCtx,
        key: &str,
        cache: &CacheLookup,
        now_ms: u64,
    ) -> Result<AcquisitionPlan, ServiceError> {
        let (capabilities, mechanisms, health) = {
            let registry = self.registry.lock().map_err(|_| ServiceError::Poisoned)?;
            let capabilities = registry
                .capabilities(source)
                .ok_or(ServiceError::Source(SourceError::Disabled))?;
            let mechanisms: Vec<AcquisitionMechanism> = capabilities.mechanisms.clone();
            let health = Self::planner_health(&registry, source, &mechanisms, now_ms);
            (capabilities, mechanisms, health)
        };
        let ttl_ms = self.ttls.ttl_ms(class);
        let state = RuntimeAcquisitionState {
            identity: key.to_string(),
            fields: fields.clone(),
            freshness: Self::acquire_freshness(freshness),
            account_scope: ctx.account_scope.as_ref().map(|a| a.as_str().to_string()),
            capability: Self::acquire_capabilities(&capabilities, &fields),
            credential: CredentialAvailability::none(),
            quota: Self::acquire_quota(ctx.quota.as_ref(), now_ms),
            cache: Self::acquire_cache_state(key, ctx, &fields, cache),
            health,
            mechanisms: mechanisms
                .iter()
                .map(|m| Self::acquire_mechanism(*m))
                .collect(),
            ttl: FreshnessClassTtl {
                slow_ms: ttl_ms,
                moderate_ms: ttl_ms,
                fast_ms: ttl_ms,
            },
            field_health: ExtractionHealth::new(),
            now_ms,
        };
        Ok(self.planner.plan(&state))
    }

    /// The independent API/browser breaker states, for diagnostics/tests.
    pub fn breaker_state(&self, source: &SourceId, now_ms: u64) -> (Option<u64>, Option<u64>) {
        match self.registry.lock() {
            Ok(registry) => registry.breaker_state(source, now_ms),
            Err(_) => (None, None),
        }
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
            let mut lookup = CacheLookup::Miss;
            let mut stale_fallback: Option<(CachedRow, Freshness)> = None;
            if req.freshness != FreshnessMode::Live {
                match self.consult_cache(&identity, ttl, req.freshness, now)? {
                    CacheLookup::Fresh(row, freshness) => {
                        lookup = CacheLookup::Fresh(row, freshness);
                    }
                    CacheLookup::Stale(row, freshness) => {
                        if req.freshness == FreshnessMode::CacheOnly {
                            lookup = CacheLookup::Fresh(row, freshness);
                        } else {
                            stale_fallback = Some((row.clone(), freshness));
                            lookup = CacheLookup::Stale(row, freshness);
                        }
                    }
                    CacheLookup::Miss => {}
                }
            }

            let key = identity.digest();
            let plan = self.plan_source(
                &source,
                CacheClass::Discovery,
                Self::requested_fields(Capability::Discovery),
                req.freshness,
                ctx,
                &key,
                &lookup,
                now,
            )?;
            let planned = match plan.decision {
                PlanDecision::ServeFromCache { .. } => {
                    let (row, freshness) = match lookup {
                        CacheLookup::Fresh(row, freshness) | CacheLookup::Stale(row, freshness) => {
                            (row, freshness)
                        }
                        CacheLookup::Miss => return Err(ServiceError::Source(SourceError::Store)),
                    };
                    let cached: Vec<Discovery> =
                        row.payload.parse().map_err(ServiceError::Store)?;
                    marks.push(freshness);
                    discoveries.extend(cached);
                    continue;
                }
                PlanDecision::Acquire { mechanism, .. } => {
                    let mechanism = Self::commerce_mechanism(mechanism);
                    let path = AcquisitionPath::of_mechanism(mechanism);
                    let call_ctx = ctx
                        .clone()
                        .with_freshness(req.freshness)
                        .with_mechanism(mechanism);
                    let connector = {
                        let registry = self.registry.lock().map_err(|_| ServiceError::Poisoned)?;
                        registry
                            .connector(&source)
                            .ok_or(ServiceError::Source(SourceError::Disabled))?
                    };
                    let coalesce_key =
                        format!("discovery:{}:{}:{}", source, identity.digest(), req.limit);
                    let request = req.clone();
                    (
                        path,
                        self.coalescer
                            .run(coalesce_key, || {
                                let connector = connector.clone();
                                let call_ctx = call_ctx.clone();
                                async move {
                                    call_ctx
                                        .run_bounded(connector.discover(&call_ctx, request))
                                        .await
                                        .map(Acquired::Discoveries)
                                }
                            })
                            .await,
                    )
                }
                PlanDecision::RequireVerification { kind } => {
                    let error = SourceError::VerificationRequired {
                        kind: Self::acquire_verification(kind),
                    };
                    match stale_fallback {
                        Some((row, freshness)) => {
                            let cached: Vec<Discovery> =
                                row.payload.parse().map_err(ServiceError::Store)?;
                            marks.push(freshness);
                            discoveries.extend(cached);
                        }
                        None => return Err(ServiceError::Source(error)),
                    }
                    continue;
                }
                PlanDecision::Refuse { error } => {
                    if let Some((row, freshness)) = stale_fallback {
                        let cached: Vec<Discovery> =
                            row.payload.parse().map_err(ServiceError::Store)?;
                        marks.push(freshness);
                        discoveries.extend(cached);
                        continue;
                    }
                    if req.freshness == FreshnessMode::CacheOnly {
                        return Err(ServiceError::CacheMiss {
                            class: CacheClass::Discovery,
                        });
                    }
                    return Err(ServiceError::Source(Self::acquire_error(&error)));
                }
            };
            let (path, acquired) = planned;
            match acquired {
                Ok(Acquired::Discoveries(rows)) => {
                    if let Ok(registry) = self.registry.lock() {
                        registry.note_success(&source, path);
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
                        registry.note_failure(&source, path, &error, now);
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
                Self::ctx_pricing_scope(ctx),
                None,
                None,
                None,
            )?;
            let mut lookup = CacheLookup::Miss;
            let mut stale_fallback: Option<(CachedRow, Freshness)> = None;
            if req.freshness != FreshnessMode::Live {
                match self.consult_cache(&identity, ttl, req.freshness, now)? {
                    CacheLookup::Fresh(row, freshness) => {
                        lookup = CacheLookup::Fresh(row, freshness);
                    }
                    CacheLookup::Stale(row, freshness) => {
                        if req.freshness == FreshnessMode::CacheOnly {
                            lookup = CacheLookup::Fresh(row, freshness);
                        } else {
                            stale_fallback = Some((row.clone(), freshness));
                            lookup = CacheLookup::Stale(row, freshness);
                        }
                    }
                    CacheLookup::Miss => {}
                }
            }
            let key = identity.digest();
            let plan = self.plan_source(
                source,
                CacheClass::Product,
                Self::requested_fields(Capability::ExactProduct),
                req.freshness,
                ctx,
                &key,
                &lookup,
                now,
            )?;
            let planned = match plan.decision {
                PlanDecision::ServeFromCache { .. } => {
                    let (row, freshness) = match lookup {
                        CacheLookup::Fresh(row, freshness) | CacheLookup::Stale(row, freshness) => {
                            (row, freshness)
                        }
                        CacheLookup::Miss => return Err(ServiceError::Source(SourceError::Store)),
                    };
                    let offer: CommercialOffer =
                        row.payload.parse().map_err(ServiceError::Store)?;
                    return Ok(ProductOutcome { offer, freshness });
                }
                PlanDecision::Acquire { mechanism, .. } => {
                    let mechanism = Self::commerce_mechanism(mechanism);
                    let path = AcquisitionPath::of_mechanism(mechanism);
                    let call_ctx = ctx
                        .clone()
                        .with_freshness(req.freshness)
                        .with_mechanism(mechanism);
                    let connector = {
                        let registry = self.registry.lock().map_err(|_| ServiceError::Poisoned)?;
                        registry
                            .connector(source)
                            .ok_or(ServiceError::Source(SourceError::Disabled))?
                    };
                    let coalesce_key = format!("product:{}:{}", source, identity.digest());
                    let request = req.clone();
                    (
                        path,
                        self.coalescer
                            .run(coalesce_key, || {
                                let connector = connector.clone();
                                let call_ctx = call_ctx.clone();
                                async move {
                                    call_ctx
                                        .run_bounded(connector.product(&call_ctx, request))
                                        .await
                                        .map(|offer| Acquired::Offer(Box::new(offer)))
                                }
                            })
                            .await,
                    )
                }
                PlanDecision::RequireVerification { kind } => {
                    if let Some((row, freshness)) = stale_fallback {
                        let offer: CommercialOffer =
                            row.payload.parse().map_err(ServiceError::Store)?;
                        return Ok(ProductOutcome { offer, freshness });
                    }
                    last_error = Some(SourceError::VerificationRequired {
                        kind: Self::acquire_verification(kind),
                    });
                    continue;
                }
                PlanDecision::Refuse { error } => {
                    if let Some((row, freshness)) = stale_fallback {
                        let offer: CommercialOffer =
                            row.payload.parse().map_err(ServiceError::Store)?;
                        return Ok(ProductOutcome { offer, freshness });
                    }
                    if req.freshness == FreshnessMode::CacheOnly {
                        return Err(ServiceError::CacheMiss {
                            class: CacheClass::Product,
                        });
                    }
                    last_error = Some(Self::acquire_error(&error));
                    continue;
                }
            };
            let (path, acquired) = planned;
            match acquired {
                Ok(Acquired::Offer(offer)) => {
                    if let Ok(registry) = self.registry.lock() {
                        registry.note_success(source, path);
                    }
                    let mut offer = *offer;
                    crate::result::scrub_offer(&mut offer);
                    let payload = NormalizedPayload::from_serializable(&offer)?;
                    let admitted = Self::admit_cache_scope(ctx, &offer)?;
                    let write_identity = self.observation_identity(
                        CacheClass::Product,
                        source,
                        &product_key,
                        ctx,
                        admitted,
                        None,
                        None,
                        None,
                    )?;
                    self.write_cache(&write_identity, &payload, now)?;
                    self.persist_offer_snapshot(&offer, now);
                    return Ok(ProductOutcome {
                        offer,
                        freshness: Freshness::Live,
                    });
                }
                Ok(_) => return Err(ServiceError::Source(SourceError::Store)),
                Err(error) => {
                    if let Ok(registry) = self.registry.lock() {
                        registry.note_failure(source, path, &error, now);
                    }
                    if let Some((row, freshness)) = stale_fallback {
                        let offer: CommercialOffer =
                            row.payload.parse().map_err(ServiceError::Store)?;
                        return Ok(ProductOutcome { offer, freshness });
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
        // LOOKUP key only: writes take their scope from the observation.
        let pricing_scope = Self::ctx_pricing_scope(ctx);
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
            let mut lookup = CacheLookup::Miss;
            let mut stale_fallback: Option<(CachedRow, Freshness)> = None;
            if req.freshness != FreshnessMode::Live {
                match self.consult_cache(&identity, ttl, req.freshness, now)? {
                    CacheLookup::Fresh(row, freshness) => {
                        lookup = CacheLookup::Fresh(row, freshness);
                    }
                    CacheLookup::Stale(row, freshness) => {
                        if req.freshness == FreshnessMode::CacheOnly {
                            lookup = CacheLookup::Fresh(row, freshness);
                        } else {
                            stale_fallback = Some((row.clone(), freshness));
                            lookup = CacheLookup::Stale(row, freshness);
                        }
                    }
                    CacheLookup::Miss => {}
                }
            }
            let key = identity.digest();
            let plan = self.plan_source(
                source,
                CacheClass::Price,
                Self::requested_fields(Capability::QuantityPricing),
                req.freshness,
                ctx,
                &key,
                &lookup,
                now,
            )?;
            let planned = match plan.decision {
                PlanDecision::ServeFromCache { .. } => {
                    let (row, freshness) = match lookup {
                        CacheLookup::Fresh(row, freshness) | CacheLookup::Stale(row, freshness) => {
                            (row, freshness)
                        }
                        CacheLookup::Miss => return Err(ServiceError::Source(SourceError::Store)),
                    };
                    let candidates: Vec<QuoteCandidate> =
                        row.payload.parse().map_err(ServiceError::Store)?;
                    return Ok(QuoteOutcome {
                        candidates,
                        freshness,
                        account: ctx.account_scope.clone(),
                    });
                }
                PlanDecision::Acquire { mechanism, .. } => {
                    let mechanism = Self::commerce_mechanism(mechanism);
                    let path = AcquisitionPath::of_mechanism(mechanism);
                    let call_ctx = ctx
                        .clone()
                        .with_freshness(req.freshness)
                        .with_mechanism(mechanism);
                    let connector = {
                        let registry = self.registry.lock().map_err(|_| ServiceError::Poisoned)?;
                        registry
                            .connector(source)
                            .ok_or(ServiceError::Source(SourceError::Disabled))?
                    };
                    let coalesce_key = format!("quote:{}:{}", source, identity.digest());
                    let request = req.clone();
                    (
                        path,
                        self.coalescer
                            .run(coalesce_key, || {
                                let connector = connector.clone();
                                let call_ctx = call_ctx.clone();
                                async move {
                                    call_ctx
                                        .run_bounded(connector.quote(&call_ctx, request))
                                        .await
                                        .map(Acquired::Quotes)
                                }
                            })
                            .await,
                    )
                }
                PlanDecision::RequireVerification { kind } => {
                    if let Some((row, freshness)) = stale_fallback {
                        let candidates: Vec<QuoteCandidate> =
                            row.payload.parse().map_err(ServiceError::Store)?;
                        return Ok(QuoteOutcome {
                            candidates,
                            freshness,
                            account: ctx.account_scope.clone(),
                        });
                    }
                    last_error = Some(SourceError::VerificationRequired {
                        kind: Self::acquire_verification(kind),
                    });
                    continue;
                }
                PlanDecision::Refuse { error } => {
                    if let Some((row, freshness)) = stale_fallback {
                        let candidates: Vec<QuoteCandidate> =
                            row.payload.parse().map_err(ServiceError::Store)?;
                        return Ok(QuoteOutcome {
                            candidates,
                            freshness,
                            account: ctx.account_scope.clone(),
                        });
                    }
                    if req.freshness == FreshnessMode::CacheOnly {
                        return Err(ServiceError::CacheMiss {
                            class: CacheClass::Price,
                        });
                    }
                    last_error = Some(Self::acquire_error(&error));
                    continue;
                }
            };
            let (path, acquired) = planned;
            match acquired {
                Ok(Acquired::Quotes(candidates)) => {
                    if let Ok(registry) = self.registry.lock() {
                        registry.note_success(source, path);
                    }
                    let mut candidates = candidates;
                    crate::result::scrub_quote_candidates(&mut candidates);
                    let payload = NormalizedPayload::from_serializable(&candidates)?;
                    let admitted = Self::admit_candidates_cache_scope(ctx, &candidates)?;
                    let write_identity = self.observation_identity(
                        CacheClass::Price,
                        source,
                        &product_key,
                        ctx,
                        admitted,
                        Some(req.quantity),
                        req.packaging,
                        req.variant.clone(),
                    )?;
                    self.write_cache(&write_identity, &payload, now)?;
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
                        registry.note_failure(source, path, &error, now);
                    }
                    if let Some((row, freshness)) = stale_fallback {
                        let candidates: Vec<QuoteCandidate> =
                            row.payload.parse().map_err(ServiceError::Store)?;
                        return Ok(QuoteOutcome {
                            candidates,
                            freshness,
                            account: ctx.account_scope.clone(),
                        });
                    }
                    last_error = Some(error);
                }
            }
        }
        Err(ServiceError::Source(
            last_error.unwrap_or(SourceError::ProductNotFound),
        ))
    }

    /// Submit and run a deterministic BOM job (`bom`) for `requester`. The
    /// request's account scope must equal the principal's account scope: a
    /// job can never execute under an identity other than its owner's.
    pub async fn bom(
        &self,
        requester: &CommercePrincipal,
        ctx: &AcquireCtx,
        sources: SourceSet,
        freshness: FreshnessMode,
        detail: DetailLevel,
        bom: crate::bom::Bom,
    ) -> Result<JobOutcome, ServiceError> {
        self.ensure_enabled()?;
        let request = bom_request(bom, sources, freshness, detail, ctx.account_scope.clone());
        self.submit_and_advance(requester, ctx, request).await
    }

    /// Submit and run any deterministic job request for `requester`. The
    /// owner key is part of the digest and the durable row, so identical
    /// requests from different sessions/accounts never attach to one job.
    pub async fn submit_and_advance(
        &self,
        requester: &CommercePrincipal,
        ctx: &AcquireCtx,
        request: CommerceJobRequest,
    ) -> Result<JobOutcome, ServiceError> {
        self.ensure_enabled()?;
        if request.account != requester.account_scope {
            // The owner identity and the requested account scope are the
            // same fact; a divergence would execute under a mismatched
            // identity. Refuse typed instead of guessing.
            return Err(ServiceError::Source(SourceError::InvalidRequest));
        }
        let store = self.store_ref()?.clone();
        let now = now_ms();
        let enabled = self.enabled_sources(Capability::QuantityPricing);
        let profile = ctx.profile.clone();
        let (job, _attached) =
            crate::jobs::submit_job(&store, request, &enabled, profile.as_ref(), requester, now)?;
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

    /// The deterministic state/result of a job (`job`), visible ONLY to the
    /// principal that owns it. A different owner — including a legacy
    /// ownerless row that cannot be attributed at all — is indistinguishable
    /// from a missing job: typed NotFound, never forbidden.
    pub fn job_status(
        &self,
        requester: &CommercePrincipal,
        job_id: &str,
    ) -> Result<JobStatus, ServiceError> {
        self.ensure_enabled()?;
        let store = self.store_ref()?;
        let row = store
            .job(job_id)?
            .ok_or(ServiceError::Source(SourceError::ProductNotFound))?;
        let owner_key = requester.owner_key();
        if row.owner_key.as_deref() != Some(owner_key.as_str()) {
            return Err(ServiceError::Source(SourceError::ProductNotFound));
        }
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

    /// The digest a job request would receive for `requester`
    /// (diagnostics/tests). The owner key is part of the digest.
    pub fn digest_for(
        &self,
        requester: &CommercePrincipal,
        request: &CommerceJobRequest,
        profile: Option<&ProfileIdentity>,
    ) -> String {
        job_digest(
            request,
            &self.enabled_sources(Capability::QuantityPricing),
            profile,
            requester,
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
        // Every line carries its own deterministic selections (a `Quote`
        // job's request, or one BOM line's own variant/packaging, both bound
        // into the job digest). They must reach the quote request, or a
        // variant/packaging-scoped line would resolve at the offer level or
        // report VariantAmbiguous instead of the requested price.
        let packaging = line.packaging;
        let variant = line.variant.clone();
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

/// Rank a cache pricing scope by isolation strength, so a candidate set is
/// admitted under its strongest observation (never weakened to `Public`).
fn scope_strength(scope: PricingScope) -> u8 {
    match scope {
        PricingScope::Unknown => 0,
        PricingScope::Public => 1,
        PricingScope::Promotional => 2,
        PricingScope::Authenticated => 3,
        PricingScope::Account => 4,
    }
}

/// The wall clock in milliseconds.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
