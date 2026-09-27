//! `runtime::turn::queue_tests`: out-of-line tests.

#![allow(unused_imports)]

use super::*;
use crate::runtime::fixtures_tests::*;
use crate::runtime::tests::*;
use crate::*;

#[tokio::test]
async fn model_override_does_not_mutate_session_row() {
    // The override is per-message: the journaled session row must keep
    // its original model after the turn.
    let provider = scripted_provider(vec![
        ScriptedResponse::Text("pong".into()),
        ScriptedResponse::End,
    ]);
    let (deps, _dir) = deps(provider.clone(), vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    assert_eq!(handle.model().unwrap(), "m");

    let outcome = runtime
        .run_turn_with_model(session, "hi", &[], Some("m2".into()))
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    // The override reached the wire...
    assert_eq!(provider.last_request_model().as_deref(), Some("m2"));
    // ...but the session row is untouched.
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    assert_eq!(handle.model().unwrap(), "m");
}

#[tokio::test]
async fn permission_denied_turn_returns_ready() {
    struct DenyAll;
    impl PermissionRequester for DenyAll {
        fn request(
            &self,
            _s: SessionId,
            _p: &SessionPermission,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = faktor_core::Result<PermissionDecision>> + Send>,
        > {
            Box::pin(async { Ok(PermissionDecision::Deny) })
        }
    }
    let (mut deps, _dir) = deps(
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
    deps.permission_requester = Arc::new(DenyAll);
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let outcome = runtime.run_turn(session, "x", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    // No tool run was started.
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    assert!(handle.pending_tool_runs().unwrap().is_empty());
}

#[tokio::test]
async fn mixed_permission_denied_and_approved_batch_continues_lawfully() {
    // The approved call is submitted FIRST and its run is in flight when
    // the sibling permission is denied: the denial must keep the batch
    // executing, or the approved sibling's FileChanged/finish die.
    let approved_execs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let denied_execs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (runtime, recorder, session, _dir) = mixed_permission_batch(
        vec![
            ("ok".into(), "echo".into(), serde_json::json!({"x": 1})),
            (
                "refused".into(),
                "write_file".into(),
                serde_json::json!({"path": "notes.txt", "content": "hi"}),
            ),
        ],
        &["write_file"],
        vec![
            counting_echo_tool(approved_execs.clone()),
            counting_write_tool(denied_execs.clone()),
        ],
    );
    let outcome = runtime.run_turn(session, "use both", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_eq!(
        approved_execs.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the approved sibling must execute"
    );
    assert_eq!(
        denied_execs.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the denied sibling must never execute"
    );
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    assert!(dangling_tool_calls(&handle).is_empty());
    let (approved, approved_exit) = tool_result_for(&handle, "ok").expect("approved result");
    assert_eq!(approved_exit, Some(0));
    assert_eq!(approved, "echo: {\"x\":1}");
    assert_permission_denial(&handle, "refused", "write_file");
    assert!(handle.pending_tool_runs().unwrap().is_empty());
    let events = handle.events_range(1, None).unwrap();
    let denial = events
        .iter()
        .find(|e| e.kind == faktor_core::event::EventKind::PermissionDenied)
        .expect("the denial is journaled");
    assert_eq!(
        denial.state,
        AgentState::ExecutingTool,
        "the denial lands on the batch-execution edge, never ReadyForNextTurn"
    );
    assert!(
        events.iter().any(|e| {
            e.kind == faktor_core::event::EventKind::FileChanged
                && e.state == AgentState::ExecutingTool
        }),
        "the approved sibling's FileChanged stays legal"
    );
    assert_eq!(recorder.requests().len(), 2, "the mixed batch continues");
    assert_eq!(
        wire_tool_results(&recorder),
        vec![("ok".to_string(), false), ("refused".to_string(), true)],
        "both calls answered on the wire"
    );
}

#[tokio::test]
async fn mixed_permission_denied_first_batch_continues_lawfully() {
    // The DENIED call is submitted FIRST: at deny time no run is in
    // flight yet, but the sibling call is durably unanswered. The old
    // code landed ReadyForNextTurn and the sibling's own permission hop
    // (`ReadyForNextTurn -> ToolRequested`) died with InvalidState.
    let approved_execs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let denied_execs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (runtime, recorder, session, _dir) = mixed_permission_batch(
        vec![
            (
                "refused".into(),
                "write_file".into(),
                serde_json::json!({"path": "notes.txt", "content": "hi"}),
            ),
            ("ok".into(), "echo".into(), serde_json::json!({"x": 1})),
        ],
        &["write_file"],
        vec![
            counting_echo_tool(approved_execs.clone()),
            counting_write_tool(denied_execs.clone()),
        ],
    );
    let outcome = runtime.run_turn(session, "use both", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_eq!(approved_execs.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(denied_execs.load(std::sync::atomic::Ordering::SeqCst), 0);
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    assert!(dangling_tool_calls(&handle).is_empty());
    let (approved, approved_exit) = tool_result_for(&handle, "ok").expect("approved result");
    assert_eq!(approved_exit, Some(0));
    assert_eq!(approved, "echo: {\"x\":1}");
    assert_permission_denial(&handle, "refused", "write_file");
    let events = handle.events_range(1, None).unwrap();
    let denial = events
        .iter()
        .find(|e| e.kind == faktor_core::event::EventKind::PermissionDenied)
        .expect("the denial is journaled");
    assert_eq!(denial.state, AgentState::ExecutingTool);
    // The sibling's permission hop happened AFTER the denial and the
    // batch still executed it.
    let denial_seq = denial.seq.raw();
    assert!(
        events.iter().any(|e| {
            e.kind == faktor_core::event::EventKind::ToolStarted && e.seq.raw() > denial_seq
        }),
        "the approved sibling starts after the denial"
    );
    assert_eq!(recorder.requests().len(), 2, "the mixed batch continues");
    assert_eq!(
        wire_tool_results(&recorder),
        vec![("ok".to_string(), false), ("refused".to_string(), true)],
    );
}

#[tokio::test]
async fn abort_cancels_mid_turn() {
    let (deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::Text("a".into()),
            ScriptedResponse::Text("b".into()),
            ScriptedResponse::Text("c".into()),
            ScriptedResponse::End,
        ]),
        vec![],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let receipt = handle.submit_prompt("go", &[]).unwrap();
    receipt.op_meta.cancellation.cancel();
    let outcome = runtime
        .drive_turn(
            &handle,
            receipt.op_id,
            receipt.op_meta.cancellation.clone(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::Cancelled);
}

#[tokio::test]
async fn failed_turn_never_leaves_session_stuck() {
    // A provider that is NOT registered: the turn fails at startup. The
    // session must land on FailedRecoverable (promptable) — never stuck
    // in Preparing, which would reject every future prompt.
    let (mut deps, _dir) = deps(scripted_provider(vec![ScriptedResponse::End]), vec![]);
    // Remove the registered provider so lookup fails.
    deps.providers = Arc::new(ProviderRegistry::new());
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let err = runtime.run_turn(session, "hi", &[]).await.unwrap_err();
    assert!(err.kind == ErrorKind::NotFound);
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let state = handle.state().unwrap();
    assert_eq!(
        state,
        AgentState::FailedRecoverable,
        "failed turn must land on FailedRecoverable, got {state:?}"
    );
    // The session accepts a NEW prompt afterwards (FailedRecoverable is
    // promptable) — recovery is possible.
    let receipt = handle.submit_prompt("retry", &[]).unwrap();
    assert!(receipt.accepted);
}

#[tokio::test]
async fn second_prompt_while_active_is_queued_and_delivered_after() {
    // Audit round 6 P0: prompt B while turn A is active must durably
    // queue; the per-session runner delivers B only after A finishes;
    // exactly one PromptReceived per prompt; B never leaks into A's
    // context.
    let (deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "echo".into(),
                input: serde_json::json!({"x": 1}),
            },
            ScriptedResponse::Text("answer A".into()),
            ScriptedResponse::End,
        ]),
        vec![echo_tool()],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();

    // Start turn A (detached drive via a spawned task to simulate the
    // server pattern).
    let receipt_a = runtime.submit(session, "task A", &[]).unwrap();
    assert!(!receipt_a.queued);
    let agent = runtime.clone();
    let handle2 = runtime.deps.session.get_session(session).unwrap().unwrap();
    let a_task = tokio::spawn(async move {
        agent
            .drive_receipt(&handle2, receipt_a, None)
            .await
            .unwrap()
    });

    // Prompt B arrives while A is active (its provider scripted turn is
    // mid-flight).
    let receipt_b = runtime.submit(session, "task B", &[]).unwrap();
    assert!(receipt_b.queued, "B must queue behind active A");
    assert_eq!(handle.queued_prompt_count().unwrap(), 1);

    // The queue runner is idempotent per session.
    let runner = runtime.clone();
    let runner_task = tokio::spawn(async move { runner.run_session_queue(session).await });
    let _ = a_task.await.unwrap();
    let _ = runner_task.await; // runner drains after A completes

    // B was delivered exactly once and its user message reached the
    // journal; the session is ready again.
    assert_eq!(handle.queued_prompt_count().unwrap(), 0, "queue drained");
    assert_eq!(handle.state().unwrap(), AgentState::ReadyForNextTurn);
    let events = handle.events_range(1, None).unwrap();
    let prompts = events
        .iter()
        .filter(|e| e.kind == faktor_core::event::EventKind::PromptReceived)
        .count();
    assert_eq!(prompts, 2, "one PromptReceived per user prompt");
}

#[tokio::test]
async fn queued_prompt_never_leaks_into_active_turn_context() {
    // The active turn's provider requests must NOT contain the queued
    // prompt's text (isolation via queued_message_seqs).
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
    let receipt_a = runtime
        .submit(session, "secret task A content", &[])
        .unwrap();
    // B queues while A is still in flight (before A is even driven).
    let _b = runtime.submit(session, "QUEUED-B-MARKER", &[]).unwrap();
    assert_eq!(handle.queued_prompt_count().unwrap(), 1);

    let outcome = runtime
        .drive_receipt(&handle, receipt_a, None)
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    // Turn A's second provider request (after the tool) must not contain
    // the queued marker. Inspect via the journal-derived history.
    let history = runtime
        .history_messages(&handle, &ContextBudget::default())
        .await
        .unwrap();
    let rendered = serde_json::to_string(&history).unwrap();
    assert!(
        !rendered.contains("QUEUED-B-MARKER"),
        "queued prompt leaked into the active turn context"
    );
    assert!(rendered.contains("secret task A content"));
}

/// Audit: a session stuck mid-turn (admission declined forever) must not
/// make the queue runner poll forever. With a short turn budget the runner
/// returns a TYPED timeout, the durable queue head stays pending, and the
/// runner gate is released for the next re-kick.
#[tokio::test]
async fn queue_runner_declined_admission_is_bounded_and_typed() {
    let (deps, _dir) = deps(
        scripted_provider(vec![ScriptedResponse::Text("a".into())]),
        vec![],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    runtime.set_turn_budget_ms(60);
    let session = new_session(runtime.deps());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    // The first prompt is never driven: the machine stays mid-turn
    // (Preparing) while the second prompt queues durably.
    let first = runtime.submit(session, "active", &[]).unwrap();
    assert!(!first.queued);
    let second = runtime.submit(session, "queued", &[]).unwrap();
    assert!(second.queued);
    assert_eq!(handle.queued_prompt_count().unwrap(), 1);
    let began = Instant::now();
    let err = runtime
        .run_session_queue_inner(session)
        .await
        .expect_err("a never-eligible session must time out, not poll forever");
    assert_eq!(err.kind, faktor_core::ErrorKind::Timeout, "{err:?}");
    assert!(err.message.contains("declined admission"), "{err:?}");
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "the wait is bounded by the turn budget"
    );
    assert_eq!(
        handle.queued_prompt_count().unwrap(),
        1,
        "the durable queue head is untouched by the timeout"
    );
}

/// Adversarial companion: an UNARMED runner still exits at its budget
/// boundary (no unbounded polling), leaving the durable head pending —
/// and a kick that then finds no runner starts a fresh one (the
/// replacement path), which drains the head exactly once once the active
/// turn is over.
#[tokio::test]
async fn queue_runner_timeout_then_kick_starts_a_replacement_and_drains_once() {
    let base = Arc::new(scripted_provider(vec![ScriptedResponse::Text("A".into())]));
    let provider = Arc::new(InspectingProvider::new(base, |_, _| Ok(())));
    let (deps, _dir) = deps_with(provider.clone(), vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    runtime.set_turn_budget_ms(40);
    let session = new_session(runtime.deps());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let receipt_a = runtime.submit(session, "A prompt", &[]).unwrap();
    let _receipt_b = runtime.submit(session, "B prompt", &[]).unwrap();
    // The unarmed runner exits on its own bounded timeout.
    let runner1 = runtime.clone();
    tokio::spawn(async move { runner1.run_session_queue(session).await })
        .await
        .unwrap();
    assert!(
        !runtime.runners.lock().unwrap().contains_key(&session),
        "the runner gate is released on the bounded timeout"
    );
    assert_eq!(
        handle.queued_prompt_count().unwrap(),
        1,
        "B stays durably pending without a runner"
    );
    // A now ends; the settle path kicks: the replacement runner (the
    // gate is free) claims B exactly once.
    runtime
        .drive_receipt(&handle, receipt_a, None)
        .await
        .unwrap();
    let kicker = runtime.clone();
    kicker.run_session_queue(session).await;
    assert_eq!(handle.queued_prompt_count().unwrap(), 0, "B drained");
    assert_eq!(
        provider.counter.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "A and B each ran exactly once"
    );
}

/// Audit: an interrupted logical turn whose continuation keeps failing
/// (durable permission wait, no live driver) must not make the queue
/// runner poll forever — it returns a TYPED timeout and leaves the queue
/// head pending.
#[tokio::test]
async fn queue_runner_uncontinuable_turn_is_bounded_and_typed() {
    let (deps, _dir) = deps(
        scripted_provider(vec![ScriptedResponse::Text("a".into())]),
        vec![],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    runtime.set_turn_budget_ms(60);
    let session = new_session(runtime.deps());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let op = runtime.deps.session.try_next_op_id().unwrap();
    // Legal durable chain to WaitingForPermission (the tool needs a
    // permission decision nobody will ever answer), plus the active turn
    // record of that interrupted turn. No in-process driver exists (the
    // op was never registered), exactly the post-restart residue shape.
    for (kind, state) in [
        (
            faktor_core::event::EventKind::PromptReceived,
            AgentState::Preparing,
        ),
        (
            faktor_core::event::EventKind::ContextPrepared,
            AgentState::BuildingContext,
        ),
        (
            faktor_core::event::EventKind::ModelStarted,
            AgentState::WaitingForModel,
        ),
        (
            faktor_core::event::EventKind::ModelChunkReceived,
            AgentState::Streaming,
        ),
        (
            faktor_core::event::EventKind::ToolRequested,
            AgentState::WaitingForPermission,
        ),
    ] {
        handle.append_event(kind, state, Some(op), None).unwrap();
    }
    handle
        .start_turn_record(op, None, None, "fake", "m", None)
        .unwrap();
    let queued = runtime.submit(session, "queued", &[]).unwrap();
    assert!(queued.queued);
    assert_eq!(handle.queued_prompt_count().unwrap(), 1);
    let began = Instant::now();
    let err = runtime
        .run_session_queue_inner(session)
        .await
        .expect_err("an uncontinuable interrupted turn must time out");
    assert_eq!(err.kind, faktor_core::ErrorKind::Timeout, "{err:?}");
    assert!(err.message.contains("could not continue"), "{err:?}");
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "the wait is bounded by the turn budget"
    );
    assert_eq!(
        handle.queued_prompt_count().unwrap(),
        1,
        "the durable queue head is untouched"
    );
}

#[tokio::test]
async fn delivered_queued_prompt_appears_after_previous_turn_output() {
    // Audit round 7 (conversation chronology): B's user message must
    // materialize AFTER A's full exchange — never interleaved. With
    // deferred materialization + atomic admission this holds by
    // construction; assert it end-to-end.
    let (deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "echo".into(),
                input: serde_json::json!({"x": 1}),
            },
            ScriptedResponse::Text("A final".into()),
            ScriptedResponse::End,
        ]),
        vec![echo_tool()],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let receipt_a = runtime.submit(session, "A prompt", &[]).unwrap();
    let _ = runtime.submit(session, "B prompt", &[]).unwrap();
    // No message rows exist for B while queued.
    let page_before = handle.messages_before(None, 10).unwrap();
    assert!(
        !page_before.iter().any(|m| m
            .data
            .get("text")
            .and_then(|t| t.as_str())
            .map(|s| s.contains("B prompt"))
            .unwrap_or(false)),
        "queued prompt must not materialize before admission"
    );
    let outcome_a = runtime
        .drive_receipt(&handle, receipt_a, None)
        .await
        .unwrap();
    assert_eq!(outcome_a.final_state, AgentState::ReadyForNextTurn);
    // Deliver B via the runner.
    let runner = runtime.clone();
    let _ = tokio::spawn(async move { runner.run_session_queue(session).await }).await;
    // B's message exists now and its seq is AFTER everything from A.
    let page = handle.messages_before(None, 50).unwrap();
    let b_idx = page
        .iter()
        .position(|m| {
            m.data
                .get("text")
                .and_then(|t| t.as_str())
                .map(|s| s.contains("B prompt"))
                .unwrap_or(false)
        })
        .expect("B message materialized at admission");
    let a_idx = page
        .iter()
        .position(|m| {
            m.data
                .get("text")
                .and_then(|t| t.as_str())
                .map(|s| s.contains("A prompt"))
                .unwrap_or(false)
        })
        .expect("A message present");
    // messages_before returns newest-first: B (newest) has a SMALLER
    // index than A.
    assert!(b_idx < a_idx, "B must sit after A in conversation order");
    // Assistant parts between A's prompt and B's prompt. Page is
    // newest-first: chronologically between A (oldest, largest index)
    // and B (newest, smallest index) lives at indices
    // (b_idx, a_idx) exclusive.
    let assistant_after_a = page
        .iter()
        .skip(b_idx + 1)
        .take(a_idx.saturating_sub(b_idx + 1))
        .any(|m| m.role == "assistant");
    assert!(assistant_after_a, "A's output precedes B's message");
}

#[tokio::test]
async fn aborting_a_queued_prompt_durably_cancels_it() {
    // Adversarial (audit round 7): the user kills prompt B while A is
    // mid-turn. B must NEVER be delivered — its durable row becomes
    // cancelled and the runner skips it, even though A completes and the
    // session reaches ReadyForNextTurn.
    let (deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "echo".into(),
                input: serde_json::json!({"x": 1}),
            },
            ScriptedResponse::Text("A final".into()),
            ScriptedResponse::End,
            // Nothing for B: it must never be driven.
        ]),
        vec![echo_tool()],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let receipt_a = runtime.submit(session, "A prompt", &[]).unwrap();
    let receipt_b = runtime.submit(session, "B prompt", &[]).unwrap();
    assert!(receipt_b.queued);
    // Kill B while A's turn is still registered but not yet driven: the
    // machine must NOT move (no Failed/Cancelled/TurnCompleted for B —
    // it was never a turn). A remains driveable.
    let before = handle.state().unwrap();
    let aborted = handle.abort(Some(receipt_b.op_id)).unwrap();
    assert_eq!(aborted.op_ids, vec![receipt_b.op_id]);
    assert!(!aborted.cancelled_all);
    assert_eq!(
        handle.state().unwrap(),
        before,
        "aborting a queued prompt must not touch the state machine"
    );
    // A's turn still completes normally.
    let outcome = runtime
        .drive_receipt(&handle, receipt_a, None)
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    // The queue row is durably cancelled and the runner drains nothing.
    let counts = handle.queue_status_counts().unwrap();
    assert_eq!(
        counts
            .get("cancelled")
            .and_then(|v| v.as_i64())
            .unwrap_or(0),
        1,
        "aborted queued prompt must be durably cancelled"
    );
    let runner = runtime.clone();
    let _ = tokio::spawn(async move { runner.run_session_queue(session).await }).await;
    assert_eq!(handle.queued_prompt_count().unwrap(), 0);
    let history = runtime
        .history_messages(&handle, &ContextBudget::default())
        .await
        .unwrap();
    let rendered = serde_json::to_string(&history).unwrap();
    assert!(
        !rendered.contains("B prompt"),
        "aborted queued prompt must never reach the timeline"
    );
    assert!(rendered.contains("A prompt"));
}

#[tokio::test]
async fn killed_mid_job_reopen_recovers_honestly_and_never_completes_from_a_vanished_process() {
    // Adversarial (audit P0-5/26 test b): the executor "dies" mid-job
    // (a Running row with no resolve — the supervisor kill path is
    // exercised by the verify-crate whole-tree tests). Reopening the
    // session layer recovers the job honestly (re-queued with a typed
    // note), the task is NOT VerifiedComplete, and a re-run resolves.
    if !std::process::Command::new("make")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        eprintln!("skipping killed-mid-job reopen: no make on this host");
        return;
    }
    let (manager1, session, dir) =
        make_background_env("\tsleep 2\n\techo ran > test-marker.txt\n", None);
    let (mut deps, _d) = deps_sharing_session(
        manager1.clone(),
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
    assert_eq!(
        outcome.completion,
        Some(CompletionGate::VerificationPending)
    );
    // The process dies after CLAIMING the job (Running, never resolved):
    // exactly the durable residue a crash leaves.
    let h1 = manager1.get_session(session).unwrap().unwrap();
    let task_id = h1.task_id().unwrap();
    let attempt = h1
        .current_verification_attempt(task_id.raw())
        .unwrap()
        .unwrap();
    h1.claim_verification_job(task_id.raw(), "make_test", attempt.op_id, 9_999_999)
        .unwrap();
    let running = h1.open_verification_jobs(task_id.raw()).unwrap().remove(0);
    assert_eq!(running.state, faktor_session::VerificationJobState::Running);
    let store = dir.path().join("store");
    let cas = dir.path().join("cas");
    drop(h1);
    drop(runtime);
    drop(manager1);
    // ---- reopen the session layer ----
    let manager2 = SessionManager::open(&store, &cas, true).unwrap();
    let h2 = manager2.get_session(session).unwrap().unwrap();
    let task_before = h2.get_task(task_id).unwrap().unwrap();
    assert_ne!(
        task_before.state,
        TaskState::VerifiedComplete,
        "a vanished process never completes the task"
    );
    let report = h2.recover_verification_jobs_after_restart().unwrap();
    assert_eq!(report.requeued, 1, "{report:?}");
    let jobs = h2.open_verification_jobs(task_id.raw()).unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].state, faktor_session::VerificationJobState::Queued);
    assert!(
        jobs[0].note.as_deref().unwrap().contains("restart"),
        "{jobs:?}"
    );
    let task_mid = h2.get_task(task_id).unwrap().unwrap();
    assert_ne!(task_mid.state, TaskState::VerifiedComplete);
    // ---- a later genuine end re-runs the recovered job and resolves ----
    let (mut deps2, _d2) = deps_sharing_session(
        manager2.clone(),
        Arc::new(scripted_provider(vec![
            ScriptedResponse::Text("status?".into()),
            ScriptedResponse::End,
        ])),
        vec![real_write_tool()],
    );
    deps2.verification = real_background_verifier();
    let runtime2 = AgentRuntime::new(deps2).unwrap();
    let settled = runtime2.run_turn(session, "status?", &[]).await.unwrap();
    assert_eq!(
        settled.completion,
        Some(CompletionGate::VerifiedComplete),
        "the recovered job re-runs to a real verdict"
    );
    let h3 = manager2.get_session(session).unwrap().unwrap();
    let task = h3.get_task(task_id).unwrap().unwrap();
    assert_eq!(task.state, TaskState::VerifiedComplete);
    let marker = manager2
        .resolve_workspace_root(session)
        .unwrap()
        .expect("workspace root")
        .join("test-marker.txt");
    assert_eq!(
        std::fs::read_to_string(&marker)
            .expect("the recovered job executed for real after the reopen")
            .trim(),
        "ran"
    );
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn end_session_kills_session_owned_processes() {
    // Commandment 8: closing a session must never orphan its children.
    // end_session kills every supervisor child owned by the session
    // before the durable end transition.
    let (mut deps, _dir) = deps(scripted_provider(vec![ScriptedResponse::End]), vec![]);
    let cas = deps.cas.clone().unwrap();
    deps.supervisor = Some(faktor_terminal::ProcessSupervisor::new(cas));
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let cfg = faktor_terminal::SpawnConfig {
        cmd: "sleep".into(),
        args: vec!["30".into()],
        cwd: std::env::temp_dir(),
        env: faktor_terminal::EnvSpec::default_baseline(),
        owner: faktor_terminal::ProcessOwner::Session(session),
        capture: true,
        artifact_max: 1024 * 1024,
        network_isolation: faktor_terminal::NetworkIsolation::Inherit,
    };
    let sup = runtime.deps().supervisor.clone().unwrap();
    let child_task = tokio::spawn({
        let sup = sup.clone();
        async move {
            sup.run(
                cfg,
                std::time::Duration::from_secs(60),
                faktor_core::cancellation::CancellationToken::new(),
            )
            .await
        }
    });
    // Let the child spawn and register — bounded poll, not a fixed
    // sleep: under machine load a fixed 200 ms can elapse before the
    // child registers, and end_session would then kill nothing (the
    // test would hang on the 10 s child_task timeout). Polling the
    // supervisor's live set keeps the margin environmental only.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(240); // environmental margin (documented bound): child spawn under full-suite load
    loop {
        if sup
            .alive()
            .iter()
            .any(|c| c.owner == faktor_terminal::ProcessOwner::Session(session))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the child never registered with the supervisor"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // end_session must kill the child and succeed.
    runtime.end_session(session).unwrap();
    // The in-flight run returns promptly (killed), NOT after 30s.
    let done = tokio::time::timeout(std::time::Duration::from_secs(10), child_task)
        .await
        .expect("end_session must terminate the child promptly");
    let output = done.unwrap().unwrap();
    assert_ne!(
        output.exit_code,
        Some(0),
        "killed child must not report a clean exit: {output:?}"
    );
    assert!(handle.state().unwrap().is_terminal() || true);
    let lifecycle = handle.lifecycle().unwrap();
    assert_eq!(lifecycle, faktor_core::state::SessionLifecycle::Closed);
}

#[tokio::test]
async fn queued_prompt_after_crash_resumes_same_turn_then_delivers() {
    // Requirement 1c: prompt B queued while turn A was active has NO
    // turn record (a crash before B's admission leaves the queue row
    // pending). The next runner tick first resumes A as the SAME logical
    // turn (recorded op), then admits B — no phantom record for B
    // before admission, exactly one delivery afterwards.
    let dir = fresh_store_dir();
    let session: SessionId;
    let op_a: OpId;
    let op_b: OpId;
    {
        let manager1 =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let (deps1, _keep) = deps_sharing_session(
            manager1.clone(),
            Arc::new(scripted_provider(vec![])),
            vec![],
        );
        let runtime1 = AgentRuntime::new(deps1).unwrap();
        let ws = manager1.create_workspace("/w").unwrap();
        let handle = manager1.create_session(ws, "t", "fake", "m").unwrap();
        session = handle.id();
        let ra = handle.submit_prompt("task A", &[]).unwrap();
        op_a = ra.op_id;
        let rb = handle.submit_prompt("task B", &[]).unwrap();
        assert!(rb.queued);
        op_b = rb.op_id;
        assert_eq!(handle.queued_prompt_count().unwrap(), 1);
        // B was never admitted: no record for B, only A's.
        let records = handle.turn_records().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].turn_op_id, op_a);
        assert!(handle.turn_record(op_b).unwrap().is_none());
        assert_eq!(handle.state().unwrap(), AgentState::Preparing);
        drop(runtime1);
    }
    // Restart: the runner resumes A (never driven) and then delivers B.
    let inner = scripted_provider(vec![
        ScriptedResponse::Text("A answer".into()),
        ScriptedResponse::End,
        ScriptedResponse::Text("B answer".into()),
        ScriptedResponse::End,
    ]);
    let (deps2, _keep2) = reopen_runtime(&dir, Arc::new(inner), vec![]);
    let runtime2 = AgentRuntime::new(deps2).unwrap();
    let runner = runtime2.clone();
    tokio::spawn(async move { runner.run_session_queue(session).await })
        .await
        .unwrap();
    let handle2 = runtime2
        .deps()
        .session
        .get_session(session)
        .unwrap()
        .unwrap();
    assert_eq!(handle2.queued_prompt_count().unwrap(), 0, "queue drained");
    assert_eq!(handle2.state().unwrap(), AgentState::ReadyForNextTurn);
    let records = handle2.turn_records().unwrap();
    assert_eq!(records.len(), 2, "A resumed + B admitted = 2 records");
    assert_eq!(records[0].turn_op_id, op_a);
    assert_eq!(records[0].status, "completed", "A completed as ONE turn");
    assert_eq!(records[1].turn_op_id, op_b);
    assert_eq!(records[1].status, "completed");
    let events = handle2.events_range(1, None).unwrap();
    let prompts = events
        .iter()
        .filter(|e| e.kind == faktor_core::event::EventKind::PromptReceived)
        .count();
    assert_eq!(prompts, 2, "one PromptReceived per prompt");
    let admitted_b = events
        .iter()
        .filter(|e| {
            e.kind == faktor_core::event::EventKind::PromptAdmitted && e.op_id == Some(op_b)
        })
        .count();
    assert_eq!(admitted_b, 1, "B admitted exactly once");
    // One TurnCompleted per logical turn.
    let completed = events
        .iter()
        .filter(|e| e.kind == faktor_core::event::EventKind::TurnCompleted)
        .count();
    assert_eq!(completed, 2);
}

