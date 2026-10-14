//! Frozen capture artifacts for the coding benchmark: the durable record of
//! what a coding model actually produced (live capture) or what the labeled
//! FIXTURE plan encodes (machinery proof), with everything the deterministic
//! replay needs to re-run the outputs through the pipeline and re-measure.
//!
//! An artifact is self-describing about its evidence class:
//!
//! - `capture_source = "live"` — produced by REAL models through the
//!   pipeline (opt-in, paid); its measurements are model evidence;
//! - `capture_source = "fixture"` — produced by the scripted, policy-blind
//!   FIXTURE plan; it proves the replay/measurement machinery and MUST NOT
//!   be published as model-quality evidence.
//!
//! Integrity: every artifact carries a BLAKE3 digest over its canonical
//! serialization; every candidate file carries its content hash; every
//! unified diff is regenerated and compared at replay; a verification
//! outcome is re-executed, never trusted. Any drift is a hard error.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use faktor_core::model::{ModelDescriptor, PricingState};
use serde::{Deserialize, Serialize};

use crate::coding::{CodingCategory, CodingLanguage, CodingRuntime};
use crate::VerifyResult;

/// Artifact schema id.
pub const FROZEN_SCHEMA: &str = "faktor-coding-benchmark-capture/v1";
/// Hard bounds of one artifact.
pub const MAX_FROZEN_RUNS: usize = 64;
pub const MAX_FROZEN_ATTEMPTS: usize = 4;
pub const MAX_RAW_RESPONSE_BYTES: usize = 64 * 1024;
pub const MAX_FROZEN_TEXT_BYTES: usize = 256 * 1024;
pub const MAX_TOKEN_COUNT: u64 = 10_000_000;
pub const MAX_SPEND_MICRO: u64 = 1_000_000_000_000;
pub const MAX_ATTEMPT_MS: u64 = 3_600_000;
pub const MAX_DIFF_LINES: usize = 400;
/// Hard bound on the recorded normalization exclusions of one attempt: every
/// excluded section of the turn's provider calls, in call order.
pub const MAX_VOLATILE_SECTIONS: usize = 64;

/// Which model-size/placeholder arm of the experiment a run belongs to.
///
/// Isolation semantics (see `docs/certification.md`):
///
/// - [`Arm::Baseline`] isolates the RAW model: goal + repository prompt,
///   no Faktor machinery anywhere in the loop;
/// - [`Arm::FaktorMachinery`] isolates the CONTRACT-CONTEXT-ENHANCED model:
///   the compiled engineering contract + intelligence brief ride the prompt
///   and the harness enforces the documented escalation discipline;
/// - [`Arm::FullDaemonRuntime`] isolates the COMPLETE RUNTIME: the SAME
///   frozen model outputs are executed by the REAL `AgentRuntime` turn loop
///   over a real durable session (contract-gated completion, independent
///   review, proof/gate state recorded), deterministically and without any
///   live model call. All three arms are driven by frozen outputs; only the
///   opt-in live-paid capture lane may produce model-quality numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Arm {
    /// The bare pipeline: goal + repository, no Faktor machinery.
    Baseline,
    /// The full Faktor contract/intelligence/proof machinery in the prompt
    /// and enforcement path.
    FaktorMachinery,
    /// The complete runtime: the same frozen outputs driven through the real
    /// `AgentRuntime` turn loop (daemon-equivalent assembly). Replay-only in
    /// CI; never a live model call.
    FullDaemonRuntime,
}

impl Arm {
    /// The two MODEL arms (the ones the live capture lane can drive).
    pub const ALL: [Arm; 2] = [Arm::Baseline, Arm::FaktorMachinery];
    /// Every benchmark arm, the deterministic runtime arm included.
    pub const WITH_RUNTIME: [Arm; 3] =
        [Arm::Baseline, Arm::FaktorMachinery, Arm::FullDaemonRuntime];

    pub fn as_str(self) -> &'static str {
        match self {
            Arm::Baseline => "baseline",
            Arm::FaktorMachinery => "faktor_machinery",
            Arm::FullDaemonRuntime => "full_daemon_runtime",
        }
    }

    /// TRUE for the runtime-driven arm (frozen outputs through the real
    /// `AgentRuntime`; no model call of any kind).
    pub fn is_runtime(self) -> bool {
        self == Arm::FullDaemonRuntime
    }
}

/// How the capture was produced (the evidence class).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureSource {
    /// Scripted FIXTURE plan: machinery proof only, never model evidence.
    Fixture,
    /// Real models through the pipeline (opt-in live-paid lane).
    Live,
}

