#![cfg(unix)]
//! ROUTER-ECONOMICS REGRESSION tests for `faktor-tests-router-economics`
//! (audit item 9 follow-up). These tests certify the ROUTER's economics
//! discipline over a scripted, policy-independent fake model; they are NOT
//! model-quality evidence and must never be cited as coding-competitiveness
//! or local-model-ability measurements. Real model evidence comes from the
//! capture/replay coding benchmark in the same crate. Every test here is
//! adversarial about honesty:
//!
//! - the corpus contract is proved BOTH directions with real subprocesses
//!   (pristine repos fail their own tests; the scripted fix passes at the
//!   documented attempt; unsolvable patches never pass);
//! - the regression is deterministic across full runs;
//! - a policy regression (always-frontier) still runs the comparison
//!   harness but produces measurably worse values;
//! - unsolvable tasks are counted as failures, never successes;
//! - the verified-outcome learning loop records real measured samples;
//! - verification is wall-bounded and the loader rejects hostile corpora.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ecc::PrimaryPolicy;
use faktor_tests_router_economics as ecc;

fn write_file(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("create parent");
    std::fs::write(path, content).expect("write fixture file");
}

fn task_json(id: &str, fix_at_attempt: &str) -> String {
    format!(
        r#"{{"id":"{id}","order":1,"task_class":"easy","phase":"implement","input_tokens":1000,"output_tokens":100,"fix_at_attempt":{fix_at_attempt}}}"#
    )
}

fn scratch_task(root: &Path, name: &str, json_id: &str, fix_at_attempt: &str) -> PathBuf {
    let dir = root.join(name);
    write_file(&dir.join("repo/program.sh"), "echo ok\n");
    write_file(&dir.join("repo/verify.sh"), "exit 0\n");
    write_file(&dir.join("patches/attempt-1.sh"), "echo ok\n");
    write_file(&dir.join("task.json"), &task_json(json_id, fix_at_attempt));
    dir
}

// ------------------------------------------------------ corpus + verification

#[test]
fn corpus_contract_holds_in_both_directions_under_real_verification() {
    let corpus = ecc::load_corpus(&ecc::corpus_root()).expect("frozen corpus loads");
    assert_eq!(corpus.tasks.len(), 12);
    assert!(
        corpus.tasks.iter().filter(|t| t.solvable()).count() >= 8,
        "most planted defects must be fixable by the scripted model"
    );
    assert!(
        corpus.tasks.iter().filter(|t| !t.solvable()).count() >= 1,
        "the corpus must carry at least one unsolvable task"
    );
    assert!(
        corpus.tasks.iter().any(|t| t.fix_at_attempt == Some(2)),
        "the corpus must carry tasks that require a second attempt"
    );
    assert!(
        corpus
            .tasks
            .iter()
            .any(|t| t.fix_at_attempt.is_some_and(|f| f >= 3)),
        "the corpus must carry at least one deeply reworked task"
    );
    let orders: Vec<u32> = corpus.tasks.iter().map(|t| t.order).collect();
    assert!(
        orders.windows(2).all(|w| w[0] < w[1]),
        "the frozen execution order must be strictly ascending"
    );
    assert_eq!(corpus.tasks.first().expect("first").id, "counter-sum");
    assert_eq!(corpus.tasks.last().expect("last").id, "sort-three");
    let scratch = tempfile::TempDir::new().expect("scratch");
    for spec in &corpus.tasks {
        let ws = scratch.path().join(&spec.id);
        ecc::copy_repo(spec, &ws).expect("materialize repo copy");
        assert_eq!(
            ecc::run_verification(&ws, ecc::VERIFY_TIMEOUT),
            ecc::VerifyResult::Failed,
            "{}: the pristine repo MUST fail its own tests",
            spec.id
        );
        for i in 0..spec.patches.len() {
            ecc::apply_scripted_patch(&ws, spec, i).expect("apply scripted patch");
            let passed =
                ecc::run_verification(&ws, ecc::VERIFY_TIMEOUT) == ecc::VerifyResult::Passed;
            let expected = spec.fix_at_attempt == Some(i as u32 + 1);
            assert_eq!(
                passed,
                expected,
                "{} attempt {}: scripted patch verification outcome",
                spec.id,
                i + 1
            );
        }
    }
    let again = ecc::load_corpus(&ecc::corpus_root()).expect("reload corpus");
    assert_eq!(corpus.digest, again.digest, "corpus digest is stable");
}

#[test]
fn verification_is_wall_bounded() {
    let scratch = tempfile::TempDir::new().expect("scratch");
    let ws = scratch.path().join("hanging");
    write_file(&ws.join("verify.sh"), "exec sleep 30\n");
    let started = Instant::now();
    let result = ecc::run_verification(&ws, Duration::from_millis(200));
    assert_eq!(result, ecc::VerifyResult::TimedOut);
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "a hanging verification must be killed at the bound"
    );
}

