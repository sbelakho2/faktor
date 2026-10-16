//! Deterministic CI replay of frozen coding-benchmark outputs.
//!
//! Replay re-runs every frozen attempt through the pipeline offline:
//!
//! 1. the REAL contract/intelligence prompt machinery is rebuilt and its
//!    per-attempt prompt hash must match the artifact (drift is fatal);
//! 2. every routing decision is recomputed with the production
//!    EconomicRoutingPolicy over the frozen descriptors/prices and the
//!    recorded outcome feed, and must match the artifact;
//! 3. every candidate is re-applied with its before-hash chain checked, its
//!    unified diff regenerated and compared, and its recorded verification
//!    outcome re-executed as a real subprocess (any disagreement is fatal);
//! 4. each run is written through the REAL wired Engineering Outcome Ledger
//!    (durable SQLite store, exactly-once append) and the reported metrics
//!    are read back from the durable ledger, including the cohort
//!    Verified Cost per Successful Change.
//!
//! The replay makes no network calls and no model calls: it measures the
//! frozen REAL outputs. Fixture captures prove the machinery only; live
//! captures are model evidence.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use faktor_agent::{
    ChangedCodeScale, EconomicSettlement, FinalVerificationBasis, MeasuredMoney, MoneyState,
    OutcomeLedger, OutcomeLedgerEntry, OutcomeLedgerMetrics, ProofCoverage, RunModelStep,
    RunVerifiedSample, SettlementStep, TerminalEngineeringOutcome, OUTCOME_RECORD_SCHEMA_V,
};
use faktor_core::model::ExecutionClass;
use faktor_store::Store;
use serde::Serialize;

use crate::coding::{
    apply_candidate, copy_coding_repo, run_task_verification, validate_candidate_path, Candidate,
    CodingCorpus, CodingTaskSpec, CODING_VERIFY_TIMEOUT,
};
use crate::frozen::{Arm, CaptureSource, FrozenAttempt, FrozenCapture, FrozenRun, VerifyOutcome};
use crate::pipeline::{attempt_prompt, build_prompt, AttemptFact, RouteEngine, RouteMeta};

/// Report schema id.
pub const REPLAY_SCHEMA: &str = "faktor-coding-benchmark-replay/v1";

/// Aggregate metrics of one arm over one capture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArmMetrics {
    pub arm: String,
    pub runs: u64,
    pub verified: u64,
    pub first_pass_verified: u64,
    pub verified_first_pass_ppm: u32,
    pub completion_ppm: u32,
    pub attempts: u64,
    pub tokens_in: u64,
    pub tokens_out: u64,
    pub prompt_tokens_est_total: u64,
    pub inference_spend_micro: u64,
    pub rework_spend_micro: u64,
    pub rework_turns: u64,
    pub wall_time_ms: u64,
    pub local_attempts: u64,
    pub frontier_attempts: u64,
    pub local_only_verified: u64,
    pub local_only_verified_ppm: u32,
    pub local_failure_runs: u64,
    pub fallback_runs: u64,
    pub local_to_hosted_fallback_ppm: u32,
    pub frontier_escalations: u64,
    pub frontier_escalation_ppm: u32,
    pub ledger: OutcomeLedgerMetrics,
}

/// The with/without-machinery comparison (machinery minus baseline).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArmComparison {
    pub baseline_verified: u64,
    pub machinery_verified: u64,
    pub completion_delta_ppm: i64,
    pub baseline_first_pass_ppm: u32,
    pub machinery_first_pass_ppm: u32,
    pub first_pass_delta_ppm: i64,
    pub baseline_tokens_in: u64,
    pub machinery_tokens_in: u64,
    pub tokens_in_delta: i64,
    pub baseline_tokens_out: u64,
    pub machinery_tokens_out: u64,
    pub tokens_out_delta: i64,
    pub baseline_prompt_tokens_est: u64,
    pub machinery_prompt_tokens_est: u64,
    pub prompt_tokens_delta: i64,
    pub baseline_verified_cost_micro: u64,
    pub machinery_verified_cost_micro: u64,
    pub verified_cost_delta_micro: i64,
}

