//! faktor-agent — the durable agent reasoning loop.
//!
//! Drives `faktor-session` with commands, consumes `faktor-provider` streams,
//! schedules tools through `faktor-scheduler`, and keeps context bounded via
//! `faktor-context`. Rules (from the architecture spec):
//!
//! - **No provider-name conditionals.** Behavior comes from
//!   `ModelCapabilities`; provider quirks stay inside adapters.
//! - **State-aware continuation.** A provider stream that dies mid-tool never
//!   replays the turn; the journal determines the continuation point.
//! - **Repair once, never five times.** Malformed tool JSON gets one
//!   deterministic repair pass; repeated identical failures trip the loop
//!   detector and stop the turn.
//! - **Bounded context before sending.** Budget enforced by the assembler;
//!   compaction triggers proactively at the configured usage fraction.

use std::sync::Arc;

use faktor_core::id::SessionId;
use faktor_router::stability::TurnPrefix;
use faktor_verify::exec::{BudgetDecision, CheckOutcome, CheckRunStatus};

pub mod activation;
pub mod credits;
pub mod loop_detect;
pub mod runtime;
pub mod stall;
pub mod tool;
pub mod tool_json;
pub mod wire_plan;

pub use activation::{
    acquire_source_phases, acquire_source_triggers, evaluate_triggers, PhaseMask,
    ToolActivationSet, ToolExposure, ToolTrigger, TriggerVerdict, ACQUIRE_FLAG_OFF,
    ACQUIRE_FLAG_ON, ACQUIRE_PRODUCT_URL_HOSTS, ACQUIRE_STRONG_SIGNALS, ACQUIRE_WEAK_SIGNALS,
};
pub use credits::{
    AttemptDebits, DebitDecision, DebitError, DebitHold, ProviderAttemptDebit,
    ProviderAttemptDebits, MAX_DEBIT_TEXT_BYTES,
};
pub use faktor_core::model::{RiskBucket, RouteDecision, RouterPhase, RoutingMode, TaskClass};
pub use faktor_core::state::{
    CheckExecution, CriterionVerification, FileStateEvidence, OutcomeReason, ReasonCode, TaskState,
    TaskTransition, VerificationStatus,
};
pub use faktor_session::VerificationRecord;
pub use faktor_verify::Acceptance;
pub use loop_detect::LoopDetector;
pub use runtime::{
    review_model_identity_of, AgentCard, AgentDeps, AgentRuntime, ChunkEvent, ChunkSink,
    CompletionGate, EvidenceProvider, EvidenceQuery, IntegratedRootVerification, NoEvidence,
    PermissionRequester, ReviewModelIdentity, ToolArtifactSink, TurnOutcome, VerificationQuality,
};
pub use stall::{StallTracker, DEFAULT_STALL_SILENCE_MS};
pub use tool::{
    board_post_tool, board_read_tool, BoardToolGateway, FilePostcondition, RecoveryHint,
    ReplayDescriptor, Tool, ToolBundle, ToolBundleId, ToolOutcome, ToolRegistry, ToolRunCtx,
    BOARD_POST_TOOL, BOARD_READ_TOOL, BOARD_TOOL_MAX_LIMIT, BOARD_TOOL_MAX_REFS,
    BOARD_TOOL_TEXT_MAX, SEMANTIC_BUNDLE_MAX_SPECS, SEMANTIC_QUERY_TOOL,
};
pub use tool_json::{parse_tool_calls, repair_json, ToolCallMode};

/// The production efficiency flags (audit 86 + the efficiency-variant
/// production switches): the agent-side mirror of the daemon's parsed
/// `[efficiency]` section. Every flag defaults to `false` — the baseline
/// production behavior — and each switch gates an ADDITIVE behavior only:
///
/// - [`EfficiencyFlags::failure_learning`]: apply the installed failure-aware
///   context prior ([`AgentDeps::context_prior`]) to non-Required candidate
///   selection through `plan_context_with_information_and_prior`;
/// - `ccr`, `typed_handoff`, `semantic_context`, `rework_routing`: parsed and
///   carried by [`AgentDeps`] for the corresponding efficiency components.
///
/// With every flag off (and/or no prior handle installed) the runtime's
/// plans are byte-identical to the pre-flag path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EfficiencyFlags {
    /// Failure-learning prior in context selection (audit 68).
    pub failure_learning: bool,
    /// Compressed Context Representation for tool/evidence payloads.
    pub ccr: bool,
    /// Typed handoff: re-sent history rendered from durable task rows.
    pub typed_handoff: bool,
    /// Semantic context: information-gain selection of evidence.
    pub semantic_context: bool,
    /// Rework-aware routing over durable verified-outcome stats.
    pub rework_routing: bool,
}

impl EfficiencyFlags {
    /// The production gate for the failure-aware context prior: the prior is
    /// applied ONLY when `failure_learning` is on AND a handle was installed.
    /// Any other combination yields `None`, so the planner takes its
    /// baseline path and the plan stays byte-identical.
    pub fn context_prior<'a>(
        &self,
        prior: Option<&'a (dyn faktor_context::information::FailurePrior + Send + Sync)>,
    ) -> Option<&'a (dyn faktor_context::information::FailurePrior + Send + Sync)> {
        if self.failure_learning {
            prior
        } else {
            None
        }
    }
}

/// The fallback-only semantic-provider registry (audit 48-54/58/79): the
/// additive [`AgentDeps::semantic`] handle every construction site installs
/// unless a host explicitly registers a richer provider. The generic
/// fallback answers every operation itself, so ordinary operation NEVER
/// fails solely because no semantic provider is installed — and with only
/// the fallback registered the runtime's optional consults stay
/// byte-identical to a provider-less runtime (parity).
pub fn fallback_semantic_registry() -> Arc<faktor_semantic::SemanticProviderRegistry> {
    Arc::new(faktor_semantic::SemanticProviderRegistry::new(
        faktor_semantic::GenericSemanticFallback::default(),
    ))
}

// --------------------------------------------------------------------------
// VerificationService (P0-9/P0-10): the runtime's single verification
// execution engine. Replaces the legacy `Option<Arc<Verifier>>` seam: the
// field is NON-optional and typed — every deployment carries a service, and
// "no objective mechanism" is an explicit [`VerificationService::disabled`]
// state that classifies mutating turns Unverified (the old `None` behavior).
// Execution is typed (program, argv) specs through the
// `faktor-verify::exec` async executor, never a shell string through `sh -c`.
// --------------------------------------------------------------------------

/// Command-string backend shared by test seams and embedded hosts: receives
/// the canonical `program arg...` string of each typed check (the legacy
/// [`faktor_verify::RunFn`] contract). Kept deterministic and synchronous —
/// the backend exists to inject scripted verdicts, not to run processes
/// (real execution goes through the async executor).
pub type VerificationRunFn = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// Execution backend of a [`VerificationService`].
#[derive(Clone)]
enum VerificationBackend {
    /// Real executor: typed child processes through THE workspace
    /// supervisor under the context's deadline and cancellation (bounded
    /// capture, process-group kill). The only backend that can persist
    /// background verification jobs.
    Async(Arc<faktor_verify::exec::AsyncCheckExecutor>),
    /// Scripted command-string runner (tests / embedded hosts).
    Command(VerificationRunFn),
}

/// The agent runtime's verification service (P0-9/P0-10). Wraps the typed
/// executor and the [`faktor_verify::exec::VerificationPolicy`] so the
/// genuine-end verification site is purely policy-driven:
///
/// - budgets come from [`faktor_verify::exec::budget_for`] per check
///   category — there is NO universal ~10 s wall cap anywhere on this path;
/// - checks whose policy says "task-owned background operation" run as
///   DURABLE background verification jobs (audit P0-5/26): the runtime
///   persists each such check as a `faktor-session` VerificationJob row,
///   the task row stays Verifying, and the supervisor-backed executor
///   settles the jobs at a later genuine end. Only the real (supervisor)
///   backend can background checks ([`VerificationService::can_persist_jobs`]);
///   scripted command backends (test seams) run every decision inline —
///   their verdicts are instantaneous and deterministic;
/// - [`VerificationService::disabled`] fails closed to the runtime's
///   Unverified classification (no objective mechanism configured);
/// - every check executes against the session's DURABLE workspace root —
///   the daemon's current directory is never consulted.
#[derive(Clone)]
pub struct VerificationService {
    backend: VerificationBackend,
    policy: faktor_verify::exec::VerificationPolicy,
    enabled: bool,
}

impl VerificationService {
    /// The real service: a typed async executor under `policy`.
    pub fn new(
        executor: Arc<faktor_verify::exec::AsyncCheckExecutor>,
        policy: faktor_verify::exec::VerificationPolicy,
    ) -> Arc<Self> {
        Arc::new(Self {
            backend: VerificationBackend::Async(executor),
            policy,
            enabled: true,
        })
    }

    /// No objective mechanism for this deployment: mutating turns classify
    /// Unverified (never silently complete). Replaces the old
    /// `AgentDeps.verifier: None` wiring; every other construction site in
    /// tests that previously passed `None` uses this.
    pub fn disabled() -> Arc<Self> {
        Arc::new(Self {
            backend: VerificationBackend::Command(Arc::new(|_| {
                Err("verification disabled".to_string())
            })),
            policy: faktor_verify::exec::VerificationPolicy::disabled(),
            enabled: false,
        })
    }

    /// A scripted service that runs every check through `run` (the legacy
    /// command-string contract): `Ok(())` -> Passed/exit 0, `Err(msg)` ->
    /// Failed with the message as the summary. Keeps the wave-16 test
    /// ergonomics (deterministic verdicts, asserted command vectors).
    pub fn fake<R>(run: R) -> Arc<Self>
    where
        R: Fn(&str) -> Result<(), String> + Send + Sync + 'static,
    {
        Arc::new(Self {
            backend: VerificationBackend::Command(Arc::new(run)),
            policy: faktor_verify::exec::VerificationPolicy::default(),
            enabled: true,
        })
    }

    /// A scripted service whose checks always pass (the ubiquitous
    /// always-Ok verifier test seam).
    pub fn fake_ok() -> Arc<Self> {
        Self::fake(|_| Ok(()))
    }

    pub fn is_disabled(&self) -> bool {
        !self.enabled
    }

    /// The policy in effect (observability / tests).
    pub fn policy(&self) -> faktor_verify::exec::VerificationPolicy {
        self.policy
    }

    /// The budget decision for one spec under this service's policy. The
    /// remaining-turn budget is `None`: the genuine-end verification site
    /// has no cheaper per-check accounting, so the policy's category caps
    /// bound every inline check (documented — the policy, never a hard-coded
    /// wall cap, is the authority).
    pub fn budget_for(&self, spec: &faktor_verify::exec::CheckSpec) -> BudgetDecision {
        faktor_verify::exec::budget_for(spec.category, &self.policy, None)
    }

    /// True when the service executes checks through the REAL supervisor
    /// executor — the only backend that can persist background
    /// verification jobs (audit P0-5/26). Scripted command backends (test
    /// seams) return false: their verdicts are instantaneous, so the
    /// runtime keeps executing every decision inline for them.
    pub fn can_persist_jobs(&self) -> bool {
        matches!(self.backend, VerificationBackend::Async(_))
    }

    /// Execute ONE typed check under the context's deadline and
    /// cancellation. Infra errors (spawn refusal, unreadable root, ...)
    /// become [`CheckRunStatus::Unavailable`] outcomes — the CALLER decides
    /// the completion-gate meaning (BlockedVerification), never a silent
    /// failure of the code under check and never a turn failure.
    pub async fn execute(
        &self,
        spec: &faktor_verify::exec::CheckSpec,
        ctx: &faktor_verify::exec::VerificationContext,
    ) -> CheckOutcome {
        match &self.backend {
            VerificationBackend::Async(executor) => match executor.run_check(spec, ctx).await {
                Ok(outcome) => outcome,
                Err(e) => unavailable_outcome(spec, format!("verification infra error: {e}")),
            },
            VerificationBackend::Command(run) => {
                let command = canonical_command(spec);
                let started_ms = now_ms();
                match run(&command) {
                    Ok(()) => CheckOutcome {
                        status: CheckRunStatus::Passed,
                        exit: Some(0),
                        started_ms,
                        finished_ms: now_ms(),
                        summary: None,
                        truncated: false,
                    },
                    Err(message) => CheckOutcome {
                        status: CheckRunStatus::Failed,
                        exit: None,
                        started_ms,
                        finished_ms: now_ms(),
                        summary: Some(truncate_line(&message)),
                        truncated: false,
                    },
                }
            }
        }
    }
}

/// The canonical command text of a typed spec (program + args, single-space
/// joined). Specs are built by strict simple-token rules, so the join is
/// deterministic and lossless; it is what the scripted command backend runs
/// and what legacy mirrors carry as their canonical text.
fn canonical_command(spec: &faktor_verify::exec::CheckSpec) -> String {
    let mut text = spec.program.to_string_lossy().into_owned();
    for arg in &spec.args {
        text.push(' ');
        text.push_str(&arg.to_string_lossy());
    }
    text
}

fn unavailable_outcome(spec: &faktor_verify::exec::CheckSpec, summary: String) -> CheckOutcome {
    let now = now_ms();
    CheckOutcome {
        status: CheckRunStatus::Unavailable,
        exit: None,
        started_ms: now,
        finished_ms: now,
        summary: Some(format!(
            "{}: {summary}",
            spec.program.to_string_lossy().into_owned()
        )),
        truncated: false,
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Bound one command-backend failure line before it rides a durable record.
fn truncate_line(text: &str) -> String {
    const MAX: usize = 512;
    let mut out: String = text.chars().take(MAX).collect();
    if text.chars().count() > MAX {
        out.push('…');
    }
    out
}

// --------------------------------------------------------------------------
// Routing policy (P0-2/85/87/88): the daemon's single decision authority
// over which provider/model serves every model call.
// --------------------------------------------------------------------------

/// Why a routed call was refused. Fail-closed matrix (P0-88 + attempt-
/// accounting audit): there is NO fallback — every failure is a typed
/// terminal error on the call (no silent degradation, no half-configured
/// substitution, no session-configured-model bypass). The runtime never
/// reaches a provider whose call the policy refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteFailure {
    /// The request cannot be served within the task's remaining budget.
    BudgetExceeded,
    /// No candidate (or the pinned model) can serve the request:
    /// capabilities, context/output fit or phase quality (a hard quality
    /// floor with no candidate above it is a NoCapableModel refusal, never
    /// a floor-lowering).
    NoCapableModel,
    /// The policy refused: the pin lost the router's own evaluation, the
    /// request violated a policy constraint, or the router denied for
    /// non-budget reasons.
    PolicyDenied,
    /// Routing telemetry/health machinery failed internally.
    InternalTelemetry,
}

/// The routing decision authority consumed by the agent runtime before
/// EVERY paid model call (the former "auto"-sentinel path is gone: there is
/// no un-routed model call anymore, and there is no fallback).
///
/// The quality requirement of one routed model call (attempt-accounting
/// audit): quality is a requirement the router must SERVE, never a ceiling
/// the policy lowers toward the best available candidate. A hard floor that
/// no candidate clears is a typed [`RouteFailure::NoCapableModel`] refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "requirement")]
pub enum QualityRequirement {
    /// The floor is a hard minimum: no candidate below it may serve the
    /// call, and the route is refused when none clears it. Implement and
    /// Review calls route under this requirement.
    Hard { minimum: u8 },
    /// A preferred target that may relax to `minimum` (never below) when
    /// the routing mode's economics decide.
    Adaptive { target: u8, minimum: u8 },
}

impl QualityRequirement {
    /// The floor the router's qualification pass must apply verbatim.
    pub fn minimum(&self) -> u8 {
        match self {
            QualityRequirement::Hard { minimum } | QualityRequirement::Adaptive { minimum, .. } => {
                *minimum
            }
        }
    }

    /// The preferred target (the minimum for a hard requirement).
    pub fn target(&self) -> u8 {
        match self {
            QualityRequirement::Hard { minimum } => *minimum,
            QualityRequirement::Adaptive { target, .. } => *target,
        }
    }
}

/// The intent of ONE model call the runtime routes (attempt-accounting
/// audit, item D): phase, required capabilities, the quality requirement,
/// the caller's planned output cap and the call's semantic risk. The wire
/// dimensions of the FINAL planned request (actual input estimate + output
/// cap) are attached at route time through
/// [`ModelCallIntent::route_request`], so the routed decision prices and
/// qualifies the REAL call, never a hard-coded guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCallIntent {
    pub phase: RouterPhase,
    pub required_capabilities: Vec<String>,
    pub quality: QualityRequirement,
    /// The planned request's output cap in tokens (what the caller will
    /// accept; never a fabricated 2048 default).
    pub expected_output_tokens: u64,
    /// 0..=100 semantic risk of the call (a review of a risky change is
    /// high; interior summarization is low).
    pub semantic_risk: u8,
}

impl ModelCallIntent {
    /// The hard quality floor the router applies verbatim (never lowered
    /// toward the best available candidate).
    pub fn quality_floor(&self) -> u8 {
        self.quality.minimum().min(100)
    }

    /// Build the route request for this intent against the ACTUAL planned
    /// dimensions: `context_estimate_tokens` is the final wire plan's input
    /// estimate (the plan's own total), `output_cap_tokens` the caller's
    /// planned output cap, `task_budget_remaining_micro` the durable free
    /// budget (0 = unlimited). 0 <= semantic_risk <= 100 is enforced at
    /// construction sites through [`ModelCallIntent::with_semantic_risk`].
    pub fn route_request(
        &self,
        context_estimate_tokens: u64,
        output_cap_tokens: u64,
        task_budget_remaining_micro: u64,
    ) -> faktor_router::RouteRequest {
        faktor_router::RouteRequest {
            phase: self.phase,
            required_capabilities: self.required_capabilities.clone(),
            context_tokens: context_estimate_tokens,
            estimated_output_tokens: output_cap_tokens.min(self.expected_output_tokens.max(1)),
            quality_floor: self.quality_floor(),
            task_budget_remaining_micro,
            latency_preference_ms: None,
            task_class: TaskClass::Medium,
            risk_bucket: match self.semantic_risk {
                0..=33 => RiskBucket::Low,
                34..=66 => RiskBucket::Medium,
                _ => RiskBucket::High,
            },
        }
    }

    /// Bounded risk assignment (clamped, never accepted raw).
    pub fn with_semantic_risk(mut self, risk: u8) -> Self {
        self.semantic_risk = risk.min(100);
        self
    }
}

/// The per-phase intent builders the runtime uses (quality is a HARD floor
/// for Implement/Review — the audit's default; interior calls stay on the
/// documented 60 floor with their own real dimensions).
impl ModelCallIntent {
    /// The main Implement-phase model call of an iteration: the required
    /// capabilities mirror the planner's wire needs (tools + streaming).
    pub fn implement_main() -> Self {
        Self {
            phase: RouterPhase::Implement,
            required_capabilities: vec!["tools".into(), "streaming".into()],
            quality: QualityRequirement::Hard { minimum: 60 },
            expected_output_tokens: u64::MAX,
            semantic_risk: 0,
        }
    }

    /// A Review-phase call (the independent risky-change review): bounded
    /// package input, a small typed verdict output.
    pub fn review() -> Self {
        Self {
            phase: RouterPhase::Review,
            required_capabilities: vec!["streaming".into()],
            quality: QualityRequirement::Hard { minimum: 60 },
            expected_output_tokens: 2048,
            semantic_risk: 100,
        }
    }

    /// The compaction summarizer call (interior, low semantic risk).
    pub fn compact() -> Self {
        Self {
            phase: RouterPhase::Compact,
            required_capabilities: vec!["streaming".into()],
            quality: QualityRequirement::Hard { minimum: 60 },
            expected_output_tokens: 4096,
            semantic_risk: 0,
        }
    }
}

/// The routing decision authority consumed by the agent runtime before
pub trait RoutingPolicy: Send + Sync {
    /// Route one model call. The returned decision's provider/model are
    /// authoritative; an EMPTY provider/model means "the session's own
    /// configured side" (the passthrough test policy's pin — the runtime
    /// keeps the session defaults verbatim).
    fn route(&self, req: &faktor_router::RouteRequest) -> Result<RouteDecision, RouteFailure>;

    /// The mode this policy was built with (audit/reporting surface; the
    /// policy itself applies it inside [`RoutingPolicy::route`]).
    fn mode(&self) -> RoutingMode;

    /// Cache-economics consult (P0-82): `route` plus the session's stored
    /// prefix-stability history. The production policy prices a churning
    /// session WITHOUT provider-side cache-read discounts and charges the
    /// churn premium on the decision (see
    /// `faktor_router::RouterService::route_with_prefix_stability`).
    /// `None`/empty history (no rows, or a stability read that failed —
    /// never an error on the turn) routes exactly like [`RoutingPolicy::route`]:
    /// no penalty, no cache discount zeroing. Policies without stability
    /// machinery ignore the history (default = `route`).
    fn route_with_session_stability(
        &self,
        req: &faktor_router::RouteRequest,
        _prefix_history: Option<&[TurnPrefix]>,
    ) -> Result<RouteDecision, RouteFailure> {
        self.route(req)
    }

    /// Candidate-sized consult (candidate-specific accounting audit): routes
    /// with the injected per-candidate wire-plan builder, so every
    /// seriously-considered candidate is measured under its OWN tokenizer
    /// before the final selection and the winning wire plan crosses back for
    /// reuse. Default: the plain stability consult with no plan (callers keep
    /// their own plan; byte-identical behavior for every policy that does not
    /// opt in).
    fn route_with_session_stability_and_candidate_plans(
        &self,
        req: &faktor_router::RouteRequest,
        prefix_history: Option<&[TurnPrefix]>,
        _planner: &dyn faktor_router::CandidatePlanner,
    ) -> Result<faktor_router::SizedRouteDecision, RouteFailure> {
        self.route_with_session_stability(req, prefix_history)
            .map(faktor_router::SizedRouteDecision::without_plan)
    }

    /// The telemetry outcome record entry (P0-28 residuals): the runtime
    /// calls this after each settled model call with the actual outcome
    /// (attempted/resolved, latency, provider/model, reliability signals).
    /// Default: no telemetry (test policies, no-op hosts).
    fn record_call_outcome(&self, _outcome: &SettledCallOutcome) {}
}

/// One settled model call's outcome, recorded into the router telemetry
/// (provider/model of the ACTUAL settled call — also correct for passthrough
/// decisions — plus the measured latency and reliability signals).
///
/// The verified attribution entry (audit items 13/14/L) is carried ONLY at
/// the deterministic gate sites: "the model said done" is not a verified
/// signal, so the runtime's settle/uncertain feeds leave `verified` as `None`
/// (telemetry only — the outcomes registry never learns a success it cannot
/// prove) and the task-complete/gate site re-records the retained settled
/// call with the explicit verified signal. `None` = no verified sample
/// (never a success).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettledCallOutcome {
    pub provider: String,
    pub model: String,
    pub phase: RouterPhase,
    /// The call resolved (stream completed); false = the call failed.
    pub success: bool,
    /// This logical call needed more than one attempt.
    pub retried: bool,
    /// The terminal failure was a rate limit (cooldown + prior decay).
    pub rate_limited: bool,
    /// Measured latency of the settled (final) attempt in ms.
    pub latency_ms: u64,
    /// Explicit verified-outcome attribution carried by the deterministic
    /// verification gate sites only (see the struct docs).
    pub verified: Option<VerifiedCallAttribution>,
}

/// One settled call's explicit verified-outcome attribution (audit items
/// 13/14/L): the full registry key dimensions the runtime knows at
/// settlement plus the verified-success signal and the rework the call's
/// failure eventually caused until its task verified. A sample is recorded
/// as a FIRST-PASS SUCCESS only when the task ultimately passed
/// deterministic verification AND attribution identified this call/phase;
/// every other gate outcome records a FAILURE sample (rework was needed),
/// never a success. Rework sums are the caller's measured numbers and stay 0
/// when they cannot be attributed yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifiedCallAttribution {
    pub task_class: TaskClass,
    pub risk_bucket: RiskBucket,
    /// The explicit verified-success signal: the task ultimately passed
    /// deterministic verification.
    pub verified_success: bool,
    /// Downstream spend this settled call's failure caused until its task
    /// verified (microUSD); 0 = unmeasured.
    pub rework_cost_micro: u64,
    /// Downstream turns this settled call's failure caused; 0 = unmeasured.
    pub rework_turns: u64,
}

/// The production policy: a real [`faktor_router::RouterService`] under a
/// fixed [`RoutingMode`] decided once at graph build.
///
/// - [`RoutingMode::Economy`]: every request is routed; the decision's
///   provider/model override the session-configured defaults.
/// - [`RoutingMode::MaximumQuality`]: every request is routed to the top
///   phase-quality tier that clears the router's hard caps — the mode
///   probes the router's own qualification pass at descending quality
///   floors (distinct candidate qualities above the request's floor) and
///   takes the decision of the highest floor that the router can serve.
///   Within the top tier the router's expected-cost ladder applies
///   (success-ppm aware, cost second).
/// - [`RoutingMode::Balanced`]: expected-cost routing (like Economy) but
///   never below the balanced quality band ([`EconomicRoutingPolicy::BALANCED_QUALITY_FLOOR`])
///   while a band candidate can serve the request.
/// - [`RoutingMode::Pinned`]: every request is VALIDATED against the pin —
///   capability/fit/quality/budget axes through the same RouterService — and
///   the pin wins as long as it clears them. The router's free choice is
///   never silently substituted for the pin (fail closed).
pub struct EconomicRoutingPolicy {
    service: Arc<faktor_router::RouterService>,
    mode: RoutingMode,
}

impl std::fmt::Debug for EconomicRoutingPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EconomicRoutingPolicy")
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

/// The phase-quality metric the router applies per phase (mirror of the
/// router crate's documented rule; duplicated here because the router's
/// helper is private): heavy phases (Implement/Review/Debug) judge the
/// coding-reliability mean, every other phase judges context reliability.
fn phase_quality(econ: &faktor_core::model::ModelEconomics, phase: RouterPhase) -> u8 {
    match phase {
        RouterPhase::Implement | RouterPhase::Review | RouterPhase::Debug => econ.coding_quality(),
        _ => econ.context_reliability,
    }
}

impl EconomicRoutingPolicy {
    /// The balanced-mode quality band boundary (P0-85): candidates whose
    /// phase quality sits at or above this band are treated as the
    /// verified-reliable tier; Balanced never routes below the band while a
    /// band candidate clears the request's hard caps.
    pub const BALANCED_QUALITY_FLOOR: u8 = 88;

    pub fn new(service: Arc<faktor_router::RouterService>, mode: RoutingMode) -> Arc<Self> {
        Arc::new(Self { service, mode })
    }

    /// The service's candidate set (observability/tests).
    pub fn candidates(&self) -> &[faktor_core::model::ModelDescriptor] {
        &self.service.router.candidates
    }

    fn map_denial(&self, msg: &str, req: &faktor_router::RouteRequest) -> RouteFailure {
        if msg.contains("no candidate clears capability/fit") || msg.contains("quality floor") {
            // No candidate serves the request's hard axes — capabilities,
            // context/output fit, or the phase quality floor. The floor is a
            // hard requirement: an empty above-floor candidate set is a
            // NoCapableModel refusal (requested hard 60 with best available
            // 50 => NoCapableModel), never a PolicyDenied of a capable set.
            return RouteFailure::NoCapableModel;
        }
        // The remaining denial text names the budget/latency axes together;
        // the runtime never sets a latency preference, so a positive
        // remaining budget means the budget axis denied.
        if req.task_budget_remaining_micro > 0 {
            RouteFailure::BudgetExceeded
        } else {
            RouteFailure::PolicyDenied
        }
    }