#[test]
fn loader_rejects_hostile_corpus_shapes() {
    let scratch = tempfile::TempDir::new().expect("scratch");
    // id mismatch with the directory name.
    let bad = scratch_task(scratch.path(), "task-a", "other-id", "1");
    let err = ecc::load_task_dir(&bad).expect_err("id mismatch must be rejected");
    assert!(err.contains("does not match"), "unexpected: {err}");
    // missing verification script.
    let bad = scratch_task(scratch.path(), "task-b", "task-b", "1");
    std::fs::remove_file(bad.join("repo/verify.sh")).expect("remove verify.sh");
    assert!(ecc::load_task_dir(&bad).is_err());
    // non-sequential attempt patches.
    let bad = scratch_task(scratch.path(), "task-c", "task-c", "1");
    write_file(&bad.join("patches/attempt-3.sh"), "echo ok\n");
    let err = ecc::load_task_dir(&bad).expect_err("patch gap must be rejected");
    assert!(err.contains("sequential"), "unexpected: {err}");
    // fix_at_attempt beyond the available patches.
    let bad = scratch_task(scratch.path(), "task-d", "task-d", "5");
    let err = ecc::load_task_dir(&bad).expect_err("out-of-range fix must be rejected");
    assert!(err.contains("fix_at_attempt"), "unexpected: {err}");
    // a symlink anywhere under the repo is refused.
    let bad = scratch_task(scratch.path(), "task-e", "task-e", "1");
    std::os::unix::fs::symlink("/etc/passwd", bad.join("repo/leak")).expect("create symlink");
    let err = ecc::load_task_dir(&bad).expect_err("symlink must be rejected");
    assert!(err.contains("symlink"), "unexpected: {err}");
    // corpus-level shape bound: an empty corpus root is refused.
    let empty = scratch.path().join("empty-corpus");
    std::fs::create_dir_all(&empty).expect("create empty corpus");
    assert!(ecc::load_corpus(&empty).is_err());
}

// ------------------------------------------------- router-economics regression

#[test]
fn router_economics_regression_passes_generous_bounds_and_reports_artifact() {
    let cert = ecc::run_certification(PrimaryPolicy::Economy).expect("certification runs");
    assert_eq!(cert.schema, ecc::SCHEMA);
    assert_eq!(cert.corpus.tasks, 12);
    assert!(cert.corpus.solvable >= 8);
    assert!(cert.corpus.unsolvable >= 1);
    let failing: Vec<_> = cert.bounds.iter().filter(|b| !b.pass).collect();
    assert!(
        cert.all_bounds_pass,
        "economy policy must clear the generous bounds; failing: {failing:?}"
    );
    assert!(cert.lanes.contains_key("economy"));
    assert!(cert.lanes.contains_key("frontier_only"));
    let ratio = cert
        .comparison
        .cost_per_success_ratio_ppm
        .expect("both lanes verified something");
    assert!(
        ratio <= ecc::BOUND_COST_PER_SUCCESS_RATIO_MAX_PPM as u64,
        "verified cost per success ratio {ratio} ppm exceeds the bound"
    );
    // The machine-readable artifact lands where the nightly lane binds it.
    let path = ecc::default_artifact_path();
    ecc::write_artifact(&cert, &path).expect("artifact write");
    let raw = std::fs::read_to_string(&path).expect("artifact read");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("artifact is strict JSON");
    assert_eq!(parsed["schema"], ecc::SCHEMA);
    assert_eq!(parsed["primary_policy"], "economy");
    assert_eq!(parsed["lanes"]["economy"]["unsolvable_verified"], 0);
    for bound in parsed["bounds"].as_array().expect("bounds array") {
        assert_eq!(bound["pass"], true, "bound {} failed", bound["name"]);
    }
}

#[test]
fn certification_is_deterministic_across_runs() {
    let first = ecc::run_certification(PrimaryPolicy::Economy).expect("first run");
    let second = ecc::run_certification(PrimaryPolicy::Economy).expect("second run");
    assert_eq!(
        serde_json::to_value(&first).expect("first json"),
        serde_json::to_value(&second).expect("second json"),
        "identical corpus runs must produce identical results"
    );
    assert_eq!(first.corpus.digest, second.corpus.digest);
}

