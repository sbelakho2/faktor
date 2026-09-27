//! `runtime::turn::drive_tests`: out-of-line tests.

#![allow(unused_imports)]

use super::*;
use crate::runtime::fixtures_tests::*;
use crate::runtime::tests::*;
use crate::*;

#[test]
fn canonical_usage_folds_into_recorded_input_total_without_double_billing() {
    // Audit Phase-1 item C: adapters split cache lines off the uncached
    // input counter at their boundary, so the recorded row input total
    // is the sum of all input-category tokens — never the uncached
    // counter PLUS cache lines that were subtracted twice, and never a
    // provider-total that still CONTAINS the cache lines.
    // openai-style: prompt total 1000 INCLUDED its 600 cached tokens —
    // the canonical frame arrives split (400 uncached + 600 cache
    // reads); the row fold is 1000, the frame is NOT 1000+600.
    let openai_style = CanonicalUsage {
        uncached_input_tokens: 400,
        cache_read_tokens: 600,
        cache_write_tokens: 0,
        output_tokens: 50,
        ..CanonicalUsage::ZERO
    };
    assert_eq!(recorded_input_total(&openai_style), 1000);
    // anthropic-style: input_tokens EXCLUDES cache reads and writes —
    // the frame arrives split (100 uncached + 900 reads + 1500
    // writes); every input-category token folds into the row.
    let anthropic_style = CanonicalUsage {
        uncached_input_tokens: 100,
        cache_read_tokens: 900,
        cache_write_tokens: 1500,
        output_tokens: 50,
        ..CanonicalUsage::ZERO
    };
    assert_eq!(recorded_input_total(&anthropic_style), 2500);
    // Cache-heavy without any uncached input: the cache lines alone
    // must never zero the recorded row.
    let cache_only = CanonicalUsage {
        uncached_input_tokens: 0,
        cache_read_tokens: 900,
        cache_write_tokens: 100,
        output_tokens: 7,
        ..CanonicalUsage::ZERO
    };
    assert_eq!(recorded_input_total(&cache_only), 1000);
    assert_eq!(recorded_input_total(&CanonicalUsage::ZERO), 0);
    // The authoritative reported-cost gate: USD passes, anything else
    // is refused (the runtime keeps the override rule).
    let usd = ReportedCost::usd(123, faktor_provider::ReportedCostSource::ProviderUsage);
    assert_eq!(authoritative_reported_micro(&usd), Some(123));
    let eur = ReportedCost {
        micro_usd: 999,
        currency: ReportedCurrency::Other { code: "eur".into() },
        source: faktor_provider::ReportedCostSource::ProviderUsage,
        request_id: None,
    };
    assert_eq!(authoritative_reported_micro(&eur), None);
}

#[tokio::test]
async fn text_only_turn_completes() {
    let (deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::Text("hello there".into()),
            ScriptedResponse::End,
        ]),
        vec![],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let outcome = runtime.run_turn(session, "hi", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_eq!(outcome.turns, 1);
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let page = handle.messages_page(None, 10).unwrap();
    let texts: Vec<&String> = page
        .messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            faktor_protocol::native::Part::Text { text } => Some(text),
            _ => None,
        })
        .collect();
    assert!(texts.iter().any(|t| t.contains("hello there")));
}