    /// One router consult at an explicit quality floor, churn-aware when a
    /// prefix history is supplied (the session-stability path; `None`/empty
    /// history routes exactly like the plain consult — no penalty).
    ///
    /// [`EconomicRoutingPolicy::consult_at_with_plans`] with the optional
    /// candidate plan builder (candidate-specific accounting audit): with
    /// `planner` present the router's candidate-sized entry runs and the
    /// winning plan crosses back; without one the plain consult runs
    /// byte-identically.
    fn consult_at_with_plans(
        &self,
        req: &faktor_router::RouteRequest,
        floor: u8,
        prefix_history: Option<&[TurnPrefix]>,
        planner: Option<&dyn faktor_router::CandidatePlanner>,
    ) -> Result<faktor_router::SizedRouteDecision, String> {
        let mut routed = req.clone();
        routed.quality_floor = floor;
        match (planner, prefix_history) {
            (Some(planner), None) => self
                .service
                .route_with_candidate_plans(&routed, &[], planner),
            (Some(planner), Some(history)) => self
                .service
                .route_with_prefix_stability_and_candidate_plans(
                    &routed,
                    &[],
                    faktor_router::stability::DEFAULT_STABILITY_FLOOR,
                    Some(history),
                    planner,
                ),
            (None, None) => self
                .service
                .route(&routed, &[])
                .map(faktor_router::SizedRouteDecision::without_plan),
            (None, Some(history)) => self
                .service
                .route_with_prefix_stability(
                    &routed,
                    &[],
                    faktor_router::stability::DEFAULT_STABILITY_FLOOR,
                    Some(history),
                )
                .map(faktor_router::SizedRouteDecision::without_plan),
        }
    }

    fn route_economy_sized(
        &self,
        req: &faktor_router::RouteRequest,
        prefix_history: Option<&[TurnPrefix]>,
        planner: Option<&dyn faktor_router::CandidatePlanner>,
    ) -> Result<faktor_router::SizedRouteDecision, RouteFailure> {
        // The requested floor applies VERBATIM: the policy never lowers a
        // hard quality requirement toward the best available candidate.
        self.consult_at_with_plans(req, req.quality_floor.min(100), prefix_history, planner)
            .map_err(|e| self.map_denial(&e, req))
    }

    fn route_economy(
        &self,
        req: &faktor_router::RouteRequest,
        prefix_history: Option<&[TurnPrefix]>,
    ) -> Result<RouteDecision, RouteFailure> {
        self.route_economy_sized(req, prefix_history, None)
            .map(|sized| sized.decision)
    }

    /// Maximum-quality selection: probe the router's own qualification pass
    /// at the descending distinct phase qualities of the candidates (never
    /// below the request's effective floor), and take the decision of the
    /// HIGHEST floor the router can serve. A tier that the router refuses —
    /// cooldown, budget, capability, fit — simply drops out, so the mode
    /// maximizes the verified-success quality subject to the router's hard
    /// caps. Denials surface when NO tier above the effective floor clears
    /// the caps: the most permissive (lowest-floor) refusal is propagated.
    /// The number of router consults is bounded by the distinct phase
    /// qualities in the candidate set (small; the candidate catalog itself
    /// is bounded).
    fn route_maximum_quality_sized(
        &self,
        req: &faktor_router::RouteRequest,
        prefix_history: Option<&[TurnPrefix]>,
        planner: Option<&dyn faktor_router::CandidatePlanner>,
    ) -> Result<faktor_router::SizedRouteDecision, RouteFailure> {
        // The requested floor is the hard lower bound of the tier probe:
        // tiers below it never serve, and when no tier above it clears the
        // caps the refusal names the empty above-floor set.
        let mut tiers: Vec<u8> = self
            .service
            .router
            .candidates
            .iter()
            .map(|c| phase_quality(&c.economics, req.phase))
            .filter(|&q| q >= req.quality_floor.min(100))
            .collect();
        tiers.sort_unstable();
        tiers.dedup();
        tiers.reverse();
        let mut last_denial: Option<RouteFailure> = None;
        for floor in tiers {
            match self.consult_at_with_plans(req, floor, prefix_history, planner) {
                Ok(d) => return Ok(d),
                Err(e) => last_denial = Some(self.map_denial(&e, req)),
            }
        }
        last_denial.map_or(Err(RouteFailure::NoCapableModel), Err)
    }

    fn route_maximum_quality(
        &self,
        req: &faktor_router::RouteRequest,
        prefix_history: Option<&[TurnPrefix]>,
    ) -> Result<RouteDecision, RouteFailure> {
        self.route_maximum_quality_sized(req, prefix_history, None)
            .map(|sized| sized.decision)
    }

    fn route_balanced_sized(
        &self,
        req: &faktor_router::RouteRequest,
        prefix_history: Option<&[TurnPrefix]>,
        planner: Option<&dyn faktor_router::CandidatePlanner>,
    ) -> Result<faktor_router::SizedRouteDecision, RouteFailure> {
        // Balanced never routes below its quality band; the band floor is
        // applied verbatim (no lowering toward the best available).
        let floor = req.quality_floor.clamp(Self::BALANCED_QUALITY_FLOOR, 100);
        self.consult_at_with_plans(req, floor, prefix_history, planner)
            .map_err(|e| self.map_denial(&e, req))
    }

    fn route_balanced(
        &self,
        req: &faktor_router::RouteRequest,
        prefix_history: Option<&[TurnPrefix]>,
    ) -> Result<RouteDecision, RouteFailure> {
        self.route_balanced_sized(req, prefix_history, None)
            .map(|sized| sized.decision)
    }

    fn route_pinned(
        &self,
        req: &faktor_router::RouteRequest,
        provider: &str,
        model: &str,
    ) -> Result<RouteDecision, RouteFailure> {
        let pinned = self
            .service
            .router
            .candidates
            .iter()
            .find(|c| c.provider == provider && c.model == model)
            .ok_or(RouteFailure::NoCapableModel)?;
        let pin_quality = phase_quality(&pinned.economics, req.phase);
        if pin_quality < req.quality_floor.min(100) {
            // The pin is below the request's HARD quality floor: fail
            // closed — the request's floor is never lowered to the pin.
            return Err(RouteFailure::PolicyDenied);
        }
        // Validation request only the pin can serve among candidates that
        // do not dominate it on every quality axis: the pin's own true
        // capabilities, its own phase quality as the floor, and its own
        // estimated latency as the preference. A candidate that beats the
        // pin on cost while matching it on caps/quality/latency still wins
        // the router's evaluation — and that is a loud PolicyDenied below,
        // never a silent substitution of the pin.
        let mut caps: Vec<String> = req.required_capabilities.clone();
        for (flag, name) in [
            (pinned.tools, "tools"),
            (pinned.parallel_tools, "parallel_tools"),
            (pinned.reasoning, "reasoning"),
            (pinned.thinking, "thinking"),
            (pinned.vision, "vision"),
            (pinned.structured_output, "structured_output"),
            (pinned.embeddings, "embeddings"),
            (pinned.streaming, "streaming"),
        ] {
            if flag && !caps.iter().any(|c| c == name) {
                caps.push(name.to_string());
            }
        }
        let validation = faktor_router::RouteRequest {
            phase: req.phase,
            required_capabilities: caps,
            context_tokens: req.context_tokens,
            estimated_output_tokens: req.estimated_output_tokens,
            quality_floor: pin_quality,
            task_budget_remaining_micro: req.task_budget_remaining_micro,
            latency_preference_ms: Some(pinned.economics.estimated_latency_ms),
            task_class: req.task_class,
            risk_bucket: req.risk_bucket,
        };
        let decision = match self
            .service
            .route_pinned_decision(provider, model, &validation, &[])
        {
            Ok(d) => Some(d),
            Err(e) => {
                // The pinned service runs ONLY the router's pinned
                // qualification ([`RouterService::qualify_specific`]), whose
                // failure text names the axis that refused the pin:
                // capability/fit and quality-floor denials are typed
                // NoCapableModel refusals, a hard-cap denial is
                // BudgetExceeded. There is deliberately no per-token
                // fallback estimate here — money rides the exact quote the
                // service evaluates.
                return Err(self.map_denial(&e, req));
            }
        };
        let decision = decision.expect("routed above");
        if decision.provider == provider && decision.model == model {
            Ok(decision)
        } else {
            Err(RouteFailure::PolicyDenied)
        }
    }
}

impl RoutingPolicy for EconomicRoutingPolicy {
    fn route(&self, req: &faktor_router::RouteRequest) -> Result<RouteDecision, RouteFailure> {
        match &self.mode {
            RoutingMode::Economy => self.route_economy(req, None),
            RoutingMode::MaximumQuality => self.route_maximum_quality(req, None),
            RoutingMode::Balanced => self.route_balanced(req, None),
            RoutingMode::Pinned { provider, model } => {
                // An empty side of the pin means "the session's own
                // configured side": nothing to validate against the pin's
                // descriptor — fall back to the session defaults (the
                // runtime treats the empty provider/model as passthrough).
                if provider.is_empty() && model.is_empty() {
                    return Ok(empty_passthrough_decision());
                }
                self.route_pinned(req, provider, model)
            }
        }
    }

    fn mode(&self) -> RoutingMode {
        self.mode.clone()
    }

    /// Cache economics in production routing (P0-82/15): the session's
    /// stored prefix stability feeds the router's stability consult — a
    /// session whose last recorded stability sits below the floor is
    /// priced without provider-side cache-read discounts and its decision
    /// carries the churn premium (the cost prediction must never pretend a
    /// churning prefix hits provider caches). `None`/empty history — no
    /// rows, or a stability read that failed — routes exactly like
    /// [`RoutingPolicy::route`]: no penalty, never an error on the turn.
    fn route_with_session_stability(
        &self,
        req: &faktor_router::RouteRequest,
        prefix_history: Option<&[TurnPrefix]>,
    ) -> Result<RouteDecision, RouteFailure> {
        match &self.mode {
            RoutingMode::Economy => self.route_economy(req, prefix_history),
            RoutingMode::MaximumQuality => self.route_maximum_quality(req, prefix_history),
            RoutingMode::Balanced => self.route_balanced(req, prefix_history),
            RoutingMode::Pinned { provider, model } => {
                if provider.is_empty() && model.is_empty() {
                    return Ok(empty_passthrough_decision());
                }
                let mut decision = self.route_pinned(req, provider, model)?;
                // The pinned path validates through the router without the
                // stability premium (the pin is not selectable anyway);
                // the decision's COST still reflects churn so downstream
                // budget math never pretends a churning prefix caches.
                if let Some(history) = prefix_history {
                    if let Some(last) = faktor_router::stability::turn_stabilities(history).pop() {
                        let penalty = faktor_router::stability::churn_penalty(
                            last,
                            faktor_router::stability::DEFAULT_STABILITY_FLOOR,
                        );
                        if penalty > 0.0 {
                            decision.estimated_cost_micro =
                                faktor_router::stability::apply_churn_penalty(
                                    decision.estimated_cost_micro,
                                    last,
                                    faktor_router::stability::DEFAULT_STABILITY_FLOOR,
                                );
                            decision.reasoning = format!(
                                "{} prefix_stability={last:.3} churn_penalty={penalty:.4}",
                                decision.reasoning
                            );
                        }
                    }
                }
                Ok(decision)
            }
        }
    }

    /// Candidate-sized production consult (candidate-specific accounting
    /// audit): the SAME mode semantics as
    /// [`RoutingPolicy::route_with_session_stability`] — Economy/ Balanced
    /// floors, MaximumQuality tier probe, Pinned validation, churn premium —
    /// but the router sizes each seriously-considered candidate under its
    /// OWN tokenizer through `planner` and the winning wire plan crosses
    /// back inside the returned [`faktor_router::SizedRouteDecision`]. A
    /// pinned (or empty-pin passthrough) consult is unsized: the pin never
    /// competes, so no candidate plan is consumed.
    fn route_with_session_stability_and_candidate_plans(
        &self,
        req: &faktor_router::RouteRequest,
        prefix_history: Option<&[TurnPrefix]>,
        planner: &dyn faktor_router::CandidatePlanner,
    ) -> Result<faktor_router::SizedRouteDecision, RouteFailure> {
        match &self.mode {
            RoutingMode::Economy => self.route_economy_sized(req, prefix_history, Some(planner)),
            RoutingMode::MaximumQuality => {
                self.route_maximum_quality_sized(req, prefix_history, Some(planner))
            }
            RoutingMode::Balanced => self.route_balanced_sized(req, prefix_history, Some(planner)),
            RoutingMode::Pinned { .. } => self
                .route_with_session_stability(req, prefix_history)
                .map(faktor_router::SizedRouteDecision::without_plan),
        }
    }

    /// Telemetry outcome record entry (P0-28): every settled model call the
    /// runtime reports lands in the wrapped RouterService's reliability
    /// priors and rate-limit cooldowns (see
    /// `RouterService::record_outcome`).
    ///
    /// Verified-outcome learning (audit items 13/14/L): when the outcome
    /// carries the explicit verified attribution (the deterministic gate
    /// sites only), the sample ALSO lands in the service's verified-outcome
    /// registry (`RouterService::outcomes` — the store-backed impl the
    /// daemon graph wires through `with_pricing_and_outcomes`, an
    /// [`faktor_router::EmptyOutcomeStore`] anywhere else), keyed by the
    /// FULL (provider, model, phase, task_class, risk_bucket) key. Telemetry
    /// always records; the verified sample records only on the explicit
    /// signal — a `None` attribution never learns a success it cannot prove.
    fn record_call_outcome(&self, outcome: &SettledCallOutcome) {
        self.service.record_outcome(
            &outcome.provider,
            &outcome.model,
            outcome.phase,
            outcome.success,
            outcome.retried,
            outcome.rate_limited,
            outcome.latency_ms,
        );
        if let Some(v) = &outcome.verified {
            self.service.outcomes.append_sample(
                &faktor_router::OutcomeKey {
                    provider: outcome.provider.clone(),
                    model: outcome.model.clone(),
                    phase: outcome.phase,
                    task_class: v.task_class,
                    risk_bucket: v.risk_bucket,
                },
                faktor_router::OutcomeSample {
                    verified_success: v.verified_success,
                    rework_cost_micro: v.rework_cost_micro,
                    rework_turns: v.rework_turns,
                },
            );
        }
    }
}

/// The store-backed verified-outcome registry (audit items 13/14/L): the
/// durable [`faktor_router::OutcomeStore`] impl over the store crate's v18
/// `model_outcome_stats` projection (append + per-key get + per-phase fold).
/// Wiring builds the daemon's RouterService through
/// `faktor_router::RouterService::with_pricing_and_outcomes` with this impl
/// over the daemon store, so verified samples recorded by the policy's
/// `record_call_outcome` survive restarts and serve every later route.
///
/// Best-effort by contract: a sample that cannot be appended (corrupt row,
/// IO failure) is logged, never an error on the turn — routing simply keeps
/// the conservative no-history estimate for that key. Reads that fail
/// consult as a miss (no history), which is the same conservative shape.
#[derive(Debug, Clone)]
pub struct StoreOutcomeStore {
    store: Arc<faktor_store::Store>,
}

impl StoreOutcomeStore {
    pub fn new(store: Arc<faktor_store::Store>) -> Self {
        Self { store }
    }
}

impl faktor_router::OutcomeStore for StoreOutcomeStore {
    fn append_sample(&self, key: &faktor_router::OutcomeKey, sample: faktor_router::OutcomeSample) {
        if let Err(e) = self.store.model_outcome_stats_append(
            &key.provider,
            &key.model,
            key.phase,
            key.task_class,
            key.risk_bucket,
            faktor_store::ModelOutcomeSample {
                verified_success: sample.verified_success,
                rework_cost_micro: sample.rework_cost_micro,
                rework_turns: sample.rework_turns,
            },
        ) {
            tracing::warn!(
                provider = %key.provider,
                model = %key.model,
                "verified-outcome sample append failed: {e}"
            );
        }
    }

    fn stats(
        &self,
        key: &faktor_router::OutcomeKey,
    ) -> Option<faktor_router::VerifiedOutcomeStats> {
        match self.store.model_outcome_stats_get(
            &key.provider,
            &key.model,
            key.phase,
            key.task_class,
            key.risk_bucket,
        ) {
            Ok(Some(row)) => Some(outcome_stats_from_store(&row)),
            _ => None,
        }
    }

    fn phase_stats(
        &self,
        provider: &str,
        model: &str,
        phase: RouterPhase,
    ) -> Option<faktor_router::VerifiedOutcomeStats> {
        match self.store.model_outcome_stats_phase(provider, model, phase) {
            Ok(Some(row)) => Some(outcome_stats_from_store(&row)),
            _ => None,
        }
    }
}

/// Project one durable per-key accumulator row onto the router registry's
/// stats shape (the store's read path already validated the row's
/// consistency invariants fallibly).
fn outcome_stats_from_store(
    row: &faktor_store::ModelOutcomeStatsRow,
) -> faktor_router::VerifiedOutcomeStats {
    faktor_router::VerifiedOutcomeStats {
        successes_first_pass: row.successes_first_pass,
        failures_first_pass: row.failures_first_pass,
        rework_cost_micro_sum: row.rework_cost_micro_sum,
        rework_turns_sum: row.rework_turns_sum,
        sample_count: row.sample_count,
    }
}

/// The decision of a policy that defers to the session's configured
/// provider/model (the test graph's passthrough pin). Empty strings are the
/// documented "session defaults" marker consumed by the runtime.
pub fn empty_passthrough_decision() -> RouteDecision {
    RouteDecision {
        provider: String::new(),
        model: String::new(),
        estimated_cost_micro: 0,
        estimated_latency_ms: 0,
        reasoning: "routing policy defers to the session-configured provider/model".into(),
        considered: 0,
        source: faktor_core::model::ModelSource::ConservativeDefault,
        // P0-1: NO pricing authority was consulted for a passthrough — the
        // snapshot stays None (never a fabricated price), so an unpriced
        // decision under a hard task cost budget fails closed at settle and
        // records Unknown spend without one.
        pricing_snapshot: None,
    }
}

/// Deterministic test policy: returns a fixed decision (or a fixed
/// failure) for every request. `passthrough()` returns the empty-decision
/// marker (session defaults win); `pinned(decision)` forces one decision;
/// `failing(failure)` exercises the runtime's fail-closed matrix.
#[derive(Debug, Clone)]
pub struct FixedRoutingPolicy {
    pub decision: RouteDecision,
    pub fail: Option<RouteFailure>,
}

impl FixedRoutingPolicy {
    pub fn passthrough() -> Arc<dyn RoutingPolicy> {
        Arc::new(Self {
            decision: empty_passthrough_decision(),
            fail: None,
        })
    }

    pub fn pinned(decision: RouteDecision) -> Arc<dyn RoutingPolicy> {
        Arc::new(Self {
            decision,
            fail: None,
        })
    }

    pub fn failing(failure: RouteFailure) -> Arc<dyn RoutingPolicy> {
        Arc::new(Self {
            decision: empty_passthrough_decision(),
            fail: Some(failure),
        })
    }
}

impl RoutingPolicy for FixedRoutingPolicy {
    fn route(&self, _req: &faktor_router::RouteRequest) -> Result<RouteDecision, RouteFailure> {
        match &self.fail {
            Some(f) => Err(f.clone()),
            None => Ok(self.decision.clone()),
        }
    }

    fn mode(&self) -> RoutingMode {
        if self.fail.is_some() {
            return RoutingMode::Economy;
        }
        RoutingMode::Pinned {
            provider: self.decision.provider.clone(),
            model: self.decision.model.clone(),
        }
    }
}

/// One PHYSICAL attempt's budget machine (attempt-accounting audit): owns
/// one reservation through its whole lifecycle and GUARDS the state
/// ordering so refund/uncertain/settle calls cannot be scattered wrongly
/// again:
///
/// ```text
///               reserve (outside the machine)
///                    |
///                    v
///           RESERVED --mark_dispatched--> DISPATCHED --clean end--> settle(usage)
///              |                              |
///    fail_before_dispatch (=refund)   fail_after_dispatch (=mark_uncertain)
///              |                    (error / stall / cancel-after-dispatch)
///              v                              v
///           REFUNDED                       UNCERTAIN (keeps consuming until
///                                         reconcile / task-end finalize)
/// ```
///
/// Every transition is a local guard PLUS the durable ledger call: a refund
/// after dispatch is refused locally with the ledger's own typed
/// [`faktor_session::BudgetError::CannotRefundDispatched`] BEFORE any
/// authority call (the durable SQL guard stays the backstop), an
/// uncertain/after-dispatch call before dispatch is refused as
/// [`faktor_session::BudgetError::NotOpen`] (a never-dispatched failure
/// REFUNDS — it never marks UNCERTAIN), and settle/refund/uncertain on a
/// closed machine are refused — money moves exactly once per reservation.
///
/// The machine is per PHYSICAL ATTEMPT: a retry builds a NEW machine over a
/// fresh reservation (and a fresh durable attempt op id); earlier
/// uncertain attempts stay uncertain until reconciled/finalized.
pub struct AttemptAccounting {
    budgets: Arc<dyn faktor_session::BudgetAuthority>,
    session_id: SessionId,
    reservation: Option<faktor_session::ReservationId>,
    dispatched: bool,
    closed: bool,
}

impl std::fmt::Debug for AttemptAccounting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttemptAccounting")
            .field("session_id", &self.session_id)
            .field("reservation", &self.reservation)
            .field("dispatched", &self.dispatched)
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

impl AttemptAccounting {
    /// Adopt one freshly reserved reservation (see
    /// [`faktor_session::BudgetAuthority::reserve_attempt`]). `None` = no
    /// reservation (an unbudgeted call): every machine call is a silent
    /// no-op — there is no money to move.
    pub fn new(
        budgets: Arc<dyn faktor_session::BudgetAuthority>,
        session_id: SessionId,
        reservation: Option<faktor_session::ReservationId>,
    ) -> Self {
        Self {
            budgets,
            session_id,
            reservation,
            dispatched: false,
            closed: false,
        }
    }

    pub fn reservation(&self) -> Option<faktor_session::ReservationId> {
        self.reservation
    }

    /// True once the durable dispatch marker was written (the provider
    /// request left the process and may have billed).
    pub fn dispatched(&self) -> bool {
        self.dispatched
    }

    /// True once the machine reached a terminal state (settled, refunded or
    /// marked uncertain): money moved exactly once.
    pub fn closed(&self) -> bool {
        self.closed
    }

    /// The machine still holds an open reservation.
    pub fn is_open(&self) -> bool {
        !self.closed
    }

    fn guard_not_closed(&self) -> Result<(), faktor_session::BudgetError> {
        if self.closed {
            let raw = self.reservation.map(|r| r.raw()).unwrap_or(0);
            return Err(faktor_session::BudgetError::NotOpen {
                reservation: raw,
                status: "settled/refunded/uncertain".into(),
            });
        }
        Ok(())
    }

    /// RESERVED -> DISPATCHED: write the durable dispatch marker immediately
    /// BEFORE the provider request is sent. Idempotent for the machine (the
    /// ledger makes the row-level marker idempotent too).
    pub async fn mark_dispatched(&mut self) -> Result<(), faktor_session::BudgetError> {
        self.guard_not_closed()?;
        let Some(reservation) = self.reservation else {
            return Ok(());
        };
        self.budgets
            .mark_dispatched(self.session_id, reservation)
            .await?;
        self.dispatched = true;
        Ok(())
    }

    /// DISPATCHED -> SETTLED at the usage actual (clean stream end: usage
    /// frame + Done). Guarded: only a dispatched, open machine settles.
    pub async fn settle_usage(
        &mut self,
        uncached_input_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        output_tokens: u64,
        provider_reported_micro: Option<u64>,
        route_decision_json: Option<String>,
    ) -> Result<Option<u64>, faktor_session::BudgetError> {
        self.guard_not_closed()?;
        if !self.dispatched {
            // A stream that never dispatched cannot settle: a clean end
            // without a dispatch marker is a machine-order violation (the
            // durable marker must precede the request).
            let raw = self.reservation.map(|r| r.raw()).unwrap_or(0);
            return Err(faktor_session::BudgetError::NotOpen {
                reservation: raw,
                status: "reserved".into(),
            });
        }
        let Some(reservation) = self.reservation else {
            return Ok(None);
        };
        let out = self
            .budgets
            .settle_usage(
                self.session_id,
                reservation,
                uncached_input_tokens,
                cache_read_tokens,
                cache_write_tokens,
                output_tokens,
                provider_reported_micro,
                route_decision_json,
            )
            .await?;
        self.closed = true;
        Ok(out)
    }

    /// DISPATCHED -> UNCERTAIN: a POST-DISPATCH terminal failure (error,
    /// stall verdict, cancel-after-dispatch, a settle that the ledger
    /// refused). The reserved amount KEEPS consuming the free budget until a
    /// reconcile settles the attempt's exact usage or the task-end finalize
    /// charges the estimate — never a silent $0 and never a dangling
    /// dispatched row. Guarded: only a dispatched, open machine may go
    /// UNCERTAIN; a never-dispatched failure REFUNDS instead.
    pub async fn fail_after_dispatch(
        &mut self,
        reason_code: impl Into<String>,
        request_id: Option<String>,
    ) -> Result<(), faktor_session::BudgetError> {
        self.guard_not_closed()?;
        if !self.dispatched {
            let raw = self.reservation.map(|r| r.raw()).unwrap_or(0);
            return Err(faktor_session::BudgetError::NotOpen {
                reservation: raw,
                status: "reserved".into(),
            });
        }
        let Some(reservation) = self.reservation else {
            return Ok(());
        };
        self.budgets
            .mark_uncertain(self.session_id, reservation, reason_code.into(), request_id)
            .await?;
        self.closed = true;
        Ok(())
    }

    /// RESERVED -> REFUNDED: a definitely-not-sent failure (pre-dispatch
    /// only). Guarded: a refund after the dispatch marker is refused with
    /// [`faktor_session::BudgetError::CannotRefundDispatched`] BEFORE the
    /// authority is reached — the provider may have billed, so the caller
    /// must settle or mark UNCERTAIN instead.
    pub async fn fail_before_dispatch(&mut self) -> Result<(), faktor_session::BudgetError> {
        self.guard_not_closed()?;
        if self.dispatched {
            let raw = self.reservation.map(|r| r.raw()).unwrap_or(0);
            return Err(faktor_session::BudgetError::CannotRefundDispatched { reservation: raw });
        }
        let Some(reservation) = self.reservation else {
            // No reservation: nothing to release, but the machine still
            // reached its terminal pre-dispatch state.
            self.closed = true;
            return Ok(());
        };
        self.budgets.refund(self.session_id, reservation).await?;
        self.closed = true;
        Ok(())
    }
}

// --------------------------------------------------------------------------
// Off-turn blocking bridge + the bounded evidence executor (audit 14/26; P1
// unbounded-thread-leak fix): the drive awaits evidence under hard wall
// budgets, but a provider that blocks inside its poll must never occupy a
// turn thread, and the runtime.rs verification-path source probes forbid the
// blocking-pool API name there. Both helpers live here.
//
// The advisory poll runs on a FIXED pool of long-lived, owned worker threads
// fed by a bounded queue — never one OS thread per poll, which leaked one
// live thread per timed-out poll. Saturation is a typed `NotSpawned`
// refusal, and every worker is cancelled and joined on shutdown/Drop.
// --------------------------------------------------------------------------

use faktor_core::cancellation::CancellationToken;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Why an off-turn blocking task produced no value: the TYPED outcome of
/// [`run_off_turn_thread_outcome`]. A panic (`JoinError::is_panic`) and a
/// scheduling refusal are distinct — the old `.ok()` collapsed both, plus a
/// normal `None`, into one indistinguishable fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OffTurnThreadFailure {
    /// The blocking pool could not schedule the task (runtime shutting
    /// down / refusing work).
    Unscheduled { message: String },
    /// The task panicked; the bounded panic payload message is carried.
    Panicked { message: String },
}