impl CaptureSource {
    pub fn as_str(self) -> &'static str {
        match self {
            CaptureSource::Fixture => "fixture",
            CaptureSource::Live => "live",
        }
    }
}

/// Where the recorded token counts came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenBasis {
    /// Provider-reported usage frames (real tokens).
    ProviderUsage,
    /// Deterministic character-based estimate (fixture only).
    Estimate,
}

/// Where the recorded wall time came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WallTimeBasis {
    /// Real measured wall clock (live capture).
    Captured,
    /// Deterministic placeholder derived from model economics (fixture).
    Fixture,
}

/// Where a recorded attempt's spend came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostSource {
    /// The provider's own USD cost report (authoritative).
    ProviderReported,
    /// The recorded exact price table × recorded tokens.
    PriceTable,
    /// A locally served zero-cost model.
    LocalZero,
    /// The scripted fixture plan (no monetary claim).
    Fixture,
}

/// One model of a capture, frozen with the exact descriptor and price
/// authority the router used, so replay rebuilds byte-identical route
/// candidates offline.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenModel {
    pub descriptor: ModelDescriptor,
    pub pricing: PricingState,
    pub local: bool,
    pub price_source: String,
}

/// The native contract/intelligence evidence rendered into the machinery
/// prompt (bounded, deterministic).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenIntelligence {
    pub build_systems: Vec<String>,
    pub touched_files: Vec<String>,
    pub touched_symbols: Vec<String>,
}

/// The prompt metadata of one arm.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenPrompt {
    pub arm: Arm,
    pub machinery: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intelligence: Option<FrozenIntelligence>,
}

/// One file changed by one attempt (full new content; hashes prove both the
/// before and after states).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenFileChange {
    pub path: String,
    /// Hash of the workspace file BEFORE the attempt; `None` = new file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_blake3: Option<String>,
    pub after_blake3: String,
    pub content: String,
}

/// One verification re-executed at replay time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerifyOutcome {
    Passed,
    Failed,
    TimedOut,
    SpawnFailed,
}

impl From<VerifyResult> for VerifyOutcome {
    fn from(result: VerifyResult) -> Self {
        match result {
            VerifyResult::Passed => VerifyOutcome::Passed,
            VerifyResult::Failed => VerifyOutcome::Failed,
            VerifyResult::TimedOut => VerifyOutcome::TimedOut,
            VerifyResult::SpawnFailed(_) => VerifyOutcome::SpawnFailed,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenVerification {
    pub outcome: VerifyOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub millis: u64,
}

/// Deterministic runtime-drive evidence of one attempt of the
/// `full_daemon_runtime` arm: the completion gate the REAL `AgentRuntime`
/// classified at its genuine turn end, the independent-review verdict, and
/// the provider-call shape of the turn. Labels only — no wall time, no op
/// ids — so the record is replayable byte-for-byte.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenRuntimeEvidence {
    /// Completion-gate classification (`verified_complete`, `unverified`,
    /// `failed_verification`, `blocked_verification`,
    /// `verification_pending`, or `none` for a turn that claimed nothing).
    pub completion_gate: String,
    /// The independent completion review's verdict, when a review ran.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review_verdict: Option<String>,
    /// Required checks the runtime DERIVED for this turn's change (the
    /// production multi-component derivation; honestly 0 for the minimal
    /// manifest-less corpus repos, which is why the objective success axis
    /// stays the task's own `verify.sh` run by the harness).
    pub derived_checks: u64,
    /// Model-provider calls the runtime made during the turn.
    pub provider_calls: u64,
    /// Review-model calls the runtime made during the turn.
    pub review_calls: u64,
    /// Workspace files the runtime actually applied (changed) this turn.
    pub files_applied: u64,
}

/// One explicitly recorded normalization exclusion of the runtime arm's
/// wire-request hashing: the DOCUMENTED rule name, the EXACT bytes removed
/// from the system text, and the frozen rationale. This is capture-time
/// evidence — the extracted bytes are scheduling-dependent by design (that
/// is why the rule exists at all) — so replay and regeneration verify the
/// rule vocabulary and completeness, never the volatile bytes themselves;
/// the hash identity stays the normalized text ([`FrozenAttempt::prompt_blake3`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenVolatileSection {
    pub name: String,
    pub extract: String,
    pub rationale: String,
}

/// One settled model attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenAttempt {
    pub index: u32,
    pub provider: String,
    pub model: String,
    pub local: bool,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub spend_micro: u64,
    pub cost_source: CostSource,
    pub latency_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_response: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parse_error: Option<String>,
    pub files: Vec<FrozenFileChange>,
    pub diff_unified: String,
    pub added_lines: u64,
    pub removed_lines: u64,
    pub verification: FrozenVerification,
    pub prompt_blake3: String,
    pub prompt_tokens_est: u64,
    /// Every wire section the runtime-arm hashing normalized OUT of this
    /// attempt's provider calls, in call order: the rule name, the exact
    /// removed bytes and the frozen rationale. Empty for the non-runtime
    /// arms (they hash their rendered prompt without normalization). The
    /// recorded extracts make the exclusion provable instead of hidden; a
    /// rule not in the documented vocabulary fails artifact validation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volatile: Vec<FrozenVolatileSection>,
    /// Runtime-drive evidence; `Some` exactly for the
    /// `full_daemon_runtime` arm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<FrozenRuntimeEvidence>,
}

