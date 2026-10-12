#![cfg(unix)]
//! Coding-benchmark harness tests: corpus contract in BOTH directions with
//! real subprocesses, the frozen labeled fixture (regeneration drift gate),
//! deterministic replay through the wired outcome ledger, the
//! with/without-machinery measurement axis, and adversarial artifact /
//! candidate shapes.
//!
//! Honesty: everything exercised here is `capture_source = fixture` —
//! machinery proof, never model-quality evidence. Real model numbers exist
//! only after the opt-in live capture lane freezes `capture_source = live`
//! artifacts.

use std::collections::BTreeMap;
use std::path::Path;

use ecc::coding::{
    apply_candidate, copy_coding_repo, load_coding_corpus, parse_model_candidate,
    run_task_verification, runtime_available, validate_candidate_path, Candidate,
    CODING_VERIFY_TIMEOUT,
};
use ecc::frozen::{load_capture, Arm, CaptureSource, VerifyOutcome};
use ecc::replay::replay_capture;
use faktor_tests_router_economics as ecc;

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("create dirs");
    std::fs::write(path, content).expect("write");
}

// ------------------------------------------------------- corpus contract

#[test]
fn coding_corpus_contract_holds_in_both_directions() {
    let corpus = load_coding_corpus(&ecc::coding::coding_corpus_root()).expect("corpus loads");
    assert_eq!(corpus.tasks.len(), 9);
    let scratch = tempfile::TempDir::new().expect("scratch");
    for task in &corpus.tasks {
        assert!(
            runtime_available(task.runtime),
            "{}: required runtime {} missing on this host",
            task.id,
            task.runtime.as_str()
        );
        let ws = scratch.path().join(&task.id);
        copy_coding_repo(task, &ws).expect("copy repo");
        let pristine = run_task_verification(&ws, CODING_VERIFY_TIMEOUT);
        assert_eq!(
            pristine.result,
            ecc::VerifyResult::Failed,
            "{}: the pristine repo MUST fail its own verification (stdout: {})",
            task.id,
            pristine.stdout_tail
        );
        let reference: Candidate = Candidate {
            files: task
                .reference_files
                .iter()
                .filter(|(p, _)| p.as_str() != "verify.sh")
                .map(|(p, c)| (p.clone(), c.clone()))
                .collect(),
        };
        apply_candidate(&ws, &reference).expect("apply reference fix");
        let fixed = run_task_verification(&ws, CODING_VERIFY_TIMEOUT);
        assert_eq!(
            fixed.result,
            ecc::VerifyResult::Passed,
            "{}: the reference fix MUST pass verification (stderr: {})",
            task.id,
            fixed.stderr_tail
        );
    }
    // A second load yields the same frozen digest.
    let again = load_coding_corpus(&ecc::coding::coding_corpus_root()).expect("reload");
    assert_eq!(corpus.digest, again.digest);
}

