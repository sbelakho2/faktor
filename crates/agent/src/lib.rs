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

/// What one model call's output may be trusted to BE (item 26): the
/// explicit trust class of an intent's output. The class gates what the
/// runtime may do with the output — in particular, a
/// [`OutputTrust::ContextCompression`] summary can replace TRANSCRIPT
/// context but can never certify task completion nor replace immutable
/// task facts/ledger rows (those remain the deterministic durable rows'
/// authority).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputTrust {
    /// Throwaway output (titles, labels): never a durable fact, never
    /// completion evidence.
    Ephemeral,
    /// A context-compression summary (compaction): it may stand in for
    /// transcript context, but it certifies NOTHING and overwrites no
    /// immutable task fact or ledger row. Its provenance is recorded as
    /// [`OutputTrust::provenance_tag`].
    ContextCompression,
    /// Implementation work product (code edits, tool calls): a
    /// deterministic gate must still verify it before completion.
    Implementation,
    /// An independent verification opinion (review verdict): evidence the
    /// deterministic gates consume, never a certification by itself.
    VerificationOpinion,
}

impl OutputTrust {
    /// Stable provenance tag recorded wherever this output's origin is
    /// journaled (context-compression provenance included).
    pub const fn provenance_tag(self) -> &'static str {
        match self {
            OutputTrust::Ephemeral => "ephemeral",
            OutputTrust::ContextCompression => "context_compression",
            OutputTrust::Implementation => "implementation",
            OutputTrust::VerificationOpinion => "verification_opinion",
        }
    }

    /// Inverse of [`OutputTrust::provenance_tag`]: decode a durable
    /// provenance tag. An unknown/absent tag decodes to `None` — the
    /// durable readers then refuse (never default to a trusted class).
    pub fn from_provenance_tag(tag: &str) -> Option<Self> {
        match tag {
            "ephemeral" => Some(OutputTrust::Ephemeral),
            "context_compression" => Some(OutputTrust::ContextCompression),
            "implementation" => Some(OutputTrust::Implementation),
            "verification_opinion" => Some(OutputTrust::VerificationOpinion),
            _ => None,
        }
    }

    /// Whether this output may feed the completion-certification path.
    /// Compaction summaries and ephemeral output NEVER can — a compaction
    /// failure (or an accepted summary) cannot certify completion.
    pub const fn certifies_completion(self) -> bool {
        matches!(
            self,
            OutputTrust::Implementation | OutputTrust::VerificationOpinion
        )
    }

    /// Whether this output may replace immutable durable task facts/ledger
    /// rows. Only a verification opinion may author durable evidence rows
    /// (its verdicts); context compression and ephemeral output may never
    /// replace or overwrite any durable row.
    pub const fn may_replace_durable_facts(self) -> bool {
        matches!(self, OutputTrust::VerificationOpinion)
    }

    /// Whether this output may author VERIFICATION evidence (criterion
    /// verdicts, review evidence, completion facts derived from a reviewer
    /// verdict). Only an independent verification opinion may; an
    /// implementation output's code is verified by deterministic gates and
    /// is never authored as evidence itself; context compression and
    /// ephemeral outputs author nothing.
    pub const fn may_author_verification_evidence(self) -> bool {
        matches!(self, OutputTrust::VerificationOpinion)
    }
}

/// ONE model call's output with its explicit trust class and the durable
/// identity of the call that produced it (audit item 3): `value` is the raw
/// text the model produced, `trust` is the trust class of the intent that
/// routed the call (never inferred from content), and `call_id` names the
/// physical call durably (the attempt op id; 0 = no durable identity).
///
/// Every correctness-critical writer (verification evidence, criterion
/// evidence, completion facts, immutable task facts) consumes THIS type —
/// or a [`FactSource`] carrying it — and checks the trust class BEFORE any
/// durable byte moves: [`ModelOutput::completion_fact`] admits the
/// completion-capable classes, [`ModelOutput::verification_evidence`] admits
/// only a verification opinion, and both refuse
/// [`OutputTrust::ContextCompression`]/[`OutputTrust::Ephemeral`] with a
/// typed [`TrustRefusal`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelOutput {
    pub value: String,
    pub trust: OutputTrust,
    /// Durable identity of the physical call that produced the value (the
    /// attempt op id). `0` is the documented "no durable call identity".
    pub call_id: u64,
}