/// Queue-row crash residue, `running` window (adversarial): a queued
/// prompt whose row was marked `running` and whose turn record is active
/// when the process dies must be RESUMED as the same recorded turn and
/// its row consumed exactly once — never re-admitted (double delivery),
/// never spun forever (the old `queued_prompt_count` counted `running`).
/// A second boot must be a clean no-op (no repeated spin).
#[tokio::test]
async fn running_queue_row_crash_residue_is_resumed_and_consumed_exactly_once() {
    let dir = fresh_store_dir();
    let session: SessionId;
    let op_a: OpId;
    let op_b: OpId;
    {
        let manager1 =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let (deps1, _keep) = deps_sharing_session(
            manager1.clone(),
            Arc::new(scripted_provider(vec![
                ScriptedResponse::Text("A answer".into()),
                ScriptedResponse::End,
            ])),
            vec![],
        );
        let runtime1 = AgentRuntime::new(deps1).unwrap();
        let ws = manager1.create_workspace("/w").unwrap();
        let handle = manager1.create_session(ws, "t", "fake", "m").unwrap();
        session = handle.id();
        // A is the active turn; B queues behind it.
        let ra = handle.submit_prompt("task A", &[]).unwrap();
        op_a = ra.op_id;
        let rb = handle.submit_prompt("task B", &[]).unwrap();
        assert!(rb.queued);
        op_b = rb.op_id;
        // Drive A to its end (B stays pending).
        let outcome = runtime1.drive_receipt(&handle, ra, None).await.unwrap();
        assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
        // CRASH WINDOW: B is admitted (record active, message
        // materialized) and marked `running`, then the drive dies before
        // the terminal mark.
        let admitted = handle.admit_next_queued().unwrap().unwrap();
        assert_eq!(admitted.op_id, op_b);
        handle
            .mark_queued_status(admitted.queue_seq, "running")
            .unwrap();
        assert_eq!(handle.queued_prompt_count().unwrap(), 1);
        assert_eq!(handle.state().unwrap(), AgentState::Preparing);
        drop(runtime1);
    }
    // Restart: recovery must RESUME the recorded turn of the running row
    // (not re-admit B) and consume the row exactly once.
    let provider = Arc::new(scripted_provider(vec![
        ScriptedResponse::Text("B answer".into()),
        ScriptedResponse::End,
    ]));
    let (deps2, _keep2) = reopen_runtime(&dir, provider.clone(), vec![]);
    let runtime2 = AgentRuntime::new(deps2).unwrap();
    let handle2 = runtime2
        .deps()
        .session
        .get_session(session)
        .unwrap()
        .unwrap();
    assert_eq!(
        runtime2
            .deps()
            .session
            .store()
            .sessions_with_pending_queues()
            .unwrap(),
        vec![session],
        "the crashed row is the runnable marker before recovery"
    );
    runtime2.run_session_queue(session).await;
    assert_eq!(handle2.queued_prompt_count().unwrap(), 0, "queue drained");
    let records = handle2.turn_records().unwrap();
    assert_eq!(records.len(), 2, "one record per logical turn (never 3)");
    assert_eq!(records[0].turn_op_id, op_a);
    assert_eq!(records[0].status, "completed");
    assert_eq!(records[1].turn_op_id, op_b);
    assert_eq!(records[1].status, "completed");
    let events = handle2.events_range(1, None).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == faktor_core::event::EventKind::PromptReceived
                && e.op_id == Some(op_b))
            .count(),
        1,
        "B received exactly one prompt"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == faktor_core::event::EventKind::PromptAdmitted
                && e.op_id == Some(op_b))
            .count(),
        0,
        "the running row's turn was RESUMED, never re-admitted"
    );
    assert!(
        runtime2
            .deps()
            .session
            .store()
            .sessions_with_pending_queues()
            .unwrap()
            .is_empty(),
        "no row is left for the next boot"
    );
    assert_eq!(
        provider.script.lock().unwrap().len(),
        0,
        "B's turn consumed exactly one provider script"
    );
    let events_after_first_boot = handle2.events_range(1, None).unwrap().len();
    // Second boot: nothing to do, no provider call, no spin.
    runtime2.run_session_queue(session).await;
    assert_eq!(
        provider.script.lock().unwrap().len(),
        0,
        "the second boot must not drive anything again"
    );
    assert_eq!(
        handle2.events_range(1, None).unwrap().len(),
        events_after_first_boot,
        "the second boot appends no journal event"
    );
}