/// One capture's replay report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CaptureReport {
    pub capture_id: String,
    pub source: String,
    pub corpus_digest: String,
    pub token_basis: String,
    pub wall_time_basis: String,
    pub runs_replayed: u64,
    pub arms: BTreeMap<String, ArmMetrics>,
    /// The wired ledger's fold over this capture's runs.
    pub cohort_ledger: OutcomeLedgerMetrics,
    pub comparison: ArmComparison,
}

/// The full replay report over every committed capture set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReplayReport {
    pub schema: String,
    pub engine: String,
    /// TRUE when every capture replayed was a fixture (machinery proof only,
    /// no model-quality numbers).
    pub fixtures_only: bool,
    pub captures: Vec<CaptureReport>,
}

fn ppm(numer: u64, denom: u64) -> u32 {
    if denom == 0 {
        return 0;
    }
    u32::try_from(numer.saturating_mul(1_000_000) / denom).unwrap_or(1_000_000)
}

/// All committed captures replayed against the shipped coding corpus.
pub fn replay_all_committed(scratch: &Path) -> Result<ReplayReport, String> {
    let corpus = crate::coding::load_coding_corpus(&crate::coding::coding_corpus_root())?;
    let captures = crate::frozen::load_committed_captures()?;
    if captures.is_empty() {
        return Err("no committed capture sets under frozen/".into());
    }
    let mut reports = Vec::with_capacity(captures.len());
    for capture in &captures {
        reports.push(replay_capture(capture, &corpus, scratch)?);
    }
    let fixtures_only = captures.iter().all(|c| c.source == CaptureSource::Fixture);
    Ok(ReplayReport {
        schema: REPLAY_SCHEMA.into(),
        engine: format!(
            "faktor-tests-router-economics {}",
            env!("CARGO_PKG_VERSION")
        ),
        fixtures_only,
        captures: reports,
    })
}

/// Default report artifact path (override `FAKTOR_ECON_REPLAY_OUT`).
pub fn default_replay_artifact_path() -> PathBuf {
    if let Some(path) = std::env::var_os("FAKTOR_ECON_REPLAY_OUT") {
        return PathBuf::from(path);
    }
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .join("target")
        .join("certification")
        .join("router-economics-replay.json")
}