/// Run `f` on a blocking-pool thread and return its TYPED result: a normal
/// value, a panic (the `JoinError` panic message is surfaced) or a
/// scheduling refusal. Used to isolate the synchronous cold-evidence ladder
/// (file reads + supervised git/rg children) from the turn thread. This is
/// the ONLY off-turn bridge: the legacy `Option`-shaped wrapper that
/// collapsed panics/refusals into `None` was deleted (P2), and every
/// production caller maps its typed failure into an explicit
/// [`EvidencePollStatus`] degradation.
pub(crate) async fn run_off_turn_thread_outcome<T, F>(f: F) -> Result<T, OffTurnThreadFailure>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(value) => Ok(value),
        Err(join) if join.is_panic() => {
            let payload = join.into_panic();
            Err(OffTurnThreadFailure::Panicked {
                message: poll_panic_message(payload.as_ref()),
            })
        }
        Err(join) => Err(OffTurnThreadFailure::Unscheduled {
            message: bounded_poll_message(&join.to_string()),
        }),
    }
}

/// Map one TYPED off-turn failure into an explicit evidence degradation
/// (never `None`, never "no evidence"): a panic becomes
/// [`EvidencePollStatus::ProviderPanicked`], a scheduling refusal becomes
/// the typed [`EvidencePollStatus::NotSpawned`] refusal. Both are
/// degradations (`is_degraded`), so a caller can never mistake a broken
/// cold ladder for an honest empty answer.
pub(crate) fn evidence_status_from_off_turn_failure(
    failure: OffTurnThreadFailure,
) -> EvidencePollStatus {
    match failure {
        OffTurnThreadFailure::Panicked { message } => {
            EvidencePollStatus::ProviderPanicked { message }
        }
        OffTurnThreadFailure::Unscheduled { message } => EvidencePollStatus::NotSpawned { message },
    }
}

/// Hard byte bound of one poll diagnostic message (a hostile provider can
/// answer an arbitrarily large error string; the diagnostic is truncated at
/// a char boundary, never stored or logged unbounded).
const EVIDENCE_POLL_MESSAGE_MAX_BYTES: usize = 512;

/// Typed outcome of one advisory evidence poll. `NoEvidence` is the
/// provider's honest "nothing matched"; every other variant that is not
/// `Served` is an EXPLICIT degradation ("retrieval failed") that the
/// advisory path logs structurally and callers can distinguish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EvidencePollStatus {
    /// The provider answered under budget with a non-empty package.
    Served,
    /// The provider answered successfully with an empty package: an honest
    /// "no evidence matched", NOT a retrieval failure.
    NoEvidence,
    /// The provider returned a typed error under budget.
    RetrievalFailed {
        /// Provider code when the provider surfaced one, else the error
        /// kind name.
        code: String,
        /// Whether the provider marked the failure retryable.
        retryable: bool,
        /// Bounded provider message.
        message: String,
    },
    /// The provider future panicked, or the poll thread died before it
    /// could answer.
    ProviderPanicked { message: String },
    /// The wall budget fired before the provider answered.
    TimedOut { budget_ms: u64 },
    /// The poll could not be STARTED: the bounded executor refused admission
    /// (every worker busy and its bounded queue full, or the executor is
    /// shut down), or no worker thread could be spawned. Explicit, never a
    /// silently spawned thread.
    NotSpawned { message: String },
    /// The executor's CIRCUIT is OPEN (DEGRADED): a stuck worker NEEDS
    /// abandonment (its quarantine grace expired) but the runtime
    /// abandonment budget ([`EVIDENCE_EXECUTOR_MAX_ABANDONED_THREADS`]) is
    /// exhausted, so its logical slot cannot be reclaimed. Healthy
    /// replacement workers keep serving; this poll is refused because it
    /// could not be admitted (bounded queue full) — typed and distinct from
    /// a plain saturation [`Self::NotSpawned`]. The circuit closes again
    /// when the stuck provider returns or an abandoned provider thread
    /// drains.
    CircuitOpen {
        /// Physical worker threads currently abandoned by RUNTIME retirement
        /// (blocked inside non-yielding providers).
        abandoned: usize,
        /// Absolute documented cap on RUNTIME-abandoned physical threads.
        cap: usize,
        /// Bounded human diagnostic naming the blocked stuck worker(s).
        message: String,
    },
}

impl EvidencePollStatus {
    /// True when the poll degraded for a reason OTHER than the provider
    /// honestly reporting no matches. An empty package with a degraded
    /// status is "retrieval failed"; an empty package with `NoEvidence` is
    /// "nothing matched" — the advisory path never conflates them.
    pub(crate) fn is_degraded(&self) -> bool {
        !matches!(self, Self::Served | Self::NoEvidence)
    }
}

/// One poll's result: the possibly-empty advisory package plus WHY it is
/// what it is (see [`EvidencePollStatus`]).
#[derive(Debug)]
pub(crate) struct EvidencePollOutcome {
    pub evidence: Vec<faktor_context::assembler::Evidence>,
    pub status: EvidencePollStatus,
}

/// Truncate one diagnostic to [`EVIDENCE_POLL_MESSAGE_MAX_BYTES`] on a char
/// boundary.
fn bounded_poll_message(message: &str) -> String {
    if message.len() <= EVIDENCE_POLL_MESSAGE_MAX_BYTES {
        return message.to_string();
    }
    let mut end = EVIDENCE_POLL_MESSAGE_MAX_BYTES;
    while end > 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    message[..end].to_string()
}

/// Map one provider error into the typed status, preserving the provider
/// code/retryability when the error carries it.
fn status_from_provider_error(error: faktor_core::Error) -> EvidencePollStatus {
    let (code, retryable) = match &error.kind {
        faktor_core::error::ErrorKind::Provider { code, retryable } => (code.clone(), *retryable),
        kind => (format!("{kind:?}"), error.retryable),
    };
    EvidencePollStatus::RetrievalFailed {
        code,
        retryable,
        message: bounded_poll_message(&error.message),
    }
}

/// Extract a bounded message from a caught panic payload.
fn poll_panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        bounded_poll_message(s)
    } else if let Some(s) = payload.downcast_ref::<String>() {
        bounded_poll_message(s)
    } else {
        "evidence provider panicked".to_string()
    }
}

// Test-observable per-THREAD count of emitted degraded-poll diagnostics.
// The tracing sink is process-global and cannot be captured
// deterministically under parallel tests, while diagnostics are emitted on
// the polling caller's thread (the executor's worker never logs itself):
// a thread-local counter makes the "loud, never silent" contract
// assertable without a lock or a global subscriber.
#[cfg(test)]
thread_local! {
    pub(crate) static EVIDENCE_POLL_DEGRADED_DIAGNOSTICS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn evidence_poll_degraded_diagnostics() -> usize {
    EVIDENCE_POLL_DEGRADED_DIAGNOSTICS.with(|count| count.get())
}

/// Emit the structured diagnostic of one degraded poll. Loud by contract:
/// the advisory path may degrade, but "retrieval failed" is NEVER silent.
/// Owned by the TEST-ONLY legacy wrapper: production callers
/// ([`poll_evidence_with_wall_budget_outcome`]) own their diagnostic.
#[cfg(test)]
fn log_evidence_poll_degrade(status: &EvidencePollStatus, budget: std::time::Duration) {
    #[cfg(test)]
    EVIDENCE_POLL_DEGRADED_DIAGNOSTICS.with(|count| count.set(count.get() + 1));
    let budget_ms = budget.as_millis().min(u64::MAX as u128) as u64;
    match status {
        EvidencePollStatus::Served | EvidencePollStatus::NoEvidence => {}
        EvidencePollStatus::RetrievalFailed {
            code,
            retryable,
            message,
        } => {
            tracing::error!(
                target: "faktor_agent::evidence",
                status = "retrieval_failed",
                provider_code = %code,
                retryable,
                budget_ms,
                "advisory evidence poll failed: {} (advisory contract: the turn continues with an empty package, never silently)",
                message
            );
        }
        EvidencePollStatus::ProviderPanicked { message } => {
            tracing::error!(
                target: "faktor_agent::evidence",
                status = "provider_panicked",
                budget_ms,
                "advisory evidence poll panicked: {message} (advisory contract: the turn continues with an empty package, never silently)"
            );
        }
        EvidencePollStatus::TimedOut { budget_ms } => {
            tracing::warn!(
                target: "faktor_agent::evidence",
                status = "timed_out",
                budget_ms,
                "advisory evidence poll missed its wall budget (advisory contract: the turn continues with an empty package)"
            );
        }
        EvidencePollStatus::NotSpawned { message } => {
            tracing::error!(
                target: "faktor_agent::evidence",
                status = "not_spawned",
                budget_ms,
                "advisory evidence poll could not start on its bounded worker: {message} (advisory contract: the turn continues with an empty package)"
            );
        }
        EvidencePollStatus::CircuitOpen {
            abandoned,
            cap,
            message,
        } => {
            tracing::error!(
                target: "faktor_agent::evidence",
                status = "circuit_open",
                abandoned,
                cap,
                budget_ms,
                "advisory evidence poll refused: the evidence executor circuit is OPEN (degraded: {abandoned}/{cap} physical worker threads abandoned by runtime retirement and a stuck worker cannot be reclaimed): {message} (advisory contract: the turn continues with an empty package; the circuit closes when a stuck provider returns)"
            );
        }
    }
}

/// Fixed worker count of the process-wide advisory evidence executor: the
/// whole concurrency budget of the feature (the audit's 2..=4 bound). A
/// provider that never yields can occupy at most these workers; every
/// further poll is refused with a typed status instead of spawning a thread.
pub(crate) const EVIDENCE_EXECUTOR_WORKERS: usize = 4;

/// Bounded admission queue of the evidence executor: at most this many polls
/// wait for a worker at any instant. Combined with
/// [`EVIDENCE_EXECUTOR_WORKERS`], the executor holds at most
/// `workers + queue` polls; the next poll of a saturated executor is refused
/// (typed `NotSpawned`), never buffered without bound.
pub(crate) const EVIDENCE_EXECUTOR_QUEUE_CAPACITY: usize = 8;

/// Hard per-job RETIREMENT DEADLINE of the bounded evidence executor (P1
/// worker retirement): a worker whose CURRENT poll has been executing for
/// longer than this is QUARANTINED — it may be blocked inside a provider
/// that ignores cancellation. The quarantine is keyed to THAT job: a worker
/// that returns and starts different work clears the stale quarantine
/// immediately and the new job starts its own retirement clock. The
/// production value is deliberately generous (a healthy provider answers in
/// milliseconds), so ordinary slow polls are never retired; tests drive
/// short deadlines through [`EvidenceRetirementPolicy`].
pub(crate) const EVIDENCE_EXECUTOR_RETIREMENT_DEADLINE: Duration = Duration::from_secs(30);

/// QUARANTINE GRACE of the bounded evidence executor: how long a
/// quarantined worker is given to return on its own (a yielding future
/// cancelled by the caller's budget usually returns immediately) before its
/// physical thread is ABANDONED and its logical slot is replaced. The grace
/// clock belongs to the JOB that timed out, never to the worker id: a
/// different job on the same worker is never retired by an old clock. The
/// process is never killed and the request is never lost silently: the
/// caller already holds a typed `TimedOut`/`CircuitOpen` outcome.
pub(crate) const EVIDENCE_EXECUTOR_QUARANTINE_GRACE: Duration = Duration::from_secs(10);

/// ABSOLUTE cap on RUNTIME-abandoned physical evidence-worker threads per
/// executor (quarantine retirement after a provider ignored cancellation):
/// one replacement logical slot per abandoned worker, so an executor holds
/// at most `EVIDENCE_EXECUTOR_WORKERS + cap` physical evidence threads ever
/// (documented growth bound; never one thread per poll). Reaching the cap
/// does NOT stop the executor from serving: replacement logical slots are
/// restored and healthy workers keep taking polls. The circuit OPENS only
/// when a currently stuck worker NEEDS abandonment and this budget refuses
/// it — typed refusals for polls that cannot be admitted, until the stuck
/// provider returns or an abandoned thread drains. Shutdown abandonment
/// ([`EvidenceExecutorShutdownState`]) is a separate, non-runtime budget:
/// bounded by the fixed worker count and deliberately outside
/// `max_abandoned`.
pub(crate) const EVIDENCE_EXECUTOR_MAX_ABANDONED_THREADS: usize = EVIDENCE_EXECUTOR_WORKERS;

/// Retirement/quarantine policy of one executor. Production uses
/// [`EvidenceRetirementPolicy::DEFAULT`]; tests construct short deadlines so
/// a blocked provider is retired deterministically without waiting the
/// production bound out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EvidenceRetirementPolicy {
    /// Per-job wall bound before its worker is quarantined.
    pub retirement_deadline: Duration,
    /// Grace a quarantined worker gets to return before abandonment.
    pub quarantine_grace: Duration,
    /// Absolute cap on abandoned physical threads (see
    /// [`EVIDENCE_EXECUTOR_MAX_ABANDONED_THREADS`]).
    pub max_abandoned: usize,
}

impl EvidenceRetirementPolicy {
    pub(crate) const DEFAULT: Self = Self {
        retirement_deadline: EVIDENCE_EXECUTOR_RETIREMENT_DEADLINE,
        quarantine_grace: EVIDENCE_EXECUTOR_QUARANTINE_GRACE,
        max_abandoned: EVIDENCE_EXECUTOR_MAX_ABANDONED_THREADS,
    };
}

impl Default for EvidenceRetirementPolicy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Typed circuit state of the bounded evidence executor (observable in
/// [`EvidenceExecutorStats`] and through the typed
/// [`EvidencePollStatus::CircuitOpen`] refusal): `Closed` while every stuck
/// worker can still be abandoned (or none needs to be — healthy replacement
/// slots keep serving even AT the abandoned cap), `Open` once a currently
/// stuck worker NEEDS abandonment and the exhausted runtime abandonment
/// budget refuses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum EvidenceCircuitState {
    /// Every stuck worker can be abandoned (or none needs to be): polls are
    /// admitted normally.
    #[default]
    Closed,
    /// A quarantined stuck worker's grace expired while the runtime
    /// abandonment budget was exhausted: its logical slot cannot be
    /// reclaimed until its provider returns. Healthy replacement workers
    /// keep serving; polls that cannot be admitted are refused typed.
    Open {
        /// Physical threads currently abandoned by runtime retirement.
        abandoned: usize,
        /// Absolute cap ([`EVIDENCE_EXECUTOR_MAX_ABANDONED_THREADS`]).
        cap: usize,
        /// Stuck workers whose grace expired and whose abandonment the
        /// exhausted budget refuses (each is an unreclaimable logical slot).
        blocked: usize,
    },
}

/// Terminal disposition of the bounded evidence executor (P2): PERSISTED so
/// repeated [`EvidenceExecutor::shutdown`] calls return the SAME terminal
/// disposition — the old shape reported `false` on the first call and then
/// `true` on the next one merely because the abandoned join handles had been
/// forgotten, which lied about the abandoned workers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum EvidenceExecutorShutdownState {
    /// No shutdown has run yet.
    #[default]
    Running,
    /// Every owned worker exited and was joined within the bound.
    Clean,
    /// `workers` physical worker threads could not be joined (they block
    /// inside a non-yielding provider) and were abandoned.
    Abandoned {
        /// Number of abandoned physical worker threads at shutdown time.
        workers: usize,
    },
}

/// Bounded wait for the worker pool to drain and exit on shutdown/Drop
/// before the remaining (synchronously blocked) workers are abandoned. A
/// provider that blocks its OS thread inside one `poll` cannot be cancelled
/// from safe Rust, so the executor abandons at most the fixed worker count —
/// never one thread per poll. Cooperative providers (any future that
/// yields) are cancelled by token and exit immediately.
pub(crate) const EVIDENCE_EXECUTOR_JOIN_BOUND: Duration = Duration::from_secs(5);

/// Worker thread-name prefix (observable in crash forensics).
const EVIDENCE_WORKER_NAME: &str = "faktor-evidence";

/// Test-observable count of evidence worker threads ever spawned: a bounded
/// executor spawns its fixed workers once and never again, so the counter is
/// flat across any number of polls.
#[cfg(test)]
pub(crate) static EVIDENCE_WORKERS_SPAWNED: AtomicUsize = AtomicUsize::new(0);

/// Test-observable count of LIVE evidence worker threads (decremented when a
/// worker exits): the thread-leak gate.
#[cfg(test)]
pub(crate) static EVIDENCE_WORKERS_LIVE: AtomicUsize = AtomicUsize::new(0);

/// One poll admitted to the executor: the provider seam inputs, the reply
/// channel and the cooperative cancellation token the caller fires when its
/// wall budget expires (or the executor fires on shutdown).
struct EvidenceJob {
    id: u64,
    provider: Arc<dyn EvidenceProvider>,
    session: SessionId,
    query: EvidenceQuery,
    reply: tokio::sync::oneshot::Sender<WorkerReply>,
    cancel: CancellationToken,
    handle: tokio::runtime::Handle,
}

/// The provider outcome of one executed poll: the caught panic payload when
/// the provider panicked, else the typed provider result.
type ProviderPollResult =
    std::thread::Result<faktor_core::Result<Vec<faktor_context::assembler::Evidence>>>;

/// The bounded executor's reply: the provider outcome (with the caught panic
/// payload when it panicked) or an explicit cancellation (the caller's
/// budget fired before the provider was invoked, or shutdown intervened).
enum WorkerReply {
    Completed(ProviderPollResult),
    Cancelled,
}

/// Why one poll was refused admission to the bounded executor. Every refusal
/// maps to [`EvidencePollStatus::NotSpawned`] at the caller: an EXPLICIT
/// typed degradation, never a silently spawned thread.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SubmitRefusal {
    /// The executor is closed (shutdown/Drop): no new poll is accepted.
    Closed,
    /// Every worker is busy and the bounded queue is full.
    Saturated {
        workers: usize,
        capacity: usize,
        /// How many of the busy workers are QUARANTINED (blocked inside a
        /// non-yielding provider past the hard retirement deadline): named in
        /// the refusal so a saturation caused by stuck providers is
        /// distinguishable from plain load.
        quarantined: usize,
    },
    /// No owned worker slot exists (spawn failed at startup, or every
    /// logical worker was retired): admission gates on RUNNABLE OWNED slots,
    /// never on physical threads that survive only as detached/retired
    /// shells.
    NoWorkers,
    /// The executor is DEGRADED (open circuit): a stuck worker NEEDS
    /// abandonment (its grace expired) but the runtime abandonment budget is
    /// exhausted, and this poll could not be admitted (the bounded queue is
    /// full). Healthy replacement workers keep serving.
    CircuitOpen {
        abandoned: usize,
        cap: usize,
        /// Stuck workers the exhausted budget refuses to abandon.
        blocked: usize,
    },
}

impl SubmitRefusal {
    fn message(&self) -> String {
        match self {
            SubmitRefusal::Closed => {
                "evidence executor is closed (shutdown); poll refused".to_string()
            }
            SubmitRefusal::Saturated {
                workers,
                capacity,
                quarantined,
            } => format!(
                "evidence executor saturated: all {workers} poll workers are busy ({quarantined} quarantined inside non-yielding providers) and the bounded queue ({capacity}) is full; the poll was refused instead of spawning another thread"
            ),
            SubmitRefusal::NoWorkers => {
                "evidence executor has no runnable worker slots (every logical worker was retired or never spawned); poll refused".to_string()
            }
            SubmitRefusal::CircuitOpen {
                abandoned,
                cap,
                blocked,
            } => format!(
                "evidence executor circuit is OPEN (degraded): {blocked} stuck worker(s) need abandonment but the runtime abandonment budget is exhausted ({abandoned}/{cap} physical worker threads already abandoned) and the bounded queue is full; healthy replacement workers keep serving and the circuit closes when a stuck provider returns"
            ),
        }
    }
}

/// Atomic instrumentation of the executor (the bounded-growth gate: the
/// queue high-water can never exceed its capacity and the active high-water
/// can never exceed the worker count).
#[derive(Default)]
struct ExecutorStatsCore {
    enqueued: AtomicU64,
    completed: AtomicU64,
    refused: AtomicU64,
    cancelled: AtomicU64,
    active: AtomicU64,
    max_active: AtomicU64,
    max_queue_depth: AtomicU64,
}

/// Snapshot of the executor's instrumentation (observable by tests and by
/// the "no unbounded leak" gate).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct EvidenceExecutorStats {
    /// Logical worker slots (healthy + quarantined); never exceeds the
    /// configured worker count.
    pub workers: usize,
    /// Logical slots whose current poll is inside its retirement deadline.
    pub workers_healthy: usize,
    /// Logical slots QUARANTINED because their current poll exceeded the
    /// hard retirement deadline (possibly blocked inside a provider).
    pub workers_quarantined: usize,
    /// Physical worker threads currently ABANDONED by RUNTIME retirement
    /// (detached while blocked inside a non-yielding provider). Only runtime
    /// abandonment is subject to `max_abandoned`: this value NEVER exceeds
    /// it (the public invariant).
    pub runtime_abandoned: usize,
    /// Total physical threads ever abandoned by runtime retirement.
    pub runtime_abandoned_total: u64,
    /// Physical worker threads currently ABANDONED by SHUTDOWN (the bounded
    /// join elapsed; detached and still possibly alive). NOT subject to
    /// `max_abandoned`: bounded by the fixed worker count instead.
    pub shutdown_abandoned: usize,
    /// Total physical threads ever abandoned by shutdown.
    pub shutdown_abandoned_total: u64,
    /// Absolute cap on RUNTIME-abandoned physical threads. Shutdown
    /// abandonment is deliberately outside this budget.
    pub max_abandoned: usize,
    /// Typed circuit state: `Open` while a stuck worker NEEDS abandonment
    /// and the exhausted runtime budget refuses it (healthy replacements
    /// keep serving).
    pub circuit: EvidenceCircuitState,
    pub capacity: usize,
    pub enqueued: u64,
    pub completed: u64,
    pub refused: u64,
    pub cancelled: u64,
    /// High-water of concurrently executing polls; never exceeds `workers`.
    pub max_active: u64,
    /// High-water of queued polls; never exceeds `capacity`.
    pub max_queue_depth: u64,
}

/// Condvar-backed all-workers-exited signal, waitable with a timeout.
#[derive(Default)]
struct ExitFlag {
    state: Mutex<bool>,
    cv: Condvar,
}

impl ExitFlag {
    fn notify_exited(&self) {
        *self.state.lock().unwrap_or_else(|p| p.into_inner()) = true;
        self.cv.notify_all();
    }

    /// Clear the all-exited flag when a replacement worker is spawned (the
    /// pool may have transiently reached zero live threads when every
    /// abandoned worker drained before its replacement existed).
    fn reset(&self) {
        *self.state.lock().unwrap_or_else(|p| p.into_inner()) = false;
    }
}

/// One admitted poll currently executing on a worker: its cooperative
/// cancellation token, the identity of the worker that owns it and the
/// instant it started — the inputs of the retirement deadline.
struct ExecutingJob {
    cancel: CancellationToken,
    worker_id: u64,
    started: Instant,
}

/// One QUARANTINED worker: the identity of the JOB whose overdue poll
/// quarantined it and when that job's quarantine clock started. The entry is
/// valid ONLY while that SAME job is still executing on the worker: a worker
/// that returned and picked up different work clears the stale entry
/// immediately and the new job starts its OWN retirement clock (P1: the
/// quarantine is keyed by the job that timed out, never by the worker id
/// alone).
struct QuarantinedJob {
    job_id: u64,
    since: Instant,
}

/// Why one worker thread was RETIRED (it must exit as soon as its blocked
/// provider returns). The kind decides which abandoned counter its exit
/// releases and keeps the two budgets apart: runtime retirement is subject
/// to `max_abandoned`, shutdown abandonment is bounded by the fixed worker
/// count instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerRetirement {
    /// A normal owned logical slot (not retired).
    Active,
    /// Abandoned by runtime quarantine retirement (subject to the runtime
    /// abandonment cap).
    Runtime,
    /// Abandoned by shutdown after the bounded join elapsed (NOT subject to
    /// the runtime abandonment cap).
    Shutdown,
}

impl WorkerRetirement {
    fn bits(self) -> u8 {
        match self {
            Self::Active => 0,
            Self::Runtime => 1,
            Self::Shutdown => 2,
        }
    }

    fn store(self, flag: &AtomicU8) {
        flag.store(self.bits(), Ordering::SeqCst);
    }

    fn load(flag: &AtomicU8) -> Self {
        match flag.load(Ordering::SeqCst) {
            1 => Self::Runtime,
            2 => Self::Shutdown,
            _ => Self::Active,
        }
    }
}

/// Outcome of one runtime abandonment attempt (see
/// [`EvidenceExecutor::abandon_worker`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AbandonOutcome {
    /// The physical thread was retired and one runtime abandoned slot was
    /// reserved.
    Abandoned,
    /// The quarantine was stale (the job returned, or the worker is gone):
    /// nothing was abandoned.
    Stale,
    /// The runtime abandonment budget is exhausted: a stuck worker NEEDS
    /// abandonment and the circuit is open until a slot drains.
    CapReached,
}

/// State shared between the executor handle and its worker threads.
struct ExecutorShared {
    capacity: usize,
    /// Logical worker slots the executor restores after an abandonment.
    target_workers: usize,
    policy: EvidenceRetirementPolicy,
    queue: Mutex<VecDeque<EvidenceJob>>,
    wake: Condvar,
    closed: AtomicBool,
    executing: Mutex<HashMap<u64, ExecutingJob>>,
    next_id: AtomicU64,
    next_worker_id: AtomicU64,
    /// Live worker threads (logical slots + abandoned threads still alive).
    /// PHYSICAL-thread accounting only: admission gates on the owned slots
    /// in `workers`, never on this count.
    running: AtomicUsize,
    /// Worker id -> the job whose overdue poll QUARANTINED that worker. Live
    /// only while the SAME job is still executing on the worker.
    quarantined: Mutex<HashMap<u64, QuarantinedJob>>,
    /// Physical threads currently abandoned by RUNTIME retirement (detached;
    /// still possibly alive). Only runtime abandonment is subject to the
    /// absolute cap: never above `policy.max_abandoned`.
    runtime_abandoned_live: AtomicUsize,
    /// Total physical threads ever abandoned by runtime retirement.
    runtime_abandoned_total: AtomicU64,
    /// Physical threads currently abandoned by SHUTDOWN (the bounded join
    /// elapsed; detached and still possibly alive). NOT subject to
    /// `policy.max_abandoned`: bounded by the fixed worker count instead.
    shutdown_abandoned_live: AtomicUsize,
    /// Total physical threads ever abandoned by shutdown.
    shutdown_abandoned_total: AtomicU64,
    exit: ExitFlag,
    stats: ExecutorStatsCore,
}

/// One owned worker thread: its join handle (bounded join on shutdown/Drop),
/// the flag set just before it exits and the RETIREMENT kind set when the
/// executor abandons it — a retired worker never takes another poll and
/// exits the moment its blocked provider returns.
struct EvidenceWorker {
    id: u64,
    join: Option<std::thread::JoinHandle<()>>,
    exited: Arc<AtomicBool>,
    retirement: Arc<AtomicU8>,
}

/// The bounded, owned evidence executor: a fixed set of long-lived worker
/// threads fed by a bounded queue. There is NO per-poll thread spawn and NO
/// detached thread: an admission that cannot be served is refused with a
/// typed [`SubmitRefusal`], and shutdown/Drop cancels the workers and joins
/// them (bounded).
pub(crate) struct EvidenceExecutor {
    shared: Arc<ExecutorShared>,
    workers: Mutex<Vec<EvidenceWorker>>,
    /// Serializes retirement passes (quarantine/abandon/replace) so the
    /// absolute cap is never exceeded by concurrent admissions.
    maintain_lock: Mutex<()>,
    /// PERSISTED terminal disposition (see [`EvidenceExecutorShutdownState`]).
    shutdown_state: Mutex<EvidenceExecutorShutdownState>,
    /// TEST SEAM: when set, replacement spawning is refused — the OS
    /// refusing worker threads, which production can reach under thread
    /// exhaustion. Lets a test pin the admission gate against a pool whose
    /// logical slots are gone while detached physical threads survive.
    #[cfg(test)]
    restore_refused: AtomicBool,
}