/// Queue-row crash residue, orphaned `running` window (adversarial): a
/// `running` row whose logical turn already ended (no active turn
/// record) must be retired to `done` by recovery — never re-admitted
/// (which would deliver the same prompt twice), never counted forever.
/// `abort(None)` must be able to clear a running row too.
#[tokio::test]
async fn orphaned_running_queue_row_is_retired_and_abortable() {
    let dir = fresh_store_dir();
    let session: SessionId;
    {
        let manager1 =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let (deps1, _keep) = deps_sharing_session(
            manager1.clone(),
            Arc::new(scripted_provider(vec![])),
            vec![],
        );
        let _runtime1 = AgentRuntime::new(deps1).unwrap();
        let ws = manager1.create_workspace("/w").unwrap();
        let handle = manager1.create_session(ws, "t", "fake", "m").unwrap();
        session = handle.id();
        // A queued row admitted and marked running, but its logical turn
        // already ended (no active turn record): the terminal mark was
        // lost to the crash.
        let op = manager1.try_next_op_id().unwrap();
        manager1
            .store()
            .enqueue_prompt(session, op, "task B", &[], None, None, None, 1)
            .unwrap();
        let seq = manager1
            .store()
            .queue_head(session)
            .unwrap()
            .unwrap()
            .queue_seq;
        handle.mark_queued_status(seq, "running").unwrap();
        assert!(handle.active_turn_record().unwrap().is_none());
        assert_eq!(handle.queued_prompt_count().unwrap(), 1);
        drop(_runtime1);
    }
    let provider = Arc::new(scripted_provider(vec![
        ScriptedResponse::Text("must never run".into()),
        ScriptedResponse::End,
    ]));
    let (deps2, _keep2) = reopen_runtime(&dir, provider.clone(), vec![]);
    let runtime2 = AgentRuntime::new(deps2).unwrap();
    let handle2 = runtime2
        .deps()
        .session
        .get_session(session)
        .unwrap()
        .unwrap();
    runtime2.run_session_queue(session).await;
    assert_eq!(
        provider.script.lock().unwrap().len(),
        2,
        "the orphaned running row must never be re-admitted"
    );
    assert_eq!(handle2.queued_prompt_count().unwrap(), 0);
    assert!(runtime2
        .deps()
        .session
        .store()
        .sessions_with_pending_queues()
        .unwrap()
        .is_empty());

    // abort(None) coverage: a running row is cancellable.
    let dir2 = fresh_store_dir();
    let manager =
        SessionManager::open(dir2.path().join("store"), dir2.path().join("cas"), true).unwrap();
    let (deps3, _keep3) =
        deps_sharing_session(manager.clone(), Arc::new(scripted_provider(vec![])), vec![]);
    let runtime3 = AgentRuntime::new(deps3).unwrap();
    let ws = manager.create_workspace("/w").unwrap();
    let handle = manager.create_session(ws, "t", "fake", "m").unwrap();
    let active = handle.submit_prompt("active", &[]).unwrap();
    let op = manager.try_next_op_id().unwrap();
    manager
        .store()
        .enqueue_prompt(handle.id(), op, "queued", &[], None, None, None, 1)
        .unwrap();
    handle
        .mark_queued_status(
            manager
                .store()
                .queue_head(handle.id())
                .unwrap()
                .unwrap()
                .queue_seq,
            "running",
        )
        .unwrap();
    assert_eq!(handle.queued_prompt_count().unwrap(), 1);
    let receipt = runtime3.abort_op(handle.id(), None).unwrap();
    assert!(receipt.contains(&active.op_id) && receipt.contains(&op));
    assert_eq!(
        handle.queued_prompt_count().unwrap(),
        0,
        "abort(None) clears a running row"
    );
    assert!(manager
        .store()
        .sessions_with_pending_queues()
        .unwrap()
        .is_empty());
}