#[tokio::test]
async fn two_denied_one_approved_batch_continues_lawfully() {
    // Two denied calls + one approved sibling: every denial keeps the
    // batch executing, the approved call runs once, and all three calls
    // are answered (the two denials typed, the approval with its output).
    let approved_execs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let denied_execs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (runtime, recorder, session, _dir) = mixed_permission_batch(
        vec![
            (
                "refused_1".into(),
                "write_file".into(),
                serde_json::json!({"path": "a.txt", "content": "a"}),
            ),
            (
                "refused_2".into(),
                "write_file".into(),
                serde_json::json!({"path": "b.txt", "content": "b"}),
            ),
            ("ok".into(), "echo".into(), serde_json::json!({"x": 3})),
        ],
        &["write_file"],
        vec![
            counting_echo_tool(approved_execs.clone()),
            counting_write_tool(denied_execs.clone()),
        ],
    );
    let outcome = runtime
        .run_turn(session, "use all three", &[])
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_eq!(approved_execs.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(denied_execs.load(std::sync::atomic::Ordering::SeqCst), 0);
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    assert!(dangling_tool_calls(&handle).is_empty());
    let (approved, exit) = tool_result_for(&handle, "ok").expect("approved result");
    assert_eq!(exit, Some(0));
    assert_eq!(approved, "echo: {\"x\":3}");
    assert_permission_denial(&handle, "refused_1", "write_file");
    assert_permission_denial(&handle, "refused_2", "write_file");
    assert_eq!(
        handle
            .events_range(1, None)
            .unwrap()
            .iter()
            .filter(|e| e.kind == faktor_core::event::EventKind::PermissionDenied)
            .count(),
        2
    );
    assert_eq!(recorder.requests().len(), 2);
    assert_eq!(
        wire_tool_results(&recorder),
        vec![
            ("ok".to_string(), false),
            ("refused_1".to_string(), true),
            ("refused_2".to_string(), true)
        ],
    );
}

#[tokio::test]
async fn mixed_approved_and_secret_denied_batch_continues_with_both_results_on_the_wire() {
    // Adversarial mixed batch on the CONTINUING path: one call is
    // approved and executes, its sibling is refused by the secret gate.
    // The turn continues (executed > 0) and the next wire request must
    // carry BOTH results — the real output for the approved call and the
    // typed denial for the refused one — so the model never sees a
    // dangling call.
    let approved_execs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let denied_execs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let fake = scripted_provider(vec![
        ScriptedResponse::ToolCall {
            id: "ok".into(),
            name: "echo".into(),
            input: serde_json::json!({"x": 1}),
        },
        ScriptedResponse::ToolCall {
            id: "refused".into(),
            name: "write_file".into(),
            input: serde_json::json!({"path": "creds.txt", "content": SK_SAMPLE}),
        },
        ScriptedResponse::Text("done".into()),
        ScriptedResponse::End,
    ]);
    let recorder = RecordingProvider::new(Arc::new(fake));
    let (deps, _dir) = deps_with(
        recorder.clone(),
        vec![
            counting_echo_tool(approved_execs.clone()),
            counting_write_tool(denied_execs.clone()),
        ],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let outcome = runtime.run_turn(session, "use both", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_eq!(approved_execs.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        denied_execs.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the refused sibling must never execute"
    );
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    assert!(dangling_tool_calls(&handle).is_empty());
    let (approved, approved_exit) = tool_result_for(&handle, "ok").expect("approved result");
    assert_eq!(approved_exit, Some(0));
    assert_eq!(approved, "echo: {\"x\":1}");
    let (denied, denied_exit) = tool_result_for(&handle, "refused").expect("denial result");
    assert_eq!(denied_exit, Some(1));
    assert!(
        denied.contains("tool call denied (secret_detected)"),
        "{denied}"
    );
    assert!(!denied.contains(SK_SAMPLE), "no secret echo: {denied}");
    // The continuing turn's SECOND request carries both answers.
    let requests = recorder.requests();
    assert_eq!(requests.len(), 2, "the mixed batch continues the same turn");
    let mut seen: Vec<(String, bool)> = Vec::new();
    for m in &requests[1].messages {
        for p in &m.content {
            if let ContentKind::ToolResult { is_error, .. } = &p.kind {
                seen.push((p.tool_call_id.clone().unwrap_or_default(), *is_error));
            }
        }
    }
    seen.sort();
    assert_eq!(
        seen,
        vec![("ok".to_string(), false), ("refused".to_string(), true)],
        "both calls answered on the wire, the denial marked as an error"
    );
}

#[tokio::test]
async fn one_logical_turn_has_exactly_one_turn_completed_and_no_mid_turn_ready() {
    // Audit round 6 P0: a turn with TWO tool batches must journal exactly
    // ONE TurnCompleted and must never enter ReadyForNextTurn between
    // the batches.
    let (deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "echo".into(),
                input: serde_json::json!({"x": 1}),
            },
            ScriptedResponse::ToolCall {
                id: "c2".into(),
                name: "echo".into(),
                input: serde_json::json!({"x": 2}),
            },
            ScriptedResponse::Text("final answer".into()),
            ScriptedResponse::End,
        ]),
        vec![echo_tool()],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let outcome = runtime.run_turn(session, "do work", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_eq!(
        outcome.turns, 1,
        "one logical turn despite two tool batches"
    );
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let events = handle.events_range(1, None).unwrap();
    let turn_completed = events
        .iter()
        .filter(|e| e.kind == faktor_core::event::EventKind::TurnCompleted)
        .count();
    assert_eq!(
        turn_completed, 1,
        "exactly one TurnCompleted per logical turn"
    );
    // ReadyForNextTurn must appear in the journal EXACTLY ONCE (the end).
    let ready = events
        .iter()
        .filter(|e| e.state == AgentState::ReadyForNextTurn)
        .count();
    assert_eq!(ready, 1, "ReadyForNextTurn only at the genuine end");
    // The interior tool batches used PhaseChanged hops (never TurnCompleted).
    let interior = events
        .iter()
        .filter(|e| e.kind == faktor_core::event::EventKind::PhaseChanged)
        .count();
    assert!(interior >= 2, "interior hops must use PhaseChanged");
}

#[tokio::test]
async fn mid_turn_crash_resumes_the_same_logical_turn() {
    // Crash AFTER the first tool batch: the journal ends at
    // WaitingForModel (interior hop). continue_turn must resume the SAME
    // logical turn: no second PromptReceived, and the model sees the
    // tool result in request #1 of the resumed turn.
    let (deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "echo".into(),
                input: serde_json::json!({"x": 1}),
            },
            ScriptedResponse::End,
        ]),
        vec![echo_tool()],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let receipt = handle.submit_prompt("crash me", &[]).unwrap();
    let outcome = runtime
        .drive_turn(
            &handle,
            receipt.op_id,
            receipt.op_meta.cancellation.clone(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        outcome.final_state,
        AgentState::ReadyForNextTurn,
        "turn ran to completion with a single batch"
    );
    // The crash happens BEFORE the model continuation? Simulate by
    // ending the provider script: first stream consumed the ToolCall;
    // second stream (continuation) has no script → Done → turn ends.
    let events = handle.events_range(1, None).unwrap();
    let prompt_events = events
        .iter()
        .filter(|e| e.kind == faktor_core::event::EventKind::PromptReceived)
        .count();
    assert_eq!(prompt_events, 1, "one prompt for the whole logical turn");
    let turn_completed = events
        .iter()
        .filter(|e| e.kind == faktor_core::event::EventKind::TurnCompleted)
        .count();
    assert_eq!(turn_completed, 1);
}

#[tokio::test]
async fn full_check_becomes_a_durable_job_and_never_holds_the_turn_inline() {
    // Adversarial (audit P0-5/26 test a): a LONG Full-category check
    // (`make test` = real sleep through the supervisor) must NOT hold
    // the mutating turn inline beyond the quick budget. The turn
    // returns with the task parked at Verifying and a Queued job row;
    // a later text turn settles the job and completion proceeds from
    // the job results — records included.
    if !std::process::Command::new("make")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        eprintln!("skipping full_check background job: no make on this host");
        return;
    }
    let (manager, session, _dir) =
        make_background_env("\tsleep 3\n\techo ran > test-marker.txt\n", None);
    let (mut deps, _d) = deps_sharing_session(
        manager.clone(),
        Arc::new(scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/main.c", "content": "int main(void) {\n    int base = 40;\n    int step = 2;\n    printf(\"%d\\n\", base + step);\n    return 0;\n}\n"}),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ])),
        vec![real_write_tool()],
    );
    deps.verification = real_background_verifier();
    let runtime = AgentRuntime::new(deps).unwrap();
    let outcome = runtime
        .run_turn(session, "change main.c", &[])
        .await
        .unwrap();
    // The sleep-3 Full check never ran inline: the turn returns while
    // the job is Queued and no marker exists (nothing executed it). The
    // proof is the non-terminal durable row + absent marker below —
    // never a wall-clock guess.
    assert_eq!(
        outcome.completion,
        Some(CompletionGate::VerificationPending),
        "the gate is the honest mid-flight state"
    );
    assert_eq!(
        outcome.verification,
        vec![("make_build".to_string(), true)],
        "only the quick inline check ran: {:?}",
        outcome.verification
    );
    assert_eq!(outcome.acceptance, Some(faktor_verify::Acceptance::Pending));
    let root = manager
        .resolve_workspace_root(session)
        .unwrap()
        .expect("workspace root");
    assert!(
        !root.join("test-marker.txt").exists(),
        "the long check did not run during the mutating turn"
    );
    let h = manager.get_session(session).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    let jobs = h.open_verification_jobs(task_id.raw()).unwrap();
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert_eq!(jobs[0].check_id, "make_test");
    assert!(
        jobs[0].state == faktor_session::VerificationJobState::Queued,
        "the job is durable and waits for an executor: {:?}",
        jobs[0].state
    );
    let facts = h.memory_facts().unwrap();
    assert!(
        facts
            .iter()
            .any(|(k, key, v)| k == "task_state" && key == "state" && v == "verifying"),
        "the task fact records Verifying: {facts:?}"
    );
    let task = h.get_task(task_id).unwrap().unwrap();
    assert_eq!(
        task.state,
        TaskState::Verifying,
        "the durable row parks at Verifying until the jobs settle"
    );
    // ---- a later TEXT turn settles the job and completes ----
    let (mut deps2, _d2) = deps_sharing_session(
        manager.clone(),
        Arc::new(scripted_provider(vec![
            ScriptedResponse::Text("status?".into()),
            ScriptedResponse::End,
        ])),
        vec![real_write_tool()],
    );
    deps2.verification = real_background_verifier();
    let runtime2 = AgentRuntime::new(deps2).unwrap();
    let settled = runtime2
        .run_turn(session, "what is the status?", &[])
        .await
        .unwrap();
    assert_eq!(
        settled.completion,
        Some(CompletionGate::VerifiedComplete),
        "the settled job set completes the claim: {settled:?}"
    );
    assert_eq!(settled.acceptance, Some(faktor_verify::Acceptance::Pass));
    assert_eq!(
        settled.verification,
        vec![
            ("make_build".to_string(), true),
            ("make_test".to_string(), true)
        ],
        "results rebuilt from the inline evidence + job result: {:?}",
        settled.verification
    );
    let h2 = manager.get_session(session).unwrap().unwrap();
    let jobs = h2.verification_attempt_jobs(
        task_id.raw(),
        h2.current_verification_attempt(task_id.raw())
            .unwrap()
            .unwrap()
            .op_id,
    );
    let job_rows = jobs.unwrap();
    assert_eq!(job_rows.len(), 1);
    assert_eq!(
        job_rows[0].state,
        faktor_session::VerificationJobState::Passed
    );
    let facts = h2.memory_facts().unwrap();
    assert!(
        facts
            .iter()
            .any(|(k, key, v)| k == "task_state" && key == "state" && v == "verified_complete"),
        "the task completes only through the settled jobs: {facts:?}"
    );
    let task = h2.get_task(task_id).unwrap().unwrap();
    assert_eq!(task.state, TaskState::VerifiedComplete);
    let records = h2.list_verification_records(task_id).unwrap();
    assert_eq!(records.len(), 1, "one durable record from the job results");
    assert_eq!(
        records[0].status,
        VerificationStatus::Passed,
        "the record certifies the settled attempt"
    );
    let row = records[0]
        .checks
        .iter()
        .find(|c| c.check == "make_test")
        .expect("the job execution row rides the record");
    assert_eq!(row.program, "make", "{row:?}");
    // The marker proves the job ran through a real spawned process at
    // settlement time — not during the mutating turn.
    let marker = root.join("test-marker.txt");
    assert_eq!(
        std::fs::read_to_string(&marker)
            .expect("the job really executed at settlement")
            .trim(),
        "ran"
    );
}

