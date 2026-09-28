//! `runtime::verification_attribution_tests`: out-of-line tests.

use super::*;

use crate::runtime::fixtures_tests::*;
use crate::runtime::tests::*;
use crate::*;
use faktor_router::OutcomeView;

#[tokio::test]
async fn queue_survives_runner_gate_and_drains_sequentially() {
    // Multiple runners racing for one session: the gate lets only one
    // through, and queued prompts are delivered in FIFO order.
    let (deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::Text("a".into()),
            ScriptedResponse::End,
        ]),
        vec![],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let ra = runtime.submit(session, "one", &[]).unwrap();
    let rb = runtime.submit(session, "two", &[]).unwrap();
    let rc = runtime.submit(session, "three", &[]).unwrap();
    assert!(!ra.queued);
    assert!(rb.queued && rc.queued);
    // A is not driven yet; drive it, then let TWO racing runners drain.
    let oa = runtime.drive_receipt(&handle, ra, None).await.unwrap();
    assert_eq!(oa.final_state, AgentState::ReadyForNextTurn);
    let r1 = runtime.clone();
    let r2 = runtime.clone();
    let t1 = tokio::spawn(async move { r1.run_session_queue(session).await });
    let t2 = tokio::spawn(async move { r2.run_session_queue(session).await });
    let _ = t1.await;
    let _ = t2.await;
    assert_eq!(handle.queued_prompt_count().unwrap(), 0, "FIFO drain");
    assert_eq!(handle.state().unwrap(), AgentState::ReadyForNextTurn);
}

#[tokio::test]
async fn turn_verification_runs_required_check_and_passes() {
    // The model changed src/a.rs in a Rust repo: the engine must run
    // `cargo check` itself (never the model's discretion) and the turn
    // reports Pass with the recorded result.
    let calls: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let calls2 = calls.clone();
    let verifier = fake(move |cmd: &str| {
        calls2.lock().unwrap().push(cmd.to_string());
        Ok(())
    });
    let (deps, _dir, root) = verified_rust_env(
        vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/a.rs", "content": "x"}),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
        Some(verifier),
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = session_in_workspace(runtime.deps(), &root);
    let outcome = runtime
        .run_turn(session, "write src/a.rs", &[])
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_eq!(
        *calls.lock().unwrap(),
        vec!["cargo check".to_string()],
        "the derived required check runs exactly once"
    );
    assert_eq!(outcome.verification, vec![("rust_check".to_string(), true)]);
    assert_eq!(outcome.acceptance, Some(faktor_verify::Acceptance::Pass));
    assert_eq!(outcome.completion, Some(CompletionGate::VerifiedComplete));
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let facts = handle.memory_facts().unwrap();
    assert!(
        facts
            .iter()
            .all(|(k, key, _)| k != "verification" || key == "last"),
        "no per-check failure fact on Pass, only the last-run summary: {facts:?}"
    );
    assert!(
        facts
            .iter()
            .any(|(k, key, v)| k == "task_state" && key == "state" && v == "verified_complete"),
        "task_state fact must record VerifiedComplete: {facts:?}"
    );
    let last = facts
        .iter()
        .find(|(k, key, _)| k == "verification" && key == "last")
        .expect("verification last-run row must exist")
        .2
        .clone();
    let last: serde_json::Value = serde_json::from_str(&last).unwrap();
    assert_eq!(last["status"], "passed", "{last}");
    assert_eq!(
        last["checks"],
        serde_json::json!([{ "id": "rust_check", "passed": true }]),
        "{last}"
    );
    assert!(last["changed"][0] == "src/a.rs", "{last}");
}

/// P2: the only production `VerifiedComplete` producer refuses with a
/// typed gate while a requested completion step lacks a durable
/// Succeeded row, leaves the task row at Verifying and lands no attempt
/// record; recording the step success lets the same claim certify.
#[tokio::test]
async fn completion_contract_gate_refuses_verified_complete_until_step_succeeds() {
    use faktor_core::completion::{CompletionContract, CompletionStep, CompletionStepOutcome};
    let (deps, _dir) = deps(scripted_provider(vec![]), vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let handle = runtime
        .deps()
        .session
        .get_session(session)
        .unwrap()
        .unwrap();
    let task_id = handle.task_id().unwrap();
    let now = handle.now_ms();
    handle
        .create_task(faktor_session::Task {
            task_id,
            session_id: session,
            goal: "ship the PR".into(),
            acceptance_criteria: vec![],
            plan: vec![],
            attachments: Vec::new(),
            budget: faktor_session::TaskBudget::default(),
            state: TaskState::Pending,
            created_ms: now,
            updated_ms: now,
        })
        .unwrap();
    let contract_rev = handle.task_revision(task_id).unwrap();
    handle
        .set_completion_contract(
            task_id,
            contract_rev,
            CompletionContract {
                include_commit: false,
                include_push: true,
                include_pr: false,
            },
        )
        .unwrap();
    // Drive the row to Verifying through the legal machine edges.
    for transition in [
        TaskTransition::StartRunning,
        TaskTransition::RequestVerification,
        TaskTransition::StartVerification,
    ] {
        let rev = handle.task_revision(task_id).unwrap();
        handle
            .transition_task(task_id, rev, transition, None)
            .unwrap();
    }
    assert_eq!(
        handle.get_task(task_id).unwrap().unwrap().state,
        TaskState::Verifying
    );
    let proof = VerificationProof {
        checks: vec![],
        criteria: vec![],
        changed_files: vec![],
        review: None,
    };
    // The refusal names the unmet push step, BEFORE the row moves and
    // BEFORE any attempt record is created.
    let refusal = runtime
        .apply_gate_to_task_row(
            &handle,
            Some(CompletionGate::VerifiedComplete),
            Some(&proof),
        )
        .unwrap();
    match refusal {
        Some(CompletionGate::BlockedVerification { reasons }) => assert!(
            reasons.iter().any(|r| r.detail.contains("Push")),
            "the refusal must name the unmet Push step: {reasons:?}"
        ),
        other => panic!("expected a BlockedVerification refusal, got {other:?}"),
    }
    assert_eq!(
        handle.get_task(task_id).unwrap().unwrap().state,
        TaskState::Verifying,
        "the task stays Verifying"
    );
    assert!(
        handle
            .list_verification_records(task_id)
            .unwrap()
            .is_empty(),
        "an unmet contract must land no attempt record"
    );
    // Record the durable success: the same claim now certifies.
    handle
        .set_completion_step_status(
            task_id,
            CompletionStep::Push,
            CompletionStepOutcome::Succeeded,
            "pushed to origin/main",
        )
        .unwrap();
    let landed = runtime
        .apply_gate_to_task_row(
            &handle,
            Some(CompletionGate::VerifiedComplete),
            Some(&proof),
        )
        .unwrap();
    assert!(
        landed.is_none(),
        "the claim must land once the step succeeded: {landed:?}"
    );
    assert_eq!(
        handle.get_task(task_id).unwrap().unwrap().state,
        TaskState::VerifiedComplete
    );
}

#[tokio::test]
async fn turn_verification_failure_writes_durable_fact() {
    // A failing required check must NOT fail the turn, but the failure
    // lands as a durable memory fact (kind "verification", key = check
    // id, value "failed:<command>") so later turns know.
    let verifier = fake(|_cmd: &str| Err("type error".to_string()));
    let (deps, _dir, root) = verified_rust_env(
        vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/a.rs", "content": "x"}),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
        Some(verifier),
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = session_in_workspace(runtime.deps(), &root);
    let outcome = runtime
        .run_turn(session, "write src/a.rs", &[])
        .await
        .unwrap();
    assert_eq!(
        outcome.final_state,
        AgentState::ReadyForNextTurn,
        "a failing verification never fails the turn"
    );
    assert_eq!(
        outcome.verification,
        vec![("rust_check".to_string(), false)]
    );
    assert_eq!(outcome.acceptance, Some(faktor_verify::Acceptance::Fail));
    assert_eq!(
        outcome.completion,
        Some(CompletionGate::FailedVerification {
            reasons: vec![OutcomeReason::new(
                ReasonCode::CheckFailed,
                "required check 'rust_check' (cargo check) failed"
            )]
        }),
        "a failed required check must gate the turn as FailedVerification"
    );
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let facts = handle.memory_facts().unwrap();
    assert!(
        facts.iter().any(|(k, key, v)| k == "verification"
            && key == "rust_check"
            && v == "failed:cargo check"),
        "durable failure fact missing: {facts:?}"
    );
    assert!(
        facts
            .iter()
            .any(|(k, key, v)| k == "task_state" && key == "state" && v == "failed"),
        "{facts:?}"
    );
}

/// Static spec-coverage table for `docs/specs/completion_proof.md`
/// (normative): every section heading must map to a real code path.
/// Each probe touches the path it names, so drift (a section losing its
/// implementation) fails the test with the section's name in the panic.
#[tokio::test]
async fn completion_proof_spec_sections_map_to_code_paths() {
    const SPEC: &str = include_str!("../../../../docs/specs/completion_proof.md");
    let mut probed: Vec<&'static str> = Vec::new();
    macro_rules! spec_probe {
        ($name:expr, $body:block) => {{
            let outcome: Result<(), String> = (|| $body)();
            if let Err(e) = outcome {
                panic!(
                    "completion_proof spec section {:?} lost its code path: {e}",
                    $name
                );
            }
            probed.push($name);
        }};
    }

    // ---- evidence legs (probes below read these results) ----
    let (manager, session, _dir) = verified_shared_env();
    let (deps_pass, _dp) = verified_turn_deps(
        &manager,
        vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                input: serde_json::json!({
                    "path": "src/adder.rs",
                    "content": "pub fn adder(a: u32, b: u32) -> u32 {\n    let base: u32 = 41;\n    let step: u32 = 1;\n    base.saturating_add(a).saturating_mul(step).saturating_add(b)\n}\n",
                }),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
        fake(|cmd: &str| {
            assert_eq!(cmd, "cargo check");
            Ok(())
        }),
        0.65,
    );
    let runtime = AgentRuntime::new(deps_pass).unwrap();
    let pass_out = runtime
        .run_turn(session, "implement the adder", &[])
        .await
        .unwrap();
    drop(runtime);
    let handle = manager.get_session(session).unwrap().unwrap();
    let pass_facts = handle.memory_facts().unwrap();
    let pass_task = handle.list_tasks().unwrap().remove(0);
    let pass_criteria = criteria_fact(&handle);
    let pass_last: Option<serde_json::Value> = pass_facts
        .iter()
        .find(|(k, key, _)| k == "verification" && key == "last")
        .map(|(_, _, v)| serde_json::from_str(v).unwrap());
    drop(handle);
    drop(manager);
    assert_eq!(pass_out.completion, Some(CompletionGate::VerifiedComplete));

    // Failing leg: same change shape under a failing required check.
    let (manager2, session2, _d2) = verified_shared_env();
    let (deps_fail, _df) = verified_turn_deps(
        &manager2,
        vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                input: serde_json::json!({
                    "path": "src/other.rs",
                    "content": "pub fn other(a: u32) -> u32 {\n    let base: u32 = 41;\n    base.saturating_add(a).saturating_mul(2)\n}\n",
                }),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
        fake(|_cmd: &str| Err("type error".to_string())),
        0.65,
    );
    let runtime2 = AgentRuntime::new(deps_fail).unwrap();
    let fail_out = runtime2
        .run_turn(session2, "write other", &[])
        .await
        .unwrap();
    drop(runtime2);
    let handle2 = manager2.get_session(session2).unwrap().unwrap();
    let fail_facts = handle2.memory_facts().unwrap();
    drop(handle2);
    drop(manager2);

    // ---- 1. VerificationRecord ----
    spec_probe!("VerificationRecord", {
        let last = pass_last.as_ref().ok_or("verification/last row missing")?;
        if last["status"].as_str() != Some("passed") {
            return Err(format!("last-run status: {last}"));
        }
        let checks = last["checks"].as_array().ok_or("checks list missing")?;
        if checks
            .iter()
            .any(|c| c["passed"] != serde_json::json!(true))
        {
            return Err(format!("a check did not pass: {last}"));
        }
        let criteria = pass_criteria.as_deref().ok_or("criteria row missing")?;
        if !criteria.contains("required check: cargo check") {
            return Err(format!("criteria record: {criteria}"));
        }
        if pass_task.acceptance_criteria.is_empty() {
            return Err("typed task row carries no acceptance criteria".into());
        }
        if last["changed"].as_array().is_none_or(|c| c.is_empty()) {
            return Err("record does not list changed files".into());
        }
        if pass_task.state != TaskState::VerifiedComplete {
            return Err(format!("row state: {:?}", pass_task.state));
        }
        Ok(())
    });

    // ---- 2. Rule 1: Completed-without-a-record is ILLEGAL when an
    // objective mechanism exists ----
    spec_probe!("Rule 1", {
        if pass_out.acceptance != Some(faktor_verify::Acceptance::Pass) {
            return Err("pass leg was not Pass".into());
        }
        let has_record = pass_facts
            .iter()
            .any(|(k, key, _)| k == "verification" && key == "last");
        if !has_record {
            return Err("Pass acceptance without a durable verification record".into());
        }
        let state_fact = pass_facts
            .iter()
            .find(|(k, key, _)| k == "task_state" && key == "state")
            .map(|(_, _, v)| v.clone());
        if state_fact.as_deref() != Some("verified_complete") {
            return Err(format!("task_state fact: {state_fact:?}"));
        }
        Ok(())
    });

    // ---- 3. Rule 2: a required check that FAILED can never yield Pass
    spec_probe!("Rule 2", {
        let profile = faktor_verify::derive::detect_project_profile(
            std::path::Path::new("/nonexistent-verify-root"),
            &["Cargo.toml".to_string(), "src/a.rs".to_string()],
        );
        let specs =
            faktor_verify::derive::derive_checks(&profile, &[std::path::PathBuf::from("src/a.rs")])
                .map_err(|e| e.to_string())?;
        let checks: Vec<faktor_verify::Check> = specs.iter().map(legacy_mirror_of_spec).collect();
        let results = vec![("rust_check".to_string(), false)];
        if faktor_verify::acceptance(&checks, &results) != faktor_verify::Acceptance::Fail {
            return Err("failed required check did not yield Fail".into());
        }
        if !matches!(
            &fail_out.completion,
            Some(CompletionGate::FailedVerification { .. })
        ) {
            return Err("failed check did not gate FailedVerification".into());
        }
        Ok(())
    });

    // ---- 4. Rule 3: test weakening/deletion without justification
    // fails review ----
    spec_probe!("Rule 3", {
        let hollow =
                "describe(\"x\", () => {\n    it(\"adds\", () => {\n        const got = calc.add(1, 2);\n    });\n});\n";
        let evidence = review_signals(
            &["tests/calc_spec.js".to_string()],
            &[("tests/calc_spec.js".to_string(), hollow.to_string())],
        );
        let verdict = review_verdict(&evidence, &[]);
        if verdict["verdict"] != serde_json::json!("block") {
            return Err(format!("weakened test did not fail review: {verdict}"));
        }
        Ok(())
    });

    // ---- 5. Rule 4: "done" must be backed by repository state; an
    // evidence check runs before acceptance ----
    spec_probe!("Rule 4", {
        let review_pass = pass_out.review.as_ref().ok_or("review missing")?;
        let files = review_pass
            .get("evidence")
            .and_then(|e| e.get("files"))
            .and_then(|f| f.as_array())
            .ok_or("review evidence missing")?;
        let adder = files
            .iter()
            .find(|f| f["path"] == serde_json::json!("src/adder.rs"))
            .ok_or("changed file missing from review evidence")?;
        if adder["unread"] == serde_json::json!(true) || adder["head_chars"] == serde_json::json!(0)
        {
            return Err(format!(
                "evidence did not read the repository state: {adder}"
            ));
        }
        if pass_out.completion != Some(CompletionGate::VerifiedComplete) {
            return Err("clean repo-backed change was not verified complete".into());
        }
        Ok(())
    });

    // ---- 6. Implementation status: crates/verify ----
    spec_probe!("Implementation status: `crates/verify` (faktor-verify)", {
        let files = vec!["Cargo.toml".to_string(), "src/lib.rs".to_string()];
        let profile = faktor_verify::derive::detect_project_profile(
            std::path::Path::new("/nonexistent-verify-root"),
            &files,
        );
        if !profile.components.iter().any(|component| {
            component
                .languages
                .contains(&faktor_verify::derive::LanguageFamily::Rust)
                && component
                    .build_systems
                    .contains(&faktor_verify::derive::BuildSystem::Cargo)
        }) {
            return Err("project-profile detection regressed".into());
        }
        if faktor_verify::MAX_CHECKS != 256 {
            return Err("daemon runner cap (<=256 checks) drifted".into());
        }
        let specs = faktor_verify::derive::derive_checks(
            &profile,
            &[
                std::path::PathBuf::from("src/a.rs"),
                std::path::PathBuf::from("tests/b.rs"),
                std::path::PathBuf::from("src/c.rs"),
                std::path::PathBuf::from("tests/d.rs"),
            ],
        )
        .map_err(|e| e.to_string())?;
        if specs.len() > faktor_verify::MAX_CHECKS {
            return Err("derived checks exceeded the bounded runner cap".into());
        }
        let hostile = faktor_verify::derive::derive_checks(
            &profile,
            &[std::path::PathBuf::from("tests/$evil.rs")],
        )
        .map_err(|e| e.to_string())?;
        if hostile.iter().any(|spec| {
            spec.program.to_string_lossy().contains('$')
                || spec
                    .args
                    .iter()
                    .any(|arg| arg.to_string_lossy().contains('$'))
        }) {
            return Err(format!("hostile filter was interpolated: {hostile:?}"));
        }
        // Multi-component contract: a mixed repo must never certify a
        // firmware change through the root Rust family alone — the
        // component-specific family owns it. Content-deciding manifests
        // are probed through an ADMITTED, anchored root (candidate
        // generation): the manifests must exist on disk, or the probes
        // refuse typed and derivation fails closed.
        let mixed_root = tempdir().unwrap();
        std::fs::create_dir_all(mixed_root.path().join("src")).unwrap();
        std::fs::create_dir_all(mixed_root.path().join("firmware").join("src")).unwrap();
        std::fs::write(mixed_root.path().join("Cargo.toml"), "").unwrap();
        std::fs::write(mixed_root.path().join("src").join("a.rs"), "").unwrap();
        std::fs::write(
            mixed_root.path().join("firmware").join("CMakeLists.txt"),
            "",
        )
        .unwrap();
        std::fs::write(
            mixed_root.path().join("firmware").join("platformio.ini"),
            "",
        )
        .unwrap();
        std::fs::write(
            mixed_root
                .path()
                .join("firmware")
                .join("src")
                .join("main.cpp"),
            "",
        )
        .unwrap();
        let mixed = vec![
            "Cargo.toml".to_string(),
            "src/a.rs".to_string(),
            "firmware/CMakeLists.txt".to_string(),
            "firmware/platformio.ini".to_string(),
            "firmware/src/main.cpp".to_string(),
        ];
        let profile = faktor_verify::derive::detect_project_profile(mixed_root.path(), &mixed);
        let derived = faktor_verify::derive::derive_checks(
            &profile,
            &[std::path::PathBuf::from("firmware/src/main.cpp")],
        )
        .map_err(|e| e.to_string())?;
        let derived_ids: Vec<&str> = derived.iter().map(|s| s.id.as_str()).collect();
        if !derived_ids.iter().any(|id| id.starts_with("firmware:")) {
            return Err(format!(
                "mixed-repo firmware change lost its component family: {derived_ids:?}"
            ));
        }
        if derived_ids.contains(&"rust_check") {
            return Err(format!(
                "mixed-repo firmware change certified through Rust only: {derived_ids:?}"
            ));
        }
        // The same mixed profile still certifies a root Rust change
        // through the root component's own family.
        let rust =
            faktor_verify::derive::derive_checks(&profile, &[std::path::PathBuf::from("src/a.rs")])
                .map_err(|e| e.to_string())?;
        if !rust
            .iter()
            .any(|spec| spec.id == "rust_check" && spec.cwd_rel.as_os_str().is_empty())
        {
            return Err(format!(
                "mixed-repo Rust change lost its root family: {:?}",
                rust.iter().map(|s| s.id.as_str()).collect::<Vec<_>>()
            ));
        }
        Ok(())
    });

    // ---- 7. Implementation status: agent hook ----
    spec_probe!("Implementation status: agent hook (`faktor-agent`)", {
        if pass_out.verification != vec![("rust_check".to_string(), true)] {
            return Err(format!("verification results: {:?}", pass_out.verification));
        }
        if pass_out.acceptance != Some(faktor_verify::Acceptance::Pass) {
            return Err("acceptance missing".into());
        }
        if pass_out.review.is_none() {
            return Err("review missing at a genuine mutating end".into());
        }
        let failed_fact = fail_facts.iter().any(|(k, key, v)| {
            k == "verification" && key == "rust_check" && v.starts_with("failed:")
        });
        if !failed_fact {
            return Err("failed checks did not write durable verification facts".into());
        }
        if !matches!(
            &fail_out.completion,
            Some(CompletionGate::FailedVerification { .. })
        ) {
            return Err("failure did not gate FailedVerification".into());
        }
        Ok(())
    });

    // ---- 8. Implementation status: daemon wiring (faktor-cli) ----
    spec_probe!("Implementation status: daemon wiring (`faktor-cli`)", {
        // The typed async verifier lives in crates/cli + the agent's
        // VerificationService (outside this probe's read scope); this
        // crate locks the CONTRACT the daemon implements: the
        // verification site is policy-budgeted (per-category budgets via
        // `budget_for` — the legacy 30 s/check + 10 s wall caps are GONE
        // from the runtime verification path, locked by the negative
        // anchors below), every required check executes as a typed spec
        // through the service, and the derived-check set is capped at
        // MAX_CHECKS with typed refusals (never first-match truncation).
        // The spec's status text still names the
        // supervisor-backed runner of the docs' original wiring.
        // The runtime is decomposed across `src/runtime/` (audit 9/23/24):
        // read the whole production tree so the anchors keep their reach.
        fn read_runtime_tree(root: &std::path::Path) -> Result<String, String> {
            let mut out = String::new();
            let mut stack = vec![root.to_path_buf()];
            while let Some(dir) = stack.pop() {
                for entry in std::fs::read_dir(&dir).map_err(|e| e.to_string())? {
                    let path = entry.map_err(|e| e.to_string())?.path();
                    if path.is_dir() {
                        stack.push(path);
                    } else if path.extension().and_then(|e| e.to_str()) == Some("rs")
                        && !path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                            n.ends_with("_tests.rs") || n == "tests.rs" || n == "fixtures_tests.rs"
                        })
                    {
                        out.push_str(&std::fs::read_to_string(&path).map_err(|e| e.to_string())?);
                        out.push('\n');
                    }
                }
            }
            Ok(out)
        }
        let src = read_runtime_tree(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("src")
                .join("runtime"),
        )?;
        for anchor in [
            "service.budget_for(&spec)",
            "BudgetDecision::RunAsTaskOwnedOperation",
            "CompletionGate::VerificationPending",
            "settle_verification_jobs",
            "begin_verification_attempt",
            "service.execute(&spec, &vctx).await",
        ] {
            if !src.contains(anchor) {
                return Err(format!(
                    "typed-verifier contract anchor {anchor:?} missing from the runtime"
                ));
            }
        }
        for gone in [
            format!("const PER_CHECK: Duration = Duration::from_secs({})", 30),
            format!("const WALL_CAP: Duration = Duration::from_secs({})", 10),
            format!("{}_blocking", "spawn"),
            // Audit P0-5/26: the inline-override fallback is GONE — a
            // background-policy check on the real executor is a durable
            // job; only scripted seams execute it inline, with no note.
            // The needles are split so this probe's own text cannot
            // satisfy them (the deleted symbols are also compile-locked).
            format!("{}{}", "INLINE_OVERRIDE", "_NOTE"),
            format!("{}override_budget", "inline_"),
        ] {
            if src.contains(&gone) {
                return Err(format!(
                        "legacy cap anchor {gone:?} must NOT be present in the runtime verification path"
                    ));
            }
        }
        if !SPEC.contains("supervisor-backed runner") {
            return Err("spec status block drifted".into());
        }
        Ok(())
    });

    assert_eq!(probed.len(), 8, "all completion_proof sections mapped");
    let mut seen = std::collections::HashSet::new();
    for p in &probed {
        assert!(seen.insert(*p), "duplicate probe for {p}");
    }
}

#[tokio::test]
async fn turn_verification_with_test_change_runs_both_required_checks_in_order() {
    // A change under tests/ derives TWO required checks (cargo check +
    // cargo test <stem>); both run, in deterministic order.
    let calls: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let calls2 = calls.clone();
    let verifier = fake(move |cmd: &str| {
        calls2.lock().unwrap().push(cmd.to_string());
        Ok(())
    });
    let (deps, _dir, root) = verified_rust_env(
        vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "tests/foo.rs", "content": "x"}),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
        Some(verifier),
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = session_in_workspace(runtime.deps(), &root);
    let outcome = runtime
        .run_turn(session, "write a test", &[])
        .await
        .unwrap();
    assert_eq!(
        *calls.lock().unwrap(),
        vec!["cargo check".to_string(), "cargo test foo".to_string()],
        "both required checks run in deterministic order"
    );
    assert_eq!(outcome.acceptance, Some(faktor_verify::Acceptance::Pass));
    assert_eq!(
        outcome.verification,
        vec![
            ("rust_check".to_string(), true),
            ("rust_test:foo".to_string(), true),
        ]
    );
}