impl ModelOutput {
    pub fn new(value: impl Into<String>, trust: OutputTrust, call_id: u64) -> Self {
        Self {
            value: value.into(),
            trust,
            call_id,
        }
    }

    /// The completion-fact admission: the output may author completion
    /// facts / immutable task facts only when its class is completion
    /// capable ([`OutputTrust::Implementation`] or
    /// [`OutputTrust::VerificationOpinion`]). A context-compression summary
    /// or an ephemeral title is refused typed — the caller never touches a
    /// durable row.
    pub fn completion_fact(&self) -> Result<&Self, TrustRefusal> {
        if self.trust.certifies_completion() {
            Ok(self)
        } else {
            Err(TrustRefusal {
                trust: self.trust,
                required:
                    "completion-capable model output (implementation or verification_opinion)",
            })
        }
    }

    /// The verification-evidence admission: only a
    /// [`OutputTrust::VerificationOpinion`] may author criterion/review
    /// evidence; everything else (including an implementation output) is
    /// refused typed. Context compression can NEVER write verification
    /// evidence.
    pub fn verification_evidence(&self) -> Result<&Self, TrustRefusal> {
        if self.trust.may_author_verification_evidence() {
            Ok(self)
        } else {
            Err(TrustRefusal {
                trust: self.trust,
                required: "a verification opinion (independent reviewer output)",
            })
        }
    }
}

/// Typed refusal of a trust-gated write: names the offending trust class
/// and the capability the writer required. Nothing was written when this is
/// returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustRefusal {
    pub trust: OutputTrust,
    pub required: &'static str,
}

impl std::fmt::Display for TrustRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "model output trust class `{}` cannot author this write (requires {})",
            self.trust.provenance_tag(),
            self.required
        )
    }
}

impl std::error::Error for TrustRefusal {}

/// Where the bytes of one durable fact write came from (audit item 3). A
/// deterministic caller asserts [`FactSource::Durable`] (the value is a
/// projection of durable rows/gate results, NOT model text); a model-derived
/// value MUST ride [`FactSource::Model`], and the fact writer enforces its
/// trust class before touching a row.
#[derive(Debug, Clone, Copy)]
pub(crate) enum FactSource<'a> {
    /// A durable-row/deterministic-gate projection: never model text.
    Durable,
    /// A model call output with its explicit trust class.
    Model(&'a ModelOutput),
}

impl FactSource<'_> {
    /// Enforce the completion-fact admission for this source.
    pub(crate) fn check_completion_fact(self) -> Result<(), TrustRefusal> {
        match self {
            FactSource::Durable => Ok(()),
            FactSource::Model(output) => output.completion_fact().map(|_| ()),
        }
    }

    /// The refusal as a typed core error (the fact writers surface it).
    pub(crate) fn refusal_error(refusal: &TrustRefusal) -> faktor_core::Error {
        faktor_core::Error::new(
            faktor_core::error::ErrorKind::Permission,
            refusal.to_string(),
        )
    }
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
    /// What this call's output may be trusted to BE (item 26): the
    /// compaction/ephemeral intents carry their non-certifying trust class
    /// explicitly, so no downstream path can mistake a summary or a title
    /// for completion evidence or durable task facts.
    pub output_trust: OutputTrust,
}

impl ModelCallIntent {
    /// The hard quality floor the router applies verbatim (never lowered
    /// toward the best available candidate).
    pub fn quality_floor(&self) -> u8 {
        self.quality.minimum().min(100)
    }

    /// The preferred quality target of the call (the minimum for a hard
    /// requirement): a compaction call prefers its 60 target but may relax
    /// to its adaptive minimum.
    pub fn quality_target(&self) -> u8 {
        self.quality.target().min(100)
    }