#[tokio::test]
async fn verified_completion_lands_durable_passing_record_that_survives_reopen() {
    // Adversarial happy path (P0-8): a mutating turn whose required
    // check passes must land EXACTLY ONE durable VerificationRecord
    // certifying the completion — status Passed, checks mirroring the
    // executed runs, criterion verdicts covering every acceptance-
    // criteria entry of the row (the completion coverage contract) and
    // content-addressed changed-file evidence — while the task row
    // reaches VerifiedComplete. The record survives a full store reopen.
    let (manager, session, dir) = verified_shared_env();
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
    drop(runtime);
    assert_eq!(outcome.completion, Some(CompletionGate::VerifiedComplete));
    assert_eq!(outcome.verification, vec![("rust_check".to_string(), true)]);
    let h = manager.get_session(session).unwrap().unwrap();
    let tasks = h.list_tasks().unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].state, TaskState::VerifiedComplete);
    let task_id = tasks[0].task_id;
    let records = h.list_verification_records(task_id).unwrap();
    assert_eq!(
        records.len(),
        1,
        "exactly one record for the single verified attempt"
    );
    let rec = &records[0];
    assert_eq!(rec.task_id, task_id);
    assert_eq!(rec.status, VerificationStatus::Passed);
    // Schema v20 evidence (audits 94/116/117): every attempt record lands
    // with the bounded environment fingerprint and the compact
    // candidate-proof reference — and the reference's accounting digest
    // matches the durable accounting picture at completion.
    let fingerprint = rec
        .environment_fingerprint
        .as_ref()
        .expect("a Passing record carries the environment fingerprint");
    assert_eq!(fingerprint.platform, std::env::consts::OS);
    assert_eq!(fingerprint.arch, std::env::consts::ARCH);
    assert!(!fingerprint.verification_impl_version.is_empty());
    assert!(
        fingerprint
            .manifest_hashes
            .iter()
            .any(|m| m.path == "Cargo.toml"),
        "the verification root's manifest is fingerprinted: {fingerprint:?}"
    );
    assert_eq!(fingerprint.task_contract_hash.len(), 64);
    assert_eq!(fingerprint.check_argv_cwd_env_hash.len(), 64);
    assert!(fingerprint.base_tree_hash.is_none(), "honest unknown");
    let cref = rec
        .candidate_proof_ref
        .as_ref()
        .expect("a Passing record carries the candidate-proof reference");
    assert_eq!(cref.task_revision, rec.revision);
    assert_eq!(cref.candidate_manifest_hash.len(), 64);
    assert!(cref
        .accounting_snapshot_digest
        .starts_with("accounting:v1:"));
    assert_eq!(
        Some(cref.accounting_snapshot_digest.clone()),
        h.accounting_snapshot_digest(task_id).ok(),
        "the candidate reference pins the accounting snapshot at completion"
    );
    assert!(
        rec.completed_ms.is_some_and(|c| rec.started_ms <= c),
        "record lifecycle timestamps are ordered: {rec:?}"
    );
    // Checks mirror the executed runs (program/args/category/status/exit).
    assert_eq!(
        rec.checks.len(),
        1,
        "one execution row per check that ran: {:?}",
        rec.checks
    );
    let check = &rec.checks[0];
    assert_eq!(check.check, "rust_check");
    assert_eq!(check.program, "cargo");
    assert_eq!(check.args, vec!["check".to_string()]);
    assert_eq!(check.category, "compile");
    assert!(check.required);
    assert_eq!(check.status, VerificationStatus::Passed);
    assert_eq!(check.exit, Some(0));
    // Criterion verdicts cover EVERY current acceptance criterion with
    // passed=true (the exact contract complete_verified_task re-checks in
    // its store transaction).
    assert!(!tasks[0].acceptance_criteria.is_empty());
    for entry in &tasks[0].acceptance_criteria {
        assert!(
            rec.criteria
                .iter()
                .any(|cv| cv.passed && &cv.criterion_key == entry),
            "record must certify criterion {entry:?}: {rec:?}"
        );
    }
    assert!(
        rec.changed_files
            .iter()
            .any(|f| f.path == "src/a.rs" && f.size > 0),
        "the record carries content-addressed changed-file evidence: {:?}",
        rec.changed_files
    );
    let facts = h.memory_facts().unwrap();
    assert!(
        facts
            .iter()
            .any(|(k, key, v)| k == "task_state" && key == "state" && v == "verified_complete"),
        "{facts:?}"
    );
    // A full reopen (daemon restart) keeps the record Passed + intact.
    drop(h);
    drop(manager);
    let manager2 =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let h2 = manager2.get_session(session).unwrap().unwrap();
    let tasks2 = h2.list_tasks().unwrap();
    assert_eq!(tasks2.len(), 1);
    assert_eq!(tasks2[0].state, TaskState::VerifiedComplete);
    let records2 = h2.list_verification_records(task_id).unwrap();
    assert_eq!(records2.len(), 1, "no record duplication across reopen");
    assert_eq!(records2[0].status, VerificationStatus::Passed);
    assert_eq!(records2[0].checks.len(), 1);
    assert_eq!(records2[0].checks[0].check, "rust_check");
    assert_eq!(
        records2[0].criteria, rec.criteria,
        "record content is immutable across reopen"
    );
    assert_eq!(
        records2[0].environment_fingerprint, rec.environment_fingerprint,
        "the environment fingerprint survives the reopen byte-identically"
    );
    assert_eq!(
        records2[0].candidate_proof_ref, rec.candidate_proof_ref,
        "the candidate-proof reference survives the reopen byte-identically"
    );
}