#[tokio::test]
async fn turn_verification_absent_verifier_leaves_defaults() {
    // No verifier wired: the raw fields stay empty/None even though the
    // turn changed files in a resolvable Rust workspace — but the change
    // is classified Unverified (NEVER silently 'complete') and the
    // durable rows record it.
    let (deps, _dir, root) = verified_rust_env(
        vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/a.rs", "content": "x"}),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
        None,
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = session_in_workspace(runtime.deps(), &root);
    let outcome = runtime
        .run_turn(session, "write src/a.rs", &[])
        .await
        .unwrap();
    assert!(outcome.verification.is_empty());
    assert_eq!(outcome.acceptance, None);
    assert_eq!(
        outcome.completion,
        Some(CompletionGate::Unverified),
        "a mutating turn without a verifier is Unverified, never complete"
    );
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let facts = handle.memory_facts().unwrap();
    assert!(
        facts
            .iter()
            .any(|(k, key, v)| k == "task_state" && key == "state" && v == "needs_verification"),
        "task_state must be NeedsVerification without a verifier: {facts:?}"
    );
    let last = facts
        .iter()
        .find(|(k, key, _)| k == "verification" && key == "last")
        .expect("the last-run row must exist on Unverified")
        .2
        .clone();
    let last: serde_json::Value = serde_json::from_str(&last).unwrap();
    assert_eq!(last["status"], "unavailable", "{last}");
    assert!(last["changed"][0] == "src/a.rs", "{last}");
}

#[tokio::test]
async fn turn_verification_skipped_without_changed_files() {
    // A text-only turn must never invoke the verifier (nothing this
    // turn changed → nothing to verify). The panicking closure proves
    // it is not called.
    let verifier = fake(|_cmd: &str| panic!("verifier must not run without changed files"));
    let (deps, _dir, root) = verified_rust_env(
        vec![ScriptedResponse::Text("ok".into()), ScriptedResponse::End],
        Some(verifier),
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = session_in_workspace(runtime.deps(), &root);
    let outcome = runtime.run_turn(session, "just talk", &[]).await.unwrap();
    assert!(outcome.verification.is_empty());
    assert_eq!(outcome.acceptance, None);
    assert_eq!(outcome.review, None, "no changed files → no review either");
    assert_eq!(
        outcome.completion, None,
        "a text-only turn changes nothing: no completion claim at all"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn verification_runs_in_the_session_root_never_the_daemon_cwd() {
    // Adversarial (P0-9/10 cwd lineage): the daemon's current directory
    // is chdir'ed to a DECOY before the drive; the genuine-end
    // verification must execute the derived check inside the session's
    // DURABLE workspace root. The derived Gradle wrapper check (typed
    // builder derivation, wave 17) records its real `pwd` into a marker
    // file: the marker must exist under the session root, must be
    // ABSENT from the decoy, and the check's captured output must name
    // the session root — verification can never verify the wrong tree.
    let dir = tempdir().unwrap();
    let root = dir.path().join("ws");
    std::fs::create_dir_all(root.join("src/main/java")).unwrap();
    std::fs::write(root.join("build.gradle"), "task classes {}\n").unwrap();
    std::fs::write(
            root.join("src/main/java/A.java"),
            "class A {\n    int value() {\n        int base = 40;\n        int step = 2;\n        return base + step;\n    }\n    int doubled() {\n        int base = 40;\n        int step = 2;\n        return (base + step) * 2;\n    }\n}\n",
        )
        .unwrap();
    std::fs::write(
        root.join("gradlew"),
        "#!/bin/sh\npwd | tee verify-cwd-marker.txt\nexit 0\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root.join("gradlew"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }
    let marker = root.join("verify-cwd-marker.txt");
    let decoy = tempdir().unwrap();
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let ws = manager.create_workspace(root.to_str().unwrap()).unwrap();
    let session = manager
        .create_session(ws, "gradle gating", "fake", "m")
        .unwrap()
        .id();
    let (mut deps, _d) = deps_sharing_session(
        manager.clone(),
        Arc::new(scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                input: serde_json::json!({
                    "path": "src/main/java/A.java",
                    "content": "class A {\n    int value() {\n        int base = 40;\n        int step = 2;\n        return base + step;\n    }\n    int doubled() {\n        int base = 40;\n        int step = 2;\n        return (base + step) * 2;\n    }\n}\n",
                }),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ])),
        vec![real_write_tool()],
    );
    deps.verification = crate::VerificationService::new(
        Arc::new(
            faktor_verify::exec::AsyncCheckExecutor::try_shared().expect("standalone supervisor"),
        ),
        faktor_verify::exec::VerificationPolicy::default(),
    );
    deps.compact_at_usage = 0.65;
    // The daemon cwd points at the DECOY while the drive + verification
    // run (restored afterwards; nothing else in this crate relies on a
    // process cwd — sessions and workspaces ride durable absolute roots).
    let original_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(decoy.path()).unwrap();
    let outcome = {
        let runtime = AgentRuntime::new(deps).unwrap();
        runtime
            .run_turn(session, "write A.java", &[])
            .await
            .unwrap()
    };
    std::env::set_current_dir(&original_cwd).unwrap();
    assert_eq!(outcome.acceptance, Some(faktor_verify::Acceptance::Pass));
    assert_eq!(outcome.completion, Some(CompletionGate::VerifiedComplete));
    assert!(
        outcome
            .verification
            .iter()
            .any(|(id, ok)| id == "gradle_classes" && *ok),
        "{:?}",
        outcome.verification
    );
    let marker_text = std::fs::read_to_string(&marker)
        .expect("the derived gradlew check must run inside the session root and write its marker")
        .trim()
        .to_string();
    assert!(
        !marker_text.is_empty() && marker_text.ends_with("ws"),
        "the marker must carry the session-root path, got {marker_text:?}"
    );
    assert!(
        std::fs::metadata(decoy.path().join("verify-cwd-marker.txt")).is_err(),
        "the decoy directory must NOT contain the check's marker"
    );
    let h = manager.get_session(session).unwrap().unwrap();
    let records = h.list_verification_records(h.task_id().unwrap()).unwrap();
    assert_eq!(
        records.len(),
        1,
        "one passing record for the verified attempt"
    );
    let gradle_row = records[0]
        .checks
        .iter()
        .find(|c| c.check == "gradle_classes")
        .expect("the record carries the executed gradle_classes row");
    assert_eq!(gradle_row.program, "./gradlew");
    assert_eq!(gradle_row.args, vec!["classes".to_string()]);
    assert_eq!(gradle_row.status, VerificationStatus::Passed);
    assert_eq!(gradle_row.exit, Some(0));
    let summary = gradle_row.summary.as_deref().unwrap_or_default();
    assert!(
            summary.contains(&marker_text),
            "the check's captured output must name the session root it ran in: {summary:?} vs {marker_text:?}"
        );
    assert!(
        gradle_row
            .finished_ms
            .is_some_and(|f| f >= gradle_row.started_ms),
        "record timestamps are ordered: {gradle_row:?}"
    );
}

#[tokio::test]
async fn shadowed_session_verification_executes_inside_the_shadow_root() {
    // (c) P0-48 root re-pointing at the verification site: a session
    // carrying a LIVE durable shadow row (the exact state a shadowed
    // TaskExecutor drive holds) verifies against the SHADOW root. The
    // derived make check records its real `pwd` into a marker file: the
    // marker must exist only in the shadow (with the shadow's path), the
    // USER checkout must stay byte-identical through the whole verified
    // drive, and the durable record must name the shadow root.
    if std::process::Command::new("make")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        // make present: run the full adversarial assertion below.
    } else {
        eprintln!("skipping shadowed verification pwd-marker: no make on this host");
        return;
    }
    let dir = tempdir().unwrap();
    let user_root = dir.path().join("ws");
    std::fs::create_dir_all(&user_root).unwrap();
    std::fs::write(
        user_root.join("Makefile"),
        "all:\n\t/bin/pwd > verify-cwd-marker.txt\n\tcat verify-cwd-marker.txt\n",
    )
    .unwrap();
    std::fs::write(
            user_root.join("app.rs"),
            b"pub fn value() -> u64 {\n    let base_amount: u64 = 10;\n    let increment: u64 = 32;\n    base_amount.saturating_add(increment)\n}\n",
        )
        .unwrap();
    // The daemon-owned shadow of begin_shadow: a bounded copy of the
    // checkout plus the durable Active row.
    let shadow_root = dir.path().join("shadows").join("1").join("sh-1");
    std::fs::create_dir_all(&shadow_root).unwrap();
    std::fs::write(
        shadow_root.join("Makefile"),
        "all:\n\t/bin/pwd > verify-cwd-marker.txt\n\tcat verify-cwd-marker.txt\n",
    )
    .unwrap();
    std::fs::write(
            shadow_root.join("app.rs"),
            b"pub fn value() -> u64 {\n    let base_amount: u64 = 10;\n    let increment: u64 = 32;\n    base_amount.saturating_add(increment)\n}\n",
        )
        .unwrap();
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let ws = manager
        .create_workspace(user_root.to_str().unwrap())
        .unwrap();
    let session = manager
        .create_session(ws, "shadowed verify", "fake", "m")
        .unwrap()
        .id();
    manager
        .put_shadow_row(
            session,
            &faktor_session::ShadowRow {
                session_id: session.raw(),
                shadow_id: "sh-1".into(),
                base_root: user_root.to_str().unwrap().into(),
                root: shadow_root.to_str().unwrap().into(),
                state: faktor_session::ShadowRowState::Active,
                base_entries: 2,
                base_bytes: 100,
                created_ms: 1,
            },
        )
        .unwrap();
    assert_eq!(
        manager.active_root(session).unwrap(),
        Some(shadow_root.clone()),
        "the live shadow re-points the session"
    );
    let (mut deps, _d) = deps_sharing_session(
        manager.clone(),
        Arc::new(scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                input: serde_json::json!({
                    "path": "app.rs",
                    "content": "pub fn value() -> u64 {\n    let base_amount: u64 = 11;\n    let increment: u64 = 31;\n    base_amount.saturating_add(increment)\n}\n",
                }),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ])),
        vec![real_write_tool()],
    );
    deps.verification = crate::VerificationService::new(
        Arc::new(
            faktor_verify::exec::AsyncCheckExecutor::try_shared().expect("standalone supervisor"),
        ),
        faktor_verify::exec::VerificationPolicy::default(),
    );
    let outcome = {
        let runtime = AgentRuntime::new(deps).unwrap();
        runtime
            .run_turn(session, "write app.rs", &[])
            .await
            .unwrap()
    };
    assert_eq!(outcome.acceptance, Some(faktor_verify::Acceptance::Pass));
    assert_eq!(outcome.completion, Some(CompletionGate::VerifiedComplete));
    assert!(
        outcome
            .verification
            .iter()
            .any(|(id, ok)| id == "make_build" && *ok),
        "{:?}",
        outcome.verification
    );
    // The drive's write landed in the SHADOW; the user checkout is
    // byte-identical.
    assert_eq!(
            std::fs::read(user_root.join("app.rs")).unwrap(),
            b"pub fn value() -> u64 {\n    let base_amount: u64 = 10;\n    let increment: u64 = 32;\n    base_amount.saturating_add(increment)\n}\n",
            "user checkout untouched through the verified shadowed drive"
        );
    assert_eq!(
            std::fs::read(shadow_root.join("app.rs")).unwrap(),
            b"pub fn value() -> u64 {\n    let base_amount: u64 = 11;\n    let increment: u64 = 31;\n    base_amount.saturating_add(increment)\n}\n",
            "the write landed in the shadow"
        );
    // The verification check ran with the SHADOW root as its cwd: its
    // marker exists in the shadow only, carrying the shadow path.
    let marker = std::fs::read_to_string(shadow_root.join("verify-cwd-marker.txt"))
        .expect("the derived check must run inside the shadow root")
        .trim()
        .to_string();
    assert!(
        marker.ends_with("/sh-1"),
        "the marker must carry the shadow-root path, got {marker:?}"
    );
    assert!(
        std::fs::metadata(user_root.join("verify-cwd-marker.txt")).is_err(),
        "the user checkout must never contain the check's marker"
    );
    let h = manager.get_session(session).unwrap().unwrap();
    let records = h.list_verification_records(h.task_id().unwrap()).unwrap();
    assert_eq!(records.len(), 1, "one passing record");
    let make_row = records[0]
        .checks
        .iter()
        .find(|c| c.check == "make_build")
        .expect("the make_build row is recorded");
    assert_eq!(make_row.program, "make");
    assert_eq!(make_row.status, VerificationStatus::Passed);
    assert_eq!(make_row.exit, Some(0));
    let summary = make_row.summary.as_deref().unwrap_or_default();
    assert!(
        summary.contains("sh-1") && summary.contains(&marker),
        "the recorded summary must name the shadow root it ran in: {summary:?}"
    );
}