impl FrozenAttempt {
    pub fn passed(&self) -> bool {
        self.verification.outcome == VerifyOutcome::Passed
    }
}

/// One task × arm experiment row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenRun {
    pub task_id: String,
    pub order: u32,
    pub category: CodingCategory,
    pub language: CodingLanguage,
    pub runtime: CodingRuntime,
    pub prompt: FrozenPrompt,
    pub attempts: Vec<FrozenAttempt>,
    pub verified: bool,
    pub first_pass: bool,
    /// Content digest of the final workspace tree.
    pub final_tree_blake3: String,
    pub wall_time_ms: u64,
    pub completed_at_ms: i64,
}

/// The signed-by-digest frozen capture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenCapture {
    pub schema: String,
    pub capture_id: String,
    pub source: CaptureSource,
    /// Engine identity string (`faktor-tests-router-economics <version>`).
    pub engine: String,
    /// Human label of when/why the capture was produced (never an input to
    /// any measurement).
    pub created_label: String,
    pub corpus_digest: String,
    pub token_basis: TokenBasis,
    pub wall_time_basis: WallTimeBasis,
    pub models: Vec<FrozenModel>,
    pub runs: Vec<FrozenRun>,
    /// BLAKE3 over the canonical serialization with this field emptied.
    pub digest: String,
}

impl FrozenCapture {
    /// The canonical digest over everything except the digest field itself.
    pub fn computed_digest(&self) -> Result<String, String> {
        let mut copy = self.clone();
        copy.digest = String::new();
        let bytes = serde_json::to_vec(&copy).map_err(|e| e.to_string())?;
        Ok(blake3::hash(&bytes).to_hex().to_string())
    }

    /// Seal the digest after construction/decoding.
    pub fn finish(mut self) -> Result<Self, String> {
        self.digest = self.computed_digest()?;
        Ok(self)
    }

    /// Validate shape, digest and internal consistency.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != FROZEN_SCHEMA {
            return Err(format!(
                "capture schema {:?} != {FROZEN_SCHEMA}",
                self.schema
            ));
        }
        if self.capture_id.is_empty()
            || self.capture_id.len() > 64
            || !self
                .capture_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.')
        {
            return Err("capture_id must be [A-Za-z0-9._-] and <= 64 bytes".into());
        }
        if self.digest != self.computed_digest()? {
            return Err("capture digest mismatch (artifact was modified)".into());
        }
        if self.runs.is_empty() || self.runs.len() > MAX_FROZEN_RUNS {
            return Err(format!(
                "capture has {} runs; expected 1..={MAX_FROZEN_RUNS}",
                self.runs.len()
            ));
        }
        if self.models.is_empty() {
            return Err("capture has no models".into());
        }
        let mut model_keys = std::collections::BTreeSet::new();
        for model in &self.models {
            if model.descriptor.provider.is_empty() || model.descriptor.model.is_empty() {
                return Err("capture model has an empty provider/model".into());
            }
            if !model_keys.insert((
                model.descriptor.provider.clone(),
                model.descriptor.model.clone(),
            )) {
                return Err("capture models are not unique".into());
            }
        }
        let mut run_keys = std::collections::BTreeSet::new();
        for run in &self.runs {
            if !run_keys.insert((run.task_id.clone(), run.prompt.arm)) {
                return Err(format!(
                    "capture has duplicate run ({}, {})",
                    run.task_id,
                    run.prompt.arm.as_str()
                ));
            }
            validate_run(run, self)?;
        }
        Ok(())
    }

    pub fn run(&self, task_id: &str, arm: Arm) -> Option<&FrozenRun> {
        self.runs
            .iter()
            .find(|run| run.task_id == task_id && run.prompt.arm == arm)
    }
}

