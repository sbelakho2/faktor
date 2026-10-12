//! ROUTER-ECONOMICS REGRESSION (audit item 9 follow-up). This suite is a
//! regression test for the ROUTER's economics discipline, NOT a measure of
//! local-model coding competitiveness:
//!
//! - the coding model is a deterministic, frozen SCRIPT. For attempt index
//!   i (0-based) it copies `patches/attempt-{min(i+1, n)}.sh` over the
//!   repository's `program.sh` — policy-independent, so its success rate is
//!   planted in the corpus by construction (`fix_at_attempt`), never
//!   measured from a model;
//! - because the script ignores the router, the suite can only certify that
//!   the ROUTER's decisions are economically sane (cheap-first ordering,
//!   escalation past a failed model, verified-cost discipline, unsolvable
//!   work never counted as success). It must never be published as
//!   model-quality, coding-competitiveness or local-model ability evidence.
//!   Real model evidence lives in the capture/replay coding benchmark in
//!   this crate (`coding`, `capture`, `replay` modules), whose artifacts
//!   carry `capture_source = live` only when produced by real models.
//!
//! What is real here (nothing is simulated as a model success probability):
//!
//! - routing: every attempt's (provider, model) decision comes from the
//!   production [`faktor_agent::EconomicRoutingPolicy`] over a real
//!   [`faktor_router::RouterService`] built from priced
//!   [`faktor_router::RouteCandidate`]s — expected-cost scoring, the
//!   outcome-hierarchy consult, quality/latency/budget qualification;
//! - outcome learning: after each task settles, the deterministic
//!   verification gate re-records every settled call through
//!   `EconomicRoutingPolicy::record_call_outcome` with the explicit
//!   [`faktor_agent::VerifiedCallAttribution`] (verified success for the
//!   call that passed verification, measured downstream rework spend for
//!   the calls whose failure caused it); the next task's routes consult
//!   that durable-style [`faktor_router::MemoryOutcomeStore`] through the
//!   conservative work-cost estimate — never a fabricated prior;
//! - verification: the scripted patch is applied to a fresh copy of the
//!   task repo and `<repo>/verify.sh` is executed with `sh`. Pass/fail is
//!   the real exit status, bounded by a hard timeout.
//!
//! Tasks whose scripts only pass from a later attempt require rework
//! (`fix_at_attempt`); unsolvable tasks (`fix_at_attempt: null`) have only
//! failing patches and therefore count as failures — never as successes.
//!
//! Two lanes are measured over the same frozen corpus in the same fixed
//! order: `economy` (the real Economy policy over the full candidate set)
//! and `frontier_only` (the same policy pinned to the frontier model).
//! Requests carry `quality_floor: 0` so the router's conservative
//! measured-confidence floor gate cannot mask the economic axis under
//! test; measured outcomes still drive expected-cost scoring, escalation
//! and the success ladder exactly as in production. When a served model's
//! change fails verification, the economy driver excludes that model for
//! the remainder of that task (the runtime does not loop failed attempts
//! back through the router; this driver policy mirrors the documented
//! escalation discipline of the economy suite) and re-consults the real
//! policy.
//!
//! The regression artifact is written to
//! `target/certification/router-economics-regression.json` (override with
//! `FAKTOR_ECON_CERT_OUT`), and the comparison is evaluated against
//! GENEROUS regression bounds whose constants live below.

pub mod capture;
pub mod coding;
pub mod fixture;
pub mod frozen;
pub mod pipeline;
pub mod replay;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use faktor_agent::{
    EconomicRoutingPolicy, RouteFailure, RoutingPolicy, SettledCallOutcome, VerifiedCallAttribution,
};
use faktor_core::model::{
    MicroUsdPerMillionTokens, ModelDescriptor, ModelEconomics, ModelSource, PriceQuote,
    PricingSnapshot, PricingState, RiskBucket, RouteDecision, RouterPhase, RoutingMode, TaskClass,
};
use faktor_router::{
    MemoryOutcomeStore, OutcomeKey, OutcomeSample, RouteCandidate, RouteRequest, Router,
    RouterService, RouterTelemetry,
};
use serde::{Deserialize, Serialize};

/// Artifact schema id. The artifact is a router-economics regression record,
/// not model-quality evidence (see the module docs).
pub const SCHEMA: &str = "faktor-router-economics-regression/v1";
/// Attempt cap per task (a failed attempt still spent its call cost).
pub const MAX_ATTEMPTS: usize = 4;
/// Hard wall-clock bound on ONE verification subprocess run.
pub const VERIFY_TIMEOUT: Duration = Duration::from_secs(20);
/// Corpus shape bounds (frozen, small, deterministic).
pub const CORPUS_TASK_MIN: usize = 8;
pub const CORPUS_TASK_MAX: usize = 15;
/// The repo file the scripted fake model patches.
pub const PATCHED_FILE: &str = "program.sh";
/// The pinned frontier candidate of the `frontier_only` control lane.
pub const FRONTIER_PROVIDER: &str = "frontier";
pub const FRONTIER_MODEL: &str = "big-1";

// ---------------------------------------------------------- regression bounds
//
// GENEROUS by construction: the corpus exists to catch order-of-magnitude
// regressions in the economic policy (cheap-first turned into always-frontier,
// outcome learning turned into fabricated success, unsolvable work counted as
// success), not to pin exact values. Every bound is evaluated and recorded in
// the artifact.