#[tokio::test]
async fn completion_gate_verified_complete_with_facts_on_every_path() {
    // (a) adversarial twin of the pass test above: a mutating Rust turn
    // with a passing `cargo check` runner must be VerifiedComplete with
    // acceptance Pass AND the durable rows written (task_state +
    // verification last-run summary + once-only criteria row).
    let (deps, _dir, root) = verified_rust_env(
        vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/a.rs", "content": "x"}),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
        Some(fake(|cmd: &str| {
            assert_eq!(cmd, "cargo check");
            Ok(())
        })),
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = session_in_workspace(runtime.deps(), &root);
    let outcome = runtime
        .run_turn(session, "write src/a.rs", &[])
        .await
        .unwrap();
    assert_eq!(outcome.acceptance, Some(faktor_verify::Acceptance::Pass));
    assert_eq!(outcome.completion, Some(CompletionGate::VerifiedComplete));
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let facts = handle.memory_facts().unwrap();
    assert!(
        facts
            .iter()
            .any(|(k, key, v)| k == "task_state" && key == "state" && v == "verified_complete"),
        "task_state row missing on VerifiedComplete: {facts:?}"
    );
    let last = facts
        .iter()
        .find(|(k, key, _)| k == "verification" && key == "last")
        .expect("verification last-run row missing")
        .2
        .clone();
    let last: serde_json::Value = serde_json::from_str(&last).unwrap();
    assert_eq!(last["status"], "passed", "{last}");
    assert!(last["changed"][0] == "src/a.rs", "{last}");
    let criteria = facts
        .iter()
        .find(|(k, key, _)| k == "criteria" && key == "0")
        .expect("criteria row must seed with the derived check")
        .2
        .clone();
    assert!(
        criteria.contains("required check: cargo check"),
        "{criteria}"
    );
    assert!(
        !criteria.contains("goal: "),
        "the goal is not an automatic proof obligation: {criteria}"
    );
}

#[tokio::test]
async fn failed_verification_then_fixed_turn_verifies_complete() {
    // (b) adversarial: a required check whose runner returns Err gates
    // the turn FailedVerification with reasons naming the check AND
    // acceptance Fail — while the session STAYS ReadyForNextTurn (still
    // usable). The next turn that fixes the file yields VerifiedComplete.
    let (manager, session, _dir) = verified_shared_env();
    let (turn1_deps, _d1) = verified_turn_deps(
        &manager,
        vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                input: serde_json::json!({
                    "path": "src/broken.rs",
                    "content": "pub fn broken() -> u32 { 1 }\n",
                }),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
        fake(|_cmd: &str| Err("type error".to_string())),
        0.65,
    );
    let runtime1 = AgentRuntime::new(turn1_deps).unwrap();
    let o1 = runtime1
        .run_turn(session, "write broken.rs", &[])
        .await
        .unwrap();
    assert_eq!(
        o1.final_state,
        AgentState::ReadyForNextTurn,
        "a failed verification must NOT fail the turn: the session stays ready"
    );
    assert_eq!(o1.acceptance, Some(faktor_verify::Acceptance::Fail));
    match o1.completion {
        Some(CompletionGate::FailedVerification { reasons }) => {
            assert!(
                reasons.iter().any(|r| {
                    r.code == ReasonCode::CheckFailed
                        && r.detail.contains("rust_check")
                        && r.detail.contains("cargo check")
                }),
                "reasons must name the failed check: {reasons:?}"
            );
        }
        other => panic!("expected FailedVerification, got {other:?}"),
    }
    let handle1 = manager.get_session(session).unwrap().unwrap();
    let facts1 = handle1.memory_facts().unwrap();
    assert!(
        facts1
            .iter()
            .any(|(k, key, v)| k == "task_state" && key == "state" && v == "failed"),
        "task_state must be Failed: {facts1:?}"
    );
    // The session is still usable: the next logical turn fixes the file
    // under a working verifier → VerifiedComplete.
    let (turn2_deps, _d2) = verified_turn_deps(
        &manager,
        vec![
            ScriptedResponse::ToolCall {
                id: "c2".into(),
                name: "write_file".into(),
                input: serde_json::json!({
                    "path": "src/fixed.rs",
                    "content": "pub fn fixed() -> u32 {\n    let base: u32 = 41;\n    let step: u32 = 1;\n    base.saturating_add(step).saturating_mul(2).saturating_add(1)\n}\n",
                }),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
        fake_ok(),
        0.65,
    );
    let runtime2 = AgentRuntime::new(turn2_deps).unwrap();
    let o2 = runtime2
        .run_turn(session, "write fixed.rs", &[])
        .await
        .unwrap();
    assert_eq!(o2.final_state, AgentState::ReadyForNextTurn);
    assert_eq!(o2.acceptance, Some(faktor_verify::Acceptance::Pass));
    assert_eq!(o2.completion, Some(CompletionGate::VerifiedComplete));
    let handle2 = manager.get_session(session).unwrap().unwrap();
    let facts2 = handle2.memory_facts().unwrap();
    assert!(
        facts2
            .iter()
            .any(|(k, key, v)| k == "task_state" && key == "state" && v == "verified_complete"),
        "the fixed turn must flip task_state to VerifiedComplete: {facts2:?}"
    );
    let last2: serde_json::Value = serde_json::from_str(
        &facts2
            .iter()
            .find(|(k, key, _)| k == "verification" && key == "last")
            .expect("last-run row")
            .2,
    )
    .unwrap();
    assert_eq!(last2["status"], "passed", "{last2}");
    assert_eq!(last2["checks"][0]["passed"], true, "{last2}");
}

#[tokio::test]
async fn task_row_reaches_verified_complete_and_mirrors_gate_rows() {
    // A VerifiedComplete genuine end must upsert the durable task row to
    // the same state the gate facts carry, exactly once (one row per
    // session), with the criteria seeded from goal + derived checks.
    let (manager, session, _dir) = verified_shared_env();
    let (turn_deps, _d) = verified_turn_deps(
        &manager,
        vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/a.rs", "content": "pub fn a() -> u32 {\n    let base: u32 = 10;\n    let step: u32 = 41;\n    base.saturating_add(step).saturating_add(1)\n}\n"}),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
        fake_ok(),
        0.65,
    );
    let runtime = AgentRuntime::new(turn_deps).unwrap();
    let outcome = runtime
        .run_turn(session, "write src/a.rs", &[])
        .await
        .unwrap();
    assert_eq!(outcome.completion, Some(CompletionGate::VerifiedComplete));
    let handle = manager.get_session(session).unwrap().unwrap();
    let tasks = handle.list_tasks().unwrap();
    assert_eq!(tasks.len(), 1, "one durable task row per session");
    let t = &tasks[0];
    assert_eq!(t.state, TaskState::VerifiedComplete);
    assert_eq!(t.goal, "gating task", "goal seeded from the session goal");
    assert!(
        t.acceptance_criteria
            .iter()
            .any(|c| c.contains("cargo check")),
        "criteria seeded from the project-derived checks: {:?}",
        t.acceptance_criteria
    );
    let criteria_text = criteria_canonical_text(&t.acceptance_criteria);
    assert!(
            criteria_text.contains("required check: cargo check")
                && !criteria_text.contains("goal: gating task"),
            "the canonical typed criteria row carries the derived check entries (never an automatic goal obligation): {criteria_text}"
        );
    assert_eq!(
        criteria_fact(&handle).as_deref(),
        Some(criteria_text.as_str())
    );
    assert!(
        t.updated_ms >= t.created_ms,
        "the gate update bumps updated_ms over the drive-start creation"
    );
    assert_eq!(t.plan.len(), 1, "the durable plan holds the first step");
    assert!(
        t.plan[0].contains("src/a.rs"),
        "plan step carries the real path: {:?}",
        t.plan
    );
    let first_created = t.created_ms;
    let first_goal = t.goal.clone();
    let first_step = t.plan[0].clone();
    let first_updated = t.updated_ms;
    // A second VerifiedComplete turn on the SAME session: the machine
    // (audit P0-7) froze the terminal VerifiedComplete row — its content
    // is never rewritten and its state never moves again (update_task
    // refuses TerminalTask). The turn still records its OWN attempt as a
    // fresh durable Passed record, and the row keeps certifying the
    // first completion byte-identically.
    let (turn2_deps, _d2) = verified_turn_deps(
        &manager,
        vec![
            ScriptedResponse::ToolCall {
                id: "c2".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/b.rs", "content": "pub fn b() -> u32 {\n    let base: u32 = 20;\n    let step: u32 = 22;\n    base.saturating_add(step).saturating_sub(2)\n}\n"}),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
        fake_ok(),
        0.65,
    );
    let runtime2 = AgentRuntime::new(turn2_deps).unwrap();
    let o2 = runtime2
        .run_turn(session, "write src/b.rs", &[])
        .await
        .unwrap();
    assert_eq!(o2.completion, Some(CompletionGate::VerifiedComplete));
    let handle = manager.get_session(session).unwrap().unwrap();
    let tasks = handle.list_tasks().unwrap();
    assert_eq!(tasks.len(), 1, "upsert replaces, never duplicates");
    assert_eq!(tasks[0].state, TaskState::VerifiedComplete);
    assert_eq!(tasks[0].created_ms, first_created);
    assert_eq!(
        tasks[0].goal, first_goal,
        "the goal stays stable across gates"
    );
    assert_eq!(
            tasks[0].plan.len(),
            1,
            "the terminal row's plan is FROZEN at the certified steps: the second step lives in the ledger, never on the frozen row: {:?}",
            tasks[0].plan
        );
    assert_eq!(
        tasks[0].plan[0], first_step,
        "steps are never evicted or rewritten"
    );
    assert_eq!(
        tasks[0].updated_ms, first_updated,
        "a terminal row is never rewritten: updated_ms stays at the certified gate"
    );
    // The second gate's attempt is a separate durable record (one Passed
    // record per verified attempt), even though the row was already
    // terminal — per-attempt evidence, never a re-certification.
    let task_id = tasks[0].task_id;
    let records = handle.list_verification_records(task_id).unwrap();
    assert_eq!(records.len(), 2, "one Passed record per verified attempt");
    assert!(records
        .iter()
        .all(|r| r.status == VerificationStatus::Passed));
    // Idle reflection: with no turn in flight the row equals the LAST
    // VerifiedComplete gate and the task_state fact agrees with it.
    let facts = handle.memory_facts().unwrap();
    let state_fact = facts
        .iter()
        .find(|(k, key, _)| k == "task_state" && key == "state")
        .map(|(_, _, v)| v.as_str())
        .unwrap();
    assert_eq!(state_fact, "verified_complete");
    assert_eq!(
        handle.memory_facts().unwrap().len(),
        facts.len(),
        "restart restore is idempotent: no facts duplicated by the drive"
    );
}

#[tokio::test]
async fn review_block_never_claims_completion_and_lands_no_linked_record() {
    // Adversarial P0-8 (iv): the checks PASS but the skeptical review
    // blocks the change. The turn must NOT be VerifiedComplete, the
    // machine row lands Blocked (the legal Running -> Blocked edge from
    // the fresh row), and NO record exists to point at — completion
    // proof is only ever linked by an actual completion. A later clean
    // turn on the SAME session re-verifies from Blocked (Blocked ->
    // Running -> NeedsVerification -> Verifying) and completes with
    // exactly one record.
    let (manager, session, _dir) = verified_shared_env();
    let ok = fake_ok();
    // Turn 1: a TODO-placeholder change — checks pass, the review blocks.
    let (turn1_deps, _d1) = verified_turn_deps(
        &manager,
        vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                input: serde_json::json!({
                    "path": "src/bad.rs",
                    "content": "// TODO: implement the real fix\n",
                }),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
        ok.clone(),
        0.65,
    );
    let runtime1 = AgentRuntime::new(turn1_deps).unwrap();
    let o1 = runtime1
        .run_turn(session, "fix the bug", &[])
        .await
        .unwrap();
    drop(runtime1);
    assert_eq!(o1.acceptance, Some(faktor_verify::Acceptance::Pass));
    match o1.completion {
        Some(CompletionGate::BlockedVerification { reasons }) => {
            assert!(
                reasons.iter().any(|r| r.code == ReasonCode::ReviewBlocked),
                "the review must block: {reasons:?}"
            );
        }
        other => panic!("review block must gate, got {other:?}"),
    }
    let h = manager.get_session(session).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    let row = &h.list_tasks().unwrap()[0];
    assert_ne!(row.state, TaskState::VerifiedComplete);
    assert_eq!(
        row.state,
        TaskState::Blocked,
        "the machine lands Blocked (Running -> Blocked) for a review-blocked claim"
    );
    assert_eq!(
            h.list_verification_records(task_id).unwrap().len(),
            0,
            "a blocked gate links NO completion proof: records only exist for attempts that certify or fail verification"
        );
    let facts = h.memory_facts().unwrap();
    assert!(
        facts
            .iter()
            .any(|(k, key, v)| k == "task_state" && key == "state" && v == "blocked"),
        "{facts:?}"
    );
    drop(h);
    // Turn 2: the clean fix on the same session — the blocked row
    // unblocks through the machine and completes.
    let (turn2_deps, _d2) = verified_turn_deps(
        &manager,
        vec![
            ScriptedResponse::ToolCall {
                id: "c2".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/clean.rs", "content": "pub fn clean() -> u32 {\n    let base: u32 = 41;\n    let step: u32 = 1;\n    base.saturating_add(step).saturating_mul(2).saturating_add(1)\n}\n"}),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
        ok,
        0.65,
    );
    let runtime2 = AgentRuntime::new(turn2_deps).unwrap();
    let o2 = runtime2
        .run_turn(session, "write the real fix", &[])
        .await
        .unwrap();
    drop(runtime2);
    assert_eq!(
        o2.completion,
        Some(CompletionGate::VerifiedComplete),
        "a clean turn after a review block completes"
    );
    let h = manager.get_session(session).unwrap().unwrap();
    assert_eq!(
        h.list_tasks().unwrap()[0].state,
        TaskState::VerifiedComplete
    );
    let records = h.list_verification_records(task_id).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].status, VerificationStatus::Passed);
}

#[tokio::test]
async fn task_row_records_failed_verification_as_retryable_needs_verification() {
    // A failed REQUIRED check gates FailedVerification. The durable FACT
    // keeps recording the gate ("failed", the wave-8/9 durable semantic),
    // but the typed task row is now machine-driven (audit P0-7): the
    // attempt lands a durable FAILED VerificationRecord (one per
    // attempt) and the row lands NeedsVerification (Verifying ->
    // NeedsVerification = "verification needs another iteration") —
    // NEVER the terminal Failed state a patch used to write, because a
    // terminal row would freeze and forbid the next turn's fix-and-re-
    // verify cycle the gate semantics require.
    let (manager, session, _dir) = verified_shared_env();
    let (turn_deps, _d) = verified_turn_deps(
        &manager,
        vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/broken.rs", "content": "pub fn broken() -> u32 {\n    let base: u32 = 0;\n    let step: u32 = 1;\n    base.saturating_add(step).saturating_add(0)\n}\n"}),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
        fake(|_cmd: &str| Err("boom".to_string())),
        0.65,
    );
    let runtime = AgentRuntime::new(turn_deps).unwrap();
    let outcome = runtime
        .run_turn(session, "write broken", &[])
        .await
        .unwrap();
    assert!(matches!(
        outcome.completion,
        Some(CompletionGate::FailedVerification { .. })
    ));
    let handle = manager.get_session(session).unwrap().unwrap();
    let tasks = handle.list_tasks().unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(
            tasks[0].state,
            TaskState::NeedsVerification,
            "the row carries the retryable machine state, never the terminal Failed a patch used to write"
        );
    let records = handle.list_verification_records(tasks[0].task_id).unwrap();
    assert_eq!(
        records.len(),
        1,
        "one durable Failed record per verification attempt"
    );
    assert_eq!(records[0].status, VerificationStatus::Failed);
    assert!(
        records[0]
            .checks
            .iter()
            .any(|c| c.status == VerificationStatus::Failed && c.required),
        "the record carries the executed check's failed verdict: {:?}",
        records[0].checks
    );
    assert!(
        records[0].criteria.iter().all(|c| !c.passed),
        "a failed attempt certifies no criterion: {:?}",
        records[0].criteria
    );
    let facts = handle.memory_facts().unwrap();
    assert!(
        facts
            .iter()
            .any(|(k, key, v)| k == "task_state" && key == "state" && v == "failed"),
        "the durable gate fact still records the Failed gate: {facts:?}"
    );
}

#[tokio::test]
async fn spend_over_budget_and_review_block_precedence_is_identical_after_restart() {
    // Audit 25/92 (c): when BOTH blockers apply — the review gates the
    // change AND the durable spend exceeds the task budget — the
    // review-block (computed first, at the verdict) keeps precedence;
    // the budget refusal only ever downgrades a PASSING gate. A restart
    // must recompute the SAME gate with the SAME reasons (deterministic
    // re-derivation over the same durable rows).
    let (manager, session, dir) = verified_shared_env();
    let h = manager.get_session(session).unwrap().unwrap();
    // Durable spend first (provider-call rows are the crash-safe source).
    h.record_provider_call(
        OpId::new(4242),
        "fake",
        "m",
        "completed",
        Some(500),
        Some(200),
        None,
    )
    .unwrap();
    assert_eq!(h.spent_tokens().unwrap(), 700);
    // Pre-create the row with a token budget the spend already exceeds.
    let now = h.now_ms();
    h.create_task(Task {
        task_id: h.task_id().unwrap(),
        session_id: session,
        goal: "gating task".into(),
        acceptance_criteria: vec![],
        plan: vec![],
        attachments: Vec::new(),
        budget: faktor_session::TaskBudget {
            max_tokens: Some(10),
            max_turns: None,
            spent_tokens: 700,
            spent_turns: 0,
        },
        state: TaskState::Running,
        created_ms: now,
        updated_ms: now,
    })
    .unwrap();
    let ok = fake_ok();
    // The change is review-blocking (a TODO placeholder file) and the
    // spend is over budget: the gate must carry ONLY the review reason
    // — the budget refusal never overwrites an existing blocker.
    let todo_content = "// TODO: implement the real fix\n";
    let (turn_deps, _d) = verified_turn_deps(
        &manager,
        vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                input: serde_json::json!({
                    "path": "src/bad.rs",
                    "content": todo_content,
                }),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
        ok.clone(),
        0.65,
    );
    let runtime = AgentRuntime::new(turn_deps).unwrap();
    let o1 = runtime.run_turn(session, "fix the bug", &[]).await.unwrap();
    drop(runtime);
    assert_eq!(o1.acceptance, Some(faktor_verify::Acceptance::Pass));
    let codes1: Vec<ReasonCode> = match &o1.completion {
        Some(CompletionGate::BlockedVerification { reasons }) => {
            assert!(
                    reasons
                        .iter()
                        .any(|r| r.code == ReasonCode::ReviewBlocked
                            && r.detail.contains("src/bad.rs")),
                    "review precedence: {reasons:?}"
                );
            assert!(
                    !reasons
                        .iter()
                        .any(|r| r.code == ReasonCode::SpendOverBudget),
                    "the budget refusal only downgrades a PASSING gate; the review already blocks: {reasons:?}"
                );
            reasons.iter().map(|r| r.code).collect()
        }
        other => panic!("review block must gate, got {other:?}"),
    };
    // The durable rows record the same gate.
    let h = manager.get_session(session).unwrap().unwrap();
    assert_eq!(h.list_tasks().unwrap()[0].state, TaskState::Blocked);
    let facts = h.memory_facts().unwrap();
    assert!(
        facts
            .iter()
            .any(|(k, key, v)| k == "task_state" && key == "state" && v == "blocked"),
        "{facts:?}"
    );

    // ---- restart over the SAME store: an identical turn recomputes
    // the IDENTICAL gate and reasons (deterministic re-derivation).
    drop(manager);
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let (turn_deps2, _d2) = verified_turn_deps(
        &manager,
        vec![
            ScriptedResponse::ToolCall {
                id: "c2".into(),
                name: "write_file".into(),
                input: serde_json::json!({
                    "path": "src/bad.rs",
                    "content": todo_content,
                }),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
        ok,
        0.65,
    );
    let runtime2 = AgentRuntime::new(turn_deps2).unwrap();
    let o2 = runtime2
        .run_turn(session, "fix the bug", &[])
        .await
        .unwrap();
    drop(runtime2);
    let codes2: Vec<ReasonCode> = match &o2.completion {
        Some(CompletionGate::BlockedVerification { reasons }) => {
            reasons.iter().map(|r| r.code).collect()
        }
        other => panic!("after restart the review block must gate, got {other:?}"),
    };
    assert_eq!(codes1, codes2, "precedence must be identical after restart");
    let h2 = manager.get_session(session).unwrap().unwrap();
    let tasks = h2.list_tasks().unwrap();
    assert_eq!(tasks.len(), 1, "one row after restart");
    assert_eq!(tasks[0].state, TaskState::Blocked);
}

#[tokio::test]
async fn crash_between_durable_criteria_and_gate_write_converges_after_restart() {
    // Adversarial (b): crash the drive INSIDE the genuine end's durable
    // write tail — after the durable criteria/facts landed (persist
    // wrote task_state/verification rows) and BEFORE the typed task-row
    // gate write completed. Restart must converge to the SAME gate an
    // uninterrupted run produces: verification recomputes from the same
    // durable repo state, and the criteria fact + typed row agree
    // byte-for-byte again.
    let (manager, session, dir) = verified_shared_env();
    // Turn 1 (ok verifier): VerifiedComplete seeds criteria rows.
    let ok = fake_ok();
    let (deps1, _d1) = verified_turn_deps(
        &manager,
        vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                input: serde_json::json!({
                    "path": "src/a.rs",
                    "content": "pub fn a() -> u32 {\n    let base: u32 = 10;\n    let step: u32 = 41;\n    base.saturating_add(step).saturating_add(1)\n}\n",
                }),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
        ok.clone(),
        0.65,
    );
    let runtime1 = AgentRuntime::new(deps1).unwrap();
    let o1 = runtime1
        .run_turn(session, "gating task", &[])
        .await
        .unwrap();
    drop(runtime1);
    assert_eq!(o1.completion, Some(CompletionGate::VerifiedComplete));
    let h = manager.get_session(session).unwrap().unwrap();
    let criteria_turn1 = criteria_fact(&h).expect("criteria fact seeded");
    let row_criteria1 = criteria_canonical_text(&h.list_tasks().unwrap()[0].acceptance_criteria);
    assert!(
        row_criteria1.contains("required check: cargo check")
            && !row_criteria1.contains("goal: gating task"),
        "{row_criteria1}"
    );
    assert_eq!(criteria_turn1, row_criteria1);

    // Turn 2 FAILS its check. The drive is aborted the moment the
    // durable task_state fact flips to "failed" — i.e. INSIDE
    // finish_logical_turn, after persist_gate_facts (criteria/facts
    // durable) and before/around the task-row gate write + TurnCompleted
    // tail. The watcher only ever aborts AFTER the crash point is
    // durable, so the crash window is real, never speculative.
    let failing = fake(|_cmd: &str| Err("type error".to_string()));
    let (deps2, _d2) = verified_turn_deps(
        &manager,
        vec![
            ScriptedResponse::ToolCall {
                id: "c2".into(),
                name: "write_file".into(),
                input: serde_json::json!({
                    "path": "src/b.rs",
                    "content": "pub fn b() -> u32 {\n    let base: u32 = 41;\n    base.saturating_add(1)\n}\n",
                }),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
        failing.clone(),
        0.65,
    );
    let runtime2 = AgentRuntime::new(deps2).unwrap();
    let rt = runtime2.clone();
    let drive = tokio::spawn(async move { rt.run_turn(session, "write broken b", &[]).await });
    let h2 = manager.get_session(session).unwrap().unwrap();
    // Environmental margin (documented bound): the watcher waits for the
    // DRIVE (a separate task) under full-suite load; the deadline is a
    // hang bound, never a timing assertion. Full-suite starvation on
    // wide-parallel machines has repeatedly pushed this far past the
    // turn's ~seconds of real work, so the bound is generous: a drive
    // that cannot reach its durable gate write within it is wedged, not
    // slow.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
    loop {
        if let Some((_, _, v)) = h2
            .memory_facts()
            .unwrap()
            .iter()
            .find(|(k, key, _)| k == "task_state" && key == "state")
            .cloned()
        {
            if v == "failed" {
                // The durable gate facts exist: crash here.
                break;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "turn 2 never reached the durable gate write"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    drive.abort();
    let _ = drive.await;
    drop(runtime2);
    // The session DbActor replies on ENQUEUE, so the aborted drive may
    // die with its FINAL tail writes (gate rows, TurnCompleted, the
    // ready row) still queued — applied by the actor only later, while
    // the reopened manager and the resumed drive below are already
    // writing the same store. shutdown() closes the caller queue and
    // waits for every already-enqueued write to apply and the actor to
    // exit, so the reopen below reads the TRUE crash image and the
    // resume never races a ghost writer mid-iteration.
    assert!(
        manager.actor().shutdown(Duration::from_secs(10)).await,
        "the crashed drive's queued writes must drain before the reopen"
    );
    drop(manager);
    // The abort either crashed the drive inside the durable tail
    // (machine mid-finish, turn record open) or landed after the finish
    // completed (no crash; the uninterrupted state already stands).
    // The DURABLE truth decides — never the possibly-buffered pre-drop
    // handle — and both branches converge to the same assertions below.

    // ---- restart: if the turn is durably interrupted, resume it (lands
    // the session ready); then re-run the same mutating work: the
    // genuine end re-computes verification over the same durable repo
    // state and must converge to the same FailedVerification gate an
    // uninterrupted run produced, with row + facts byte-consistent.
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let h_durable = manager.get_session(session).unwrap().unwrap();
    let interrupted = state_is_op_active(h_durable.state().unwrap());
    drop(h_durable);
    if interrupted {
        let (deps3, _d3) = verified_turn_deps(
            &manager,
            vec![
                ScriptedResponse::Text("finished after crash".into()),
                ScriptedResponse::End,
            ],
            failing.clone(),
            0.65,
        );
        let runtime3 = AgentRuntime::new(deps3).unwrap();
        let o3 = runtime3.continue_turn(session).await.unwrap();
        drop(runtime3);
        assert_eq!(o3.final_state, AgentState::ReadyForNextTurn);
    }
    // The recompute turn: identical durable inputs (same goal, same
    // changed file, same failing runner) => identical gate.
    let (deps4, _d4) = verified_turn_deps(
        &manager,
        vec![
            ScriptedResponse::ToolCall {
                id: "c4".into(),
                name: "write_file".into(),
                input: serde_json::json!({
                    "path": "src/b.rs",
                    "content": "pub fn b() -> u32 {\n    let base: u32 = 41;\n    base.saturating_add(1)\n}\n",
                }),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
        failing,
        0.65,
    );
    let runtime4 = AgentRuntime::new(deps4).unwrap();
    let o4 = runtime4
        .run_turn(session, "write broken b", &[])
        .await
        .unwrap();
    drop(runtime4);
    assert_eq!(o4.final_state, AgentState::ReadyForNextTurn);
    match o4.completion {
        Some(CompletionGate::FailedVerification { reasons }) => {
            assert!(
                reasons
                    .iter()
                    .any(|r| r.code == ReasonCode::CheckFailed && r.detail.contains("cargo check")),
                "restart must converge to the same failed gate: {reasons:?}"
            );
        }
        other => {
            panic!("restart must recompute the same FailedVerification gate, got {other:?}")
        }
    }
    let h3 = manager.get_session(session).unwrap().unwrap();
    let tasks = h3.list_tasks().unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(
            tasks[0].state,
            TaskState::VerifiedComplete,
            "turn 1's certification is TERMINAL under audit P0-7: the later Failed gates land on the facts + records, never on the frozen row (the old patch rewrote the row to Failed and back; the machine freezes it at VerifiedComplete)"
        );
    assert!(
        tasks[0]
            .acceptance_criteria
            .iter()
            .any(|c| c.contains("cargo check")),
        "row criteria converge: {:?}",
        tasks[0].acceptance_criteria
    );
    // Record lifecycle across the crash + convergence: turn 1's PASSED
    // record durably certifies the VerifiedComplete row (record-first),
    // and every later FAILED attempt lands its own Failed record — the
    // row is never VerifiedComplete without a matching Passed record.
    let records = h3.list_verification_records(tasks[0].task_id).unwrap();
    assert!(
        records
            .iter()
            .any(|r| r.status == VerificationStatus::Passed),
        "the completion is always backed by a durable Passed record: {records:?}"
    );
    assert!(
        records
            .iter()
            .any(|r| r.status == VerificationStatus::Failed),
        "each failed attempt lands its Failed record: {records:?}"
    );
    assert!(
        records
            .iter()
            .all(|r| r.criteria.iter().any(|c| !c.passed)
                == (r.status == VerificationStatus::Failed)),
        "only Failed records certify failing criteria: {records:?}"
    );
    let facts = h3.memory_facts().unwrap();
    let criteria = criteria_fact(&h3).expect("criteria fact survives");
    assert_eq!(
        criteria, criteria_turn1,
        "the once-only criteria fact is byte-identical across crash + restart"
    );
    assert_eq!(
        criteria,
        criteria_canonical_text(&tasks[0].acceptance_criteria),
        "durable criteria fact and typed task row agree after convergence"
    );
    assert!(
        facts
            .iter()
            .any(|(k, key, v)| k == "task_state" && key == "state" && v == "failed"),
        "the LAST Failed gate still records its fact: {facts:?}"
    );
}

#[tokio::test]
async fn turn_review_blocks_when_written_file_contains_todo() {
    // A scripted write that leaves "// TODO: implement" in the changed
    // file: at the genuine turn end the review (which only runs where
    // the verifier runs) must report it as a blocking reason, without
    // failing the turn.
    let (deps, _dir, root) = review_env(vec![
        ScriptedResponse::ToolCall {
            id: "c1".into(),
            name: "write_file".into(),
            input: serde_json::json!({
                "path": "src/bad.rs",
                "content": "// TODO: implement the real fix\n"
            }),
        },
        ScriptedResponse::Text("done".into()),
        ScriptedResponse::End,
    ]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = session_in_workspace(runtime.deps(), &root);
    let outcome = runtime.run_turn(session, "fix the bug", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_eq!(outcome.acceptance, Some(faktor_verify::Acceptance::Pass));
    let review = outcome.review.expect("review must run with a verifier");
    assert_eq!(review["verdict"], "block", "{review}");
    let blocking: Vec<String> = review["blocking"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s.as_str().map(str::to_string))
        .collect();
    assert!(
        blocking
            .iter()
            .any(|b| b.contains("TODO") && b.contains("src/bad.rs")),
        "blocking must carry the placeholder/TODO reason: {blocking:?}"
    );
    let evidence = review["evidence"].get("files").unwrap().as_array().unwrap();
    let bad = &evidence[0];
    assert_eq!(bad["path"], "src/bad.rs");
    assert_eq!(bad["contains_todo"], true, "{review}");
    assert_eq!(
        bad["head_chars"], 32,
        "evidence bound at the head, not the file"
    );
    assert!(review["evidence"]["todo_files"][0] == "src/bad.rs");
}

#[tokio::test]
async fn review_block_gates_completion_despite_passing_checks() {
    // (d) the skeptical-review gate (audit: review must not be advisory
    // for high-impact completion): a "block" review verdict coexisting
    // with a PASSING verification downgrades the completion gate to
    // BlockedVerification whose reasons come from the review — the turn
    // stays ReadyForNextTurn and never claims verified completion.
    let (deps, _dir, root) = review_env(vec![
        ScriptedResponse::ToolCall {
            id: "c1".into(),
            name: "write_file".into(),
            input: serde_json::json!({
                "path": "src/bad.rs",
                "content": "// TODO: implement the real fix\n"
            }),
        },
        ScriptedResponse::Text("done".into()),
        ScriptedResponse::End,
    ]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = session_in_workspace(runtime.deps(), &root);
    let outcome = runtime.run_turn(session, "fix the bug", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_eq!(outcome.acceptance, Some(faktor_verify::Acceptance::Pass));
    let review = outcome.review.as_ref().expect("review must run");
    assert_eq!(review["verdict"], "block", "{review}");
    match outcome.completion {
        Some(CompletionGate::BlockedVerification { reasons }) => {
            assert!(
                reasons.iter().any(|r| {
                    r.code == ReasonCode::ReviewBlocked
                        && r.detail.contains("TODO")
                        && r.detail.contains("src/bad.rs")
                }),
                "blocked reasons must carry the review's finding: {reasons:?}"
            );
        }
        other => panic!("review block must gate BlockedVerification, got {other:?}"),
    }
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let facts = handle.memory_facts().unwrap();
    assert!(
        facts
            .iter()
            .any(|(k, key, v)| k == "task_state" && key == "state" && v == "blocked"),
        "task_state must be Blocked under a review block: {facts:?}"
    );
    let last: serde_json::Value = serde_json::from_str(
        &facts
            .iter()
            .find(|(k, key, _)| k == "verification" && key == "last")
            .expect("last-run row")
            .2,
    )
    .unwrap();
    assert_eq!(
        last["status"], "passed",
        "the verification engine itself passed; the gate is blocked by the review: {last}"
    );
}

#[tokio::test]
async fn strict_rejects_advisory_review_on_mutating_task_normal_keeps_today() {
    // Audit 92 (d): a MUTATING change whose only review finding is
    // ADVISORY (a TODO comment above real code — ≥60 code chars, so not
    // a placeholder) must be refused by the Strict bar (the mutating
    // default) while Normal — today's behavior — still verifies.
    let todo_over_code = "// TODO: revisit once the retry spec lands\n\
                              pub fn backoff(attempt: u32) -> Duration {\n\
                                  let base = Duration::from_millis(100);\n\
                                  let growth: u32 = 1 << attempt.min(6);\n\
                                  Duration::from_millis(u64::from(base.as_millis() as u32) * u64::from(growth))\n\
                              }\n";
    // Normal quality: advisory suspects never gate (today's behavior).
    let (deps, _dir, root) = review_env(vec![
        ScriptedResponse::ToolCall {
            id: "c1".into(),
            name: "write_file".into(),
            input: serde_json::json!({
                "path": "src/backoff.rs",
                "content": todo_over_code,
            }),
        },
        ScriptedResponse::Text("done".into()),
        ScriptedResponse::End,
    ]);
    let runtime = AgentRuntime::new(deps).unwrap();
    runtime.set_verification_quality(VerificationQuality::Normal);
    let session = session_in_workspace(runtime.deps(), &root);
    let outcome = runtime
        .run_turn(session, "fix the retry", &[])
        .await
        .unwrap();
    let review = outcome.review.as_ref().expect("review runs");
    assert_eq!(review["verdict"], "pass", "advisory-only: {review}");
    assert!(
        !review["suspects"].as_array().unwrap().is_empty(),
        "{review}"
    );
    assert_eq!(
        outcome.completion,
        Some(CompletionGate::VerifiedComplete),
        "Normal keeps today's behavior: advisory findings never gate"
    );
    // Default quality (mutating turn => Strict): the SAME advisory
    // finding gates BlockedVerification with the review code.
    let (deps, _dir, root) = review_env(vec![
        ScriptedResponse::ToolCall {
            id: "c1".into(),
            name: "write_file".into(),
            input: serde_json::json!({
                "path": "src/backoff.rs",
                "content": todo_over_code,
            }),
        },
        ScriptedResponse::Text("done".into()),
        ScriptedResponse::End,
    ]);
    let runtime = AgentRuntime::new(deps).unwrap(); // Strict by default
    let session = session_in_workspace(runtime.deps(), &root);
    let outcome = runtime
        .run_turn(session, "fix the retry", &[])
        .await
        .unwrap();
    match outcome.completion {
        Some(CompletionGate::BlockedVerification { reasons }) => {
            assert!(
                reasons.iter().any(|r| {
                    r.code == ReasonCode::ReviewBlocked && r.detail.contains("contains TODO")
                }),
                "strict must block the advisory finding: {reasons:?}"
            );
        }
        other => {
            panic!("Strict must reject an advisory review on a mutating task, got {other:?}")
        }
    }
}

#[tokio::test]
async fn weakened_test_change_never_clears_the_gate_under_strict() {
    // Audit 92 (a): a review skepticism case end-to-end. A high-impact
    // change that hollows out a test (test markers, zero assertions)
    // must gate BlockedVerification under Strict (the mutating default)
    // — the weakened-test review verdict must NOT clear the gate even
    // though every derived check passed.
    let (deps, _dir, root) = review_env(vec![
        ScriptedResponse::ToolCall {
            id: "c1".into(),
            name: "write_file".into(),
            input: serde_json::json!({
                "path": "tests/calc.rs",
                "content": "#[test]\nfn adds() -> u32 {\n    let got: u32 = add(1, 2);\n    got\n}\n"
            }),
        },
        ScriptedResponse::Text("done".into()),
        ScriptedResponse::End,
    ]);
    let runtime = AgentRuntime::new(deps).unwrap(); // Strict by default
    let session = session_in_workspace(runtime.deps(), &root);
    let outcome = runtime
        .run_turn(session, "harden the calc tests", &[])
        .await
        .unwrap();
    assert_eq!(outcome.acceptance, Some(faktor_verify::Acceptance::Pass));
    let review = outcome.review.as_ref().expect("review runs");
    let files = review["evidence"]["files"].as_array().unwrap();
    assert!(
        files.iter().any(|f| f["weakened_test_suspect"] == true),
        "the hollowed test must be flagged: {review}"
    );
    match outcome.completion {
        Some(CompletionGate::BlockedVerification { reasons }) => {
            assert!(
                reasons.iter().any(|r| {
                    r.code == ReasonCode::ReviewBlocked && r.detail.contains("weakened test file")
                }),
                "the weakened-test review must gate: {reasons:?}"
            );
        }
        other => panic!("weakened test must not clear the gate, got {other:?}"),
    }
}

#[tokio::test]
async fn turn_review_passes_for_clean_real_change() {
    // A genuine implementation change (assertions in the test, real body
    // in the source) must come out of the review as pass — no blocking.
    let (deps, _dir, root) = review_env(vec![
        ScriptedResponse::ToolCall {
            id: "c1".into(),
            name: "write_file".into(),
            input: serde_json::json!({
                "path": "src/calc.rs",
                "content": "pub fn add(a: i32, b: i32) -> i32 {\n    a.saturating_add(b)\n}\n"
            }),
        },
        ScriptedResponse::ToolCall {
            id: "c2".into(),
            name: "write_file".into(),
            input: serde_json::json!({
                "path": "tests/calc.rs",
                "content": "#[test]\nfn adds() {\n    assert_eq!(add(1, 2), 3);\n}\n"
            }),
        },
        ScriptedResponse::Text("done".into()),
        ScriptedResponse::End,
    ]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = session_in_workspace(runtime.deps(), &root);
    let outcome = runtime
        .run_turn(session, "implement add with a test", &[])
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    let review = outcome.review.expect("review must run with a verifier");
    assert_eq!(review["verdict"], "pass", "{review}");
    let blocking = review["blocking"].as_array().unwrap();
    assert!(blocking.is_empty(), "{review}");
    let files = review["evidence"]["files"].as_array().unwrap();
    assert_eq!(files.len(), 2, "both changed files reviewed: {review}");
    assert!(
        files.iter().all(|f| f["weakened_test_suspect"] == false),
        "{review}"
    );
}

/// F3: a transient store READ FAILURE on the queue runner's durable
/// re-check is never an empty queue. The gate stays armed, retries
/// (bounded), and the retried pass drains the durable head — the prompt
/// is not dropped and the runner is not released on the error.
#[tokio::test]
async fn queue_head_read_failure_recheck_keeps_gate_armed_then_drains() {
    let dir = fresh_store_dir();
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let provider = Arc::new(scripted_provider(vec![
        ScriptedResponse::Text("answer".into()),
        ScriptedResponse::End,
    ]));
    let (deps, _keep) = deps_sharing_session(manager.clone(), provider.clone(), vec![]);
    let runtime = Arc::new(AgentRuntime::new(deps).unwrap());
    let ws = manager.create_workspace("/w").unwrap();
    let handle = manager.create_session(ws, "t", "fake", "m").unwrap();
    let op = manager.try_next_op_id().unwrap();
    manager
        .store()
        .enqueue_prompt(handle.id(), op, "queued", &[], None, None, None, 1)
        .unwrap();
    assert_eq!(handle.queued_prompt_count().unwrap(), 1);
    durable_faults_tests::arm(manager.store().root(), DW_SITE_QUEUE_HEAD_READ);
    runtime.run_session_queue(handle.id()).await;
    assert_eq!(
        provider.script.lock().unwrap().len(),
        0,
        "the retried pass must drain the durable queue head (no prompt dropped)"
    );
    assert_eq!(handle.queued_prompt_count().unwrap(), 0);
}

#[test]
fn review_flags_todo_head_and_verdict_blocks() {
    // A head carrying "// TODO: implement x" is a placeholder body: the
    // evidence must flag contains_todo AND the verdict must block.
    let evidence = signals_for(&[("src/fixme.rs", "// TODO: implement x\n")]);
    let f = &evidence["files"][0];
    assert_eq!(f["path"], "src/fixme.rs");
    assert_eq!(f["contains_todo"], true);
    assert_eq!(f["unread"], false);
    assert_eq!(f["placeholder_detected"], true, "{evidence}");
    assert_eq!(evidence["todo_files"][0], "src/fixme.rs");
    let verdict = review_verdict(&evidence, &[]);
    assert_eq!(verdict["verdict"], "block");
    let blocking = blocking_of(&verdict);
    assert!(
        blocking
            .iter()
            .any(|b| b.contains("placeholder/TODO") && b.contains("src/fixme.rs")),
        "blocking must name the placeholder/TODO file: {blocking:?}"
    );
}

#[test]
fn review_todo_on_comment_over_real_code_is_warn_not_block() {
    // A TODO that lives in a comment above a REAL implementation must
    // not block: placeholder detection requires a stub/short body.
    let head = "// TODO: revisit once the retry spec lands\n\
                    pub fn backoff(attempt: u32) -> Duration {\n\
                        let base = Duration::from_millis(100);\n\
                        let growth: u32 = 1 << attempt.min(6);\n\
                        Duration::from_millis(u64::from(base.as_millis() as u32) * u64::from(growth))\n\
                    }\n";
    let evidence = signals_for(&[("src/backoff.rs", head)]);
    let f = &evidence["files"][0];
    assert_eq!(f["contains_todo"], true);
    assert_eq!(f["placeholder_detected"], false, "{evidence}");
    let verdict = review_verdict(&evidence, &[]);
    assert_eq!(
        verdict["verdict"], "pass",
        "todo on a comment alone is warn-level"
    );
    assert!(blocking_of(&verdict).is_empty());
    assert!(
        suspects_of(&verdict)
            .iter()
            .any(|s| s.contains("contains TODO in changed file: src/backoff.rs")),
        "{verdict}"
    );
}

#[test]
fn review_flags_weakened_test_suspect_and_blocks() {
    // Test markers (describe/it) with ZERO assertion tokens: the head
    // looks like the test body was hollowed out — blocking.
    let head = "describe(\"calculator\", () => {\n    it(\"adds\", () => {\n        const got = calc.add(1, 2);\n    });\n});\n";
    let evidence = signals_for(&[("tests/calc_spec.js", head)]);
    let f = &evidence["files"][0];
    assert_eq!(f["looks_like_test"], true);
    assert_eq!(f["weakened_test_suspect"], true, "{evidence}");
    assert_eq!(evidence["weakened_test_files"][0], "tests/calc_spec.js");
    let verdict = review_verdict(&evidence, &[]);
    assert_eq!(verdict["verdict"], "block");
    assert!(
        blocking_of(&verdict)
            .iter()
            .any(|b| b.contains("weakened test file without assertions")),
        "{verdict}"
    );
}

#[test]
fn review_clean_implementation_passes() {
    // A real implementation (assertions present, no TODO/stub) yields no
    // blocking and no suspects: verdict pass.
    let code = "pub fn add(a: i32, b: i32) -> i32 {\n\
                    let sum = a.checked_add(b).unwrap_or_else(|| panic!(\"overflow\"));\n\
                    sum\n\
                    }\n";
    let test = "use super::*;\n\
                    #[test]\n\
                    fn adds() {\n\
                        let got = add(1, 2);\n\
                        assert_eq!(got, 3);\n\
                    }\n";
    let evidence = signals_for(&[("src/calc.rs", code), ("tests/calc.rs", test)]);
    let verdict = review_verdict(&evidence, &[]);
    assert_eq!(verdict["verdict"], "pass");
    assert!(blocking_of(&verdict).is_empty());
    assert!(suspects_of(&verdict).is_empty(), "{verdict}");
}

#[test]
fn review_placeholder_body_dot_dot_dot_flagged() {
    // A body made of "..." only is placeholder evidence (suspect-level;
    // nothing to block unless a TODO rides along).
    let evidence = signals_for(&[("src/stub.rs", "...\n")]);
    let f = &evidence["files"][0];
    assert_eq!(f["placeholder_detected"], true);
    assert_eq!(f["contains_todo"], false);
    assert_eq!(evidence["placeholder_files"][0], "src/stub.rs");
    let verdict = review_verdict(&evidence, &[]);
    assert_eq!(verdict["verdict"], "pass");
    assert!(blocking_of(&verdict).is_empty());
    assert!(
        suspects_of(&verdict)
            .iter()
            .any(|s| s.contains("placeholder body in changed file: src/stub.rs")),
        "{verdict}"
    );
}

#[test]
fn normal_review_gate_keeps_today_semantics_unknown_verdicts_never_block() {
    // Normal = today's behavior: only an exact "block" verdict gates.
    // A weakened/mislabeled review (verdict "weakened", even WITH a
    // blocking list) must not gate under Normal — the label says
    // advisory, and today's code honored only the literal "block".
    let weakened = review_json(
        "weakened",
        &["weakened test file without assertions: tests/a.rs"],
        &[],
    );
    assert!(
        review_blocking_reasons(Some(&weakened), VerificationQuality::Normal).is_empty(),
        "Normal ignores a mislabeled verdict (today's behavior)"
    );
    let advisory = review_json("pass", &[], &["contains TODO in changed file: src/a.rs"]);
    assert!(
        review_blocking_reasons(Some(&advisory), VerificationQuality::Normal).is_empty(),
        "Normal ignores advisory suspects (today's behavior)"
    );
    let blocked = review_json(
        "block",
        &["weakened test file without assertions: tests/a.rs"],
        &[],
    );
    let reasons = review_blocking_reasons(Some(&blocked), VerificationQuality::Normal);
    assert_eq!(reasons.len(), 1);
    assert_eq!(reasons[0].code, ReasonCode::ReviewBlocked);
    assert!(reasons[0].detail.contains("tests/a.rs"));
    let hostile = review_json("block", &[], &[]);
    assert!(
        review_blocking_reasons(Some(&hostile), VerificationQuality::Normal).is_empty(),
        "Normal: a block verdict with no listed reasons clears (documented today behavior)"
    );
    assert!(
        review_blocking_reasons(None, VerificationQuality::Normal).is_empty(),
        "no review never blocks"
    );
}

#[test]
fn strict_review_gate_blocks_weakened_advisory_and_mislabeled_verdicts() {
    // Strict (the mutating-turn default) is fail-closed: ANY non-clean
    // review gates. A weakened-test review — or a review mislabeled
    // "weakened" to look advisory — must NOT clear the gate for a
    // high-impact file change.
    let weakened = review_json(
        "weakened",
        &["weakened test file without assertions: tests/calc.rs"],
        &[],
    );
    let reasons = review_blocking_reasons(Some(&weakened), VerificationQuality::Strict);
    assert_eq!(reasons.len(), 1, "the weakened verdict must block");
    assert_eq!(reasons[0].code, ReasonCode::ReviewBlocked);
    assert!(
        reasons[0].detail.contains("weakened"),
        "detail must carry the shape: {:?}",
        reasons[0]
    );
    // Advisory suspects on a mutating change block in Strict.
    let advisory = review_json(
        "pass",
        &[],
        &["contains TODO in changed file: src/backoff.rs"],
    );
    let reasons = review_blocking_reasons(Some(&advisory), VerificationQuality::Strict);
    assert_eq!(reasons.len(), 1);
    assert_eq!(reasons[0].code, ReasonCode::ReviewBlocked);
    assert!(reasons[0].detail.contains("contains TODO"));
    // Hostile shape: verdict missing entirely.
    let mut hostile = review_json("pass", &[], &[]);
    hostile.as_object_mut().unwrap().remove("verdict");
    let reasons = review_blocking_reasons(Some(&hostile), VerificationQuality::Strict);
    assert_eq!(reasons.len(), 1);
    assert_eq!(reasons[0].code, ReasonCode::ReviewBlocked);
    // A genuinely clean review passes in Strict.
    let clean = review_json("pass", &[], &[]);
    assert!(review_blocking_reasons(Some(&clean), VerificationQuality::Strict).is_empty());
    // Blocking + suspects merge, deduped, all coded review_blocked.
    let mixed = review_json(
        "block",
        &["weakened test file without assertions: tests/a.rs"],
        &["contains TODO in changed file: src/a.rs"],
    );
    let reasons = review_blocking_reasons(Some(&mixed), VerificationQuality::Strict);
    assert_eq!(reasons.len(), 2);
    assert!(reasons.iter().all(|r| r.code == ReasonCode::ReviewBlocked));
}

#[test]
fn review_hostile_megabyte_head_is_bounded() {
    // A 1 MiB head whose ONLY TODO marker sits beyond the 400-char scan
    // window must not flag: the fn reads nothing past the bounded head.
    let mut huge = "a".repeat(1024 * 1024);
    huge.push_str("// TODO: implement buried past the scan window\n");
    let evidence = signals_for(&[("src/huge.rs", huge.as_str())]);
    let f = &evidence["files"][0];
    assert_eq!(
        f["contains_todo"], false,
        "TODO beyond 400 chars must stay invisible"
    );
    assert_eq!(f["head_chars"], 400);
    let rendered = serde_json::to_string(&evidence).unwrap();
    assert!(
        rendered.len() < 4 * 1024,
        "evidence must stay tiny for hostile heads ({} bytes)",
        rendered.len()
    );
    let verdict = review_verdict(&evidence, &[]);
    assert_eq!(verdict["verdict"], "pass");
    // And the SAME hostile head with the marker inside the window still
    // flags (the bound is a window, not an excuse).
    let mut early = "a".repeat(100);
    early.push_str("// TODO: implement\n");
    early.push_str(&"b".repeat(1024 * 1024));
    let evidence = signals_for(&[("src/huge.rs", early.as_str())]);
    assert_eq!(evidence["files"][0]["contains_todo"], true);
}

#[test]
fn review_criteria_relevance_suspect_only_when_unaddressed() {
    // Non-empty criteria with no token overlap across the changed paths
    // → warn suspect, never a block. A path that shares a topic token
    // suppresses it.
    let addressed = signals_for(&[("src/parser.rs", "pub fn parse() {}\n")]);
    let verdict = review_verdict(&addressed, &["rewrite the parser to be fully async".into()]);
    assert_eq!(verdict["verdict"], "pass");
    assert!(
        !suspects_of(&verdict).iter().any(|s| s.contains("criteria")),
        "{verdict}"
    );
    let unaddressed = signals_for(&[("README.md", "docs\n")]);
    let verdict = review_verdict(
        &unaddressed,
        &["rewrite the parser to be fully async".into()],
    );
    assert_eq!(verdict["verdict"], "pass");
    assert!(
        suspects_of(&verdict)
            .iter()
            .any(|s| s.contains("criteria not obviously addressed")),
        "{verdict}"
    );
    assert!(verdict["criteria_reviewed"] == true);
}

#[test]
fn review_verdict_empty_evidence_passes() {
    let verdict = review_verdict(&review_signals(&[], &[]), &[]);
    assert_eq!(verdict["verdict"], "pass");
    assert!(blocking_of(&verdict).is_empty());
    assert!(suspects_of(&verdict).is_empty());
}

#[test]
fn review_changed_file_absent_from_snapshot_stays_unread() {
    // A changed path whose head could not be read (deleted/moved) is
    // evidence as unread with its path test-likeness only — never a
    // crash, never fabricated flags.
    let evidence = review_signals(&["tests/gone.rs".to_string()], &[]);
    let f = &evidence["files"][0];
    assert_eq!(f["path"], "tests/gone.rs");
    assert_eq!(f["unread"], true);
    assert_eq!(f["looks_like_test"], true);
    assert_eq!(f["contains_todo"], false);
    assert_eq!(f["weakened_test_suspect"], false);
    let verdict = review_verdict(&evidence, &[]);
    assert_eq!(verdict["verdict"], "pass");
}

#[tokio::test]
async fn unavailable_hard_cap_budget_read_refuses_the_review_before_the_provider() {
    // Adversarial: a hard cap whose accounting cannot be read is NOT a
    // free budget. The review (a paid call) must refuse typed and the
    // provider stream must never open.
    let costly = CostReportingProvider::new(vec![(None, false)]);
    let (mut deps, _dir) = deps_with(costly.clone(), vec![]);
    let failing = ReadFailingBudget::new(BudgetCapEvidence::HardCap(9_999));
    deps.budgets = failing.clone();
    let session = new_session(&deps);
    let handle = deps.session.get_session(session).unwrap().unwrap();
    let cancel = CancellationToken::new();
    let outcome = run_independent_review_call(
        &deps,
        &handle,
        "{\"package\":\"x\"}",
        &["criterion".to_string()],
        None,
        &cancel,
    )
    .await;
    assert!(
        outcome.verdict.is_none(),
        "no verdict without a provider call"
    );
    let reason = outcome.refused.expect("a refused review");
    assert!(reason.contains("budget accounting unavailable"), "{reason}");
    assert_eq!(
        costly.stream_count(),
        0,
        "no paid provider call may be issued"
    );
}

/// Audit 16 (hash-mismatch refusal path): a high-risk review whose
/// changed file no longer hashes to the edit authority's expected hash
/// REFUSES to rely on the evidence — the review blocks with the typed
/// refusal, never a silent pass.
#[tokio::test]
async fn high_risk_review_refuses_drifted_evidence_hash() {
    let review = run_high_risk_drift_scenario(true).await;
    assert_eq!(review["verdict"], "block", "{review}");
    let joined = review_strings(review.get("blocking")).join("\n");
    assert!(
        joined.contains("high-risk evidence refused (refresh required)"),
        "the refusal must be typed: {joined}"
    );
    assert!(
        joined.contains("src/auth.rs") && joined.contains("currently hashes to"),
        "the drift must name the file and both hashes: {joined}"
    );
}

/// Audit 16 (refresh branch): with NO durable edit expectation there is
/// no recorded hash to drift FROM, so the review relies on evidence bound
/// to the bytes it just read — it is refreshed, never refused.
#[tokio::test]
async fn high_risk_review_without_an_edit_expectation_reviews_current_bytes() {
    let review = run_high_risk_drift_scenario(false).await;
    assert_ne!(review["verdict"], "block", "{review}");
    let joined = review_strings(review.get("blocking")).join("\n");
    assert!(
        !joined.contains("high-risk evidence refused"),
        "a missing expectation is a refresh, not a refusal: {joined}"
    );
}

#[tokio::test]
async fn risky_change_independent_review_block_gates_even_when_checks_pass() {
    // (a) P0-13: a RISKY change (security path) with a mocked review
    // model returning Block must gate BlockedVerification even though
    // every derived check PASSED. The old review-block tests exercised
    // local signals only; this proves the review-model verdict itself
    // gates.
    let (manager, session, cas, snapshots, _dir) = snapshot_review_env(&[]);
    let script = vec![
        ScriptedResponse::ToolCall {
            id: "c1".into(),
            name: "write_file".into(),
            input: serde_json::json!({
                "path": "src/security.rs",
                "content": "pub fn authenticate(user: u32, secret: u32) -> u32 {\n    user.checked_add(secret).unwrap_or(0)\n}\n",
            }),
        },
        ScriptedResponse::Text("done".into()),
        ScriptedResponse::End,
    ];
    let (routing, _calls) = PhasePinnedRouting::review_to("reviewmock", "rev");
    let (deps, _d) = snapshot_review_deps(
        &manager,
        &snapshots,
        &cas,
        vec![
            Arc::new(scripted_provider(script)),
            mock_review_provider(
                r#"{"verdict":"block","findings":["the security change is not accompanied by any test change"]}"#,
            ),
        ],
        vec![checkpoint_write_tool()],
        routing,
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let outcome = runtime
        .run_turn(session, "harden the auth path", &[])
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_eq!(
        outcome.acceptance,
        Some(faktor_verify::Acceptance::Pass),
        "the derived checks PASS — only the independent review gates"
    );
    let review = outcome.review.expect("review must run with a verifier");
    assert_eq!(review["verdict"], "block", "{review}");
    let structured = review_evidence_structured(&review);
    assert_eq!(structured["risk"]["level"], "high", "{structured}");
    assert_eq!(
        structured["review_model"]["status"], "called",
        "{structured}"
    );
    assert_eq!(
        structured["review_model"]["verdict"], "block",
        "{structured}"
    );
    match outcome.completion {
        Some(CompletionGate::BlockedVerification { reasons }) => {
            assert!(
                reasons.iter().any(|r| {
                    r.code == ReasonCode::ReviewBlocked
                        && r.detail.contains("independent review model")
                        && r.detail.contains("not accompanied by any test change")
                }),
                "the model's blocking finding must gate: {reasons:?}"
            );
        }
        other => panic!("review-model block must gate BlockedVerification, got {other:?}"),
    }
}

/// The ACTUAL review-model identity is extracted ONLY from a review value
/// whose recorded call was attempted with a non-empty routed pair; every
/// other shape is an honest `None` (the parent's configured pair is never
/// a substitute).
#[test]
fn review_model_identity_extraction_is_honest_about_absence() {
    let attempted = serde_json::json!({
        "verdict": "pass",
        "evidence": {
            "structured": {
                "review_model": {
                    "attempted": true,
                    "status": "called",
                    "provider": "reviewmock",
                    "model": "rev"
                }
            }
        }
    });
    assert_eq!(
        review_model_identity_of(&attempted),
        Some(ReviewModelIdentity {
            provider: "reviewmock".into(),
            model: "rev".into(),
        })
    );
    for absent in [
        // No review-model call was attempted at all.
        serde_json::json!({
            "verdict": "pass",
            "evidence": {"structured": {"review_model": {"attempted": false}}}
        }),
        // Attempted, but refused before routing (no pair recorded).
        serde_json::json!({
            "verdict": "pass",
            "evidence": {"structured": {"review_model": {
                "attempted": true, "provider": "", "model": ""
            }}}
        }),
        // A hostile/legacy shape without the structured record.
        serde_json::json!({"verdict": "pass"}),
    ] {
        assert_eq!(review_model_identity_of(&absent), None, "{absent}");
    }
}

/// PRODUCTION path: the integrated-root verification of a risky change
/// runs the review-model call through the router and exposes the ROUTED
/// provider/model on the verdict — while the parent session's configured
/// pair (fake/m) is demonstrably different.
#[tokio::test]
async fn integrated_root_exposes_the_actual_review_model_identity() {
    let (manager, session, cas, snapshots, dir) = snapshot_review_env(&[(
            "src/security.rs",
            "pub fn authenticate(user: u32, secret: u32) -> u32 {\n    user.checked_add(secret).unwrap_or(0)\n}\n",
        )]);
    let (routing, review_calls) = PhasePinnedRouting::review_to("reviewmock", "rev");
    let (deps, _d) = snapshot_review_deps(
        &manager,
        &snapshots,
        &cas,
        vec![mock_review_provider(r#"{"verdict":"clean","findings":[]}"#)],
        vec![],
        routing,
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let h = manager.get_session(session).unwrap().unwrap();
    let parent_pair = {
        let row = h.row().unwrap();
        (row.provider, row.model)
    };
    assert_ne!(parent_pair, ("reviewmock".to_string(), "rev".to_string()));
    let criterion = Criterion::derived(
        "the security change is independently reviewed",
        CriterionOrigin::ProjectPolicy,
        CriterionRequirement::Required,
        None,
    )
    .with_binding(CriterionBinding::IndependentReview {
        reviewer_id: "final-reviewer".into(),
    });
    let criteria = vec![criterion.encode()];
    let root = dir.path().join("ws");
    let run = runtime
        .verify_integrated_root(
            &h,
            &root,
            &["src/security.rs".to_string()],
            &criteria,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(review_calls.load(std::sync::atomic::Ordering::SeqCst) >= 1);
    assert_eq!(
        run.review_model_identity,
        Some(ReviewModelIdentity {
            provider: "reviewmock".into(),
            model: "rev".into(),
        }),
        "the verdict must name the ACTUAL routed reviewer, not the parent pair"
    );
    assert!(
        run.criteria.iter().any(|c| c.passed),
        "the aggregate-goal criterion passes through the recorded review: {:?}",
        run.criteria
    );
}

#[tokio::test]
async fn non_risky_change_never_routes_a_review_phase_call() {
    // (b) P0-13: a NON-risky change (plain src + test) performs NO
    // review-model call (the routing spy sees zero Review-phase routes)
    // and completes with the local signals alone.
    let (manager, session, cas, snapshots, _dir) = snapshot_review_env(&[]);
    let script = vec![
        ScriptedResponse::ToolCall {
            id: "c1".into(),
            name: "write_file".into(),
            input: serde_json::json!({
                "path": "src/calc.rs",
                "content": "pub fn add(a: i32, b: i32) -> i32 {\n    let sum: i32 = a.checked_add(b).expect(\"overflow\");\n    sum.saturating_mul(2)\n}\n",
            }),
        },
        ScriptedResponse::ToolCall {
            id: "c2".into(),
            name: "write_file".into(),
            input: serde_json::json!({
                "path": "tests/calc.rs",
                "content": "#[test]\nfn adds() {\n    assert_eq!(add(1, 2), 3);\n    assert_eq!(add(0, 0), 0);\n}\n",
            }),
        },
        ScriptedResponse::Text("done".into()),
        ScriptedResponse::End,
    ];
    let (routing, review_routes) = PhasePinnedRouting::review_to("reviewmock", "rev");
    let reviewmock = RecordingProvider::new(mock_review_provider(r#"{"verdict":"block"}"#));
    let (deps, _d) = snapshot_review_deps(
        &manager,
        &snapshots,
        &cas,
        vec![Arc::new(scripted_provider(script)), reviewmock.clone()],
        vec![checkpoint_write_tool()],
        routing,
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let outcome = runtime
        .run_turn(session, "implement add with a test", &[])
        .await
        .unwrap();
    assert_eq!(
        outcome.completion,
        Some(CompletionGate::VerifiedComplete),
        "{outcome:?}"
    );
    let review = outcome.review.expect("review must run");
    assert_eq!(review["verdict"], "pass", "{review}");
    let structured = review_evidence_structured(&review);
    assert_eq!(structured["risk"]["level"], "low", "{structured}");
    assert_eq!(
        structured["review_model"]["attempted"],
        serde_json::json!(false),
        "no independent review for a low-risk change: {structured}"
    );
    assert_eq!(
        review_routes.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "a non-risky change must never route a Review-phase call"
    );
    assert!(
        reviewmock.requests().is_empty(),
        "the mock reviewer must never stream for a non-risky change"
    );
}

#[tokio::test]
async fn review_model_request_is_isolated_and_carries_the_diff_package() {
    // (c) P0-13 isolation + P0-12 row 1: the review call's wire request
    // carries ONLY the diff package + criteria (a malicious change at
    // line 900 — beyond the old 400-byte head scan — appears in it),
    // and NONE of the implementation context (a marker phrase that rode
    // the drive's own transcript is absent).
    let mut lines = String::new();
    for i in 0..900 {
        if i == 899 {
            lines.push_str("pub fn replaced() -> u32 { 0 }\n");
        } else {
            lines.push_str(&format!("pub fn f{i}() -> u32 {{ 1 }}\n"));
        }
    }
    let (manager, session, cas, snapshots, _dir) =
        snapshot_review_env(&[("src/unsafe_shim.rs", lines.as_str())]);
    // Turn 1: benign change (seeds the durable criteria row + a clean
    // transcript the review must NOT see).
    let t1 = vec![
        ScriptedResponse::ToolCall {
            id: "t1c1".into(),
            name: "write_file".into(),
            input: serde_json::json!({
                "path": "src/adder.rs",
                "content": "pub fn adder(a: u32, b: u32) -> u32 {\n    let base: u32 = 41;\n    base.saturating_add(a).saturating_mul(2).saturating_add(b)\n}\n",
            }),
        },
        ScriptedResponse::Text("done".into()),
        ScriptedResponse::End,
    ];
    let (t1_deps, _d1) = snapshot_review_deps(
        &manager,
        &snapshots,
        &cas,
        vec![Arc::new(scripted_provider(t1))],
        vec![checkpoint_write_tool()],
        crate::FixedRoutingPolicy::passthrough(),
    );
    let runtime1 = AgentRuntime::new(t1_deps).unwrap();
    let o1 = runtime1
        .run_turn(session, "add an adder", &[])
        .await
        .unwrap();
    assert_eq!(o1.completion, Some(CompletionGate::VerifiedComplete));
    drop(runtime1);

    // Turn 2: RISKY change (unsafe path) replacing line 900 with a
    // distinctive marker the old 400-char head scan can never see.
    let mut new_lines = String::new();
    for i in 0..900 {
        if i == 899 {
            new_lines.push_str("pub fn replaced() -> u32 { let base: u32 = 42; base.saturating_mul(2).saturating_add(1) } // FAKTOR_BEYOND_HEAD_900_7B2\n");
        } else {
            new_lines.push_str(&format!("pub fn f{i}() -> u32 {{ 1 }}\n"));
        }
    }
    let t2 = vec![
        ScriptedResponse::ToolCall {
            id: "t2c1".into(),
            name: "write_file".into(),
            input: serde_json::json!({
                "path": "src/unsafe_shim.rs",
                "content": new_lines,
            }),
        },
        ScriptedResponse::Text("done".into()),
        ScriptedResponse::End,
    ];
    let (routing, _calls) = PhasePinnedRouting::review_to("reviewmock", "rev");
    let fake_rec = RecordingProvider::new(Arc::new(scripted_provider(t2)));
    let review_rec =
        RecordingProvider::new(mock_review_provider(r#"{"verdict":"clean","findings":[]}"#));
    let (t2_deps, _d2) = snapshot_review_deps(
        &manager,
        &snapshots,
        &cas,
        vec![fake_rec.clone(), review_rec.clone()],
        vec![checkpoint_write_tool()],
        routing,
    );
    let runtime2 = AgentRuntime::new(t2_deps).unwrap();
    let o2 = runtime2
        .run_turn(
            session,
            "finish the unsafe shim: FAKTOR_REVIEW_ISOLATION_7F2",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        o2.completion,
        Some(CompletionGate::VerifiedComplete),
        "clean independent review + clean local signals complete"
    );
    drop(runtime2);

    let review_requests = review_rec.requests();
    assert_eq!(
        review_requests.len(),
        1,
        "exactly one review call for the risky change"
    );
    let rendered = rendered_request(&review_requests[0]);
    // The package + criteria ride the request ...
    assert!(
        rendered.contains("FAKTOR_BEYOND_HEAD_900_7B2"),
        "the line-900 change must appear in the diff package: {rendered:?}"
    );
    assert!(rendered.contains("src/unsafe_shim.rs"), "{rendered:?}");
    assert!(
        rendered.contains("required check: cargo check"),
        "{rendered:?}"
    );
    assert!(
            !rendered.contains("goal: gating task"),
            "the review package carries the typed criteria, not an automatic goal obligation: {rendered:?}"
        );
    // ... and NO implementation context does (the marker rode the drive
    // transcript of THIS very turn).
    assert!(
        !rendered.contains("FAKTOR_REVIEW_ISOLATION_7F2"),
        "the review must never receive the implementation context: {rendered:?}"
    );
    // Positive control: the marker DID ride the drive requests.
    let drive_rendered: Vec<String> = fake_rec.requests().iter().map(rendered_request).collect();
    assert!(
        drive_rendered
            .iter()
            .any(|r| r.contains("FAKTOR_REVIEW_ISOLATION_7F2")),
        "the drive transcript must carry the marker (control): {drive_rendered:?}"
    );
    // And the OLD head scan alone could not have seen line 900: the
    // legacy evidence lists the file clean while the structured hunks
    // carry the change.
    let review = o2.review.expect("review runs");
    let files = review["evidence"]["files"].as_array().unwrap();
    let shim = files
        .iter()
        .find(|f| f["path"] == "src/unsafe_shim.rs")
        .expect("changed file present in evidence");
    assert_eq!(shim["contains_todo"], serde_json::json!(false), "{shim}");
    let structured = review_evidence_structured(&review);
    assert!(
        structured["hunks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["path"] == "src/unsafe_shim.rs" && h["added_lines"].as_u64() >= Some(1)),
        "the structured hunks must report the beyond-head change: {structured}"
    );
}

#[tokio::test]
async fn risky_change_with_oversized_diff_never_reviewed_partially() {
    // (e) P0-12 row 5: a hostile giant diff (package beyond the 64 KiB
    // bound) is a HARD Oversized refusal — blocking reason, no partial
    // package, no model call with truncated content.
    let (manager, session, cas, snapshots, _dir) =
        snapshot_review_env(&[("src/security.rs", "pub fn old() -> u32 { 0 }\n")]);
    let giant = format!("pub fn giant() -> u32 {{ {} }}\n", "x".repeat(70_000));
    let script = vec![
        ScriptedResponse::ToolCall {
            id: "c1".into(),
            name: "write_file".into(),
            input: serde_json::json!({
                "path": "src/security.rs",
                "content": giant,
            }),
        },
        ScriptedResponse::Text("done".into()),
        ScriptedResponse::End,
    ];
    let (deps, _d) = snapshot_review_deps(
        &manager,
        &snapshots,
        &cas,
        vec![Arc::new(scripted_provider(script))],
        vec![checkpoint_write_tool()],
        crate::FixedRoutingPolicy::passthrough(),
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let outcome = runtime
        .run_turn(session, "harden the auth path", &[])
        .await
        .unwrap();
    let review = outcome.review.expect("review runs");
    assert_eq!(review["verdict"], "block", "{review}");
    let structured = review_evidence_structured(&review);
    assert!(
        structured["oversize"]
            .as_str()
            .is_some_and(|o| o.contains("byte bound") || o.contains("exceeds")),
        "the hard oversize refusal must be recorded: {structured}"
    );
    match outcome.completion {
        Some(CompletionGate::BlockedVerification { reasons }) => {
            assert!(
                reasons.iter().any(|r| {
                    r.code == ReasonCode::ReviewBlocked
                        && r.detail
                            .contains("independent review of a risky change could not run")
                }),
                "an oversized risky change must block, never partially review: {reasons:?}"
            );
        }
        other => panic!("oversized risky change must gate Blocked, got {other:?}"),
    }
}

#[tokio::test]
async fn ci_workflow_change_rides_the_package_and_triggers_independent_review() {
    // P0-12 row 3 + (f): a CI workflow change appears in
    // ci_build_files_changed and the inventory's ci_workflow_changes,
    // and classifies the change risky (independent review runs).
    let (manager, session, cas, snapshots, _dir) = snapshot_review_env(&[
            (
                ".github/workflows/ci.yml",
                "name: ci\non: [push]\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n",
            ),
        ]);
    let script = vec![
        ScriptedResponse::ToolCall {
            id: "c1".into(),
            name: "write_file".into(),
            input: serde_json::json!({
                "path": ".github/workflows/ci.yml",
                "content": "name: ci\non: [push]\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: actions/checkout@v4\n      - run: cargo test\n",
            }),
        },
        ScriptedResponse::Text("done".into()),
        ScriptedResponse::End,
    ];
    let (routing, _calls) = PhasePinnedRouting::review_to("reviewmock", "rev");
    let (deps, _d) = snapshot_review_deps(
        &manager,
        &snapshots,
        &cas,
        vec![
            Arc::new(scripted_provider(script)),
            mock_review_provider(r#"{"verdict":"clean","findings":[]}"#),
        ],
        vec![checkpoint_write_tool()],
        routing,
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let outcome = runtime
        .run_turn(session, "add the test step to CI", &[])
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    let review = outcome.review.clone().expect("review runs");
    let structured = review_evidence_structured(&review);
    assert_eq!(
        structured["ci_build_files_changed"][0], ".github/workflows/ci.yml",
        "{structured}"
    );
    assert_eq!(
        structured["inventory"]["ci_workflow_changes"][0], ".github/workflows/ci.yml",
        "{structured}"
    );
    assert_eq!(structured["risk"]["level"], "high", "{structured}");
    assert_eq!(
        structured["review_model"]["status"], "called",
        "{structured}"
    );
    assert_eq!(
        structured["review_model"]["verdict"], "clean",
        "{structured}"
    );
    // A CI-only change derives no language check: the gate is
    // Unverified (nothing objective ran) — the independent review still
    // ran and recorded its clean verdict on the change.
    assert_eq!(
        outcome.completion,
        Some(CompletionGate::Unverified),
        "a CI-only change derives no checks: {outcome:?}"
    );
}

#[tokio::test]
async fn deterministic_gate_feeds_verified_samples_keyed_by_provider_model_phase() {
    // Audit items 13/14/L fill site: the deterministic gate verdict of a
    // completion-claiming turn is the ONLY verified signal. A
    // VerifiedComplete gate re-records the turn's settled Implement
    // calls with verified_success=true (a success sample keyed by
    // provider/model/phase); a FailedVerification gate re-records the
    // SAME shape as FAILURE samples — no success is ever learned from a
    // turn that did not verify.
    let (manager, session, cas, snapshots, _dir) = snapshot_review_env(&[]);
    let outcomes: Arc<faktor_router::MemoryOutcomeStore> =
        Arc::new(faktor_router::MemoryOutcomeStore::new());
    let routing = outcome_wired_policy(outcomes.clone());
    let script = vec![
        ScriptedResponse::ToolCall {
            id: "c1".into(),
            name: "write_file".into(),
            input: serde_json::json!({
                "path": "src/calc.rs",
                "content": "pub fn add(a: i32, b: i32) -> i32 {\n    let sum: i32 = a.checked_add(b).expect(\"overflow\");\n    sum.saturating_mul(2)\n}\n",
            }),
        },
        ScriptedResponse::ToolCall {
            id: "c2".into(),
            name: "write_file".into(),
            input: serde_json::json!({
                "path": "tests/calc.rs",
                "content": "#[test]\nfn adds() {\n    assert_eq!(add(1, 2), 3);\n    assert_eq!(add(0, 0), 0);\n}\n",
            }),
        },
        ScriptedResponse::Text("done".into()),
        ScriptedResponse::End,
    ];
    // Turn 1: a mutating change whose deterministic verification PASSES.
    let (deps, _d) = snapshot_review_deps_full(
        &manager,
        &snapshots,
        &cas,
        vec![Arc::new(scripted_provider(script.clone()))],
        vec![checkpoint_write_tool()],
        routing.clone(),
        fake_ok(),
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let o1 = runtime
        .run_turn(session, "implement add with a test", &[])
        .await
        .unwrap();
    assert_eq!(o1.completion, Some(CompletionGate::VerifiedComplete));
    drop(runtime);
    let after_success = outcomes
        .phase_stats("fake", "m", RouterPhase::Implement)
        .expect("the verified turn recorded Implement samples");
    assert!(
        after_success.successes_first_pass >= 1,
        "the deterministic verified-success signal recorded success samples: {after_success:?}"
    );
    assert_eq!(
        after_success.failures_first_pass, 0,
        "a verified-complete turn never records failure samples: {after_success:?}"
    );
    assert_eq!(
        after_success.rework_cost_micro_sum, 0,
        "success samples never carry rework"
    );
    assert!(
        outcomes
            .phase_stats("fake", "m", RouterPhase::Review)
            .is_none(),
        "no Review samples without a review call"
    );
    let successes_before = after_success.successes_first_pass;

    // Turn 2: the SAME mutating shape but with a REAL change and
    // deterministic verification that FAILS: the gate records FAILURE
    // samples for the settled calls and never a success.
    let failing_script = vec![
        ScriptedResponse::ToolCall {
            id: "c1".into(),
            name: "write_file".into(),
            input: serde_json::json!({
                "path": "src/calc.rs",
                "content": "pub fn add(a: i32, b: i32) -> i32 {\n    let sum: i32 = a.checked_add(b).expect(\"overflow\");\n    sum.saturating_mul(3)\n}\n",
            }),
        },
        ScriptedResponse::Text("done".into()),
        ScriptedResponse::End,
    ];
    let failing = crate::VerificationService::fake(|_cmd| {
        Err("deterministic verification failed".to_string())
    });
    let (deps2, _d2) = snapshot_review_deps_full(
        &manager,
        &snapshots,
        &cas,
        vec![Arc::new(scripted_provider(failing_script))],
        vec![checkpoint_write_tool()],
        routing,
        failing,
    );
    let runtime2 = AgentRuntime::new(deps2).unwrap();
    let o2 = runtime2
        .run_turn(session, "implement add with a test again", &[])
        .await
        .unwrap();
    assert!(
        matches!(
            o2.completion,
            Some(CompletionGate::FailedVerification { .. })
        ),
        "{o2:?}"
    );
    drop(runtime2);
    let after_failure = outcomes
        .phase_stats("fake", "m", RouterPhase::Implement)
        .expect("samples still recorded");
    assert_eq!(
        after_failure.successes_first_pass, successes_before,
        "a failed verification never learns a success"
    );
    assert!(
        after_failure.failures_first_pass >= 1,
        "the failed gate recorded failure samples for the settled calls: {after_failure:?}"
    );
    assert_eq!(
        after_failure.sample_count,
        after_failure.successes_first_pass + after_failure.failures_first_pass
    );
}

#[tokio::test]
async fn semantic_high_risk_upgrades_review_strength_tier_and_parallelism() {
    // A REGISTERED provider reporting a high-blast-radius change forces
    // the independent review call even though the path heuristic sees a
    // benign `src/change_*.rs` file — and the review route carries the
    // ESCALATED quality floor through the existing routing request.
    let (manager, session, cas, snapshots, _dir) = snapshot_review_env(&[]);
    let script = vec![
        ScriptedResponse::ToolCall {
            id: "c1".into(),
            name: "write_file".into(),
            input: serde_json::json!({
                "path": "src/change_00.rs",
                "content": "pub fn changed() -> u32 { 42 }\n"
            }),
        },
        ScriptedResponse::Text("done".into()),
        ScriptedResponse::End,
    ];
    let routing = ReviewSpyRouting::pinned("reviewmock", "rev");
    let (mut deps, _d) = snapshot_review_deps(
        &manager,
        &snapshots,
        &cas,
        vec![
            Arc::new(scripted_provider(script)),
            mock_review_provider(r#"{"verdict":"clean","findings":[]}"#),
        ],
        vec![checkpoint_write_tool()],
        Arc::new(routing.clone()),
    );
    deps.semantic = semantic_registry_with(FakeSemanticProvider::affected(high_risk_paths()));
    let runtime = AgentRuntime::new(deps).unwrap();
    let outcome = runtime
        .run_turn(session, "make the change", &[])
        .await
        .unwrap();
    assert_eq!(outcome.semantic_risk, Some(RiskLevel::High));
    let review = outcome.review.expect("review must run");
    let structured = review_evidence_structured(&review);
    assert_eq!(structured["semantic"]["risk"], "high", "{structured}");
    assert_eq!(
        structured["review_model"]["status"], "called",
        "semantic high risk must force the independent review: {structured}"
    );
    assert_eq!(
        structured["review_model"]["semantic_risk"], "high",
        "{structured}"
    );
    let evidence = structured["semantic"]["evidence"]
        .as_str()
        .expect("rendered DATA block");
    assert!(
        evidence.starts_with("[evidence:data]") && evidence.ends_with("[/evidence:data]"),
        "provider output must ride as DATA: {evidence}"
    );
    let requests = routing.requests.lock().unwrap();
    let review_req = requests
        .iter()
        .find(|r| r.phase == RouterPhase::Review)
        .expect("the review call must be routed");
    assert_eq!(review_req.quality_floor, REVIEW_ESCALATED_QUALITY_FLOOR);
    assert!(
        review_req.quality_floor > crate::ModelCallIntent::review().quality_floor(),
        "escalation must raise the floor"
    );
}

/// (1) Review: a RISKY change drives the independent review call; the
/// routed request carries the Review role and its configured reserve.
#[tokio::test]
async fn model_call_intent_tripwire_review_is_the_real_role() {
    let (manager, session, cas, snapshots, _dir) = snapshot_review_env(&[]);
    let script = vec![
        ScriptedResponse::ToolCall {
            id: "c1".into(),
            name: "write_file".into(),
            input: serde_json::json!({
                "path": "src/security.rs",
                "content": "pub fn authenticate(user: u32, secret: u32) -> u32 {\n    user.checked_add(secret).unwrap_or(0)\n}\n",
            }),
        },
        ScriptedResponse::Text("done".into()),
        ScriptedResponse::End,
    ];
    let routing = RecordingRouter::pin_review("reviewmock", "rev");
    let (deps, _d) = snapshot_review_deps(
        &manager,
        &snapshots,
        &cas,
        vec![
            Arc::new(scripted_provider(script)),
            mock_review_provider(r#"{"verdict":"block","findings":["reviewed"]}"#),
        ],
        vec![checkpoint_write_tool()],
        routing.clone(),
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let outcome = runtime
        .run_turn(session, "harden the auth path", &[])
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    let requests = routing.requests();
    let review: Vec<&faktor_router::RouteRequest> = requests
        .iter()
        .filter(|r| r.phase == RouterPhase::Review)
        .collect();
    assert_eq!(review.len(), 1, "exactly one Review-phase consult");
    assert_eq!(
        review[0].estimated_output_tokens, 2048,
        "review output tokens == the configured verdict reserve"
    );
    assert!(
        review[0].context_tokens > 0 && review[0].context_tokens <= 33_792,
        "the review consult carries the bounded package estimate: {}",
        review[0].context_tokens
    );
    assert_eq!(review[0].quality_floor, 60);
}

/// Adversarial unit pin of the marker session-identity classifier: only
/// a numeric id that names THIS session is replayable; every absent,
/// mistyped, negative, zero, fractional or out-of-range shape is
/// Malformed (never "mine").
#[test]
fn marker_session_verdict_rejects_every_untrusted_shape() {
    let ours = 42u64;
    assert_eq!(
        marker_session_verdict(&serde_json::json!({ "session": 42u64 }), ours),
        MarkerSession::Ours
    );
    assert_eq!(
        marker_session_verdict(&serde_json::json!({ "session": u64::MAX }), u64::MAX),
        MarkerSession::Ours,
        "the id high half round-trips"
    );
    assert_eq!(
        marker_session_verdict(&serde_json::json!({ "session": 43 }), ours),
        MarkerSession::Foreign(43)
    );
    for hostile in [
        serde_json::json!({}),
        serde_json::json!({ "session": "42" }),
        serde_json::json!({ "session": "not-a-session" }),
        serde_json::json!({ "session": -42 }),
        serde_json::json!({ "session": -1 }),
        serde_json::json!({ "session": 0 }),
        serde_json::json!({ "session": 1.5 }),
        serde_json::json!({ "session": null }),
        serde_json::json!({ "session": [42] }),
        serde_json::json!({ "session": { "id": 42 } }),
        serde_json::json!({ "session": 1.8446744073709552e19f64 }),
    ] {
        assert_eq!(
            marker_session_verdict(&hostile, ours),
            MarkerSession::Malformed,
            "hostile session shape must never be treated as ours: {hostile}"
        );
    }
}

/// F5: `CancelVerificationAttempt` replay is deduped by the attempt's own
/// job rows — an open job is cancelled exactly once, and a stale marker
/// (the crash-after-commit window) is skipped, never a second spurious
/// cancellation.
#[tokio::test]
async fn cancel_verification_attempt_replay_dedups_on_open_jobs() {
    let (deps, _dir) = deps(scripted_provider(vec![]), vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let manager = runtime.deps().session.clone();
    let session = new_session(runtime.deps());
    let handle = manager.get_session(session).unwrap().unwrap();
    let task = handle.task_id().unwrap();
    let op = manager.try_next_op_id().unwrap().raw();
    let check = |id: &str| faktor_session::VerificationAttemptCheck {
        check_id: id.into(),
        command: format!("make {id}"),
        inline: None,
    };
    let job = |id: &str| faktor_session::VerificationJobInput {
        check_id: id.into(),
        kind: "test".into(),
        command: format!("make {id}"),
        program: "make".into(),
        args: vec![id.into()],
        spec_json: "{}".into(),
        budget_ms: 10_000,
    };
    handle
        .begin_verification_attempt(
            task.raw(),
            1,
            op,
            "/w",
            &[],
            &[check("make_test")],
            &[job("make_test")],
        )
        .unwrap();
    let root = manager.store().root().to_path_buf();
    let marker_dir = root.join(DURABLE_WRITE_MARKER_DIR);
    let marker = |note: &str| {
        serde_json::json!({
            "status": "pending",
            "attempts": 0,
            "site": "test.dedup",
            "session": session.raw(),
            "at_ms": 1,
            "intent": {
                "write": "cancel_verification_attempt",
                "task_id": task.raw(),
                "attempt_op": op,
                "note": note,
            },
        })
    };
    write_raw_marker(&marker_dir, "dw-cancel-1.json", &marker("first"));
    runtime.replay_durable_write_failures(&handle);
    let jobs = handle.verification_attempt_jobs(task.raw(), op).unwrap();
    assert!(
        jobs.iter().all(|job| !job.state.is_open()),
        "the open job is cancelled: {jobs:?}"
    );
    // The crash-window duplicate: every job is terminal, so the replay
    // must skip the second cancellation and consume the marker.
    write_raw_marker(&marker_dir, "dw-cancel-2.json", &marker("second"));
    runtime.replay_durable_write_failures(&handle);
    let after = handle.verification_attempt_jobs(task.raw(), op).unwrap();
    assert!(after.iter().all(|job| !job.state.is_open()), "{after:?}");
    assert!(
        marker_files(&root)
            .iter()
            .all(|path| !path.to_string_lossy().contains("cancel")),
        "both markers are consumed"
    );
}

#[tokio::test]
async fn completion_gate_refusal_rows_lost_are_marked_and_replayed() {
    // Adversarial: the durable budget refusal mirrors (task_state fact +
    // typed ledger decision) are written AFTER TurnCompleted. Both writes
    // fail; the turn must still report the BlockedVerification outcome
    // (never an Err), and the reopen must replay both mirror writes.
    let (manager, session, dir) = verified_shared_env();
    let h = manager.get_session(session).unwrap().unwrap();
    h.record_provider_call(
        OpId::new(4242),
        "fake",
        "m",
        "completed",
        Some(500),
        Some(200),
        None,
    )
    .unwrap();
    assert_eq!(h.spent_tokens().unwrap(), 700);
    let now = h.now_ms();
    h.create_task(Task {
        task_id: h.task_id().unwrap(),
        session_id: session,
        goal: "gating task".into(),
        acceptance_criteria: vec![],
        plan: vec![],
        attachments: Vec::new(),
        budget: faktor_session::TaskBudget {
            max_tokens: Some(10),
            max_turns: None,
            spent_tokens: 700,
            spent_turns: 0,
        },
        state: TaskState::Running,
        created_ms: now,
        updated_ms: now,
    })
    .unwrap();
    let (turn_deps, _d) = verified_turn_deps(
        &manager,
        vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/a.rs", "content": "pub fn a() -> u32 {\n    let base: u32 = 10;\n    let step: u32 = 41;\n    base.saturating_add(step).saturating_add(1)\n}\n"}),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
        fake_ok(),
        0.65,
    );
    let runtime = AgentRuntime::new(turn_deps).unwrap();
    // Real store corruption for the decision write (an aborting trigger on
    // decision inserts); the injected fault covers the fact write. Both
    // markers must be durable and the completed turn must stay Ok.
    manager
        .store()
        .sql_execute(
            "CREATE TRIGGER dw_test_fail_ledger_decision BEFORE INSERT ON ledger_entry \
                 WHEN NEW.entry_type = 'decision' \
                 BEGIN SELECT RAISE(ABORT, 'injected ledger corruption'); END",
        )
        .unwrap();
    durable_faults_tests::arm(
        runtime.deps().session.store().root(),
        DW_SITE_GATE_BUDGET_FACT,
    );
    let outcome = runtime
        .run_turn(session, "write src/a.rs", &[])
        .await
        .expect("a lost gate mirror must not fail the completed turn");
    assert!(matches!(
        outcome.completion,
        Some(CompletionGate::BlockedVerification { .. })
    ));
    let root = manager.store().root().to_path_buf();
    let mut sites = marker_sites(&root);
    sites.sort();
    let mut expected = vec![
        DW_SITE_GATE_BUDGET_FACT.to_string(),
        DW_SITE_GATE_BUDGET_DECISION.to_string(),
    ];
    expected.sort();
    assert_eq!(sites, expected);
    manager
        .store()
        .sql_execute("DROP TRIGGER dw_test_fail_ledger_decision")
        .unwrap();
    drop(runtime);
    drop(manager);
    let manager2 = reopen_manager(&dir);
    let (deps2, _d2) = verified_turn_deps(&manager2, vec![ScriptedResponse::End], fake_ok(), 0.65);
    AgentRuntime::new(deps2).unwrap().recover().unwrap();
    let h2 = manager2.get_session(session).unwrap().unwrap();
    let facts = h2.memory_facts().unwrap();
    assert!(
        facts
            .iter()
            .any(|(kind, key, value)| kind == "task_state" && key == "state" && value == "blocked"),
        "the blocked task_state fact was not replayed: {facts:?}"
    );
    let decisions = manager2
        .store()
        .ledger_entries_desc(session, None, 64)
        .unwrap();
    assert!(
        decisions.iter().any(|row| {
            row.payload.get("kind").and_then(|k| k.as_str()) == Some("decision")
                && row
                    .payload
                    .get("rationale")
                    .and_then(|r| r.as_str())
                    .is_some_and(|r| r.contains("durable task budget exhausted"))
        }),
        "the budget refusal decision was not replayed: {decisions:?}"
    );
    assert!(marker_files(manager2.store().root()).is_empty());
}

#[tokio::test]
async fn verification_job_resolve_loss_is_marked_and_reconstructed_on_reopen() {
    // Adversarial: the executor's resolve CAS loses its durable write; the
    // job must stay open (never a silent pass/fail), the marker must be
    // durable, and the reopen replay must land the intended resolution.
    if !std::process::Command::new("make")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        eprintln!("skipping resolve-replay test: no make on this host");
        return;
    }
    let (manager, session, dir) = make_background_env("\t@true\n", None);
    let root = dir.path().join("ws");
    let (mut deps, _d) = deps_sharing_session(
        manager.clone(),
        Arc::new(scripted_provider(vec![ScriptedResponse::End])),
        vec![real_write_tool()],
    );
    deps.verification = real_background_verifier();
    let runtime = AgentRuntime::new(deps).unwrap();
    let h = manager.get_session(session).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    let now = h.now_ms();
    h.create_task(Task {
        task_id,
        session_id: session,
        goal: "bg".into(),
        acceptance_criteria: vec![],
        plan: vec![],
        attachments: Vec::new(),
        budget: Default::default(),
        state: TaskState::Running,
        created_ms: now,
        updated_ms: now,
    })
    .unwrap();
    let attempt = runtime
        .verify_integrated_root_attempt(
            &h,
            &root,
            &["src/main.c".to_string()],
            &[],
            &CancellationToken::new(),
            manager.try_next_op_id().unwrap().raw(),
        )
        .await
        .unwrap();
    assert!(attempt.pending);
    durable_faults_tests::arm(runtime.deps().session.store().root(), DW_SITE_JOB_RESOLVE);
    assert_eq!(runtime.execute_open_verification_jobs(&h).await.unwrap(), 1);
    let rows = h
        .verification_attempt_jobs(task_id.raw(), attempt.attempt_op)
        .unwrap();
    assert!(
        rows.iter().any(|job| job.state.is_open()),
        "the lost resolve left the job open: {rows:?}"
    );
    let root_dir = manager.store().root().to_path_buf();
    assert_eq!(
        marker_sites(&root_dir),
        vec![DW_SITE_JOB_RESOLVE.to_string()]
    );
    drop(runtime);
    drop(manager);
    let manager2 = reopen_manager(&dir);
    let (mut deps2, _d2) = deps_sharing_session(
        manager2.clone(),
        Arc::new(scripted_provider(vec![ScriptedResponse::End])),
        vec![real_write_tool()],
    );
    deps2.verification = real_background_verifier();
    AgentRuntime::new(deps2).unwrap().recover().unwrap();
    let h2 = manager2.get_session(session).unwrap().unwrap();
    let rows = h2
        .verification_attempt_jobs(task_id.raw(), attempt.attempt_op)
        .unwrap();
    assert!(
        rows.iter().any(|job| job.check_id == "make_test"
            && job.state == faktor_session::VerificationJobState::Passed),
        "the resolve was not reconstructed: {rows:?}"
    );
    assert!(marker_files(manager2.store().root()).is_empty());
}

/// Audit item 3: verification evidence is a TYPED admission. Only a
/// VerificationOpinion model output can author review evidence; a
/// context-compression summary or an ephemeral output is refused typed,
/// and a durable row is only admitted when it carries an evidence-authoring
/// provenance tag.
#[test]
fn review_evidence_admits_only_verification_opinion_and_tagged_durable_rows() {
    let compaction = ModelOutput::new("summary", OutputTrust::ContextCompression, 41);
    let ephemeral = ModelOutput::new("title", OutputTrust::Ephemeral, 42);
    let opinion = ModelOutput::new(
        "{\"verdict\":\"pass\"}",
        OutputTrust::VerificationOpinion,
        43,
    );
    let implementation = ModelOutput::new("patch", OutputTrust::Implementation, 44);

    // Negative: compression/ephemeral/implementation can never author
    // review evidence.
    for refused in [&compaction, &ephemeral, &implementation] {
        let err = ReviewEvidence::from_review_output(refused, serde_json::json!({}))
            .expect_err("only a verification opinion may author review evidence");
        assert_eq!(err.trust, refused.trust);
        assert!(err.to_string().contains(refused.trust.provenance_tag()));
    }
    // Positive: the opinion is admitted and the provenance is stamped so a
    // durable round-trip can re-admit it.
    let admitted =
        ReviewEvidence::from_review_output(&opinion, serde_json::json!({"verdict":"pass"}))
            .expect("verification opinion is evidence");
    assert_eq!(
        admitted
            .as_value()
            .get(ReviewEvidence::OUTPUT_TRUST_KEY)
            .and_then(|v| v.as_str()),
        Some("verification_opinion")
    );
    assert_eq!(
        admitted
            .as_value()
            .get("output_call_id")
            .and_then(|v| v.as_u64()),
        Some(43)
    );
    let durable = ReviewEvidence::from_durable(admitted.clone().into_value())
        .expect("stamped durable row re-admits");
    assert_eq!(durable.as_value(), admitted.as_value());

    // Durable negatives: untagged rows and compression-tagged rows refuse.
    assert!(ReviewEvidence::from_durable(serde_json::json!({"verdict":"pass"})).is_err());
    assert!(ReviewEvidence::from_durable(serde_json::json!({
        ReviewEvidence::OUTPUT_TRUST_KEY: "context_compression",
        "verdict": "pass",
    }))
    .is_err());
    assert!(ReviewEvidence::from_durable(serde_json::json!({
        ReviewEvidence::OUTPUT_TRUST_KEY: "ephemeral",
    }))
    .is_err());

    // Deterministic-local evidence is admitted only when it does not claim
    // a reviewer attempt; a claimed reviewer needs the admitted output.
    let local = ReviewEvidence::from_deterministic_local(serde_json::json!({
        "verdict": "pass",
        "evidence": {"structured": {"review_model": {"attempted": false}}},
    }))
    .expect("local evidence without a reviewer attempt is admitted");
    assert_eq!(
        local
            .as_value()
            .get(ReviewEvidence::OUTPUT_TRUST_KEY)
            .and_then(|v| v.as_str()),
        Some(ReviewEvidence::DETERMINISTIC_LOCAL_TAG)
    );
    assert!(ReviewEvidence::from_deterministic_local(serde_json::json!({
        "verdict": "pass",
        "evidence": {"structured": {"review_model": {"attempted": true}}},
    }))
    .is_err());
    assert!(ReviewEvidence::from_durable(serde_json::json!({
        ReviewEvidence::OUTPUT_TRUST_KEY: ReviewEvidence::DETERMINISTIC_LOCAL_TAG,
        "evidence": {"structured": {"review_model": {"attempted": true}}},
    }))
    .is_err());

    // The raw-admission helper drops untrusted values (findings-only), so a
    // compaction summary can never back criterion verdicts.
    assert!(admit_review_evidence(Some(&serde_json::json!({"verdict":"pass"}))).is_none());
    assert!(admit_review_evidence(Some(admitted.as_value())).is_some());
}
