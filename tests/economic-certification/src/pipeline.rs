//! The shared coding-benchmark pipeline: the REAL Faktor stages that both
//! the live capture lane and the deterministic replay run.
//!
//! Stages (all real production components, wired at library level):
//!
//! 1. **Contract machinery** — `faktor_core::compile_contract` compiles a
//!    deterministic engineering contract (constraints, invariants, risks,
//!    proof obligations with bound evidence) from the task goal and the
//!    bounded repository snapshot. Pure: byte-identical for the same input.
//! 2. **Intelligence machinery** — the compiler's native repository
//!    analysis (build systems, touched files, text-level touched symbols) is
//!    rendered as the bounded intelligence brief of the enriched arm.
//! 3. **Routing** — every attempt's (provider, model) decision comes from
//!    the production `faktor_agent::EconomicRoutingPolicy` over a real
//!    `faktor_router::RouterService` built from the frozen model descriptors
//!    and price authorities, with verified-outcome learning fed between
//!    attempts exactly like the production gate sites.
//! 4. **Model call** — a [`CodingModel`]: the live lane drives the real
//!    provider adapters (native Ollama, OpenAI-compatible frontier); the
//!    labeled fixture lane drives a deterministic scripted plan.
//! 5. **Proof / verification** — the candidate is applied to a fresh copy of
//!    the task repository and the task's own objective `sh verify.sh` is
//!    executed as a real subprocess; the verdict is the process exit status.
//!
//! The two experiment arms differ ONLY in the machinery stages (contract,
//! intelligence, proof obligations) being present in the prompt and
//! enforcement path (`faktor_machinery`) or absent (`baseline`).

use crate::coding::{
    apply_candidate, copy_coding_repo, parse_model_candidate, run_task_verification, CodingCorpus,
    CodingTaskSpec, CODING_VERIFY_TIMEOUT, MAX_CANDIDATE_ATTEMPTS,
};
pub use crate::frozen::Arm;
use crate::frozen::{
    CostSource, FrozenAttempt, FrozenFileChange, FrozenIntelligence, FrozenModel, FrozenPrompt,
    FrozenRun, FrozenVerification, VerifyOutcome,
};
use crate::{corpus_digest, io_err};

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;

use faktor_agent::{
    EconomicRoutingPolicy, RouteFailure, RoutingPolicy, SettledCallOutcome, VerifiedCallAttribution,
};
use faktor_core::model::{RiskBucket, RouteDecision, RouterPhase, RoutingMode, TaskClass};
use faktor_core::{
    compile_contract, ContractCompileRequest, EngineeringContract, RepositoryFile,
    RepositorySnapshot,
};
use faktor_router::{
    MemoryOutcomeStore, OutcomeKey, RouteCandidate, RouteRequest, Router, RouterService,
    RouterTelemetry,
};

/// Hard bound on ONE rendered prompt (inputs and enriched machinery).
pub const MAX_PROMPT_BYTES: usize = 64 * 1024;

/// The routing dimensions one coding task carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteMeta {
    pub phase: RouterPhase,
    pub task_class: TaskClass,
    pub risk_bucket: RiskBucket,
}

impl RouteMeta {
    pub fn of(task: &CodingTaskSpec) -> Self {
        RouteMeta {
            phase: task.phase,
            task_class: task.task_class,
            risk_bucket: task.risk_bucket,
        }
    }
}

fn decision_key(provider: &str, model: &str) -> (String, String) {
    (provider.to_string(), model.to_string())
}

fn outcome_key(meta: RouteMeta, provider: &str, model: &str) -> OutcomeKey {
    OutcomeKey {
        provider: provider.into(),
        model: model.into(),
        phase: meta.phase,
        task_class: meta.task_class,
        risk_bucket: meta.risk_bucket,
    }
}

/// One settled attempt fact used for verified attribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptFact {
    pub provider: String,
    pub model: String,
    pub cost_micro: u64,
    pub verified: bool,
    pub latency_ms: u64,
}

/// The real routing engine: frozen model candidates + the production
/// EconomicRoutingPolicy + the verified-outcome store, with per-task
/// model exclusion after a failed attempt (the documented escalation
/// discipline; the runtime does not loop failed attempts back through the
/// router).
pub struct RouteEngine {
    models: Vec<FrozenModel>,
    mode: RoutingMode,
    store: Arc<MemoryOutcomeStore>,
    excluded: BTreeSet<(String, String)>,
}