/// The primary (cheap-first) lane must verify at least 70% of tasks.
pub const BOUND_PRIMARY_COMPLETION_MIN_PPM: i64 = 700_000;
/// The primary lane may finish at most 15 points behind the frontier lane.
pub const BOUND_COMPLETION_DELTA_MIN_PPM: i64 = -150_000;
/// Verified cost per successful change must stay at/below 65% of frontier.
pub const BOUND_COST_PER_SUCCESS_RATIO_MAX_PPM: i64 = 650_000;
/// Total rework spend must stay at/below 120% of the frontier lane's.
pub const BOUND_REWORK_COST_RATIO_MAX_PPM: i64 = 1_200_000;
/// At least 25% of verified changes must finish on the local (zero-cost) model.
pub const BOUND_LOCAL_SUBSTITUTION_MIN_PPM: i64 = 250_000;
/// At least 50% of verified changes must finish WITHOUT a frontier call.
pub const BOUND_NON_FRONTIER_SUBSTITUTION_MIN_PPM: i64 = 500_000;
/// No more than 60% of tasks may escalate to a frontier model.
pub const BOUND_FRONTIER_ESCALATION_MAX_PPM: i64 = 600_000;
/// Unsolvable tasks verified must be exactly zero.
pub const BOUND_UNSOLVABLE_VERIFIED_MAX: i64 = 0;

// -------------------------------------------------------------- the corpus

/// One frozen corpus task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSpec {
    pub id: String,
    /// Frozen execution order (ascending; ties by id).
    pub order: u32,
    pub task_class: TaskClass,
    pub phase: RouterPhase,
    pub risk_bucket: RiskBucket,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// 1-based attempt at which the scripted model's patch first passes
    /// verification; `None` = the task is unsolvable by the frozen patches.
    pub fix_at_attempt: Option<u32>,
    /// `patches/attempt-N.sh` files in order (patch i is applied on attempt
    /// i by every lane — the fake model is policy-independent).
    pub patches: Vec<PathBuf>,
    /// The pristine fixture repository (never written to).
    pub repo: PathBuf,
}

impl TaskSpec {
    pub fn solvable(&self) -> bool {
        self.fix_at_attempt.is_some()
    }

    /// The real route request for this task.
    pub fn request(&self) -> RouteRequest {
        RouteRequest {
            phase: self.phase,
            required_capabilities: vec!["tools".into(), "streaming".into()],
            context_tokens: self.input_tokens,
            estimated_output_tokens: self.output_tokens,
            quality_floor: 0,
            quality_target: None,
            task_budget_remaining_micro: 0,
            latency_preference_ms: None,
            task_class: self.task_class,
            risk_bucket: self.risk_bucket,
        }
    }
}

/// A loaded corpus plus its content digest (frozen-corpus evidence).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Corpus {
    pub tasks: Vec<TaskSpec>,
    pub digest: String,
}

#[derive(Debug, Deserialize)]
struct RawTask {
    id: String,
    /// Frozen execution order (the corpus is deterministic: tasks run in
    /// ascending order, ties broken by id).
    order: u32,
    task_class: TaskClass,
    phase: RouterPhase,
    input_tokens: u64,
    output_tokens: u64,
    fix_at_attempt: Option<u32>,
}

pub(crate) fn io_err(what: &str, e: io::Error) -> String {
    format!("{what}: {e}")
}

/// Load and validate the frozen corpus rooted at `root`. Fails loudly on any
/// hostile or inconsistent shape: a missing/malformed task file, an id that
/// does not match its directory or contains path characters, a missing repo
/// or verification script, non-sequential attempt patches, symlinks, or
/// unbounded metadata.
pub fn load_corpus(root: &Path) -> Result<Corpus, String> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(root)
        .map_err(|e| io_err("read corpus root", e))?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            entry.file_type().ok().filter(|t| t.is_dir())?;
            Some(entry.path())
        })
        .collect();
    dirs.sort();
    if dirs.len() < CORPUS_TASK_MIN || dirs.len() > CORPUS_TASK_MAX {
        return Err(format!(
            "corpus has {} tasks; expected between {CORPUS_TASK_MIN} and {CORPUS_TASK_MAX}",
            dirs.len()
        ));
    }
    let mut tasks = Vec::with_capacity(dirs.len());
    for dir in &dirs {
        tasks.push(load_task(dir)?);
    }
    tasks.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.id.cmp(&b.id)));
    let mut ids: Vec<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.len() != tasks.len() {
        return Err("corpus task ids are not unique".into());
    }
    let digest = corpus_digest(root)?;
    Ok(Corpus { tasks, digest })
}

/// Load ONE task directory (adversarial-loader evidence helper). The full
/// corpus loader uses the same validation.
pub fn load_task_dir(dir: &Path) -> Result<TaskSpec, String> {
    load_task(dir)
}