#[tokio::test]
async fn failed_attempt_then_fixed_turn_completes_with_a_second_fresh_record() {
    // Adversarial P0-8 (ii): a failing required check lands a FAILED
    // record and NEVER a VerifiedComplete row; the next successful turn
    // must create a SECOND, FRESH record and complete through it — the
    // earlier Failed record for the same revision must never poison the
    // later attempt and completion never reuses an old record.
    let (manager, session, _dir) = verified_shared_env();
    let failing = fake(|_cmd: &str| Err("type error".to_string()));
    let ok = fake_ok();
    // Turn 1: the required check FAILS.
    let (turn1_deps, _d1) = verified_turn_deps(
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
        failing,
        0.65,
    );
    let runtime1 = AgentRuntime::new(turn1_deps).unwrap();
    let o1 = runtime1
        .run_turn(session, "write broken.rs", &[])
        .await
        .unwrap();
    drop(runtime1);
    assert!(matches!(
        o1.completion,
        Some(CompletionGate::FailedVerification { .. })
    ));
    let h = manager.get_session(session).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    assert_eq!(
        h.list_tasks().unwrap()[0].state,
        TaskState::NeedsVerification,
        "a failed attempt never leaves the row verified"
    );
    let records = h.list_verification_records(task_id).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].status, VerificationStatus::Failed);
    drop(h);
    // Turn 2: the fix passes — a SECOND record (fresh, Passed) completes.
    let (turn2_deps, _d2) = verified_turn_deps(
        &manager,
        vec![
            ScriptedResponse::ToolCall {
                id: "c2".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/fixed.rs", "content": "pub fn fixed() -> u32 {\n    let base: u32 = 41;\n    let step: u32 = 1;\n    base.saturating_add(step).saturating_mul(2).saturating_add(1)\n}\n"}),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
        ok,
        0.65,
    );
    let runtime2 = AgentRuntime::new(turn2_deps).unwrap();
    let o2 = runtime2
        .run_turn(session, "write fixed.rs", &[])
        .await
        .unwrap();
    drop(runtime2);
    assert_eq!(o2.completion, Some(CompletionGate::VerifiedComplete));
    let h = manager.get_session(session).unwrap().unwrap();
    assert_eq!(
        h.list_tasks().unwrap()[0].state,
        TaskState::VerifiedComplete,
        "the fixed turn completes through its fresh record"
    );
    let records = h.list_verification_records(task_id).unwrap();
    assert_eq!(
        records.len(),
        2,
        "one record PER ATTEMPT — the failed attempt's record is never reused"
    );
    assert_eq!(records[0].status, VerificationStatus::Failed);
    assert_eq!(records[1].status, VerificationStatus::Passed);
    assert_ne!(records[0].record_id, records[1].record_id);
    let row_criteria = &h.list_tasks().unwrap()[0].acceptance_criteria;
    for entry in row_criteria {
        assert!(
            records[1]
                .criteria
                .iter()
                .any(|cv| cv.passed && &cv.criterion_key == entry),
            "the completing record covers {entry:?}"
        );
    }
    let facts = h.memory_facts().unwrap();
    assert!(
        facts
            .iter()
            .any(|(k, key, v)| k == "task_state" && key == "state" && v == "verified_complete"),
        "{facts:?}"
    );
}

#[tokio::test]
async fn crash_between_record_creation_and_completion_converges_after_restart() {
    // Adversarial P0-8 (iii): the completion seam between the durable
    // record and complete_verified_task has NO await point inside one
    // drive (the genuine-end tail is synchronous store work), so an abort
    // cannot land there deterministically. The crash residue is therefore
    // constructed exactly as an abort in that window would leave it: the
    // row at Verifying, this attempt's record Running (crashed between
    // record-create and finalize) and/or Passed (crashed between
    // finalize and complete). Reopen must show a CONSISTENT row that is
    // NOT VerifiedComplete; the next real drive re-runs the checks and
    // converges with a FRESH record — the stale residue never completes
    // the task by itself and never poisons the new attempt.
    let (manager, session, dir) = verified_shared_env();
    let h = manager.get_session(session).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    let now = h.now_ms();
    h.create_task(Task {
        task_id,
        session_id: session,
        goal: "gating task".into(),
        acceptance_criteria: vec![],
        plan: vec![],
        attachments: Vec::new(),
        budget: Default::default(),
        state: TaskState::Running,
        created_ms: now,
        updated_ms: now,
    })
    .unwrap();
    // The crashed completion attempt drove the claim to Verifying.
    h.transition_task(
        task_id,
        h.task_revision(task_id).unwrap(),
        TaskTransition::RequestVerification,
        None,
    )
    .unwrap();
    h.transition_task(
        task_id,
        h.task_revision(task_id).unwrap(),
        TaskTransition::StartVerification,
        None,
    )
    .unwrap();
    // Residue records of the crashed attempt: one left Running (crash
    // after record-create, before finalize) and one Passed (crash after
    // finalize, before complete_verified_task).
    let running_rec = h
        .create_verification_record(
            task_id,
            None,
            vec![],
            vec![],
            vec![],
            vec![],
            None,
            VerificationStatus::Running,
            now,
        )
        .unwrap();
    let passed_rec = h
        .create_verification_record(
            task_id,
            None,
            vec![],
            vec![],
            vec![],
            vec![],
            None,
            VerificationStatus::Running,
            now,
        )
        .unwrap();
    h.finalize_verification_record(passed_rec, VerificationStatus::Passed, now)
        .unwrap();
    // Full daemon restart over the same store.
    drop(h);
    drop(manager);
    let manager2 =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let h2 = manager2.get_session(session).unwrap().unwrap();
    let row = &h2.list_tasks().unwrap()[0];
    assert_eq!(
        row.state,
        TaskState::Verifying,
        "the crashed attempt left the row under verification, never VerifiedComplete"
    );
    assert_eq!(
        h2.list_verification_records(task_id).unwrap().len(),
        2,
        "both residue records survive the reopen"
    );
    let records = h2.list_verification_records(task_id).unwrap();
    assert_eq!(records[0].record_id, running_rec);
    assert_eq!(records[0].status, VerificationStatus::Running);
    assert_eq!(records[1].record_id, passed_rec);
    assert_eq!(records[1].status, VerificationStatus::Passed);
    let row_rev = h2.task_revision(task_id).unwrap();
    assert!(
        records.iter().all(|r| r.revision == row_rev),
        "residue records certify the row's revision: {records:?}"
    );
    drop(h2);
    // The next real drive re-runs the checks and completes with a FRESH
    // record (the residue records are never reused as completion proof).
    let (turn_deps, _d) = verified_turn_deps(
        &manager2,
        vec![
            ScriptedResponse::ToolCall {
                id: "c3".into(),
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
    let o = runtime
        .run_turn(session, "write src/a.rs", &[])
        .await
        .unwrap();
    drop(runtime);
    assert_eq!(o.completion, Some(CompletionGate::VerifiedComplete));
    let h3 = manager2.get_session(session).unwrap().unwrap();
    assert_eq!(
        h3.list_tasks().unwrap()[0].state,
        TaskState::VerifiedComplete,
        "the next drive converges to VerifiedComplete"
    );
    let records = h3.list_verification_records(task_id).unwrap();
    assert_eq!(
        records.len(),
        3,
        "the converging attempt lands its OWN fresh record"
    );
    assert_eq!(records[2].status, VerificationStatus::Passed);
    assert_ne!(
        records[2].record_id, passed_rec,
        "never reuses residue proof"
    );
    // The residue Running record is still there (crashed attempts stay
    // observable) but the completion is certified by the fresh record.
    assert!(records
        .iter()
        .any(|r| r.status == VerificationStatus::Running));
    let facts = h3.memory_facts().unwrap();
    assert!(
        facts
            .iter()
            .any(|(k, key, v)| k == "task_state" && key == "state" && v == "verified_complete"),
        "{facts:?}"
    );
}

#[tokio::test]
async fn revision_bump_between_record_and_completion_refuses_the_claim() {
    // Adversarial P0-7/P0-8 (vi): an external writer bumps the task
    // row's revision between the record's certification and
    // complete_verified_task — a race this runtime cannot produce inside
    // one synchronous finish tail, so the seam state is constructed
    // exactly as it would be left. The completion transaction refuses
    // with the typed RevisionMismatch; the runtime's translation
    // downgrades the claim to BlockedVerification (durable rows
    // disagree) naming the typed cause; the row is NOT marked complete;
    // and the claim reverts to NeedsVerification where a FRESH record at
    // the CURRENT revision completes — the stale record never poisons
    // the task and no VerifiedComplete ever lands without a matching
    // current-revision record.
    let (manager, session, _dir) = verified_shared_env();
    let h = manager.get_session(session).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    let now = h.now_ms();
    h.create_task(Task {
        task_id,
        session_id: session,
        goal: "gating task".into(),
        acceptance_criteria: vec![],
        plan: vec![],
        attachments: Vec::new(),
        budget: Default::default(),
        state: TaskState::Running,
        created_ms: now,
        updated_ms: now,
    })
    .unwrap();
    // The completion tail drove the claim to Verifying.
    h.transition_task(
        task_id,
        h.task_revision(task_id).unwrap(),
        TaskTransition::RequestVerification,
        None,
    )
    .unwrap();
    h.transition_task(
        task_id,
        h.task_revision(task_id).unwrap(),
        TaskTransition::StartVerification,
        None,
    )
    .unwrap();
    let certified_rev = h.task_revision(task_id).unwrap();
    // The attempt's Passed record certifying the CURRENT revision.
    let stale_record = h
        .create_verification_record(
            task_id,
            None,
            vec![],
            vec![],
            vec![],
            vec![],
            None,
            VerificationStatus::Passed,
            now,
        )
        .unwrap();
    // The EXTERNAL bump between the record's certification and the
    // completion call (hostile writer: any content change moves the
    // revision).
    h.update_task(
        task_id,
        TaskPatch {
            acceptance_criteria: Some(vec!["externally revised".into()]),
            ..Default::default()
        },
    )
    .unwrap();
    // The typed refusal (session contract, one store transaction):
    let err = h
        .complete_verified_task(task_id, certified_rev, stale_record)
        .unwrap_err();
    assert!(
        matches!(err, TaskError::RevisionMismatch { .. }),
        "the completion transaction must refuse the stale certification: {err:?}"
    );
    // The runtime's translation: a BlockedVerification whose reason names
    // the typed cause — the turn outcome is NEVER a silent completion.
    let gate = completion_refusal_gate(&err);
    match gate {
        CompletionGate::BlockedVerification { reasons } => {
            assert!(
                reasons
                    .iter()
                    .any(|r| r.detail.contains("revision mismatch")),
                "the refused claim must name the revision mismatch: {reasons:?}"
            );
        }
        other => panic!("a completion refusal must downgrade to Blocked, got {other:?}"),
    }
    assert_eq!(
        h.list_tasks().unwrap()[0].state,
        TaskState::Verifying,
        "the typed refusal leaves the row untouched (session contract)"
    );
    // The runtime's seam handling: the claim reverts to
    // NeedsVerification, and a FRESH record certifying the CURRENT
    // revision completes — the stale record never poisons the task.
    h.transition_task(
        task_id,
        h.task_revision(task_id).unwrap(),
        TaskTransition::Reverify,
        None,
    )
    .unwrap();
    assert_eq!(
        h.list_tasks().unwrap()[0].state,
        TaskState::NeedsVerification
    );
    // The row's criteria were externally rewritten: the next verification
    // derives fresh ones (here: the honest re-derivation restores the
    // canonical entries before the retry).
    h.update_task(
        task_id,
        TaskPatch {
            acceptance_criteria: Some(vec![]),
            ..Default::default()
        },
    )
    .unwrap();
    h.transition_task(
        task_id,
        h.task_revision(task_id).unwrap(),
        TaskTransition::StartVerification,
        None,
    )
    .unwrap();
    let current_rev = h.task_revision(task_id).unwrap();
    let fresh_record = h
        .create_verification_record(
            task_id,
            None,
            vec![],
            vec![],
            vec![],
            vec![],
            None,
            VerificationStatus::Passed,
            now,
        )
        .unwrap();
    h.complete_verified_task(task_id, current_rev, fresh_record)
        .unwrap();
    let row = &h.list_tasks().unwrap()[0];
    assert_eq!(row.state, TaskState::VerifiedComplete);
    let records = h.list_verification_records(task_id).unwrap();
    assert_eq!(records.len(), 2, "one record per attempt");
    assert_ne!(records[0].record_id, records[1].record_id);
    assert!(records
        .iter()
        .any(|r| r.status == VerificationStatus::Passed));
}

#[tokio::test]
async fn crash_resume_uses_recorded_turn_op_and_model_override() {
    // P0 (requirement 1): a crash mid-turn (durable ToolStarted, no
    // completion) with a NON-DEFAULT model override active resumes the
    // SAME logical turn: the recorded turn op id (never OpId::new(1) or
    // a fresh op), the recorded model "m2" (NOT the session default
    // "m"), and no fresh TurnRecord. After the resume the record reads
    // completed (requirement 1b).
    let dir = fresh_store_dir();
    let file = dir.path().join("w.txt");
    std::fs::write(&file, b"landed").unwrap();
    let expected = FileHash::from(blake3::hash(b"landed").into());
    let turn_op: OpId;
    let tool_op: OpId;
    let session: SessionId;
    {
        let manager1 =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let (deps1, _keep) = deps_sharing_session(
            manager1.clone(),
            Arc::new(scripted_provider(vec![])),
            vec![],
        );
        let runtime1 = AgentRuntime::new(deps1).unwrap();
        // The legacy VerifyHash row records an ABSOLUTE path; the
        // workspace root is the tempdir so the path is provably inside
        // and migrates (audit P1-F). The turn identity assertions below
        // are what this test pins.
        let ws = manager1
            .create_workspace(dir.path().to_str().unwrap())
            .unwrap();
        let handle = manager1.create_session(ws, "t", "fake", "m").unwrap();
        session = handle.id();
        let receipt = handle.submit_prompt("crash me", &[]).unwrap();
        turn_op = receipt.op_id;
        // The drive had STARTED with the per-message override: the
        // record's envelope already carries "m2" (the session default
        // stays "m").
        handle
            .set_turn_envelope(turn_op, "fake", "m2", None, Some("native"))
            .unwrap();
        let meta = op_meta(
            &manager1,
            session,
            RecoveryStrategy::VerifyHash {
                path: file.to_string_lossy().to_string(),
                expected,
            },
        );
        tool_op = meta.operation_id;
        chain_to_streaming(&handle, turn_op);
        crash_tool_start(
            &handle,
            turn_op,
            "write_file",
            serde_json::json!({}),
            "call_1",
            meta,
        );
        // Session default UNCHANGED by the override.
        assert_eq!(handle.model().unwrap(), "m");
        drop(runtime1);
    }
    // Daemon restart over the same durable dir.
    let inner = scripted_provider(vec![
        ScriptedResponse::Text("resumed final".into()),
        ScriptedResponse::End,
    ]);
    let (deps2, _keep2) = reopen_runtime(&dir, Arc::new(inner.clone()), vec![]);
    let runtime2 = AgentRuntime::new(deps2).unwrap();
    // Crash recovery first (sync sweep: resolves the pending row to
    // completed/verified without re-running the tool).
    let reports = runtime2.recover().unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].crashed_ops.len(), 1);
    assert_eq!(reports[0].crashed_ops[0].op_id, tool_op);
    assert_eq!(reports[0].crashed_ops[0].status, "completed");
    // The recorded identity survived the sweep: ONE record, still the
    // original turn op, still active (the turn resumes, not a new one).
    let handle2 = runtime2
        .deps()
        .session
        .get_session(session)
        .unwrap()
        .unwrap();
    let records = handle2.turn_records().unwrap();
    assert_eq!(records.len(), 1, "no fresh TurnRecord was created");
    assert_eq!(records[0].turn_op_id, turn_op);
    assert_eq!(records[0].status, "active");
    assert_eq!(records[0].effective_model, "m2");
    // Resume the interrupted logical turn: same op id, recorded model.
    let outcome = runtime2.continue_turn(session).await.unwrap();
    assert_eq!(
        outcome.final_state,
        AgentState::ReadyForNextTurn,
        "the resumed turn must drive to its genuine end"
    );
    assert_eq!(outcome.op_id, turn_op);
    // The provider saw the RECORDED model, not the session default.
    assert_eq!(
        inner.last_request_model().as_deref(),
        Some("m2"),
        "resume must use the recorded model override"
    );
    // Journal events of the resumed turn reference the recorded op id.
    let events = handle2.events_range(1, None).unwrap();
    let crash_seq = events
        .iter()
        .find(|e| e.kind == faktor_core::event::EventKind::CrashDetected)
        .expect("CrashDetected journaled")
        .seq;
    for e in events.iter().filter(|e| e.seq.raw() > crash_seq.raw()) {
        match e.kind {
            faktor_core::event::EventKind::PhaseChanged
            | faktor_core::event::EventKind::ModelStarted
            | faktor_core::event::EventKind::TurnCompleted => {
                assert_eq!(
                    e.op_id,
                    Some(turn_op),
                    "resumed-turn event {:?} must reference the recorded op",
                    e.kind
                );
            }
            _ => {}
        }
    }
    // The record is completed after the successful resume (1b) and the
    // session default was never consulted.
    let records = handle2.turn_records().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].status, "completed");
    assert_eq!(handle2.model().unwrap(), "m");
    // Exactly one tool run happened (no replay of the verify row).
    let tool_events = events
        .iter()
        .filter(|e| e.kind == faktor_core::event::EventKind::ToolStarted)
        .count();
    assert_eq!(tool_events, 1);
}

