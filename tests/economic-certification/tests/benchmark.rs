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
    run_task_verification, runtime_available, validate_candidate_path, Candidate, Verification,
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
    let mut generated_value = serde_json::to_value(&generated).expect("generated json");
    let mut committed_value = serde_json::to_value(&committed).expect("committed json");
    // The recorded volatile normalization evidence is EXCLUDED from the
    // regeneration identity: its extracts are scheduling-dependent by design
    // (that is why the rules exist — under load the evidence refresh can
    // serve the previous hop's cached package), while the identity itself is
    // the normalized `prompt_blake3`. The recorded vocabulary is still gated
    // (asserted below and by `frozen::validate_run`).
    strip_volatile_evidence(&mut generated_value);
    strip_volatile_evidence(&mut committed_value);
    if let Some(diff) = first_json_diff(&generated_value, &committed_value, String::new()) {
        panic!("the committed fixture drifted from regeneration at {diff}; re-bless with FAKTOR_ECON_BLESS=1");
    }
    // The committed runtime attempts DO record their normalization
    // exclusions, and every recorded rule is one of the documented ones (a
    // new normalization rule that reached the committed artifact without the
    // vocabulary being extended fails here).
    let mut recorded_sections = 0usize;
    for run in &committed.runs {
        for attempt in &run.attempts {
            for section in &attempt.volatile {
                assert!(
                    ecc::daemon_arm::documented_volatile_rule(&section.name, &section.rationale),
                    "{}: committed volatile section names an undocumented rule {}",
                    run.task_id,
                    section.name
                );
                assert!(
                    !section.extract.is_empty(),
                    "{}: a recorded normalization exclusion must carry its extract",
                    run.task_id
                );
                recorded_sections += 1;
            }
        }
    }
    assert!(
        recorded_sections > 0,
        "the committed runtime fixture records its normalized-out evidence explicitly"
    );
    // Every task appears in every arm exactly once.
    for task in load_coding_corpus(&ecc::coding::coding_corpus_root())
        .expect("corpus")
        .tasks
    {
        for arm in Arm::WITH_RUNTIME {
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
    assert_eq!(fixture.runs_replayed, 27, "9 tasks x 3 arms");
    let baseline = fixture.arms.get("baseline").expect("baseline arm");
    let machinery = fixture.arms.get("faktor_machinery").expect("machinery arm");
    let runtime = fixture
        .arms
        .get("full_daemon_runtime")
        .expect("full_daemon_runtime arm");
    assert_eq!(baseline.runs, 9);
    assert_eq!(machinery.runs, 9);
    assert_eq!(runtime.runs, 9);
    // The unsolvable fixture run (order 6) can never verify in any arm.
    assert_eq!(baseline.verified, 8);
    assert_eq!(machinery.verified, 8);
    assert_eq!(
        runtime.verified, machinery.verified,
        "the runtime arm drives the SAME frozen outputs: its objective success axis must match the machinery arm exactly"
    );
    // The runtime arm is the frozen script: no hosted attempts, no spend.
    assert_eq!(runtime.frontier_attempts, 0);
    assert_eq!(runtime.inference_spend_micro, 0);
    assert_eq!(runtime.frontier_escalations, 0);
    assert_eq!(runtime.attempts, runtime.local_attempts);
    assert!(runtime.attempts > 9, "some runtime runs must rework");
    // Rework and fallback paths are exercised by the plan.
    assert!(baseline.attempts > 9, "some runs must rework");
    assert!(baseline.rework_spend_micro > 0 || baseline.rework_turns > 0);
    assert!(
        baseline.local_failure_runs >= 1,
        "the plan encodes a local first-attempt failure"
    );
    // The cohort metric is the wired ledger's fold, not a re-derivation.
    assert_eq!(fixture.cohort_ledger.runs, 27);
    assert_eq!(fixture.cohort_ledger.verified_changes, 24);
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
        fixture.cohort_ledger.local_to_hosted_fallback_runs
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
fn repair_excerpt_normalization_is_alias_independent() {
    let scratch = tempfile::TempDir::new().expect("scratch");
    let ws = scratch
        .path()
        .join("baseline")
        .join("py-migration-config-v2");
    std::fs::create_dir_all(&ws).expect("workspace dir");
    let canonical = ws.canonicalize().expect("canonical workspace");
    // Interpreters on hosts whose temp dir is a symlink alias (macOS
    // /var -> /private/var) print the CANONICAL spelling while the pipeline
    // hands `failure_excerpt_for` the path it created. The canonical spelling
    // must be replaced whole: replacing the non-canonical alias inside it
    // strands the differing prefix (`/private<workspace>`), which made the
    // repair prompt host-dependent (trusted-lane drift: 24 extra bytes = 6
    // prompt tokens per excerpt on the blessing host).
    let verification = Verification {
        result: ecc::VerifyResult::Failed,
        stdout_tail: String::new(),
        stderr_tail: format!(
            "Traceback (most recent call last):\n  File \"{}/migrate.py\", line 28\nKeyError: 'port'\n",
            canonical.display()
        ),
        millis: 0,
    };
    let from_alias = verification
        .failure_excerpt_for(&ws)
        .expect("a failed verification carries a repair excerpt");
    assert!(
        from_alias.contains("<workspace>/migrate.py"),
        "canonical workspace spelling must be replaced whole: {from_alias}"
    );
    let from_canonical = verification
        .failure_excerpt_for(&canonical)
        .expect("a failed verification carries a repair excerpt");
    assert_eq!(
        from_alias, from_canonical,
        "the repair excerpt must not depend on which host spelling names the workspace"
    );
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

// ------------------------------------------------- full_daemon_runtime arm

#[test]
fn full_daemon_runtime_arm_is_deterministic_and_runtime_gated() {
    let corpus = corpus();
    let scratch_a = tempfile::TempDir::new().expect("scratch a");
    let first = ecc::daemon_arm::run_runtime_arm(&corpus, scratch_a.path()).expect("drive once");
    let scratch_b = tempfile::TempDir::new().expect("scratch b");
    let second = ecc::daemon_arm::run_runtime_arm(&corpus, scratch_b.path()).expect("drive twice");
    assert_eq!(first.len(), 9, "the runtime arm covers the frozen corpus");
    // Every recorded normalization exclusion is DOCUMENTED (rule name +
    // rationale) and carries its exact extract; the pinned unit test proves
    // the extracts are exactly the excluded bytes.
    for run in first.iter().chain(second.iter()) {
        for attempt in &run.attempts {
            for section in &attempt.volatile {
                assert!(
                    ecc::daemon_arm::documented_volatile_rule(&section.name, &section.rationale),
                    "{}: recorded rule {} is not documented",
                    run.task_id,
                    section.name
                );
                assert!(!section.extract.is_empty());
            }
        }
    }
    let mut first_value = serde_json::to_value(&first).expect("first json");
    let mut second_value = serde_json::to_value(&second).expect("second json");
    // The recorded volatile evidence is excluded from this byte-identity
    // claim: its extracts are the scheduling-dependent bytes normalization
    // dropped from the HASH by design (the identity lives in
    // `prompt_blake3`, which stays under the comparison); the rule
    // vocabulary is checked above.
    strip_volatile_evidence(&mut first_value);
    strip_volatile_evidence(&mut second_value);
    if let Some(diff) = first_json_diff(&first_value, &second_value, String::new()) {
        panic!("driving the REAL runtime twice must be byte-identical; drifted at {diff}");
    }
    for run in &first {
        assert_eq!(run.prompt.arm, Arm::FullDaemonRuntime);
        assert!(
            run.prompt.contract_digest.is_some(),
            "{}: the runtime arm carries the compiled contract digest",
            run.task_id
        );
        assert!(
            run.prompt.intelligence.is_none(),
            "{}: the runtime arm records no rendered prompt-side brief",
            run.task_id
        );
        assert!(!run.attempts.is_empty());
        for attempt in &run.attempts {
            let evidence = attempt
                .runtime
                .as_ref()
                .expect("every runtime attempt carries runtime evidence");
            assert_eq!(attempt.provider, "frozen-runtime");
            assert_eq!(attempt.model, "frozen-plan");
            assert_eq!(attempt.spend_micro, 0, "no monetary claim on this lane");
            assert_eq!(attempt.cost_source, ecc::frozen::CostSource::Fixture);
            assert!(
                [
                    "verified_complete",
                    "unverified",
                    "failed_verification",
                    "blocked_verification",
                    "verification_pending",
                    "none"
                ]
                .contains(&evidence.completion_gate.as_str()),
                "{}: unknown gate label {}",
                run.task_id,
                evidence.completion_gate
            );
            // The runtime made at least one model call (its frozen script) and
            // the derived-check state is recorded honestly.
            assert!(evidence.provider_calls >= 1);
            assert!(
                evidence.review_calls <= evidence.provider_calls,
                "review calls are a subset of the turn's provider calls"
            );
        }
    }
}

#[test]
fn runtime_evidence_tampering_is_rejected_at_replay() {
    let scratch = tempfile::TempDir::new().expect("scratch");
    let generated = ecc::fixture::generate_fixture_capture(scratch.path()).expect("generate");
    let mut tampered = generated.clone();
    let runtime_run = tampered
        .runs
        .iter_mut()
        .find(|r| r.prompt.arm == Arm::FullDaemonRuntime)
        .expect("fixture carries runtime runs");
    let evidence = runtime_run.attempts[0]
        .runtime
        .as_mut()
        .expect("runtime evidence");
    evidence.completion_gate = "verified_complete".into();
    let tampered = tampered.finish().expect("re-seal");
    let err = replay_capture(&tampered, &corpus(), scratch.path()).expect_err("replay rejects");
    assert!(
        err.contains("runtime gate/review evidence drifted"),
        "unexpected: {err}"
    );
}

#[test]
fn tampered_runtime_prompt_hash_is_rejected_at_redrive() {
    let scratch = tempfile::TempDir::new().expect("scratch");
    let generated = ecc::fixture::generate_fixture_capture(scratch.path()).expect("generate");
    let mut tampered = generated.clone();
    let runtime_run = tampered
        .runs
        .iter_mut()
        .find(|r| r.prompt.arm == Arm::FullDaemonRuntime)
        .expect("fixture carries runtime runs");
    runtime_run.attempts[0].prompt_blake3 = "0".repeat(64);
    let tampered = tampered.finish().expect("re-seal");
    let err = replay_capture(&tampered, &corpus(), scratch.path()).expect_err("replay rejects");
    assert!(err.contains("runtime prompt drifted"), "unexpected: {err}");
}

/// The recorded volatile normalization evidence is itself gated: an
/// UNDOCUMENTED rule (a new normalization rule that never extended the
/// vocabulary) and an empty extract (an exclusion hiding no bytes) are both
/// refused at artifact validation, before any replay.
#[test]
fn tampered_volatile_evidence_is_rejected_at_artifact_validation() {
    let scratch = tempfile::TempDir::new().expect("scratch");
    let generated = ecc::fixture::generate_fixture_capture(scratch.path()).expect("generate");
    assert!(
        generated
            .runs
            .iter()
            .filter(|r| r.prompt.arm == Arm::FullDaemonRuntime)
            .flat_map(|r| r.attempts.iter())
            .any(|a| !a.volatile.is_empty()),
        "the fixture records its normalization exclusions"
    );

    let mut tampered = generated.clone();
    let runtime_run = tampered
        .runs
        .iter_mut()
        .find(|r| r.prompt.arm == Arm::FullDaemonRuntime)
        .expect("fixture carries runtime runs");
    let attempt = runtime_run
        .attempts
        .iter_mut()
        .find(|a| !a.volatile.is_empty())
        .expect("a recorded exclusion");
    attempt.volatile[0].name = "custom_rule".into();
    let tampered = tampered.finish().expect("re-seal");
    let err = replay_capture(&tampered, &corpus(), scratch.path()).expect_err("replay rejects");
    assert!(err.contains("UNDOCUMENTED"), "unexpected: {err}");

    let mut tampered = generated.clone();
    let runtime_run = tampered
        .runs
        .iter_mut()
        .find(|r| r.prompt.arm == Arm::FullDaemonRuntime)
        .expect("fixture carries runtime runs");
    let attempt = runtime_run
        .attempts
        .iter_mut()
        .find(|a| !a.volatile.is_empty())
        .expect("a recorded exclusion");
    attempt.volatile[0].extract = String::new();
    let tampered = tampered.finish().expect("re-seal");
    let err = replay_capture(&tampered, &corpus(), scratch.path()).expect_err("replay rejects");
    assert!(err.contains("extract is empty"), "unexpected: {err}");
}

fn corpus() -> ecc::coding::CodingCorpus {
    load_coding_corpus(&ecc::coding::coding_corpus_root()).expect("corpus")
}

/// Remove every attempt's recorded `volatile` normalization evidence from a
/// capture JSON value (and, for a capture object, the top-level `digest`,
/// which is computed over the full serialization INCLUDING those volatile
/// extracts; both sides are independently digest-validated by `finish()` /
/// `load_capture`). The extracts are capture-time scheduling-dependent bytes
/// (that is why normalization exists); identity comparisons over the
/// normalized hash stay intact, and the recorded vocabulary is checked
/// separately against the documented rules. Accepts both shapes used in this
/// suite: a `FrozenCapture` object (its `runs`) and a bare run array.
fn strip_volatile_evidence(value: &mut serde_json::Value) {
    if let serde_json::Value::Object(object) = value {
        object.remove("digest");
    }
    let runs: Option<&mut Vec<serde_json::Value>> = match value {
        serde_json::Value::Array(runs) => Some(runs),
        serde_json::Value::Object(object) => object.get_mut("runs").and_then(|v| v.as_array_mut()),
        _ => None,
    };
    let Some(runs) = runs else {
        return;
    };
    for run in runs {
        let Some(attempts) = run.get_mut("attempts").and_then(|v| v.as_array_mut()) else {
            continue;
        };
        for attempt in attempts {
            if let Some(object) = attempt.as_object_mut() {
                object.remove("volatile");
            }
        }
    }
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