fn load_task(dir: &Path) -> Result<TaskSpec, String> {
    let dir_name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("task directory {} has no name", dir.display()))?;
    let meta_path = dir.join("task.json");
    let meta = fs::read_to_string(&meta_path).map_err(|e| io_err("read task.json", e))?;
    if meta.len() > 64 * 1024 {
        return Err(format!("{dir_name}: task.json exceeds 64 KiB"));
    }
    let raw: RawTask =
        serde_json::from_str(&meta).map_err(|e| format!("{dir_name}: invalid task.json: {e}"))?;
    if raw.id != dir_name {
        return Err(format!(
            "{dir_name}: task id {:?} does not match its directory",
            raw.id
        ));
    }
    if raw.id.is_empty()
        || raw.id.len() > 64
        || !raw
            .id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(format!(
            "{dir_name}: task id must be [a-z0-9-] and <= 64 bytes"
        ));
    }
    if raw.input_tokens == 0 || raw.output_tokens == 0 {
        return Err(format!("{dir_name}: token usage must be nonzero"));
    }
    if raw.input_tokens > 1_000_000 || raw.output_tokens > 1_000_000 {
        return Err(format!(
            "{dir_name}: token usage exceeds the bounded corpus"
        ));
    }
    let repo = dir.join("repo");
    require_regular(&repo.join(PATCHED_FILE))?;
    require_regular(&repo.join("verify.sh"))?;
    reject_symlinks(&repo)?;

    let patches_dir = dir.join("patches");
    let mut patches: Vec<(u32, PathBuf)> = fs::read_dir(&patches_dir)
        .map_err(|e| io_err("read patches dir", e))?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            let name = entry.file_name().to_str()?.to_string();
            let n = name
                .strip_prefix("attempt-")?
                .strip_suffix(".sh")?
                .parse::<u32>()
                .ok()?;
            Some((n, entry.path()))
        })
        .collect();
    patches.sort_by_key(|(n, _)| *n);
    if patches.is_empty() {
        return Err(format!("{dir_name}: no attempt patches"));
    }
    if patches.len() > MAX_ATTEMPTS {
        return Err(format!(
            "{dir_name}: {} attempt patches exceed the {MAX_ATTEMPTS}-attempt cap",
            patches.len()
        ));
    }
    for (i, (n, _)) in patches.iter().enumerate() {
        if *n as usize != i + 1 {
            return Err(format!(
                "{dir_name}: attempt patches must be sequential from 1"
            ));
        }
    }
    for (_, path) in &patches {
        require_regular(path)?;
    }
    if let Some(f) = raw.fix_at_attempt {
        if f == 0 || f as usize > patches.len() {
            return Err(format!(
                "{dir_name}: fix_at_attempt {f} exceeds the {} scripted patch(es)",
                patches.len()
            ));
        }
    }
    if raw.order == 0 || raw.order > 1000 {
        return Err(format!("{dir_name}: order must be between 1 and 1000"));
    }
    Ok(TaskSpec {
        id: raw.id,
        order: raw.order,
        task_class: raw.task_class,
        phase: raw.phase,
        risk_bucket: match raw.task_class {
            TaskClass::Easy => RiskBucket::Low,
            TaskClass::Medium => RiskBucket::Medium,
            TaskClass::Hard => RiskBucket::High,
        },
        input_tokens: raw.input_tokens,
        output_tokens: raw.output_tokens,
        fix_at_attempt: raw.fix_at_attempt,
        patches: patches.into_iter().map(|(_, path)| path).collect(),
        repo,
    })
}

pub(crate) fn require_regular(path: &Path) -> Result<(), String> {
    let meta = fs::symlink_metadata(path).map_err(|e| io_err(&path.display().to_string(), e))?;
    if !meta.file_type().is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    Ok(())
}

pub(crate) fn reject_symlinks(root: &Path) -> Result<(), String> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).map_err(|e| io_err(&dir.display().to_string(), e))? {
            let entry = entry.map_err(|e| io_err("read corpus entry", e))?;
            let ft = entry
                .file_type()
                .map_err(|e| io_err("stat corpus entry", e))?;
            if ft.is_symlink() {
                return Err(format!(
                    "corpus symlink refused: {}",
                    entry.path().display()
                ));
            }
            if ft.is_dir() {
                stack.push(entry.path());
            }
        }
    }
    Ok(())
}

/// Content digest of EVERY file under the corpus root: relative path, length
/// and bytes, sorted by path. Frozen-corpus evidence, not a security hash.
pub fn corpus_digest(root: &Path) -> Result<String, String> {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).map_err(|e| io_err(&dir.display().to_string(), e))? {
            let entry = entry.map_err(|e| io_err("read corpus entry", e))?;
            let ft = entry
                .file_type()
                .map_err(|e| io_err("stat corpus entry", e))?;
            if ft.is_symlink() {
                return Err(format!(
                    "corpus symlink refused: {}",
                    entry.path().display()
                ));
            }
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
                files.push(entry.path());
            }
        }
    }
    files.sort();
    let mut hasher = blake3::Hasher::new();
    for file in &files {
        let rel = file
            .strip_prefix(root)
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .replace('\\', "/");
        let bytes = fs::read(file).map_err(|e| io_err(&file.display().to_string(), e))?;
        hasher.update(rel.as_bytes());
        hasher.update(&[0]);
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

// ------------------------------------------------------- real verification

/// Outcome of ONE real verification subprocess run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyResult {
    Passed,
    Failed,
    TimedOut,
    SpawnFailed(String),
}

/// Run `<workspace>/verify.sh` with `sh`, bounded by `timeout`. The verdict
/// is the REAL process exit status; stdout/stderr are discarded so output is
/// bounded by construction.
pub fn run_verification(workspace: &Path, timeout: Duration) -> VerifyResult {
    let mut child = match Command::new("sh")
        .arg("verify.sh")
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return VerifyResult::SpawnFailed(e.to_string()),
    };
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    return VerifyResult::Passed;
                }
                return VerifyResult::Failed;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return VerifyResult::TimedOut;
                }
                sleep(Duration::from_millis(2));
            }
            Err(e) => return VerifyResult::SpawnFailed(e.to_string()),
        }
    }
}

// ---------------------------------------------------- scripted fake model