    /// The output trust class of this call.
    pub fn output_trust(&self) -> OutputTrust {
        self.output_trust
    }

    /// True when this intent may feed completion certification at all
    /// (compaction and ephemeral intents never can).
    pub fn certifies_completion(&self) -> bool {
        self.output_trust.certifies_completion()
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
            // The adaptive distinction rides the request (audit item 2): a
            // Hard requirement sends NO target (the floor is the whole
            // requirement); an Adaptive one sends its preferred target, which
            // is exactly the router's permission to relax into
            // `[minimum, target)` when no target candidate survives.
            quality_target: match self.quality {
                QualityRequirement::Hard { .. } => None,
                QualityRequirement::Adaptive { .. } => Some(self.quality_target()),
            },
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
/// for Implement/Review/Debug — the audit's default; the compaction
/// summarizer runs under the ADAPTIVE floor below its 60 target, and its
/// output trust class can never certify completion).
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
            output_trust: OutputTrust::Implementation,
        }
    }

    /// A Debug-phase call: like Implement, a HARD 60 coding floor; its
    /// output is implementation work product (deterministically verified).
    pub fn debug() -> Self {
        Self {
            phase: RouterPhase::Debug,
            required_capabilities: vec!["tools".into(), "streaming".into()],
            quality: QualityRequirement::Hard { minimum: 60 },
            expected_output_tokens: u64::MAX,
            semantic_risk: 0,
            output_trust: OutputTrust::Implementation,
        }
    }

    /// A Review-phase call (the independent risky-change review): bounded
    /// package input, a small typed verdict output. The HARD 60 floor is
    /// unchanged; the verdict is a verification opinion, never a
    /// certification by itself.
    pub fn review() -> Self {
        Self {
            phase: RouterPhase::Review,
            required_capabilities: vec!["streaming".into()],
            quality: QualityRequirement::Hard { minimum: 60 },
            expected_output_tokens: 2048,
            semantic_risk: 100,
            output_trust: OutputTrust::VerificationOpinion,
        }
    }

    /// The compaction summarizer call (interior, low semantic risk): the
    /// floor is ADAPTIVE — the 60 target is preferred, but the router may
    /// relax to the 50 minimum when the mode's economics decide, so a
    /// lower-quality declared model may summarize. The output trust is
    /// [`OutputTrust::ContextCompression`]: the summary can replace
    /// transcript context and NOTHING else (no completion certification,
    /// no durable task facts/ledger rows).
    pub fn compact() -> Self {
        Self {
            phase: RouterPhase::Compact,
            required_capabilities: vec!["streaming".into()],
            quality: QualityRequirement::Adaptive {
                minimum: 50,
                target: 60,
            },
            expected_output_tokens: 4096,
            semantic_risk: 0,
            output_trust: OutputTrust::ContextCompression,
        }
    }

    /// A Title-phase call: throwaway ephemeral output, never a durable fact
    /// and never completion evidence.
    pub fn title() -> Self {
        Self {
            phase: RouterPhase::Title,
            required_capabilities: vec!["streaming".into()],
            quality: QualityRequirement::Hard { minimum: 60 },
            expected_output_tokens: 512,
            semantic_risk: 0,
            output_trust: OutputTrust::Ephemeral,
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

/// The DESCRIPTOR-ONLY phase quality of one candidate, read through the
/// SAME authority the router's descriptor-only compatibility qualification
/// applies ([`faktor_router::qualified_candidates`] via
/// `clears_quality_floor` -> `QualityStatement::from_legacy_performance`).
/// It deliberately delegates to the router-owned metric instead of mirroring
/// `ModelEconomics::coding_quality()` (the mean of three dimensions), which
/// is NOT what the descriptor floor compares: heavy phases judge
/// `coding_reliability` alone. Tier and qualification therefore never
/// disagree.
fn descriptor_phase_quality(d: &faktor_core::model::ModelDescriptor, phase: RouterPhase) -> u8 {
    faktor_router::phase_quality_value(
        &faktor_core::model::QualityStatement::from_legacy_performance(
            &d.performance(),
            faktor_core::model::QualityAuthority::BuiltInPrior,
        ),
        phase,
    )
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
    #[allow(clippy::too_many_arguments)] // one explicit axis per parameter, mirroring the router
    fn consult_at_with_plans(
        &self,
        req: &faktor_router::RouteRequest,
        floor: u8,
        target: Option<u8>,
        prefix_history: Option<&[TurnPrefix]>,
        planner: Option<&dyn faktor_router::CandidatePlanner>,
        outcomes: Option<&dyn faktor_router::outcomes::OutcomeView>,
        now_ms: u64,
    ) -> Result<faktor_router::SizedRouteDecision, String> {
        let mut routed = req.clone();
        routed.quality_floor = floor;
        // The mode decides whether the adaptive preference may relax: the
        // Economy consult forwards the request's target (the policy's
        // permission), Balanced raises BOTH band edges to its band (never
        // below it), MaximumQuality probes each tier as a hard floor, and a
        // pin validates the minimum only.
        routed.quality_target = target;
        // A route-scoped outcome override (MaximumQuality's memo) makes every
        // probe reuse the same candidate lookups; absent one, the service's
        // own registry is consulted exactly as before.
        let stable_floor = faktor_router::stability::DEFAULT_STABILITY_FLOOR;
        match (planner, prefix_history, outcomes) {
            (Some(planner), None, Some(o)) => {
                self.service
                    .route_with_candidate_plans_at_with(&routed, &[], planner, now_ms, o)
            }
            (Some(planner), None, None) => {
                self.service
                    .route_with_candidate_plans(&routed, &[], planner)
            }
            (Some(planner), Some(history), Some(o)) => self
                .service
                .route_with_prefix_stability_and_candidate_plans_at_with(
                    &routed,
                    &[],
                    stable_floor,
                    Some(history),
                    planner,
                    now_ms,
                    o,
                ),
            (Some(planner), Some(history), None) => self
                .service
                .route_with_prefix_stability_and_candidate_plans(
                    &routed,
                    &[],
                    stable_floor,
                    Some(history),
                    planner,
                ),
            (None, None, Some(o)) => self
                .service
                .route_at_with(&routed, &[], now_ms, o)
                .map(faktor_router::SizedRouteDecision::without_plan),
            (None, None, None) => self
                .service
                .route(&routed, &[])
                .map(faktor_router::SizedRouteDecision::without_plan),
            (None, Some(history), Some(o)) => self
                .service
                .route_with_prefix_stability_at_with(
                    &routed,
                    &[],
                    stable_floor,
                    Some(history),
                    now_ms,
                    o,
                )
                .map(faktor_router::SizedRouteDecision::without_plan),
            (None, Some(history), None) => self
                .service
                .route_with_prefix_stability(&routed, &[], stable_floor, Some(history))
                .map(faktor_router::SizedRouteDecision::without_plan),
        }
    }

    fn route_economy_sized(
        &self,
        req: &faktor_router::RouteRequest,
        prefix_history: Option<&[TurnPrefix]>,
        planner: Option<&dyn faktor_router::CandidatePlanner>,
    ) -> Result<faktor_router::SizedRouteDecision, RouteFailure> {
        // The requested band applies VERBATIM: the policy never lowers a
        // hard quality requirement toward the best available candidate, and
        // the adaptive target (when the request carries one) is forwarded as
        // the router's permission to prefer the target tier and relax into
        // `[minimum, target)` only without a target survivor.
        let (floor, target) = faktor_router::quality_band(req.quality_floor, req.quality_target);
        self.consult_at_with_plans(
            req,
            floor,
            target,
            prefix_history,
            planner,
            None,
            faktor_core::model::unix_now_ms(),
        )
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
    ///
    /// Tier formation and qualification read ONE quality authority: a
    /// service carrying priced candidates qualifies through the
    /// authority-aware effective quality (durable measured outcomes
    /// supersede the descriptor and `ConservativeUnknown` authorizes nothing
    /// above floor 0), so the tiers are built from that same statement; a
    /// descriptor-only service qualifies through the descriptor's declared
    /// built-in prior, so its tiers are the descriptor values. Probing a
    /// descriptor tier against a measured qualification (or the reverse)
    /// could pick a lower-current-quality model or waste probes on a
    /// descriptor-high candidate the router would refuse.
    fn route_maximum_quality_sized(
        &self,
        req: &faktor_router::RouteRequest,
        prefix_history: Option<&[TurnPrefix]>,
        planner: Option<&dyn faktor_router::CandidatePlanner>,
    ) -> Result<faktor_router::SizedRouteDecision, RouteFailure> {
        // The requested floor is the hard lower bound of the tier probe:
        // tiers below it never serve, and when no tier above it clears the
        // caps the refusal names the empty above-floor set.
        let floor_bound = req.quality_floor.min(100);
        // ONE route-scoped memo: tier formation and every descending probe
        // share one durable-outcome lookup per candidate instead of
        // re-reading the store once per tier.
        let now_ms = faktor_core::model::unix_now_ms();
        let outcomes = faktor_router::outcomes::MemoOutcomeStore::new(self.service.outcome_store());
        let mut tiers: Vec<u8> = if self.service.priced.is_empty() {
            self.service
                .router
                .candidates
                .iter()
                .map(|c| descriptor_phase_quality(c, req.phase))
                .filter(|&q| q >= floor_bound)
                .collect()
        } else {
            self.service
                .priced
                .iter()
                .map(|c| {
                    faktor_router::phase_quality_value(
                        &self
                            .service
                            .effective_quality_for_with(c, req, now_ms, &outcomes),
                        req.phase,
                    )
                })
                .filter(|&q| q >= floor_bound)
                .collect()
        };
        tiers.sort_unstable();
        tiers.dedup();
        tiers.reverse();
        let mut last_denial: Option<RouteFailure> = None;
        for floor in tiers {
            // Each probed tier is a HARD floor: the mode already takes the
            // highest servable tier, so no target preference can apply
            // inside a probe (the tier itself is the preference).
            match self.consult_at_with_plans(
                req,
                floor,
                None,
                prefix_history,
                planner,
                Some(&outcomes),
                now_ms,
            ) {
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
        // Balanced never routes below its quality band; both band edges are
        // raised into the band (no lowering toward the best available, and
        // an adaptive target below the band cannot relax below it either).
        let (minimum, target) = faktor_router::quality_band(req.quality_floor, req.quality_target);
        let floor = minimum.clamp(Self::BALANCED_QUALITY_FLOOR, 100);
        let target = target.map(|t| t.clamp(floor, 100)).filter(|t| *t > floor);
        self.consult_at_with_plans(
            req,
            floor,
            target,
            prefix_history,
            planner,
            None,
            faktor_core::model::unix_now_ms(),
        )
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
        let floor = req.quality_floor.min(100);
        // The pin's authority-aware quality is validated by the router's
        // OWN qualification below (measured outcomes supersede the pin's
        // declared statement, and a ConservativeUnknown placeholder never
        // clears a positive floor) — the pin is never pre-judged from a
        // numeric placeholder here.
        //
        // Validation request only the pin can serve among candidates that
        // do not dominate it on every quality axis: the pin's own true
        // capabilities, the request's authority-aware quality floor, and
        // its own estimated latency as the preference. A candidate that
        // beats the pin on cost while matching it on caps/quality/latency
        // still wins the router's evaluation — and that is a loud
        // PolicyDenied below, never a silent substitution of the pin.
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
            quality_floor: floor,
            // A pin is validated against the request's HARD minimum only:
            // the preference tier belongs to the free choice and must never
            // reject a pin that clears every hard axis.
            quality_target: None,
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

impl faktor_router::OutcomeView for StoreOutcomeStore {
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

#[cfg(test)]
mod lib_tests;