impl RouteEngine {
    pub fn new(models: Vec<FrozenModel>) -> Self {
        RouteEngine {
            models,
            mode: RoutingMode::Economy,
            store: Arc::new(MemoryOutcomeStore::new()),
            excluded: BTreeSet::new(),
        }
    }

    fn build_service(&self, eligible: &[&FrozenModel]) -> RouterService {
        let priced: Vec<RouteCandidate> = eligible
            .iter()
            .map(|m| RouteCandidate::new(m.descriptor.clone(), m.pricing.clone()))
            .collect();
        let descriptors = eligible.iter().map(|m| m.descriptor.clone()).collect();
        RouterService {
            router: Router::new(descriptors),
            telemetry: RouterTelemetry::new(),
            pricing: HashMap::new(),
            priced,
            pinned: None,
            outcomes: self.store.clone(),
        }
    }

    /// One real routing decision over the remaining (non-excluded) models.
    pub fn decide(
        &self,
        meta: RouteMeta,
        context_tokens: u64,
        output_tokens: u64,
    ) -> Result<FrozenModel, String> {
        let eligible: Vec<&FrozenModel> = self
            .models
            .iter()
            .filter(|m| {
                !self
                    .excluded
                    .contains(&decision_key(&m.descriptor.provider, &m.descriptor.model))
            })
            .collect();
        if eligible.is_empty() {
            return Err("no eligible model remains".into());
        }
        let svc = Arc::new(self.build_service(&eligible));
        let policy = EconomicRoutingPolicy::new(svc, self.mode.clone());
        let request = RouteRequest {
            phase: meta.phase,
            required_capabilities: vec!["streaming".into()],
            context_tokens,
            estimated_output_tokens: output_tokens,
            quality_floor: 0,
            quality_target: None,
            task_budget_remaining_micro: 0,
            latency_preference_ms: None,
            task_class: meta.task_class,
            risk_bucket: meta.risk_bucket,
        };
        match policy.route(&request) {
            Ok(decision) => {
                let chosen = self
                    .models
                    .iter()
                    .find(|m| {
                        m.descriptor.provider == decision.provider
                            && m.descriptor.model == decision.model
                    })
                    .ok_or_else(|| {
                        format!(
                            "router chose {}/{} which is not a frozen candidate",
                            decision.provider, decision.model
                        )
                    })?;
                frozen_route_matches(chosen, &decision)?;
                Ok(chosen.clone())
            }
            Err(RouteFailure::NoCapableModel) => Err("router: no capable model".into()),
            Err(other) => Err(format!("router refused the request: {other:?}")),
        }
    }

    /// Record one settled attempt through the REAL outcome channel.
    pub fn record_attempt(&self, meta: RouteMeta, fact: &AttemptFact, retried: bool) {
        let policy =
            EconomicRoutingPolicy::new(Arc::new(self.build_service(&[])), self.mode.clone());
        policy.record_call_outcome(&SettledCallOutcome {
            provider: fact.provider.clone(),
            model: fact.model.clone(),
            phase: meta.phase,
            success: fact.verified,
            retried,
            rate_limited: false,
            latency_ms: fact.latency_ms,
            verified: None,
        });
    }

    /// Record the deterministic verified attribution for every settled call
    /// of one task: the verifying call carries the verified success; failed
    /// calls carry the measured downstream rework spend they caused.
    pub fn record_attribution(
        &self,
        meta: RouteMeta,
        facts: &[AttemptFact],
        verified_index: Option<usize>,
    ) {
        let policy =
            EconomicRoutingPolicy::new(Arc::new(self.build_service(&[])), self.mode.clone());
        for (i, fact) in facts.iter().enumerate() {
            let downstream: u64 = facts[i + 1..].iter().map(|a| a.cost_micro).sum();
            let downstream_turns = (facts.len() - 1 - i) as u64;
            let verified_success = verified_index == Some(i);
            let (rework_cost_micro, rework_turns) = if verified_success {
                (0, 0)
            } else {
                (downstream, downstream_turns)
            };
            policy.record_call_outcome(&SettledCallOutcome {
                provider: fact.provider.clone(),
                model: fact.model.clone(),
                phase: meta.phase,
                success: fact.verified,
                retried: i > 0,
                rate_limited: false,
                latency_ms: fact.latency_ms,
                verified: Some(VerifiedCallAttribution {
                    task_class: meta.task_class,
                    risk_bucket: meta.risk_bucket,
                    verified_success,
                    rework_cost_micro,
                    rework_turns,
                }),
            });
        }
    }

