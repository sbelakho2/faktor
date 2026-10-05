//! Economic model router (audit: minimize expected cost to VERIFIED
//! success, not price per token).
//!
//! Two candidate surfaces share one qualification/scoring ladder:
//! the PRICED production path ([`RouteCandidate`] = descriptor + a
//! [`PricingState`]) prices every call through the exact per-million
//! [`faktor_core::model::PriceQuote`] and never touches a per-token field;
//! the legacy descriptor-only constructors remain for untouched callers.
//!
//! Deterministic algorithm:
//! 1. ONE authoritative qualification pass (`qualified_candidates` /
//!    `qualified_priced_candidates`) applies every axis — capability filter
//!    (fail closed on unknown capabilities), context/output fit, phase
//!    quality floor, request budget, latency preference, and live
//!    rate-limit/cooldown state (static Hard state plus telemetry cooldowns
//!    fed in as a `LiveHealth` snapshot). Every selection path consumes
//!    ONLY `QualifiedCandidate` vectors; there is exactly one place that
//!    filters;
//! 2. scoring is explicit on `ScoredCandidate`/`PricedScoredCandidate`:
//!    expected cost-to-success in microUSD (prompt-cache-aware, integer,
//!    rounded up) with the documented tie-break ladder — no tuple-slot
//!    overloading, no unit-confused comparisons (base microUSD is never
//!    compared against milliseconds), and an unpriced/Unknown candidate is
//!    NON-numeric (ranked after every number, never read as zero);
//! 3. money is computed only from a [`faktor_core::model::PricingSnapshot`]
//!    quote ([`faktor_core::model::PriceQuote::quote_cost_micro`]) and
//!    carried as [`CostEstimate`]; the legacy per-token projection is
//!    confined to the compatibility constructors and is never consulted by
//!    the priced path;
//! 4. local zero-cost models count as cost-free but latency-weighted;
//! 5. every decision carries an audit string (phase, considered,
//!    qualified, chosen, cost, latency, floor) — no hidden choices;
//! 6. verified-outcome history is ADDITIVE (audit items 13/14/L): a
//!    [`RouterService`] may be built with an [`OutcomeStore`]
//!    ([`RouterService::with_outcomes`]) whose per-key verified stats, when
//!    they exist, replace the telemetry-prior expected-cost term with the
//!    conservative [`WorkCostEstimate`] — expected cost to VERIFIED
//!    completion = immediate cost + P(rework) x downstream spend, with
//!    rework probability as a Wilson upper bound and the conservative
//!    verified-success confidence as the ladder's success prior
//!    (MaximumQuality's consumption). The default registry is empty and
//!    scoring over it is byte-identical to the legacy math.
//!
//! Pinned routing ([`RouterService::with_pinned_route_candidates`]) calls
//! ONLY [`qualify_specific`]: the pin never competes, and no other
//! candidate can reject it.

use std::collections::HashMap;

/// Classified lock recovery for DERIVED state (caches, registries, rings,
/// process/ownership projections): a poisoned guard is recovered with the
/// poison flag cleared, so one panicking caller can never wedge later use.
/// The durable authority (store/journal/OS process state) remains the
/// source of truth; the recovered value is only ever a projection of it.
fn recover_lock<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(|poisoned| {
        lock.clear_poison();
        poisoned.into_inner()
    })
}
use std::sync::{Arc, Mutex};

use faktor_core::model::{
    unix_now_ms, EffectivePriceState, ModelDescriptor, ModelEconomics, ModelPerformance,
    PriceAuthority, PricingSnapshot, PricingState, QualityAuthority, QualityStatement,
    RateLimitState, RiskBucket, RouteDecision, RouterPhase, TaskClass, TokenUsage, VerifiedOutcome,
};

/// MODEL-CHECK-ONLY journaled task-budget ledger (micro-units) with
/// reservations, settlements, refunds and crash reconstruction from the
/// denial journal — model-checked against the router's hard-budget
/// semantics (audit 79-80).
///
/// This is a verification model, NOT a product budget authority: no
/// runtime code consumes it to authorize spend. The durable accounting
/// authority is `faktor-session`'s budget ledger; keeping this module
/// named `budget_model` makes that distinction explicit at every use site.
pub mod budget_model;

/// Prefix-cache stability measurement: per-turn stability, session mean/std,
/// the churn advisory detector and the churn cost premium (audits 65-66).
/// Pure and deterministic; inputs are per-turn prefix observations the
/// settlement layer persists.
pub mod stability;

/// Verified-outcome learning (audit items 13/14/L): conservative Bayesian
/// rework estimates from verified-only history, plus the outcome-registry
/// surface a [`RouterService`] consults when stats exist.
pub mod outcomes;

pub use outcomes::{
    rework_probability_ppm, verified_success_confidence_ppm, work_cost_estimate, EmptyOutcomeStore,
    MemoryOutcomeStore, OutcomeKey, OutcomeSample, OutcomeStore, OutcomeView, VerifiedOutcomeStats,
    WorkCostEstimate,
};

/// One routing request.
///
/// `task_class`/`risk_bucket` are the OUTCOME-LEARNING dimensions of the
/// request (audit items 13/14/L): the verified-outcome consult walks the
/// hierarchy (provider, model, phase, task, risk) -> (provider, model,
/// phase, task) -> (provider, model, phase) -> global telemetry prior —
/// it never collapses straight to the global prior while a narrower key
/// holds evidence. Both default to the neutral bucket for additive
/// compatibility.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RouteRequest {
    pub phase: RouterPhase,
    pub required_capabilities: Vec<String>,
    pub context_tokens: u64,
    pub estimated_output_tokens: u64,
    /// 0..=100; the HARD quality minimum every qualified candidate must
    /// clear (`None`-target requests are hard-only).
    pub quality_floor: u8,
    /// The adaptive quality target (adaptive-requirement audit): when this
    /// is `Some(target)` with `target > quality_floor`, the router
    /// qualifies against the hard `quality_floor` and then PREFERS every
    /// candidate whose phase quality clears `target`; candidates in
    /// `[quality_floor, target)` serve only when no target-or-higher
    /// candidate survives the other hard axes (capability/fit, budget,
    /// latency, live health) — the policy permits the relaxation exactly
    /// when it sends a target at all. `None` is a hard requirement: nothing
    /// below `quality_floor` may serve and no relaxation exists. Values are
    /// clamped (`target >= floor`, `<= 100`) by the qualification pass.
    #[serde(default)]
    pub quality_target: Option<u8>,
    /// Remaining task budget in micro-units (0 = unlimited).
    pub task_budget_remaining_micro: u64,
    pub latency_preference_ms: Option<u64>,
    /// The class of work this request carries (outcome-learning dimension).
    #[serde(default)]
    pub task_class: TaskClass,
    /// The semantic risk bucket of the operation (outcome-learning
    /// dimension), decided by the runtime's own risk model.
    #[serde(default)]
    pub risk_bucket: RiskBucket,
}

impl Default for RouteRequest {
    fn default() -> Self {
        Self {
            phase: RouterPhase::Implement,
            required_capabilities: vec!["tools".into(), "streaming".into()],
            context_tokens: 16_384,
            estimated_output_tokens: 2048,
            quality_floor: 60,
            quality_target: None,
            task_budget_remaining_micro: 0,
            latency_preference_ms: None,
            task_class: TaskClass::Medium,
            risk_bucket: RiskBucket::Low,
        }
    }
}

/// Observed cache state for (provider, model): tokens already cached
/// (read hits) and tokens this call writes into the cache.
#[derive(Debug, Clone, PartialEq)]
pub struct CacheState {
    pub provider: String,
    pub model: String,
    pub cached_input_tokens: u64,
    pub will_write_tokens: u64,
}

/// The router's monetary estimate of ONE candidate call (pricing-path
/// audit): the EXACT per-million-token quote evaluated over the request's
/// token categories. The variant carries the pricing AUTHORITY, never a
/// number inferred from one:
///
/// - [`CostEstimate::Known`] — an exact quote priced the call;
/// - [`CostEstimate::Conservative`] — a conservative ceiling (or the
///   legacy descriptor projection on compatibility paths) priced it;
/// - [`CostEstimate::LocalZero`] — an authoritative local zero (exactly 0);
/// - [`CostEstimate::Unknown`] — NO number is honest; the router never
///   treats it as 0 and never lets it into numeric comparisons.
///
/// [`CostEstimate::numeric`] is the ONLY way to reach a number: `Unknown`
/// returns `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostEstimate {
    Known(u64),
    Conservative(u64),
    LocalZero,
    Unknown,
}

impl CostEstimate {
    /// Evaluate a frozen [`PricingSnapshot`] against one usage frame: the
    /// summed-numerator, one-ceiling-division quote math, keyed by the
    /// snapshot's AUTHORITY. A hostile inconsistency (Unknown authority
    /// with a quote, LocalZero without one) resolves to `Unknown` — no
    /// fabricated number.
    pub fn from_snapshot(snapshot: &PricingSnapshot, usage: TokenUsage) -> Self {
        match snapshot.authority {
            PriceAuthority::Exact => snapshot
                .quote
                .map(|q| CostEstimate::Known(q.quote_cost_micro(usage)))
                .unwrap_or(CostEstimate::Unknown),
            PriceAuthority::ConservativeCeiling => snapshot
                .quote
                .map(|q| CostEstimate::Conservative(q.quote_cost_micro(usage)))
                .unwrap_or(CostEstimate::Unknown),
            PriceAuthority::LocalZero => snapshot
                .quote
                .map(|_| CostEstimate::LocalZero)
                .unwrap_or(CostEstimate::Unknown),
            PriceAuthority::Unknown => CostEstimate::Unknown,
        }
    }

    /// Evaluate a [`PricingState`] against one usage frame.
    pub fn from_state(state: &PricingState, usage: TokenUsage) -> Self {
        Self::from_snapshot(&state.snapshot(), usage)
    }

    /// Evaluate the ROUTE-TIME [`EffectivePriceState`] (derived from a
    /// pricing state + clock + hard-cap flag) against one usage frame. An
    /// expired exact quote arrives here as `Unknown` (or `Conservative`
    /// when a documented ceiling exists) and is never read as its old
    /// number.
    pub fn from_effective(state: &EffectivePriceState, usage: TokenUsage) -> Self {
        match state {
            EffectivePriceState::Exact(s) => s
                .quote
                .map(|q| CostEstimate::Known(q.quote_cost_micro(usage)))
                .unwrap_or(CostEstimate::Unknown),
            EffectivePriceState::Conservative(s) => s
                .quote
                .map(|q| CostEstimate::Conservative(q.quote_cost_micro(usage)))
                .unwrap_or(CostEstimate::Unknown),
            EffectivePriceState::LocalZero => CostEstimate::LocalZero,
            EffectivePriceState::Unknown(_) => CostEstimate::Unknown,
        }
    }

    /// The honest number, when one exists: exact/ceiling costs and the
    /// authoritative local zero. `Unknown` is NON-numeric — it is never 0.
    pub const fn numeric(self) -> Option<u64> {
        match self {
            CostEstimate::Known(m) | CostEstimate::Conservative(m) => Some(m),
            CostEstimate::LocalZero => Some(0),
            CostEstimate::Unknown => None,
        }
    }

    pub const fn is_unknown(self) -> bool {
        matches!(self, CostEstimate::Unknown)
    }

    pub const fn is_local_zero(self) -> bool {
        matches!(self, CostEstimate::LocalZero)
    }
}

/// The router's PRICED unit (pricing-path audit): a model descriptor plus
/// the pricing state the catalog authority resolved for it. Every priced
/// qualification/scoring path consumes these; the descriptor alone can no
/// longer smuggle a lossy per-token projection into a decision.
///
/// `quality` is the authority-carrying quality statement of the same
/// catalog row (quality-authority audit items 1/11): the four reliability
/// dimensions plus their provenance. Qualification reads THIS, so a row
/// whose quality is [`QualityAuthority::ConservativeUnknown`] can never
/// clear a floor by its placeholder number — only a user declaration or
/// durable verified outcomes authorize it.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteCandidate {
    pub descriptor: ModelDescriptor,
    pub pricing: PricingState,
    pub quality: QualityStatement,
}

impl RouteCandidate {
    /// The legacy shape: the quality statement is derived from the
    /// descriptor's performance with [`QualityAuthority::BuiltInPrior`]
    /// provenance (a prior, never a measured value). Untouched
    /// descriptor-only callers keep their exact numeric behavior; catalog
    /// callers use [`RouteCandidate::with_quality`] to carry the row's REAL
    /// authority.
    ///
    /// This is the ONLY caller of the LOSSY compat constructor
    /// [`QualityStatement::from_legacy_performance`] (audit item 21): a
    /// descriptor has no tool/reasoning dimensions, so those two metrics
    /// are fabricated from coding here. New production code must carry a
    /// per-dimension [`QualityStatement`] through
    /// [`RouteCandidate::with_quality`] instead.
    pub fn new(descriptor: ModelDescriptor, pricing: PricingState) -> Self {
        let quality = QualityStatement::from_legacy_performance(
            &descriptor.performance(),
            QualityAuthority::BuiltInPrior,
        );
        Self {
            descriptor,
            pricing,
            quality,
        }
    }