#[test]
fn unsolvable_tasks_are_counted_as_failures_not_successes() {
    let cert = ecc::run_certification(PrimaryPolicy::Economy).expect("certification runs");
    let unsolvable: Vec<_> = cert.per_task.iter().filter(|t| !t.solvable).collect();
    assert!(
        !unsolvable.is_empty(),
        "corpus must contain unsolvable tasks"
    );
    for row in &unsolvable {
        assert!(!row.economy.verified, "{} must not verify", row.id);
        assert!(!row.frontier_only.verified, "{} must not verify", row.id);
        assert!(
            row.economy.attempts >= 1,
            "{} must consume attempts",
            row.id
        );
        assert_eq!(
            row.frontier_only.attempts,
            ecc::MAX_ATTEMPTS as u64,
            "{}: the always-frontier control must exhaust the attempt budget",
            row.id
        );
        assert!(row.economy.verified_by.is_none());
    }
    assert_eq!(cert.lanes["economy"].unsolvable_verified, 0);
    assert_eq!(cert.lanes["frontier_only"].unsolvable_verified, 0);
    assert!(
        cert.lanes["economy"].completion_rate_ppm < 1_000_000,
        "a corpus with unsolvable tasks can never be 100% complete"
    );
}

#[test]
fn always_frontier_regression_still_runs_but_measures_worse() {
    let baseline = ecc::run_certification(PrimaryPolicy::Economy).expect("baseline run");
    let regression =
        ecc::run_certification(PrimaryPolicy::AlwaysFrontier).expect("regression run completes");
    // The harness still produces a full measurement for the regressed policy.
    assert!(regression.lanes.contains_key("frontier_only"));
    assert_eq!(regression.comparison.primary, "frontier_only");
    // ... but the measured value is worse: no local substitution, no
    // non-frontier substitution, and a strictly higher verified cost per
    // successful change than the certified economy baseline.
    let regressed = &regression.lanes["frontier_only"];
    assert_eq!(regressed.local_substitution_rate_ppm, Some(0));
    assert_eq!(regressed.non_frontier_substitution_rate_ppm, Some(0));
    let base_cost = baseline.lanes["economy"]
        .cost_per_success_micro
        .expect("economy verified");
    let regressed_cost = regressed.cost_per_success_micro.expect("frontier verified");
    assert!(
        regressed_cost > base_cost,
        "always-frontier ({regressed_cost}) must cost more per success than economy ({base_cost})"
    );
    let base_ratio = baseline
        .comparison
        .cost_per_success_ratio_ppm
        .expect("baseline ratio");
    let regressed_ratio = regression
        .comparison
        .cost_per_success_ratio_ppm
        .expect("regression ratio");
    assert!(
        regressed_ratio > base_ratio,
        "the regressed policy's cost ratio ({regressed_ratio}) must be worse than baseline ({base_ratio})"
    );
    assert!(
        !regression.all_bounds_pass,
        "the always-frontier regression must fail at least one generous bound"
    );
    // Success is not bought back: completion cannot exceed the baseline.
    assert!(
        regressed.completion_rate_ppm <= baseline.lanes["economy"].completion_rate_ppm,
        "the regression must not improve verified completion"
    );
}

#[test]
fn outcome_learning_records_measured_verified_samples() {
    let corpus = ecc::load_corpus(&ecc::corpus_root()).expect("frozen corpus loads");
    let scratch = tempfile::TempDir::new().expect("scratch");
    let run = ecc::run_lane_with_store(ecc::LaneKind::Economy, &corpus.tasks, scratch.path());
    // `clamp` (phase debug, class medium) is the first debug-phase task: the
    // cheap-first policy tries the local model, its change fails real
    // verification, the driver escalates, and the gate records a local
    // failure with MEASURED downstream rework (plus whatever the later
    // tasks of the same key add).
    let clamp = corpus
        .tasks
        .iter()
        .find(|t| t.id == "clamp")
        .expect("clamp task");
    let clamp_outcome = run
        .report
        .outcomes
        .iter()
        .find(|o| o.id == "clamp")
        .expect("clamp outcome");
    assert_eq!(
        clamp_outcome.attempts[0].tier,
        ecc::ModelTier::Local,
        "cheap-first must open with the local model on a fresh key"
    );
    assert!(!clamp_outcome.attempts[0].verified);
    assert!(
        clamp_outcome.verified,
        "clamp must verify on a later attempt"
    );
    let local = ecc::stats_for(&run.store, "local", "qwen-3b", clamp)
        .expect("local model must have recorded samples on the debug/medium key");
    assert!(local.failures_first_pass >= 1);
    assert!(
        local.rework_cost_micro_sum > 0,
        "the failure sample must carry measured downstream rework spend"
    );
    let cheap = ecc::stats_for(&run.store, "cheap", "eco-mini", clamp)
        .expect("cheap model must have recorded samples");
    assert!(cheap.successes_first_pass >= 1);
    assert_eq!(
        cheap.sample_count,
        cheap.successes_first_pass + cheap.failures_first_pass
    );
    // The lane genuinely reworked at least one task.
    assert!(run.report.metrics.rework_cost_micro > 0);
}