/// Write the replay report as pretty JSON (tmp + rename).
pub fn write_replay_report(report: &ReplayReport, path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(report).map_err(std::io::Error::other)?;
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

/// One replayed run plus its durable outcome + settlement projection.
struct ReplayedRun {
    run: FrozenRun,
    record: OutcomeLedgerEntry,
}

/// Append one replayed run's record through the durable ledger and demand the
/// read-back equals it (an exactly-once append is a no-op; a replayed append
/// still reads back identical).
fn append_and_read_back(
    ledger: &OutcomeLedger,
    record: &OutcomeLedgerEntry,
    task_id: &str,
) -> Result<(), String> {
    ledger
        .record(&record.outcome, &record.settlement)
        .map_err(|e| format!("{task_id}: ledger append: {e}"))?;
    let read_back = ledger
        .get(&record.outcome.run_id)
        .map_err(|e| format!("{task_id}: ledger read-back: {e}"))?;
    let read_settlement = ledger
        .settlement(&record.outcome.run_id)
        .map_err(|e| format!("{task_id}: settlement read-back: {e}"))?;
    if read_back.as_ref() != Some(&record.outcome)
        || read_settlement.as_ref() != Some(&record.settlement)
    {
        return Err(format!(
            "{task_id}: durable ledger record differs from the replayed record"
        ));
    }
    Ok(())
}

/// Replay ONE capture set: integrity, routing reproduction, re-verification,
/// durable ledger append and durable-read metrics.
pub fn replay_capture(
    capture: &FrozenCapture,
    corpus: &CodingCorpus,
    scratch: &Path,
) -> Result<CaptureReport, String> {
    capture.validate()?;
    if capture.corpus_digest != corpus.digest {
        return Err(format!(
            "capture {} was produced over corpus digest {} but the shipped corpus is {}",
            capture.capture_id, capture.corpus_digest, corpus.digest
        ));
    }
    let ledger_dir = scratch.join("replay-ledger").join(&capture.capture_id);
    if ledger_dir.exists() {
        fs::remove_dir_all(&ledger_dir).map_err(|e| crate::io_err("clear replay ledger", e))?;
    }
    fs::create_dir_all(&ledger_dir).map_err(|e| crate::io_err("create replay ledger dir", e))?;
    let store = Arc::new(Store::open_fast(&ledger_dir).map_err(|e| format!("open ledger: {e}"))?);
    let ledger = OutcomeLedger::new(store);
    // ONE routing engine per arm, fed in captured order exactly like the
    // capture run did, so every recomputed decision sees the same
    // verified-outcome history. The runtime arm needs no routing engine: it
    // re-drives the REAL AgentRuntime over the artifact's frozen outputs.
    let mut engines: BTreeMap<Arm, RouteEngine> = BTreeMap::new();
    let mut daemon_env: Option<crate::daemon_arm::DaemonArmEnv> = None;
    let mut replayed = Vec::with_capacity(capture.runs.len());
    for run in &capture.runs {
        let task = corpus
            .tasks
            .iter()
            .find(|t| t.id == run.task_id)
            .ok_or_else(|| format!("capture run names unknown task {}", run.task_id))?;
        if !crate::coding::runtime_available(task.runtime) {
            return Err(format!(
                "{}: runtime {} is not available; replay cannot reproduce the captured run",
                run.task_id,
                task.runtime.as_str()
            ));
        }
        if run.prompt.arm.is_runtime() {
            let env = match &daemon_env {
                Some(env) => env,
                None => {
                    // Each replay starts from an EMPTY durable runtime store:
                    // the evidence authority / retrieval ladder is durable
                    // state, so reusing a store across replays would let
                    // earlier replays' learned evidence shift the context the
                    // runtime compiles (a replay must reproduce the capture
                    // from the artifact alone).
                    let env_root = scratch.join("replay-daemon-env").join(&capture.capture_id);
                    if env_root.exists() {
                        fs::remove_dir_all(&env_root)
                            .map_err(|e| crate::io_err("clear replay daemon env", e))?;
                    }
                    daemon_env = Some(crate::daemon_arm::DaemonArmEnv::open(&env_root)?);
                    daemon_env.as_ref().expect("daemon env was just installed")
                }
            };
            let workspace = crate::daemon_arm::redrive_runtime_run(
                env,
                &capture.capture_id,
                task,
                run,
                scratch,
            )?;
            let verified_index = run.attempts.iter().position(|a| a.passed());
            if verified_index.is_some() != run.verified {
                return Err(format!(
                    "{}: runtime verified flag disagrees with its attempts",
                    run.task_id
                ));
            }
            let record = build_record(capture, task, run, verified_index, &workspace)?;
            append_and_read_back(&ledger, &record, &run.task_id)?;
            replayed.push(ReplayedRun {
                run: run.clone(),
                record,
            });
            continue;
        }
        let engine = engines
            .entry(run.prompt.arm)
            .or_insert_with(|| RouteEngine::new(capture.models.clone()));
        engine.begin_task();
        let replayed_run = replay_run(capture, task, run, engine, scratch)
            .map_err(|e| format!("{}: {e}", run.task_id))?;
        append_and_read_back(&ledger, &replayed_run.record, &run.task_id)?;
        replayed.push(replayed_run);
    }
    // The cohort metric comes from the DURABLE ledger fold (the wired path).
    let cohort_ledger = ledger
        .metrics()
        .map_err(|e| format!("ledger metrics: {e}"))?
        .metrics;
    let arms = arm_metrics(&replayed);
    let comparison = comparison(&arms);
    Ok(CaptureReport {
        capture_id: capture.capture_id.clone(),
        source: capture.source.as_str().into(),
        corpus_digest: capture.corpus_digest.clone(),
        token_basis: match capture.token_basis {
            crate::frozen::TokenBasis::ProviderUsage => "provider_usage".into(),
            crate::frozen::TokenBasis::Estimate => "estimate".into(),
        },
        wall_time_basis: match capture.wall_time_basis {
            crate::frozen::WallTimeBasis::Captured => "captured".into(),
            crate::frozen::WallTimeBasis::Fixture => "fixture".into(),
        },
        runs_replayed: replayed.len() as u64,
        arms,
        cohort_ledger,
        comparison,
    })
}

/// Replay ONE frozen run and build its durable outcome record. The engine
/// carries the arm's accumulated outcome history and per-task exclusions.
fn replay_run(
    capture: &FrozenCapture,
    task: &CodingTaskSpec,
    run: &FrozenRun,
    engine: &mut RouteEngine,
    scratch: &Path,
) -> Result<ReplayedRun, String> {
    let workspace = scratch
        .join("replay-work")
        .join(&capture.capture_id)
        .join(format!("{}-{}", run.task_id, run.prompt.arm.as_str()));
    if workspace.exists() {
        fs::remove_dir_all(&workspace).map_err(|e| crate::io_err("clear replay workspace", e))?;
    }
    copy_coding_repo(task, &workspace).map_err(|e| crate::io_err("copy coding repo", e))?;
    let base_prompt = build_prompt(task, run.prompt.arm)?;
    if base_prompt.contract_digest != run.prompt.contract_digest {
        return Err("machinery contract digest drifted".into());
    }
    if base_prompt.intelligence != run.prompt.intelligence {
        return Err("machinery intelligence brief drifted".into());
    }
    let meta = RouteMeta::of(task);
    let mut facts = Vec::with_capacity(run.attempts.len());
    let mut previous_failure: Option<String> = None;
    let mut verified_index: Option<usize> = None;
    for (i, attempt) in run.attempts.iter().enumerate() {
        let prompt = attempt_prompt(&base_prompt, attempt.index, previous_failure.as_deref());
        if prompt.blake3() != attempt.prompt_blake3 {
            return Err(format!("attempt {}: prompt drifted", attempt.index));
        }
        let chosen = engine.decide(meta, prompt.tokens_est(), task.output_budget_tokens())?;
        if chosen.descriptor.provider != attempt.provider
            || chosen.descriptor.model != attempt.model
            || chosen.local != attempt.local
        {
            return Err(format!(
                "attempt {}: routing decision drifted ({} vs {}/{})",
                attempt.index, attempt.model, chosen.descriptor.provider, chosen.descriptor.model
            ));
        }
        apply_recorded_attempt(&workspace, attempt)?;
        let verification = run_task_verification(&workspace, CODING_VERIFY_TIMEOUT);
        let replayed_outcome = VerifyOutcome::from(verification.result.clone());
        if replayed_outcome != attempt.verification.outcome {
            return Err(format!(
                "attempt {}: verification outcome drifted (recorded {:?}, re-ran {:?})",
                attempt.index, attempt.verification.outcome, replayed_outcome
            ));
        }
        let fact = AttemptFact {
            provider: attempt.provider.clone(),
            model: attempt.model.clone(),
            cost_micro: attempt.spend_micro,
            verified: attempt.passed(),
            latency_ms: attempt.latency_ms,
        };
        engine.record_attempt(meta, &fact, i > 0);
        facts.push(fact);
        if attempt.passed() {
            verified_index = Some(i);
            break;
        }
        engine.exclude(&attempt.provider, &attempt.model);
        previous_failure = verification.failure_excerpt_for(&workspace);
    }
    if verified_index.is_some() != run.verified {
        return Err("verified flag drifted".into());
    }
    engine.record_attribution(meta, &facts, verified_index);
    let final_tree = crate::corpus_digest(&workspace)?;
    if final_tree != run.final_tree_blake3 {
        return Err("final workspace drifted".into());
    }
    let record = build_record(capture, task, run, verified_index, &workspace)?;
    Ok(ReplayedRun {
        run: run.clone(),
        record,
    })
}

/// Re-apply one recorded attempt with full integrity: before-hash chain,
/// candidate validation, regenerated diff aggregate.
fn apply_recorded_attempt(workspace: &Path, attempt: &FrozenAttempt) -> Result<(), String> {
    let mut diff_text = String::new();
    let mut added_total = 0u64;
    let mut removed_total = 0u64;
    for file in &attempt.files {
        validate_candidate_path(&file.path)?;
        let current = fs::read_to_string(workspace.join(&file.path)).ok();
        let current_hash = current
            .as_ref()
            .map(|c| blake3::hash(c.as_bytes()).to_hex().to_string());
        match (&file.before_blake3, current_hash) {
            (Some(expected), Some(actual)) if expected == &actual => {}
            (None, None) => {}
            _ => {
                return Err(format!(
                    "file {} before-state drifted from the artifact chain",
                    file.path
                ))
            }
        }
        let (diff, added, removed) = crate::frozen::unified_diff(
            &file.path,
            current.as_deref().unwrap_or(""),
            &file.content,
        )?;
        diff_text.push_str(&diff);
        added_total += added;
        removed_total += removed;
    }
    if diff_text != attempt.diff_unified
        || added_total != attempt.added_lines
        || removed_total != attempt.removed_lines
    {
        return Err(format!(
            "attempt {}: unified diff drifted from the frozen artifact",
            attempt.index
        ));
    }
    let candidate = Candidate {
        files: attempt
            .files
            .iter()
            .map(|f| (f.path.clone(), f.content.clone()))
            .collect(),
    };
    apply_candidate(workspace, &candidate)?;
    Ok(())
}

fn build_record(
    capture: &FrozenCapture,
    task: &CodingTaskSpec,
    run: &FrozenRun,
    verified_index: Option<usize>,
    workspace: &Path,
) -> Result<OutcomeLedgerEntry, String> {
    let verified = verified_index.is_some();
    // The durable execution class of one frozen attempt: a local step's
    // frozen LocalZero authority establishes `local`; a fixture stand-in
    // model has no durable route-time tier authority, so its class is
    // `unknown` (counted as NEITHER local nor hosted by the ledger — the
    // honest pre-tier state, never a fabricated hosted tier).
    let class_of = |local: bool| {
        if local {
            ExecutionClass::Local
        } else {
            ExecutionClass::Unknown
        }
    };
    let model_sequence: Vec<RunModelStep> = run
        .attempts
        .iter()
        .map(|a| RunModelStep {
            provider: a.provider.clone(),
            model: a.model.clone(),
            local: a.local,
            execution_class: class_of(a.local),
        })
        .collect();
    // The exactly-preserved boundary classes of the frozen attempt history:
    // the classification authority (a folded display sequence never
    // depresses the local cohorts), never a re-inference.
    let first_execution_class = model_sequence
        .first()
        .map(|step| step.execution_class)
        .unwrap_or(ExecutionClass::Unknown);
    let final_execution_class = model_sequence
        .last()
        .map(|step| step.execution_class)
        .unwrap_or(ExecutionClass::Unknown);
    // The replay's attribution comes from the frozen capture itself (every
    // attempt's provider/model is recorded): complete.
    let attribution_state = faktor_agent::AttributionState::Complete;
    let steps: Vec<SettlementStep> = run
        .attempts
        .iter()
        .map(|a| SettlementStep {
            provider: a.provider.clone(),
            model: a.model.clone(),
            local: a.local,
            execution_class: class_of(a.local),
            spend_micro: a.spend_micro,
        })
        .collect();
    let model_spend_micro = steps.iter().map(|s| s.spend_micro).sum::<u64>();
    let rework_spend_micro = run
        .attempts
        .iter()
        .skip(1)
        .map(|a| a.spend_micro)
        .sum::<u64>();
    let frontier_price = capture.models.iter().find(|m| !m.local).map(|m| &m.pricing);
    // The runtime arm's attempts are the deterministic frozen script (a
    // local, zero-cost plumbing model): no counterfactual measurement exists
    // for it, so its avoided-hosted fact is UNAVAILABLE (never a fabricated
    // zero — v2 settlement semantics).
    let avoided_hosted = if run.prompt.arm.is_runtime() {
        MeasuredMoney::Unavailable
    } else {
        MeasuredMoney::measured(
            run.attempts
                .iter()
                .filter(|a| a.local)
                .filter_map(|a| {
                    frontier_price
                        .and_then(|p| p.snapshot().settle_cost(a.tokens_in, 0, 0, a.tokens_out))
                })
                .sum::<u64>(),
        )
    };
    let total_attempts = run.attempts.len() as u64;
    let repair_attempts = if verified {
        total_attempts.saturating_sub(1)
    } else {
        total_attempts
    };
    let frontier_escalations = run.attempts.iter().skip(1).filter(|a| !a.local).count() as u64;
    let verified_samples: Vec<RunVerifiedSample> = run
        .attempts
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let verified_success = verified_index == Some(i);
            let (rework_cost_micro, rework_turns) = if verified_success {
                (0, 0)
            } else {
                let downstream: u64 = run.attempts[i + 1..].iter().map(|x| x.spend_micro).sum();
                (downstream, (run.attempts.len() - 1 - i) as u64)
            };
            RunVerifiedSample {
                provider: a.provider.clone(),
                model: a.model.clone(),
                phase: task.phase,
                verified_success,
                rework_cost_micro,
                rework_turns,
            }
        })
        .collect();
    let change_scale = change_scale(task, workspace)?;
    let run_id = format!(
        "econ-bench/{}/{}/{}",
        capture.capture_id,
        task.id,
        run.prompt.arm.as_str()
    );
    let outcome = TerminalEngineeringOutcome {
        schema_version: OUTCOME_RECORD_SCHEMA_V,
        run_id: run_id.clone(),
        session_id: 1_000 + u64::from(task.order),
        task_id: 2_000 + u64::from(task.order),
        task_class: task.task_class,
        risk_class: task.risk_bucket,
        candidate_snapshot: None,
        final_snapshot: run.final_tree_blake3.clone(),
        accepted: verified,
        integrated: verified,
        // In this benchmark's model every attempt ends in a verification:
        // the single-attempt-verified runs are exactly the runs whose FIRST
        // verification attempt passed (the ledger field's honest meaning).
        verified_first_verification_pass: verified && total_attempts == 1,
        repair_attempts,
        model_sequence,
        first_execution_class,
        final_execution_class,
        attribution_state,
        wall_time_ms: run.wall_time_ms,
        human_interventions: 0,
        proof: if verified {
            ProofCoverage {
                checks_passed: 1,
                checks_required: 1,
            }
        } else {
            ProofCoverage {
                checks_passed: 0,
                checks_required: 1,
            }
        },
        change_scale,
        rollback_events: 0,
        recovery_events: 0,
        final_basis: if verified {
            FinalVerificationBasis::DeterministicChecks
        } else {
            FinalVerificationBasis::Unverified
        },
        frontier_escalations,
        completed_at_ms: run.completed_at_ms,
        verified_samples,
    };
    // The settlement projection: the frozen run's exact recorded money
    // (fixture price table / local zero / runtime-script zero), classified
    // `complete` because every attempt's spend was measured by the capture
    // lane (never estimated at replay time).
    let settlement = EconomicSettlement {
        schema_version: faktor_agent::outcome_ledger::SETTLEMENT_SCHEMA_V,
        run_id,
        seq: 0,
        money_state: MoneyState::Complete,
        reason: "frozen capture: per-attempt spend recorded by the capture lane (fixture price table / local zero)".into(),
        model_spend_micro: Some(model_spend_micro),
        verification_spend_micro: Some(0),
        rework_spend_micro: Some(rework_spend_micro),
        avoided_hosted,
        steps,
        settled_at_ms: run.completed_at_ms,
    };
    Ok(OutcomeLedgerEntry {
        outcome,
        settlement,
    })
}