    /// Exclude a model for the remainder of the current task.
    pub fn exclude(&mut self, provider: &str, model: &str) {
        self.excluded.insert(decision_key(provider, model));
    }

    /// Clear per-task exclusions (the store's learned history persists).
    pub fn begin_task(&mut self) {
        self.excluded.clear();
    }

    /// Verified stats of one model key (evidence helper).
    pub fn stats(
        &self,
        meta: RouteMeta,
        provider: &str,
        model: &str,
    ) -> Option<faktor_router::VerifiedOutcomeStats> {
        use faktor_router::OutcomeView;
        self.store.stats(&outcome_key(meta, provider, model))
    }
}

fn frozen_route_matches(model: &FrozenModel, decision: &RouteDecision) -> Result<(), String> {
    if model.descriptor.provider != decision.provider || model.descriptor.model != decision.model {
        return Err("route decision does not name the frozen candidate".into());
    }
    Ok(())
}

// -------------------------------------------------------------------- prompts

/// One rendered prompt for one arm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptArtifacts {
    pub system: String,
    pub user: String,
    pub contract_digest: Option<String>,
    pub intelligence: Option<FrozenIntelligence>,
}

impl PromptArtifacts {
    pub fn blake3(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.system.as_bytes());
        hasher.update(&[0]);
        hasher.update(self.user.as_bytes());
        hasher.finalize().to_hex().to_string()
    }

    pub fn tokens_est(&self) -> u64 {
        estimate_tokens(&self.system)
            .saturating_add(estimate_tokens(&self.user))
            .max(1)
    }
}

/// Deterministic character-based token estimate (4 bytes/token). The LIVE
/// lane records provider-reported usage instead; this estimate is only the
/// router's `context_tokens` input and the FIXTURE lane's token basis.
pub fn estimate_tokens(text: &str) -> u64 {
    ((text.len() as u64) / 4).max(1)
}

const SYSTEM_BASELINE: &str = "You are a coding agent working in a small repository. \
Reply with EXACTLY one fenced ```json code block and nothing else, shaped as \
{\"files\": {\"<relative path>\": \"<complete new file content>\"}}. \
The content must be the COMPLETE new content of each file you change; \
paths are relative to the repository root. Do not touch verify.sh.";

const SYSTEM_MACHINERY: &str = "You are a coding agent working in a small repository under the \
Faktor engineering contract. The contract below states the constraints, invariants, risks and \
proof obligations your change must satisfy, and the repository audit lists the files and symbols \
the change touches. The deterministic verification command will be executed against your change; \
it must pass. Reply with EXACTLY one fenced ```json code block and nothing else, shaped as \
{\"files\": {\"<relative path>\": \"<complete new file content>\"}}. \
The content must be the COMPLETE new content of each file you change; paths are relative to the \
repository root. Do not touch verify.sh.";

fn render_repo(task: &CodingTaskSpec) -> String {
    let mut out = String::new();
    for (path, content) in &task.repo_files {
        out.push_str(&format!("===== {path} =====\n{content}\n"));
    }
    out
}

fn render_contract(contract: &EngineeringContract) -> String {
    let mut out = String::new();
    if !contract.constraints.is_empty() {
        out.push_str("constraints:\n");
        for constraint in &contract.constraints {
            out.push_str(&format!("- {}\n", constraint.statement));
        }
    }
    if !contract.invariants.is_empty() {
        out.push_str("invariants (must remain true):\n");
        for invariant in &contract.invariants {
            out.push_str(&format!("- {}\n", invariant.statement));
        }
    }
    if !contract.risks.is_empty() {
        out.push_str("risks:\n");
        for risk in &contract.risks {
            out.push_str(&format!(
                "- [{}] {}\n",
                risk.severity.severity_str(),
                risk.description
            ));
        }
    }
    if !contract.proof_obligations.is_empty() {
        out.push_str("proof obligations (must hold):\n");
        for obligation in &contract.proof_obligations {
            out.push_str(&format!(
                "- ({}, evidence {}) {}\n",
                obligation.requirement.as_str(),
                obligation.evidence.as_str(),
                obligation.claim
            ));
        }
    }
    out
}