#[cfg(unix)]
#[tokio::test]
async fn task_complete_hook_fires_at_the_genuine_turn_end() {
    // The TaskComplete hook must fire at the real end-of-turn boundary
    // (after TurnCompleted) with the final state and the verification/
    // review evidence. Capture the FAKTOR_HOOK_INPUT into a file.
    let out_dir = tempdir().unwrap();
    let out = out_dir.path().join("task_complete.json");
    let (mut deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::Text("final answer".into()),
            ScriptedResponse::End,
        ]),
        vec![],
    );
    let hooks = Arc::new(faktor_hooks::HookRegistry::try_new().expect("standalone supervisor"));
    hooks
        .register(file_writing_hook(
            "task_done",
            faktor_hooks::HookEvent::TaskComplete,
            &out,
        ))
        .unwrap();
    deps.hooks = Some(hooks.clone());
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let outcome = runtime.run_turn(session, "finish", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_eq!(outcome.turns, 1);
    let written = std::fs::read_to_string(&out).expect("the hook must have written its input");
    assert!(
        written.contains("\"task_complete\""),
        "payload event: {written}"
    );
    assert!(
        written.contains("\"finalState\":\"ready_for_next_turn\""),
        "payload final state: {written}"
    );
    assert!(
        written.contains("\"verification\""),
        "verification evidence: {written}"
    );
    assert!(written.contains("\"review\""), "review evidence: {written}");
}