#[test]
fn coding_loader_rejects_hostile_corpus_shapes() {
    let scratch = tempfile::TempDir::new().expect("scratch");
    let root = scratch.path();
    let task_dir = root.join("task-a");
    write(
        &task_dir.join("task.json"),
        r#"{"id":"other","order":1,"category":"debugging","language":"shell","runtime":"sh","task_class":"easy","phase":"debug","goal":"fix it"}"#,
    );
    write(&task_dir.join("repo/verify.sh"), "exit 1\n");
    write(&task_dir.join("reference/run.sh"), "ok\n");
    let err = ecc::coding::load_coding_task(&task_dir).expect_err("id mismatch");
    assert!(err.contains("does not match"), "unexpected: {err}");

    // Language/runtime mismatch.
    let task_dir = root.join("task-b");
    write(
        &task_dir.join("task.json"),
        r#"{"id":"task-b","order":1,"category":"debugging","language":"shell","runtime":"python3","task_class":"easy","phase":"debug","goal":"fix it"}"#,
    );
    write(&task_dir.join("repo/verify.sh"), "exit 1\n");
    write(&task_dir.join("reference/run.sh"), "ok\n");
    let err = ecc::coding::load_coding_task(&task_dir).expect_err("runtime mismatch");
    assert!(err.contains("does not match"), "unexpected: {err}");

    // Identical reference tree (no defect planted).
    let task_dir = root.join("task-c");
    write(
        &task_dir.join("task.json"),
        r#"{"id":"task-c","order":1,"category":"debugging","language":"shell","runtime":"sh","task_class":"easy","phase":"debug","goal":"fix it"}"#,
    );
    write(&task_dir.join("repo/verify.sh"), "exit 1\n");
    write(&task_dir.join("repo/run.sh"), "same\n");
    write(&task_dir.join("reference/verify.sh"), "exit 1\n");
    write(&task_dir.join("reference/run.sh"), "same\n");
    let err = ecc::coding::load_coding_task(&task_dir).expect_err("identical reference");
    assert!(err.contains("identical"), "unexpected: {err}");

    // Symlink anywhere in the task is refused.
    let task_dir = root.join("task-d");
    write(
        &task_dir.join("task.json"),
        r#"{"id":"task-d","order":1,"category":"debugging","language":"shell","runtime":"sh","task_class":"easy","phase":"debug","goal":"fix it"}"#,
    );
    write(&task_dir.join("repo/verify.sh"), "exit 1\n");
    write(&task_dir.join("reference/run.sh"), "ok\n");
    std::os::unix::fs::symlink("/etc/passwd", task_dir.join("repo/leak")).expect("symlink");
    let err = ecc::coding::load_coding_task(&task_dir).expect_err("symlink refused");
    assert!(err.contains("symlink"), "unexpected: {err}");

    // Empty corpus root is refused.
    let empty = root.join("empty");
    std::fs::create_dir_all(&empty).expect("mkdir");
    assert!(load_coding_corpus(&empty).is_err());
}

// ----------------------------------------------------------- frozen fixture

#[test]
fn fixture_capture_is_frozen_and_regenerates_identically() {
    let scratch = tempfile::TempDir::new().expect("scratch");
    let generated = ecc::fixture::generate_fixture_capture(scratch.path()).expect("generate");
    if let Ok(dump) = std::env::var("FAKTOR_ECON_FIXTURE_DUMP") {
        ecc::frozen::save_capture(&generated, Path::new(&dump)).expect("dump fixture");
        eprintln!("fixture dumped to {dump}");
    }
    let path = ecc::frozen::fixture_capture_path();
    if std::env::var("FAKTOR_ECON_BLESS").ok().as_deref() == Some("1") {
        ecc::frozen::save_capture(&generated, &path).expect("bless fixture");
        eprintln!("blessed fixture written to {}", path.display());
    }
    let committed = load_capture(&path).expect("committed fixture loads");
    assert_eq!(committed.source, CaptureSource::Fixture);
    let generated_value = serde_json::to_value(&generated).expect("generated json");
    let committed_value = serde_json::to_value(&committed).expect("committed json");
    if let Some(diff) = first_json_diff(&generated_value, &committed_value, String::new()) {
        panic!("the committed fixture drifted from regeneration at {diff}; re-bless with FAKTOR_ECON_BLESS=1");
    }
    // Every task appears in both arms exactly once.
    for task in load_coding_corpus(&ecc::coding::coding_corpus_root())
        .expect("corpus")
        .tasks
    {
        for arm in Arm::ALL {
            assert!(
                committed.run(&task.id, arm).is_some(),
                "fixture misses ({}, {})",
                task.id,
                arm.as_str()
            );
        }
    }
}

// ------------------------------------------------------------------ replay