    /// The authority-carrying constructor: descriptor + catalog pricing
    /// state + the catalog row's quality statement (provenance included).
    pub fn with_quality(
        descriptor: ModelDescriptor,
        pricing: PricingState,
        quality: QualityStatement,
    ) -> Self {
        Self {
            descriptor,
            pricing,
            quality,
        }
    }

    /// The candidate's non-monetary performance view.
    pub fn performance(&self) -> ModelPerformance {
        self.descriptor.performance()
    }

    /// The candidate's effective price state at `now_ms` under the
    /// presence/absence of a hard cost cap. A `Known` quote past its
    /// validity window is NOT usable as exact: it becomes `Unknown` (or
    /// `Conservative` at a documented ceiling under a hard cap), and a
    /// `Stale` row is always `Unknown` — a stale exact quote is not a
    /// conservative bound.
    pub fn effective_pricing(&self, now_ms: u64, hard_cost_cap: bool) -> EffectivePriceState {
        self.pricing.effective_at(now_ms, hard_cost_cap)
    }

    /// The candidate's exact cost estimate for this request/cache state at
    /// route time, evaluated through the effective per-million-token quote
    /// (never through a per-token projection).
    pub fn cost_estimate_at(
        &self,
        req: &RouteRequest,
        cache: &[CacheState],
        now_ms: u64,
    ) -> CostEstimate {
        let effective = self.effective_pricing(now_ms, req.task_budget_remaining_micro > 0);
        CostEstimate::from_effective(&effective, request_usage(req, cache, &self.descriptor))
    }

    /// [`RouteCandidate::cost_estimate_at`] at the current wall clock.
    pub fn cost_estimate(&self, req: &RouteRequest, cache: &[CacheState]) -> CostEstimate {
        self.cost_estimate_at(req, cache, unix_now_ms())
    }
}

/// The token categories one request/cache state projects for a candidate.
fn request_usage(req: &RouteRequest, cache: &[CacheState], d: &ModelDescriptor) -> TokenUsage {
    request_usage_with_input(req, cache, d, req.context_tokens)
}

/// [`request_usage`] over an EXPLICIT input-token count: the
/// candidate-specific sizing pass measures the rendered request under the
/// candidate's own tokenizer and re-prices the call with THIS count, while
/// the cache-read accounting keys off the same measured input (a read can
/// never discount more than the request actually carries).
fn request_usage_with_input(
    req: &RouteRequest,
    cache: &[CacheState],
    d: &ModelDescriptor,
    input_tokens: u64,
) -> TokenUsage {
    let cs = cache
        .iter()
        .find(|c| c.provider == d.provider && c.model == d.model);
    let (cached, will_write) = cs
        .map(|c| (c.cached_input_tokens.min(input_tokens), c.will_write_tokens))
        .unwrap_or((0, 0));
    TokenUsage::new(
        input_tokens.saturating_sub(cached),
        cached,
        will_write,
        req.estimated_output_tokens,
    )
}

/// LEGACY descriptor-only cost estimate (microUSD) over the per-token
/// compatibility fields of [`ModelEconomics`]. **The priced routing path
/// never calls this**: it prices calls through
/// [`CostEstimate::from_state`] over the exact per-million-token quote.
/// This function survives for untouched descriptor-only callers (agent
/// budget fallbacks, compatibility constructors and their tests); values
/// below $1/M cannot be represented here, which is exactly why new price
/// knowledge must never enter through it.
///
/// UNITS (audit 9 + P0-5): the price fields are
/// [`faktor_core::model::MicroUsdPerToken`] — microUSD PER TOKEN, typed so
/// money can never mix with latency. The per-token microUSD value is
/// numerically equal to USD per million tokens (1e6 microUSD per USD over
/// 1e6 tokens), so a $15/Mtok price occupies the same integer as 15
/// microUSD/token and one integer serves both readings: cost_micro =
/// sum(tokens x price) with NO division. Saturating arithmetic makes
/// hostile magnitudes safe.
pub fn estimated_call_cost(
    ec: &ModelEconomics,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
) -> u64 {
    let uncached = input_tokens.saturating_sub(cache_read_tokens);
    let mut cost = ec
        .input_price_per_mtok
        .saturating_mul(uncached)
        .saturating_add(
            ec.cache_read_price_per_mtok
                .saturating_mul(cache_read_tokens),
        )
        .saturating_add(
            ec.cache_write_price_per_mtok
                .saturating_mul(cache_write_tokens),
        )
        .saturating_add(ec.output_price_per_mtok.saturating_mul(output_tokens));
    // Cost is never understated: any priced byte carries at least 1 micro.
    if cost == 0 && !ec.is_local_zero_cost() {
        cost = 1;
    }
    cost
}

// ---------------------------------------------------------- qualification

/// Blended success prior when nothing was ever recorded for a key:
/// [`PRIOR_SUCCESS`] (0.8) expressed in parts-per-million.
const DEFAULT_SUCCESS_PPM: u32 = 800_000;

/// Point-in-time live-health snapshot consumed by the single qualification
/// pass ([`qualified_candidates`]).
///
/// [`RouterService`] captures one from its [`RouterTelemetry`] at the start
/// of every route; the plain [`Router`] path uses the default view (no
/// live cooldowns, prior success everywhere). Capturing once per decision
/// keeps a route deterministic within its own evaluation.
#[derive(Debug, Clone, Default)]
pub struct LiveHealth {
    /// Providers inside an active rate-limit cooldown window.
    cooldown: std::collections::HashSet<String>,
    /// Telemetry-blended success prior in ppm, keyed
    /// `(provider, model, phase)`.
    success_ppm: std::collections::HashMap<(String, String, RouterPhase), u32>,
}

impl LiveHealth {
    fn cooldown_active(&self, provider: &str) -> bool {
        self.cooldown.contains(provider)
    }

    fn success_ppm(&self, provider: &str, model: &str, phase: RouterPhase) -> u32 {
        self.success_ppm
            .get(&(provider.to_string(), model.to_string(), phase))
            .copied()
            .unwrap_or(DEFAULT_SUCCESS_PPM)
    }
}

/// One candidate that cleared EVERY qualification axis for a request:
/// live rate-limit/cooldown state, capabilities, context/output fit, the
/// phase quality floor, the request budget and the latency preference.
///
/// Every selection path — plain cheapest ([`Router::route`]),
/// expected-cost ([`RouterService::route`]), maximum-quality requests and
/// the escalation pool — consumes ONLY vectors of these. There is exactly
/// one filtering pass in the router; a candidate absent here can never be
/// chosen, costed as an escalation target, or tie-broken into a decision.
#[derive(Debug, Clone, Copy)]
pub struct QualifiedCandidate<'a> {
    pub descriptor: &'a ModelDescriptor,
    /// Cache-aware single-call cost in microUSD (the base cost: integer,
    /// rounded up to >= 1 micro for any priced model). For an
    /// [`CostEstimate::Unknown`] priced candidate this carries `u64::MAX`
    /// (unbounded) — [`QualifiedCandidate::cost`] is the authoritative
    /// estimate; unknown is NEVER a numeric zero.
    pub call_cost_micro: u64,
    /// Telemetry-blended success prior in ppm (1_000_000 = certain).
    pub success_ppm: u32,
    /// The authoritative cost estimate of this candidate (pricing-path
    /// audit): exact quote cost, conservative ceiling, authoritative local
    /// zero, or non-numeric Unknown.
    pub cost: CostEstimate,
}

impl QualifiedCandidate<'_> {
    /// The numeric base cost, when one exists (`None` for Unknown).
    pub const fn numeric_cost(&self) -> Option<u64> {
        self.cost.numeric()
    }
}

/// Why [`qualified_candidates`] returned an empty vector, staged so each
/// path reproduces its distinct denial message (capability/fit, quality
/// floor, or budget/latency).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QualificationFailure {
    pub phase: RouterPhase,
    /// The floor actually applied: `quality_floor` clamped to 100.
    pub quality_floor: u8,
    /// Required capabilities supported by no candidate that cleared the
    /// live-health axis.
    pub unsupported_capabilities: Vec<String>,
    /// Survivors of capability + context/output fit.
    pub fit_survivors: usize,
    /// Survivors of the phase quality floor as well.
    pub quality_survivors: usize,
}

impl QualificationFailure {
    /// The distinct denial strings of the legacy plain router, derived from
    /// the staged survivor counts (never from a second filtering loop).
    pub fn route_error(&self) -> String {
        if self.fit_survivors == 0 {
            format!(
                "no candidate clears capability/fit filtering (missing: {})",
                self.unsupported_capabilities.join(",")
            )
        } else if self.quality_survivors == 0 {
            format!(
                "no candidate clears the quality floor {} for phase {:?}",
                self.quality_floor, self.phase
            )
        } else {
            "no candidate within the remaining budget/latency constraints".to_string()
        }
    }
}

fn clears_quality_floor(p: &ModelPerformance, phase: RouterPhase, floor: u8) -> bool {
    // Heavy phases (Implement/Review/Debug) judge the coding metric; cheap
    // phases judge context — the single per-phase mapping lives in
    // `QualityStatement::phase_metric`. The descriptor-only compatibility
    // path carries no per-dimension authority: its numbers are declared as
    // the legacy built-in prior (exactly their pre-audit numeric behavior).
    // Authority-carrying callers qualify through
    // `effective_quality_statement`, whose statements read the PHASE
    // METRIC's own authority.
    QualityStatement::from_legacy_performance(p, QualityAuthority::BuiltInPrior)
        .clears_floor(phase, floor)
}

/// The clamped adaptive quality band `(minimum, Option<target>)` of one
/// request (adaptive-requirement audit): the minimum is `quality_floor`
/// bounded to 0..=100, and the target (when present) is clamped into
/// `minimum..=100` and DROPPED when it equals the minimum — a request whose
/// target is its minimum is a hard requirement with no preference tier.
pub fn quality_band(floor: u8, target: Option<u8>) -> (u8, Option<u8>) {
    let minimum = floor.min(100);
    let target = target
        .map(|t| t.min(100).max(minimum))
        .filter(|t| *t > minimum);
    (minimum, target)
}

/// Apply the adaptive target preference to a fully qualified set: when the
/// request carries a target and ANY qualified candidate clears it on the
/// SAME authority-aware phase metric the floor axis used, only those
/// candidates remain; otherwise the `[minimum, target)` survivors stay
/// (the relaxation the target itself permits). A hard request (no target)
/// is untouched. The closure must return the candidate's target-tier
/// decision for the given target value.
fn prefer_quality_target<'a>(
    qualified: &mut Vec<QualifiedCandidate<'a>>,
    req: &RouteRequest,
    clears_target: impl Fn(&ModelDescriptor, u8) -> bool,
) {
    let (_, target) = quality_band(req.quality_floor, req.quality_target);
    let Some(target) = target else {
        return;
    };
    if qualified
        .iter()
        .any(|q| clears_target(q.descriptor, target))
    {
        qualified.retain(|q| clears_target(q.descriptor, target));
    }
}

/// The default (empty) outcome registry for qualification entry points that
/// carry no durable verified-outcome store: every measured consult misses
/// and the candidate's OWN quality statement decides.
static EMPTY_OUTCOMES: outcomes::EmptyOutcomeStore = outcomes::EmptyOutcomeStore;

/// The AUTHORITY-AWARE quality statement of one candidate under one request
/// (quality-authority audit items 1/11):
///
/// - when the durable verified-outcome store holds evidence for this
///   (provider, model, phase, task_class, risk_bucket) key, the MEASURED
///   statement is returned — its reliability is the conservative
///   verified-success confidence scaled to 0..=100 and its authority is
///   [`QualityAuthority::Measured`] with the evidence attached. Measured
///   outcomes supersede the candidate's declared quality (a user
///   declaration can never exceed measured reality);
/// - otherwise the candidate's own authority-carrying statement is
///   returned verbatim. A [`QualityAuthority::ConservativeUnknown`]
///   placeholder clears nothing above floor 0; only a user declaration
///   (or a declared built-in prior) authorizes it.
pub fn effective_quality_statement(
    candidate: &RouteCandidate,
    phase: RouterPhase,
    task_class: TaskClass,
    risk_bucket: RiskBucket,
    now_ms: u64,
    outcomes: &dyn outcomes::OutcomeView,
) -> QualityStatement {
    let d = &candidate.descriptor;
    if let Some(stats) =
        outcomes.lookup_stats(&d.provider, &d.model, phase, task_class, risk_bucket)
    {
        let success_ppm = outcomes::verified_success_confidence_ppm(&stats);
        return QualityStatement::measured(
            success_ppm,
            VerifiedOutcome {
                provider: d.provider.clone(),
                model: d.model.clone(),
                phase,
                success_ppm,
                sample_count: stats.sample_count,
                observed_at_ms: now_ms,
            },
        );
    }
    candidate.quality.clone()
}