/// Apply the scripted patch for `attempt_index` (0-based) to the workspace
/// copy: `patches/attempt-{min(i+1, n)}.sh` replaces `program.sh`. Beyond
/// the last patch the model retries its last scripted change.
pub fn apply_scripted_patch(
    workspace: &Path,
    spec: &TaskSpec,
    attempt_index: usize,
) -> io::Result<()> {
    let idx = attempt_index.min(spec.patches.len().saturating_sub(1));
    fs::copy(&spec.patches[idx], workspace.join(PATCHED_FILE))?;
    Ok(())
}

pub(crate) fn copy_tree(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("corpus symlink refused: {}", entry.path().display()),
            ));
        }
        let target = dst.join(entry.file_name());
        if ft.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if ft.is_file() {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

// ------------------------------------------------------- the candidate set

/// The model tier of one fixture candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTier {
    Local,
    Cheap,
    Mid,
    Frontier,
}

/// One fixture candidate: descriptor + the catalog pricing authority the
/// router prices every call through + the certification tier label.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub descriptor: ModelDescriptor,
    pub pricing: PricingState,
    pub tier: ModelTier,
}

impl Candidate {
    pub fn key(&self) -> (String, String) {
        (
            self.descriptor.provider.clone(),
            self.descriptor.model.clone(),
        )
    }

    fn route_candidate(&self) -> RouteCandidate {
        RouteCandidate::new(self.descriptor.clone(), self.pricing.clone())
    }

    /// The EXACT settlement cost of one attempt over this task's token usage,
    /// through the candidate's own price authority (authoritative local zero
    /// included). Every fixture candidate carries numeric pricing.
    fn settled_cost(&self, spec: &TaskSpec) -> u64 {
        self.pricing
            .snapshot()
            .settle_cost(spec.input_tokens, 0, 0, spec.output_tokens)
            .expect("fixture candidates carry numeric pricing")
    }
}

fn econ(coding: u8, context: u8, tool: u8, reasoning: u8, latency_ms: u64) -> ModelEconomics {
    ModelEconomics {
        estimated_latency_ms: latency_ms,
        tool_reliability: tool,
        reasoning_reliability: reasoning,
        coding_reliability: coding,
        context_reliability: context,
        availability: 100,
        ..Default::default()
    }
}

fn desc(provider: &str, model: &str, economics: ModelEconomics) -> ModelDescriptor {
    ModelDescriptor {
        provider: provider.into(),
        model: model.into(),
        context: 512_000,
        max_output: 64_000,
        tools: true,
        parallel_tools: true,
        reasoning: true,
        thinking: true,
        vision: false,
        structured_output: true,
        embeddings: false,
        streaming: true,
        economics,
        source: ModelSource::ProviderCatalog,
    }
}

fn quote(input: u64, output: u64) -> PriceQuote {
    PriceQuote {
        input: MicroUsdPerMillionTokens(input),
        output: MicroUsdPerMillionTokens(output),
        cache_read: MicroUsdPerMillionTokens(input / 4),
        cache_write: MicroUsdPerMillionTokens(input / 2),
    }
}

fn exact(input: u64, output: u64) -> PricingState {
    PricingState::Known(PricingSnapshot::exact(
        quote(input, output),
        1,
        "econ-cert-fixture".into(),
    ))
}

/// The frozen candidate universe: one local zero-cost model, one cheap paid
/// model, one mid model, and two frontier models.
pub fn candidate_universe() -> Vec<Candidate> {
    vec![
        Candidate {
            descriptor: desc("local", "qwen-3b", econ(80, 88, 78, 76, 3000)),
            pricing: PricingState::LocalZero,
            tier: ModelTier::Local,
        },
        Candidate {
            descriptor: desc("cheap", "eco-mini", econ(82, 84, 80, 78, 800)),
            pricing: exact(2_000_000, 8_000_000),
            tier: ModelTier::Cheap,
        },
        Candidate {
            descriptor: desc("mid", "mid-24b", econ(88, 90, 86, 86, 1200)),
            pricing: exact(6_000_000, 18_000_000),
            tier: ModelTier::Mid,
        },
        Candidate {
            descriptor: desc(FRONTIER_PROVIDER, FRONTIER_MODEL, econ(96, 96, 95, 95, 400)),
            pricing: exact(15_000_000, 60_000_000),
            tier: ModelTier::Frontier,
        },
        Candidate {
            descriptor: desc("frontier", "big-2", econ(94, 95, 93, 93, 600)),
            pricing: exact(18_000_000, 72_000_000),
            tier: ModelTier::Frontier,
        },
    ]
}

// -------------------------------------------------------------- lane runner

/// Which lane's policy is under certification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrimaryPolicy {
    Economy,
    AlwaysFrontier,
}

impl PrimaryPolicy {
    fn lane_kind(self) -> LaneKind {
        match self {
            PrimaryPolicy::Economy => LaneKind::Economy,
            PrimaryPolicy::AlwaysFrontier => LaneKind::FrontierOnly,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneKind {
    Economy,
    FrontierOnly,
}

impl LaneKind {
    pub fn as_str(self) -> &'static str {
        match self {
            LaneKind::Economy => "economy",
            LaneKind::FrontierOnly => "frontier_only",
        }
    }
}

/// One settled model attempt over a task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptRecord {
    pub provider: String,
    pub model: String,
    pub tier: ModelTier,
    pub cost_micro: u64,
    pub verified: bool,
    pub latency_ms: u64,
}

/// One task's outcome within one lane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskOutcome {
    pub id: String,
    pub task_class: TaskClass,
    pub phase: RouterPhase,
    pub solvable: bool,
    pub verified: bool,
    pub attempts: Vec<AttemptRecord>,
    pub cost_micro: u64,
    pub rework_cost_micro: u64,
    pub route_denials: u64,
}