fn change_scale(task: &CodingTaskSpec, workspace: &Path) -> Result<ChangedCodeScale, String> {
    let final_files = crate::coding::read_tree(workspace)?;
    let mut files = 0u64;
    let mut added_lines = 0u64;
    let mut removed_lines = 0u64;
    let mut paths: std::collections::BTreeSet<&String> = std::collections::BTreeSet::new();
    paths.extend(task.repo_files.keys());
    paths.extend(final_files.keys());
    for path in paths {
        let before = task.repo_files.get(path).map(String::as_str).unwrap_or("");
        let after = final_files.get(path).map(String::as_str).unwrap_or("");
        if before == after {
            continue;
        }
        files += 1;
        let (_, added, removed) = crate::frozen::unified_diff(path, before, after)?;
        added_lines += added;
        removed_lines += removed;
    }
    Ok(ChangedCodeScale {
        files,
        added_lines,
        removed_lines,
    })
}

fn arm_metrics(replayed: &[ReplayedRun]) -> BTreeMap<String, ArmMetrics> {
    let mut arms = BTreeMap::new();
    for arm in Arm::WITH_RUNTIME {
        let runs: Vec<&ReplayedRun> = replayed
            .iter()
            .filter(|r| r.run.prompt.arm == arm)
            .collect();
        if runs.is_empty() {
            continue;
        }
        let entries: Vec<OutcomeLedgerEntry> = runs.iter().map(|r| r.record.clone()).collect();
        let ledger = OutcomeLedgerMetrics::compute(&entries);
        let verified = runs.iter().filter(|r| r.run.verified).count() as u64;
        let first_pass = runs
            .iter()
            .filter(|r| r.run.verified && r.run.attempts.len() == 1)
            .count() as u64;
        let attempts: u64 = runs.iter().map(|r| r.run.attempts.len() as u64).sum();
        let tokens_in: u64 = runs
            .iter()
            .flat_map(|r| r.run.attempts.iter())
            .map(|a| a.tokens_in)
            .sum();
        let tokens_out: u64 = runs
            .iter()
            .flat_map(|r| r.run.attempts.iter())
            .map(|a| a.tokens_out)
            .sum();
        let prompt_tokens_est_total: u64 = runs
            .iter()
            .flat_map(|r| r.run.attempts.iter())
            .map(|a| a.prompt_tokens_est)
            .sum();
        let inference_spend_micro: u64 = runs
            .iter()
            .flat_map(|r| r.run.attempts.iter())
            .map(|a| a.spend_micro)
            .sum();
        let rework_spend_micro: u64 = runs
            .iter()
            .map(|r| r.record.settlement.rework_spend_micro.unwrap_or(0))
            .sum();
        let rework_turns: u64 = runs
            .iter()
            .map(|r| {
                r.record
                    .outcome
                    .verified_samples
                    .iter()
                    .map(|s| s.rework_turns)
                    .sum::<u64>()
            })
            .sum();
        let wall_time_ms: u64 = runs.iter().map(|r| r.run.wall_time_ms).sum();
        let mut local_attempts = 0u64;
        let mut frontier_attempts = 0u64;
        for r in &runs {
            for a in &r.run.attempts {
                if a.local {
                    local_attempts += 1;
                } else {
                    frontier_attempts += 1;
                }
            }
        }
        let local_only_verified = runs
            .iter()
            .filter(|r| r.run.verified && r.run.attempts.iter().all(|a| a.local))
            .count() as u64;
        let mut local_failure_runs = 0u64;
        let mut fallback_runs = 0u64;
        for r in &runs {
            let local_failed = r.run.attempts.iter().any(|a| a.local && !a.passed());
            if local_failed {
                local_failure_runs += 1;
                let local_first_fail = r
                    .run
                    .attempts
                    .iter()
                    .position(|a| a.local && !a.passed())
                    .expect("local failure position");
                let later_frontier_verified = r.run.attempts[local_first_fail + 1..]
                    .iter()
                    .any(|a| !a.local && a.passed());
                if later_frontier_verified {
                    fallback_runs += 1;
                }
            }
        }
        let frontier_escalations = runs
            .iter()
            .filter(|r| {
                r.run
                    .attempts
                    .iter()
                    .enumerate()
                    .any(|(i, a)| i > 0 && !a.local)
            })
            .count() as u64;
        arms.insert(
            arm.as_str().to_string(),
            ArmMetrics {
                arm: arm.as_str().into(),
                runs: runs.len() as u64,
                verified,
                first_pass_verified: first_pass,
                verified_first_pass_ppm: ppm(first_pass, verified),
                completion_ppm: ppm(verified, runs.len() as u64),
                attempts,
                tokens_in,
                tokens_out,
                prompt_tokens_est_total,
                inference_spend_micro,
                rework_spend_micro,
                rework_turns,
                wall_time_ms,
                local_attempts,
                frontier_attempts,
                local_only_verified,
                local_only_verified_ppm: ppm(local_only_verified, verified),
                local_failure_runs,
                fallback_runs,
                local_to_hosted_fallback_ppm: ppm(fallback_runs, local_failure_runs),
                frontier_escalations,
                frontier_escalation_ppm: ppm(frontier_escalations, runs.len() as u64),
                ledger,
            },
        );
    }
    arms
}