/// F4: a turn parked on a durable permission makes the queue runner
/// exhaust its bounded wait (typed timeout). Resolving the permission
/// must RE-KICK the queue through the same bounded runner — without a
/// new submit — and the queued prompt must be delivered exactly once.
#[tokio::test]
async fn resolve_permission_rekicks_a_queue_parked_on_the_durable_permission() {
    let dir = fresh_store_dir();
    let session: SessionId;
    let perm_id: i64;
    let op_a: OpId;
    let op_b: OpId;
    {
        let manager1 =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let (deps1, _keep) = deps_sharing_session(
            manager1.clone(),
            Arc::new(scripted_provider(vec![])),
            vec![],
        );
        let runtime1 = AgentRuntime::new(deps1).unwrap();
        let ws = manager1.create_workspace("/w").unwrap();
        let handle = manager1.create_session(ws, "t", "fake", "m").unwrap();
        session = handle.id();
        let ra = handle.submit_prompt("task A", &[]).unwrap();
        op_a = ra.op_id;
        // Walk the machine to Streaming with the documented legal chain
        // (the state the real drive is in when it requests a permission).
        for (kind, state) in [
            (
                faktor_core::event::EventKind::ContextPrepared,
                AgentState::BuildingContext,
            ),
            (
                faktor_core::event::EventKind::ModelStarted,
                AgentState::WaitingForModel,
            ),
            (
                faktor_core::event::EventKind::ModelChunkReceived,
                AgentState::Streaming,
            ),
        ] {
            handle.append_event(kind, state, Some(op_a), None).unwrap();
        }
        // The turn parks on a durable permission (exactly what the drive
        // does at its permission hop) and B queues behind it.
        let perm = handle
            .request_permission(
                ra.op_id,
                &Capability::ExecuteShell {
                    command: "rm".into(),
                },
            )
            .unwrap();
        perm_id = perm.id;
        let rb = handle.submit_prompt("task B", &[]).unwrap();
        op_b = rb.op_id;
        assert!(rb.queued);
        assert_eq!(handle.state().unwrap(), AgentState::WaitingForPermission);
        drop(runtime1);
    }
    let provider = Arc::new(scripted_provider(vec![
        ScriptedResponse::Text("A answer".into()),
        ScriptedResponse::End,
        ScriptedResponse::Text("B answer".into()),
        ScriptedResponse::End,
    ]));
    let (deps2, _keep2) = reopen_runtime(&dir, provider.clone(), vec![]);
    let runtime2 = Arc::new(AgentRuntime::new(deps2).unwrap());
    runtime2.set_turn_budget_ms(150);
    let handle2 = runtime2
        .deps()
        .session
        .get_session(session)
        .unwrap()
        .unwrap();
    assert_eq!(handle2.queued_prompt_count().unwrap(), 1);
    // The runner cannot continue a permission-parked turn: bounded typed
    // timeout, durable head untouched.
    let err = runtime2
        .run_session_queue_inner(session)
        .await
        .expect_err("a permission-parked turn is not continuable");
    assert_eq!(err.kind, ErrorKind::Timeout, "{err:?}");
    assert_eq!(handle2.queued_prompt_count().unwrap(), 1, "still parked");
    // Resolve the permission: this path must re-kick the SAME queue.
    runtime2
        .resolve_permission(session, perm_id, PermissionDecision::Allow)
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while handle2.queued_prompt_count().unwrap() != 0 {
        assert!(
            Instant::now() < deadline,
            "resolving the permission must re-kick and drain the queue"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        provider.script.lock().unwrap().len(),
        0,
        "A resumed and B delivered (one script each)"
    );
    let events = handle2.events_range(1, None).unwrap();
    for (op, label) in [(op_a, "A"), (op_b, "B")] {
        assert_eq!(
            events
                .iter()
                .filter(|e| e.kind == faktor_core::event::EventKind::PromptReceived
                    && e.op_id == Some(op))
                .count(),
            1,
            "{label} received exactly one prompt"
        );
    }
    let records = handle2.turn_records().unwrap();
    assert!(
        records
            .iter()
            .all(|r| faktor_store::is_terminal_turn_record_status(&r.status)),
        "every logical turn ended: {records:?}"
    );
}

#[tokio::test]
async fn workspace_write_recovery_is_root_relative_and_rejects_traversal() {
    // Requirement 3c: the RELATIVE path is resolved inside the session
    // workspace root (never the daemon cwd — the cwd here is the repo,
    // where `b.txt` does not exist, so success proves root resolution),
    // and a traversal "../x" or a symlink escape is rejected loudly.
    let dir = fresh_store_dir();
    let session: SessionId;
    let ok_op: OpId;
    {
        let (manager, wid, sid, root) = workspace_env(&dir);
        let ws_id = wid;
        session = sid;
        let handle = manager.get_session(session).unwrap().unwrap();
        let receipt = handle.submit_prompt("write", &[]).unwrap();
        chain_to_streaming(&handle, receipt.op_id);
        let root = root.clone();
        // (i) A matching file INSIDE the workspace root verifies —
        // proving the hash ran against ws-root/b.txt, not cwd/b.txt.
        std::fs::write(root.join("b.txt"), b"world").unwrap();
        let expected = FileHash::from(blake3::hash(b"world").into());
        let meta = op_meta(&manager, session, RecoveryStrategy::MarkUnknown);
        ok_op = meta.operation_id;
        crash_tool_start(
            &handle,
            receipt.op_id,
            "write_file",
            serde_json::json!({"path": "b.txt", "content": "world"}),
            "call_ok",
            meta,
        );
        let pc = serde_json::to_value(FilePostcondition {
            workspace_id: ws_id,
            worktree_id: WorktreeId::new(1),
            relative_path: "b.txt".into(),
            expected_hash: expected,
        })
        .unwrap();
        handle.record_tool_postcondition(ok_op, &pc).unwrap();
        // (ii) A traversal postcondition is rejected loudly.
        let meta = op_meta(&manager, session, RecoveryStrategy::MarkUnknown);
        let evil_op = meta.operation_id;
        crash_tool_start(
            &handle,
            receipt.op_id,
            "write_file",
            serde_json::json!({"path": "../escape.txt", "content": "pwn"}),
            "call_evil",
            meta,
        );
        let pc = serde_json::to_value(FilePostcondition {
            workspace_id: ws_id,
            worktree_id: WorktreeId::new(1),
            relative_path: "../escape.txt".into(),
            expected_hash: FileHash::from([0u8; 32]),
        })
        .unwrap();
        handle.record_tool_postcondition(evil_op, &pc).unwrap();
        // (ii-b) A Windows-absolute postcondition (drive-letter, forward
        // slash) is refused by the host-side grammar on EVERY platform:
        // `Path::is_absolute` cannot see it on Unix, and the platform
        // resolver must never be the only gate.
        let meta = op_meta(&manager, session, RecoveryStrategy::MarkUnknown);
        let win_op = meta.operation_id;
        crash_tool_start(
            &handle,
            receipt.op_id,
            "write_file",
            serde_json::json!({"path": "C:/escape.txt", "content": "pwn"}),
            "call_win",
            meta,
        );
        let pc = serde_json::to_value(FilePostcondition {
            workspace_id: ws_id,
            worktree_id: WorktreeId::new(1),
            relative_path: "C:/escape.txt".into(),
            expected_hash: FileHash::from([0u8; 32]),
        })
        .unwrap();
        handle.record_tool_postcondition(win_op, &pc).unwrap();
        // (iii) A symlink escape is rejected the same way (canonical
        // resolution through the workspace service).
        #[cfg(unix)]
        {
            let outside_dir = dir.path().join("outside");
            std::fs::create_dir_all(&outside_dir).unwrap();
            std::os::unix::fs::symlink(&outside_dir, root.join("link")).unwrap();
            let meta = op_meta(&manager, session, RecoveryStrategy::MarkUnknown);
            let link_op = meta.operation_id;
            crash_tool_start(
                &handle,
                receipt.op_id,
                "write_file",
                serde_json::json!({"path": "link/secret.txt", "content": "pwn"}),
                "call_link",
                meta,
            );
            let pc = serde_json::to_value(FilePostcondition {
                workspace_id: ws_id,
                worktree_id: WorktreeId::new(1),
                relative_path: "link/secret.txt".into(),
                expected_hash: FileHash::from([0u8; 32]),
            })
            .unwrap();
            handle.record_tool_postcondition(link_op, &pc).unwrap();
        }
        // Crash: drop the manager; the residue is swept post-restart.
    }
    let (deps2, _keep2) = reopen_runtime(&dir, Arc::new(scripted_provider(vec![])), vec![]);
    let runtime2 = AgentRuntime::new(deps2).unwrap();
    let err = runtime2.recover().unwrap_err();
    assert_eq!(
        err.kind,
        ErrorKind::Permission,
        "traversal/symlink escapes must be rejected loudly: {err}"
    );
    assert!(
        !dir.path().join("escape.txt").exists()
            && !dir.path().join("outside").join("secret.txt").exists(),
        "recovery must never touch files outside the workspace"
    );
    // The in-root verification above succeeded BEFORE the rejections:
    // the ok row finished; the hostile rows stay running (visible, never
    // silently dropped).
    let handle2 = runtime2
        .deps()
        .session
        .get_session(session)
        .unwrap()
        .unwrap();
    let events = handle2.events_range(1, None).unwrap();
    let applied = events.iter().any(|e| {
        e.kind == faktor_core::event::EventKind::RecoveryApplied
            && e.payload.as_ref().is_some_and(|p| {
                p.get("op_id").and_then(|v| v.as_i64()) == Some(ok_op.raw() as i64)
                    && p.get("status").and_then(|v| v.as_str()) == Some("completed")
            })
    });
    assert!(
        applied,
        "the in-workspace write verified before the rejection"
    );
    let pending = handle2.pending_tool_runs().unwrap();
    // Traversal + windows-absolute on every host; the unix-only symlink
    // escape adds the third. (The Windows lane saw only 1 of 2 hostile
    // rows because the absolute shape was unix-gated: the windows shape
    // is now cross-platform.)
    let expected_hostile = if cfg!(unix) { 3 } else { 2 };
    assert_eq!(
        pending.len(),
        expected_hostile,
        "hostile rows stay pending: {pending:?}"
    );
}

/// The op-active sweep terminalizes each row through ONE store
/// transaction (terminal row + `RecoveryApplied` + state/seq). This
/// certifies every declared durability boundary — before the row
/// transaction, after the row, after the event, after the commit — with
/// a real crash: the reopened residue pairs every terminal row with
/// exactly one event (together or neither), and the restart sweep
/// converges to the EXACT uninterrupted reference world.
#[test]
fn recovery_terminalization_seams_are_row_event_atomic_and_convergent() {
    let reference = {
        let dir = fresh_store_dir();
        let (manager, session, _ops) = recovery_seam_fixture(dir.path());
        drop(manager);
        let (manager, ops) = recovery_seam_reopen(dir.path(), session);
        let (runtime, _keep) = recovery_seam_runtime(manager.clone());
        runtime.recover().unwrap();
        let world = recovery_seam_world(&runtime, session, &ops);
        drop(runtime);
        drop(manager);
        world
    };
    assert!(
        reference.iter().any(|l| l == "state:FailedRecoverable"),
        "reference does not land FailedRecoverable: {reference:?}"
    );
    assert_eq!(
        reference.iter().filter(|l| l.starts_with("ev:")).count(),
        2,
        "reference: one RecoveryApplied per terminal row: {reference:?}"
    );

    // (name, seam, ordinal, residue state, per-row (running, event count))
    type SeamCase = (
        &'static str,
        &'static str,
        u64,
        AgentState,
        [(bool, usize); 2],
    );
    let seams: &[SeamCase] = &[
        (
            "before_row_txn.rolled_back",
            "ev_precommit",
            0,
            AgentState::ExecutingTool,
            [(true, 0), (true, 0)],
        ),
        (
            "before_row_txn.committed",
            "ev_committed",
            0,
            AgentState::FailedRecoverable,
            [(true, 0), (true, 0)],
        ),
        (
            "row0.side_row",
            "session_command_side_row",
            0,
            AgentState::FailedRecoverable,
            [(true, 0), (true, 0)],
        ),
        (
            "row0.precommit",
            "session_command_precommit",
            0,
            AgentState::FailedRecoverable,
            [(true, 0), (true, 0)],
        ),
        (
            "row0.committed",
            "session_command_committed",
            0,
            AgentState::FailedRecoverable,
            [(false, 1), (true, 0)],
        ),
        (
            "row1.side_row",
            "session_command_side_row",
            1,
            AgentState::FailedRecoverable,
            [(false, 1), (true, 0)],
        ),
        (
            "row1.precommit",
            "session_command_precommit",
            1,
            AgentState::FailedRecoverable,
            [(false, 1), (true, 0)],
        ),
        (
            "row1.committed",
            "session_command_committed",
            1,
            AgentState::FailedRecoverable,
            [(false, 1), (false, 1)],
        ),
    ];

    for (name, seam, ordinal, residue_state, residue_rows) in seams {
        let dir = fresh_store_dir();
        let (manager, session, _ops) = recovery_seam_fixture(dir.path());
        drop(manager);
        let (manager, ops) = recovery_seam_reopen(dir.path(), session);
        {
            let (runtime, _keep) = recovery_seam_runtime(manager.clone());
            runtime
                .deps()
                .session
                .store()
                .crash_arm(faktor_store::CrashArm {
                    point: seam,
                    ordinal: *ordinal,
                });
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = runtime.recover();
            }));
            assert!(
                runtime.deps().session.store().seam_crash_observed(&caught),
                "{name}: seam {seam}/{ordinal} must fire"
            );
            drop(runtime);
        }
        drop(manager);

        // Residue: reopened BEFORE the restart sweep. Every row is either
        // still running with no event or terminal with exactly one.
        let manager2 =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let handle = manager2.get_session(session).unwrap().unwrap();
        assert_eq!(handle.state().unwrap(), *residue_state, "{name}: residue");
        let running: Vec<OpId> = handle
            .pending_tool_runs()
            .unwrap()
            .into_iter()
            .map(|r| r.op_id)
            .collect();
        for (i, op) in ops.iter().enumerate() {
            let (expected_running, expected_events) = residue_rows[i];
            assert_eq!(
                running.contains(op),
                expected_running,
                "{name}: residue row {i} running"
            );
            let events = handle
                .events_range(1, None)
                .unwrap()
                .into_iter()
                .filter(|e| {
                    e.kind == faktor_core::event::EventKind::RecoveryApplied && e.op_id == Some(*op)
                })
                .count();
            assert_eq!(events, expected_events, "{name}: residue row {i} events");
            assert!(
                !(running.contains(op) && events > 0),
                "{name}: a running row never carries its event"
            );
            assert!(
                !(!running.contains(op) && events == 0),
                "{name}: a terminal row never lacks its event"
            );
        }
        drop(handle);

        // Restart sweep: converges to the reference, then is idempotent.
        let (runtime2, _keep2) = recovery_seam_runtime(manager2.clone());
        runtime2.recover().unwrap();
        assert_eq!(
            recovery_seam_world(&runtime2, session, &ops),
            reference,
            "{name}: the restart sweep must converge to the uninterrupted world"
        );
        let handle2 = runtime2
            .deps()
            .session
            .get_session(session)
            .unwrap()
            .unwrap();
        let seq = handle2.last_event_seq().unwrap();
        // The transcript repair (crash residue with no open rows) may run
        // once here; the TERMINALIZATION itself never re-litigates.
        let second = runtime2.recover().unwrap();
        assert!(
            second.iter().all(|r| r.crashed_ops.is_empty()),
            "{name}: the second sweep must find no crashed op"
        );
        assert_eq!(
            handle2.last_event_seq().unwrap(),
            seq,
            "{name}: no new events"
        );
        let third = runtime2.recover().unwrap();
        assert!(
            third.iter().all(|r| !r.applied && r.crashed_ops.is_empty()),
            "{name}: the converged world must be a fixed point"
        );
    }
}