#[tokio::test]
async fn provider_reported_cost_over_the_cap_stops_the_turn_before_the_next_call() {
    // P0-6/12 (d): a turn whose provider-reported cost exceeds the task
    // cap settles honestly (spent > max is recorded — the money WAS
    // spent) and the NEXT reservation refuses with a typed
    // budget_exceeded stop BEFORE the next model call: the second
    // stream is never opened.
    let echoed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let tool = {
        let echoed = echoed.clone();
        Tool {
            name: "echo".into(),
            description: "e".into(),
            input_schema: serde_json::json!({}),
            resource_class: faktor_core::resource::ResourceClass::Cpu,
            capability: None,
            recovery_hint: RecoveryHint::Idempotent,
            path_args: vec![],
            execute: Arc::new(move |_ctx, _a: serde_json::Value| {
                let c = echoed.clone();
                Box::pin(async move {
                    c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(ToolOutcome {
                        text: "done".into(),
                        exit_code: Some(0),
                        ..Default::default()
                    })
                })
            }),
        }
    };
    // Stream 1 opens with a TOOL CALL (interior hop) and reports a cost
    // of 1_000_000 micro — far above the 10_000 cap.
    let costly = CostReportingProvider::new(vec![(Some(1_000_000), true)]);
    let (mut adeps, _dir) = deps_with(costly.clone(), vec![tool]);
    let ledger = faktor_session::DurableBudgetLedger::new(adeps.session.clone());
    let budgets: Arc<dyn faktor_session::BudgetAuthority> = ledger.clone();
    adeps.budgets = budgets;
    let runtime = AgentRuntime::new(adeps).unwrap();
    let session = new_session(runtime.deps());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let now = handle.now_ms();
    handle
        .create_task(faktor_session::Task {
            task_id: handle.task_id().unwrap(),
            session_id: session,
            goal: "budgeted".into(),
            acceptance_criteria: vec![],
            plan: vec![],
            attachments: Vec::new(),
            budget: faktor_session::TaskBudget::default(),
            state: faktor_core::state::TaskState::Pending,
            created_ms: now,
            updated_ms: now,
        })
        .unwrap();
    ledger
        .set_task_max_cost(session, handle.task_id().unwrap(), Some(10_000))
        .unwrap();
    let outcome = runtime.run_turn(session, "hi", &[]).await.unwrap();
    assert_eq!(
        outcome.final_state,
        AgentState::FailedRecoverable,
        "the overshoot must stop the turn at the NEXT reservation"
    );
    assert_eq!(
        outcome.stop_reason.as_ref().map(|r| r.code),
        Some(ReasonCode::BudgetExceeded),
        "the typed stop reason carries budget_exceeded"
    );
    assert_eq!(
        costly.stream_count(),
        1,
        "the second model call must NEVER reach the provider"
    );
    assert_eq!(echoed.load(std::sync::atomic::Ordering::SeqCst), 1);
    let view = ledger
        .session_budget_view(session, handle.task_id().unwrap())
        .expect("durable budget view");
    assert_eq!(
        view.spent_cost_micro, 1_000_000,
        "the overshoot is recorded honestly, never clamped"
    );
    let rows = ledger
        .reservations_of(session, handle.task_id().unwrap(), 10)
        .unwrap();
    assert_eq!(rows[0].provider_reported_micro, Some(1_000_000));
}

#[tokio::test]
async fn read_surface_unavailable_fails_the_turn_before_any_provider_call() {
    // The drive's accounting read runs through the bounded read pool.
    // Once that surface cannot serve reads, the turn refuses typed and
    // the provider is never contacted: an unreadable budget is never a
    // synthesized free budget.
    let costly = CostReportingProvider::new(vec![(None, false)]);
    let (deps, _dir) = deps_with(costly.clone(), vec![]);
    let session = new_session(&deps);
    let runtime = AgentRuntime::new(deps).unwrap();
    assert!(
        runtime
            .deps()
            .session
            .read_service()
            .shutdown(Duration::from_secs(10))
            .await,
        "the read pool shuts down"
    );
    let result = runtime.run_turn(session, "hi", &[]).await;
    assert!(result.is_err(), "the turn must refuse a typed error");
    assert_eq!(
        costly.stream_count(),
        0,
        "no paid provider call may be issued while accounting is unavailable"
    );
}