/// The authority-aware 0..=100 phase quality of one statement: the numeric
/// value the qualification floor compares, on the SAME authority the floor
/// decision uses. A [`QualityAuthority::Measured`] statement carries its
/// scaled confidence; a [`QualityAuthority::UserConfigured`] or
/// [`QualityAuthority::BuiltInPrior`] declaration carries its declared
/// value; a [`QualityAuthority::ConservativeUnknown`] placeholder is `0` —
/// its numeric placeholder 50 is NOT a measured 50 and authorizes nothing
/// above floor 0.
pub fn phase_quality_value(statement: &QualityStatement, phase: RouterPhase) -> u8 {
    let metric = statement.phase_metric(phase);
    match metric.authority {
        QualityAuthority::Measured(_)
        | QualityAuthority::UserConfigured { .. }
        | QualityAuthority::BuiltInPrior => metric.value,
        QualityAuthority::ConservativeUnknown => 0,
    }
}

/// Cache-aware base cost of one call, shared by the qualification budget
/// axis and the scored candidates (computed exactly once per candidate).
fn base_call_cost(d: &ModelDescriptor, req: &RouteRequest, cache: &[CacheState]) -> u64 {
    let cs = cache
        .iter()
        .find(|c| c.provider == d.provider && c.model == d.model);
    let (cached, will_write) = cs
        .map(|c| {
            (
                c.cached_input_tokens.min(req.context_tokens),
                c.will_write_tokens,
            )
        })
        .unwrap_or((0, 0));
    estimated_call_cost(
        &d.economics,
        req.context_tokens,
        req.estimated_output_tokens,
        cached,
        will_write,
    )
}

/// THE single authoritative qualification pass (audit P0-3/P0-89).
///
/// Applied once per route, in this order, with every axis evaluated in one
/// loop over the candidates:
/// 1. live rate-limit/cooldown state: static [`RateLimitState::Hard`] or an
///    active cooldown in `health` (replaces the inert
///    `RouteRequest::rate_limit_blocks` — P0-89);
/// 2. required capabilities (fail closed on unknown capabilities);
/// 3. context window and max-output fit;
/// 4. the phase quality floor (coding mean for Implement/Review/Debug,
///    context reliability otherwise);
/// 5. the request budget (`task_budget_remaining_micro`, 0 = unlimited);
/// 6. the latency preference (a hard filter when set).
///
/// The returned vector contains only candidates that passed every axis; it
/// is the ONLY input every selection path is allowed to choose from.
pub fn qualified_candidates<'a>(
    candidates: &'a [ModelDescriptor],
    req: &RouteRequest,
    cache: &[CacheState],
    health: &LiveHealth,
) -> Result<Vec<QualifiedCandidate<'a>>, QualificationFailure> {
    let (floor, _target) = quality_band(req.quality_floor, req.quality_target);
    let mut fit_survivors = 0usize;
    let mut quality_survivors = 0usize;
    let mut supported: Vec<String> = Vec::new();
    let mut qualified: Vec<QualifiedCandidate<'a>> = Vec::new();
    for d in candidates {
        // Axis 1: live rate-limit/cooldown state (static + telemetry).
        if d.performance().rate_limit_state == RateLimitState::Hard
            || health.cooldown_active(&d.provider)
        {
            continue;
        }
        for cap in &req.required_capabilities {
            if !supported.iter().any(|s| s == cap) && d.capability_ok(std::slice::from_ref(cap)) {
                supported.push(cap.clone());
            }
        }
        // Axis 2: capabilities (fail closed).
        if !d.capability_ok(&req.required_capabilities) {
            continue;
        }
        // Axis 3: context/output fit.
        if d.context < req.context_tokens || d.max_output < req.estimated_output_tokens {
            continue;
        }
        fit_survivors += 1;
        // Axis 4: quality floor.
        if !clears_quality_floor(&d.performance(), req.phase, floor) {
            continue;
        }
        quality_survivors += 1;
        // Axis 5: request budget.
        let cost = base_call_cost(d, req, cache);
        if req.task_budget_remaining_micro > 0 && cost > req.task_budget_remaining_micro {
            continue;
        }
        // Axis 6: latency preference (hard filter when set).
        if let Some(lp) = req.latency_preference_ms {
            if d.performance().estimated_latency_ms > lp {
                continue;
            }
        }
        qualified.push(QualifiedCandidate {
            descriptor: d,
            call_cost_micro: cost,
            success_ppm: health.success_ppm(&d.provider, &d.model, req.phase),
            // The legacy descriptor path has no exact quote: its rounded-up
            // per-token projection is a CONSERVATIVE estimate, never a
            // claimed exact price.
            cost: CostEstimate::Conservative(cost),
        });
    }
    if qualified.is_empty() {
        let unsupported_capabilities = req
            .required_capabilities
            .iter()
            .filter(|c| !supported.iter().any(|s| s == *c))
            .cloned()
            .collect();
        return Err(QualificationFailure {
            phase: req.phase,
            quality_floor: floor,
            unsupported_capabilities,
            fit_survivors,
            quality_survivors,
        });
    }
    // Adaptive target preference (adaptive-requirement audit): a target
    // survivor ALWAYS wins; the `[floor, target)` survivors serve only when
    // no target candidate survived every hard axis above — the relaxation
    // the policy authorized by sending a target at all.
    prefer_quality_target(&mut qualified, req, |d, target| {
        clears_quality_floor(&d.performance(), req.phase, target)
    });

    Ok(qualified)
}

/// THE priced qualification pass (pricing-path audit): the same axes as
/// [`qualified_candidates`], but every monetary decision is made on the
/// candidate's [`CostEstimate`] from its exact per-million quote — never on
/// a per-token projection.
///
/// The budget axis is where "unknown is nonnumeric" bites: with a positive
/// `task_budget_remaining_micro`, an [`CostEstimate::Unknown`] candidate is
/// excluded (no honest bound fits the cap — fail closed); without a cap it
/// is admitted and its spend settles as a documented Unknown amount.
/// Numeric costs (exact, conservative, local zero) compare against the
/// remaining budget exactly.
pub fn qualified_priced_candidates<'a>(
    candidates: &'a [RouteCandidate],
    req: &RouteRequest,
    cache: &[CacheState],
    health: &LiveHealth,
) -> Result<Vec<QualifiedCandidate<'a>>, QualificationFailure> {
    qualified_priced_candidates_at(candidates, req, cache, health, unix_now_ms())
}

/// [`qualified_priced_candidates`] at an explicit wall clock: the
/// deterministic entry point tests and replay use. Expiry is derived from
/// this instant (see [`RouteCandidate::effective_pricing`]).
pub fn qualified_priced_candidates_at<'a>(
    candidates: &'a [RouteCandidate],
    req: &RouteRequest,
    cache: &[CacheState],
    health: &LiveHealth,
    now_ms: u64,
) -> Result<Vec<QualifiedCandidate<'a>>, QualificationFailure> {
    let refs: Vec<&'a RouteCandidate> = candidates.iter().collect();
    qualify_priced_refs(&refs, req, cache, health, now_ms, &EMPTY_OUTCOMES, true)
}

/// [`qualified_priced_candidates_at`] with the durable verified-outcome
/// registry consulted for the quality axis: measured outcomes supersede
/// each candidate's declared quality statement. This is the production
/// entry point every [`RouterService`] route uses.
pub fn qualified_priced_candidates_authoritative_at<'a>(
    candidates: &'a [RouteCandidate],
    req: &RouteRequest,
    cache: &[CacheState],
    health: &LiveHealth,
    now_ms: u64,
    outcomes: &dyn outcomes::OutcomeView,
) -> Result<Vec<QualifiedCandidate<'a>>, QualificationFailure> {
    let refs: Vec<&'a RouteCandidate> = candidates.iter().collect();
    qualify_priced_refs(&refs, req, cache, health, now_ms, outcomes, true)
}

#[allow(clippy::too_many_arguments)] // one explicit axis per parameter, mirroring the callers
fn qualify_priced_refs<'a>(
    candidates: &[&'a RouteCandidate],
    req: &RouteRequest,
    cache: &[CacheState],
    health: &LiveHealth,
    now_ms: u64,
    outcomes: &dyn outcomes::OutcomeView,
    // The adaptive target preference is applied at the END of this pass
    // (`true`), after every hard axis filtered the set. The candidate-sized
    // route calls with `false` and re-applies the preference after its
    // MEASURED-fit re-check, so a target candidate eliminated by its real
    // footprint still relaxes the selection into `[minimum, target)`.
    prefer_target: bool,
) -> Result<Vec<QualifiedCandidate<'a>>, QualificationFailure> {
    let (floor, _target) = quality_band(req.quality_floor, req.quality_target);
    let mut fit_survivors = 0usize;
    let mut quality_survivors = 0usize;
    let mut supported: Vec<String> = Vec::new();
    let mut qualified: Vec<QualifiedCandidate<'a>> = Vec::new();
    for c in candidates {
        let d = &c.descriptor;
        // Axis 1: live rate-limit/cooldown state (static + telemetry).
        if d.performance().rate_limit_state == RateLimitState::Hard
            || health.cooldown_active(&d.provider)
        {
            continue;
        }
        for cap in &req.required_capabilities {
            if !supported.iter().any(|s| s == cap) && d.capability_ok(std::slice::from_ref(cap)) {
                supported.push(cap.clone());
            }
        }
        // Axis 2: capabilities (fail closed).
        if !d.capability_ok(&req.required_capabilities) {
            continue;
        }
        // Axis 3: context/output fit.
        if d.context < req.context_tokens || d.max_output < req.estimated_output_tokens {
            continue;
        }
        fit_survivors += 1;
        // Axis 4: the authority-aware phase quality floor (quality-authority
        // audit): a ConservativeUnknown placeholder clears NOTHING above 0
        // (its numeric 50 is not a measured 50), a user-declared or
        // built-in prior clears by its value, and durable measured outcomes
        // supersede both. The floor VALUES themselves are unchanged
        // (implement/review/debug keep hard 60).
        let quality = effective_quality_statement(
            c,
            req.phase,
            req.task_class,
            req.risk_bucket,
            now_ms,
            outcomes,
        );
        if !quality.clears_floor(req.phase, floor) {
            continue;
        }
        quality_survivors += 1;
        // Axis 5: the request budget, over the EFFECTIVE cost estimate at
        // `now_ms`. An expired exact quote is not exact anymore: under a
        // positive budget it is Unknown (or a documented conservative
        // ceiling) and Unknown fails closed; without a cap it is admitted
        // as Unknown and settles as such.
        let cost = c.cost_estimate_at(req, cache, now_ms);
        if req.task_budget_remaining_micro > 0 {
            match cost.numeric() {
                Some(n) if n <= req.task_budget_remaining_micro => {}
                Some(_) => continue,
                None => continue,
            }
        }
        // Axis 6: latency preference (hard filter when set).
        if let Some(lp) = req.latency_preference_ms {
            if d.performance().estimated_latency_ms > lp {
                continue;
            }
        }
        qualified.push(QualifiedCandidate {
            descriptor: d,
            call_cost_micro: cost.numeric().unwrap_or(u64::MAX),
            success_ppm: health.success_ppm(&d.provider, &d.model, req.phase),
            cost,
        });
    }
    if qualified.is_empty() {
        let unsupported_capabilities = req
            .required_capabilities
            .iter()
            .filter(|c| !supported.iter().any(|s| s == *c))
            .cloned()
            .collect();
        return Err(QualificationFailure {
            phase: req.phase,
            quality_floor: floor,
            unsupported_capabilities,
            fit_survivors,
            quality_survivors,
        });
    }
    if prefer_target {
        // Adaptive target preference (adaptive-requirement audit): only the
        // target-clearing survivors remain while ANY of them survived every
        // hard axis; `[floor, target)` serves exactly when none did.
        prefer_quality_target(&mut qualified, req, |d, target| {
            candidates
                .iter()
                .find(|c| std::ptr::eq(&c.descriptor, d))
                .is_some_and(|c| {
                    effective_quality_statement(
                        c,
                        req.phase,
                        req.task_class,
                        req.risk_bucket,
                        now_ms,
                        outcomes,
                    )
                    .clears_floor(req.phase, target)
                })
        });
    }
    Ok(qualified)
}

/// PINNED qualification (audit item: pinned routing never competes): check
/// exactly ONE (provider, model) candidate against every hard axis and
/// return it, or a staged [`QualificationFailure`]. No other candidate is
/// evaluated, scored or allowed to win — "another model was cheaper" can
/// never reject a pin that clears the request's own hard caps.
///
/// The failure staging matches [`qualified_priced_candidates`] so the
/// pinned denial maps to the same typed failures (a pin missing tools is a
/// capability failure, an unknown pin under a hard cap is a budget
/// failure).
pub fn qualify_specific<'a>(
    candidates: &'a [RouteCandidate],
    provider: &str,
    model: &str,
    req: &RouteRequest,
    cache: &[CacheState],
    health: &LiveHealth,
) -> Result<QualifiedCandidate<'a>, QualificationFailure> {
    qualify_specific_at(
        candidates,
        provider,
        model,
        req,
        cache,
        health,
        unix_now_ms(),
    )
}