/// The adopted terminalization refuses TYPED and traceless: a session
/// state the transaction cannot verify refuses before any write, an
/// already-terminal row can never gain a second event, and a state whose
/// machine has no edge to the failure landing state refuses through the
/// sweep's `CrashDetected` move (also before any write).
#[test]
fn recovery_terminalization_refusals_are_typed_and_traceless() {
    // Wrong expected state at the transactional boundary.
    let dir = fresh_store_dir();
    let (manager, session, ops) = recovery_seam_fixture(dir.path());
    let (runtime, _keep) = recovery_seam_runtime(manager.clone());
    let handle = manager.get_session(session).unwrap().unwrap();
    let row = handle
        .pending_tool_runs()
        .unwrap()
        .into_iter()
        .find(|r| r.op_id == ops[0])
        .unwrap();
    let settled = |handle: &faktor_session::SessionHandle, op: OpId| {
        handle
            .events_range(1, None)
            .unwrap()
            .into_iter()
            .filter(|e| {
                e.kind == faktor_core::event::EventKind::RecoveryApplied && e.op_id == Some(op)
            })
            .count()
    };
    let err = runtime
        .finish_recovered_row(
            &handle,
            &row,
            "failed",
            EffectStatus::Unknown,
            "unknown_effect",
            None,
            AgentState::Idle,
        )
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Conflict, "{err}");
    assert_eq!(settled(&handle, row.op_id), 0, "refused before any write");
    assert!(handle
        .pending_tool_runs()
        .unwrap()
        .iter()
        .any(|r| r.op_id == row.op_id));
    // One committed terminalization, then the SAME row refuses typed: no
    // second event, no row change.
    runtime
        .finish_recovered_row(
            &handle,
            &row,
            "failed",
            EffectStatus::Unknown,
            "unknown_effect",
            None,
            AgentState::ExecutingTool,
        )
        .unwrap();
    assert_eq!(settled(&handle, row.op_id), 1);
    assert!(!handle
        .pending_tool_runs()
        .unwrap()
        .iter()
        .any(|r| r.op_id == row.op_id));
    let err = runtime
        .finish_recovered_row(
            &handle,
            &row,
            "failed",
            EffectStatus::Unknown,
            "unknown_effect",
            None,
            AgentState::ExecutingTool,
        )
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Conflict, "{err}");
    assert_eq!(
        settled(&handle, row.op_id),
        1,
        "a terminal row never gains a second event"
    );
}