impl EvidenceExecutor {
    /// Start an executor with `workers` owned threads and a queue of
    /// `capacity` polls, under the production retirement policy. Thread-spawn
    /// failures are logged loudly; the executor simply runs with fewer
    /// workers (zero workers refuses every poll with a typed refusal).
    pub(crate) fn start(workers: usize, capacity: usize) -> Arc<Self> {
        Self::start_with_policy(workers, capacity, EvidenceRetirementPolicy::DEFAULT)
    }

    /// [`Self::start`] with an explicit retirement policy (the test seam for
    /// short retirement deadlines and small abandonment caps).
    pub(crate) fn start_with_policy(
        workers: usize,
        capacity: usize,
        policy: EvidenceRetirementPolicy,
    ) -> Arc<Self> {
        // A zero cap would make the circuit permanently open (nothing could
        // ever be replaced); clamp to the smallest meaningful cap. The
        // production policy uses the documented absolute cap.
        let policy = EvidenceRetirementPolicy {
            max_abandoned: policy.max_abandoned.max(1),
            ..policy
        };
        let shared = Arc::new(ExecutorShared {
            capacity: capacity.max(1),
            target_workers: workers,
            policy,
            queue: Mutex::new(VecDeque::new()),
            wake: Condvar::new(),
            closed: AtomicBool::new(false),
            executing: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            next_worker_id: AtomicU64::new(0),
            running: AtomicUsize::new(0),
            quarantined: Mutex::new(HashMap::new()),
            runtime_abandoned_live: AtomicUsize::new(0),
            runtime_abandoned_total: AtomicU64::new(0),
            shutdown_abandoned_live: AtomicUsize::new(0),
            shutdown_abandoned_total: AtomicU64::new(0),
            exit: ExitFlag::default(),
            stats: ExecutorStatsCore::default(),
        });
        let mut spawned: Vec<EvidenceWorker> = Vec::with_capacity(workers);
        for _ in 0..workers {
            match spawn_worker(&shared) {
                Some(worker) => spawned.push(worker),
                None => {
                    // `spawn_worker` logged the typed spawn failure; the
                    // bounded pool runs with fewer workers.
                }
            }
        }
        // Production observability of the fixed size: the pool's worker count
        // and queue bound are visible in the daemon log (never per-poll).
        tracing::info!(
            target: "faktor_agent::evidence",
            workers = spawned.len(),
            requested_workers = workers,
            capacity = shared.capacity,
            retirement_deadline_ms = policy.retirement_deadline.as_millis() as u64,
            quarantine_grace_ms = policy.quarantine_grace.as_millis() as u64,
            max_abandoned = policy.max_abandoned,
            "evidence executor started: fixed bounded pool of owned workers fed by a bounded queue (saturation is a typed refusal, never another thread)"
        );
        Arc::new(Self {
            shared,
            workers: Mutex::new(spawned),
            maintain_lock: Mutex::new(()),
            shutdown_state: Mutex::new(EvidenceExecutorShutdownState::Running),
            #[cfg(test)]
            restore_refused: AtomicBool::new(false),
        })
    }

    /// Owned logical worker slots (healthy + quarantined); a replacement
    /// slot is spawned for every abandoned worker, so this returns to the
    /// configured count even at the abandoned cap. Admission gates on this
    /// count: it is the runnable-capacity authority, while
    /// [`ExecutorShared::running`] is physical-thread accounting only.
    pub(crate) fn worker_count(&self) -> usize {
        self.workers.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    /// Instrumentation snapshot, including the retirement counters and the
    /// typed circuit state (tests/health surface).
    pub(crate) fn stats(&self) -> EvidenceExecutorStats {
        let stats = &self.shared.stats;
        let workers = self.worker_count();
        let runtime_abandoned = self.shared.runtime_abandoned_live.load(Ordering::SeqCst);
        let shutdown_abandoned = self.shared.shutdown_abandoned_live.load(Ordering::SeqCst);
        let cap = self.shared.policy.max_abandoned;
        let now = Instant::now();
        let (quarantined_live, blocked) = {
            let quarantined = self
                .shared
                .quarantined
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let executing = self
                .shared
                .executing
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let live = |worker_id: u64, job: &QuarantinedJob| {
                executing
                    .get(&job.job_id)
                    .is_some_and(|executing_job| executing_job.worker_id == worker_id)
            };
            let quarantined_live = quarantined
                .iter()
                .filter(|(worker_id, job)| live(**worker_id, job))
                .count();
            // A stuck worker is BLOCKED (the open circuit) only while its
            // grace expired AND the runtime budget has no room to abandon
            // it; with room, the next maintain pass reclaims the slot.
            let blocked = if runtime_abandoned >= cap {
                quarantined
                    .iter()
                    .filter(|(worker_id, job)| {
                        live(**worker_id, job)
                            && now.saturating_duration_since(job.since)
                                >= self.shared.policy.quarantine_grace
                    })
                    .count()
            } else {
                0
            };
            (quarantined_live, blocked)
        };
        let circuit = if blocked > 0 {
            EvidenceCircuitState::Open {
                abandoned: runtime_abandoned,
                cap,
                blocked,
            }
        } else {
            EvidenceCircuitState::Closed
        };
        EvidenceExecutorStats {
            workers,
            workers_healthy: workers.saturating_sub(quarantined_live),
            workers_quarantined: quarantined_live,
            runtime_abandoned,
            runtime_abandoned_total: self.shared.runtime_abandoned_total.load(Ordering::SeqCst),
            shutdown_abandoned,
            shutdown_abandoned_total: self.shared.shutdown_abandoned_total.load(Ordering::SeqCst),
            max_abandoned: cap,
            circuit,
            capacity: self.shared.capacity,
            enqueued: stats.enqueued.load(Ordering::Relaxed),
            completed: stats.completed.load(Ordering::Relaxed),
            refused: stats.refused.load(Ordering::Relaxed),
            cancelled: stats.cancelled.load(Ordering::Relaxed),
            max_active: stats.max_active.load(Ordering::Relaxed),
            max_queue_depth: stats.max_queue_depth.load(Ordering::Relaxed),
        }
    }

    /// The PERSISTED terminal shutdown disposition: `Running` before the
    /// first [`Self::shutdown`], then the same terminal value on every later
    /// call.
    pub(crate) fn shutdown_state(&self) -> EvidenceExecutorShutdownState {
        *self
            .shutdown_state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
    }

    /// Admit one poll or refuse it. NEVER spawns a thread.
    fn submit(
        &self,
        provider: Arc<dyn EvidenceProvider>,
        session: SessionId,
        query: EvidenceQuery,
        cancel: CancellationToken,
        handle: tokio::runtime::Handle,
        reply: tokio::sync::oneshot::Sender<WorkerReply>,
    ) -> Result<(), SubmitRefusal> {
        // One retirement pass per admission: quarantined workers are
        // detected, expired grace abandons a bounded number of physical
        // threads and restores their replacement slots, and a recovered
        // abandoned thread closes the circuit again.
        self.maintain();
        let job = EvidenceJob {
            id: self.shared.next_id.fetch_add(1, Ordering::Relaxed),
            provider,
            session,
            query,
            reply,
            cancel,
            handle,
        };
        let mut queue = self.shared.queue.lock().unwrap_or_else(|p| p.into_inner());
        if self.shared.closed.load(Ordering::SeqCst) {
            self.shared.stats.refused.fetch_add(1, Ordering::Relaxed);
            return Err(SubmitRefusal::Closed);
        }
        // Admission gates on the OWNED logical slots that can run work, NOT
        // on `running` (physical threads, including detached/retired
        // shells): a pool whose logical slots are all gone must refuse typed
        // even while detached threads are still alive, otherwise the poll
        // would only wait for the caller's budget to fire.
        let owned_slots = self.workers.lock().unwrap_or_else(|p| p.into_inner()).len();
        if owned_slots == 0 {
            self.shared.stats.refused.fetch_add(1, Ordering::Relaxed);
            return Err(SubmitRefusal::NoWorkers);
        }
        if queue.len() >= self.shared.capacity {
            // Reclaim capacity held by polls whose callers already gave up:
            // a cancelled queued poll can never produce an answer, so it
            // must not block a live one.
            let mut kept = VecDeque::with_capacity(queue.len());
            let mut reclaimed = 0u64;
            while let Some(stale) = queue.pop_front() {
                if stale.cancel.is_cancelled() {
                    let _ = stale.reply.send(WorkerReply::Cancelled);
                    reclaimed += 1;
                } else {
                    kept.push_back(stale);
                }
            }
            *queue = kept;
            if reclaimed > 0 {
                self.shared
                    .stats
                    .cancelled
                    .fetch_add(reclaimed, Ordering::Relaxed);
            }
        }
        if queue.len() >= self.shared.capacity {
            self.shared.stats.refused.fetch_add(1, Ordering::Relaxed);
            let blocked = self.blocked_abandonments();
            if blocked > 0 {
                // DEGRADED: a stuck worker needs abandonment the exhausted
                // runtime budget refuses, and this poll could not be
                // admitted. Healthy replacements keep serving; the circuit
                // closes when a stuck provider returns or a slot drains.
                return Err(SubmitRefusal::CircuitOpen {
                    abandoned: self.shared.runtime_abandoned_live.load(Ordering::SeqCst),
                    cap: self.shared.policy.max_abandoned,
                    blocked,
                });
            }
            return Err(SubmitRefusal::Saturated {
                workers: owned_slots,
                capacity: self.shared.capacity,
                quarantined: self.quarantined_live(),
            });
        }
        queue.push_back(job);
        self.shared.stats.enqueued.fetch_add(1, Ordering::Relaxed);
        self.shared
            .stats
            .max_queue_depth
            .fetch_max(queue.len() as u64, Ordering::Relaxed);
        drop(queue);
        self.shared.wake.notify_one();
        Ok(())
    }

    /// Live QUARANTINED slots: entries whose SAME job is still executing on
    /// the worker (a returned job's entry is stale and never counted).
    fn quarantined_live(&self) -> usize {
        let quarantined = self
            .shared
            .quarantined
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let executing = self
            .shared
            .executing
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        quarantined
            .iter()
            .filter(|(worker_id, job)| {
                executing
                    .get(&job.job_id)
                    .is_some_and(|executing_job| executing_job.worker_id == **worker_id)
            })
            .count()
    }

    /// Quarantined stuck workers whose grace expired while the RUNTIME
    /// abandonment budget is exhausted: each is a logical slot the executor
    /// cannot reclaim until its provider returns (the open circuit). `0`
    /// when the budget has room (the next maintain pass abandons them) or no
    /// stuck worker is past its grace.
    fn blocked_abandonments(&self) -> usize {
        if self.shared.runtime_abandoned_live.load(Ordering::SeqCst)
            < self.shared.policy.max_abandoned
        {
            return 0;
        }
        let now = Instant::now();
        let grace = self.shared.policy.quarantine_grace;
        let quarantined = self
            .shared
            .quarantined
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let executing = self
            .shared
            .executing
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        quarantined
            .iter()
            .filter(|(worker_id, job)| {
                executing
                    .get(&job.job_id)
                    .is_some_and(|executing_job| executing_job.worker_id == **worker_id)
                    && now.saturating_duration_since(job.since) >= grace
            })
            .count()
    }

    /// One retirement pass (P1 worker retirement; called on every admission,
    /// so a wedged pool is maintained while requests keep arriving — also
    /// the test/health seam):
    ///
    /// 1. QUARANTINE every worker whose current poll exceeded the hard
    ///    retirement deadline (keyed to that job);
    /// 2. clear the quarantine of workers whose job returned or was
    ///    REPLACED by different work (the worker is healthy again — a
    ///    cooperative provider freed its thread);
    /// 3. ABANDON the physical threads whose grace expired (up to the
    ///    absolute runtime cap) and spawn their replacement logical slots;
    /// 4. restore capacity after abandoned threads actually exited.
    ///
    /// Never kills the process, never spawns unboundedly: the runtime cap is
    /// absolute (`target_workers + cap` physical threads) and reaching it
    /// only opens the circuit when a stuck worker actually NEEDS an
    /// abandonment the budget refuses (typed refusals for polls that cannot
    /// be admitted); healthy replacements keep serving.
    pub(crate) fn maintain(&self) {
        if self.shared.closed.load(Ordering::SeqCst) {
            return;
        }
        let _pass = self.maintain_lock.lock().unwrap_or_else(|p| p.into_inner());
        let now = Instant::now();
        let policy = self.shared.policy;
        // The OWNED logical slots: only they are quarantinable. A job still
        // running on an ABANDONED (detached) thread must never be
        // re-quarantined: the executor no longer owns that slot.
        let owned_workers: Vec<u64> = {
            let workers = self.workers.lock().unwrap_or_else(|p| p.into_inner());
            workers.iter().map(|worker| worker.id).collect()
        };
        let expired: Vec<(u64, u64)> = {
            let mut quarantined = self
                .shared
                .quarantined
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            {
                let executing = self
                    .shared
                    .executing
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                // A quarantine is live only while the SAME job is still
                // executing on that OWNED worker. A different job (the
                // quarantined one returned; the worker picked up healthy
                // work) clears the old entry IMMEDIATELY and starts its own
                // clock: a recovered worker's stale timestamp can never
                // retire the job that replaced it.
                quarantined.retain(|worker_id, job| {
                    owned_workers.contains(worker_id)
                        && executing
                            .get(&job.job_id)
                            .is_some_and(|executing_job| executing_job.worker_id == *worker_id)
                });
                for (job_id, job) in executing.iter() {
                    if now.saturating_duration_since(job.started) < policy.retirement_deadline {
                        continue;
                    }
                    if !owned_workers.contains(&job.worker_id) {
                        // Detached thread: not a logical slot any more.
                        continue;
                    }
                    let newly_quarantined = match quarantined.entry(job.worker_id) {
                        std::collections::hash_map::Entry::Occupied(mut occupied) => {
                            if occupied.get().job_id == *job_id {
                                false
                            } else {
                                // Different job on the same worker: its own
                                // retirement clock starts now.
                                occupied.insert(QuarantinedJob {
                                    job_id: *job_id,
                                    since: now,
                                });
                                true
                            }
                        }
                        std::collections::hash_map::Entry::Vacant(vacant) => {
                            vacant.insert(QuarantinedJob {
                                job_id: *job_id,
                                since: now,
                            });
                            true
                        }
                    };
                    if newly_quarantined {
                        tracing::warn!(
                            target: "faktor_agent::evidence",
                            worker_id = job.worker_id,
                            job_id = *job_id,
                            deadline_ms = policy.retirement_deadline.as_millis() as u64,
                            grace_ms = policy.quarantine_grace.as_millis() as u64,
                            "evidence executor quarantined a worker whose poll exceeded the hard retirement deadline (the provider may ignore cancellation); the quarantine belongs to THIS job, the logical slot is replaced after the grace expires and the physical thread is abandoned only up to the absolute runtime cap"
                        );
                    }
                }
            }
            quarantined
                .iter()
                .filter(|(_, job)| {
                    now.saturating_duration_since(job.since) >= policy.quarantine_grace
                })
                .map(|(worker_id, job)| (*worker_id, job.job_id))
                .collect()
        };
        for (worker_id, job_id) in expired {
            match self.abandon_worker(worker_id, job_id) {
                AbandonOutcome::Abandoned | AbandonOutcome::Stale => {}
                // The runtime budget is exhausted while a stuck worker
                // NEEDS abandonment: the circuit is open and no further
                // physical thread may be retired. Healthy replacements keep
                // serving.
                AbandonOutcome::CapReached => break,
            }
        }
        self.restore_capacity();
    }

    /// Abandon ONE physical worker thread (its provider ignores
    /// cancellation): mark it retired so it exits the moment it returns,
    /// detach its join handle and reserve one RUNTIME abandoned slot. Only
    /// the SAME quarantined job may be abandoned: when the worker returned
    /// and is running DIFFERENT work (or nothing), the stale quarantine is
    /// cleared and the healthy worker is kept. Returns
    /// [`AbandonOutcome::CapReached`] when the runtime budget is exhausted —
    /// the caller's circuit-open condition.
    fn abandon_worker(&self, worker_id: u64, job_id: u64) -> AbandonOutcome {
        let cap = self.shared.policy.max_abandoned;
        if self.shared.runtime_abandoned_live.load(Ordering::SeqCst) >= cap {
            return AbandonOutcome::CapReached;
        }
        // Hold the EXECUTING lock across the same-job check AND the
        // retirement store: a worker removes its job from `executing` (after
        // the provider returns) before it can dequeue anything else, so this
        // makes "the SAME job is still executing" + "retire this worker"
        // atomic against the A-returned/B-started transition (P1: a stale
        // quarantine must never retire the job that replaced it).
        let executing = self
            .shared
            .executing
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let same_job = executing
            .get(&job_id)
            .is_some_and(|job| job.worker_id == worker_id);
        if !same_job {
            drop(executing);
            let mut quarantined = self
                .shared
                .quarantined
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if quarantined
                .get(&worker_id)
                .is_some_and(|job| job.job_id == job_id)
            {
                quarantined.remove(&worker_id);
            }
            return AbandonOutcome::Stale;
        }
        let mut workers = self.workers.lock().unwrap_or_else(|p| p.into_inner());
        let Some(index) = workers.iter().position(|worker| worker.id == worker_id) else {
            // Already abandoned or exited: the quarantine entry is stale.
            drop(workers);
            drop(executing);
            let mut quarantined = self
                .shared
                .quarantined
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if quarantined
                .get(&worker_id)
                .is_some_and(|job| job.job_id == job_id)
            {
                quarantined.remove(&worker_id);
            }
            return AbandonOutcome::Stale;
        };
        if self.shared.runtime_abandoned_live.load(Ordering::SeqCst) >= cap {
            // Re-checked under the worker lock: a concurrent retirement may
            // have consumed the budget since the first check.
            drop(workers);
            drop(executing);
            return AbandonOutcome::CapReached;
        }
        let mut worker = workers.remove(index);
        // Reserve the runtime abandoned slot BEFORE publishing the
        // retirement kind, so the worker's exit (which observes it) can
        // never decrement the counter before the increment. Both happen
        // under the worker lock, so an observer either sees the worker still
        // owned or already counted as abandoned — never neither.
        self.shared
            .runtime_abandoned_live
            .fetch_add(1, Ordering::SeqCst);
        self.shared
            .runtime_abandoned_total
            .fetch_add(1, Ordering::SeqCst);
        WorkerRetirement::Runtime.store(&worker.retirement);
        // Drop the join handle: the thread is DETACHED (it cannot be killed
        // from safe Rust), bounded by the absolute runtime cap. The process
        // lives on and the caller already holds a typed outcome.
        worker.join.take();
        drop(workers);
        drop(executing);
        self.shared
            .quarantined
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&worker_id);
        tracing::error!(
            target: "faktor_agent::evidence",
            worker_id,
            job_id,
            runtime_abandoned = self.shared.runtime_abandoned_live.load(Ordering::SeqCst),
            cap,
            "evidence executor abandoned a worker thread blocked inside a non-yielding provider (grace expired); its logical slot is replaced and the physical thread exits if the provider ever returns"
        );
        // Wake an idle retired worker so it observes the flag and exits.
        self.shared.wake.notify_all();
        AbandonOutcome::Abandoned
    }

    /// Spawn replacement logical slots after an abandonment or after
    /// abandoned threads exited, up to the configured worker count. The
    /// runtime abandonment cap does NOT gate restoration: at the cap the
    /// executor restores missing slots too, because healthy replacements
    /// must keep serving (the documented physical bound is
    /// `target_workers + max_abandoned`).
    fn restore_capacity(&self) {
        if self.shared.closed.load(Ordering::SeqCst) {
            return;
        }
        #[cfg(test)]
        if self.restore_refused.load(Ordering::SeqCst) {
            return;
        }
        loop {
            let missing = {
                let workers = self.workers.lock().unwrap_or_else(|p| p.into_inner());
                self.shared.target_workers.saturating_sub(workers.len())
            };
            if missing == 0 {
                return;
            }
            let Some(worker) = spawn_worker(&self.shared) else {
                return;
            };
            self.workers
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(worker);
        }
    }

    /// Close the queue, cancel every executing poll and release every queued
    /// one with an explicit cancellation, then wake the workers.
    fn close_and_cancel(&self) {
        self.shared.closed.store(true, Ordering::SeqCst);
        {
            let mut queue = self.shared.queue.lock().unwrap_or_else(|p| p.into_inner());
            while let Some(job) = queue.pop_front() {
                self.shared.stats.cancelled.fetch_add(1, Ordering::Relaxed);
                self.shared.stats.completed.fetch_add(1, Ordering::Relaxed);
                let _ = job.reply.send(WorkerReply::Cancelled);
            }
        }
        {
            let executing = self
                .shared
                .executing
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            for job in executing.values() {
                job.cancel.cancel();
            }
        }
        self.shared.wake.notify_all();
    }

    /// Bounded shutdown: cancel, wait up to `timeout` for the workers to
    /// exit, then join every exited worker. Workers still blocked inside a
    /// non-yielding provider are ABANDONED after the bound (at most the
    /// fixed worker count, never one per poll) and their physical threads
    /// stay alive until their provider returns; the process is never killed.
    /// Shutdown abandonment is a SEPARATE budget from runtime retirement:
    /// bounded by the fixed worker count and deliberately outside
    /// `max_abandoned`, so the public runtime invariant
    /// (`runtime_abandoned <= max_abandoned`) stays true.
    ///
    /// The terminal disposition is PERSISTED: the first call returns
    /// [`EvidenceExecutorShutdownState::Clean`] or
    /// [`EvidenceExecutorShutdownState::Abandoned`] and every later call
    /// (including the `Drop` after an explicit `shutdown`) returns the SAME
    /// value with the SAME abandoned count — never the old "false, then
    /// true" lie after blocked workers had been abandoned.
    pub(crate) fn shutdown(&self, timeout: Duration) -> EvidenceExecutorShutdownState {
        // Fast path through the diagnostic accessor; the guard below
        // re-checks under the lock so two concurrent shutdowns cannot both
        // run the bounded join.
        let terminal = self.shutdown_state();
        if terminal != EvidenceExecutorShutdownState::Running {
            return terminal;
        }
        let mut state = self
            .shutdown_state
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if *state != EvidenceExecutorShutdownState::Running {
            // Terminal: report the SAME disposition again (with the same
            // abandoned count) instead of waiting out the bound on a pool
            // whose join handles were already forgotten.
            return *state;
        }
        self.close_and_cancel();
        let deadline = Instant::now() + timeout;
        {
            let mut exited = self
                .shared
                .exit
                .state
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            while !*exited && self.shared.running.load(Ordering::SeqCst) > 0 {
                let now = Instant::now();
                if now >= deadline {
                    break;
                }
                match self.shared.exit.cv.wait_timeout(exited, deadline - now) {
                    Ok((guard, _)) => exited = guard,
                    Err(poisoned) => exited = poisoned.into_inner().0,
                }
            }
        }
        let (all_exited, newly_abandoned) = {
            let mut workers = self.workers.lock().unwrap_or_else(|p| p.into_inner());
            let mut all_exited = true;
            let mut newly_abandoned = 0usize;
            for worker in workers.iter_mut() {
                if worker.exited.load(Ordering::SeqCst) {
                    if let Some(join) = worker.join.take() {
                        let _ = join.join();
                    }
                } else {
                    all_exited = false;
                    newly_abandoned += 1;
                    // Bounded SHUTDOWN abandonment: at most one thread per
                    // FIXED worker slot, only when the provider blocks its
                    // OS thread past the join bound (uncancellable from safe
                    // Rust). Never one thread per poll, and deliberately
                    // OUTSIDE the runtime abandonment budget: shutdown
                    // abandonment is bounded by the fixed worker count, so
                    // the public runtime invariant
                    // (`runtime_abandoned <= max_abandoned`) stays true.
                    self.shared
                        .shutdown_abandoned_live
                        .fetch_add(1, Ordering::SeqCst);
                    self.shared
                        .shutdown_abandoned_total
                        .fetch_add(1, Ordering::SeqCst);
                    WorkerRetirement::Shutdown.store(&worker.retirement);
                    worker.join.take();
                }
            }
            workers.clear();
            (all_exited, newly_abandoned)
        };
        // The logical slots are gone; a shutdown-abandoned job's stale
        // quarantine entry must not outlive them.
        self.shared
            .quarantined
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        // Every abandoned physical thread still alive (runtime retirement
        // plus this shutdown's) — the honest count. A pool whose logical
        // slots are gone but whose abandoned threads are still alive is NOT
        // clean: the persisted disposition must never claim it is.
        let runtime_abandoned = self.shared.runtime_abandoned_live.load(Ordering::SeqCst);
        let shutdown_abandoned = self.shared.shutdown_abandoned_live.load(Ordering::SeqCst);
        let abandoned_live = runtime_abandoned + shutdown_abandoned;
        let disposition = if all_exited && abandoned_live == 0 {
            EvidenceExecutorShutdownState::Clean
        } else {
            EvidenceExecutorShutdownState::Abandoned {
                workers: abandoned_live,
            }
        };
        *state = disposition;
        if !all_exited {
            // Loud, structured: a bounded abandonment is still a defect
            // signal (the provider ignores cancellation), and the counters
            // are the operator's proof that no poll leaked a thread.
            let stats = self.stats();
            tracing::error!(
                target: "faktor_agent::evidence",
                abandoned = newly_abandoned,
                runtime_abandoned = stats.runtime_abandoned,
                shutdown_abandoned = stats.shutdown_abandoned,
                workers = stats.workers,
                "evidence executor shutdown bound elapsed: {newly_abandoned} worker(s) blocked inside a non-yielding provider were abandoned by SHUTDOWN and their logical slots dropped (bounded by the fixed pool, deliberately outside max_abandoned={}; the runtime invariant runtime_abandoned <= max_abandoned still holds); terminal disposition is persisted and repeated shutdown calls report the same Abandoned count; counters enqueued={} completed={} refused={} cancelled={} max_active={} max_queue_depth={} capacity={}",
                stats.max_abandoned,
                stats.enqueued,
                stats.completed,
                stats.refused,
                stats.cancelled,
                stats.max_active,
                stats.max_queue_depth,
                stats.capacity,
            );
        }
        disposition
    }
}

impl Drop for EvidenceExecutor {
    fn drop(&mut self) {
        let _ = self.shutdown(EVIDENCE_EXECUTOR_JOIN_BOUND);
    }
}

/// FIFO dequeue with a condvar wait; `None` once the executor is closed and
/// the queue is drained, or once this worker was RETIRED (abandoned): a
/// retired worker must never take another poll, so it exits the moment its
/// blocked provider returns (or immediately, when it was idle).
fn next_job(shared: &Arc<ExecutorShared>, retirement: &AtomicU8) -> Option<EvidenceJob> {
    let mut queue = shared.queue.lock().unwrap_or_else(|p| p.into_inner());
    loop {
        if WorkerRetirement::load(retirement) != WorkerRetirement::Active {
            return None;
        }
        if let Some(job) = queue.pop_front() {
            return Some(job);
        }
        if shared.closed.load(Ordering::SeqCst) {
            return None;
        }
        queue = shared.wake.wait(queue).unwrap_or_else(|p| p.into_inner());
    }
}

/// Spawn one owned worker thread. `None` (with a loud typed log) when the OS
/// refuses the thread: the bounded pool runs with fewer logical slots.
fn spawn_worker(shared: &Arc<ExecutorShared>) -> Option<EvidenceWorker> {
    let id = shared.next_worker_id.fetch_add(1, Ordering::Relaxed);
    let exited = Arc::new(AtomicBool::new(false));
    let retirement = Arc::new(AtomicU8::new(WorkerRetirement::Active.bits()));
    let worker_shared = shared.clone();
    let worker_exited = exited.clone();
    let worker_retirement = retirement.clone();
    // The live-thread accounting is reserved BEFORE the thread starts: the
    // new worker may exit immediately (a closed executor) and must never
    // decrement a counter its spawn has not incremented yet.
    shared.running.fetch_add(1, Ordering::SeqCst);
    // A replacement may follow a transient zero-live-thread window.
    shared.exit.reset();
    #[cfg(test)]
    EVIDENCE_WORKERS_LIVE.fetch_add(1, Ordering::SeqCst);
    match std::thread::Builder::new()
        .name(format!("{EVIDENCE_WORKER_NAME}-{id}"))
        .spawn(move || worker_main(worker_shared, id, worker_exited, worker_retirement))
    {
        Ok(join) => {
            #[cfg(test)]
            EVIDENCE_WORKERS_SPAWNED.fetch_add(1, Ordering::SeqCst);
            Some(EvidenceWorker {
                id,
                join: Some(join),
                exited,
                retirement,
            })
        }
        Err(error) => {
            #[cfg(test)]
            EVIDENCE_WORKERS_LIVE.fetch_sub(1, Ordering::SeqCst);
            if shared.running.fetch_sub(1, Ordering::SeqCst) == 1 {
                shared.exit.notify_exited();
            }
            tracing::error!(
                target: "faktor_agent::evidence",
                worker_id = id,
                "evidence executor worker could not be spawned: {error} (the bounded pool runs with fewer workers; saturation is a typed refusal)"
            );
            None
        }
    }
}