/// [`qualify_specific`] at an explicit wall clock.
pub fn qualify_specific_at<'a>(
    candidates: &'a [RouteCandidate],
    provider: &str,
    model: &str,
    req: &RouteRequest,
    cache: &[CacheState],
    health: &LiveHealth,
    now_ms: u64,
) -> Result<QualifiedCandidate<'a>, QualificationFailure> {
    qualify_specific_authoritative_at(
        candidates,
        provider,
        model,
        req,
        cache,
        health,
        now_ms,
        &EMPTY_OUTCOMES,
    )
}

/// [`qualify_specific_at`] with the durable verified-outcome registry
/// consulted for the pinned candidate's quality axis (measured outcomes
/// supersede the pin's declared statement).
#[allow(clippy::too_many_arguments)] // one explicit axis per parameter, mirroring the callers
pub fn qualify_specific_authoritative_at<'a>(
    candidates: &'a [RouteCandidate],
    provider: &str,
    model: &str,
    req: &RouteRequest,
    cache: &[CacheState],
    health: &LiveHealth,
    now_ms: u64,
    outcomes: &dyn outcomes::OutcomeView,
) -> Result<QualifiedCandidate<'a>, QualificationFailure> {
    let refs: Vec<&'a RouteCandidate> = candidates
        .iter()
        .filter(|c| c.descriptor.provider == provider && c.descriptor.model == model)
        .collect();
    let mut qualified = qualify_priced_refs(&refs, req, cache, health, now_ms, outcomes, true)?;
    Ok(qualified.remove(0))
}

// ------------------------------------------- candidate-specific fit sizing

/// Hard bound on how many seriously-considered candidates one route may size
/// with their REAL tokenizer (audit: candidate-specific token accounting).
/// Only the top of the already qualified/ranked order is ever measured, so
/// the tokenizer work is bounded no matter how large the catalog is.
pub const MAX_SIZED_CANDIDATES: usize = 8;

/// One ranked candidate after the candidate-specific fit re-check: its REAL
/// input footprint (the rendered request counted under ITS tokenizer, or a
/// conservative upper bound) and whether that count was exact.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SizedCandidate<'a> {
    pub candidate: &'a ModelDescriptor,
    /// The rendered request's real input footprint under this candidate's
    /// tokenizer (honest upper bound when no local vocabulary exists).
    pub input_tokens: u64,
    /// True only when a real local tokenizer counted every run.
    pub exact: bool,
}

/// THE candidate-specific fit pass: before the final selection, size each of
/// the top `max_sized` (hard-capped to [`MAX_SIZED_CANDIDATES`]) candidates
/// in the caller's already-ranked, already-CHEAPLY-qualified order with the
/// caller's candidate sizer — which must count the rendered request under
/// that candidate's own tokenizer (or return a conservative upper bound).
/// A candidate whose REAL footprint exceeds its own context window is
/// filtered out; the survivors keep the ranked order and carry their
/// measured footprints.
///
/// Bounded work: the sizer runs at most `min(max_sized, 8)` times per route
/// (each sized candidate may warm the shared token cache, never iterate the
/// catalog). Candidates beyond the top-K are never sized and therefore never
/// returned — callers must treat an empty survivor set as "no candidate
/// cleared the real fit", never fall back to an unsized pick.
pub fn size_candidates_top_k<'a>(
    ranked: &[QualifiedCandidate<'a>],
    max_sized: usize,
    mut size: impl FnMut(&ModelDescriptor) -> (u64, bool),
) -> Vec<SizedCandidate<'a>> {
    let take = max_sized.min(MAX_SIZED_CANDIDATES).min(ranked.len());
    let mut survivors = Vec::with_capacity(take);
    for q in &ranked[..take] {
        let descriptor = q.descriptor;
        let (input_tokens, exact) = size(descriptor);
        if input_tokens <= descriptor.context {
            survivors.push(SizedCandidate {
                candidate: descriptor,
                input_tokens,
                exact,
            });
        }
    }
    survivors
}

/// The measured input footprint of one candidate's rendered request under
/// that candidate's own tokenizer (or an honest upper bound when the family
/// has no local vocabulary) — the value the router's fit and cost re-checks
/// consume after candidate-specific sizing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CandidateFootprint {
    pub input_tokens: u64,
    pub exact: bool,
}

/// A candidate's request, ALREADY BUILT and measured by the injected
/// [`CandidatePlanner`]: `wire_plan` is opaque to the router (the router
/// stays render-free); `footprint` is what the candidate-specific fit pass
/// consumes. The payload travels back inside the winning [`CandidatePlan`],
/// so the provider call reuses it and the winner is never built twice.
pub struct BuiltCandidatePlan {
    pub wire_plan: Box<dyn std::any::Any + Send + Sync>,
    pub footprint: CandidateFootprint,
}

/// One candidate plan the router sized and costed over its MEASURED
/// footprint. `call_cost` is the effective per-million quote re-evaluated
/// against the measured input — not the cheap pre-rank projection.
pub struct CandidatePlan {
    pub candidate: ModelDescriptor,
    pub wire_plan: Box<dyn std::any::Any + Send + Sync>,
    pub footprint: CandidateFootprint,
    pub call_cost: CostEstimate,
}

/// The router's decision plus the winning candidate's plan when an injected
/// planner sized the route. The unsized compatibility paths return `None`
/// and the caller keeps its own pre-built plan.
pub struct SizedRouteDecision {
    pub decision: RouteDecision,
    pub plan: Option<CandidatePlan>,
}

impl SizedRouteDecision {
    /// The unsized decision (no candidate plan was built on this path).
    pub fn without_plan(decision: RouteDecision) -> Self {
        Self {
            decision,
            plan: None,
        }
    }
}

/// The wire-plan builder seam (candidate-specific accounting audit): the
/// agent/context side injects an implementation that renders and measures
/// the logical request under each candidate's OWN tokenizer. The router
/// owns selection and never renders; this trait is the ONLY crossing.
pub trait CandidatePlanner {
    /// Build and measure the request for `descriptor`. `None` = the request
    /// cannot be rendered for this candidate (e.g. required content alone
    /// exceeds the budget) and the candidate is dropped from the sized set.
    fn build(&self, descriptor: &ModelDescriptor) -> Option<BuiltCandidatePlan>;
}

// ---------------------------------------------------------------- scoring

/// One fully qualified, fully scored candidate.
///
/// All money fields are microUSD; all time fields are milliseconds. The
/// ranking ladder in [`ScoredCandidate::compare`] is explicit and ordered —
/// no tuple-slot overloading and never a comparison between a microUSD
/// figure and a millisecond figure (audit P0-4: the old tie-break compared
/// a base cost against the previous candidate's latency).
#[derive(Debug, Clone, Copy)]
pub struct ScoredCandidate<'a> {
    pub candidate: &'a ModelDescriptor,
    /// Expected cost-to-success in microUSD: base call cost plus the
    /// probabilistic retry and escalation terms (integer, rounded up), or —
    /// when per-phase verified-outcome history exists — the conservative
    /// [`WorkCostEstimate::total_expected_micro`] (immediate + P(rework) x
    /// downstream spend to VERIFIED completion).
    pub expected_cost_micro: u64,
    /// Estimated latency of one call in milliseconds.
    pub expected_latency_ms: u64,
    /// Success prior in ppm (1_000_000 = certain): the telemetry-blended
    /// prior when no verified history exists; otherwise the conservative
    /// one-sided lower-bound verified-success confidence (the prior
    /// MaximumQuality semantics consume — a two-sample track record stays
    /// far below the documented "excellent" bar).
    pub success_ppm: u32,
    /// Base per-call cost in microUSD (cache-aware).
    pub call_cost_micro: u64,
    /// The conservative work-cost estimate when verified history exists for
    /// this candidate's (provider, model, phase); `None` = legacy
    /// telemetry-prior scoring was used.
    pub work_estimate: Option<WorkCostEstimate>,
}

impl<'a> ScoredCandidate<'a> {
    /// The documented total order over scored candidates. Returns
    /// `Ordering::Less` when `self` ranks ahead of `other`. Ladder:
    /// 1. `expected_cost_micro` (microUSD cost-to-success) ascending,
    /// 2. `success_ppm` (success prior, ppm) descending,
    /// 3. `expected_latency_ms` (milliseconds) ascending,
    /// 4. `call_cost_micro` (microUSD) ascending,
    /// 5. `(provider, model)` lexicographic ascending — the deterministic
    ///    final key (identical candidates resolve identically every run).
    pub fn compare(&self, other: &Self) -> std::cmp::Ordering {
        self.expected_cost_micro
            .cmp(&other.expected_cost_micro)
            .then_with(|| other.success_ppm.cmp(&self.success_ppm))
            .then_with(|| self.expected_latency_ms.cmp(&other.expected_latency_ms))
            .then_with(|| self.call_cost_micro.cmp(&other.call_cost_micro))
            .then_with(|| {
                (&self.candidate.provider, &self.candidate.model)
                    .cmp(&(&other.candidate.provider, &other.candidate.model))
            })
    }
}

/// Expected cost-to-success of one qualified candidate (audit-9 integer
/// math): base + P(fail)/2 * base (retry) + P(fail)/2 * escalation, with
/// P(fail) = 1 - success_ppm/1e6. Each probabilistic term rounds the exact
/// product UP to the next integer micro, so the estimate never understates.
/// `escalation_cost_micro` is the base cost of the best OTHER qualified
/// candidate (or 3x the candidate's own base when none exists).
fn expected_cost_to_success(
    success_ppm: u32,
    call_cost_micro: u64,
    escalation_cost_micro: u128,
) -> u64 {
    let base = u128::from(call_cost_micro);
    let p_fail_ppm = u128::from(1_000_000u64 - u64::from(success_ppm.min(1_000_000)));
    // ceil(x * p_fail_ppm / 2_000_000) == (x * p_fail_ppm + 1_999_999) / 2_000_000.
    let retry = (base.saturating_mul(p_fail_ppm).saturating_add(1_999_999)) / 2_000_000;
    let escalate = (escalation_cost_micro
        .saturating_mul(p_fail_ppm)
        .saturating_add(1_999_999))
        / 2_000_000;
    let total = base + retry + escalate;
    u64::try_from(total).unwrap_or(u64::MAX)
}

/// The two cheapest DISTINCT (provider, model) qualified candidates,
/// ordered deterministically by `(cost, provider, model)`. Every candidate
/// escalates to the cheapest candidate that is not itself — the top two
/// suffice for that lookup.
#[derive(Debug, Clone, Copy)]
struct CheapestTwo<'a> {
    cheapest: Option<(u64, &'a str, &'a str)>,
    second: Option<(u64, &'a str, &'a str)>,
}

impl<'a> CheapestTwo<'a> {
    /// Base cost of the best OTHER candidate, or `fallback` (3x the
    /// candidate's own base) when it is the only qualified candidate.
    fn escalation_for(&self, provider: &str, model: &str, fallback: u128) -> u128 {
        match self.cheapest {
            None => fallback,
            Some((cost, p, m)) => {
                if p == provider && m == model {
                    self.second
                        .map(|(c, _, _)| u128::from(c))
                        .unwrap_or(fallback)
                } else {
                    u128::from(cost)
                }
            }
        }
    }
}

fn two_cheapest_distinct<'a>(qualified: &[QualifiedCandidate<'a>]) -> CheapestTwo<'a> {
    let mut first: Option<(u64, &'a str, &'a str)> = None;
    let mut second: Option<(u64, &'a str, &'a str)> = None;
    for q in qualified {
        let d = q.descriptor;
        let entry = (q.call_cost_micro, d.provider.as_str(), d.model.as_str());
        let same_key_as =
            |current: &(u64, &str, &str)| current.1 == entry.1 && current.2 == entry.2;
        let overtakes = |current: &(u64, &str, &str)| {
            entry.0 < current.0
                || (entry.0 == current.0 && (entry.1, entry.2) < (current.1, current.2))
        };
        let take_first = match &first {
            None => true,
            Some(current) => overtakes(current),
        };
        if take_first {
            // The displaced minimum is the new second-best — but never when
            // it is the SAME (provider, model) as the new leader (that
            // would let a candidate escalate to itself).
            if let Some(old) = first {
                if !same_key_as(&old) {
                    second = Some(old);
                }
            }
            first = Some(entry);
        } else {
            let same_key = first.as_ref().map(same_key_as).unwrap_or(false);
            if !same_key {
                let take_second = match &second {
                    None => true,
                    Some(current) => overtakes(current),
                };
                if take_second {
                    second = Some(entry);
                }
            }
        }
    }
    CheapestTwo {
        cheapest: first,
        second,
    }
}