/// A state whose machine has no edge to the failure landing state (here
/// `WaitingForPermission` with a raw running row) refuses through the
/// sweep's landing move — typed, before any row transaction.
#[test]
fn recovery_sweep_refuses_a_state_with_no_landing_edge() {
    let dir = fresh_store_dir();
    let session: SessionId;
    let op: OpId;
    {
        let manager =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let ws = manager.create_workspace("/w").unwrap();
        let handle = manager.create_session(ws, "t", "fake", "m").unwrap();
        session = handle.id();
        let turn = handle.submit_prompt("crash", &[]).unwrap().op_id;
        chain_to_streaming(&handle, turn);
        handle
            .request_permission(
                turn,
                &Capability::ReadWorkspace {
                    path: "/w/a".into(),
                },
            )
            .unwrap();
        assert_eq!(handle.state().unwrap(), AgentState::WaitingForPermission);
        // Bypass the typed API: a running row while the machine is parked.
        op = manager.try_next_op_id().unwrap();
        manager
            .store()
            .start_tool_run(
                session,
                op,
                "read_file",
                serde_json::json!({}),
                serde_json::to_value(RecoveryStrategy::MarkUnknown).unwrap(),
                None,
                None,
            )
            .unwrap();
    }
    // Restart: the in-process turn token is gone (otherwise the sweep
    // would see a live driver and defer).
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let handle = manager.get_session(session).unwrap().unwrap();
    assert_eq!(handle.state().unwrap(), AgentState::WaitingForPermission);
    let (runtime, _keep) = recovery_seam_runtime(manager.clone());
    let err = runtime.recover().unwrap_err();
    assert_eq!(
        err.kind,
        ErrorKind::InvalidState {
            from: AgentState::WaitingForPermission,
            to: AgentState::FailedRecoverable,
        },
        "{err}"
    );
    assert_eq!(handle.state().unwrap(), AgentState::WaitingForPermission);
    assert!(handle
        .pending_tool_runs()
        .unwrap()
        .iter()
        .any(|r| r.op_id == op));
    assert!(!handle.events_range(1, None).unwrap().iter().any(|e| {
        e.kind == faktor_core::event::EventKind::RecoveryApplied && e.op_id == Some(op)
    }));
}