fn is_hex64(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn validate_run(run: &FrozenRun, capture: &FrozenCapture) -> Result<(), String> {
    if run.attempts.is_empty() || run.attempts.len() > MAX_FROZEN_ATTEMPTS {
        return Err(format!(
            "{}: run has {} attempts; expected 1..={MAX_FROZEN_ATTEMPTS}",
            run.task_id,
            run.attempts.len()
        ));
    }
    if run.prompt.arm == Arm::FaktorMachinery && run.prompt.contract_digest.is_none() {
        return Err(format!(
            "{}: machinery run lacks a contract digest",
            run.task_id
        ));
    }
    if run.prompt.arm == Arm::Baseline
        && (run.prompt.contract_digest.is_some() || run.prompt.intelligence.is_some())
    {
        return Err(format!(
            "{}: baseline run must not carry machinery evidence",
            run.task_id
        ));
    }
    if run.prompt.arm.is_runtime() {
        // The runtime arm must carry the compiled-contract digest (it drives
        // the REAL contract gate) and no RENDERED intelligence brief (the
        // runtime's retrieval does not consume the prompt-side brief).
        if run.prompt.contract_digest.is_none() {
            return Err(format!(
                "{}: full_daemon_runtime run lacks the compiled-contract digest",
                run.task_id
            ));
        }
        if run.prompt.intelligence.is_some() {
            return Err(format!(
                "{}: full_daemon_runtime run must not carry a rendered intelligence brief",
                run.task_id
            ));
        }
    } else if run.attempts.iter().any(|a| a.runtime.is_some()) {
        return Err(format!(
            "{}: only full_daemon_runtime attempts carry runtime evidence",
            run.task_id
        ));
    }
    let mut passed_any = false;
    for (i, attempt) in run.attempts.iter().enumerate() {
        if attempt.index as usize != i {
            return Err(format!(
                "{}: attempt indexes are not sequential",
                run.task_id
            ));
        }
        if attempt.tokens_in > MAX_TOKEN_COUNT || attempt.tokens_out > MAX_TOKEN_COUNT {
            return Err(format!(
                "{}: attempt token counts exceed the bound",
                run.task_id
            ));
        }
        if attempt.spend_micro > MAX_SPEND_MICRO {
            return Err(format!("{}: attempt spend exceeds the bound", run.task_id));
        }
        if attempt.latency_ms > MAX_ATTEMPT_MS || attempt.verification.millis > MAX_ATTEMPT_MS {
            return Err(format!("{}: attempt time exceeds the bound", run.task_id));
        }
        if !is_hex64(&attempt.prompt_blake3) {
            return Err(format!(
                "{}: attempt prompt hash is not blake3 hex",
                run.task_id
            ));
        }
        match (run.prompt.arm.is_runtime(), &attempt.runtime) {
            (true, None) => {
                return Err(format!(
                    "{}: runtime attempt lacks its runtime-drive evidence",
                    run.task_id
                ))
            }
            (true, Some(evidence)) => {
                if evidence.completion_gate.is_empty() {
                    return Err(format!(
                        "{}: runtime attempt carries an empty completion gate label",
                        run.task_id
                    ));
                }
                if attempt.parse_error.is_some() {
                    return Err(format!(
                        "{}: runtime attempts are tool-call driven and cannot carry parse errors",
                        run.task_id
                    ));
                }
            }
            (false, Some(_)) => {
                return Err(format!(
                    "{}: non-runtime attempt carries runtime evidence",
                    run.task_id
                ))
            }
            (false, None) => {}
        }
        if run.prompt.arm.is_runtime() {
            if attempt.volatile.len() > MAX_VOLATILE_SECTIONS {
                return Err(format!(
                    "{}: attempt records {} volatile normalization sections (bound {MAX_VOLATILE_SECTIONS})",
                    run.task_id,
                    attempt.volatile.len()
                ));
            }
            for section in &attempt.volatile {
                if section.extract.is_empty() || section.extract.len() > MAX_FROZEN_TEXT_BYTES {
                    return Err(format!(
                        "{}: volatile section '{}' extract is empty or exceeds the bound",
                        run.task_id, section.name
                    ));
                }
                if !crate::daemon_arm::documented_volatile_rule(&section.name, &section.rationale) {
                    return Err(format!(
                        "{}: volatile section names an UNDOCUMENTED normalization rule: {}",
                        run.task_id, section.name
                    ));
                }
            }
        } else if !attempt.volatile.is_empty() {
            return Err(format!(
                "{}: non-runtime attempts never carry volatile normalization evidence",
                run.task_id
            ));
        }
        if let Some(raw) = &attempt.raw_response {
            if raw.len() > MAX_RAW_RESPONSE_BYTES {
                return Err(format!("{}: raw response exceeds the bound", run.task_id));
            }
        }
        if run.prompt.arm.is_runtime() {
            // The runtime arm's frozen script is NOT a routable candidate: it
            // has no model-table row. Its identity is fixed by the daemon-arm
            // contract, and the frozen script is authoritatively local.
            if attempt.provider != crate::daemon_arm::DAEMON_ARM_PROVIDER
                || attempt.model != crate::daemon_arm::DAEMON_ARM_MODEL
                || !attempt.local
            {
                return Err(format!(
                    "{}: runtime attempt must name the local frozen script {}/{} (got {}/{}, local={})",
                    run.task_id,
                    crate::daemon_arm::DAEMON_ARM_PROVIDER,
                    crate::daemon_arm::DAEMON_ARM_MODEL,
                    attempt.provider,
                    attempt.model,
                    attempt.local
                ));
            }
        } else {
            let model = capture
                .models
                .iter()
                .find(|m| {
                    m.descriptor.provider == attempt.provider && m.descriptor.model == attempt.model
                })
                .ok_or_else(|| {
                    format!(
                        "{}: attempt names unknown model {}/{}",
                        run.task_id, attempt.provider, attempt.model
                    )
                })?;
            if model.local != attempt.local {
                return Err(format!(
                    "{}: attempt local flag disagrees with the model table",
                    run.task_id
                ));
            }
        }
        for file in &attempt.files {
            crate::coding::validate_candidate_path(&file.path)?;
            if !is_hex64(&file.after_blake3)
                || file.before_blake3.as_deref().is_some_and(|h| !is_hex64(h))
            {
                return Err(format!("{}: file hash is not blake3 hex", run.task_id));
            }
            if file.content.len() > crate::coding::MAX_CANDIDATE_FILE_BYTES {
                return Err(format!("{}: file content exceeds the bound", run.task_id));
            }
            let actual = blake3::hash(file.content.as_bytes()).to_hex().to_string();
            if actual != file.after_blake3 {
                return Err(format!(
                    "{}: file {} content does not match its recorded hash",
                    run.task_id, file.path
                ));
            }
        }
        if attempt.diff_unified.len() > MAX_FROZEN_TEXT_BYTES {
            return Err(format!("{}: diff exceeds the bound", run.task_id));
        }
        if attempt.passed() {
            passed_any = true;
        }
    }
    if run.verified != passed_any {
        return Err(format!(
            "{}: verified flag disagrees with attempt outcomes",
            run.task_id
        ));
    }
    if run.first_pass && (!run.verified || run.attempts.len() != 1) {
        return Err(format!(
            "{}: first_pass requires exactly one passing attempt",
            run.task_id
        ));
    }
    if !is_hex64(&run.final_tree_blake3) {
        return Err(format!(
            "{}: final tree hash is not blake3 hex",
            run.task_id
        ));
    }
    if run.wall_time_ms > MAX_ATTEMPT_MS * MAX_FROZEN_ATTEMPTS as u64 {
        return Err(format!("{}: wall time exceeds the bound", run.task_id));
    }
    Ok(())
}

/// Parse and validate a capture artifact from `path`.
pub fn load_capture(path: &Path) -> Result<FrozenCapture, String> {
    let raw =
        fs::read_to_string(path).map_err(|e| crate::io_err(&path.display().to_string(), e))?;
    if raw.len() > 8 * 1024 * 1024 {
        return Err(format!(
            "{} exceeds the 8 MiB artifact bound",
            path.display()
        ));
    }
    let capture: FrozenCapture = serde_json::from_str(&raw)
        .map_err(|e| format!("{}: invalid capture: {e}", path.display()))?;
    capture.validate()?;
    Ok(capture)
}

/// Write the artifact as pretty JSON (tmp + rename).
pub fn save_capture(capture: &FrozenCapture, path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(capture).map_err(io::Error::other)?;
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

/// The shipped fixture capture path (`frozen/fixture/capture.json`).
pub fn fixture_capture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("frozen")
        .join("fixture")
        .join("capture.json")
}

/// All committed capture sets under `frozen/` (fixture + any live captures),
/// loaded and validated, sorted by capture id.
pub fn load_committed_captures() -> Result<Vec<FrozenCapture>, String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("frozen");
    let mut captures = Vec::new();
    if !root.is_dir() {
        return Ok(captures);
    }
    let mut dirs: Vec<PathBuf> = fs::read_dir(&root)
        .map_err(|e| crate::io_err(&root.display().to_string(), e))?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            entry.file_type().ok().filter(|t| t.is_dir())?;
            Some(entry.path())
        })
        .collect();
    dirs.sort();
    for dir in dirs {
        let path = dir.join("capture.json");
        if path.is_file() {
            captures.push(load_capture(&path)?);
        }
    }
    Ok(captures)
}