#[tokio::test]
async fn prefix_observations_land_per_turn_and_stability_tracks_reality() {
    // Fill-site end to end: every completed provider call of a driven
    // session lands a durable provider_call row whose digest is
    // byte-truth against the cacheable head of the REQUEST the provider
    // actually received (in a test turn the volatile tail is empty, so
    // the head is the whole system — the recorded digest must equal
    // blake3 of the captured request system). Three identical turns
    // record per-row stability 1.0; an instruction-rewrite turn (same
    // token count, different bytes) records 0.0; the rows survive a
    // store reopen and chain across runtimes.
    let dir = fresh_store_dir();
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let ws = manager.create_workspace("/w").unwrap();
    let session = manager.create_session(ws, "t", "fake", "m").unwrap().id();

    // Capture the exact system bytes every request carries to the
    // provider (byte-truth cross-check for the recorded digests).
    let fake = scripted_provider(vec![
        ScriptedResponse::Text("ok".into()),
        ScriptedResponse::End,
    ]);
    let captured: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let cap = captured.clone();
    let inspected: Arc<dyn faktor_provider::Provider> =
        Arc::new(InspectingProvider::new(Arc::new(fake), move |_n, req| {
            cap.lock().unwrap().push(req.system.clone());
            Ok(())
        }));

    let rows_of = |m: &SessionManager| {
        m.store()
            .provider_call_prefix_rows(session)
            .unwrap()
            .into_iter()
            .collect::<Vec<_>>()
    };

    // Turns 1-3: byte-identical prompts on a byte-identical head.
    let (mut deps_a, _keep_a) = deps_sharing_session(manager.clone(), inspected.clone(), vec![]);
    deps_a.instructions = "You are a blue agent.".into();
    let runtime_a = AgentRuntime::new(deps_a).unwrap();
    for _ in 0..3 {
        let outcome = runtime_a.run_turn(session, "hi", &[]).await.unwrap();
        assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    }
    drop(runtime_a);

    let systems: Vec<String> = captured.lock().unwrap().clone();
    assert_eq!(systems.len(), 3, "one wire request per text-only turn");
    let rows = rows_of(&manager);
    assert_eq!(rows.len(), 3, "one prefix observation per completed call");
    for (i, (row, sys)) in rows.iter().zip(systems.iter()).enumerate() {
        let expect: [u8; 32] = blake3::hash(sys.as_bytes()).into();
        assert_eq!(
            row.prompt_prefix_hash, expect,
            "row {i} digest must be the byte truth of the sent request"
        );
        assert!(row.prompt_tokens > 0, "row {i} tokens non-NULL");
        assert!(
            row.prefix_stability.is_some(),
            "row {i} must carry a recorded stability"
        );
    }
    assert_eq!(systems[0], systems[1], "turns 1-2 sent identical heads");
    assert_eq!(systems[1], systems[2], "turns 2-3 sent identical heads");
    assert!(
        rows.iter().all(|r| r.prefix_stability == Some(1.0)),
        "three byte-stable turns must record ~1.0 each: {rows:?}"
    );
    let agg = manager
        .store()
        .session_stored_prefix_stability(session)
        .unwrap()
        .unwrap();
    assert_eq!(agg.observations, 3);
    assert_eq!(agg.mean, 1.0);
    assert_eq!(agg.std_dev, 0.0);

    // Reopen: the observations are durable and readable through a fresh
    // manager over the same store.
    drop(manager);
    let manager2 =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let rows2 = rows_of(&manager2);
    assert_eq!(rows2, rows, "observations must survive the reopen");
    let agg2 = manager2
        .store()
        .session_stored_prefix_stability(session)
        .unwrap()
        .unwrap();
    assert_eq!((agg2.observations, agg2.mean), (3, 1.0));

    // Turn 4: a reordering turn — a reconfigured runtime (operator
    // instructions swapped for SAME-length different bytes) rewrites
    // the cacheable head without growing it: the recorded stability of
    // that turn must drop below 1.0 (exactly 0.0).
    let (mut deps_b, _keep_b) = deps_sharing_session(manager2.clone(), inspected.clone(), vec![]);
    deps_b.instructions = "You are a gold agent.".into();
    let runtime_b = AgentRuntime::new(deps_b).unwrap();
    let outcome = runtime_b.run_turn(session, "hi", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    drop(runtime_b);

    let systems: Vec<String> = captured.lock().unwrap().clone();
    assert_eq!(systems.len(), 4);
    let rows3 = rows_of(&manager2);
    assert_eq!(rows3.len(), 4);
    let expect4: [u8; 32] = blake3::hash(systems[3].as_bytes()).into();
    assert_eq!(rows3[3].prompt_prefix_hash, expect4);
    assert_ne!(
        rows3[3].prompt_prefix_hash, rows3[2].prompt_prefix_hash,
        "the rewrite turn must change the head digest"
    );
    assert_eq!(
        rows3[3].prompt_tokens, rows3[2].prompt_tokens,
        "same-length instruction rewrite must keep the token count"
    );
    assert_eq!(
        rows3[3].prefix_stability,
        Some(0.0),
        "the reordering turn must record < 1.0"
    );
    let agg3 = manager2
        .store()
        .session_stored_prefix_stability(session)
        .unwrap()
        .unwrap();
    assert_eq!(agg3.observations, 4);
    assert!(
        (agg3.mean - 0.75).abs() < 1e-12,
        "mean = 3×1.0 + 0.0 over 4"
    );
    assert!((agg3.std_dev - 0.4330127018922193).abs() < 1e-12);
}

#[tokio::test]
async fn two_attempts_of_one_logical_op_leave_exactly_two_attempt_keyed_calls_and_no_legacy_merged_row(
) {
    // Attempt-accounting closure: two PHYSICAL attempts of ONE logical
    // op (a retryable pre-content network failure, then a clean settle
    // with a canonical cache-split usage frame) leave exactly ONE
    // attempt-keyed terminal provider-call row per attempt — the failed
    // row of the crashed attempt (NULL usage counters: a failed stream's
    // partial usage is not a durable spend basis) and the completed row
    // of the settled attempt carrying its OWN canonical usage. The
    // legacy logical-op MERGED row (one op-keyed row for the whole
    // logical call) must be gone: the session's durable token spend is
    // exactly the settled attempt's frame once.
    let provider = Arc::new(RetryOnceThenCacheSplitProvider::default());
    let (runtime, session, ledger) = routed_settlement_runtime_with_retries(
        provider.clone(),
        faktor_core::retry::RetryPolicy {
            max_attempts: 3,
            base_delay_ms: 1,
            max_delay_ms: 5,
            jitter: 0.0,
            class: faktor_core::retry::RetryClass::Network,
        },
    )
    .await;
    let outcome = runtime.run_turn(session, "retry me", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    let wire = provider.requests();
    assert_eq!(wire.len(), 2, "two physical attempts reached the provider");
    assert_eq!(
        wire.iter().map(|(_, attempt)| *attempt).collect::<Vec<_>>(),
        vec![0, 1],
        "the wire carried attempt ordinals 0 and 1"
    );
    assert!(
        wire.iter().all(|(op, _)| *op == outcome.op_id),
        "every attempt preserved the logical request identity on the wire: {wire:?}"
    );
    let task_id = runtime
        .deps
        .session
        .get_session(session)
        .unwrap()
        .unwrap()
        .task_id()
        .unwrap();
    let view = ledger
        .session_budget_view(session, task_id)
        .expect("durable budget view");
    assert_eq!(
        view.spent_cost_micro, 750,
        "only the SETTLED attempt's frame prices: 400@1 + 600@0.5 + 50@1"
    );
    assert_eq!(view.uncertain_reservations, 1);
    let rows = ledger.reservations_of(session, task_id, 10).unwrap();
    assert_eq!(
        rows.len(),
        2,
        "two attempt-keyed reservations, one per attempt"
    );
    assert_ne!(
        rows[0].attempt_op_id, rows[1].attempt_op_id,
        "each attempt got its OWN reservation keyed by its own attempt op"
    );
    for r in &rows {
        assert_eq!(
            r.parent_op_id,
            Some(outcome.op_id),
            "the shared logical op id rides every attempt reservation"
        );
    }
    assert_eq!(rows[0].status, "settled");
    assert_eq!(rows[0].provider_cost_micro, Some(750));
    assert_eq!(rows[1].status, "uncertain");
    assert_eq!(rows[1].provider_cost_micro, None);
    // The settled attempt's canonical usage rides its completed row
    // EXACTLY ONCE: in-fold 400+600, output 50 — no legacy logical-op
    // merged row (which would add the same usage again under the op).
    let tokens = runtime
        .deps
        .session
        .store()
        .session_usage_tokens(session)
        .unwrap();
    assert_eq!(
        tokens, 1050,
        "durable token spend = the completed attempt's canonical usage once"
    );
    drop(runtime);
}

/// (4) DB read pool runtime tripwire: a full real turn's bounded reads
/// are submitted through the pool with the history/budget/prefix tags,
/// and the task/verification/memory wrappers tag on the SAME pool.
#[tokio::test]
async fn db_read_pool_full_turn_is_tagged_and_bounded() {
    let (deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::Text("ok".into()),
            ScriptedResponse::End,
        ]),
        vec![],
    );
    let manager = deps.session.clone();
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let before = manager.read_service().stats();
    let outcome = runtime.run_turn(session, "hello", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    let after_turn = manager.read_service().stats();
    assert!(
        after_turn.enqueued > before.enqueued,
        "the turn's reads must be submitted to the bounded pool"
    );
    for tag in ["history", "budget", "prefix"] {
        assert!(
            after_turn.kind_label(tag) > before.kind_label(tag),
            "the turn's {tag} read must be served and tagged by the pool: {after_turn:?}"
        );
    }
    // The remaining capabilities are exercised on the SAME pool; each
    // tag must be counted (the manager wrappers, never inline sync reads).
    manager.task(session, TaskId::new(1)).await.unwrap();
    manager
        .verification_records(session, TaskId::new(1))
        .await
        .unwrap();
    manager.memory_page(session, None, 1).await.unwrap();
    let stats = manager.read_service().stats();
    for tag in [
        "history",
        "task",
        "budget",
        "prefix",
        "verification",
        "memory",
    ] {
        assert!(
            stats.kind_label(tag) > 0,
            "tagged count for {tag} must be > 0: {stats:?}"
        );
    }
    assert_eq!(
        stats.tagged.iter().sum::<u64>(),
        stats.enqueued,
        "every submitted pool read is tagged"
    );
}

#[tokio::test]
async fn failed_turn_journal_lost_to_corrupt_event_table_is_marked_and_reconstructed() {
    // Adversarial (corrupt store): a BEFORE INSERT trigger aborts every
    // event write, so the drive's journal appends fail and the failed-turn
    // cleanup write fails too. The ORIGINAL drive error must reach the
    // caller; the fs marker channel (independent of SQLite) records the
    // transition; once the injected trigger is removed the next open
    // reconstructs the Failed journal from the marker.
    let (deps, dir) = deps(scripted_provider(vec![ScriptedResponse::End]), vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let manager = runtime.deps().session.clone();
    let session = new_session(runtime.deps());
    let receipt = runtime.submit(session, "run", &[]).unwrap();
    let op_id = receipt.op_id;
    manager
        .store()
        .sql_execute(
            "CREATE TRIGGER dw_test_fail_journal BEFORE INSERT ON event \
                 BEGIN SELECT RAISE(ABORT, 'injected journal corruption'); END",
        )
        .unwrap();
    let err = runtime
        .drive_receipt(
            &manager.get_session(session).unwrap().unwrap(),
            receipt,
            None,
        )
        .await
        .expect_err("a corrupt journal must fail the drive loudly");
    assert!(!err.message.is_empty(), "{err:?}");
    let root = manager.store().root().to_path_buf();
    assert_eq!(
        marker_sites(&root),
        vec![DW_SITE_RECEIPT_JOURNAL.to_string()],
        "exactly the lost Failed-journal is compensated"
    );
    let marker = marker_json(&marker_files(&root)[0]);
    assert_eq!(marker["status"], "pending");
    assert_eq!(marker["intent"]["write"], "journal_failed");
    assert_eq!(marker["session"], serde_json::json!(session.raw()));
    // The injected corruption is removed (the store's own rows survive);
    // reopen + recovery replays the marker.
    manager
        .store()
        .sql_execute("DROP TRIGGER dw_test_fail_journal")
        .unwrap();
    drop(runtime);
    drop(manager);
    let manager2 = reopen_manager(&dir);
    let (deps2, _d2) = deps_sharing_session(
        manager2.clone(),
        Arc::new(scripted_provider(vec![ScriptedResponse::End])),
        vec![],
    );
    AgentRuntime::new(deps2).unwrap().recover().unwrap();
    let h2 = manager2.get_session(session).unwrap().unwrap();
    let events = h2.events_range(1, Some(512)).unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.op_id == Some(op_id) && e.kind == faktor_core::event::EventKind::Failed),
        "the Failed journal was not reconstructed: {events:?}"
    );
    assert_eq!(h2.state().unwrap(), AgentState::FailedRecoverable);
    assert!(h2.active_turn_record().unwrap().is_none());
    assert!(marker_files(manager2.store().root()).is_empty());
}