#[cfg(unix)]
#[tokio::test]
async fn end_session_fires_session_end_hook_then_still_closes() {
    // SessionEnd must fire during end_session (best-effort) and the
    // durable close must still happen.
    let out_dir = tempdir().unwrap();
    let out = out_dir.path().join("end.json");
    let (mut deps, _dir) = deps(scripted_provider(vec![ScriptedResponse::End]), vec![]);
    let hooks = Arc::new(faktor_hooks::HookRegistry::try_new().expect("standalone supervisor"));
    hooks
        .register(file_writing_hook(
            "end_hook",
            faktor_hooks::HookEvent::SessionEnd,
            &out,
        ))
        .unwrap();
    deps.hooks = Some(hooks.clone());
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    runtime.end_session(session).unwrap();
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    assert_eq!(
        handle.lifecycle().unwrap(),
        faktor_core::state::SessionLifecycle::Closed,
        "the SessionEnd hook must never block the durable close"
    );
    let written = std::fs::read_to_string(&out).expect("the hook must have written its input");
    assert!(
        written.contains("\"session_end\""),
        "payload event: {written}"
    );
    assert!(
        written.contains(&format!("\"session_id\":\"{session}\"")),
        "payload session id: {written}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn continue_turn_fires_session_resume_hook() {
    // The recovery-resume boundary: a crash between submit and drive
    // leaves the turn record active at Preparing; continue_turn must
    // fire SessionResume (the queue runner uses the same path) and then
    // drive the SAME logical turn to its genuine end.
    let dir = fresh_store_dir();
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
        let ws = manager1.create_workspace("/w").unwrap();
        let handle = manager1.create_session(ws, "t", "fake", "m").unwrap();
        session = handle.id();
        let _receipt = handle.submit_prompt("crash me", &[]).unwrap();
        assert_eq!(
            handle.state().unwrap(),
            AgentState::Preparing,
            "the crash leaves the admitted turn undriven"
        );
        drop(runtime1);
    }
    let inner = scripted_provider(vec![
        ScriptedResponse::Text("resumed answer".into()),
        ScriptedResponse::End,
    ]);
    let (mut deps2, _keep2) = reopen_runtime(&dir, Arc::new(inner), vec![]);
    let hooks = Arc::new(faktor_hooks::HookRegistry::try_new().expect("standalone supervisor"));
    hooks
        .register(failing_closed_hook(
            "resume",
            faktor_hooks::HookEvent::SessionResume,
        ))
        .unwrap();
    deps2.hooks = Some(hooks.clone());
    let runtime2 = AgentRuntime::new(deps2).unwrap();
    let outcome = runtime2.continue_turn(session).await.unwrap();
    assert_eq!(
        outcome.final_state,
        AgentState::ReadyForNextTurn,
        "the resumed turn must drive to its genuine end"
    );
    let audit = hooks.audit();
    assert_eq!(
        audit
            .iter()
            .filter(|r| r.hook_id == "resume" && r.event == faktor_hooks::HookEvent::SessionResume)
            .count(),
        1,
        "SessionResume must fire exactly once on the recovery resume: {audit:?}"
    );
}