/// Build the prompt for one task and arm. The `faktor_machinery` arm compiles
/// the REAL engineering contract and renders the native intelligence brief;
/// the `baseline` arm renders goal + repository only.
pub fn build_prompt(task: &CodingTaskSpec, arm: Arm) -> Result<PromptArtifacts, String> {
    let mut user = String::new();
    user.push_str(&format!("TASK:\n{}\n\n", task.goal.trim()));
    user.push_str("REPOSITORY FILES:\n");
    user.push_str(&render_repo(task));
    user.push_str("\nOutput the complete corrected files now.\n");
    let mut contract_digest = None;
    let mut intelligence = None;
    let system = match arm {
        Arm::Baseline => SYSTEM_BASELINE.to_string(),
        Arm::FullDaemonRuntime => {
            // The runtime arm has no prompt-side artifact: the frozen outputs
            // are driven through the real AgentRuntime (`daemon_arm`), never
            // through this prompt pipeline.
            return Err(format!(
                "{}: the full_daemon_runtime arm is driven through the AgentRuntime, \
                 not the prompt pipeline",
                task.id
            ));
        }
        Arm::FaktorMachinery => {
            let files: Vec<RepositoryFile> = task
                .repo_files
                .iter()
                .map(|(path, content)| {
                    RepositoryFile::with_content(path.as_str(), content.as_str())
                })
                .collect();
            let compiled = compile_contract(&ContractCompileRequest {
                goal: task.goal.clone(),
                repository: RepositorySnapshot::from_files(files),
            });
            let rendered = render_contract(&compiled.contract);
            if !rendered.is_empty() {
                user.push_str("\nENGINEERING CONTRACT (compiled by Faktor):\n");
                user.push_str(&rendered);
            }
            let mut brief = String::new();
            if !compiled.build_systems.is_empty() {
                brief.push_str(&format!(
                    "build systems: {}\n",
                    compiled.build_systems.join(", ")
                ));
            }
            if !compiled.touched_files.is_empty() {
                brief.push_str(&format!(
                    "intent-relevant files: {}\n",
                    compiled.touched_files.join(", ")
                ));
            }
            if !compiled.touched_symbols.is_empty() {
                brief.push_str(&format!(
                    "touched symbols: {}\n",
                    compiled.touched_symbols.join("; ")
                ));
            }
            if !brief.is_empty() {
                user.push_str("\nNATIVE INTELLIGENCE (repository analysis):\n");
                user.push_str(&brief);
            }
            user.push_str(
                "\nPROOF: the repository's `sh verify.sh` will be executed against your change \
                 and must exit zero.\n",
            );
            contract_digest = Some(contract_digest_of(&compiled));
            intelligence = Some(FrozenIntelligence {
                build_systems: compiled.build_systems.clone(),
                touched_files: compiled.touched_files.clone(),
                touched_symbols: compiled.touched_symbols.clone(),
            });
            SYSTEM_MACHINERY.to_string()
        }
    };
    let prompt = PromptArtifacts {
        system,
        user,
        contract_digest,
        intelligence,
    };
    let total = prompt.system.len() + prompt.user.len();
    if total > MAX_PROMPT_BYTES {
        return Err(format!(
            "{}: rendered prompt is {total} bytes (cap {MAX_PROMPT_BYTES})",
            task.id
        ));
    }
    Ok(prompt)
}

/// Digest of the compiled contract + repository view (machinery evidence).
pub(crate) fn contract_digest_of(compiled: &faktor_core::CompiledContract) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"faktor.coding.contract.v1\0");
    hasher.update(compiled.repository_digest.as_bytes());
    hasher.update(render_contract(&compiled.contract).as_bytes());
    for path in &compiled.touched_files {
        hasher.update(path.as_bytes());
        hasher.update(&[0]);
    }
    hasher.finalize().to_hex().to_string()
}