/// The worker loop: dequeue, skip cancelled polls WITHOUT invoking the
/// provider (cooperative admission check), drive the provider future under a
/// cancellation select (a yielding future is dropped at its next await point
/// when the caller's budget fires), and reply under panic isolation. A
/// worker RETIRED by the executor (abandoned after quarantine, or abandoned
/// by shutdown) never takes another poll and exits as soon as it returns,
/// releasing the matching abandoned physical slot so the circuit can close.
fn worker_main(
    shared: Arc<ExecutorShared>,
    worker_id: u64,
    exited: Arc<AtomicBool>,
    retirement: Arc<AtomicU8>,
) {
    loop {
        let Some(job) = next_job(&shared, &retirement) else {
            break;
        };
        if job.cancel.is_cancelled() || shared.closed.load(Ordering::SeqCst) {
            shared.stats.cancelled.fetch_add(1, Ordering::Relaxed);
            shared.stats.completed.fetch_add(1, Ordering::Relaxed);
            let _ = job.reply.send(WorkerReply::Cancelled);
            continue;
        }
        let job_id = job.id;
        {
            let mut executing = shared.executing.lock().unwrap_or_else(|p| p.into_inner());
            executing.insert(
                job_id,
                ExecutingJob {
                    cancel: job.cancel.clone(),
                    worker_id,
                    started: Instant::now(),
                },
            );
        }
        // Register BEFORE the shutdown re-check so `close_and_cancel` can
        // never miss this poll: either shutdown sees the registration and
        // cancels it, or this check sees `closed` and self-cancels. The
        // select below then returns immediately either way.
        if shared.closed.load(Ordering::SeqCst) {
            job.cancel.cancel();
        }
        let active = shared.stats.active.fetch_add(1, Ordering::Relaxed) + 1;
        shared.stats.max_active.fetch_max(active, Ordering::Relaxed);
        let EvidenceJob {
            provider,
            session,
            query,
            cancel,
            handle,
            reply,
            ..
        } = job;
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            handle.block_on(async move {
                tokio::select! {
                    biased;
                    () = cancel.cancelled() => None,
                    result = provider.evidence_for(session, query) => Some(result),
                }
            })
        }));
        shared.stats.active.fetch_sub(1, Ordering::Relaxed);
        shared.stats.completed.fetch_add(1, Ordering::Relaxed);
        {
            let mut executing = shared.executing.lock().unwrap_or_else(|p| p.into_inner());
            executing.remove(&job_id);
        }
        let reply_value = match outcome {
            Ok(Some(result)) => WorkerReply::Completed(Ok(result)),
            Ok(None) => WorkerReply::Cancelled,
            Err(payload) => WorkerReply::Completed(Err(payload)),
        };
        let _ = reply.send(reply_value);
        if WorkerRetirement::load(&retirement) != WorkerRetirement::Active {
            // Abandoned while blocked inside the provider: the reply is
            // already sent (the caller may still be listening) and this
            // physical thread must exit instead of taking another poll.
            break;
        }
    }
    #[cfg(test)]
    EVIDENCE_WORKERS_LIVE.fetch_sub(1, Ordering::SeqCst);
    exited.store(true, Ordering::SeqCst);
    match WorkerRetirement::load(&retirement) {
        WorkerRetirement::Runtime => {
            // The runtime-abandoned physical thread drained: release its
            // slot so the circuit can close and capacity can be restored.
            shared.runtime_abandoned_live.fetch_sub(1, Ordering::SeqCst);
            shared.wake.notify_all();
        }
        WorkerRetirement::Shutdown => {
            // The shutdown-abandoned physical thread drained: release its
            // separate (non-runtime) slot.
            shared
                .shutdown_abandoned_live
                .fetch_sub(1, Ordering::SeqCst);
        }
        WorkerRetirement::Active => {}
    }
    if shared.running.fetch_sub(1, Ordering::SeqCst) == 1 {
        shared.exit.notify_exited();
    }
}

/// The process-wide executor: started lazily on the first advisory poll and
/// kept for the process lifetime (a fixed, bounded pool — long-lived workers
/// by design; there is nothing per-poll to leak).
fn global_evidence_executor() -> Arc<EvidenceExecutor> {
    static EXECUTOR: OnceLock<Arc<EvidenceExecutor>> = OnceLock::new();
    EXECUTOR
        .get_or_init(|| {
            EvidenceExecutor::start(EVIDENCE_EXECUTOR_WORKERS, EVIDENCE_EXECUTOR_QUEUE_CAPACITY)
        })
        .clone()
}

/// [`poll_evidence_with_wall_budget_outcome`] on an explicit executor (test
/// seam; the production entry point always uses the process-wide bounded
/// pool). The executor handle is dropped before the poll is awaited: the
/// poll does not keep the pool alive, so an executor can be shut down while
/// polls are in flight (they settle with a typed cancellation).
async fn poll_on_executor(
    executor: Arc<EvidenceExecutor>,
    provider: Arc<dyn EvidenceProvider>,
    session: SessionId,
    query: EvidenceQuery,
    budget: std::time::Duration,
) -> EvidencePollOutcome {
    let budget_ms = budget.as_millis().min(u64::MAX as u128) as u64;
    let cancel = CancellationToken::new();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let submitted = executor.submit(
        provider,
        session,
        query,
        cancel.clone(),
        tokio::runtime::Handle::current(),
        tx,
    );
    drop(executor);
    if let Err(refusal) = submitted {
        // Every refusal is typed. A closed/saturated/no-worker executor is
        // `NotSpawned`; an OPEN CIRCUIT is its own status carrying the
        // abandoned/cap counts, never a plain saturation refusal.
        let message = bounded_poll_message(&refusal.message());
        let status = match refusal {
            SubmitRefusal::CircuitOpen { abandoned, cap, .. } => EvidencePollStatus::CircuitOpen {
                abandoned,
                cap,
                message,
            },
            SubmitRefusal::Closed | SubmitRefusal::Saturated { .. } | SubmitRefusal::NoWorkers => {
                EvidencePollStatus::NotSpawned { message }
            }
        };
        return EvidencePollOutcome {
            evidence: Vec::new(),
            status,
        };
    }
    let mut evidence = Vec::new();
    let status = match tokio::time::timeout(budget, rx).await {
        Ok(Ok(WorkerReply::Completed(Ok(Ok(package))))) => {
            if package.is_empty() {
                EvidencePollStatus::NoEvidence
            } else {
                evidence = package;
                EvidencePollStatus::Served
            }
        }
        Ok(Ok(WorkerReply::Completed(Ok(Err(error))))) => status_from_provider_error(error),
        Ok(Ok(WorkerReply::Completed(Err(payload)))) => EvidencePollStatus::ProviderPanicked {
            message: poll_panic_message(payload.as_ref()),
        },
        Ok(Ok(WorkerReply::Cancelled)) => EvidencePollStatus::NotSpawned {
            message:
                "evidence poll cancelled before it answered (executor shutdown or caller budget)"
                    .to_string(),
        },
        Ok(Err(_)) => EvidencePollStatus::ProviderPanicked {
            message: "evidence poll worker died without answering".to_string(),
        },
        Err(_) => {
            // Cooperative cancellation: the executing future observes the
            // token and is dropped at its next await point; a queued poll is
            // skipped before the provider is ever invoked. A provider that
            // blocks its OS thread is NOT cancellable: the caller gets this
            // typed timeout while the executor quarantines and (bounded by
            // the absolute cap) abandons the blocked physical worker —
            // observable in [`EvidenceExecutorStats`]; the request is never
            // lost silently and the process is never killed.
            cancel.cancel();
            EvidencePollStatus::TimedOut { budget_ms }
        }
    };
    EvidencePollOutcome { evidence, status }
}

/// Poll one async `EvidenceProvider` on the process-wide BOUNDED executor
/// and await it under `budget`, returning the typed outcome: a provider that
/// panics, errors, or simply never yields degrades to an empty package once
/// the budget fires. The provider's future is driven on an OWNED worker
/// thread via the captured runtime handle — never on a runtime worker, never
/// on a tokio blocking-pool task, and never a freshly spawned detached
/// thread per poll (the P1 leak this replaced). When the budget fires the
/// caller cancels the admission: a queued poll is skipped before the
/// provider is invoked, and a yielding in-flight future is dropped at its
/// next await point, freeing the worker.
///
/// Degradation is observable by contract: the status distinguishes an
/// honest [`EvidencePollStatus::NoEvidence`] answer from
/// [`EvidencePollStatus::RetrievalFailed`] (provider code + retryability),
/// a panic, a missed budget and a refusal (saturation/shutdown/spawn
/// failure). The caller OWNS the diagnostic:
/// [`poll_evidence_with_wall_budget`] emits the structured log for the
/// frozen `runtime.rs` call shape; any other caller must surface a degraded
/// status itself (never swallow it).
pub(crate) async fn poll_evidence_with_wall_budget_outcome(
    provider: Arc<dyn EvidenceProvider>,
    session: SessionId,
    query: EvidenceQuery,
    budget: std::time::Duration,
) -> EvidencePollOutcome {
    poll_on_executor(global_evidence_executor(), provider, session, query, budget).await
}

/// Advisory legacy poll (TEST-ONLY): returns the evidence package, empty
/// when the provider fails/panics/times out. No production caller remains —
/// the runtime drive owns the typed outcome
/// ([`poll_evidence_with_wall_budget_outcome`]) and archives the status — so
/// the status-free shape survives only to pin the documented legacy
/// contract in tests:
///
/// - an empty package is the SAME input as "no evidence" for the turn (the
///   advisorship never blocks a turn), and the degradation is NEVER silent:
///   every degraded [`poll_evidence_with_wall_budget_outcome`] status is
///   logged here with the provider code/retryability;
/// - callers that need to distinguish "no evidence" from "retrieval failed"
///   at the decision boundary must use
///   [`poll_evidence_with_wall_budget_outcome`] instead.
#[cfg(test)]
pub(crate) async fn poll_evidence_with_wall_budget(
    provider: Arc<dyn EvidenceProvider>,
    session: SessionId,
    query: EvidenceQuery,
    budget: std::time::Duration,
) -> Vec<faktor_context::assembler::Evidence> {
    let outcome = poll_evidence_with_wall_budget_outcome(provider, session, query, budget).await;
    if outcome.status.is_degraded() {
        log_evidence_poll_degrade(&outcome.status, budget);
    }
    outcome.evidence
}

/// The agent may never match on provider names (Commandment 4). This test
/// locks that invariant structurally across the whole crate.
#[cfg(test)]
mod no_provider_switching {
    #[test]
    fn agent_source_has_no_provider_name_conditionals() {
        // Scan production sources only (skip test modules, whose own
        // assertions necessarily mention the forbidden literals).
        let mut sources = String::new();
        for file in [
            "lib.rs",
            "runtime.rs",
            "tool.rs",
            "tool_json.rs",
            "loop_detect.rs",
            "stall.rs",
        ] {
            let path = format!("{}/src/{file}", env!("CARGO_MANIFEST_DIR"));
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let text = strip_test_modules(&text);
            sources.push_str(&text);
        }
        for needle in [
            "if provider ==",
            "match provider",
            "provider == \"deepseek\"",
            "provider == \"ollama\"",
            "provider == \"openai\"",
        ] {
            assert!(
                !sources.contains(needle),
                "agent source must not contain {needle:?}"
            );
        }
    }

    fn strip_test_modules(src: &str) -> String {
        // Remove #[cfg(test)] blocks so the invariant test cannot see its
        // own literals.
        let mut out = String::new();
        let mut rest = src;
        while let Some(idx) = rest.find("#[cfg(test)]") {
            out.push_str(&rest[..idx]);
            rest = &rest[idx + "#[cfg(test)]".len()..];
            // Skip to the closing brace of the mod at depth 0.
            let mut depth = 0i32;
            let mut consumed = 0usize;
            let mut found = false;
            for (i, c) in rest.char_indices() {
                match c {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            consumed = i + 1;
                            found = true;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            if found {
                rest = &rest[consumed..];
            }
        }
        out.push_str(rest);
        out
    }
}

// ---------------------------------------------------------------- service

/// Advisory evidence poll (audits 14/26): a provider failure MUST be
/// observable — typed status plus a structured diagnostic — never a silent
/// empty package. `NoEvidence` and `RetrievalFailed` are distinguishable,
/// and the legacy status-free wrapper is only allowed because the typed
/// path it delegates to logs the degradation loudly.
#[cfg(test)]
mod advisory_evidence_poll_tests {
    use super::*;
    use faktor_context::assembler::Evidence;
    use futures::future::BoxFuture;

    fn query() -> EvidenceQuery {
        EvidenceQuery {
            prompt: "where is the parser".into(),
            changed_files: vec!["src/a.rs".into()],
            failures: vec![],
        }
    }

    fn budget() -> std::time::Duration {
        std::time::Duration::from_millis(500)
    }

    struct FailingProvider {
        error: faktor_core::Error,
    }

    impl EvidenceProvider for FailingProvider {
        fn evidence_for(
            &self,
            _session: SessionId,
            _query: EvidenceQuery,
        ) -> BoxFuture<'_, faktor_core::Result<Vec<Evidence>>> {
            let error = self.error.clone();
            Box::pin(async move { Err(error) })
        }
    }

    struct EmptyProvider;

    impl EvidenceProvider for EmptyProvider {
        fn evidence_for(
            &self,
            _session: SessionId,
            _query: EvidenceQuery,
        ) -> BoxFuture<'_, faktor_core::Result<Vec<Evidence>>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    struct ServingProvider;

    impl EvidenceProvider for ServingProvider {
        fn evidence_for(
            &self,
            _session: SessionId,
            _query: EvidenceQuery,
        ) -> BoxFuture<'_, faktor_core::Result<Vec<Evidence>>> {
            Box::pin(async {
                Ok(vec![Evidence {
                    path: "src/a.rs".into(),
                    snippet: "fn parser()".into(),
                    score: 0.9,
                }])
            })
        }
    }

    struct PanickingProvider;

    impl EvidenceProvider for PanickingProvider {
        fn evidence_for(
            &self,
            _session: SessionId,
            _query: EvidenceQuery,
        ) -> BoxFuture<'_, faktor_core::Result<Vec<Evidence>>> {
            Box::pin(async {
                panic!("provider exploded mid-poll");
                #[allow(unreachable_code)]
                Ok(Vec::new())
            })
        }
    }

    struct HangingProvider;