impl TaskOutcome {
    pub fn verified_by(&self) -> Option<&AttemptRecord> {
        self.attempts.iter().find(|a| a.verified)
    }

    pub fn frontier_escalated(&self) -> bool {
        self.attempts
            .iter()
            .enumerate()
            .any(|(i, a)| i > 0 && a.tier == ModelTier::Frontier)
    }
}

/// Aggregate metrics for one lane (machine-readable units: microUSD, ppm).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaneMetrics {
    pub lane: String,
    pub tasks: u64,
    pub verified: u64,
    pub failed: u64,
    pub completion_rate_ppm: u32,
    pub attempts: u64,
    pub route_denials: u64,
    pub total_cost_micro: u64,
    pub rework_cost_micro: u64,
    pub cost_per_success_micro: Option<u64>,
    pub frontier_attempts: u64,
    pub frontier_escalations: u64,
    pub frontier_escalation_rate_ppm: Option<u32>,
    pub local_verifications: u64,
    pub local_substitution_rate_ppm: Option<u32>,
    pub non_frontier_verifications: u64,
    pub non_frontier_substitution_rate_ppm: Option<u32>,
    pub unsolvable_verified: u64,
}

fn ppm(numer: u64, denom: u64) -> u32 {
    if denom == 0 {
        return 0;
    }
    u32::try_from(numer.saturating_mul(1_000_000) / denom).unwrap_or(1_000_000)
}

impl LaneMetrics {
    fn from_outcomes(lane: LaneKind, outcomes: &[TaskOutcome]) -> Self {
        let tasks = outcomes.len() as u64;
        let verified = outcomes.iter().filter(|t| t.verified).count() as u64;
        let attempts = outcomes.iter().map(|t| t.attempts.len() as u64).sum();
        let route_denials = outcomes.iter().map(|t| t.route_denials).sum();
        let total_cost_micro = outcomes.iter().map(|t| t.cost_micro).sum();
        let rework_cost_micro = outcomes.iter().map(|t| t.rework_cost_micro).sum();
        let frontier_attempts = outcomes
            .iter()
            .flat_map(|t| t.attempts.iter())
            .filter(|a| a.tier == ModelTier::Frontier)
            .count() as u64;
        let frontier_escalations =
            outcomes.iter().filter(|t| t.frontier_escalated()).count() as u64;
        let local_verifications = outcomes
            .iter()
            .filter(|t| t.verified_by().is_some_and(|a| a.tier == ModelTier::Local))
            .count() as u64;
        let non_frontier_verifications = outcomes
            .iter()
            .filter(|t| {
                t.verified_by()
                    .is_some_and(|a| a.tier != ModelTier::Frontier)
            })
            .count() as u64;
        let unsolvable_verified = outcomes
            .iter()
            .filter(|t| !t.solvable && t.verified)
            .count() as u64;
        LaneMetrics {
            lane: lane.as_str().into(),
            tasks,
            verified,
            failed: tasks - verified,
            completion_rate_ppm: ppm(verified, tasks),
            attempts,
            route_denials,
            total_cost_micro,
            rework_cost_micro,
            cost_per_success_micro: (verified > 0).then(|| total_cost_micro / verified),
            frontier_attempts,
            frontier_escalations,
            frontier_escalation_rate_ppm: Some(ppm(frontier_escalations, tasks)),
            local_verifications,
            local_substitution_rate_ppm: (verified > 0).then(|| ppm(local_verifications, verified)),
            non_frontier_verifications,
            non_frontier_substitution_rate_ppm: (verified > 0)
                .then(|| ppm(non_frontier_verifications, verified)),
            unsolvable_verified,
        }
    }
}

/// One lane's full report: per-task outcomes plus aggregate metrics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneReport {
    pub lane: LaneKind,
    pub outcomes: Vec<TaskOutcome>,
    pub metrics: LaneMetrics,
}

fn build_service(
    priced: Vec<RouteCandidate>,
    descriptors: Vec<ModelDescriptor>,
    telemetry: RouterTelemetry,
    pinned: Option<(String, String)>,
    store: Arc<MemoryOutcomeStore>,
) -> RouterService {
    RouterService {
        router: Router::new(descriptors),
        telemetry,
        pricing: HashMap::new(),
        priced,
        pinned,
        outcomes: store,
    }
}

fn take_telemetry(svc: Arc<RouterService>) -> RouterTelemetry {
    match Arc::try_unwrap(svc) {
        Ok(service) => service.telemetry,
        Err(_) => panic!("router service is the sole owner at this point"),
    }
}

/// Record the deterministic gate-site verified attribution for every settled
/// call of one task through the REAL policy entry point. The verifying call
/// carries the explicit verified success; the calls whose failure caused
/// measured downstream spend carry it as rework (their failure samples drive
/// the next routes' conservative work-cost estimates).
fn record_verified_attribution(
    store: Arc<MemoryOutcomeStore>,
    mode: &RoutingMode,
    spec: &TaskSpec,
    attempts: &[AttemptRecord],
    verified_index: Option<usize>,
) {
    if attempts.is_empty() {
        return;
    }
    let service = RouterService {
        router: Router::new(Vec::new()),
        telemetry: RouterTelemetry::new(),
        pricing: HashMap::new(),
        priced: Vec::new(),
        pinned: None,
        outcomes: store,
    };
    let policy = EconomicRoutingPolicy::new(Arc::new(service), mode.clone());
    for (i, attempt) in attempts.iter().enumerate() {
        let downstream: u64 = attempts[i + 1..].iter().map(|a| a.cost_micro).sum();
        let downstream_turns = (attempts.len() - 1 - i) as u64;
        let verified_success = verified_index == Some(i);
        let (rework_cost_micro, rework_turns) = if verified_success {
            (0, 0)
        } else {
            (downstream, downstream_turns)
        };
        policy.record_call_outcome(&SettledCallOutcome {
            provider: attempt.provider.clone(),
            model: attempt.model.clone(),
            phase: spec.phase,
            success: attempt.verified,
            retried: i > 0,
            rate_limited: false,
            latency_ms: attempt.latency_ms,
            verified: Some(VerifiedCallAttribution {
                task_class: spec.task_class,
                risk_bucket: spec.risk_bucket,
                verified_success,
                rework_cost_micro,
                rework_turns,
            }),
        });
    }
}