#[test]
fn replay_is_deterministic_and_reports_from_the_wired_ledger() {
    let scratch = tempfile::TempDir::new().expect("scratch");
    let first = ecc::replay::replay_all_committed(scratch.path()).expect("first replay");
    let second = ecc::replay::replay_all_committed(scratch.path()).expect("second replay");
    assert_eq!(
        serde_json::to_value(&first).expect("first json"),
        serde_json::to_value(&second).expect("second json"),
        "replay must be deterministic"
    );
    assert!(
        first.fixtures_only,
        "only fixture captures are committed today"
    );
    let fixture = first
        .captures
        .iter()
        .find(|c| c.capture_id == "fixture")
        .expect("fixture report");
    assert_eq!(fixture.source, "fixture");
    assert_eq!(fixture.wall_time_basis, "fixture");
    assert_eq!(fixture.token_basis, "estimate");
    assert_eq!(fixture.runs_replayed, 18, "9 tasks x 2 arms");
    let baseline = fixture.arms.get("baseline").expect("baseline arm");
    let machinery = fixture.arms.get("faktor_machinery").expect("machinery arm");
    assert_eq!(baseline.runs, 9);
    assert_eq!(machinery.runs, 9);
    // The unsolvable fixture run (order 6) can never verify in either arm.
    assert_eq!(baseline.verified, 8);
    assert_eq!(machinery.verified, 8);
    // Rework and fallback paths are exercised by the plan.
    assert!(baseline.attempts > 9, "some runs must rework");
    assert!(baseline.rework_spend_micro > 0 || baseline.rework_turns > 0);
    assert!(
        baseline.local_failure_runs >= 1,
        "the plan encodes a local first-attempt failure"
    );
    // The cohort metric is the wired ledger's fold, not a re-derivation.
    assert_eq!(fixture.cohort_ledger.runs, 18);
    assert_eq!(fixture.cohort_ledger.verified_changes, 16);
    assert!(
        fixture
            .cohort_ledger
            .verified_cost_per_successful_change_micro
            > 0,
        "frontier fixture attempts spent real recorded microUSD"
    );
    assert!(
        fixture.cohort_ledger.total_spend_micro
            >= fixture
                .cohort_ledger
                .verified_cost_per_successful_change_micro,
        "VCSC divides total cohort spend by successes"
    );
    assert!(fixture.cohort_ledger.total_rework_spend_micro > 0);
    // The plan encodes local-first failures that a later frontier attempt
    // rescued; the wired ledger's fallback counter (which counts local-first
    // runs ENDING hosted, verified or not) is bounded by the local failures
    // of both arms.
    assert!(
        baseline.fallback_runs >= 1,
        "plan encodes a rescued fallback"
    );
    assert!(
        fixture.cohort_ledger.local_to_frontier_fallback_runs
            <= baseline.local_failure_runs + machinery.local_failure_runs,
        "fallback accounting is bounded by local failures"
    );
    // The report artifact lands where the nightly lane binds it.
    let path = ecc::replay::default_replay_artifact_path();
    ecc::replay::write_replay_report(&first, &path).expect("write report");
    let raw = std::fs::read_to_string(&path).expect("read report");
    let parsed: serde_json::Value = serde_json::from_str(&raw).expect("strict json");
    assert_eq!(parsed["schema"], ecc::replay::REPLAY_SCHEMA);
    assert_eq!(parsed["fixtures_only"], true);
}

#[test]
fn machinery_axis_measures_token_and_completion_differences() {
    let scratch = tempfile::TempDir::new().expect("scratch");
    let report = ecc::replay::replay_all_committed(scratch.path()).expect("replay");
    let fixture = &report.captures[0];
    let comparison = &fixture.comparison;
    // The machinery prompt provably carries MORE tokens (contract +
    // intelligence) in the fixture records.
    assert!(
        comparison.prompt_tokens_delta > 0,
        "machinery prompts must be larger: {comparison:?}"
    );
    assert_eq!(
        comparison.tokens_in_delta, comparison.prompt_tokens_delta,
        "fixture tokens_in is the prompt estimate (token_basis = estimate)"
    );
    // The scripted fixture model ignores the prompt: completion outcomes and
    // output tokens are IDENTICAL across arms. Fixtures must not fabricate a
    // machinery advantage; real deltas require live capture.
    assert_eq!(comparison.completion_delta_ppm, 0);
    assert_eq!(comparison.baseline_verified, comparison.machinery_verified);
    assert_eq!(comparison.tokens_out_delta, 0);
    assert_eq!(comparison.first_pass_delta_ppm, 0);
}

// ------------------------------------------------------------- adversarial