fn comparison(arms: &BTreeMap<String, ArmMetrics>) -> ArmComparison {
    let empty = |arm: Arm| ArmMetrics {
        arm: arm.as_str().into(),
        runs: 0,
        verified: 0,
        first_pass_verified: 0,
        verified_first_pass_ppm: 0,
        completion_ppm: 0,
        attempts: 0,
        tokens_in: 0,
        tokens_out: 0,
        prompt_tokens_est_total: 0,
        inference_spend_micro: 0,
        rework_spend_micro: 0,
        rework_turns: 0,
        wall_time_ms: 0,
        local_attempts: 0,
        frontier_attempts: 0,
        local_only_verified: 0,
        local_only_verified_ppm: 0,
        local_failure_runs: 0,
        fallback_runs: 0,
        local_to_hosted_fallback_ppm: 0,
        frontier_escalations: 0,
        frontier_escalation_ppm: 0,
        ledger: OutcomeLedgerMetrics::default(),
    };
    let baseline = arms
        .get(Arm::Baseline.as_str())
        .cloned()
        .unwrap_or_else(|| empty(Arm::Baseline));
    let machinery = arms
        .get(Arm::FaktorMachinery.as_str())
        .cloned()
        .unwrap_or_else(|| empty(Arm::FaktorMachinery));
    ArmComparison {
        baseline_verified: baseline.verified,
        machinery_verified: machinery.verified,
        completion_delta_ppm: i64::from(machinery.completion_ppm)
            - i64::from(baseline.completion_ppm),
        baseline_first_pass_ppm: baseline.verified_first_pass_ppm,
        machinery_first_pass_ppm: machinery.verified_first_pass_ppm,
        first_pass_delta_ppm: i64::from(machinery.verified_first_pass_ppm)
            - i64::from(baseline.verified_first_pass_ppm),
        baseline_tokens_in: baseline.tokens_in,
        machinery_tokens_in: machinery.tokens_in,
        tokens_in_delta: machinery.tokens_in as i64 - baseline.tokens_in as i64,
        baseline_tokens_out: baseline.tokens_out,
        machinery_tokens_out: machinery.tokens_out,
        tokens_out_delta: machinery.tokens_out as i64 - baseline.tokens_out as i64,
        baseline_prompt_tokens_est: baseline.prompt_tokens_est_total,
        machinery_prompt_tokens_est: machinery.prompt_tokens_est_total,
        prompt_tokens_delta: machinery.prompt_tokens_est_total as i64
            - baseline.prompt_tokens_est_total as i64,
        baseline_verified_cost_micro: baseline.ledger.verified_cost_per_successful_change_micro,
        machinery_verified_cost_micro: machinery.ledger.verified_cost_per_successful_change_micro,
        verified_cost_delta_micro: machinery.ledger.verified_cost_per_successful_change_micro
            as i64
            - baseline.ledger.verified_cost_per_successful_change_micro as i64,
    }
}