/// One lane run plus the outcome store it learned into (learning evidence).
pub struct LaneRun {
    pub report: LaneReport,
    pub store: Arc<MemoryOutcomeStore>,
}

/// Materialize a pristine copy of a task's fixture repository at `dst`.
pub fn copy_repo(spec: &TaskSpec, dst: &Path) -> io::Result<()> {
    copy_tree(&spec.repo, dst)
}

/// Run ONE lane over the frozen corpus in its frozen order. Every routing
/// decision comes from the real policy; every verification is a real
/// subprocess; every settled call is recorded through the real outcome
/// channel.
pub fn run_lane(lane: LaneKind, corpus: &[TaskSpec], scratch: &Path) -> LaneReport {
    run_lane_with_store(lane, corpus, scratch).report
}

/// [`run_lane`] exposing the outcome store the lane recorded into, for
/// learning-evidence assertions.
pub fn run_lane_with_store(lane: LaneKind, corpus: &[TaskSpec], scratch: &Path) -> LaneRun {
    let universe = candidate_universe();
    let candidates: Vec<Candidate> = match lane {
        LaneKind::Economy => universe,
        LaneKind::FrontierOnly => universe
            .into_iter()
            .filter(|c| {
                c.descriptor.provider == FRONTIER_PROVIDER && c.descriptor.model == FRONTIER_MODEL
            })
            .collect(),
    };
    let mode = match lane {
        LaneKind::Economy => RoutingMode::Economy,
        LaneKind::FrontierOnly => RoutingMode::Pinned {
            provider: FRONTIER_PROVIDER.into(),
            model: FRONTIER_MODEL.into(),
        },
    };
    let store = Arc::new(MemoryOutcomeStore::new());
    let mut telemetry = RouterTelemetry::new();
    let mut outcomes = Vec::with_capacity(corpus.len());
    for spec in corpus {
        let workspace = scratch.join(lane.as_str()).join(&spec.id);
        copy_tree(&spec.repo, &workspace).unwrap_or_else(|e| panic!("copy {} repo: {e}", spec.id));
        let mut excluded: BTreeSet<(String, String)> = BTreeSet::new();
        let mut attempts: Vec<AttemptRecord> = Vec::new();
        let mut verified = false;
        let mut route_denials = 0u64;
        for attempt_index in 0..MAX_ATTEMPTS {
            let eligible: Vec<&Candidate> = candidates
                .iter()
                .filter(|c| !excluded.contains(&c.key()))
                .collect();
            if eligible.is_empty() {
                break;
            }
            let priced: Vec<RouteCandidate> =
                eligible.iter().map(|c| c.route_candidate()).collect();
            let descriptors: Vec<ModelDescriptor> =
                eligible.iter().map(|c| c.descriptor.clone()).collect();
            let pinned = match lane {
                LaneKind::Economy => None,
                LaneKind::FrontierOnly => Some((FRONTIER_PROVIDER.into(), FRONTIER_MODEL.into())),
            };
            let svc = Arc::new(build_service(
                priced,
                descriptors,
                telemetry,
                pinned,
                store.clone(),
            ));
            let policy = EconomicRoutingPolicy::new(svc.clone(), mode.clone());
            let req = spec.request();
            let decision: Result<RouteDecision, RouteFailure> = policy.route(&req);
            let decision = match decision {
                Ok(decision) => decision,
                Err(_) => {
                    drop(policy);
                    telemetry = take_telemetry(svc);
                    route_denials += 1;
                    break;
                }
            };
            let chosen = candidates
                .iter()
                .find(|c| {
                    c.descriptor.provider == decision.provider
                        && c.descriptor.model == decision.model
                })
                .expect("the decision names a candidate of the eligible set");
            apply_scripted_patch(&workspace, spec, attempt_index)
                .unwrap_or_else(|e| panic!("apply patch for {}: {e}", spec.id));
            let verification = run_verification(&workspace, VERIFY_TIMEOUT);
            let passed = verification == VerifyResult::Passed;
            let cost_micro = chosen.settled_cost(spec);
            policy.record_call_outcome(&SettledCallOutcome {
                provider: decision.provider.clone(),
                model: decision.model.clone(),
                phase: spec.phase,
                success: passed,
                retried: attempt_index > 0,
                rate_limited: false,
                latency_ms: chosen.descriptor.economics.estimated_latency_ms,
                verified: None,
            });
            drop(policy);
            telemetry = take_telemetry(svc);
            attempts.push(AttemptRecord {
                provider: decision.provider.clone(),
                model: decision.model.clone(),
                tier: chosen.tier,
                cost_micro,
                verified: passed,
                latency_ms: chosen.descriptor.economics.estimated_latency_ms,
            });
            if passed {
                verified = true;
                break;
            }
            if lane == LaneKind::Economy {
                // Escalate past a model whose change failed verification on
                // THIS task; the next attempt re-consults the real policy
                // over the remaining candidates.
                excluded.insert(chosen.key());
            }
        }
        let verified_index = attempts.iter().position(|a| a.verified);
        record_verified_attribution(store.clone(), &mode, spec, &attempts, verified_index);
        let cost_micro = attempts.iter().map(|a| a.cost_micro).sum();
        let rework_cost_micro = attempts.iter().skip(1).map(|a| a.cost_micro).sum();
        outcomes.push(TaskOutcome {
            id: spec.id.clone(),
            task_class: spec.task_class,
            phase: spec.phase,
            solvable: spec.solvable(),
            verified,
            attempts,
            cost_micro,
            rework_cost_micro,
            route_denials,
        });
    }
    let metrics = LaneMetrics::from_outcomes(lane, &outcomes);
    LaneRun {
        report: LaneReport {
            lane,
            outcomes,
            metrics,
        },
        store,
    }
}