/// The prompt actually sent for attempt `attempt`: the base prompt plus, for
/// repairs, the bounded verification failure excerpt of the previous attempt.
pub fn attempt_prompt(
    base: &PromptArtifacts,
    attempt: u32,
    previous_failure: Option<&str>,
) -> PromptArtifacts {
    if attempt == 0 {
        return base.clone();
    }
    let mut user = base.user.clone();
    if let Some(excerpt) = previous_failure {
        user.push_str("\nPREVIOUS ATTEMPT FAILED VERIFICATION. Failure output:\n");
        user.push_str(excerpt.trim());
        user.push('\n');
    } else {
        user.push_str("\nPREVIOUS ATTEMPT DID NOT PASS VERIFICATION. Try again.\n");
    }
    user.push_str("Fix the problem and output the complete files again.\n");
    PromptArtifacts {
        system: base.system.clone(),
        user,
        contract_digest: base.contract_digest.clone(),
        intelligence: base.intelligence.clone(),
    }
}

// --------------------------------------------------------------------- models

/// One completion request handed to a [`CodingModel`].
pub struct CompletionRequest<'a> {
    pub task: &'a CodingTaskSpec,
    pub base_prompt: &'a PromptArtifacts,
    pub attempt: u32,
    pub previous_failure: Option<&'a str>,
}

impl CompletionRequest<'_> {
    pub fn prompt(&self) -> PromptArtifacts {
        attempt_prompt(self.base_prompt, self.attempt, self.previous_failure)
    }
}

/// One recorded model answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCompletion {
    pub text: String,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub spend_micro: u64,
    pub cost_source: CostSource,
    pub latency_ms: u64,
}