/// Score every qualified candidate: expected cost-to-success with the
/// escalation pool restricted to OTHER QUALIFIED candidates — escalation
/// can never target a model that could not serve the request itself.
///
/// Verified-outcome consult (audit items 13/14/L): when `outcomes` holds
/// per-phase history for a candidate, the legacy telemetry-prior terms are
/// REPLACED by the conservative [`WorkCostEstimate`] — expected cost to
/// VERIFIED completion = immediate cost + P(rework) x downstream spend. The
/// unmeasured rework spend fallback is the candidate's escalation cost (a
/// rework costs at least one full escalation call); measured rework spend
/// from the history dominates it once any failure with spend exists. The
/// scored success prior becomes the conservative verified-success
/// confidence (lower Wilson bound). An empty registry misses every consult,
/// so scoring is byte-identical to the pre-outcome math.
fn score_candidates<'a>(
    qualified: &[QualifiedCandidate<'a>],
    phase: RouterPhase,
    outcomes: &dyn outcomes::OutcomeView,
) -> Vec<ScoredCandidate<'a>> {
    let two = two_cheapest_distinct(qualified);
    qualified
        .iter()
        .map(|q| {
            let d = q.descriptor;
            let escalation = two.escalation_for(
                &d.provider,
                &d.model,
                u128::from(q.call_cost_micro).saturating_mul(3),
            );
            let escalation_micro = u64::try_from(escalation).unwrap_or(u64::MAX);
            let stats = outcomes.phase_stats(&d.provider, &d.model, phase);
            match stats {
                Some(st) => {
                    let estimate =
                        work_cost_estimate(q.call_cost_micro, Some(&st), escalation_micro);
                    ScoredCandidate {
                        candidate: d,
                        expected_cost_micro: estimate.total_expected_micro,
                        expected_latency_ms: d.performance().estimated_latency_ms,
                        success_ppm: verified_success_confidence_ppm(&st),
                        call_cost_micro: q.call_cost_micro,
                        work_estimate: Some(estimate),
                    }
                }
                None => ScoredCandidate {
                    candidate: d,
                    expected_cost_micro: expected_cost_to_success(
                        q.success_ppm,
                        q.call_cost_micro,
                        escalation,
                    ),
                    expected_latency_ms: d.performance().estimated_latency_ms,
                    success_ppm: q.success_ppm,
                    call_cost_micro: q.call_cost_micro,
                    work_estimate: None,
                },
            }
        })
        .collect()
}

/// One fully qualified PRICED candidate, scored for selection. The money
/// field is a [`CostEstimate`]-derived option: `expected_cost_micro = None`
/// means the candidate's cost is Unknown (nonnumeric); numeric candidates
/// always rank ahead of unknown ones at the same tier, so "we cannot price
/// it" is never silently read as "free".
#[derive(Debug, Clone, Copy)]
pub struct PricedScoredCandidate<'a> {
    pub candidate: &'a RouteCandidate,
    /// Expected cost-to-success in microUSD, or `None` for Unknown pricing
    /// (no honest number exists).
    pub expected_cost_micro: Option<u64>,
    pub expected_latency_ms: u64,
    pub success_ppm: u32,
    /// The candidate's exact single-call cost estimate.
    pub call_cost: CostEstimate,
    /// The conservative verified work-cost estimate when history exists.
    pub work_estimate: Option<WorkCostEstimate>,
}

impl PricedScoredCandidate<'_> {
    /// Explicit total order: numeric expected cost ascending first,
    /// Unknown cost last; then success descending, latency ascending,
    /// numeric call cost ascending (LocalZero cheapest), Unknown last, and
    /// finally the deterministic (provider, model) key.
    pub fn compare(&self, other: &Self) -> std::cmp::Ordering {
        cmp_optional_cost(self.expected_cost_micro, other.expected_cost_micro)
            .then_with(|| other.success_ppm.cmp(&self.success_ppm))
            .then_with(|| self.expected_latency_ms.cmp(&other.expected_latency_ms))
            .then_with(|| cmp_optional_cost(self.call_cost.numeric(), other.call_cost.numeric()))
            .then_with(|| {
                (
                    &self.candidate.descriptor.provider,
                    &self.candidate.descriptor.model,
                )
                    .cmp(&(
                        &other.candidate.descriptor.provider,
                        &other.candidate.descriptor.model,
                    ))
            })
    }
}

/// Ascending numeric order with `None` (Unknown) always AFTER every number
/// — never treated as zero.
fn cmp_optional_cost(a: Option<u64>, b: Option<u64>) -> std::cmp::Ordering {
    match (a, b) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

/// Score every qualified PRICED candidate. Escalation targets are the
/// cheapest OTHER numeric qualified candidates (an unknown cost can never
/// seed a numeric escalation); an Unknown candidate itself keeps
/// `expected_cost_micro = None`. Verified history is consulted through the
/// [`OutcomeStore::lookup_stats`] hierarchy for this request's task class
/// and risk bucket, so an exact/class-level record dominates the phase
/// fold and the global prior.
pub fn score_priced_candidates<'a>(
    qualified: &[QualifiedCandidate<'a>],
    candidates: &'a [RouteCandidate],
    req: &RouteRequest,
    outcomes: &dyn outcomes::OutcomeView,
) -> Vec<PricedScoredCandidate<'a>> {
    // Numeric base costs of every qualified candidate, cheapest first —
    // the escalation pool (unknown costs never enter it).
    let mut numeric: Vec<(u64, &str, &str)> = qualified
        .iter()
        .filter_map(|q| {
            q.cost.numeric().map(|n| {
                (
                    n,
                    q.descriptor.provider.as_str(),
                    q.descriptor.model.as_str(),
                )
            })
        })
        .collect();
    numeric.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| (a.1, a.2).cmp(&(b.1, b.2))));
    numeric.dedup_by(|a, b| a.1 == b.1 && a.2 == b.2);
    let escalation_for = |provider: &str, model: &str, own: u64| -> u64 {
        numeric
            .iter()
            .find(|(_, p, m)| *p != provider || *m != model)
            .map(|(n, _, _)| *n)
            .unwrap_or_else(|| own.saturating_mul(3))
    };

    qualified
        .iter()
        .filter_map(|q| {
            let c = candidates
                .iter()
                .find(|c| std::ptr::eq(&c.descriptor, q.descriptor))?;
            let d = &c.descriptor;
            let stats = outcomes.lookup_stats(
                &d.provider,
                &d.model,
                req.phase,
                req.task_class,
                req.risk_bucket,
            );
            let (expected_cost_micro, success_ppm, work_estimate) = match q.cost.numeric() {
                Some(immediate) => {
                    let escalation = escalation_for(&d.provider, &d.model, immediate);
                    match stats {
                        Some(st) => {
                            let estimate = work_cost_estimate(immediate, Some(&st), escalation);
                            (
                                Some(estimate.total_expected_micro),
                                verified_success_confidence_ppm(&st),
                                Some(estimate),
                            )
                        }
                        None => (
                            Some(expected_cost_to_success(
                                q.success_ppm,
                                immediate,
                                u128::from(escalation),
                            )),
                            q.success_ppm,
                            None,
                        ),
                    }
                }
                // Unknown pricing: no honest expected-cost number; the
                // ladder ranks it after every numeric candidate.
                None => (None, q.success_ppm, None),
            };
            Some(PricedScoredCandidate {
                candidate: c,
                expected_cost_micro,
                expected_latency_ms: d.performance().estimated_latency_ms,
                success_ppm,
                call_cost: q.cost,
                work_estimate,
            })
        })
        .collect()
}

pub struct Router {
    pub candidates: Vec<ModelDescriptor>,
}

impl Router {
    pub fn new(candidates: Vec<ModelDescriptor>) -> Self {
        Self { candidates }
    }

    /// Plain cheapest-above-floor routing. Uses the SAME single
    /// qualification pass as every other path ([`qualified_candidates`])
    /// with the default (empty) live-health view: static
    /// [`RateLimitState::Hard`] models are excluded, live telemetry
    /// cooldowns do not exist here — they arrive through the service.
    pub fn route(&self, req: &RouteRequest, cache: &[CacheState]) -> Result<RouteDecision, String> {
        let qualified = qualified_candidates(&self.candidates, req, cache, &LiveHealth::default())
            .map_err(|f| f.route_error())?;
        // Cheapest above the floor wins; ties by latency, then candidate
        // order (deterministic). Both units are explicit: microUSD cost,
        // then milliseconds latency — never a cross-unit comparison.
        let mut best: Option<(&ModelDescriptor, u64, u64)> = None;
        for q in &qualified {
            let d = q.descriptor;
            let cost = q.call_cost_micro;
            let latency = d.performance().estimated_latency_ms;
            let better = match best {
                None => true,
                Some((_, bc, bl)) => cost < bc || (cost == bc && latency < bl),
            };
            if better {
                best = Some((d, cost, latency));
            }
        }
        let (chosen, cost, latency) = best.expect("qualified_candidates is non-empty on Ok");
        let phase_tag = serde_json::to_string(&req.phase)
            .unwrap_or_default()
            .trim_matches('"')
            .to_string();
        let quality = chosen.performance().coding_reliability;
        let floor = req.quality_floor.min(100);
        let reasoning = format!(
            "phase={phase_tag} considered={} qualified={} chosen={}/{} cost_micro={cost} latency_ms={latency} quality={quality} floor={floor}",
            self.candidates.len(),
            qualified.len(),
            chosen.provider,
            chosen.model,
        );
        Ok(RouteDecision {
            provider: chosen.provider.clone(),
            model: chosen.model.clone(),
            estimated_cost_micro: cost,
            estimated_latency_ms: latency,
            reasoning,
            considered: self.candidates.len(),
            source: chosen.source,
            // Wave-B item B: this descriptor-only path consults NO pricing
            // authority (a ModelDescriptor carries a lossy per-token
            // estimate, never a quote + authority), so the decision says so
            // — `None` — instead of inferring a snapshot from numbers. The
            // catalog-aware RouterService path stamps the entry's real
            // snapshot (see `RouterService::with_pricing`).
            pricing_snapshot: None,
        })
    }
}

// ---------------------------------------------------------------- service

/// Per (provider, model, phase) exponentially weighted observations with
/// prior blending: new models start near sane priors and a small sample can
/// never wreck a reputation.
pub struct RouterTelemetry {
    inner: std::sync::Mutex<std::collections::HashMap<(String, String, RouterPhase), Ewma>>,
    cooldown: std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
}

const PRIOR_SUCCESS: f64 = 0.8;
const ALPHA: f64 = 0.1;

#[derive(Clone, Copy)]
struct Ewma {
    success: f64,
    retry: f64,
    rate_limit: f64,
    latency_ms: f64,
}

impl Default for Ewma {
    fn default() -> Self {
        Self {
            success: PRIOR_SUCCESS,
            retry: 0.1,
            rate_limit: 0.0,
            latency_ms: 0.0,
        }
    }
}

impl Ewma {
    fn update(&mut self, obs: bool) {
        let target = if obs { 1.0 } else { 0.0 };
        self.success = ALPHA * target + (1.0 - ALPHA) * self.success;
    }
}

impl RouterTelemetry {
    pub fn new() -> Self {
        Self {
            inner: Default::default(),
            cooldown: Default::default(),
        }
    }

    pub fn record(
        &self,
        provider: &str,
        model: &str,
        phase: RouterPhase,
        success: bool,
        retried: bool,
        rate_limited: bool,
    ) {
        self.record_outcome(provider, model, phase, success, retried, rate_limited, 0);
    }

    /// The outcome record entry (P0-28 residuals): one settled model call
    /// with its measured latency. Same (provider, model, phase) EWMA
    /// reliability update as [`RouterTelemetry::record`] — latency rides
    /// the same observation so a caller that only ever saw `record` keeps
    /// identical priors.
    #[allow(clippy::too_many_arguments)]
    pub fn record_outcome(
        &self,
        provider: &str,
        model: &str,
        phase: RouterPhase,
        success: bool,
        retried: bool,
        rate_limited: bool,
        latency_ms: u64,
    ) {
        let mut m = recover_lock(&self.inner);
        let e = m.entry((provider.into(), model.into(), phase)).or_default();
        e.update(success);
        let t = if retried { 1.0 } else { 0.0 };
        e.retry = ALPHA * t + (1.0 - ALPHA) * e.retry;
        if rate_limited {
            e.rate_limit = ALPHA + (1.0 - ALPHA) * e.rate_limit;
        } else {
            e.rate_limit *= 1.0 - ALPHA;
        }
        if latency_ms > 0 {
            e.latency_ms = ALPHA * latency_ms as f64 + (1.0 - ALPHA) * e.latency_ms;
        }
    }

    /// Rate-limit cooldown: providers stay Hard-excluded until `secs` pass.
    pub fn record_rate_limit(&self, provider: &str, secs: u64) {
        recover_lock(&self.cooldown).insert(
            provider.to_string(),
            std::time::Instant::now() + std::time::Duration::from_secs(secs),
        );
    }

    pub fn cooldown_active(&self, provider: &str) -> bool {
        recover_lock(&self.cooldown)
            .get(provider)
            .map(|t| *t > std::time::Instant::now())
            .unwrap_or(false)
    }

    /// Blended success prior for (provider, model, phase).
    pub fn success_estimate(&self, provider: &str, model: &str, phase: RouterPhase) -> f64 {
        let m = recover_lock(&self.inner);
        match m.get(&(provider.into(), model.into(), phase)) {
            Some(e) => {
                // Prior blend: pull toward PRIOR_SUCCESS as n is small.
                // n is implicit via variance; use a fixed light blend.
                0.7 * e.success + 0.3 * PRIOR_SUCCESS
            }
            None => PRIOR_SUCCESS,
        }
    }