// ----------------------------------------------------------- certificate

/// Frozen-corpus evidence recorded in the artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorpusInfo {
    pub tasks: u64,
    pub solvable: u64,
    pub unsolvable: u64,
    pub digest: String,
}

/// Cross-lane comparison of the policy under test against its control.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comparison {
    pub primary: String,
    pub control: String,
    pub completion_delta_ppm: i64,
    pub cost_per_success_ratio_ppm: Option<u64>,
    pub rework_cost_ratio_ppm: Option<u64>,
    pub total_cost_ratio_ppm: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundComparator {
    Min,
    Max,
}

/// One evaluated regression bound.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundCheck {
    pub name: String,
    pub comparator: BoundComparator,
    pub observed: i64,
    pub limit: i64,
    pub pass: bool,
}

/// One task row of the merged per-task report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskLaneRow {
    pub verified: bool,
    pub attempts: u64,
    pub total_cost_micro: u64,
    pub rework_cost_micro: u64,
    pub frontier_escalated: bool,
    pub verified_by: Option<String>,
}

/// Per-task evidence across both lanes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskCertRow {
    pub id: String,
    pub task_class: TaskClass,
    pub phase: RouterPhase,
    pub solvable: bool,
    pub economy: TaskLaneRow,
    pub frontier_only: TaskLaneRow,
}

/// The machine-readable router-economics regression artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Certificate {
    pub schema: String,
    pub primary_policy: PrimaryPolicy,
    pub corpus: CorpusInfo,
    pub lanes: BTreeMap<String, LaneMetrics>,
    pub comparison: Comparison,
    pub bounds: Vec<BoundCheck>,
    pub all_bounds_pass: bool,
    pub per_task: Vec<TaskCertRow>,
}

fn ratio_ppm(primary: Option<u64>, control: Option<u64>) -> Option<u64> {
    match (primary, control) {
        (Some(p), Some(c)) if c > 0 => Some(p.saturating_mul(1_000_000) / c),
        _ => None,
    }
}

fn check(name: &str, comparator: BoundComparator, observed: i64, limit: i64) -> BoundCheck {
    let pass = match comparator {
        BoundComparator::Min => observed >= limit,
        BoundComparator::Max => observed <= limit,
    };
    BoundCheck {
        name: name.into(),
        comparator,
        observed,
        limit,
        pass,
    }
}

fn evaluate_bounds(primary: &LaneMetrics, control: &LaneMetrics) -> Vec<BoundCheck> {
    let completion_delta =
        i64::from(primary.completion_rate_ppm) - i64::from(control.completion_rate_ppm);
    let cost_ratio = ratio_ppm(
        primary.cost_per_success_micro,
        control.cost_per_success_micro,
    );
    let rework_ratio = ratio_ppm(
        Some(primary.rework_cost_micro),
        Some(control.rework_cost_micro),
    );
    vec![
        check(
            "primary_completion_min_ppm",
            BoundComparator::Min,
            i64::from(primary.completion_rate_ppm),
            BOUND_PRIMARY_COMPLETION_MIN_PPM,
        ),
        check(
            "primary_completion_no_worse_than_control_ppm",
            BoundComparator::Min,
            completion_delta,
            BOUND_COMPLETION_DELTA_MIN_PPM,
        ),
        check(
            "primary_cost_per_success_ratio_max_ppm",
            BoundComparator::Max,
            cost_ratio.map_or(i64::MAX, |r| r as i64),
            BOUND_COST_PER_SUCCESS_RATIO_MAX_PPM,
        ),
        check(
            "primary_rework_cost_ratio_max_ppm",
            BoundComparator::Max,
            rework_ratio.map_or(i64::MAX, |r| r as i64),
            BOUND_REWORK_COST_RATIO_MAX_PPM,
        ),
        check(
            "primary_local_substitution_min_ppm",
            BoundComparator::Min,
            i64::from(primary.local_substitution_rate_ppm.unwrap_or(0)),
            BOUND_LOCAL_SUBSTITUTION_MIN_PPM,
        ),
        check(
            "primary_non_frontier_substitution_min_ppm",
            BoundComparator::Min,
            i64::from(primary.non_frontier_substitution_rate_ppm.unwrap_or(0)),
            BOUND_NON_FRONTIER_SUBSTITUTION_MIN_PPM,
        ),
        check(
            "primary_frontier_escalation_max_ppm",
            BoundComparator::Max,
            i64::from(primary.frontier_escalation_rate_ppm.unwrap_or(1_000_000)),
            BOUND_FRONTIER_ESCALATION_MAX_PPM,
        ),
        check(
            "primary_unsolvable_verified_max",
            BoundComparator::Max,
            primary.unsolvable_verified as i64,
            BOUND_UNSOLVABLE_VERIFIED_MAX,
        ),
    ]
}