/// The model-call seam. The live lane implements this over the real provider
/// adapters; the labeled fixture lane implements it over the scripted plan.
pub trait CodingModel: Send + Sync {
    fn routed(&self) -> &FrozenModel;
    fn complete(&self, request: &CompletionRequest<'_>) -> Result<ModelCompletion, String>;
}

/// Run ONE arm over the corpus in frozen order: real routing, real
/// verification, real outcome recording; the model call comes from the
/// injected [`CodingModel`]s.
pub fn run_arm(
    corpus: &CodingCorpus,
    arm: Arm,
    models: &[Arc<dyn CodingModel>],
    scratch: &Path,
    source: crate::frozen::CaptureSource,
    completed_at_ms: &dyn Fn(&CodingTaskSpec) -> i64,
) -> Result<Vec<FrozenRun>, String> {
    if models.is_empty() {
        return Err("run_arm requires at least one model".into());
    }
    let frozen_models: Vec<FrozenModel> = models.iter().map(|m| m.routed().clone()).collect();
    let model_by_key: BTreeMap<(String, String), &Arc<dyn CodingModel>> = models
        .iter()
        .map(|m| {
            let routed = m.routed();
            (
                (
                    routed.descriptor.provider.clone(),
                    routed.descriptor.model.clone(),
                ),
                m,
            )
        })
        .collect();
    let mut engine = RouteEngine::new(frozen_models);
    let mut runs = Vec::with_capacity(corpus.tasks.len());
    for task in &corpus.tasks {
        if !crate::coding::runtime_available(task.runtime) {
            return Err(format!(
                "{}: runtime {} is not available on this host",
                task.id,
                task.runtime.as_str()
            ));
        }
        engine.begin_task();
        let workspace = scratch.join(arm.as_str()).join(&task.id);
        copy_coding_repo(task, &workspace).map_err(|e| io_err("copy coding repo", e))?;
        let base_prompt = build_prompt(task, arm)?;
        let meta = RouteMeta::of(task);
        let mut attempts = Vec::new();
        let mut facts = Vec::new();
        let mut previous_failure: Option<String> = None;
        let mut first_pass = false;
        for attempt_index in 0..MAX_CANDIDATE_ATTEMPTS {
            let request = CompletionRequest {
                task,
                base_prompt: &base_prompt,
                attempt: attempt_index,
                previous_failure: previous_failure.as_deref(),
            };
            let prompt = request.prompt();
            let chosen = match engine.decide(meta, prompt.tokens_est(), task.output_budget_tokens())
            {
                Ok(model) => model,
                Err(e) if attempts.is_empty() => {
                    return Err(format!("{}: first routing decision failed: {e}", task.id))
                }
                Err(_) => break,
            };
            let key = decision_key(&chosen.descriptor.provider, &chosen.descriptor.model);
            let model = model_by_key
                .get(&key)
                .copied()
                .ok_or_else(|| format!("no model implementation for {key:?}"))?;
            let completion = model.complete(&request)?;
            let (files, parse_error, diff_unified, added_lines, removed_lines) =
                match parse_model_candidate(&completion.text) {
                    Ok(candidate) => {
                        let applied = apply_candidate(&workspace, &candidate)
                            .map_err(|e| format!("{}: apply candidate: {e}", task.id))?;
                        let mut changes = Vec::with_capacity(applied.len());
                        let mut diff_text = String::new();
                        let mut added_total = 0u64;
                        let mut removed_total = 0u64;
                        for file in applied {
                            let before_hash = file
                                .before
                                .as_ref()
                                .map(|c| blake3::hash(c.as_bytes()).to_hex().to_string());
                            let (diff, added, removed) = crate::frozen::unified_diff(
                                &file.path,
                                file.before.as_deref().unwrap_or(""),
                                &file.after,
                            )?;
                            diff_text.push_str(&diff);
                            added_total += added;
                            removed_total += removed;
                            changes.push(FrozenFileChange {
                                path: file.path,
                                before_blake3: before_hash,
                                after_blake3: blake3::hash(file.after.as_bytes())
                                    .to_hex()
                                    .to_string(),
                                content: file.after,
                            });
                        }
                        (changes, None, diff_text, added_total, removed_total)
                    }
                    Err(e) => (Vec::new(), Some(e), String::new(), 0, 0),
                };
            let verification = run_task_verification(&workspace, CODING_VERIFY_TIMEOUT);
            let passed = verification.passed();
            let outcome = VerifyOutcome::from(verification.result.clone());
            let (recorded_millis, detail) = if source == crate::frozen::CaptureSource::Live {
                (
                    verification.millis,
                    verification.failure_excerpt_for(&workspace),
                )
            } else {
                (0, None)
            };
            let raw_response = if completion.text.len() <= crate::frozen::MAX_RAW_RESPONSE_BYTES {
                Some(completion.text.clone())
            } else {
                None
            };
            let fact = AttemptFact {
                provider: chosen.descriptor.provider.clone(),
                model: chosen.descriptor.model.clone(),
                cost_micro: completion.spend_micro,
                verified: passed,
                latency_ms: completion.latency_ms,
            };
            engine.record_attempt(meta, &fact, attempt_index > 0);
            attempts.push(FrozenAttempt {
                index: attempt_index,
                provider: fact.provider.clone(),
                model: fact.model.clone(),
                local: chosen.local,
                tokens_in: completion.tokens_in,
                tokens_out: completion.tokens_out,
                spend_micro: completion.spend_micro,
                cost_source: completion.cost_source,
                latency_ms: completion.latency_ms,
                raw_response,
                parse_error,
                files,
                diff_unified,
                added_lines,
                removed_lines,
                verification: FrozenVerification {
                    outcome,
                    detail,
                    millis: recorded_millis,
                },
                prompt_blake3: prompt.blake3(),
                prompt_tokens_est: prompt.tokens_est(),
                // The two prompt arms hash their rendered prompt WITHOUT the
                // runtime-arm volatile normalization, so they never record
                // exclusions.
                volatile: Vec::new(),
                runtime: None,
            });
            facts.push(fact);
            if passed {
                first_pass = attempt_index == 0;
                break;
            }
            engine.exclude(&chosen.descriptor.provider, &chosen.descriptor.model);
            previous_failure = verification.failure_excerpt_for(&workspace);
        }
        let verified_index = attempts.iter().position(|a| a.passed());
        engine.record_attribution(meta, &facts, verified_index);
        let verified = verified_index.is_some();
        let wall_time_ms = attempts
            .iter()
            .map(|a| a.latency_ms.saturating_add(a.verification.millis))
            .sum();
        let final_tree_blake3 = corpus_digest(&workspace)?;
        runs.push(FrozenRun {
            task_id: task.id.clone(),
            order: task.order,
            category: task.category,
            language: task.language,
            runtime: task.runtime,
            prompt: FrozenPrompt {
                arm,
                machinery: arm == Arm::FaktorMachinery,
                contract_digest: base_prompt.contract_digest.clone(),
                intelligence: base_prompt.intelligence.clone(),
            },
            attempts,
            verified,
            first_pass,
            final_tree_blake3,
            wall_time_ms,
            completed_at_ms: completed_at_ms(task),
        });
    }
    Ok(runs)
}