    /// EWMA of the recorded settled-call latencies (0.0 when nothing was
    /// recorded through [`RouterTelemetry::record_outcome`] yet).
    pub fn avg_latency_ms(&self, provider: &str, model: &str, phase: RouterPhase) -> f64 {
        let m = recover_lock(&self.inner);
        m.get(&(provider.into(), model.into(), phase))
            .map(|e| e.latency_ms)
            .unwrap_or(0.0)
    }

    /// Point-in-time live-health snapshot for one routing decision
    /// (audit P0-89: the qualification pass reads REAL rate-limit state,
    /// never an inert always-false helper). Cooldown expiry is evaluated
    /// once, here, so a single decision is deterministic.
    pub fn snapshot(&self) -> LiveHealth {
        let now = std::time::Instant::now();
        let cooldown = recover_lock(&self.cooldown)
            .iter()
            .filter(|(_, until)| **until > now)
            .map(|(p, _)| p.clone())
            .collect();
        let inner = recover_lock(&self.inner);
        let success_ppm = inner
            .iter()
            .map(|((p, m, phase), e)| {
                let blend = 0.7 * e.success + 0.3 * PRIOR_SUCCESS;
                let ppm = (blend * 1_000_000.0).round().clamp(0.0, 1_000_000.0) as u32;
                ((p.clone(), m.clone(), *phase), ppm)
            })
            .collect();
        LiveHealth {
            cooldown,
            success_ppm,
        }
    }
}

impl Default for RouterTelemetry {
    fn default() -> Self {
        Self::new()
    }
}

/// Production router: expected-cost-to-verified-success selection with
/// telemetry priors, rate-limit cooldowns and full audit strings.
///
/// `pricing` maps (provider, model) to the route-time [`PricingSnapshot`]
/// the routing graph cut from each candidate's REAL catalog row (wave-B
/// item B: authority + exact per-million quote, never inferred from the
/// descriptor's lossy estimate). Decisions over candidates absent from the
/// map carry `pricing_snapshot: None` — no pricing authority was
/// consulted — which settlement treats as unpriced (fail closed under a
/// hard cap, documented Unknown spend without one).
pub struct RouterService {
    pub router: Router,
    pub telemetry: RouterTelemetry,
    /// Catalog-cut route-time snapshots keyed (provider, model); the legacy
    /// compatibility path ([`RouterService::with_pricing`]). The PRICED
    /// path ([`RouterService::with_route_candidates`]) carries each
    /// candidate's [`PricingState`] directly instead.
    pub pricing: HashMap<(String, String), PricingSnapshot>,
    /// THE priced candidate set (pricing-path audit): descriptors paired
    /// with their catalog-resolved pricing state. When non-empty, every
    /// route runs the priced qualification/scoring path and no per-token
    /// projection is ever consulted.
    pub priced: Vec<RouteCandidate>,
    /// The pinned (provider, model) of a Pinned-mode service: `route()`
    /// then calls ONLY [`RouterService::qualify_specific`] — the pinned
    /// candidate never competes and can never be rejected because another
    /// model won.
    pub pinned: Option<(String, String)>,
    /// Verified-outcome registry (audit items 13/14/L): per-phase verified
    /// history the scoring consult reads when it exists. Every constructor
    /// defaults to an [`EmptyOutcomeStore`], so a service built without
    /// outcomes is byte-identical to the pre-outcome router; wiring builds
    /// the service through [`RouterService::with_outcomes`] /
    /// [`RouterService::with_pricing_and_outcomes`].
    pub outcomes: Arc<dyn OutcomeStore>,
}

impl RouterService {
    pub fn new(candidates: Vec<ModelDescriptor>) -> Self {
        Self::build(candidates, HashMap::new(), Arc::new(EmptyOutcomeStore))
    }

    /// Build the service over candidates AND the catalog pricing authority
    /// behind them: the routing graph passes the snapshot each candidate's
    /// catalog entry cut (exact quote / ceiling / authoritative local zero
    /// / explicit Unknown), so every decision freezes the real price
    /// lines at route time — settlement prices usage against this, never
    /// against later catalog repricing.
    pub fn with_pricing(
        candidates: Vec<ModelDescriptor>,
        pricing: HashMap<(String, String), PricingSnapshot>,
    ) -> Self {
        Self::build(candidates, pricing, Arc::new(EmptyOutcomeStore))
    }

    /// Build the service over candidates AND a verified-outcome registry
    /// (no catalog pricing map; decisions carry `pricing_snapshot: None`).
    /// Scoring consults the registry's per-phase verified stats when they
    /// exist ([`WorkCostEstimate`]); an empty registry keeps every decision
    /// byte-identical to [`RouterService::new`].
    pub fn with_outcomes(
        candidates: Vec<ModelDescriptor>,
        outcomes: Arc<dyn OutcomeStore>,
    ) -> Self {
        Self::build(candidates, HashMap::new(), outcomes)
    }

    /// The full wiring constructor: catalog pricing authority AND the
    /// verified-outcome registry in one additive build step.
    pub fn with_pricing_and_outcomes(
        candidates: Vec<ModelDescriptor>,
        pricing: HashMap<(String, String), PricingSnapshot>,
        outcomes: Arc<dyn OutcomeStore>,
    ) -> Self {
        Self::build(candidates, pricing, outcomes)
    }

    /// THE production constructor (pricing-path audit): the daemon graph
    /// passes [`RouteCandidate`]s (descriptor + catalog-resolved
    /// [`PricingState`]) and every route prices calls through the exact
    /// per-million quote, never through a per-token projection.
    pub fn with_route_candidates(
        candidates: Vec<RouteCandidate>,
        outcomes: Arc<dyn OutcomeStore>,
    ) -> Self {
        let descriptors: Vec<ModelDescriptor> =
            candidates.iter().map(|c| c.descriptor.clone()).collect();
        Self {
            router: Router::new(descriptors),
            telemetry: RouterTelemetry::new(),
            pricing: HashMap::new(),
            priced: candidates,
            pinned: None,
            outcomes,
        }
    }

    /// A Pinned-mode production service: exactly the pin's
    /// [`RouteCandidate`] set plus the pin identity, so `route()` runs ONLY
    /// the pinned qualification. The caller must have resolved
    /// (provider, model) against the registered provider already (the graph
    /// does, loudly, at build).
    pub fn with_pinned_route_candidates(
        candidates: Vec<RouteCandidate>,
        provider: impl Into<String>,
        model: impl Into<String>,
        outcomes: Arc<dyn OutcomeStore>,
    ) -> Self {
        let mut service = Self::with_route_candidates(candidates, outcomes);
        service.pinned = Some((provider.into(), model.into()));
        service
    }

    fn build(
        candidates: Vec<ModelDescriptor>,
        pricing: HashMap<(String, String), PricingSnapshot>,
        outcomes: Arc<dyn OutcomeStore>,
    ) -> Self {
        Self {
            router: Router::new(candidates),
            telemetry: RouterTelemetry::new(),
            pricing,
            priced: Vec::new(),
            pinned: None,
            outcomes,
        }
    }