fn task_lane_row(outcome: &TaskOutcome) -> TaskLaneRow {
    TaskLaneRow {
        verified: outcome.verified,
        attempts: outcome.attempts.len() as u64,
        total_cost_micro: outcome.cost_micro,
        rework_cost_micro: outcome.rework_cost_micro,
        frontier_escalated: outcome.frontier_escalated(),
        verified_by: outcome
            .verified_by()
            .map(|a| format!("{}/{}", a.provider, a.model)),
    }
}

/// The corpus root shipped with this crate.
pub fn corpus_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus")
}

/// Run the full router-economics regression: both lanes over the frozen
/// corpus with fresh outcome stores, then evaluate the generous bounds for
/// the selected primary policy.
pub fn run_certification(primary_policy: PrimaryPolicy) -> Result<Certificate, String> {
    let corpus = load_corpus(&corpus_root())?;
    let scratch = tempfile::TempDir::new().map_err(|e| io_err("create scratch dir", e))?;
    let economy = run_lane(LaneKind::Economy, &corpus.tasks, scratch.path());
    let frontier = run_lane(LaneKind::FrontierOnly, &corpus.tasks, scratch.path());
    let primary_lane = primary_policy.lane_kind();
    let primary = match primary_lane {
        LaneKind::Economy => &economy.metrics,
        LaneKind::FrontierOnly => &frontier.metrics,
    };
    let control = match primary_lane {
        LaneKind::Economy => &frontier.metrics,
        LaneKind::FrontierOnly => &economy.metrics,
    };
    let comparison = Comparison {
        primary: primary.lane.clone(),
        control: control.lane.clone(),
        completion_delta_ppm: i64::from(primary.completion_rate_ppm)
            - i64::from(control.completion_rate_ppm),
        cost_per_success_ratio_ppm: ratio_ppm(
            primary.cost_per_success_micro,
            control.cost_per_success_micro,
        ),
        rework_cost_ratio_ppm: ratio_ppm(
            Some(primary.rework_cost_micro),
            Some(control.rework_cost_micro),
        ),
        total_cost_ratio_ppm: ratio_ppm(
            Some(primary.total_cost_micro),
            Some(control.total_cost_micro),
        ),
    };
    let bounds = evaluate_bounds(primary, control);
    let all_bounds_pass = bounds.iter().all(|b| b.pass);
    let per_task = corpus
        .tasks
        .iter()
        .map(|spec| {
            let econ = economy
                .outcomes
                .iter()
                .find(|o| o.id == spec.id)
                .expect("economy outcome exists");
            let fr = frontier
                .outcomes
                .iter()
                .find(|o| o.id == spec.id)
                .expect("frontier outcome exists");
            TaskCertRow {
                id: spec.id.clone(),
                task_class: spec.task_class,
                phase: spec.phase,
                solvable: spec.solvable(),
                economy: task_lane_row(econ),
                frontier_only: task_lane_row(fr),
            }
        })
        .collect();
    let mut lanes = BTreeMap::new();
    lanes.insert("economy".to_string(), economy.metrics);
    lanes.insert("frontier_only".to_string(), frontier.metrics);
    Ok(Certificate {
        schema: SCHEMA.into(),
        primary_policy,
        corpus: CorpusInfo {
            tasks: corpus.tasks.len() as u64,
            solvable: corpus.tasks.iter().filter(|t| t.solvable()).count() as u64,
            unsolvable: corpus.tasks.iter().filter(|t| !t.solvable()).count() as u64,
            digest: corpus.digest,
        },
        lanes,
        comparison,
        bounds,
        all_bounds_pass,
        per_task,
    })
}

/// Default artifact path: `<workspace>/target/certification/router-economics-regression.json`
/// (override with `FAKTOR_ECON_CERT_OUT`). Independent of CARGO_TARGET_DIR so
/// the nightly lane's `--artifact target/certification/...` marker binding
/// always resolves against the repository checkout.
pub fn default_artifact_path() -> PathBuf {
    if let Some(path) = std::env::var_os("FAKTOR_ECON_CERT_OUT") {
        return PathBuf::from(path);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .join("target")
        .join("certification")
        .join("router-economics-regression.json")
}

/// Write the artifact atomically (tmp + rename).
pub fn write_artifact(cert: &Certificate, path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(cert).map_err(io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json)?;
    match fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Store key used by the certification's attribution records.
pub fn outcome_key(provider: &str, model: &str, spec: &TaskSpec) -> OutcomeKey {
    OutcomeKey {
        provider: provider.into(),
        model: model.into(),
        phase: spec.phase,
        task_class: spec.task_class,
        risk_bucket: spec.risk_bucket,
    }
}

/// Read one key's verified stats from a store (evidence helper for tests).
pub fn stats_for(
    store: &MemoryOutcomeStore,
    provider: &str,
    model: &str,
    spec: &TaskSpec,
) -> Option<faktor_router::VerifiedOutcomeStats> {
    use faktor_router::OutcomeView;
    store.stats(&outcome_key(provider, model, spec))
}

/// Sample constructor used by tests to prove the registry surface (the lane
/// runner records through the real policy channel).
pub fn sample(verified_success: bool, rework_cost_micro: u64) -> OutcomeSample {
    OutcomeSample {
        verified_success,
        rework_cost_micro,
        rework_turns: 0,
    }
}