    impl EvidenceProvider for HangingProvider {
        fn evidence_for(
            &self,
            _session: SessionId,
            _query: EvidenceQuery,
        ) -> BoxFuture<'_, faktor_core::Result<Vec<Evidence>>> {
            Box::pin(futures::future::pending())
        }
    }

    fn session() -> SessionId {
        SessionId::new(7)
    }

    #[tokio::test]
    async fn failing_provider_is_typed_and_logged_loudly_not_silently_empty() {
        let failing = || {
            Arc::new(FailingProvider {
                error: faktor_core::Error::new(
                    faktor_core::error::ErrorKind::Provider {
                        code: "e503".into(),
                        retryable: true,
                    },
                    "embedding backend down",
                ),
            }) as Arc<dyn EvidenceProvider>
        };
        // Typed path: the failure is a status, not an indistinguishable
        // empty answer.
        let outcome =
            poll_evidence_with_wall_budget_outcome(failing(), session(), query(), budget()).await;
        assert!(outcome.evidence.is_empty());
        assert_eq!(
            outcome.status,
            EvidencePollStatus::RetrievalFailed {
                code: "e503".into(),
                retryable: true,
                message: "embedding backend down".into(),
            }
        );
        assert!(outcome.status.is_degraded());
        // Legacy path (the frozen runtime.rs call shape): the empty package
        // is accompanied by a degraded diagnostic, never silent.
        let before = evidence_poll_degraded_diagnostics();
        let legacy = poll_evidence_with_wall_budget(failing(), session(), query(), budget()).await;
        let after = evidence_poll_degraded_diagnostics();
        assert!(
            legacy.is_empty(),
            "the advisory degrade is an empty package"
        );
        assert_eq!(
            after,
            before + 1,
            "a retrieval failure must emit exactly one degraded diagnostic"
        );
    }

    #[tokio::test]
    async fn empty_answer_is_no_evidence_not_a_retrieval_failure() {
        let outcome = poll_evidence_with_wall_budget_outcome(
            Arc::new(EmptyProvider),
            session(),
            query(),
            budget(),
        )
        .await;
        assert!(outcome.evidence.is_empty());
        assert_eq!(outcome.status, EvidencePollStatus::NoEvidence);
        assert!(!outcome.status.is_degraded());
        // An honest empty answer is NOT a degradation: no diagnostic fires.
        let before = evidence_poll_degraded_diagnostics();
        let legacy =
            poll_evidence_with_wall_budget(Arc::new(EmptyProvider), session(), query(), budget())
                .await;
        let after = evidence_poll_degraded_diagnostics();
        assert!(legacy.is_empty());
        assert_eq!(after, before, "no-evidence is not a retrieval failure");
    }

    #[tokio::test]
    async fn serving_provider_returns_the_package_unchanged() {
        let outcome = poll_evidence_with_wall_budget_outcome(
            Arc::new(ServingProvider),
            session(),
            query(),
            budget(),
        )
        .await;
        assert_eq!(outcome.status, EvidencePollStatus::Served);
        assert!(!outcome.status.is_degraded());
        assert_eq!(outcome.evidence.len(), 1);
        assert_eq!(outcome.evidence[0].path, "src/a.rs");
    }

    #[tokio::test]
    async fn provider_panic_is_caught_and_typed() {
        let outcome = poll_evidence_with_wall_budget_outcome(
            Arc::new(PanickingProvider),
            session(),
            query(),
            budget(),
        )
        .await;
        assert!(outcome.evidence.is_empty());
        match &outcome.status {
            EvidencePollStatus::ProviderPanicked { message } => {
                assert!(message.contains("provider exploded"), "{message}");
            }
            other => panic!("a panic must be typed, not silent: {other:?}"),
        }
        // The legacy path reports it too.
        let before = evidence_poll_degraded_diagnostics();
        let legacy = poll_evidence_with_wall_budget(
            Arc::new(PanickingProvider),
            session(),
            query(),
            budget(),
        )
        .await;
        let after = evidence_poll_degraded_diagnostics();
        assert!(legacy.is_empty());
        assert_eq!(after, before + 1, "a provider panic must be reported");
    }

    #[tokio::test]
    async fn missed_wall_budget_is_typed_and_never_blocks() {
        let started = std::time::Instant::now();
        let before = evidence_poll_degraded_diagnostics();
        let outcome = poll_evidence_with_wall_budget_outcome(
            Arc::new(HangingProvider),
            session(),
            query(),
            std::time::Duration::from_millis(20),
        )
        .await;
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(outcome.evidence.is_empty());
        assert_eq!(
            outcome.status,
            EvidencePollStatus::TimedOut { budget_ms: 20 }
        );
        assert!(outcome.status.is_degraded());
        // The legacy wrapper reports the missed budget as well.
        let legacy = poll_evidence_with_wall_budget(
            Arc::new(HangingProvider),
            session(),
            query(),
            std::time::Duration::from_millis(20),
        )
        .await;
        let after = evidence_poll_degraded_diagnostics();
        assert!(legacy.is_empty());
        assert_eq!(after, before + 1, "a timeout must be reported");
    }

    #[tokio::test]
    async fn hostile_provider_message_is_bounded() {
        let huge = "x".repeat(64 * 1024);
        let outcome = poll_evidence_with_wall_budget_outcome(
            Arc::new(FailingProvider {
                error: faktor_core::Error::new(faktor_core::error::ErrorKind::Internal, huge),
            }),
            session(),
            query(),
            budget(),
        )
        .await;
        match outcome.status {
            EvidencePollStatus::RetrievalFailed { message, .. } => {
                assert!(
                    message.len() <= EVIDENCE_POLL_MESSAGE_MAX_BYTES,
                    "{}",
                    message.len()
                );
            }
            other => panic!("expected a typed retrieval failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn non_provider_error_kinds_map_to_their_kind_name() {
        let outcome = poll_evidence_with_wall_budget_outcome(
            Arc::new(FailingProvider {
                error: faktor_core::Error::timeout("index busy"),
            }),
            session(),
            query(),
            budget(),
        )
        .await;
        assert_eq!(
            outcome.status,
            EvidencePollStatus::RetrievalFailed {
                code: "Timeout".into(),
                retryable: true,
                message: "index busy".into(),
            }
        );
    }
}

/// Adversarial coverage of the bounded evidence executor (P1): a provider
/// that never yields must never grow the thread/worker population, excess
/// polls must be explicitly typed, shutdown/Drop must join owned workers
/// within a bound, and the off-turn blocking bridge must differentiate a
/// panic from a normal value.
#[cfg(test)]
mod bounded_evidence_executor_tests {
    use super::*;
    use faktor_context::assembler::Evidence;
    use futures::future::BoxFuture;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    fn query() -> EvidenceQuery {
        EvidenceQuery {
            prompt: "where is the parser".into(),
            changed_files: vec!["src/a.rs".into()],
            failures: vec![],
        }
    }

    fn session() -> SessionId {
        SessionId::new(11)
    }

    struct ServingProvider;

    impl EvidenceProvider for ServingProvider {
        fn evidence_for(
            &self,
            _session: SessionId,
            _query: EvidenceQuery,
        ) -> BoxFuture<'_, faktor_core::Result<Vec<Evidence>>> {
            Box::pin(async {
                Ok(vec![Evidence {
                    path: "src/a.rs".into(),
                    snippet: "fn parser()".into(),
                    score: 0.9,
                }])
            })
        }
    }

    /// A future that yields forever: cooperative — the worker's cancellation
    /// select drops it the moment the caller's budget fires.
    struct CooperativeHangingProvider;

    impl EvidenceProvider for CooperativeHangingProvider {
        fn evidence_for(
            &self,
            _session: SessionId,
            _query: EvidenceQuery,
        ) -> BoxFuture<'_, faktor_core::Result<Vec<Evidence>>> {
            Box::pin(futures::future::pending())
        }
    }

    /// A provider that blocks its OS thread INSIDE the first poll forever:
    /// not cancellable from safe Rust, so it can strand at most the fixed
    /// worker count.
    struct SyncBlockingProvider;

    impl EvidenceProvider for SyncBlockingProvider {
        fn evidence_for(
            &self,
            _session: SessionId,
            _query: EvidenceQuery,
        ) -> BoxFuture<'_, faktor_core::Result<Vec<Evidence>>> {
            Box::pin(async move {
                let (_tx, rx) = std::sync::mpsc::channel::<()>();
                let _ = rx.recv();
                Ok(vec![])
            })
        }
    }

    /// Serializes the heavy measurement tests: the process-wide evidence
    /// worker counters are shared and the leak math needs a quiet window.
    static HEAVY_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// Live OS threads of this process when the platform exposes them
    /// (Linux `/proc/self/task`, macOS `ps -M`); `None` elsewhere.
    fn live_thread_count() -> Option<usize> {
        #[cfg(target_os = "linux")]
        let count = std::fs::read_dir("/proc/self/task").ok().map(|d| d.count());
        #[cfg(target_os = "macos")]
        let count = std::process::Command::new("ps")
            .arg("-M")
            .arg(std::process::id().to_string())
            .output()
            .ok()
            .and_then(|output| {
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .count()
                    .checked_sub(1)
            });
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let count = None;
        count
    }

    /// Initialize the process-wide pool once, so later spawn-counter deltas
    /// measure only this test's executors. Also pins the DOCUMENTED fixed
    /// size of the production pool: the whole concurrency budget of the
    /// feature is `EVIDENCE_EXECUTOR_WORKERS` owned workers with a
    /// `EVIDENCE_EXECUTOR_QUEUE_CAPACITY`-bounded admission queue.
    async fn warm_global_executor() {
        let outcome = poll_evidence_with_wall_budget_outcome(
            Arc::new(ServingProvider),
            session(),
            query(),
            Duration::from_millis(500),
        )
        .await;
        assert_eq!(outcome.status, EvidencePollStatus::Served);
        let global = global_evidence_executor();
        assert_eq!(
            global.worker_count(),
            EVIDENCE_EXECUTOR_WORKERS,
            "the production pool is fixed at its documented worker count"
        );
        assert_eq!(
            global.stats().capacity,
            EVIDENCE_EXECUTOR_QUEUE_CAPACITY,
            "the production pool keeps its documented bounded queue"
        );
    }

    #[tokio::test]
    async fn hundreds_of_polls_never_grow_the_worker_population() {
        let _guard = HEAVY_TESTS.lock().await;
        warm_global_executor().await;
        let live_before = EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst);
        let executor = EvidenceExecutor::start(2, 4);
        assert_eq!(executor.worker_count(), 2, "the pool is fixed at start");
        assert_eq!(
            EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst),
            live_before + 2,
            "start owns exactly its fixed worker count"
        );
        let spawned_after_start = EVIDENCE_WORKERS_SPAWNED.load(Ordering::SeqCst);
        let threads_before = live_thread_count();
        let (mut served, mut timed_out) = (0usize, 0usize);
        for i in 0..200usize {
            let (provider, budget): (Arc<dyn EvidenceProvider>, Duration) = if i % 4 == 0 {
                (Arc::new(ServingProvider), Duration::from_secs(5))
            } else {
                (
                    Arc::new(CooperativeHangingProvider),
                    Duration::from_millis(2),
                )
            };
            let outcome =
                poll_on_executor(executor.clone(), provider, session(), query(), budget).await;
            match outcome.status {
                EvidencePollStatus::Served => served += 1,
                EvidencePollStatus::TimedOut { .. } => timed_out += 1,
                other => panic!("unexpected status under bounded polling: {other:?}"),
            }
        }
        assert_eq!(served, 50, "served polls keep their normal answer");
        assert_eq!(timed_out, 150, "every missed budget is typed, none hangs");
        assert_eq!(executor.worker_count(), 2, "the pool must stay fixed");
        assert_eq!(
            EVIDENCE_WORKERS_SPAWNED.load(Ordering::SeqCst),
            spawned_after_start,
            "200 polls must never spawn a worker thread (the old leak spawned one detached thread per poll)"
        );
        let stats = executor.stats();
        assert_eq!(stats.enqueued, 200);
        // The worker's completion counter settles just after each reply; the
        // last timed-out poll races its caller, so wait (bounded) for the
        // settle instead of asserting on a snapshot.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut stats = stats;
        while stats.completed < 200 {
            assert!(
                Instant::now() < deadline,
                "every admitted poll must settle: {stats:?}"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
            stats = executor.stats();
        }
        assert_eq!(stats.workers, 2, "the pool must stay fixed");
        assert_eq!(stats.capacity, 4, "the queue bound is fixed");
        assert!(stats.max_active <= 2, "active high-water: {stats:?}");
        assert!(stats.max_queue_depth <= 4, "queue high-water: {stats:?}");
        assert_eq!(
            EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst),
            live_before + 2,
            "200 polls must neither spawn nor leak an evidence worker"
        );
        if let (Some(before), Some(after)) = (threads_before, live_thread_count()) {
            assert!(
                after <= before + 64,
                "200 polls must not leak threads (generous ceiling): before={before} after={after}"
            );
        }
    }

    #[tokio::test]
    async fn saturating_stuck_provider_is_typed_and_never_spawns_more_workers() {
        let _guard = HEAVY_TESTS.lock().await;
        warm_global_executor().await;
        let live_before = EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst);
        let executor = EvidenceExecutor::start(2, 4);
        let spawned_after_start = EVIDENCE_WORKERS_SPAWNED.load(Ordering::SeqCst);
        let threads_before = live_thread_count();
        let polls: Vec<_> = (0..60)
            .map(|_| {
                poll_on_executor(
                    executor.clone(),
                    Arc::new(SyncBlockingProvider),
                    session(),
                    query(),
                    Duration::from_millis(60),
                )
            })
            .collect();
        let outcomes = futures::future::join_all(polls).await;
        let (mut timed_out, mut refused) = (0usize, 0usize);
        for outcome in outcomes {
            match outcome.status {
                EvidencePollStatus::TimedOut { .. } => timed_out += 1,
                EvidencePollStatus::NotSpawned { message } => {
                    assert!(message.contains("saturated"), "{message}");
                    refused += 1;
                }
                other => panic!("stuck-provider polls must be typed, got {other:?}"),
            }
        }
        assert!(timed_out >= 2, "the two workers admit at least two polls");
        assert!(refused >= 1, "the burst must overflow the bounded pool");
        assert_eq!(timed_out + refused, 60, "every poll is accounted for");
        assert_eq!(
            executor.worker_count(),
            2,
            "saturation never grows the pool"
        );
        assert_eq!(
            EVIDENCE_WORKERS_SPAWNED.load(Ordering::SeqCst),
            spawned_after_start,
            "a saturating burst must not spawn a worker"
        );
        let stats = executor.stats();
        assert_eq!(
            stats.enqueued as usize + stats.refused as usize,
            60,
            "admission is a bounded partition: {stats:?}"
        );
        assert!(stats.max_active <= 2, "active high-water: {stats:?}");
        assert!(stats.max_queue_depth <= 4, "queue high-water: {stats:?}");
        if let (Some(before), Some(after)) = (threads_before, live_thread_count()) {
            assert!(
                after <= before + 64,
                "the stuck provider must not leak threads: before={before} after={after}"
            );
        }
        // Sequential hundreds of polls against the same wedged pool: every
        // poll is admitted into reclaimed capacity (cancelled queued entries
        // are purged) or refused, and ALWAYS typed — never a new thread.
        for _ in 0..200 {
            let outcome = poll_on_executor(
                executor.clone(),
                Arc::new(SyncBlockingProvider),
                session(),
                query(),
                Duration::from_millis(5),
            )
            .await;
            assert!(
                matches!(
                    outcome.status,
                    EvidencePollStatus::TimedOut { .. } | EvidencePollStatus::NotSpawned { .. }
                ),
                "sequential polls against a wedged pool must stay typed: {:?}",
                outcome.status
            );
        }
        assert_eq!(executor.worker_count(), 2, "sequential polls grow nothing");
        assert_eq!(
            EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst),
            live_before + 2,
            "the stuck pool strands exactly the fixed worker count, never one thread per poll"
        );
        assert_eq!(
            EVIDENCE_WORKERS_SPAWNED.load(Ordering::SeqCst),
            spawned_after_start,
            "200 sequential polls must not spawn a worker"
        );
        // The synchronously blocked workers cannot be cancelled from safe
        // Rust: a SHORT bounded shutdown reports the truth and abandons at
        // most the fixed worker count (never one thread per poll). The
        // executor handle is released, so Drop is immediate afterwards.
        let started = Instant::now();
        let disposition = executor.shutdown(Duration::from_millis(100));
        assert_eq!(
            disposition,
            EvidenceExecutorShutdownState::Abandoned { workers: 2 },
            "a synchronously blocked worker must be reported as abandoned, with the count"
        );
        assert_eq!(
            executor.shutdown(Duration::from_millis(100)),
            disposition,
            "the terminal disposition is persisted, not re-derived"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the bounded join attempt must not wait out the provider"
        );
        assert_eq!(executor.worker_count(), 0);
        drop(executor);
    }

    #[tokio::test]
    async fn shutdown_cancels_cooperative_workers_within_bound() {
        let _guard = HEAVY_TESTS.lock().await;
        warm_global_executor().await;
        let live_before = EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst);
        let executor = EvidenceExecutor::start(2, 4);
        let mut polls = Vec::new();
        for _ in 0..2 {
            polls.push(tokio::spawn(poll_on_executor(
                executor.clone(),
                Arc::new(CooperativeHangingProvider),
                session(),
                query(),
                Duration::from_secs(30),
            )));
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while executor.stats().max_active < 2 {
            assert!(Instant::now() < deadline, "workers never started the polls");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let started = Instant::now();
        assert_eq!(
            executor.shutdown(Duration::from_secs(2)),
            EvidenceExecutorShutdownState::Clean,
            "cooperative workers must be cancelled and joined"
        );
        assert_eq!(
            executor.shutdown(Duration::from_secs(2)),
            EvidenceExecutorShutdownState::Clean,
            "a repeated shutdown reports the same clean disposition"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "shutdown must join within its bound"
        );
        assert_eq!(executor.worker_count(), 0, "workers are owned, then joined");
        assert_eq!(
            EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst),
            live_before,
            "shutdown must leave no evidence worker alive"
        );
        for poll in polls {
            match poll.await.unwrap().status {
                EvidencePollStatus::NotSpawned { .. } => {}
                other => panic!("a shutdown-cancelled poll must be typed, got {other:?}"),
            }
        }
        let late = poll_on_executor(
            executor.clone(),
            Arc::new(ServingProvider),
            session(),
            query(),
            Duration::from_millis(50),
        )
        .await;
        assert!(
            matches!(late.status, EvidencePollStatus::NotSpawned { .. }),
            "a closed executor refuses with a typed status: {:?}",
            late.status
        );
    }

    #[tokio::test]
    async fn drop_joins_cooperative_workers_within_bound() {
        let _guard = HEAVY_TESTS.lock().await;
        warm_global_executor().await;
        let live_before = EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst);
        let executor = EvidenceExecutor::start(2, 4);
        let mut polls = Vec::new();
        for _ in 0..2 {
            polls.push(tokio::spawn(poll_on_executor(
                executor.clone(),
                Arc::new(CooperativeHangingProvider),
                session(),
                query(),
                Duration::from_secs(30),
            )));
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while executor.stats().max_active < 2 {
            assert!(Instant::now() < deadline, "workers never started the polls");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert_eq!(
            Arc::strong_count(&executor),
            1,
            "the in-flight polls must not keep the executor alive"
        );
        let started = Instant::now();
        drop(executor);
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "Drop must cancel and join owned workers within the bound"
        );
        assert_eq!(
            EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst),
            live_before,
            "Drop must join every owned worker"
        );
        for poll in polls {
            match poll.await.unwrap().status {
                EvidencePollStatus::NotSpawned { .. } => {}
                other => panic!("a Drop-cancelled poll must be typed, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn off_turn_failure_is_a_typed_degradation_never_none() {
        assert_eq!(run_off_turn_thread_outcome(|| 21 + 21).await, Ok(42));
        match run_off_turn_thread_outcome(|| -> u32 { panic!("off-turn boom") }).await {
            Err(OffTurnThreadFailure::Panicked { message }) => {
                assert!(message.contains("off-turn boom"), "{message}");
            }
            other => panic!("a panic must be a typed outcome, got {other:?}"),
        }
        // P2: the typed failure maps to an explicit evidence degradation —
        // never `None`, never conflated with an honest empty answer.
        let failure = run_off_turn_thread_outcome(|| -> u32 { panic!("cold ladder boom") })
            .await
            .expect_err("the panic must be a typed outcome");
        let status = evidence_status_from_off_turn_failure(failure);
        match &status {
            EvidencePollStatus::ProviderPanicked { message } => {
                assert!(message.contains("cold ladder boom"), "{message}");
            }
            other => panic!("a panicking off-turn task must be ProviderPanicked, got {other:?}"),
        }
        assert!(
            status.is_degraded(),
            "a broken off-turn task is never no-evidence"
        );
        let unscheduled =
            evidence_status_from_off_turn_failure(OffTurnThreadFailure::Unscheduled {
                message: "pool closed".into(),
            });
        assert!(
            matches!(
                unscheduled,
                EvidencePollStatus::NotSpawned { ref message } if message.contains("pool closed")
            ),
            "{unscheduled:?}"
        );
        assert!(unscheduled.is_degraded());
    }

    /// Test seam: a gate that parks the provider's OS thread until the test
    /// releases it, so an ABANDONED worker can actually drain and prove that
    /// capacity recovers (the forever-blocked [`SyncBlockingProvider`]
    /// proves the terminal cap/circuit behavior instead).
    #[derive(Default)]
    struct BlockGate {
        released: std::sync::Mutex<bool>,
        cv: std::sync::Condvar,
        entered: AtomicUsize,
    }

    impl BlockGate {
        fn block(&self) {
            self.entered.fetch_add(1, Ordering::SeqCst);
            let mut released = self.released.lock().unwrap_or_else(|p| p.into_inner());
            while !*released {
                released = self.cv.wait(released).unwrap_or_else(|p| p.into_inner());
            }
        }

        fn release_all(&self) {
            *self.released.lock().unwrap_or_else(|p| p.into_inner()) = true;
            self.cv.notify_all();
        }

        fn entered(&self) -> usize {
            self.entered.load(Ordering::SeqCst)
        }
    }

    struct GatedBlockingProvider {
        gate: Arc<BlockGate>,
    }

    impl EvidenceProvider for GatedBlockingProvider {
        fn evidence_for(
            &self,
            _session: SessionId,
            _query: EvidenceQuery,
        ) -> BoxFuture<'_, faktor_core::Result<Vec<Evidence>>> {
            let gate = self.gate.clone();
            Box::pin(async move {
                // Blocks the worker's OS thread inside the first poll: the
                // P1 scenario (not cancellable from safe Rust).
                gate.block();
                Ok(vec![Evidence {
                    path: "src/gated.rs".into(),
                    snippet: "served after release".into(),
                    score: 1.0,
                }])
            })
        }
    }

    /// P1: a stuck provider is quarantined after the hard retirement
    /// deadline, its physical thread is abandoned after the grace and its
    /// logical slot is REPLACED — evidence stays available AT the absolute
    /// runtime abandoned cap (healthy replacements keep serving); the
    /// degraded/open circuit appears only when a stuck worker needs an
    /// abandonment the exhausted budget refuses. The physical thread count
    /// never exceeds `workers + cap`, and a released provider drains the
    /// abandoned threads so capacity recovers.
    #[tokio::test]
    async fn stuck_workers_are_quarantined_replaced_and_capped_by_the_circuit() {
        let _guard = HEAVY_TESTS.lock().await;
        warm_global_executor().await;
        let live_before = EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst);
        let spawned_before = EVIDENCE_WORKERS_SPAWNED.load(Ordering::SeqCst);
        let threads_before = live_thread_count();
        let gate = Arc::new(BlockGate::default());
        let executor = EvidenceExecutor::start_with_policy(
            2,
            4,
            EvidenceRetirementPolicy {
                retirement_deadline: Duration::from_millis(40),
                quarantine_grace: Duration::from_millis(40),
                max_abandoned: 4,
            },
        );
        assert_eq!(
            executor.shutdown_state(),
            EvidenceExecutorShutdownState::Running,
            "no shutdown has run yet"
        );
        assert_eq!(executor.stats().circuit, EvidenceCircuitState::Closed);

        // Wedge both workers with providers that block their OS thread.
        for _ in 0..2 {
            let outcome = poll_on_executor(
                executor.clone(),
                Arc::new(GatedBlockingProvider { gate: gate.clone() }),
                session(),
                query(),
                Duration::from_millis(20),
            )
            .await;
            assert!(
                matches!(outcome.status, EvidencePollStatus::TimedOut { .. }),
                "a synchronously blocked poll times out typed: {:?}",
                outcome.status
            );
        }
        assert_eq!(gate.entered(), 2, "both workers reached the provider");

        // Hard retirement deadline exceeded -> QUARANTINED (no early
        // abandonment: the grace has not expired).
        tokio::time::sleep(Duration::from_millis(60)).await;
        executor.maintain();
        let stats = executor.stats();
        assert_eq!(stats.workers_quarantined, 2, "{stats:?}");
        assert_eq!(stats.workers_healthy, 0, "{stats:?}");
        assert_eq!(
            stats.runtime_abandoned, 0,
            "quarantine never abandons early: {stats:?}"
        );
        assert_eq!(stats.circuit, EvidenceCircuitState::Closed, "{stats:?}");

        // Grace expiry -> the blocked physical threads are ABANDONED and
        // their logical slots replaced: evidence stays available.
        tokio::time::sleep(Duration::from_millis(60)).await;
        executor.maintain();
        let stats = executor.stats();
        assert_eq!(
            stats.runtime_abandoned, 2,
            "one abandoned physical thread per wedged worker: {stats:?}"
        );
        assert_eq!(stats.runtime_abandoned_total, 2, "{stats:?}");
        assert_eq!(stats.workers, 2, "replacement logical slots: {stats:?}");
        assert_eq!(stats.workers_healthy, 2, "{stats:?}");
        assert_eq!(stats.workers_quarantined, 0, "{stats:?}");
        assert_eq!(
            stats.circuit,
            EvidenceCircuitState::Closed,
            "under the cap the circuit stays closed: {stats:?}"
        );
        assert_eq!(
            EVIDENCE_WORKERS_SPAWNED.load(Ordering::SeqCst) - spawned_before,
            4,
            "2 original + 2 replacement worker threads"
        );
        let served = poll_on_executor(
            executor.clone(),
            Arc::new(ServingProvider),
            session(),
            query(),
            Duration::from_millis(500),
        )
        .await;
        assert_eq!(
            served.status,
            EvidencePollStatus::Served,
            "replacement slots keep evidence available while stuck threads never drain"
        );

        // Wedge the replacements too: the runtime abandoned count reaches
        // the absolute cap. That is NOT an open circuit: no stuck worker is
        // awaiting an abandonment the budget refuses, and healthy slots keep
        // serving.
        for _ in 0..2 {
            let outcome = poll_on_executor(
                executor.clone(),
                Arc::new(GatedBlockingProvider { gate: gate.clone() }),
                session(),
                query(),
                Duration::from_millis(20),
            )
            .await;
            assert!(matches!(
                outcome.status,
                EvidencePollStatus::TimedOut { .. }
            ));
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
        executor.maintain();
        tokio::time::sleep(Duration::from_millis(60)).await;
        executor.maintain();
        let stats = executor.stats();
        assert_eq!(
            stats.runtime_abandoned, 4,
            "the absolute runtime cap is reached: {stats:?}"
        );
        assert_eq!(stats.runtime_abandoned_total, 4, "{stats:?}");
        assert_eq!(
            stats.workers, 2,
            "the two replacement logical slots are restored AT the cap: {stats:?}"
        );
        assert_eq!(
            stats.circuit,
            EvidenceCircuitState::Closed,
            "at the cap, no stuck worker needs abandonment yet: {stats:?}"
        );
        let served_at_cap = poll_on_executor(
            executor.clone(),
            Arc::new(ServingProvider),
            session(),
            query(),
            Duration::from_millis(500),
        )
        .await;
        assert_eq!(
            served_at_cap.status,
            EvidencePollStatus::Served,
            "healthy replacements serve even at runtime_abandoned == cap"
        );

        // Wedge ONE replacement: its grace expires while the runtime budget
        // is exhausted, so it CANNOT be abandoned — the degraded/open
        // circuit, naming the blocked stuck worker. Healthy slots still
        // serve.
        let outcome = poll_on_executor(
            executor.clone(),
            Arc::new(GatedBlockingProvider { gate: gate.clone() }),
            session(),
            query(),
            Duration::from_millis(20),
        )
        .await;
        assert!(matches!(
            outcome.status,
            EvidencePollStatus::TimedOut { .. }
        ));
        tokio::time::sleep(Duration::from_millis(60)).await;
        executor.maintain();
        tokio::time::sleep(Duration::from_millis(60)).await;
        executor.maintain();
        let stats = executor.stats();
        assert_eq!(
            stats.runtime_abandoned, 4,
            "no fifth physical thread may be abandoned: {stats:?}"
        );
        assert_eq!(stats.runtime_abandoned_total, 4, "{stats:?}");
        assert_eq!(
            stats.circuit,
            EvidenceCircuitState::Open {
                abandoned: 4,
                cap: 4,
                blocked: 1
            },
            "{stats:?}"
        );
        assert_eq!(
            stats.workers, 2,
            "the stuck slot is not silently dropped: {stats:?}"
        );
        assert_eq!(stats.workers_quarantined, 1, "{stats:?}");
        let served_degraded = poll_on_executor(
            executor.clone(),
            Arc::new(ServingProvider),
            session(),
            query(),
            Duration::from_millis(500),
        )
        .await;
        assert_eq!(
            served_degraded.status,
            EvidencePollStatus::Served,
            "healthy replacements keep serving while the stuck slot waits"
        );

        // Physical growth is bounded by workers + the absolute runtime cap.
        let live_now = EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst);
        assert!(
            live_now <= live_before + 2 + 4,
            "live evidence threads must stay within workers + cap: before={live_before} now={live_now}"
        );
        if let (Some(before), Some(after)) = (threads_before, live_thread_count()) {
            assert!(
                after <= before + 2 + 4 + 64,
                "OS thread growth must stay bounded (workers + cap + generous slack for parallel tests): before={before} after={after}"
            );
        }

        // RECOVERY: the stuck provider returns (test seam) -> the abandoned
        // threads drain -> the circuit closes -> capacity is restored.
        gate.release_all();
        let deadline = Instant::now() + Duration::from_secs(10);
        while executor.stats().runtime_abandoned > 0 {
            assert!(Instant::now() < deadline, "abandoned threads never drained");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        executor.maintain();
        let stats = executor.stats();
        assert_eq!(stats.circuit, EvidenceCircuitState::Closed, "{stats:?}");
        assert_eq!(stats.workers, 2, "capacity restored: {stats:?}");
        assert_eq!(stats.workers_healthy, 2, "{stats:?}");
        assert_eq!(
            stats.runtime_abandoned_total, 4,
            "the abandonment history stays observable: {stats:?}"
        );
        let recovered = poll_on_executor(
            executor.clone(),
            Arc::new(ServingProvider),
            session(),
            query(),
            Duration::from_millis(500),
        )
        .await;
        assert_eq!(
            recovered.status,
            EvidencePollStatus::Served,
            "recovered capacity serves evidence again"
        );
        // A drained pool shuts down cleanly, and the disposition persists.
        assert_eq!(
            executor.shutdown(Duration::from_secs(2)),
            EvidenceExecutorShutdownState::Clean
        );
        assert_eq!(
            executor.shutdown(Duration::from_secs(2)),
            EvidenceExecutorShutdownState::Clean
        );
        drop(executor);
        assert_eq!(
            EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst),
            live_before,
            "no evidence worker may outlive a clean shutdown"
        );
    }

    /// P1 regression (the EXACT production configuration: 4 workers with a
    /// runtime abandonment cap of 4): retiring all four originals must not
    /// starve evidence. Replacement logical slots are restored even at
    /// `runtime_abandoned == cap`, submit keeps serving, and the degraded
    /// circuit opens only when a stuck worker NEEDS an abandonment the
    /// exhausted budget refuses. Physical evidence threads stay within the
    /// documented `workers + cap` bound.
    #[tokio::test]
    async fn production_config_serves_evidence_after_all_four_originals_are_retired() {
        let _guard = HEAVY_TESTS.lock().await;
        warm_global_executor().await;
        let live_before = EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst);
        let gate = Arc::new(BlockGate::default());
        let executor = EvidenceExecutor::start_with_policy(
            EVIDENCE_EXECUTOR_WORKERS,
            EVIDENCE_EXECUTOR_QUEUE_CAPACITY,
            EvidenceRetirementPolicy {
                retirement_deadline: Duration::from_millis(100),
                quarantine_grace: Duration::from_millis(100),
                max_abandoned: EVIDENCE_EXECUTOR_MAX_ABANDONED_THREADS,
            },
        );
        assert_eq!(executor.worker_count(), EVIDENCE_EXECUTOR_WORKERS);

        // Wedge all four originals.
        for _ in 0..EVIDENCE_EXECUTOR_WORKERS {
            let outcome = poll_on_executor(
                executor.clone(),
                Arc::new(GatedBlockingProvider { gate: gate.clone() }),
                session(),
                query(),
                Duration::from_millis(5),
            )
            .await;
            assert!(
                matches!(outcome.status, EvidencePollStatus::TimedOut { .. }),
                "a wedged poll times out typed: {:?}",
                outcome.status
            );
        }
        assert_eq!(gate.entered(), 4, "all four originals reached the provider");
        tokio::time::sleep(Duration::from_millis(150)).await;
        executor.maintain();
        assert_eq!(
            executor.stats().workers_quarantined,
            4,
            "{:?}",
            executor.stats()
        );
        tokio::time::sleep(Duration::from_millis(150)).await;
        executor.maintain();
        let stats = executor.stats();
        assert_eq!(
            stats.runtime_abandoned, 4,
            "all four originals retired: {stats:?}"
        );
        assert_eq!(stats.runtime_abandoned_total, 4, "{stats:?}");
        assert_eq!(
            stats.workers, 4,
            "four REPLACEMENT logical slots at the cap: {stats:?}"
        );
        assert_eq!(
            stats.circuit,
            EvidenceCircuitState::Closed,
            "at the cap no stuck worker needs abandonment yet: {stats:?}"
        );

        // The cap must NOT reject healthy work: a normal provider is Served.
        let served = poll_on_executor(
            executor.clone(),
            Arc::new(ServingProvider),
            session(),
            query(),
            Duration::from_millis(500),
        )
        .await;
        assert_eq!(
            served.status,
            EvidencePollStatus::Served,
            "replacements keep serving at runtime_abandoned == cap"
        );

        // Wedge ONE replacement: the exhausted runtime budget refuses the
        // fifth physical abandonment and the degraded circuit state opens.
        let outcome = poll_on_executor(
            executor.clone(),
            Arc::new(GatedBlockingProvider { gate: gate.clone() }),
            session(),
            query(),
            Duration::from_millis(5),
        )
        .await;
        assert!(matches!(
            outcome.status,
            EvidencePollStatus::TimedOut { .. }
        ));
        tokio::time::sleep(Duration::from_millis(150)).await;
        executor.maintain();
        tokio::time::sleep(Duration::from_millis(150)).await;
        executor.maintain();
        let stats = executor.stats();
        assert_eq!(
            stats.runtime_abandoned, 4,
            "no fifth physical thread may be abandoned: {stats:?}"
        );
        assert_eq!(stats.runtime_abandoned_total, 4, "{stats:?}");
        assert_eq!(
            stats.circuit,
            EvidenceCircuitState::Open {
                abandoned: 4,
                cap: 4,
                blocked: 1
            },
            "{stats:?}"
        );
        assert_eq!(
            stats.workers, 4,
            "the stuck slot is not silently dropped: {stats:?}"
        );
        assert_eq!(stats.workers_quarantined, 1, "{stats:?}");
        let served = poll_on_executor(
            executor.clone(),
            Arc::new(ServingProvider),
            session(),
            query(),
            Duration::from_millis(500),
        )
        .await;
        assert_eq!(
            served.status,
            EvidencePollStatus::Served,
            "healthy replacements keep serving while the stuck slot waits"
        );

        // Documented physical bound: workers + cap = 8 evidence threads.
        let live_now = EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst);
        assert!(
            live_now <= live_before + EVIDENCE_EXECUTOR_WORKERS + EVIDENCE_EXECUTOR_MAX_ABANDONED_THREADS,
            "physical evidence threads must stay within workers + cap: before={live_before} now={live_now}"
        );

        // Drain: the gate releases the wedged providers (originals + the
        // stuck replacement) -> abandoned threads exit, circuit closes.
        gate.release_all();
        let deadline = Instant::now() + Duration::from_secs(10);
        while executor.stats().runtime_abandoned > 0 {
            assert!(Instant::now() < deadline, "abandoned threads never drained");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        executor.maintain();
        let stats = executor.stats();
        assert_eq!(stats.circuit, EvidenceCircuitState::Closed, "{stats:?}");
        assert_eq!(stats.workers, 4, "capacity stays restored: {stats:?}");
        assert_eq!(stats.workers_healthy, 4, "{stats:?}");
        assert_eq!(stats.runtime_abandoned_total, 4, "{stats:?}");
        assert_eq!(stats.shutdown_abandoned, 0, "{stats:?}");
        assert_eq!(
            executor.shutdown(Duration::from_secs(2)),
            EvidenceExecutorShutdownState::Clean
        );
        drop(executor);
        assert_eq!(
            EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst),
            live_before,
            "no evidence worker may outlive a clean shutdown"
        );
    }

    /// P1 regression: a quarantine belongs to the JOB that timed out, not to
    /// the worker id. Job A exceeds the retirement deadline and is
    /// quarantined, then RECOVERS before the grace; the same worker starts
    /// healthy job B. When A's OLD grace expires, B must NOT be retired and
    /// no abandonment may be charged.
    #[tokio::test]
    async fn recovered_job_quarantine_never_retires_the_next_job_on_the_same_worker() {
        let _guard = HEAVY_TESTS.lock().await;
        warm_global_executor().await;
        let live_before = EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst);
        let gate_a = Arc::new(BlockGate::default());
        let gate_b = Arc::new(BlockGate::default());
        let executor = EvidenceExecutor::start_with_policy(
            1,
            2,
            EvidenceRetirementPolicy {
                retirement_deadline: Duration::from_millis(30),
                quarantine_grace: Duration::from_millis(200),
                max_abandoned: 2,
            },
        );
        let deadline = Instant::now() + Duration::from_secs(10);

        // Job A: wedges the only worker past its retirement deadline.
        let a_task = tokio::spawn(poll_on_executor(
            executor.clone(),
            Arc::new(GatedBlockingProvider {
                gate: gate_a.clone(),
            }),
            session(),
            query(),
            Duration::from_secs(5),
        ));
        while gate_a.entered() == 0 {
            assert!(
                Instant::now() < deadline,
                "job A never reached the provider"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        executor.maintain();
        let stats = executor.stats();
        assert_eq!(stats.workers_quarantined, 1, "{stats:?}");
        assert_eq!(stats.runtime_abandoned, 0, "{stats:?}");

        // Job B is queued while A is still executing (submit runs maintain,
        // which must keep A's quarantine: A is still the executing job).
        let b_task = tokio::spawn(poll_on_executor(
            executor.clone(),
            Arc::new(GatedBlockingProvider {
                gate: gate_b.clone(),
            }),
            session(),
            query(),
            Duration::from_secs(5),
        ));
        // A recovers BEFORE its grace expires; the same worker starts B.
        gate_a.release_all();
        while gate_b.entered() == 0 {
            assert!(
                Instant::now() < deadline,
                "job B never reached the provider"
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        // Let A's OLD grace expire while B executes on the same worker.
        tokio::time::sleep(Duration::from_millis(250)).await;
        executor.maintain();
        let stats = executor.stats();
        assert_eq!(
            stats.runtime_abandoned, 0,
            "B must not inherit A's expired quarantine: {stats:?}"
        );
        assert_eq!(stats.runtime_abandoned_total, 0, "{stats:?}");
        assert_eq!(stats.workers, 1, "the worker survives: {stats:?}");
        assert_eq!(stats.circuit, EvidenceCircuitState::Closed, "{stats:?}");

        // B (still blocked) may carry its OWN fresh quarantine; releasing it
        // lets the worker serve both recovered jobs.
        gate_b.release_all();
        let b_outcome = b_task.await.unwrap();
        assert_eq!(
            b_outcome.status,
            EvidencePollStatus::Served,
            "job B was never retired"
        );
        let a_outcome = a_task.await.unwrap();
        assert_eq!(
            a_outcome.status,
            EvidencePollStatus::Served,
            "job A recovered in time"
        );
        executor.maintain();
        let stats = executor.stats();
        assert_eq!(stats.workers_quarantined, 0, "{stats:?}");
        assert_eq!(stats.runtime_abandoned_total, 0, "{stats:?}");
        assert_eq!(executor.worker_count(), 1);
        assert_eq!(
            executor.shutdown(Duration::from_secs(2)),
            EvidenceExecutorShutdownState::Clean
        );
        drop(executor);
        assert_eq!(EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst), live_before);
    }

    /// P2: runtime retirement and shutdown abandonment are DIFFERENT budgets.
    /// Only runtime abandonment is subject to `max_abandoned`; shutdown
    /// abandonment is bounded by the fixed worker count. The stats expose
    /// both, and the public invariant `runtime_abandoned <= max_abandoned`
    /// stays true even with a shutdown abandonment on top.
    #[tokio::test]
    async fn shutdown_abandonment_is_outside_the_runtime_budget() {
        let _guard = HEAVY_TESTS.lock().await;
        warm_global_executor().await;
        let live_before = EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst);
        let gate = Arc::new(BlockGate::default());
        let executor = EvidenceExecutor::start_with_policy(
            2,
            4,
            EvidenceRetirementPolicy {
                retirement_deadline: Duration::from_millis(20),
                quarantine_grace: Duration::from_millis(20),
                max_abandoned: 1,
            },
        );
        // Runtime retirement: one wedged worker is quarantined, then
        // abandoned (the whole runtime budget).
        let wedged = poll_on_executor(
            executor.clone(),
            Arc::new(GatedBlockingProvider { gate: gate.clone() }),
            session(),
            query(),
            Duration::from_millis(10),
        )
        .await;
        assert!(matches!(wedged.status, EvidencePollStatus::TimedOut { .. }));
        tokio::time::sleep(Duration::from_millis(30)).await;
        executor.maintain();
        tokio::time::sleep(Duration::from_millis(30)).await;
        executor.maintain();
        let stats = executor.stats();
        assert_eq!(stats.runtime_abandoned, 1, "{stats:?}");
        assert_eq!(stats.runtime_abandoned_total, 1, "{stats:?}");
        assert_eq!(stats.shutdown_abandoned, 0, "{stats:?}");

        // Wedge the remaining owned worker: its grace expires while the
        // runtime budget is exhausted -> blocked/degraded circuit.
        let wedged = poll_on_executor(
            executor.clone(),
            Arc::new(GatedBlockingProvider { gate: gate.clone() }),
            session(),
            query(),
            Duration::from_millis(10),
        )
        .await;
        assert!(matches!(wedged.status, EvidencePollStatus::TimedOut { .. }));
        tokio::time::sleep(Duration::from_millis(30)).await;
        executor.maintain();
        tokio::time::sleep(Duration::from_millis(30)).await;
        executor.maintain();
        let stats = executor.stats();
        assert_eq!(stats.runtime_abandoned, 1, "{stats:?}");
        assert_eq!(
            stats.circuit,
            EvidenceCircuitState::Open {
                abandoned: 1,
                cap: 1,
                blocked: 1
            },
            "{stats:?}"
        );

        // Shutdown abandons the blocked slot OUTSIDE the runtime budget: the
        // invariant is not violated and both counters are observable.
        let disposition = executor.shutdown(Duration::from_millis(50));
        assert_eq!(
            disposition,
            EvidenceExecutorShutdownState::Abandoned { workers: 2 },
            "1 runtime + 1 shutdown abandoned thread"
        );
        let stats = executor.stats();
        assert_eq!(stats.runtime_abandoned, 1, "{stats:?}");
        assert_eq!(stats.shutdown_abandoned, 1, "{stats:?}");
        assert!(
            stats.runtime_abandoned <= stats.max_abandoned,
            "only runtime abandonment is subject to max_abandoned: {stats:?}"
        );
        assert_eq!(stats.max_abandoned, 1, "{stats:?}");

        // Both stuck providers return: the two abandoned physical threads
        // drain and each counter drops against its own budget.
        gate.release_all();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let stats = executor.stats();
            if stats.runtime_abandoned == 0 && stats.shutdown_abandoned == 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "abandoned threads never drained: {stats:?}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let stats = executor.stats();
        assert_eq!(stats.runtime_abandoned_total, 1, "{stats:?}");
        assert_eq!(stats.shutdown_abandoned_total, 1, "{stats:?}");
        assert_eq!(executor.worker_count(), 0);
        drop(executor);
        assert_eq!(EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst), live_before);
    }

    /// P2 regression: admission gates on RUNNABLE OWNED slots, never on
    /// `running` (physical threads including detached ones). A pool whose
    /// logical slots are all retired must refuse typed even while a detached
    /// thread is still alive — otherwise the poll would only wait for the
    /// caller's budget to fire.
    #[tokio::test]
    async fn submit_refuses_typed_when_only_detached_workers_survive() {
        let _guard = HEAVY_TESTS.lock().await;
        warm_global_executor().await;
        let live_before = EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst);
        let gate = Arc::new(BlockGate::default());
        let executor = EvidenceExecutor::start_with_policy(
            1,
            2,
            EvidenceRetirementPolicy {
                retirement_deadline: Duration::from_millis(20),
                quarantine_grace: Duration::from_millis(20),
                max_abandoned: 2,
            },
        );
        // Wedge the only worker so it registers as executing...
        let wedged = tokio::spawn(poll_on_executor(
            executor.clone(),
            Arc::new(GatedBlockingProvider { gate: gate.clone() }),
            session(),
            query(),
            Duration::from_secs(5),
        ));
        let deadline = Instant::now() + Duration::from_secs(10);
        while gate.entered() == 0 {
            assert!(Instant::now() < deadline, "the wedged poll never started");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        // ...then detach it directly: no logical slot remains owned, while
        // the physical thread is still alive (`running > 0`).
        let (worker_id, job_id) = {
            let executing = executor
                .shared
                .executing
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let (job_id, job) = executing
                .iter()
                .next()
                .expect("the wedged job is executing");
            (job.worker_id, *job_id)
        };
        assert_eq!(
            executor.abandon_worker(worker_id, job_id),
            AbandonOutcome::Abandoned
        );
        assert_eq!(executor.worker_count(), 0, "no owned logical slot remains");
        assert!(
            executor.shared.running.load(Ordering::SeqCst) > 0,
            "the detached physical thread is still alive"
        );
        let stats = executor.stats();
        assert_eq!(stats.runtime_abandoned, 1, "{stats:?}");
        // Simulate the OS refusing replacement threads: the pool stays at
        // zero owned slots while the detached thread survives.
        executor.restore_refused.store(true, Ordering::SeqCst);

        let enqueued_before = executor.stats().enqueued;
        let refused = poll_on_executor(
            executor.clone(),
            Arc::new(ServingProvider),
            session(),
            query(),
            Duration::from_millis(50),
        )
        .await;
        match &refused.status {
            EvidencePollStatus::NotSpawned { message } => {
                assert!(message.contains("no runnable worker"), "{message}");
            }
            other => panic!("a detached-only pool must refuse typed, got {other:?}"),
        }
        assert!(refused.status.is_degraded());
        assert_eq!(
            executor.stats().enqueued,
            enqueued_before,
            "no job may be queued behind a detached-only pool"
        );
        assert_eq!(
            executor
                .shared
                .queue
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .len(),
            0
        );
        executor.restore_refused.store(false, Ordering::SeqCst);

        // Release the detached provider: its thread drains.
        gate.release_all();
        let outcome = wedged.await.unwrap();
        assert_eq!(outcome.status, EvidencePollStatus::Served);
        let deadline = Instant::now() + Duration::from_secs(10);
        while executor.stats().runtime_abandoned > 0 {
            assert!(
                Instant::now() < deadline,
                "the detached thread never drained"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        drop(executor);
        assert_eq!(EVIDENCE_WORKERS_LIVE.load(Ordering::SeqCst), live_before);
    }

    /// P1/P2 terminal behavior: a provider that NEVER returns leaves the
    /// executor DEGRADED — the runtime abandonment budget is exhausted while
    /// a stuck worker still needs abandonment, so the circuit is OPEN; the
    /// process is never killed, and a poll that cannot be admitted is
    /// refused typed (naming the exhausted budget). Shutdown abandons the
    /// blocked slot OUTSIDE the runtime budget and the persisted disposition
    /// reports the honest total on every call (never the old
    /// `false`-then-`true` lie).
    #[tokio::test]
    async fn forever_blocked_provider_opens_the_circuit_and_shutdown_stays_abandoned() {
        let _guard = HEAVY_TESTS.lock().await;
        warm_global_executor().await;
        let executor = EvidenceExecutor::start_with_policy(
            1,
            2,
            EvidenceRetirementPolicy {
                retirement_deadline: Duration::from_millis(20),
                quarantine_grace: Duration::from_millis(20),
                max_abandoned: 2,
            },
        );
        assert_eq!(
            executor.shutdown_state(),
            EvidenceExecutorShutdownState::Running
        );

        // Wedge the single worker, then its first replacement: two runtime
        // abandonments reach the absolute cap.
        for _ in 0..2 {
            let outcome = poll_on_executor(
                executor.clone(),
                Arc::new(SyncBlockingProvider),
                session(),
                query(),
                Duration::from_millis(10),
            )
            .await;
            assert!(matches!(
                outcome.status,
                EvidencePollStatus::TimedOut { .. }
            ));
            tokio::time::sleep(Duration::from_millis(30)).await;
            executor.maintain();
            tokio::time::sleep(Duration::from_millis(30)).await;
            executor.maintain();
        }
        let stats = executor.stats();
        assert_eq!(stats.runtime_abandoned, 2, "the absolute cap: {stats:?}");
        assert_eq!(
            stats.workers, 1,
            "the replacement slot is restored: {stats:?}"
        );
        assert_eq!(
            stats.circuit,
            EvidenceCircuitState::Closed,
            "at the cap no stuck worker needs abandonment yet: {stats:?}"
        );

        // Wedge the second replacement: its grace expires while the runtime
        // budget is exhausted, so the slot cannot be reclaimed — the circuit
        // is OPEN and the degradation is typed.
        let outcome = poll_on_executor(
            executor.clone(),
            Arc::new(SyncBlockingProvider),
            session(),
            query(),
            Duration::from_millis(10),
        )
        .await;
        assert!(matches!(
            outcome.status,
            EvidencePollStatus::TimedOut { .. }
        ));
        tokio::time::sleep(Duration::from_millis(30)).await;
        executor.maintain();
        tokio::time::sleep(Duration::from_millis(30)).await;
        executor.maintain();
        let stats = executor.stats();
        assert_eq!(stats.runtime_abandoned, 2, "{stats:?}");
        assert_eq!(
            stats.circuit,
            EvidenceCircuitState::Open {
                abandoned: 2,
                cap: 2,
                blocked: 1
            },
            "{stats:?}"
        );

        // Fill the bounded queue (the only owned worker is stuck): the next
        // poll cannot be admitted and is refused with the typed circuit
        // status, whose message names the exhausted abandonment budget.
        let enqueued_before = executor.stats().enqueued;
        let mut queued = Vec::new();
        for _ in 0..2 {
            queued.push(tokio::spawn(poll_on_executor(
                executor.clone(),
                Arc::new(SyncBlockingProvider),
                session(),
                query(),
                Duration::from_secs(30),
            )));
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while executor.stats().enqueued < enqueued_before + 2 {
            assert!(Instant::now() < deadline, "queued polls never enqueued");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let refused = poll_on_executor(
            executor.clone(),
            Arc::new(ServingProvider),
            session(),
            query(),
            Duration::from_millis(50),
        )
        .await;
        match &refused.status {
            EvidencePollStatus::CircuitOpen {
                abandoned,
                cap,
                message,
            } => {
                assert_eq!((*abandoned, *cap), (2, 2));
                assert!(
                    message.contains("budget is exhausted"),
                    "the refusal must name the exhausted abandonment budget: {message}"
                );
            }
            other => {
                panic!("a full queue on the open circuit must be refused typed, got {other:?}")
            }
        }
        assert!(refused.status.is_degraded());

        // Shutdown: the blocked slot is abandoned OUTSIDE the runtime
        // budget; the terminal disposition names the honest total (2 runtime
        // + 1 shutdown) and REPEATS identically.
        let first = executor.shutdown(Duration::from_millis(50));
        assert_eq!(
            first,
            EvidenceExecutorShutdownState::Abandoned { workers: 3 },
            "2 runtime + 1 shutdown abandoned physical threads"
        );
        let stats = executor.stats();
        assert_eq!(stats.runtime_abandoned, 2, "{stats:?}");
        assert!(
            stats.runtime_abandoned <= stats.max_abandoned,
            "the public runtime invariant holds: {stats:?}"
        );
        assert_eq!(stats.shutdown_abandoned, 1, "{stats:?}");
        assert_eq!(stats.runtime_abandoned_total, 2, "{stats:?}");
        assert_eq!(stats.shutdown_abandoned_total, 1, "{stats:?}");
        assert_eq!(executor.shutdown_state(), first);
        let second = executor.shutdown(Duration::from_millis(50));
        assert_eq!(
            second, first,
            "repeated shutdown must report the SAME terminal disposition"
        );
        assert_eq!(executor.worker_count(), 0);
        for poll in queued {
            match poll.await.unwrap().status {
                EvidencePollStatus::NotSpawned { .. } => {}
                other => panic!("a shutdown-cancelled poll must be typed, got {other:?}"),
            }
        }
        drop(executor);
        // The three forever-blocked threads cannot be killed from safe Rust:
        // they stay alive in this test process, bounded by workers + cap —
        // exactly the documented terminal behavior.
    }

    /// P2: the shutdown disposition is `Running` before the first call, then
    /// `Clean` on every call — never a value that flips to `true` merely
    /// because the join handles were forgotten.
    #[tokio::test]
    async fn shutdown_disposition_is_running_then_persistently_clean() {
        let _guard = HEAVY_TESTS.lock().await;
        let executor = EvidenceExecutor::start(2, 4);
        assert_eq!(
            executor.shutdown_state(),
            EvidenceExecutorShutdownState::Running
        );
        assert_eq!(
            executor.shutdown(Duration::from_secs(2)),
            EvidenceExecutorShutdownState::Clean
        );
        assert_eq!(
            executor.shutdown(Duration::from_millis(1)),
            EvidenceExecutorShutdownState::Clean,
            "the persisted disposition is returned without waiting again"
        );
        assert_eq!(
            executor.shutdown_state(),
            EvidenceExecutorShutdownState::Clean
        );
        assert_eq!(executor.worker_count(), 0);
        drop(executor);
    }
}

/// P2 structural proof: the legacy `Option`-shaped off-turn wrapper is
/// DELETED, not merely unused. The typed `run_off_turn_thread_outcome` is the
/// only bridge, so a panic or an unschedulable bridge can never collapse
/// into `None` again.
#[cfg(test)]
mod off_turn_wrapper_is_deleted {
    #[test]
    fn production_sources_never_reference_the_option_wrapper() {
        // Assembled at runtime so this test's own source can never satisfy
        // (or accidentally trip) the needle it searches for.
        let needle = ["run_off_turn", "thread("].concat();
        let mut sources = String::new();
        for file in [
            "lib.rs",
            "runtime.rs",
            "tool.rs",
            "tool_json.rs",
            "loop_detect.rs",
            "stall.rs",
        ] {
            let path = format!("{}/src/{file}", env!("CARGO_MANIFEST_DIR"));
            sources.push_str(&std::fs::read_to_string(&path).unwrap_or_default());
        }
        assert!(
            !sources.contains(&needle),
            "the Option-shaped off-turn wrapper must stay deleted: every production caller maps the typed outcome to an explicit evidence degradation"
        );
    }
}

// ---------------------------------------------------------------- service

/// VerificationService unit coverage (P0-9/10): scripted backend mapping,
/// disabled semantics, policy budgets and the REAL async executor path.
#[cfg(test)]
mod verification_service_tests {
    use super::*;
    use faktor_core::cancellation::CancellationToken;
    use faktor_verify::exec::{
        CheckCategory, CheckKind, CheckSpec, VerificationContext, VerificationPolicy,
    };

    fn ctx_in(dir: &std::path::Path) -> VerificationContext {
        VerificationContext {
            session_id: 7,
            task_id: 9,
            operation_id: 11,
            workspace_id: 3,
            worktree_id: 1,
            root: dir.to_path_buf(),
            deadline: std::time::Instant::now() + std::time::Duration::from_secs(30),
            cancellation: CancellationToken::new(),
        }
    }

    fn quick_spec(id: &str, program: &str, args: &[&str]) -> CheckSpec {
        CheckSpec::new(
            id,
            CheckKind::Compile,
            CheckCategory::Quick,
            program,
            args.iter().copied(),
            true,
        )
    }

    #[tokio::test]
    async fn scripted_backend_maps_ok_to_passed_and_err_to_failed_with_command_text() {
        let calls: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let calls2 = calls.clone();
        let service = VerificationService::fake(move |cmd: &str| {
            calls2.lock().unwrap().push(cmd.to_string());
            if cmd.starts_with("bad") {
                Err("boom".to_string())
            } else {
                Ok(())
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        let passed = service
            .execute(&quick_spec("a", "cargo", &["check"]), &ctx)
            .await;
        assert_eq!(passed.status, CheckRunStatus::Passed);
        assert_eq!(passed.exit, Some(0));
        assert!(passed.finished_ms >= passed.started_ms);
        let failed = service
            .execute(&quick_spec("b", "bad", &["tool"]), &ctx)
            .await;
        assert_eq!(failed.status, CheckRunStatus::Failed);
        assert_eq!(failed.exit, None);
        assert_eq!(failed.summary.as_deref(), Some("boom"));
        // The scripted backend saw the canonical argv join, never a shell.
        assert_eq!(
            *calls.lock().unwrap(),
            vec!["cargo check".to_string(), "bad tool".to_string()]
        );
    }

    #[tokio::test]
    async fn fake_ok_passes_every_check_and_disabled_reports_unconfigured() {
        let ok = VerificationService::fake_ok();
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        let out = ok
            .execute(&quick_spec("rust_check", "cargo", &["check"]), &ctx)
            .await;
        assert_eq!(out.status, CheckRunStatus::Passed);
        assert_eq!(out.exit, Some(0));
        assert!(!ok.is_disabled());

        let off = VerificationService::disabled();
        assert!(off.is_disabled());
        assert_eq!(off.policy(), VerificationPolicy::disabled());
        // Zero-budget policy fails closed: nothing may run inline.
        let mut full = quick_spec("full", "cmake", &["--build", "."]);
        full.category = CheckCategory::Full;
        assert!(matches!(
            off.budget_for(&full),
            BudgetDecision::RunAsTaskOwnedOperation
        ));
        // The disabled backend is scripted: it can never persist jobs and
        // its zero budget fails closed under the unit cap.
        assert!(!off.can_persist_jobs());
        assert!(off.policy().unit_max.is_zero());
    }

    #[test]
    fn budget_decisions_follow_policy_not_a_universal_cap() {
        let service = VerificationService::fake_ok();
        assert_eq!(
            service.budget_for(&quick_spec("c", "cargo", &["check"])),
            BudgetDecision::RunInline(std::time::Duration::from_secs(60))
        );
        let mut test_spec = CheckSpec::new(
            "t",
            CheckKind::Test,
            CheckCategory::Unit,
            "cargo",
            ["test", "--lib"],
            true,
        );
        assert_eq!(
            service.budget_for(&test_spec),
            BudgetDecision::RunInline(std::time::Duration::from_secs(600))
        );
        test_spec.category = CheckCategory::Full;
        assert!(matches!(
            service.budget_for(&test_spec),
            BudgetDecision::RunAsTaskOwnedOperation
        ));
        // Scripted command backends (test seams) cannot persist jobs; the
        // real supervisor-backed executor can (audit P0-5/26).
        assert!(!service.can_persist_jobs());
    }

    #[tokio::test]
    async fn real_executor_runs_typed_argv_in_the_context_root() {
        let service = VerificationService::new(
            Arc::new(faktor_verify::exec::AsyncCheckExecutor::new()),
            VerificationPolicy::default(),
        );
        assert!(!service.is_disabled());
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        let out = service
            .execute(&quick_spec("echo", "echo", &["root-marker"]), &ctx)
            .await;
        assert_eq!(out.status, CheckRunStatus::Passed, "{out:?}");
        assert_eq!(out.exit, Some(0));
        assert!(
            out.summary
                .as_deref()
                .unwrap_or_default()
                .contains("root-marker"),
            "real executor captured the child stdout: {out:?}"
        );
    }

    #[tokio::test]
    async fn real_executor_unavailable_when_the_program_is_missing() {
        let service = VerificationService::new(
            Arc::new(faktor_verify::exec::AsyncCheckExecutor::new()),
            VerificationPolicy::default(),
        );
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx_in(dir.path());
        let out = service
            .execute(
                &quick_spec("ghost", "/nonexistent-tool-for-tests", &[]),
                &ctx,
            )
            .await;
        assert_eq!(out.status, CheckRunStatus::Unavailable);
        assert!(out
            .summary
            .as_deref()
            .unwrap_or_default()
            .contains("not found"));
    }
}

// ---------------------------------------------------------------- policy
// economics (P0-82/15/28): the production policy's stability consult and
// telemetry outcome records, tested adversarially against the real
// RouterService.

#[cfg(test)]
mod economic_policy_tests {
    use super::*;
    use faktor_core::model::{
        MicroUsdPerToken, ModelDescriptor, ModelEconomics, ModelSource, RateLimitState,
    };

    fn desc(provider: &str, model: &str, input_price: u64) -> ModelDescriptor {
        ModelDescriptor {
            provider: provider.into(),
            model: model.into(),
            context: 100_000,
            max_output: 8192,
            tools: true,
            parallel_tools: true,
            reasoning: true,
            thinking: true,
            vision: false,
            structured_output: true,
            embeddings: false,
            streaming: true,
            economics: ModelEconomics {
                input_price_per_mtok: MicroUsdPerToken::from_dollars_per_million(input_price),
                output_price_per_mtok: MicroUsdPerToken::from(1),
                cache_read_price_per_mtok: MicroUsdPerToken::from(input_price / 5),
                cache_write_price_per_mtok: MicroUsdPerToken::from(input_price / 2),
                estimated_latency_ms: 300,
                tool_reliability: 90,
                reasoning_reliability: 90,
                coding_reliability: 90,
                context_reliability: 90,
                availability: 100,
                rate_limit_state: RateLimitState::Healthy,
            },
            source: ModelSource::ProviderCatalog,
        }
    }

    fn economy(pair: Vec<ModelDescriptor>) -> Arc<EconomicRoutingPolicy> {
        EconomicRoutingPolicy::new(
            Arc::new(faktor_router::RouterService::new(pair)),
            RoutingMode::Economy,
        )
    }

    /// Deterministic content digest for the stability series (router tests
    /// use the same FNV construction; identical bytes must hash identically).
    fn tp(id: u64, bytes: &[u8]) -> TurnPrefix {
        let mut h = [0u8; 32];
        let mut acc = 0xcbf29ce484222325u64;
        for &b in bytes {
            acc ^= u64::from(b);
            acc = acc.wrapping_mul(0x100000001b3);
        }
        h[..8].copy_from_slice(&acc.to_le_bytes());
        h[8..16].copy_from_slice(&acc.wrapping_mul(31).to_le_bytes());
        h[16..24].copy_from_slice(&acc.wrapping_mul(97).to_le_bytes());
        h[24..].copy_from_slice(&acc.wrapping_mul(211).to_le_bytes());
        TurnPrefix::new(id, h, bytes.len() as u32)
    }

    fn req() -> faktor_router::RouteRequest {
        faktor_router::RouteRequest {
            context_tokens: 100,
            estimated_output_tokens: 10,
            ..Default::default()
        }
    }

    /// (a) A session recording stability < floor prices its decision with
    /// the churn penalty: same candidates, stability 1.0 vs 0.3 — the
    /// chosen candidate is the same but the DECISION reflects the penalty
    /// (scaled cost + audit), deterministic, and the read-failure case
    /// (no rows) routes without any penalty and never errors the turn.
    #[test]
    fn session_stability_below_floor_inflates_the_decision_and_no_rows_never_penalize() {
        let policy = economy(vec![
            desc("cheap", "cx", 1),  // 100 tokens x 1 + 10 x 1 = 110 micro
            desc("robust", "rx", 2), // 210 micro
        ]);
        let plain = policy.route(&req()).unwrap();
        assert_eq!(
            (plain.provider.as_str(), plain.model.as_str()),
            ("cheap", "cx")
        );
        assert_eq!(plain.estimated_cost_micro, 110);

        // Stability 1.0: byte-identical prefixes — no penalty, decision
        // identical to the plain route.
        let stable_bytes = vec![b's'; 40];
        let stable = [
            tp(1, &stable_bytes),
            tp(2, &stable_bytes),
            tp(3, &stable_bytes),
        ];
        let healthy = policy
            .route_with_session_stability(&req(), Some(&stable))
            .unwrap();
        assert_eq!(healthy, plain, "stable history: no penalty");

        // Stability 0.3: growth 40 -> 130 scores 40/130 = 0.308 < 0.8, so
        // the decision carries the churn premium: cost 110 -> ceil(110 x
        // 1.1538) = 127 and the audit names stability + penalty.
        let churny = [tp(1, &stable_bytes), tp(2, &stable_bytes), {
            let mut t = tp(
                3,
                b"stable-prefix-bytes-grown-longer-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
            );
            t.prefix_tokens = 130;
            t
        }];
        let churned = policy
            .route_with_session_stability(&req(), Some(&churny))
            .unwrap();
        assert_eq!(
            (churned.provider.as_str(), churned.model.as_str()),
            ("cheap", "cx"),
            "the churn penalty scales the decision, it never silently swaps a candidate"
        );
        assert_eq!(churned.estimated_cost_micro, 127);
        assert!(
            churned.reasoning.contains("prefix_stability=0.308"),
            "{}",
            churned.reasoning
        );
        assert!(
            churned.reasoning.contains("churn_penalty=0.1538"),
            "{}",
            churned.reasoning
        );
        // Deterministic: identical history -> identical decision.
        let again = policy
            .route_with_session_stability(&req(), Some(&churny))
            .unwrap();
        assert_eq!(churned, again);
        // Stability read failure (no rows / None / empty): identical to the
        // plain route — no penalty, Ok, never an error on the turn.
        assert_eq!(
            policy.route_with_session_stability(&req(), None).unwrap(),
            plain
        );
        assert_eq!(
            policy
                .route_with_session_stability(&req(), Some(&[]))
                .unwrap(),
            plain
        );
    }

    /// (b) Telemetry outcome records: N settled calls through the policy
    /// update the wrapped RouterService reliability priors ONLY for the
    /// failing (provider, model, phase) instance pair, latency rides the
    /// records, and a rate-limited outcome cooldowns only that provider.
    #[test]
    fn settled_call_outcomes_update_priors_only_for_the_failing_instance_pair() {
        let svc = Arc::new(faktor_router::RouterService::new(vec![
            desc("a", "am", 1),
            desc("b", "bm", 1),
        ]));
        let policy = EconomicRoutingPolicy::new(svc.clone(), RoutingMode::Economy);
        assert_eq!(
            svc.telemetry
                .success_estimate("a", "am", RouterPhase::Implement),
            0.8
        );
        assert_eq!(
            svc.telemetry
                .success_estimate("b", "bm", RouterPhase::Implement),
            0.8
        );
        // N settled calls against (a, am): 9 failures + 1 success, with
        // latency. (b, bm) and every other phase stay untouched.
        for _ in 0..9 {
            policy.record_call_outcome(&SettledCallOutcome {
                provider: "a".into(),
                model: "am".into(),
                phase: RouterPhase::Implement,
                success: false,
                retried: false,
                rate_limited: false,
                latency_ms: 400,
                verified: None,
            });
        }
        policy.record_call_outcome(&SettledCallOutcome {
            provider: "a".into(),
            model: "am".into(),
            phase: RouterPhase::Implement,
            success: true,
            retried: true,
            rate_limited: false,
            latency_ms: 200,
            verified: None,
        });
        let a_after = svc
            .telemetry
            .success_estimate("a", "am", RouterPhase::Implement);
        assert!(
            a_after < 0.8 && a_after > 0.0,
            "(a, am) reliability prior must decay: {a_after}"
        );
        assert_eq!(
            svc.telemetry
                .success_estimate("b", "bm", RouterPhase::Implement),
            0.8,
            "untouched pair keeps its prior exactly"
        );
        assert_eq!(
            svc.telemetry
                .success_estimate("a", "am", RouterPhase::Review),
            0.8,
            "untouched phase keeps its prior exactly"
        );
        let avg = svc
            .telemetry
            .avg_latency_ms("a", "am", RouterPhase::Implement);
        assert!(avg > 200.0 && avg < 400.0, "latency EWMA: {avg}");
        // The priors actually CHANGE routing: the failing pair loses the
        // next route of a comparable request.
        let d = policy.route(&req()).unwrap();
        assert_ne!(
            (d.provider.as_str(), d.model.as_str()),
            ("a", "am"),
            "the decayed pair must lose the next route: {}",
            d.reasoning
        );
        // Rate-limit outcome: cooldown for the limited provider only.
        policy.record_call_outcome(&SettledCallOutcome {
            provider: "b".into(),
            model: "bm".into(),
            phase: RouterPhase::Implement,
            success: false,
            retried: false,
            rate_limited: true,
            latency_ms: 500,
            verified: None,
        });
        assert!(svc.telemetry.cooldown_active("b"));
        assert!(
            !svc.telemetry.cooldown_active("a"),
            "only the limiter cools down"
        );
    }

    /// Pinned mode: the stability consult keeps the pin's decision (fail
    /// closed) but still prices the churn premium into the estimate.
    #[test]
    fn pinned_stability_consult_keeps_the_pin_and_prices_churn() {
        // The pin is the CHEAPEST candidate (validation must let it win the
        // router's own evaluation or the pin is denied); the stability
        // premium then rides the pinned decision's cost.
        let pair = vec![desc("a", "am", 2), desc("b", "bm", 1)];
        let svc = Arc::new(faktor_router::RouterService::new(pair));
        let policy = EconomicRoutingPolicy::new(
            svc,
            RoutingMode::Pinned {
                provider: "b".into(),
                model: "bm".into(),
            },
        );
        let stable = [tp(1, b"same-bytes"), tp(2, b"same-bytes")];
        let d = policy
            .route_with_session_stability(&req(), Some(&stable))
            .unwrap();
        assert_eq!((d.provider.as_str(), d.model.as_str()), ("b", "bm"));
        assert_eq!(d.estimated_cost_micro, 110, "no penalty when stable");
        let churny = [tp(1, b"same-bytes"), {
            let mut t = tp(2, b"rewritten-to-a-different-prefix-bytes");
            t.prefix_tokens = 60;
            t
        }];
        let d2 = policy
            .route_with_session_stability(&req(), Some(&churny))
            .unwrap();
        assert_eq!(
            (d2.provider.as_str(), d2.model.as_str()),
            ("b", "bm"),
            "pin holds"
        );
        assert!(
            d2.estimated_cost_micro > 110,
            "churn premium must ride the pinned estimate: {}",
            d2.estimated_cost_micro
        );
        assert!(d2.reasoning.contains("churn_penalty="));
        // Passthrough pin: stability data changes nothing.
        let svc = Arc::new(faktor_router::RouterService::new(vec![desc("a", "am", 1)]));
        let passthrough = EconomicRoutingPolicy::new(
            svc,
            RoutingMode::Pinned {
                provider: String::new(),
                model: String::new(),
            },
        );
        let d3 = passthrough
            .route_with_session_stability(&req(), Some(&churny))
            .unwrap();
        assert!(d3.provider.is_empty() && d3.model.is_empty());
    }
}

/// Verified-outcome wiring coverage (audit items 13/14/L): the policy's
/// `record_call_outcome` verified entries land in the SAME store-backed
/// registry every route consult reads — appended once per explicit signal,
/// keyed by the FULL (provider, model, phase, task_class, risk_bucket) key,
/// durable across store reopens, and never learned from telemetry-only
/// feeds ("the model said done" is not a verified success).
#[cfg(test)]
mod verified_outcome_wiring_tests {
    use super::*;
    use faktor_core::model::{MicroUsdPerToken, ModelDescriptor, ModelEconomics, ModelSource};
    use faktor_router::OutcomeStore;

    fn desc(provider: &str, model: &str, input: u64, output: u64, rel: u8) -> ModelDescriptor {
        ModelDescriptor {
            provider: provider.into(),
            model: model.into(),
            context: 512_000,
            max_output: 64_000,
            tools: true,
            parallel_tools: true,
            reasoning: false,
            thinking: false,
            vision: false,
            structured_output: false,
            embeddings: false,
            streaming: true,
            economics: ModelEconomics {
                input_price_per_mtok: MicroUsdPerToken::from_dollars_per_million(input),
                output_price_per_mtok: MicroUsdPerToken::from_dollars_per_million(output),
                coding_reliability: rel,
                tool_reliability: rel,
                reasoning_reliability: rel,
                context_reliability: rel,
                ..Default::default()
            },
            source: ModelSource::ProviderCatalog,
        }
    }

    fn implement_req(tokens_in: u64, tokens_out: u64, floor: u8) -> faktor_router::RouteRequest {
        faktor_router::RouteRequest {
            phase: RouterPhase::Implement,
            required_capabilities: vec!["tools".into(), "streaming".into()],
            context_tokens: tokens_in,
            estimated_output_tokens: tokens_out,
            quality_floor: floor,
            task_budget_remaining_micro: 0,
            latency_preference_ms: None,
            ..Default::default()
        }
    }

    #[test]
    fn verified_entries_append_once_fully_keyed_and_telemetry_only_feeds_never_learn() {
        let dir = tempfile::tempdir().unwrap();
        let manager = faktor_session::SessionManager::open(
            dir.path().join("store"),
            dir.path().join("cas"),
            true,
        )
        .unwrap();
        let store = manager.store();
        let outcomes: Arc<dyn OutcomeStore> = Arc::new(StoreOutcomeStore::new(store.clone()));
        let policy = EconomicRoutingPolicy::new(
            Arc::new(faktor_router::RouterService::with_pricing_and_outcomes(
                vec![desc("fake", "m", 1, 3, 82)],
                std::collections::HashMap::new(),
                outcomes,
            )),
            RoutingMode::Economy,
        );
        // Telemetry-only feed (no verified signal — e.g. the settle sites
        // before the gate): the registry learns NOTHING.
        policy.record_call_outcome(&SettledCallOutcome {
            provider: "fake".into(),
            model: "m".into(),
            phase: RouterPhase::Implement,
            success: true,
            retried: false,
            rate_limited: false,
            latency_ms: 100,
            verified: None,
        });
        assert!(
            store
                .model_outcome_stats_get(
                    "fake",
                    "m",
                    RouterPhase::Implement,
                    TaskClass::Medium,
                    RiskBucket::Low,
                )
                .unwrap()
                .is_none(),
            "the model said done — without the verified signal no sample may be learned"
        );
        // A genuine verified-success signal lands ONE success sample under
        // the FULL key, in the durable store.
        let v = VerifiedCallAttribution {
            task_class: TaskClass::Medium,
            risk_bucket: RiskBucket::Low,
            verified_success: true,
            rework_cost_micro: u64::MAX,
            rework_turns: u64::MAX,
        };
        policy.record_call_outcome(&SettledCallOutcome {
            provider: "fake".into(),
            model: "m".into(),
            phase: RouterPhase::Implement,
            success: true,
            retried: false,
            rate_limited: false,
            latency_ms: 200,
            verified: Some(v),
        });
        let row = store
            .model_outcome_stats_get(
                "fake",
                "m",
                RouterPhase::Implement,
                TaskClass::Medium,
                RiskBucket::Low,
            )
            .unwrap()
            .expect("the verified signal must reach the store");
        assert_eq!(row.successes_first_pass, 1);
        assert_eq!(row.failures_first_pass, 0);
        assert_eq!(
            row.rework_cost_micro_sum, 0,
            "a verified first-pass success never carries rework — hostile success numbers are ignored"
        );
        assert_eq!(row.sample_count, 1);
        // A DIFFERENT class/risk bucket stays untouched (full-key writes).
        assert!(store
            .model_outcome_stats_get(
                "fake",
                "m",
                RouterPhase::Implement,
                TaskClass::Hard,
                RiskBucket::High,
            )
            .unwrap()
            .is_none());
        // A failed-verification attribution records a FAILURE sample with
        // its rework under ITS key and never a success.
        let failed = VerifiedCallAttribution {
            task_class: TaskClass::Hard,
            risk_bucket: RiskBucket::High,
            verified_success: false,
            rework_cost_micro: 900_000,
            rework_turns: 2,
        };
        policy.record_call_outcome(&SettledCallOutcome {
            provider: "fake".into(),
            model: "m".into(),
            phase: RouterPhase::Review,
            success: true,
            retried: false,
            rate_limited: false,
            latency_ms: 300,
            verified: Some(failed),
        });
        let row = store
            .model_outcome_stats_get(
                "fake",
                "m",
                RouterPhase::Review,
                TaskClass::Hard,
                RiskBucket::High,
            )
            .unwrap()
            .unwrap();
        assert_eq!(row.successes_first_pass, 0, "no success may be learned");
        assert_eq!(row.failures_first_pass, 1);
        assert_eq!(row.rework_cost_micro_sum, 900_000);
        assert_eq!(row.rework_turns_sum, 2);
        assert_eq!(row.sample_count, 1);
        // The store-backed phase consult folds the class/risk buckets.
        let folded = store
            .model_outcome_stats_phase("fake", "m", RouterPhase::Implement)
            .unwrap()
            .expect("Implement samples exist");
        assert_eq!(folded.successes_first_pass, 1);
        assert_eq!(folded.failures_first_pass, 0);
        let review_folded = store
            .model_outcome_stats_phase("fake", "m", RouterPhase::Review)
            .unwrap()
            .unwrap();
        assert_eq!(review_folded.failures_first_pass, 1);
    }

    #[test]
    fn store_backed_outcomes_serve_routing_after_reopen_and_failures_flip_cheap_to_strong() {
        // Routing after reopen reflects the RECORDED stats: with an empty
        // registry the $4/$30 candidate wins on price; three failed-
        // verification samples recorded against it (cheap's Implement /
        // Medium / Low key) survive a store reopen and flip the decision to
        // the $10/$25 candidate whose conservative expected cost is now
        // below the failure-history estimate. Mirrors the wave-B4 memory
        // registry tests at the router level, through the durable impl.
        let dir = tempfile::tempdir().unwrap();
        let cheap = desc("cheap", "fast", 4, 30, 95);
        let strong = desc("strong", "big", 10, 25, 95);
        let req = implement_req(10_000, 2_000, 60);
        let decide = |policy: &Arc<EconomicRoutingPolicy>| {
            let d = policy.route(&req).unwrap();
            (d.provider.clone(), d.model.clone())
        };
        let first_choice;
        {
            let manager = faktor_session::SessionManager::open(
                dir.path().join("store"),
                dir.path().join("cas"),
                true,
            )
            .unwrap();
            let store = manager.store();
            let policy = EconomicRoutingPolicy::new(
                Arc::new(faktor_router::RouterService::with_pricing_and_outcomes(
                    vec![cheap.clone(), strong.clone()],
                    std::collections::HashMap::new(),
                    Arc::new(StoreOutcomeStore::new(store.clone())),
                )),
                RoutingMode::Economy,
            );
            first_choice = decide(&policy);
            assert_eq!(
                first_choice,
                ("cheap".to_string(), "fast".to_string()),
                "with no verified history the cheaper candidate wins"
            );
            // Three failed-verification gates on cheap's settled calls.
            for _ in 0..3 {
                policy.record_call_outcome(&SettledCallOutcome {
                    provider: "cheap".into(),
                    model: "fast".into(),
                    phase: RouterPhase::Implement,
                    success: true,
                    retried: false,
                    rate_limited: false,
                    latency_ms: 250,
                    verified: Some(VerifiedCallAttribution {
                        task_class: TaskClass::Medium,
                        risk_bucket: RiskBucket::Low,
                        verified_success: false,
                        rework_cost_micro: 960_000,
                        rework_turns: 1,
                    }),
                });
            }
            let row = store
                .model_outcome_stats_get(
                    "cheap",
                    "fast",
                    RouterPhase::Implement,
                    TaskClass::Medium,
                    RiskBucket::Low,
                )
                .unwrap()
                .unwrap();
            assert_eq!(row.failures_first_pass, 3);
            assert_eq!(row.rework_cost_micro_sum, 3 * 960_000);
            // Crash + reopen: the samples live in the store.
        }
        let manager = faktor_session::SessionManager::open(
            dir.path().join("store"),
            dir.path().join("cas"),
            true,
        )
        .unwrap();
        let store = manager.store();
        let reopened = EconomicRoutingPolicy::new(
            Arc::new(faktor_router::RouterService::with_pricing_and_outcomes(
                vec![cheap.clone(), strong.clone()],
                std::collections::HashMap::new(),
                Arc::new(StoreOutcomeStore::new(store.clone())),
            )),
            RoutingMode::Economy,
        );
        let (provider, model) = decide(&reopened);
        assert_eq!(
            (provider.as_str(), model.as_str()),
            ("strong", "big"),
            "the recorded failure history must flip the route away from the cheap candidate"
        );
        let folded = store
            .model_outcome_stats_phase("cheap", "fast", RouterPhase::Implement)
            .unwrap()
            .expect("recorded stats survive the reopen");
        assert_eq!(folded.sample_count, 3);
    }
}

#[cfg(test)]
mod attempt_accounting_tests {
    use super::*;
    use faktor_core::id::TaskId;
    use faktor_core::model::PricingSnapshot;
    use faktor_core::op::ModelCallAttempt;
    use faktor_session::{BudgetAuthority, BudgetError, BudgetView};
    use std::pin::Pin;

    /// Records every authority call behind a NoopBudget — the guard test
    /// proves a misordered refund/uncertain NEVER reaches the authority.
    struct CountingBudget {
        refund_calls: Arc<std::sync::atomic::AtomicUsize>,
        uncertain_calls: Arc<std::sync::atomic::AtomicUsize>,
        settle_calls: Arc<std::sync::atomic::AtomicUsize>,
        dispatch_calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl CountingBudget {
        fn new() -> Self {
            Self {
                refund_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                uncertain_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                settle_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                dispatch_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }
    }

    impl BudgetAuthority for CountingBudget {
        fn reserve(
            &self,
            _s: SessionId,
            _t: TaskId,
            _op: faktor_core::id::OpId,
            _pred: u64,
            _snap: Option<PricingSnapshot>,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<faktor_session::ReservationId, BudgetError>>
                    + Send,
            >,
        > {
            Box::pin(async { Ok(faktor_session::ReservationId::NOOP) })
        }
        fn reserve_attempt(
            &self,
            _s: SessionId,
            _t: TaskId,
            _a: ModelCallAttempt,
            _pred: u64,
            _snap: Option<PricingSnapshot>,
        ) -> Pin<
            Box<
                dyn std::future::Future<Output = Result<faktor_session::ReservationId, BudgetError>>
                    + Send,
            >,
        > {
            Box::pin(async { Ok(faktor_session::ReservationId::NOOP) })
        }
        fn mark_dispatched(
            &self,
            _s: SessionId,
            _r: faktor_session::ReservationId,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<(), BudgetError>> + Send>> {
            let c = self.dispatch_calls.clone();
            Box::pin(async move {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            })
        }
        fn mark_uncertain(
            &self,
            _s: SessionId,
            _r: faktor_session::ReservationId,
            _reason: String,
            _request_id: Option<String>,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<(), BudgetError>> + Send>> {
            let c = self.uncertain_calls.clone();
            Box::pin(async move {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            })
        }
        fn settle_usage(
            &self,
            _s: SessionId,
            _r: faktor_session::ReservationId,
            _a: u64,
            _b: u64,
            _c: u64,
            _d: u64,
            _e: Option<u64>,
            _f: Option<String>,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<Option<u64>, BudgetError>> + Send>>
        {
            let c = self.settle_calls.clone();
            Box::pin(async move {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(None)
            })
        }
        fn refund(
            &self,
            _s: SessionId,
            _r: faktor_session::ReservationId,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<(), BudgetError>> + Send>> {
            let c = self.refund_calls.clone();
            Box::pin(async move {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            })
        }
        fn session_budget_view(
            &self,
            _s: SessionId,
            _t: TaskId,
        ) -> Result<BudgetView, BudgetError> {
            Ok(BudgetView {
                max_cost_micro: None,
                spent_cost_micro: 0,
                open_reserved_micro: 0,
                open_reservations: 0,
                uncertain_reserved_micro: 0,
                uncertain_reservations: 0,
                settled_count: 0,
            })
        }
        fn recover_after_restart(&self) {}
        fn reconcile_uncertain(
            &self,
            _s: SessionId,
            _t: TaskId,
        ) -> Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<faktor_store::CostReconcileReport, BudgetError>,
                    > + Send,
            >,
        > {
            Box::pin(async { Ok(Default::default()) })
        }
        fn finalize_uncertain(
            &self,
            _s: SessionId,
            _t: TaskId,
        ) -> Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<faktor_store::CostFinalizeReport, BudgetError>,
                    > + Send,
            >,
        > {
            Box::pin(async { Ok(Default::default()) })
        }
    }

    fn budget() -> CountingBudget {
        CountingBudget::new()
    }

    #[tokio::test]
    async fn refund_after_dispatch_is_refused_locally_and_never_reaches_the_authority() {
        // The five-runtime-site bug shape: refund AFTER mark_dispatched must
        // be impossible — the machine refuses locally with the ledger's own
        // typed error BEFORE the authority is touched.
        let authority = Arc::new(budget());
        let mut acct = AttemptAccounting::new(
            authority.clone(),
            SessionId::new(1),
            Some(faktor_session::ReservationId::new(7)),
        );
        acct.mark_dispatched().await.unwrap();
        assert!(acct.dispatched() && acct.is_open());
        let err = acct.fail_before_dispatch().await.unwrap_err();
        assert!(
            matches!(
                err,
                faktor_session::BudgetError::CannotRefundDispatched { .. }
            ),
            "{err:?}"
        );
        assert_eq!(
            authority
                .refund_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert!(acct.is_open(), "the refused refund changes nothing");
        // The legal terminal for a dispatched attempt: UNCERTAIN — exactly
        // one authority call, machine closed.
        acct.fail_after_dispatch("provider_error", None)
            .await
            .unwrap();
        assert!(acct.closed());
        assert_eq!(
            authority
                .uncertain_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[tokio::test]
    async fn uncertain_before_dispatch_is_refused_the_attempt_must_refund() {
        // A never-dispatched failure REFUNDS; marking it UNCERTAIN would
        // charge an estimate for a request that provably never left.
        let authority = Arc::new(budget());
        let mut acct = AttemptAccounting::new(
            authority.clone(),
            SessionId::new(1),
            Some(faktor_session::ReservationId::new(8)),
        );
        let err = acct
            .fail_after_dispatch("never_dispatched", None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, faktor_session::BudgetError::NotOpen { .. }),
            "{err:?}"
        );
        assert_eq!(
            authority
                .uncertain_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        acct.fail_before_dispatch().await.unwrap();
        assert!(acct.closed());
        assert_eq!(
            authority
                .refund_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }

    #[tokio::test]
    async fn settle_before_dispatch_and_double_terminal_calls_are_guarded() {
        // The machine lets money move exactly once and only in order:
        // settle requires a dispatched open attempt; a second terminal call
        // on a closed machine is refused without touching the authority.
        let authority = Arc::new(budget());
        let mut acct = AttemptAccounting::new(
            authority.clone(),
            SessionId::new(1),
            Some(faktor_session::ReservationId::new(9)),
        );
        assert!(matches!(
            acct.settle_usage(1, 0, 0, 1, None, None).await.unwrap_err(),
            faktor_session::BudgetError::NotOpen { status, .. } if status == "reserved"
        ));
        assert_eq!(
            authority
                .settle_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        acct.mark_dispatched().await.unwrap();
        acct.settle_usage(100, 0, 0, 10, None, None).await.unwrap();
        assert!(acct.closed());
        assert_eq!(
            authority
                .settle_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        let err = acct.settle_usage(1, 0, 0, 1, None, None).await.unwrap_err();
        assert!(
            matches!(err, faktor_session::BudgetError::NotOpen { .. }),
            "{err:?}"
        );
        let err = acct
            .fail_after_dispatch("double_terminal", None)
            .await
            .unwrap_err();
        assert!(
            matches!(err, faktor_session::BudgetError::NotOpen { .. }),
            "{err:?}"
        );
        let err = acct.fail_before_dispatch().await.unwrap_err();
        assert!(
            matches!(err, faktor_session::BudgetError::NotOpen { .. }),
            "{err:?}"
        );
        assert_eq!(
            authority
                .settle_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        assert_eq!(
            authority
                .uncertain_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(
            authority
                .refund_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }

    #[tokio::test]
    async fn a_machine_without_a_reservation_moves_nothing() {
        // Unbudgeted calls: no money exists to move — dispatch is a silent
        // no-op, the refund path releases nothing, and settle/uncertain
        // (which require a dispatched attempt with a reservation) stay
        // refused without touching the authority.
        let authority = Arc::new(budget());
        let mut acct = AttemptAccounting::new(authority.clone(), SessionId::new(1), None);
        acct.mark_dispatched().await.unwrap();
        assert!(
            !acct.dispatched(),
            "nothing was dispatched (no reservation)"
        );
        assert!(matches!(
            acct.settle_usage(1, 0, 0, 1, None, None).await.unwrap_err(),
            faktor_session::BudgetError::NotOpen { status, .. } if status == "reserved"
        ));
        assert!(matches!(
            acct.fail_after_dispatch("x", None).await.unwrap_err(),
            faktor_session::BudgetError::NotOpen { .. }
        ));
        acct.fail_before_dispatch().await.unwrap();
        assert!(acct.closed());
        assert_eq!(
            authority
                .settle_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(
            authority
                .refund_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(
            authority
                .uncertain_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
    }
}

#[cfg(test)]
mod model_call_intent_tests {
    use super::*;

    #[test]
    fn quality_floors_are_hard_and_never_lowered() {
        // Implement/Review route on a HARD 60 floor; Adaptive carries a
        // target and a never-below minimum; route_request applies the
        // minimum verbatim (the policy decides nothing above it).
        assert_eq!(ModelCallIntent::implement_main().quality_floor(), 60);
        assert_eq!(ModelCallIntent::review().quality_floor(), 60);
        assert_eq!(ModelCallIntent::compact().quality_floor(), 60);
        let adaptive = ModelCallIntent {
            phase: RouterPhase::Implement,
            required_capabilities: vec![],
            quality: QualityRequirement::Adaptive {
                target: 85,
                minimum: 70,
            },
            expected_output_tokens: 2048,
            semantic_risk: 0,
        };
        assert_eq!(adaptive.quality.minimum(), 70);
        assert_eq!(adaptive.quality.target(), 85);
        assert_eq!(adaptive.quality_floor(), 70);
    }

    #[test]
    fn route_request_carries_the_real_planned_dimensions_not_a_guess() {
        let intent = ModelCallIntent::implement_main();
        let req = intent.route_request(12_345, 7_000, 0);
        assert_eq!(req.context_tokens, 12_345, "the plan's real input estimate");
        assert_eq!(req.estimated_output_tokens, 7_000, "the real output cap");
        assert_eq!(req.quality_floor, 60);
        assert_ne!(req.context_tokens, 16_384, "no hard-coded pre-plan guess");
        assert_ne!(req.estimated_output_tokens, 2048, "no hard-coded 2048");
        assert_eq!(intent.phase, RouterPhase::Implement);
    }
}

#[cfg(test)]
mod hard_quality_floor_tests {
    use super::*;
    use faktor_core::model::ModelDescriptor;
    use faktor_router::RouterService;

    fn candidate(quality: u8) -> ModelDescriptor {
        ModelDescriptor {
            provider: "p".into(),
            model: "m".into(),
            context: 128_000,
            max_output: 16_000,
            tools: true,
            parallel_tools: false,
            reasoning: false,
            thinking: false,
            vision: false,
            structured_output: false,
            embeddings: false,
            streaming: true,
            economics: faktor_core::model::ModelEconomics {
                coding_reliability: quality,
                tool_reliability: quality,
                reasoning_reliability: quality,
                context_reliability: quality,
                ..Default::default()
            },
            source: faktor_core::model::ModelSource::ProviderCatalog,
        }
    }

    #[test]
    fn hard_60_with_best_available_50_is_a_no_capable_model_refusal() {
        // Audit (e): quality floors are HARD. The old logic lowered the
        // requested floor toward the best available candidate; the fix
        // refuses typed: requested hard 60 with best available 50 =>
        // NoCapableModel — and a 90-quality candidate serves the SAME
        // request.
        let policy = EconomicRoutingPolicy::new(
            Arc::new(RouterService::new(vec![candidate(50)])),
            RoutingMode::Economy,
        );
        let req = ModelCallIntent::implement_main().route_request(4_000, 2_048, 0);
        assert!(
            matches!(
                policy.route(&req),
                Err(RouteFailure::NoCapableModel)
            ),
            "hard 60 with a best available 50 must be a typed NoCapableModel, never a lowered floor"
        );
        let policy2 = EconomicRoutingPolicy::new(
            Arc::new(RouterService::new(vec![candidate(90)])),
            RoutingMode::Economy,
        );
        assert!(
            policy2.route(&req).is_ok(),
            "an above-floor candidate serves"
        );
        // Balanced raises to its band; MaximumQuality never probes below.
        let bal = EconomicRoutingPolicy::new(
            Arc::new(RouterService::new(vec![candidate(70)])),
            RoutingMode::Balanced,
        );
        assert!(matches!(bal.route(&req), Err(RouteFailure::NoCapableModel)));
        let maxq = EconomicRoutingPolicy::new(
            Arc::new(RouterService::new(vec![candidate(55)])),
            RoutingMode::MaximumQuality,
        );
        assert!(matches!(
            maxq.route(&req),
            Err(RouteFailure::NoCapableModel)
        ));
    }
}

#[cfg(test)]
mod efficiency_flags_tests {
    use super::*;
    use faktor_context::information::FailurePrior;
    use faktor_context::selection::ContextCandidate;

    /// A hostile-value prior standing in for the learning crate's handle:
    /// the value is irrelevant to the gate test.
    struct AlwaysDouble;

    impl FailurePrior for AlwaysDouble {
        fn omission_risk(&self, _candidate: &ContextCandidate) -> f64 {
            2.0
        }
    }

    #[test]
    fn efficiency_flags_default_all_off() {
        let flags = EfficiencyFlags::default();
        assert!(!flags.failure_learning);
        assert!(!flags.ccr);
        assert!(!flags.typed_handoff);
        assert!(!flags.semantic_context);
        assert!(!flags.rework_routing);
    }

    /// The gate: the prior is handed to the planner ONLY when the parsed
    /// `failure_learning` flag is on AND a handle exists. Every other
    /// combination yields `None` (baseline/parity path).
    #[test]
    fn prior_is_applied_only_when_the_flag_is_on_and_a_handle_exists() {
        let prior = AlwaysDouble;
        let handle: Option<&(dyn FailurePrior + Send + Sync)> = Some(&prior);
        let off = EfficiencyFlags::default();
        assert!(off.context_prior(handle).is_none(), "off => no prior");
        let on = EfficiencyFlags {
            failure_learning: true,
            ..Default::default()
        };
        assert!(on.context_prior(handle).is_some(), "on + handle => prior");
        assert!(
            on.context_prior(None).is_none(),
            "on + no handle => no prior"
        );
        // Running through the concrete trait object never invokes the
        // prior while the flag is off (the handle is not even observed).
        let gated = off.context_prior(handle);
        assert!(gated.is_none());
    }
}