#[tokio::test]
async fn turn_record_close_loss_is_marked_and_reconstructed_on_reopen() {
    // Adversarial (injected store fault): a SUCCESSFUL turn whose record
    // close fails must still report success (never a fabricated failed
    // turn), mint the durable marker + audit event, and have the record
    // closed by the replay at the next open.
    let (deps, dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ]),
        vec![],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let manager = runtime.deps().session.clone();
    let session = new_session(runtime.deps());
    durable_faults_tests::arm(runtime.deps().session.store().root(), DW_SITE_DRIVE_RECORD);
    let outcome = runtime.run_turn(session, "hello", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    let root = manager.store().root().to_path_buf();
    assert_eq!(marker_sites(&root), vec![DW_SITE_DRIVE_RECORD.to_string()]);
    let h = manager.get_session(session).unwrap().unwrap();
    assert!(
        h.active_turn_record().unwrap().is_some(),
        "the lost close left the record active"
    );
    let audit = h.events_range(1, Some(512)).unwrap();
    assert!(
        audit.iter().any(|e| {
            e.kind == faktor_core::event::EventKind::CrashDetected
                && e.payload
                    .as_ref()
                    .and_then(|p| p.get("durable_write_failure"))
                    .and_then(|f| f.get("site"))
                    .and_then(|s| s.as_str())
                    == Some(DW_SITE_DRIVE_RECORD)
        }),
        "the durable audit event must name the failed write: {audit:?}"
    );
    drop(runtime);
    drop(manager);
    let manager2 = reopen_manager(&dir);
    let (deps2, _d2) = deps_sharing_session(
        manager2.clone(),
        Arc::new(scripted_provider(vec![ScriptedResponse::End])),
        vec![],
    );
    AgentRuntime::new(deps2).unwrap().recover().unwrap();
    let h2 = manager2.get_session(session).unwrap().unwrap();
    assert!(
        h2.active_turn_record().unwrap().is_none(),
        "the marker replayed the record close"
    );
    assert!(marker_files(manager2.store().root()).is_empty());
}