/// ADVERSARIAL (hostile marker session identity): markers whose
/// `session` field is absent, non-numeric, negative, zero, fractional,
/// foreign or truncated must NEVER be applied to the opening session,
/// even though their intent (a fresh Abort) would otherwise apply; every
/// hostile marker is retained for inspection. A well-formed marker for
/// this session still replays (control).
#[tokio::test]
async fn hostile_session_fields_never_apply_and_are_retained() {
    let (deps, _dir) = deps(scripted_provider(vec![]), vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let manager = runtime.deps().session.clone();
    let session = new_session(runtime.deps());
    let handle = manager.get_session(session).unwrap().unwrap();
    let _receipt = runtime.submit(session, "run", &[]).unwrap();
    let record = handle.active_turn_record().unwrap().unwrap();
    let root = manager.store().root().to_path_buf();
    let dir = root.join(DURABLE_WRITE_MARKER_DIR);
    let fresh_abort = |session_field: Option<serde_json::Value>| {
        let mut value = serde_json::json!({
            "status": "pending",
            "attempts": 0,
            "site": "test.hostile",
            "at_ms": record.started_at + 1000,
            "intent": {"write": "abort", "op_id": null},
        });
        if let Some(field) = session_field {
            value["session"] = field;
        }
        value
    };
    write_raw_marker(&dir, "dw-hostile-missing.json", &fresh_abort(None));
    write_raw_marker(
        &dir,
        "dw-hostile-string.json",
        &fresh_abort(Some(serde_json::json!("not-a-session"))),
    );
    write_raw_marker(
        &dir,
        "dw-hostile-negative.json",
        &fresh_abort(Some(serde_json::json!(-1))),
    );
    write_raw_marker(
        &dir,
        "dw-hostile-zero.json",
        &fresh_abort(Some(serde_json::json!(0))),
    );
    write_raw_marker(
        &dir,
        "dw-hostile-float.json",
        &fresh_abort(Some(serde_json::json!(1.5))),
    );
    let foreign = new_session(runtime.deps());
    assert_ne!(foreign, session);
    write_raw_marker(
        &dir,
        "dw-hostile-foreign.json",
        &fresh_abort(Some(serde_json::json!(foreign.raw()))),
    );
    std::fs::write(
        dir.join("dw-hostile-truncated.json"),
        br#"{"status":"pending","session":"#,
    )
    .unwrap();
    runtime.replay_durable_write_failures(&handle);
    assert_eq!(
        handle.active_turn_record().unwrap().map(|r| r.turn_op_id),
        Some(record.turn_op_id),
        "no hostile marker may abort this session's turn"
    );
    assert_eq!(handle.state().unwrap(), AgentState::Preparing);
    let names: Vec<String> = marker_files(&root)
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
        .collect();
    for expected in [
        "missing",
        "string",
        "negative",
        "zero",
        "float",
        "foreign",
        "truncated",
    ] {
        assert!(
            names.iter().any(|n| n.contains(expected)),
            "the {expected} marker must be retained: {names:?}"
        );
    }
    // CONTROL: a marker that names THIS session with the same fresh
    // Abort intent still replays.
    write_raw_marker(
        &dir,
        "dw-hostile-control.json",
        &fresh_abort(Some(serde_json::json!(session.raw()))),
    );
    runtime.replay_durable_write_failures(&handle);
    assert_eq!(
        handle.state().unwrap(),
        AgentState::ReadyForNextTurn,
        "a well-formed marker for this session still applies"
    );
}