#[test]
fn tampered_capture_content_is_rejected_by_digest_and_replay() {
    let scratch = tempfile::TempDir::new().expect("scratch");
    let generated = ecc::fixture::generate_fixture_capture(scratch.path()).expect("generate");
    let mut tampered = generated.clone();
    let run = &mut tampered.runs[0];
    let file = run.attempts[0]
        .files
        .first_mut()
        .expect("fixture run has changed files");
    file.content.push_str("/* tampered */\n");
    // Digest still validates only if recomputed; replay must then RE-EXECUTE
    // and reject the hash chain/diff drift.
    tampered = tampered.finish().expect("finish tampered");
    let err = replay_capture(&tampered, &corpus(), scratch.path()).expect_err("replay rejects");
    assert!(
        err.contains("drifted") || err.contains("hash"),
        "unexpected: {err}"
    );
}

#[test]
fn stale_digest_capture_is_rejected_by_the_loader() {
    let scratch = tempfile::TempDir::new().expect("scratch");
    let generated = ecc::fixture::generate_fixture_capture(scratch.path()).expect("generate");
    let mut value = serde_json::to_value(&generated).expect("json");
    value["runs"][0]["attempts"][0]["tokens_in"] = serde_json::json!(123456);
    let path = scratch.path().join("stale.json");
    std::fs::write(&path, serde_json::to_string_pretty(&value).expect("json"))
        .expect("write stale");
    let err = load_capture(&path).expect_err("stale digest rejected");
    assert!(err.contains("digest"), "unexpected: {err}");
}

#[test]
fn falsified_verification_outcome_is_rejected_at_replay() {
    let scratch = tempfile::TempDir::new().expect("scratch");
    let generated = ecc::fixture::generate_fixture_capture(scratch.path()).expect("generate");
    let mut tampered = generated.clone();
    // Find a verified run with a FAILED earlier attempt and flip that
    // attempt's recorded outcome to Passed (digest re-sealed; replay must
    // re-execute the real verification and catch it).
    let mut flipped = false;
    'outer: for run in &mut tampered.runs {
        if !run.verified {
            continue;
        }
        for attempt in &mut run.attempts {
            if attempt.verification.outcome == VerifyOutcome::Failed {
                attempt.verification.outcome = VerifyOutcome::Passed;
                flipped = true;
                break 'outer;
            }
        }
    }
    assert!(flipped, "fixture must contain a reworked verified run");
    let tampered = tampered.finish().expect("finish");
    let err = replay_capture(&tampered, &corpus(), scratch.path()).expect_err("replay rejects");
    assert!(err.contains("outcome drifted"), "unexpected: {err}");
}

#[test]
fn falsified_final_tree_hash_is_rejected_at_replay() {
    let scratch = tempfile::TempDir::new().expect("scratch");
    let generated = ecc::fixture::generate_fixture_capture(scratch.path()).expect("generate");
    let mut tampered = generated.clone();
    tampered.runs[0].final_tree_blake3 = "0".repeat(64);
    let tampered = tampered.finish().expect("finish");
    let err = replay_capture(&tampered, &corpus(), scratch.path()).expect_err("replay rejects");
    assert!(
        err.contains("drifted") || err.contains("hash"),
        "unexpected: {err}"
    );
}

#[test]
fn corpus_digest_mismatch_is_rejected_at_replay() {
    let scratch = tempfile::TempDir::new().expect("scratch");
    let generated = ecc::fixture::generate_fixture_capture(scratch.path()).expect("generate");
    let mut tampered = generated.clone();
    tampered.corpus_digest = "f".repeat(64);
    let tampered = tampered.finish().expect("finish");
    let err = replay_capture(&tampered, &corpus(), scratch.path()).expect_err("replay rejects");
    assert!(err.contains("corpus digest"), "unexpected: {err}");
}