    /// PINNED qualification over this service's priced candidates (pricing-
    /// path audit): checks the single pinned (provider, model) against
    /// every hard axis and returns it. No competition, no score.
    pub fn qualify_specific(
        &self,
        provider: &str,
        model: &str,
        req: &RouteRequest,
        cache: &[CacheState],
    ) -> Result<QualifiedCandidate<'_>, QualificationFailure> {
        self.qualify_specific_at(provider, model, req, cache, unix_now_ms())
    }

    /// [`RouterService::qualify_specific`] at an explicit wall clock
    /// (deterministic replay/tests: quote expiry is derived from `now_ms`).
    pub fn qualify_specific_at(
        &self,
        provider: &str,
        model: &str,
        req: &RouteRequest,
        cache: &[CacheState],
        now_ms: u64,
    ) -> Result<QualifiedCandidate<'_>, QualificationFailure> {
        self.qualify_specific_at_with(provider, model, req, cache, now_ms, self.outcomes.as_ref())
    }

    /// [`RouterService::qualify_specific_at`] consulting an explicit outcome
    /// registry (a route-scoped memo for the pinned path of
    /// [`RouterService::route_at_with`]).
    fn qualify_specific_at_with(
        &self,
        provider: &str,
        model: &str,
        req: &RouteRequest,
        cache: &[CacheState],
        now_ms: u64,
        outcomes: &dyn outcomes::OutcomeView,
    ) -> Result<QualifiedCandidate<'_>, QualificationFailure> {
        let health = self.telemetry.snapshot();
        qualify_specific_authoritative_at(
            &self.priced,
            provider,
            model,
            req,
            cache,
            &health,
            now_ms,
            outcomes,
        )
    }

    /// The AUTHORITY-AWARE, measured-outcome-superseding quality statement
    /// of one priced candidate (quality-authority audit): `None` when the
    /// candidate is not in the priced set. Routing policies use this to
    /// validate a pin's quality without ever reading the placeholder
    /// number as measured.
    pub fn effective_quality(
        &self,
        provider: &str,
        model: &str,
        phase: RouterPhase,
        task_class: TaskClass,
        risk_bucket: RiskBucket,
    ) -> Option<QualityStatement> {
        self.priced
            .iter()
            .find(|c| c.descriptor.provider == provider && c.descriptor.model == model)
            .map(|c| {
                effective_quality_statement(
                    c,
                    phase,
                    task_class,
                    risk_bucket,
                    unix_now_ms(),
                    self.outcomes.as_ref(),
                )
            })
    }

    /// The AUTHORITY-AWARE effective quality statement of one candidate for
    /// this service's request axes at an explicit wall clock: exactly the
    /// statement the priced qualification pass ([`qualify_priced_refs`])
    /// applies, so a caller (MaximumQuality tier formation) can build
    /// quality tiers on the SAME authority the router qualifies with.
    /// Durable measured outcomes supersede the candidate's declared
    /// statement; absent evidence the declared statement is returned
    /// verbatim. Pair it with [`phase_quality_value`].
    pub fn effective_quality_for(
        &self,
        candidate: &RouteCandidate,
        req: &RouteRequest,
        now_ms: u64,
    ) -> QualityStatement {
        self.effective_quality_for_with(candidate, req, now_ms, self.outcomes.as_ref())
    }

    /// [`RouterService::effective_quality_for`] consulting an explicit
    /// outcome registry (the route-scoped memo shared with the probe).
    pub fn effective_quality_for_with(
        &self,
        candidate: &RouteCandidate,
        req: &RouteRequest,
        now_ms: u64,
        outcomes: &dyn outcomes::OutcomeView,
    ) -> QualityStatement {
        effective_quality_statement(
            candidate,
            req.phase,
            req.task_class,
            req.risk_bucket,
            now_ms,
            outcomes,
        )
    }

    /// The service's outcome registry (the durable verified-outcome store a
    /// route-scoped [`outcomes::MemoOutcomeStore`] wraps).
    pub fn outcome_store(&self) -> &dyn outcomes::OutcomeView {
        self.outcomes.as_ref()
    }

    /// Expected cost = base + P(retry)*base + (1-P(success))*escalation,
    /// where escalation = cost of the best OTHER QUALIFIED candidate
    /// (or base*3 when the candidate is the only qualified option) — or,
    /// for candidates whose per-phase VERIFIED-outcome history exists, the
    /// conservative [`WorkCostEstimate`]: expected cost to VERIFIED
    /// completion = immediate cost + P(rework) x downstream spend (measured
    /// from durable history; the escalation cost stands in until failures
    /// with measured spend exist). Selection picks the minimum EXPECTED
    /// cost by the documented [`ScoredCandidate::compare`] ladder; the
    /// decision's `estimated_cost_micro` stays the BASE cost so downstream
    /// budget math is conservative.
    ///
    /// Qualification is the single [`qualified_candidates`] pass — the
    /// same one the plain router uses — with the live telemetry snapshot
    /// (cooldowns + success priors) fed in. There is deliberately NO
    /// second loop over the unfiltered candidate list (audit P0-3): a
    /// model that fails any qualification axis can never be chosen, no
    /// matter how cheap its expected cost looks.
    pub fn route(&self, req: &RouteRequest, cache: &[CacheState]) -> Result<RouteDecision, String> {
        self.route_at(req, cache, unix_now_ms())
    }

    /// [`RouterService::route`] at an explicit wall clock (deterministic
    /// replay/tests): every candidate's effective price state — and thus
    /// budget admission and the frozen decision snapshot — is derived from
    /// this instant.
    pub fn route_at(
        &self,
        req: &RouteRequest,
        cache: &[CacheState],
        now_ms: u64,
    ) -> Result<RouteDecision, String> {
        self.route_at_with(req, cache, now_ms, self.outcomes.as_ref())
    }

    /// [`RouterService::route_at`] consulting an EXPLICIT outcome registry for
    /// the quality axis. A routing policy wraps the service registry in a
    /// route-scoped memo and passes it here so repeated probes
    /// (MaximumQuality's descending quality floors) reuse the candidate
    /// lookups already paid for, instead of re-reading the durable store per
    /// probe. Passing the service's own registry is byte-identical to
    /// [`RouterService::route_at`].
    pub fn route_at_with(
        &self,
        req: &RouteRequest,
        cache: &[CacheState],
        now_ms: u64,
        outcomes: &dyn outcomes::OutcomeView,
    ) -> Result<RouteDecision, String> {
        if let Some((provider, model)) = &self.pinned {
            return self.route_pinned(provider, model, req, cache, now_ms, outcomes);
        }
        if !self.priced.is_empty() {
            return self.route_priced(req, cache, now_ms, outcomes);
        }
        self.route_legacy(req, cache, outcomes)
    }

    /// The priced production route (pricing-path audit): qualification over
    /// [`RouteCandidate`]s, exact per-million cost estimates at `now_ms`,
    /// the outcome-hierarchy consult, and a decision carrying the
    /// candidate's EFFECTIVE price snapshot (an expired exact quote is
    /// downgraded to Unknown or a declared ceiling before it is frozen).
    fn route_priced(
        &self,
        req: &RouteRequest,
        cache: &[CacheState],
        now_ms: u64,
        outcomes: &dyn outcomes::OutcomeView,
    ) -> Result<RouteDecision, String> {
        let health = self.telemetry.snapshot();
        let qualified = qualified_priced_candidates_authoritative_at(
            &self.priced,
            req,
            cache,
            &health,
            now_ms,
            outcomes,
        )
        .map_err(|f| f.route_error())?;
        let scored = score_priced_candidates(&qualified, &self.priced, req, outcomes);
        let winner = scored.iter().min_by(|a, b| a.compare(b)).ok_or_else(|| {
            "no candidate clears capability/fit filtering (missing: )".to_string()
        })?;
        let chosen = winner.candidate;
        let base = winner.call_cost.numeric().unwrap_or(0);
        let ps = f64::from(winner.success_ppm) / 1_000_000.0;
        let verified_tag = match winner.work_estimate {
            Some(est) => format!(
                " verified rework_ppm={} exp_rework_micro={} total_expected_micro={}",
                est.rework_probability_ppm, est.expected_rework_micro, est.total_expected_micro
            ),
            None => String::new(),
        };
        let cost_tag = match winner.call_cost {
            CostEstimate::Unknown => " cost=unknown",
            _ => "",
        };
        let reasoning = format!(
            "phase={:?} expected-cost chosen={}/{} base_micro={base}{} p_success={ps:.2}{}",
            req.phase, chosen.descriptor.provider, chosen.descriptor.model, cost_tag, verified_tag,
        );
        let effective = chosen
            .pricing
            .effective_at(now_ms, req.task_budget_remaining_micro > 0);
        Ok(RouteDecision {
            provider: chosen.descriptor.provider.clone(),
            model: chosen.descriptor.model.clone(),
            estimated_cost_micro: base,
            estimated_latency_ms: winner.expected_latency_ms,
            reasoning,
            considered: self.priced.len(),
            source: chosen.descriptor.source,
            pricing_snapshot: Some(effective.snapshot()),
        })
    }

    /// THE candidate-sized priced route (candidate-specific accounting
    /// audit): the production flow is
    ///
    /// ```text
    /// logical request
    ///   -> cheap capability/quality/health qualification (the ONE pass)
    ///   -> pre-rank by the cheap expected-cost ladder
    ///   -> top-K (<= MAX_SIZED_CANDIDATES)
    ///   -> build/measure each candidate's request under ITS OWN tokenizer
    ///      (the injected CandidatePlanner; unsupported families are
    ///      honest upper bounds, never fake exact counts)
    ///   -> replace the candidate input with the measured footprint
    ///   -> re-price the call through the candidate's effective quote
    ///   -> drop candidates whose measured footprint overflows their OWN
    ///      context window
    ///   -> final route over the measured survivors
    ///   -> return the winner's ALREADY-BUILT plan for reuse
    /// ```
    ///
    /// The winner is built EXACTLY once: the plan the planner built during
    /// sizing is moved into the returned [`CandidatePlan`], never rebuilt.
    /// A pinned service never competes and a descriptor-only (legacy)
    /// service has no plans; both return an unsized decision and the caller
    /// keeps its own plan.
    pub fn route_with_candidate_plans(
        &self,
        req: &RouteRequest,
        cache: &[CacheState],
        planner: &dyn CandidatePlanner,
    ) -> Result<SizedRouteDecision, String> {
        self.route_with_candidate_plans_at(req, cache, planner, unix_now_ms())
    }

    /// [`RouterService::route_with_candidate_plans`] at an explicit wall
    /// clock (deterministic replay/tests).
    pub fn route_with_candidate_plans_at(
        &self,
        req: &RouteRequest,
        cache: &[CacheState],
        planner: &dyn CandidatePlanner,
        now_ms: u64,
    ) -> Result<SizedRouteDecision, String> {
        self.route_with_candidate_plans_at_with(req, cache, planner, now_ms, self.outcomes.as_ref())
    }

    /// [`RouterService::route_with_candidate_plans_at`] consulting an explicit
    /// outcome registry (the route-scoped memo a MaximumQuality probe
    /// reuses across its descending quality floors).
    pub fn route_with_candidate_plans_at_with(
        &self,
        req: &RouteRequest,
        cache: &[CacheState],
        planner: &dyn CandidatePlanner,
        now_ms: u64,
        outcomes: &dyn outcomes::OutcomeView,
    ) -> Result<SizedRouteDecision, String> {
        if self.pinned.is_some() || self.priced.is_empty() {
            return self
                .route_at_with(req, cache, now_ms, outcomes)
                .map(SizedRouteDecision::without_plan);
        }
        let health = self.telemetry.snapshot();
        // 1. THE cheap qualification pass (capability/quality/health/
        //    budget/latency) — the same single pass every path consumes,
        //    WITHOUT the adaptive target preference yet: the preference is
        //    re-applied after the measured-fit re-check below, so a target
        //    candidate eliminated by its REAL footprint still relaxes into
        //    `[minimum, target)`.
        let refs: Vec<&RouteCandidate> = self.priced.iter().collect();
        let qualified = qualify_priced_refs(&refs, req, cache, &health, now_ms, outcomes, false)
            .map_err(|f| f.route_error())?;
        // 2. Pre-rank by the cheap (unmeasured) expected-cost ladder.
        let mut pre = score_priced_candidates(&qualified, &self.priced, req, outcomes);
        pre.sort_by(|a, b| a.compare(b));
        let ranked: Vec<QualifiedCandidate<'_>> = pre
            .iter()
            .map(|s| QualifiedCandidate {
                descriptor: &s.candidate.descriptor,
                call_cost_micro: s.call_cost.numeric().unwrap_or(u64::MAX),
                success_ppm: s.success_ppm,
                cost: s.call_cost,
            })
            .collect();
        // 3-4. Size the top-K under each candidate's own tokenizer. The
        //      built plans wait here until the final route picks the winner.
        let mut built: HashMap<(String, String), BuiltCandidatePlan> = HashMap::new();
        let survivors =
            size_candidates_top_k(&ranked, MAX_SIZED_CANDIDATES, |d| match planner.build(d) {
                Some(plan) => {
                    let footprint = plan.footprint;
                    built.insert((d.provider.clone(), d.model.clone()), plan);
                    (footprint.input_tokens, footprint.exact)
                }
                // No plan = the request cannot be rendered for this
                // candidate; it must never survive as an unsized pick.
                None => (u64::MAX, false),
            });
        // 5-6. Re-price every survivor over its measured footprint through
        //      its effective quote and re-check the request budget.
        let mut sized: Vec<QualifiedCandidate<'_>> = Vec::with_capacity(survivors.len());
        for s in &survivors {
            let Some(route_candidate) = self
                .priced
                .iter()
                .find(|c| std::ptr::eq(&c.descriptor, s.candidate))
            else {
                continue;
            };
            let key = (
                route_candidate.descriptor.provider.clone(),
                route_candidate.descriptor.model.clone(),
            );
            if !built.contains_key(&key) {
                continue; // defensive: a survivor without a plan is dropped
            }
            let usage =
                request_usage_with_input(req, cache, &route_candidate.descriptor, s.input_tokens);
            let effective =
                route_candidate.effective_pricing(now_ms, req.task_budget_remaining_micro > 0);
            let cost = CostEstimate::from_effective(&effective, usage);
            if req.task_budget_remaining_micro > 0
                && !matches!(cost.numeric(), Some(n) if n <= req.task_budget_remaining_micro)
            {
                continue;
            }
            sized.push(QualifiedCandidate {
                descriptor: &route_candidate.descriptor,
                call_cost_micro: cost.numeric().unwrap_or(u64::MAX),
                success_ppm: health.success_ppm(
                    &route_candidate.descriptor.provider,
                    &route_candidate.descriptor.model,
                    req.phase,
                ),
                cost,
            });
        }
        if sized.is_empty() {
            // Same denial shape as the fit axis: every candidate that
            // cleared capability/quality failed the REAL footprint fit.
            return Err("no candidate clears capability/fit filtering (missing: )".to_string());
        }
        // 6b. Adaptive target preference over the MEASURED survivors: target
        //     candidates that survived the real footprint/budget re-check
        //     win; when every target candidate was eliminated by the
        //     measured hard axes, the `[minimum, target)` survivors serve.
        prefer_quality_target(&mut sized, req, |d, target| {
            self.priced
                .iter()
                .find(|c| std::ptr::eq(&c.descriptor, d))
                .is_some_and(|c| {
                    effective_quality_statement(
                        c,
                        req.phase,
                        req.task_class,
                        req.risk_bucket,
                        now_ms,
                        outcomes,
                    )
                    .clears_floor(req.phase, target)
                })
        });
        // 7. Final route over the measured survivors.
        let scored = score_priced_candidates(&sized, &self.priced, req, outcomes);
        let winner = scored.iter().min_by(|a, b| a.compare(b)).ok_or_else(|| {
            "no candidate clears capability/fit filtering (missing: )".to_string()
        })?;
        let chosen = winner.candidate;
        let key = (
            chosen.descriptor.provider.clone(),
            chosen.descriptor.model.clone(),
        );
        let plan = built
            .remove(&key)
            .ok_or_else(|| "sized winner lost its built plan".to_string())?;
        let base = winner.call_cost.numeric().unwrap_or(0);
        let ps = f64::from(winner.success_ppm) / 1_000_000.0;
        let verified_tag = match winner.work_estimate {
            Some(est) => format!(
                " verified rework_ppm={} exp_rework_micro={} total_expected_micro={}",
                est.rework_probability_ppm, est.expected_rework_micro, est.total_expected_micro
            ),
            None => String::new(),
        };
        let cost_tag = match winner.call_cost {
            CostEstimate::Unknown => " cost=unknown",
            _ => "",
        };
        let reasoning = format!(
            "phase={:?} expected-cost chosen={}/{} base_micro={base}{} p_success={ps:.2} \
             sized_input={} sized_exact={}{}",
            req.phase,
            chosen.descriptor.provider,
            chosen.descriptor.model,
            cost_tag,
            plan.footprint.input_tokens,
            plan.footprint.exact,
            verified_tag,
        );
        let effective = chosen
            .pricing
            .effective_at(now_ms, req.task_budget_remaining_micro > 0);
        Ok(SizedRouteDecision {
            decision: RouteDecision {
                provider: chosen.descriptor.provider.clone(),
                model: chosen.descriptor.model.clone(),
                estimated_cost_micro: base,
                estimated_latency_ms: winner.expected_latency_ms,
                reasoning,
                considered: self.priced.len(),
                source: chosen.descriptor.source,
                pricing_snapshot: Some(effective.snapshot()),
            },
            plan: Some(CandidatePlan {
                candidate: chosen.descriptor.clone(),
                wire_plan: plan.wire_plan,
                footprint: plan.footprint,
                call_cost: winner.call_cost,
            }),
        })
    }

    /// [`RouterService::route_with_candidate_plans`] under the churn-aware
    /// cache economics of [`RouterService::route_with_prefix_stability`]:
    /// a churning prefix prices the sized route with every cache-read
    /// discount zeroed and the decision carries the churn premium. The
    /// winning plan crosses back untouched.
    pub fn route_with_prefix_stability_and_candidate_plans(
        &self,
        req: &RouteRequest,
        cache: &[CacheState],
        floor: f64,
        prefix_history: Option<&[stability::TurnPrefix]>,
        planner: &dyn CandidatePlanner,
    ) -> Result<SizedRouteDecision, String> {
        self.route_with_prefix_stability_and_candidate_plans_at_with(
            req,
            cache,
            floor,
            prefix_history,
            planner,
            unix_now_ms(),
            self.outcomes.as_ref(),
        )
    }

    /// [`RouterService::route_with_prefix_stability_and_candidate_plans`]
    /// consulting an explicit outcome registry (the route-scoped memo).
    #[allow(clippy::too_many_arguments)] // explicit axes mirror the non-with entry point
    pub fn route_with_prefix_stability_and_candidate_plans_at_with(
        &self,
        req: &RouteRequest,
        cache: &[CacheState],
        floor: f64,
        prefix_history: Option<&[stability::TurnPrefix]>,
        planner: &dyn CandidatePlanner,
        now_ms: u64,
        outcomes: &dyn outcomes::OutcomeView,
    ) -> Result<SizedRouteDecision, String> {
        let Some(history) = prefix_history else {
            return self.route_with_candidate_plans_at_with(req, cache, planner, now_ms, outcomes);
        };
        let last = stability::turn_stabilities(history).pop();
        let Some(last_stability) = last else {
            return self.route_with_candidate_plans_at_with(req, cache, planner, now_ms, outcomes);
        };
        let penalty = stability::churn_penalty(last_stability, floor);
        if penalty == 0.0 {
            return self.route_with_candidate_plans_at_with(req, cache, planner, now_ms, outcomes);
        }
        let cache_without_reads: Vec<CacheState> = cache
            .iter()
            .map(|c| CacheState {
                provider: c.provider.clone(),
                model: c.model.clone(),
                cached_input_tokens: 0,
                will_write_tokens: c.will_write_tokens,
            })
            .collect();
        let mut sized = self.route_with_candidate_plans_at_with(
            req,
            &cache_without_reads,
            planner,
            now_ms,
            outcomes,
        )?;
        sized.decision.estimated_cost_micro = stability::apply_churn_penalty(
            sized.decision.estimated_cost_micro,
            last_stability,
            floor,
        );
        sized.decision.reasoning = format!(
            "{} prefix_stability={last_stability:.3} churn_penalty={penalty:.4}",
            sized.decision.reasoning
        );
        Ok(sized)
    }

    /// PINNED route (pricing-path audit): calls ONLY
    /// [`qualify_specific`] — the pin never competes, and no other
    /// candidate can reject it.
    /// Public pinned decision (pricing-path audit): qualifies ONLY the
    /// pinned (provider, model) — no competition — and builds the decision
    /// from that candidate. Used by pinned routing policies whose service
    /// was not constructed with [`Self::with_pinned_route_candidates`]
    /// (test/embedded callers): a pinned service never competes.
    pub fn route_pinned_decision(
        &self,
        provider: &str,
        model: &str,
        req: &RouteRequest,
        cache: &[CacheState],
    ) -> Result<RouteDecision, String> {
        self.route_pinned_decision_at(provider, model, req, cache, unix_now_ms())
    }

    /// [`RouterService::route_pinned_decision`] at an explicit wall clock.
    pub fn route_pinned_decision_at(
        &self,
        provider: &str,
        model: &str,
        req: &RouteRequest,
        cache: &[CacheState],
        now_ms: u64,
    ) -> Result<RouteDecision, String> {
        if self.priced.is_empty() {
            // Descriptor-only (legacy/embedded) service: pin by qualifying
            // a RESTRICTED single-candidate router — never the full
            // competition (pinning must not depend on what else exists).
            let descriptor = self
                .router
                .candidates
                .iter()
                .find(|d| d.provider == provider && d.model == model)
                .cloned()
                .ok_or_else(|| format!("pinned model {provider}/{model} is not registered"))?;
            let single = Router::new(vec![descriptor]);
            let qualified =
                qualified_candidates(&single.candidates, req, cache, &self.telemetry.snapshot())
                    .map_err(|f| f.route_error())?;
            let chosen = qualified
                .first()
                .ok_or_else(|| "pinned candidate failed qualification".to_string())?;
            return Ok(RouteDecision {
                provider: provider.to_string(),
                model: model.to_string(),
                estimated_cost_micro: chosen.call_cost_micro,
                estimated_latency_ms: chosen.descriptor.performance().estimated_latency_ms,
                reasoning: format!(
                    "phase={:?} pinned chosen={}/{} qualified=1 considered=1",
                    req.phase, provider, model
                ),
                considered: 1,
                source: chosen.descriptor.source,
                pricing_snapshot: None,
            });
        }
        self.route_pinned(provider, model, req, cache, now_ms, self.outcomes.as_ref())
    }

    fn route_pinned(
        &self,
        provider: &str,
        model: &str,
        req: &RouteRequest,
        cache: &[CacheState],
        now_ms: u64,
        outcomes: &dyn outcomes::OutcomeView,
    ) -> Result<RouteDecision, String> {
        let q = self
            .qualify_specific_at_with(provider, model, req, cache, now_ms, outcomes)
            .map_err(|f| f.route_error())?;
        let cost = q.cost;
        let base = cost.numeric().unwrap_or(0);
        let reasoning = format!(
            "phase={:?} pinned chosen={}/{} cost_micro={base} qualified=1 considered=1",
            req.phase, provider, model
        );
        let pinned = self
            .priced
            .iter()
            .find(|c| c.descriptor.provider == provider && c.descriptor.model == model);
        let effective = pinned
            .map(|c| {
                c.pricing
                    .effective_at(now_ms, req.task_budget_remaining_micro > 0)
            })
            .unwrap_or_else(|| {
                EffectivePriceState::Unknown(PricingSnapshot::unknown(0, "pinned-missing".into()))
            });
        Ok(RouteDecision {
            provider: provider.to_string(),
            model: model.to_string(),
            estimated_cost_micro: base,
            estimated_latency_ms: pinned
                .map(|c| c.performance().estimated_latency_ms)
                .unwrap_or(0),
            reasoning,
            considered: self.priced.len(),
            source: pinned
                .map(|c| c.descriptor.source)
                .unwrap_or(faktor_core::model::ModelSource::ConservativeDefault),
            pricing_snapshot: Some(effective.snapshot()),
        })
    }

    /// The legacy descriptor-only route (compatibility constructors; no
    /// pricing authority consulted — decisions carry no snapshot).
    fn route_legacy(
        &self,
        req: &RouteRequest,
        cache: &[CacheState],
        outcomes: &dyn outcomes::OutcomeView,
    ) -> Result<RouteDecision, String> {
        let health = self.telemetry.snapshot();
        let qualified = qualified_candidates(&self.router.candidates, req, cache, &health)
            .map_err(|f| f.route_error())?;
        let scored = score_candidates(&qualified, req.phase, outcomes);
        let winner = scored
            .iter()
            .min_by(|a, b| a.compare(b))
            .expect("qualified_candidates is non-empty on Ok");
        // The plain-router pick over the SAME qualified set (audit string).
        let plain = qualified
            .iter()
            .min_by(|a, b| {
                a.call_cost_micro.cmp(&b.call_cost_micro).then_with(|| {
                    a.descriptor
                        .economics
                        .estimated_latency_ms
                        .cmp(&b.descriptor.performance().estimated_latency_ms)
                })
            })
            .expect("qualified_candidates is non-empty on Ok");
        let chosen = winner.candidate;
        let base = winner.call_cost_micro;
        let ps = f64::from(winner.success_ppm) / 1_000_000.0;
        // Verified-outcome audit (audit 13/14/L): when the winner's score
        // rode a conservative WorkCostEstimate the reasoning names its
        // conservative rework probability and rework term explicitly.
        let verified_tag = match winner.work_estimate {
            Some(est) => format!(
                " verified rework_ppm={} exp_rework_micro={} total_expected_micro={}",
                est.rework_probability_ppm, est.expected_rework_micro, est.total_expected_micro
            ),
            None => String::new(),
        };
        let reasoning = format!(
            "phase={:?} expected-cost chosen={}/{} base_micro={base} p_success={ps:.2} plain={}/{}",
            req.phase,
            chosen.provider,
            chosen.model,
            plain.descriptor.provider,
            plain.descriptor.model,
        ) + &verified_tag;
        Ok(RouteDecision {
            provider: chosen.provider.clone(),
            model: chosen.model.clone(),
            estimated_cost_micro: base,
            estimated_latency_ms: winner.expected_latency_ms,
            reasoning,
            considered: self.router.candidates.len(),
            source: chosen.source,
            // Wave-B item B: route-time price capture of the CHOSEN
            // candidate (the winner, never the plain-cheapest comparison
            // pick) — the frozen snapshot the catalog authority cut for it,
            // or None when this service was built without a pricing map
            // (no authority consulted: settlement fails closed under a
            // hard cap and records a documented Unknown spend otherwise).
            pricing_snapshot: self
                .pricing
                .get(&(chosen.provider.clone(), chosen.model.clone()))
                .cloned(),
        })
    }

    pub fn record(
        &self,
        provider: &str,
        model: &str,
        phase: RouterPhase,
        success: bool,
        retried: bool,
        rate_limited: bool,
    ) {
        if rate_limited {
            self.telemetry.record_rate_limit(provider, 30);
        }
        self.telemetry
            .record(provider, model, phase, success, retried, rate_limited);
    }

    /// Outcome record entry with the call's measured latency (P0-28): the
    /// settlement path feeds settled model calls here — attempted/resolved
    /// outcome, latency, provider/model, and the reliability signals
    /// (retried/rate-limited) — so the next route's priors see reality.
    #[allow(clippy::too_many_arguments)]
    pub fn record_outcome(
        &self,
        provider: &str,
        model: &str,
        phase: RouterPhase,
        success: bool,
        retried: bool,
        rate_limited: bool,
        latency_ms: u64,
    ) {
        if rate_limited {
            self.telemetry.record_rate_limit(provider, 30);
        }
        self.telemetry.record_outcome(
            provider,
            model,
            phase,
            success,
            retried,
            rate_limited,
            latency_ms,
        );
    }

    /// Churn-aware routing (audits 65-66 + P0-82): the same expected-cost
    /// selection as [`RouterService::route`], plus the prefix-churn risk
    /// premium. When the session's LAST recorded prefix stability (the
    /// final per-turn stability of `prefix_history`, i.e. the most recent
    /// completed turn) sits below `floor`, the decision's
    /// `estimated_cost_micro` is scaled by `(1 + churn_penalty)` with the
    /// penalty bounded in [0, 0.25] — unstable prefixes defeat
    /// provider-side caches, so the per-turn cost prediction must not
    /// pretend they hit.
    ///
    /// Cache economics under churn: provider-side prompt caches are exactly
    /// what churn invalidates. With a penalty in effect, every cache
    /// READ discount (`CacheState.cached_input_tokens`) is zeroed for the
    /// route — a churning session must be priced as if its cache misses —
    /// while `will_write_tokens` (the call's own cache write) survives.
    /// The budget axis then sees the honest uncached costs, so a candidate
    /// that was only cheap through cache reads drops out of qualification
    /// and the CHOICE can change, not just the price.
    ///
    /// The premium is a per-SESSION factor (identical for every candidate),
    /// so candidate ORDER for equally-cached candidates is unchanged; the
    /// estimate the decision returns — and therefore downstream budget
    /// math — carries the risk premium, and the audit string records
    /// `prefix_stability` and `churn_penalty` explicitly. `None`/empty
    /// history means no recorded stability: no premium is charged and the
    /// result is identical to `route`.
    pub fn route_with_prefix_stability(
        &self,
        req: &RouteRequest,
        cache: &[CacheState],
        floor: f64,
        prefix_history: Option<&[stability::TurnPrefix]>,
    ) -> Result<RouteDecision, String> {
        self.route_with_prefix_stability_at_with(
            req,
            cache,
            floor,
            prefix_history,
            unix_now_ms(),
            self.outcomes.as_ref(),
        )
    }

    /// [`RouterService::route_with_prefix_stability`] consulting an explicit
    /// outcome registry (the route-scoped memo).
    pub fn route_with_prefix_stability_at_with(
        &self,
        req: &RouteRequest,
        cache: &[CacheState],
        floor: f64,
        prefix_history: Option<&[stability::TurnPrefix]>,
        now_ms: u64,
        outcomes: &dyn outcomes::OutcomeView,
    ) -> Result<RouteDecision, String> {
        let Some(history) = prefix_history else {
            return self.route_at_with(req, cache, now_ms, outcomes);
        };
        let last = stability::turn_stabilities(history).pop();
        let Some(last_stability) = last else {
            return self.route_at_with(req, cache, now_ms, outcomes);
        };
        let penalty = stability::churn_penalty(last_stability, floor);
        if penalty == 0.0 {
            return self.route_at_with(req, cache, now_ms, outcomes);
        }
        // Churn invalidates provider-side caches: price the route with
        // every cache-read discount zeroed (the cache write this call
        // performs still stands).
        let cache_without_reads: Vec<CacheState> = cache
            .iter()
            .map(|c| CacheState {
                provider: c.provider.clone(),
                model: c.model.clone(),
                cached_input_tokens: 0,
                will_write_tokens: c.will_write_tokens,
            })
            .collect();
        let mut decision = self.route_at_with(req, &cache_without_reads, now_ms, outcomes)?;
        decision.estimated_cost_micro =
            stability::apply_churn_penalty(decision.estimated_cost_micro, last_stability, floor);
        decision.reasoning = format!(
            "{} prefix_stability={last_stability:.3} churn_penalty={penalty:.4}",
            decision.reasoning
        );
        Ok(decision)
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

#[cfg(test)]
#[path = "quality_authority_tests.rs"]
mod quality_authority_tests;