// -------------------------------------------------------------------- diff

/// One line-level edit of the deterministic unified diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiffOp {
    Keep,
    Delete,
    Insert,
}

/// Split content into line records: `split('\n')` so a trailing newline
/// yields a final empty record and the diff is a stable line-record diff.
fn diff_lines(content: &str) -> Vec<&str> {
    content.split('\n').collect()
}

/// Deterministic line-record unified diff (`--- a/<path>`, `+++ b/<path>`,
/// `@@ -l,c +l,c @@` hunks with three context lines). Bounded: refuses
/// inputs over [`MAX_DIFF_LINES`] lines on either side.
pub fn unified_diff(path: &str, before: &str, after: &str) -> Result<(String, u64, u64), String> {
    let a = diff_lines(before);
    let b = diff_lines(after);
    if a.len() > MAX_DIFF_LINES || b.len() > MAX_DIFF_LINES {
        return Err(format!("{path}: diff input exceeds {MAX_DIFF_LINES} lines"));
    }
    let n = a.len();
    let m = b.len();
    // LCS length table over bounded inputs.
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut ops: Vec<(DiffOp, usize, usize)> = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            ops.push((DiffOp::Keep, i, j));
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            ops.push((DiffOp::Delete, i, j));
            i += 1;
        } else {
            ops.push((DiffOp::Insert, i, j));
            j += 1;
        }
    }
    while i < n {
        ops.push((DiffOp::Delete, i, j));
        i += 1;
    }
    while j < m {
        ops.push((DiffOp::Insert, i, j));
        j += 1;
    }
    let added = ops
        .iter()
        .filter(|(op, _, _)| *op == DiffOp::Insert)
        .count() as u64;
    let removed = ops
        .iter()
        .filter(|(op, _, _)| *op == DiffOp::Delete)
        .count() as u64;
    if added == 0 && removed == 0 {
        return Ok((String::new(), 0, 0));
    }
    // Group ops into hunks with 3 lines of context, merging close changes.
    const CONTEXT: usize = 3;
    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, (op, _, _))| *op != DiffOp::Keep)
        .map(|(idx, _)| idx)
        .collect();
    let mut hunks: Vec<(usize, usize)> = Vec::new();
    for idx in changed {
        let start = idx.saturating_sub(CONTEXT);
        let end = (idx + CONTEXT + 1).min(ops.len());
        match hunks.last_mut() {
            Some((_, last_end)) if start <= *last_end => {
                *last_end = (*last_end).max(end);
            }
            _ => hunks.push((start, end)),
        }
    }
    let mut out = format!("--- a/{path}\n+++ b/{path}\n");
    for (start, end) in hunks {
        // Absorb leading keeps to keep headings aligned with the op window.
        let a_start = ops[start].1.max(1) + 1;
        let b_start = ops[start].2.max(1) + 1;
        let a_count = ops[start..end]
            .iter()
            .filter(|(op, _, _)| *op != DiffOp::Insert)
            .count();
        let b_count = ops[start..end]
            .iter()
            .filter(|(op, _, _)| *op != DiffOp::Delete)
            .count();
        let a_line = if a_count == 0 { a_start - 1 } else { a_start };
        let b_line = if b_count == 0 { b_start - 1 } else { b_start };
        out.push_str(&format!("@@ -{a_line},{a_count} +{b_line},{b_count} @@\n"));
        for (op, ai, bi) in &ops[start..end] {
            match op {
                DiffOp::Keep => out.push_str(&format!(" {}\n", a[*ai])),
                DiffOp::Delete => out.push_str(&format!("-{}\n", a[*ai])),
                DiffOp::Insert => out.push_str(&format!("+{}\n", b[*bi])),
            }
        }
    }
    Ok((out, added, removed))
}