#[test]
fn hostile_candidate_shapes_are_rejected() {
    // Path validation: traversal, absolute, backslash, verify.sh.
    for bad in [
        "../etc/passwd",
        "/etc/passwd",
        "a/../../b",
        "dir\\file",
        "verify.sh",
        "sub/verify.sh",
        "",
    ] {
        assert!(
            validate_candidate_path(bad).is_err(),
            "path {bad:?} must be rejected"
        );
    }
    assert!(validate_candidate_path("src/ok.py").is_ok());

    // Fenced parsing: last fenced block wins; bare JSON fallback works.
    let parsed = parse_model_candidate(
        "noise\n```json\n{\"files\":{\"a.txt\":\"one\"}}\n```\nmore\n```json\n{\"files\":{\"b.txt\":\"two\"}}\n```\n",
    )
    .expect("fenced candidate");
    assert_eq!(parsed.files.len(), 1);
    assert_eq!(parsed.files.get("b.txt").map(String::as_str), Some("two"));
    let bare = parse_model_candidate("{\"files\":{\"a.txt\":\"one\"}}").expect("bare json");
    assert_eq!(bare.files.len(), 1);
    assert!(parse_model_candidate("not json at all").is_err());
    assert!(
        parse_model_candidate("{\"files\":{}}").is_err(),
        "empty files"
    );
    assert!(
        parse_model_candidate("{\"files\":{\"../x\":\"y\"}}").is_err(),
        "hostile path inside payload"
    );
}

#[test]
fn candidate_application_refuses_symlink_and_preserves_verification() {
    let scratch = tempfile::TempDir::new().expect("scratch");
    let ws = scratch.path().join("ws");
    std::fs::create_dir_all(&ws).expect("mkdir");
    std::fs::write(ws.join("victim.txt"), "ORIGINAL").expect("victim");
    std::os::unix::fs::symlink(ws.join("victim.txt"), ws.join("link.txt")).expect("symlink");
    let mut files = BTreeMap::new();
    files.insert("link.txt".to_string(), "CLOBBERED".to_string());
    let err = apply_candidate(&ws, &Candidate { files }).expect_err("symlink refused");
    assert!(err.contains("symlink"), "unexpected: {err}");
    assert_eq!(
        std::fs::read_to_string(ws.join("victim.txt")).expect("read victim"),
        "ORIGINAL"
    );
    let mut files = BTreeMap::new();
    files.insert("verify.sh".to_string(), "exit 0".to_string());
    let err = apply_candidate(&ws, &Candidate { files }).expect_err("verify.sh refused");
    assert!(err.contains("verification"), "unexpected: {err}");
}

#[test]
fn selection_helpers_expose_expected_shapes() {
    let task = &corpus().tasks[0];
    let plan = ecc::coding::fixture_plan(task);
    assert!(!plan.is_empty());
    let candidate = ecc::coding::fixture_candidate(task, ecc::coding::FixtureRecipe::Reference);
    assert!(
        !candidate.files.contains_key("verify.sh"),
        "candidates never touch the verifier"
    );
}

fn corpus() -> ecc::coding::CodingCorpus {
    load_coding_corpus(&ecc::coding::coding_corpus_root()).expect("corpus")
}

/// First structural difference between two JSON values with a bounded
/// pointer (keeps failure output small even for huge captures).
fn first_json_diff(a: &serde_json::Value, b: &serde_json::Value, path: String) -> Option<String> {
    use serde_json::Value;
    match (a, b) {
        (Value::Object(am), Value::Object(bm)) => {
            for key in am.keys().chain(bm.keys()) {
                match (am.get(key), bm.get(key)) {
                    (Some(av), Some(bv)) => {
                        if let Some(diff) =
                            first_json_diff(av, bv, format!("{path}/{}", escape_key(key)))
                        {
                            return Some(diff);
                        }
                    }
                    _ => return Some(format!("{path}/{} (presence)", escape_key(key))),
                }
            }
            None
        }
        (Value::Array(av), Value::Array(bv)) => {
            if av.len() != bv.len() {
                return Some(format!(
                    "{path} (array length {} vs {})",
                    av.len(),
                    bv.len()
                ));
            }
            for (i, (x, y)) in av.iter().zip(bv.iter()).enumerate() {
                if let Some(diff) = first_json_diff(x, y, format!("{path}[{i}]")) {
                    return Some(diff);
                }
            }
            None
        }
        _ if a == b => None,
        _ => Some(format!("{path} ({a} vs {b})")),
    }
}

fn escape_key(key: &str) -> String {
    key.chars().take(64).collect()
}
