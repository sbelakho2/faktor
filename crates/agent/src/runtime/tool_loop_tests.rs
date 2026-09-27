//! `runtime::tool_loop_tests`: out-of-line tests.

#![allow(unused_imports)]

use super::*;
use crate::runtime::fixtures_tests::*;
use crate::runtime::tests::*;
use crate::*;

/// `tools_bundle_for_turn` over an empty history and fresh session is
/// byte-identical to the historical `bundle_for_phase`.
#[tokio::test]
async fn runtime_bundle_with_no_activation_matches_the_historical_bundle() {
    let (mut deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::Text("ok".into()),
            ScriptedResponse::End,
        ]),
        vec![],
    );
    deps.tools = Arc::new(lazy_registry());
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let handle = runtime
        .deps()
        .session
        .get_session(session)
        .unwrap()
        .unwrap();
    let caps = ModelCapabilities::default();
    let baseline = ToolRegistry::new().bundle_for_phase(RouterPhase::Implement, &caps);
    let bundle = runtime.tools_bundle_for_turn(&handle, &[], &caps);
    assert_eq!(
        serde_json::to_vec(&baseline).unwrap(),
        serde_json::to_vec(&bundle).unwrap()
    );
    assert_eq!(baseline.bundle_hash(), bundle.bundle_hash());
}

/// Dispatch membership guard: the model names a REGISTERED lazy tool
/// (`source_market`) that is NOT in the turn's active bundle. The call is
/// refused typed BEFORE the permission hop (no permission request row, no
/// `ToolRequested` event) and BEFORE any execution; the refusal names the
/// tool and answers the call (never dangling), and it never leaks the
/// tool's schema or description.
#[tokio::test]
async fn inactive_lazy_tool_named_by_the_model_is_refused_before_any_execution() {
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (mut deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "market".into(),
                name: "source_market".into(),
                input: serde_json::json!({"op": "search", "q": "TPS5430DDAR"}),
            },
            ScriptedResponse::End,
        ]),
        vec![],
    );
    deps.tools = Arc::new(counting_lazy_registry(executions.clone()));
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    // An ordinary prompt carries no activation signal: the bundle the
    // request was planned with cannot contain `source_market`.
    let outcome = runtime
        .run_turn(session, "fix the parser in src/parser.rs", &[])
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_refusal_answered(
        &runtime,
        session,
        "market",
        "not_in_active_bundle",
        "source_market",
        &executions,
    );
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    // The permission hop itself was skipped: no ToolRequested event and
    // no durable permission row can exist for the refused call.
    let events = handle.events_range(1, None).unwrap();
    assert!(
        !events
            .iter()
            .any(|e| e.kind == faktor_core::event::EventKind::ToolRequested),
        "the membership guard must refuse before the permission hop"
    );
    // The refusal carries the NAME only: schema/description canaries stay
    // out of the durable (model-visible) result.
    let (excerpt, exit) = tool_result_for(&handle, "market").expect("answered");
    assert_eq!(exit, Some(1));
    assert!(
        !excerpt.contains("CANARY"),
        "the refusal must not leak schema/description: {excerpt}"
    );
}

/// The same call executes once the tool is ACTIVATED: a decisive signal
/// in the newest user text puts `source_market` into the bundle the
/// request is planned with, so dispatch accepts it.
#[tokio::test]
async fn activated_lazy_tool_is_dispatchable_and_executes() {
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (mut deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "market".into(),
                name: "source_market".into(),
                input: serde_json::json!({"op": "search", "q": "TPS5430DDAR"}),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ]),
        vec![],
    );
    deps.tools = Arc::new(counting_lazy_registry(executions.clone()));
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let outcome = runtime
        .run_turn(session, "please source this part: TPS5430DDAR", &[])
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_eq!(
        executions.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "an activated lazy tool must execute"
    );
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let (excerpt, exit) = tool_result_for(&handle, "market").expect("answered");
    assert_eq!(exit, Some(0));
    assert_eq!(excerpt, "sourced");
    assert!(dangling_tool_calls(&handle).is_empty());
    let load = runtime.load_tool_activation(&handle);
    assert!(load.found() && load.set.is_active("source_market"));
}

/// The bounded durable activation scan must stay LOUD when it cannot
/// reach the fact: 16x200 newest facts newer than the activation fact
/// push it out of the window. The scan keeps the safe inactive default
/// AND emits the typed diagnostic (observable here through the test
/// sink) — never a silent revert.
#[tokio::test]
async fn activation_scan_bound_exhaustion_is_loud_and_defaults_inactive() {
    let (mut deps, _dir) = deps(scripted_provider(vec![ScriptedResponse::End]), vec![]);
    deps.tools = Arc::new(lazy_registry());
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let root = runtime.deps().session.store().root().to_path_buf();
    activation_scan_diagnostics_tests::clear(&root);

    // A decisive activation fact first ...
    handle
        .upsert_memory_fact(
            TOOL_ACTIVATION_FACT_KIND,
            TOOL_ACTIVATION_FACT_KEY,
            "[\"source_market\"]",
        )
        .unwrap();
    // ... then enough strictly-NEWER facts to exhaust the page bound.
    let seeds: Vec<(String, String, String)> =
        (0..TOOL_ACTIVATION_MAX_PAGES * TOOL_ACTIVATION_PAGE as usize + 1)
            .map(|i| ("zzz_seed".to_string(), format!("k{i:05}"), "v".to_string()))
            .collect();
    let seed_refs: Vec<(&str, &str, &str)> = seeds
        .iter()
        .map(|(kind, key, value)| (kind.as_str(), key.as_str(), value.as_str()))
        .collect();
    runtime
        .deps()
        .session
        .store()
        .upsert_memory_facts(session, &seed_refs)
        .unwrap();

    let load = runtime.load_tool_activation(&handle);
    assert_eq!(load.scan, ToolActivationScan::BoundExhausted);
    assert!(!load.found());
    assert!(
        load.set.is_empty(),
        "the safe inactive default must apply: {load:?}"
    );
    assert_eq!(
        activation_scan_diagnostics_tests::events_for(&root),
        vec![
            activation_scan_diagnostics_tests::Diagnostic::BoundExhausted {
                pages: TOOL_ACTIVATION_MAX_PAGES,
                page_size: TOOL_ACTIVATION_PAGE,
            }
        ],
        "bound exhaustion must be observable, never silent"
    );

    // The turn's bundle honors the inactive default (the exhausted scan
    // cannot resurrect the tool).
    let caps = ModelCapabilities::default();
    let bundle = runtime.tools_bundle_for_turn(&handle, &[], &caps);
    assert!(
        !bundle.tool_names().contains(&"source_market"),
        "the safe inactive default must keep the lazy tool out of the bundle"
    );
}

#[tokio::test]
async fn tool_call_executes_and_continues() {
    let (deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "echo".into(),
                input: serde_json::json!({"x": 1}),
            },
            ScriptedResponse::Text("after tool".into()),
            ScriptedResponse::End,
        ]),
        vec![echo_tool()],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let outcome = runtime.run_turn(session, "use echo", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let page = handle.messages_page(None, 20).unwrap();
    let has_tool_result = page
        .messages
        .iter()
        .flat_map(|m| m.parts.iter())
        .any(|p| matches!(p, faktor_protocol::native::Part::ToolResult { .. }));
    assert!(has_tool_result, "tool result part must be durable");
    // Tool ran exactly once (never replayed).
    let runs = handle.pending_tool_runs().unwrap();
    assert!(runs.is_empty());
}

#[tokio::test]
async fn tool_ctx_identity_reads_the_session_row_not_hardcoded_ids() {
    // P1: tools were getting FAKE worktree/task identities because the
    // runtime hardcoded WorktreeId::new(1)/TaskId::new(1). The session
    // row (v8) is the single source of truth: a standalone session keeps
    // the DOCUMENTED 1/1 default, and a session adopted onto a real
    // worktree passes the REAL ids to every ToolRunCtx — durably, so a
    // reopened manager sees the same row.
    fn probe_tool(captured: &Arc<std::sync::Mutex<Vec<WorkspaceIdentity>>>) -> Tool {
        let cap = captured.clone();
        Tool {
            name: "probe".into(),
            description: "records ctx identity".into(),
            input_schema: serde_json::json!({"type": "object"}),
            resource_class: faktor_core::resource::ResourceClass::Cpu,
            capability: None,
            recovery_hint: RecoveryHint::Idempotent,
            path_args: vec![],
            execute: Arc::new(move |ctx, _args| {
                let cap = cap.clone();
                Box::pin(async move {
                    cap.lock().unwrap().push(ctx.identity);
                    Ok(ToolOutcome::default())
                })
            }),
        }
    }
    fn one_probe_turn_script() -> Arc<dyn faktor_provider::Provider> {
        Arc::new(scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "probe".into(),
                input: serde_json::json!({}),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ]))
    }
    let dir = fresh_store_dir();
    let captured: Arc<std::sync::Mutex<Vec<WorkspaceIdentity>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    // (i) A plain create_session is the documented STANDALONE default:
    //     1/1, never a fake hardcoded id.
    {
        let manager =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let (deps, _keep) = deps_sharing_session(
            manager.clone(),
            one_probe_turn_script(),
            vec![probe_tool(&captured)],
        );
        let runtime = AgentRuntime::new(deps).unwrap();
        let ws = manager.create_workspace("/w").unwrap();
        let plain = manager.create_session(ws, "plain", "fake", "m").unwrap();
        runtime.run_turn(plain.id(), "probe", &[]).await.unwrap();
        assert_eq!(
            captured.lock().unwrap()[0],
            WorkspaceIdentity::new(ws, WorktreeId::new(1), TaskId::new(1)),
            "standalone sessions keep the documented 1/1 identity"
        );
    }
    // (ii) An adopted session's REAL worktree/task ids flow into the ctx.
    let adopted_id = {
        let manager =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let (deps, _keep) = deps_sharing_session(
            manager.clone(),
            one_probe_turn_script(),
            vec![probe_tool(&captured)],
        );
        let runtime = AgentRuntime::new(deps).unwrap();
        let ws = manager.create_workspace("/w").unwrap();
        let adopted = manager.create_session(ws, "adopted", "fake", "m").unwrap();
        manager
            .adopt_identity(adopted.id(), WorktreeId::new(7), TaskId::new(9))
            .unwrap();
        runtime.run_turn(adopted.id(), "probe", &[]).await.unwrap();
        assert_eq!(
            captured.lock().unwrap()[1],
            WorkspaceIdentity::new(ws, WorktreeId::new(7), TaskId::new(9)),
            "the tool ctx must carry the session row's real identity"
        );
        adopted.id()
    };
    // (iii) The identity is durable: a reopened manager reads the same
    // adopted row.
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let row = manager
        .get_session(adopted_id)
        .unwrap()
        .unwrap()
        .row()
        .unwrap();
    assert_eq!(
        row.worktree_id,
        WorktreeId::new(7),
        "adoption survives reopen"
    );
    assert_eq!(row.task_id, TaskId::new(9));
    assert_eq!(
        manager
            .get_session(adopted_id)
            .unwrap()
            .unwrap()
            .identity()
            .unwrap(),
        WorkspaceIdentity::new(row.workspace_id, WorktreeId::new(7), TaskId::new(9))
    );
}

#[tokio::test]
async fn denied_tool_call_is_answered_with_typed_result_and_turn_continues() {
    // Permission refusal: the interactive hop denied the call. The
    // transcript must answer the call with the typed denial and the turn
    // must continue through the legal walk (ReadyForNextTurn) — never
    // InvalidState, never a dangling call, never an execution.
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (mut deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "echo".into(),
                input: serde_json::json!({"x": 1}),
            },
            ScriptedResponse::End,
        ]),
        vec![counting_echo_tool(executions.clone())],
    );
    deps.permission_requester = Arc::new(DenyEveryTool);
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let outcome = runtime.run_turn(session, "use echo", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_refusal_answered(
        &runtime,
        session,
        "c1",
        "permission_denied",
        "permission denied: echo",
        &executions,
    );
}

#[tokio::test]
async fn crash_mid_mixed_batch_resumes_without_dangling_or_blind_rerun() {
    // Crash window of the mixed batch: the approved sibling executed and
    // its result is durable, the denial decision is journaled, but the
    // denied call's typed result write was lost. The next open repairs
    // the dangling call BEFORE the next wire request, the turn completes,
    // and the approved sibling is never blindly re-run.
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
    durable_faults_tests::arm(runtime.deps().session.store().root(), DW_SITE_DENIAL_RESULT);
    let err = runtime
        .run_turn(session, "use both", &[])
        .await
        .expect_err("the lost denial result must surface");
    assert_eq!(err.kind, ErrorKind::Store, "{err:?}");
    assert_eq!(
        approved_execs.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the approved sibling executed before the crash"
    );
    assert_eq!(denied_execs.load(std::sync::atomic::Ordering::SeqCst), 0);
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let (approved, exit) = tool_result_for(&handle, "ok").expect("approved result survived");
    assert_eq!(exit, Some(0));
    assert_eq!(approved, "echo: {\"x\":1}");
    assert!(handle.pending_tool_runs().unwrap().is_empty());
    assert!(
        handle
            .events_range(1, None)
            .unwrap()
            .iter()
            .any(|e| e.kind == faktor_core::event::EventKind::PermissionDenied),
        "the denial decision is durable even though its result was lost"
    );
    assert!(tool_result_for(&handle, "refused").is_none());
    assert_eq!(dangling_tool_calls(&handle), vec!["refused".to_string()]);
    assert_eq!(
        handle.state().unwrap(),
        AgentState::FailedRecoverable,
        "the interrupted batch is honest history, never silently ready"
    );
    // Next open: the repair answers the lost call before the next wire
    // request; the session (failed recoverably, never silently ready)
    // accepts the next turn and completes it.
    let outcome = runtime.run_turn(session, "continue", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    assert!(dangling_tool_calls(&handle).is_empty());
    assert_eq!(
        approved_execs.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the approved sibling is never blindly re-run"
    );
    assert_eq!(denied_execs.load(std::sync::atomic::Ordering::SeqCst), 0);
    let (repaired, exit) = tool_result_for(&handle, "refused").expect("repaired result");
    assert_eq!(exit, Some(1));
    assert!(repaired.contains("(interrupted)"), "{repaired}");
    // The repaired call reached the wire: the next request carries BOTH
    // the surviving approved result and the repaired refusal.
    assert_eq!(
        wire_tool_results(&recorder),
        vec![("ok".to_string(), false), ("refused".to_string(), true)],
        "no dangling call ever reaches the wire"
    );
    handle
        .replay_journal()
        .expect("the journal replays lawfully");
}

#[tokio::test]
async fn unknown_tool_refusal_is_answered_with_typed_result() {
    // The model hallucinated a tool name: the refusal is journaled,
    // typed, and answered — no run row, no execution.
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "ghost_1".into(),
                name: "ghost_tool".into(),
                input: serde_json::json!({"x": 1}),
            },
            ScriptedResponse::End,
        ]),
        vec![counting_echo_tool(executions.clone())],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let outcome = runtime
        .run_turn(session, "call the ghost", &[])
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_refusal_answered(
        &runtime,
        session,
        "ghost_1",
        "unknown_tool",
        "unknown tool: ghost_tool",
        &executions,
    );
}

#[tokio::test]
async fn approved_tool_call_result_is_unchanged_by_the_denial_fix() {
    // CONTROL: the approved path is byte-identical — the tool executes,
    // its result carries the real output (exit 0, no denial tag), and
    // the documented interior hop (Validating -> UpdatingMemory ->
    // WaitingForModel) still runs for the continuing turn.
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "ok".into(),
                name: "echo".into(),
                input: serde_json::json!({"x": 1}),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ]),
        vec![counting_echo_tool(executions.clone())],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let outcome = runtime.run_turn(session, "use echo", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 1);
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let (excerpt, exit) = tool_result_for(&handle, "ok").expect("approved call answered");
    assert_eq!(exit, Some(0), "{excerpt}");
    assert_eq!(excerpt, "echo: {\"x\":1}", "approved result byte-identical");
    assert!(!excerpt.contains("denied") && !excerpt.contains("unresolved"));
    assert!(dangling_tool_calls(&handle).is_empty());
    let events = handle.events_range(1, None).unwrap();
    let last_completed = events
        .iter()
        .rposition(|e| {
            e.kind == faktor_core::event::EventKind::ToolCompleted
                && e.state == AgentState::Validating
        })
        .expect("the completed finish is journaled");
    let after: Vec<AgentState> = events[last_completed + 1..]
        .iter()
        .take(2)
        .map(|e| e.state)
        .collect();
    assert_eq!(
        after,
        vec![AgentState::UpdatingMemory, AgentState::WaitingForModel],
        "the approved interior hop is unchanged"
    );
}

#[tokio::test]
async fn crash_between_denial_and_result_write_resumes_without_a_dangling_call() {
    // The refusal decision (journaled) landed but the typed result write
    // was lost to a crash. The transcript is momentarily dangling; the
    // next session open (recovery) answers the call with the typed
    // `interrupted` result BEFORE the next model request is built — the
    // wire never carries a call with no result.
    let fake = scripted_provider(vec![
        ScriptedResponse::ToolCall {
            id: "leak_1".into(),
            name: "write_file".into(),
            input: serde_json::json!({ "path": "creds.txt", "content": SK_SAMPLE }),
        },
        ScriptedResponse::Text("recovered".into()),
        ScriptedResponse::End,
    ]);
    let recorder = RecordingProvider::new(Arc::new(fake));
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (deps, _dir) = deps_with(
        recorder.clone(),
        vec![counting_write_tool(executions.clone())],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    durable_faults_tests::arm(runtime.deps().session.store().root(), DW_SITE_DENIAL_RESULT);
    let err = runtime
        .run_turn(session, "store the key", &[])
        .await
        .expect_err("the lost denial result must surface");
    assert_eq!(err.kind, ErrorKind::Store, "{err:?}");
    assert_eq!(
        executions.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the refusal never executed the tool"
    );
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    assert!(
        tool_result_for(&handle, "leak_1").is_none(),
        "the crash window left the call unanswered"
    );
    assert_eq!(dangling_tool_calls(&handle), vec!["leak_1".to_string()]);
    // Next open: recovery answers the dangling call before the model is
    // called again, and the repaired result reaches the second request.
    let outcome = runtime.run_turn(session, "continue", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let dangling = dangling_tool_calls(&handle);
    assert!(dangling.is_empty(), "dangling after recovery: {dangling:?}");
    let (excerpt, exit) = tool_result_for(&handle, "leak_1").expect("repaired result");
    assert_eq!(exit, Some(1));
    assert!(excerpt.contains("(interrupted)"), "{excerpt}");
    assert!(
        !excerpt.contains(SK_SAMPLE),
        "no secret in the repaired result: {excerpt}"
    );
    let requests = recorder.requests();
    assert_eq!(requests.len(), 2, "one provider request per turn");
    let answered = requests[1].messages.iter().any(|m| {
        m.content.iter().any(|p| {
            matches!(&p.kind, ContentKind::ToolResult { content, is_error }
                    if content.contains("(interrupted)") && *is_error)
        })
    });
    assert!(
        answered,
        "the repaired tool_result must ride the next wire request"
    );
}

#[tokio::test]
async fn stream_death_is_state_aware_no_replay() {
    // Provider dies mid-stream after a tool call ran: effect marked
    // unknown; the turn lands NeedsUserInput, never a blind replay.
    let (deps, _dir) = deps(
        FakeProvider::with_script(
            "fake",
            ModelCapabilities {
                tools: true,
                ..Default::default()
            },
            vec![
                ScriptedResponse::ToolCall {
                    id: "c1".into(),
                    name: "echo".into(),
                    input: serde_json::json!({"x": 1}),
                },
                ScriptedResponse::Text("partial".into()),
                ScriptedResponse::Die(ProviderError::new(
                    faktor_provider::ProviderErrorKind::Network,
                    "connection vanished",
                )),
            ],
        ),
        vec![echo_tool()],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let outcome = runtime.run_turn(session, "x", &[]).await.unwrap();
    assert!(matches!(
        outcome.final_state,
        AgentState::NeedsUserInput | AgentState::FailedRecoverable
    ));
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    assert!(
        handle.pending_tool_runs().unwrap().is_empty(),
        "recovery resolves pending runs"
    );
}

#[tokio::test]
async fn tool_results_are_required_by_the_second_request() {
    // The audit's semantic test: the FIRST stream yields one tool call;
    // the SECOND request (after the tool executed) MUST carry the tool
    // result back to the model — the wrapper refuses the stream with a
    // Malformed error when it is missing, so the turn can only complete
    // once the request shape is correct. On the old code the second
    // request omits the result and this test fails.
    let inner = scripted_provider(vec![
        ScriptedResponse::ToolCall {
            id: "call_1".into(),
            name: "echo".into(),
            input: serde_json::json!({"x": 1}),
        },
        ScriptedResponse::Text("after tool".into()),
        ScriptedResponse::End,
    ]);
    let captured: Arc<std::sync::Mutex<Vec<GenericAgentRequest>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let cap = captured.clone();
    let wrapper = InspectingProvider::new(Arc::new(inner), move |n, req| {
        if n == 0 {
            // No tool has run yet: a tool result on the first request is
            // as corrupt as a missing one on the second.
            let leaked = req.messages.iter().any(|m| {
                m.content
                    .iter()
                    .any(|c| matches!(c.kind, ContentKind::ToolResult { .. }))
            });
            if leaked {
                return Err("tool result present before any tool ran".into());
            }
        } else {
            let user_has_result = req.messages.iter().any(|m| {
                m.role == Role::User
                    && m.content.iter().any(|c| {
                        matches!(
                            &c.kind,
                            ContentKind::ToolResult { content, is_error }
                                if c.tool_call_id.as_deref() == Some("call_1")
                                    && content == "echo: {\"x\":1}"
                                    && !is_error
                        )
                    })
            });
            if !user_has_result {
                return Err("tool result missing".into());
            }
            let assistant_has_call = req.messages.iter().any(|m| {
                m.role == Role::Assistant
                    && m.content.iter().any(|c| {
                        matches!(
                            &c.kind,
                            ContentKind::ToolCall { id, name, input }
                                if id == "call_1"
                                    && name == "echo"
                                    && *input == serde_json::json!({"x": 1})
                        )
                    })
            });
            if !assistant_has_call {
                return Err("tool call missing".into());
            }
        }
        cap.lock().unwrap().push(req.clone());
        Ok(())
    });
    let (deps, _dir) = deps_with(Arc::new(wrapper), vec![echo_tool()]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let outcome = runtime.run_turn(session, "use echo", &[]).await.unwrap();
    assert_eq!(
        outcome.final_state,
        AgentState::ReadyForNextTurn,
        "the turn completes only when the tool result rides the second request"
    );
    // One logical turn (prompt → tool → model) counts exactly ONE turn,
    // even though two provider requests were made (audit round 6).
    assert_eq!(outcome.turns, 1);
    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 2, "exactly two provider requests expected");
    let assistant = requests[1]
        .messages
        .iter()
        .find(|m| m.role == Role::Assistant)
        .expect("second request carries the assistant tool call");
    assert!(
        assistant
            .content
            .iter()
            .any(|c| matches!(&c.kind, ContentKind::ToolCall { id, .. } if id == "call_1")),
        "the assistant message must carry the completed tool call"
    );
}

#[tokio::test]
async fn history_reconstruction_splits_tool_results_into_user_role() {
    // Durable state shaped exactly like the turn loop writes it:
    // 1) assistant message: text + completed tool call part,
    // 2) assistant message: ONLY a tool result part (run_tool_calls),
    // 3) assistant message: a pending tool call (never sent back),
    // 4) assistant message: a failing tool result (is_error on the wire).
    let (deps, _dir) = deps(scripted_provider(vec![ScriptedResponse::End]), vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let m1 = handle
        .put_message(1, "assistant", serde_json::json!({}))
        .unwrap();
    handle.put_text_part(m1, "calling the tool").unwrap();
    handle
        .put_tool_call_part(
            m1,
            "call_1",
            "echo",
            serde_json::json!({"x": 1}),
            "completed",
        )
        .unwrap();
    let m2 = handle
        .put_message(2, "assistant", serde_json::json!({}))
        .unwrap();
    handle
        .put_tool_result_part(
            m2,
            "call_1",
            &ToolResultBody {
                excerpt: "echo: {\"x\":1}".into(),
                exit_code: Some(0),
                artifact: None,
                slice_hint: None,
            },
        )
        .unwrap();
    let m3 = handle
        .put_message(3, "assistant", serde_json::json!({}))
        .unwrap();
    handle
        .put_tool_call_part(m3, "call_2", "echo", serde_json::json!({"x": 2}), "pending")
        .unwrap();
    let m4 = handle
        .put_message(4, "assistant", serde_json::json!({}))
        .unwrap();
    handle
        .put_tool_result_part(
            m4,
            "call_3",
            &ToolResultBody {
                excerpt: "boom".into(),
                exit_code: Some(1),
                artifact: None,
                slice_hint: None,
            },
        )
        .unwrap();

    let msgs = runtime
        .history_messages(&handle, &ContextBudget::default())
        .await
        .unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0].role, Role::Assistant);
    assert!(matches!(
        &msgs[0].content[0].kind,
        ContentKind::Text { text } if text == "calling the tool"
    ));
    assert!(matches!(
        &msgs[0].content[1].kind,
        ContentKind::ToolCall { id, name, input }
            if id == "call_1" && name == "echo" && *input == serde_json::json!({"x": 1})
    ));
    assert_eq!(
        msgs[1].role,
        Role::User,
        "tool results move to the user role"
    );
    assert_eq!(
        msgs[1].content[0].tool_call_id.as_deref(),
        Some("call_1"),
        "the tool result must name the call it answers"
    );
    assert!(matches!(
        &msgs[1].content[0].kind,
        ContentKind::ToolResult { content, is_error }
            if content == "echo: {\"x\":1}" && !is_error
    ));
    assert_eq!(msgs[2].role, Role::User);
    assert!(
        matches!(
            &msgs[2].content[0].kind,
            ContentKind::ToolResult { is_error, .. } if *is_error
        ),
        "a non-zero exit code is an error result"
    );
    assert!(
        !msgs.iter().any(|m| m.content.iter().any(|c| matches!(
            &c.kind,
            ContentKind::ToolCall { id, .. } if id == "call_2"
        ))),
        "pending tool calls never reach the wire"
    );
}

#[test]
fn run_tool_calls_fills_reads_writes_from_tool_ownership() {
    // The scheduler's ownership sets must come from the tool's declared
    // path args: write_file with a path arg writes that path (the audit
    // requires the ScheduledOp's reads/writes to be non-empty so edit
    // overlap serialization works).
    let write_file = Arc::new(Tool {
        name: "write_file".into(),
        description: "w".into(),
        input_schema: serde_json::json!({}),
        resource_class: faktor_core::resource::ResourceClass::DiskWrite,
        capability: None,
        recovery_hint: RecoveryHint::WorkspaceWrite,
        path_args: vec!["path".into()],
        execute: Arc::new(|_ctx, _args| Box::pin(async move { Ok(ToolOutcome::default()) })),
    });
    let (reads, writes) = ownership_sets(
        &write_file,
        &serde_json::json!({"path": "src/main.rs", "content": "x"}),
    );
    assert!(!writes.is_empty(), "write_file must declare its write path");
    assert!(reads.is_empty(), "write_file declares no reads");
    let (reads, read_writes) = ownership_sets(
        &Arc::new(Tool {
            path_args: vec!["path".into()],
            resource_class: faktor_core::resource::ResourceClass::DiskRead,
            ..(write_file.as_ref()).clone()
        }),
        &serde_json::json!({"path": "src/main.rs"}),
    );
    assert!(!reads.is_empty(), "read_file must declare its read path");
    assert!(read_writes.is_empty());
}

#[tokio::test]
async fn mixed_tool_batch_completes_lawfully_with_per_tool_records() {
    // DEFECT REPRODUCER (mixed success+failure batch): one scheduled
    // tool succeeds and one fails recoverably in the SAME batch. The
    // failed finish moves the machine to `FailedRecoverable`; the batch
    // still has a completed tool, so the turn continues and the old
    // code tried `FailedRecoverable -> UpdatingMemory` — an illegal edge
    // — and the whole turn died with `InvalidState` instead of recording
    // the per-tool outcomes. The FAILING call is submitted FIRST on
    // purpose: the completed finishes must resolve before the failed
    // ones (the reverse order is itself illegal), so this locks the
    // order-independent resolution.
    let (deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "bad".into(),
                name: "explode".into(),
                input: serde_json::json!({"x": 2}),
            },
            ScriptedResponse::ToolCall {
                id: "ok".into(),
                name: "echo".into(),
                input: serde_json::json!({"x": 1}),
            },
            ScriptedResponse::Text("recovered".into()),
            ScriptedResponse::End,
        ]),
        vec![echo_tool(), failing_tool()],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let outcome = runtime.run_turn(session, "do work", &[]).await.unwrap();
    assert_eq!(
        outcome.final_state,
        AgentState::ReadyForNextTurn,
        "a mixed batch is a per-tool outcome, not a turn failure"
    );
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let statuses = tool_completed_statuses(&handle);
    assert_eq!(statuses.len(), 2, "{statuses:?}");
    assert!(
        statuses.iter().any(|(_, s)| s == "completed"),
        "the successful tool stays completed: {statuses:?}"
    );
    assert!(
        statuses.iter().any(|(_, s)| s == "failed"),
        "the failed tool must never be dropped: {statuses:?}"
    );
    assert!(handle.pending_tool_runs().unwrap().is_empty());
    // No dropped failure: the failure is durable turn history — the
    // ledger (and therefore the memory facts) carries it.
    let ledger: faktor_context::ledger::TaskLedger =
        serde_json::from_value(handle.get_task_ledger().unwrap().unwrap()).unwrap();
    assert!(
        ledger.known_failures.iter().any(|f| f.contains("explode")),
        "the failed tool must land in the ledger: {:?}",
        ledger.known_failures
    );
    // Every tool call is answered in the durable transcript: the
    // successful result AND the failed call's error result. The turn
    // continues to the model, so a dangling call would reach the next
    // wire request.
    let page = handle.messages_before(None, 20).unwrap();
    let result_for = |call: &str| {
        page.iter().any(|m| {
            handle.parts_of(m.id).unwrap().iter().any(|p| {
                p.kind == "tool_result"
                    && p.data.get("tool_call_id").and_then(|v| v.as_str()) == Some(call)
            })
        })
    };
    assert!(result_for("ok"), "the completed call must have its result");
    assert!(
        result_for("bad"),
        "the failed call must have its error result"
    );
    // The mixed batch recovers through the documented legal hop chain
    // (FailedRecoverable -> Preparing -> BuildingContext ->
    // WaitingForModel); the old code attempted the illegal
    // FailedRecoverable -> UpdatingMemory edge.
    let events = handle.events_range(1, None).unwrap();
    let failed_idx = events
        .iter()
        .rposition(|e| {
            e.kind == faktor_core::event::EventKind::ToolCompleted
                && e.state == AgentState::FailedRecoverable
        })
        .expect("the failed finish is journaled");
    let after: Vec<AgentState> = events[failed_idx + 1..]
        .iter()
        .take(3)
        .map(|e| e.state)
        .collect();
    assert_eq!(
        after,
        vec![
            AgentState::Preparing,
            AgentState::BuildingContext,
            AgentState::WaitingForModel
        ],
        "the mixed batch must recover through the legal retry hop"
    );
}

#[tokio::test]
async fn all_failed_tool_batch_ends_recoverable_without_illegal_hop() {
    // The all-recoverable-failure end: EVERY submitted tool fails
    // (execute -> Err), so `executed == 0` and the machine is already at
    // `FailedRecoverable` when the batch returns. The turn must report
    // that classified end — the old fall-through into the genuine-end
    // tail attempted `FailedRecoverable -> Validating` and died with
    // `InvalidState`. The session stays promptable afterwards.
    let (deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "bad_1".into(),
                name: "explode".into(),
                input: serde_json::json!({"x": 1}),
            },
            ScriptedResponse::ToolCall {
                id: "bad_2".into(),
                name: "explode".into(),
                input: serde_json::json!({"x": 2}),
            },
            ScriptedResponse::Text("unreachable".into()),
            ScriptedResponse::End,
        ]),
        vec![echo_tool(), failing_tool()],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let outcome = runtime.run_turn(session, "do work", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::FailedRecoverable);
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let statuses = tool_completed_statuses(&handle);
    assert_eq!(statuses.len(), 2, "{statuses:?}");
    assert!(
        statuses.iter().all(|(_, s)| s == "failed"),
        "every failed tool is recorded failed: {statuses:?}"
    );
    assert!(handle.pending_tool_runs().unwrap().is_empty());
    // Both failed calls are answered in the durable transcript: the
    // next turn's request must never carry a dangling tool call.
    let page = handle.messages_before(None, 20).unwrap();
    for call in ["bad_1", "bad_2"] {
        assert!(
            page.iter().any(|m| {
                handle.parts_of(m.id).unwrap().iter().any(|p| {
                    p.kind == "tool_result"
                        && p.data.get("tool_call_id").and_then(|v| v.as_str()) == Some(call)
                })
            }),
            "the failed call {call} must have its error result"
        );
    }
    // The session stays usable: a fresh prompt completes normally.
    let outcome = runtime.run_turn(session, "try again", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
}

#[tokio::test]
async fn turn_budget_slice_ends_an_infinite_tool_loop() {
    // Audit 26: no single runtime future may span more than one
    // turn_budget_ms slice. With a 5 ms budget and a provider that
    // requests tools forever, the drive MUST end its slice at the first
    // iteration boundary (one TurnCompleted, ledger persisted) instead
    // of constructing a whole-task future.
    let (deps, _dir) = deps_with(Arc::new(InfiniteToolProvider), vec![slow_tool()]);
    let runtime = AgentRuntime::new(deps).unwrap();
    runtime.set_turn_budget_ms(5);
    let session = new_session(runtime.deps());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let guarded = tokio::time::timeout(Duration::from_secs(30), async {
        runtime
            .run_turn(session, "loop forever", &[])
            .await
            .unwrap()
    })
    .await
    .expect("the turn slice must end the drive; the loop must never run unbounded");
    assert_eq!(guarded.final_state, AgentState::ReadyForNextTurn);
    assert_eq!(guarded.turns, 1, "the slice ends exactly one logical turn");
    let events = handle.events_range(1, None).unwrap();
    let turn_completed = events
        .iter()
        .filter(|e| e.kind == faktor_core::event::EventKind::TurnCompleted)
        .count();
    assert_eq!(turn_completed, 1, "exactly one genuine end for the slice");
    // Progress persisted: the task row exists with the slice's step and
    // the ledger is durable for the next slice's re-entry.
    let tasks = handle.list_tasks().unwrap();
    assert_eq!(tasks.len(), 1, "the task row exists after the slice");
    assert!(handle.get_task_ledger().unwrap().is_some());
    // The next logical turn re-enters the task normally.
    let o2 = runtime
        .run_turn(session, "continue after the slice", &[])
        .await
        .unwrap();
    assert_eq!(o2.final_state, AgentState::ReadyForNextTurn);
    assert_eq!(o2.turns, 1);
}

#[tokio::test]
async fn tool_input_carrying_a_secret_is_denied_before_execution() {
    // A write tool whose content argument carries an OpenAI-style key
    // must be DENIED at the gate: the tool never executes (counted via
    // the closure), the denial is journaled PermissionDenied with the
    // reason naming the detected kind, and no run ever starts.
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let exec = executions.clone();
    let write_tool = Tool {
        name: "write_file".into(),
        description: "writes a file".into(),
        input_schema: serde_json::json!({"type": "object"}),
        resource_class: faktor_core::resource::ResourceClass::DiskWrite,
        capability: None,
        recovery_hint: RecoveryHint::WorkspaceWrite,
        path_args: vec!["path".into()],
        execute: Arc::new(move |_ctx, _args| {
            let exec = exec.clone();
            Box::pin(async move {
                exec.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(ToolOutcome {
                    text: "wrote".into(),
                    exit_code: Some(0),
                    ..Default::default()
                })
            })
        }),
    };
    let (deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "leak_1".into(),
                name: "write_file".into(),
                input: serde_json::json!({ "path": "creds.txt", "content": SK_SAMPLE }),
            },
            ScriptedResponse::End,
        ]),
        vec![write_tool],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let outcome = runtime
        .run_turn(session, "store the key", &[])
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_eq!(
        executions.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the tool must never execute on secret-carrying input"
    );
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    assert!(handle.pending_tool_runs().unwrap().is_empty());
    let events = handle.events_range(1, None).unwrap();
    assert!(
        !events
            .iter()
            .any(|e| e.kind == faktor_core::event::EventKind::ToolStarted),
        "no run may start for a denied secret-bearing call"
    );
    let denial = events
        .iter()
        .find(|e| e.kind == faktor_core::event::EventKind::PermissionDenied)
        .expect("the secret gate journals a PermissionDenied");
    let payload = denial.payload.as_ref().expect("denial carries a payload");
    assert_eq!(payload["tool"], "write_file");
    assert_eq!(
        payload["reason"],
        "secret detected in tool input (openai_key)"
    );
}

#[tokio::test]
async fn tool_results_echoing_secrets_are_redacted_benign_outputs_byte_identical() {
    // A run_command-style tool prints a GitHub token it read: the
    // durable message must carry the redacted form, never the raw
    // credential; a benign twin output in the SAME batch stays
    // byte-identical (redaction only fires on a scan hit).
    let command_tool = Tool {
        name: "run_command".into(),
        description: "runs a command".into(),
        input_schema: serde_json::json!({"type": "object"}),
        resource_class: faktor_core::resource::ResourceClass::Cpu,
        capability: None,
        recovery_hint: RecoveryHint::UnknownEffect,
        path_args: vec![],
        execute: Arc::new(|_ctx, args| {
            Box::pin(async move {
                let cmd = args
                    .get("command")
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .to_string();
                let text = if cmd.contains("cat") {
                    format!("the key printed: {GHP_SAMPLE} end")
                } else {
                    format!("output: {cmd}")
                };
                Ok(ToolOutcome {
                    text,
                    exit_code: Some(0),
                    ..Default::default()
                })
            })
        }),
    };
    let (deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "run_command".into(),
                input: serde_json::json!({ "command": "cat token" }),
            },
            ScriptedResponse::ToolCall {
                id: "c2".into(),
                name: "run_command".into(),
                input: serde_json::json!({ "command": "fmt" }),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ]),
        vec![command_tool],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let outcome = runtime
        .run_turn(session, "run and show", &[])
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let msgs = runtime
        .history_messages(&handle, &ContextBudget::default())
        .await
        .unwrap();
    let results: Vec<(String, String)> = msgs
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|c| match &c.kind {
            ContentKind::ToolResult { content, .. } => {
                Some((c.tool_call_id.clone().unwrap_or_default(), content.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 2, "both tool results must be durable");
    let secret_result = results
        .iter()
        .find(|(id, _)| id == "c1")
        .expect("c1 result");
    assert_eq!(
        secret_result.1, "the key printed: <redacted:github_token> end",
        "the echoed credential must be redacted in the durable message"
    );
    assert!(
        !msgs.iter().any(|m| {
            m.content.iter().any(|c| {
                matches!(
                    &c.kind,
                    ContentKind::ToolResult { content, .. }
                        if content.contains(GHP_SAMPLE)
                )
            })
        }),
        "no durable message anywhere may carry the raw secret"
    );
    let benign = results
        .iter()
        .find(|(id, _)| id == "c2")
        .expect("c2 result");
    assert_eq!(
        benign.1, "output: fmt",
        "benign tool output must stay byte-identical"
    );
}

#[tokio::test]
async fn hostile_tool_inputs_whole_payload_scan_catches_deep_secrets_and_boundary_words_do_not() {
    // Adversarial scan cases through the real gate:
    //  - a secret nested DEEP inside JSON is still detected and denied;
    //  - a 1 MiB hostile input never panics: secret scanning is
    //    WHOLE-PAYLOAD (P0-37 — the old 256 KiB prefix window let a
    //    secret past the boundary report "Clean"), so the credential
    //    past the former window is CAUGHT and the tool is denied;
    //  - "sk" and "-<long digits>" fragments SPLIT across JSON
    //    key/value boundaries never form a contiguous "sk-…" — no false
    //    positive, the tool executes;
    //  - prose text "task-…" (a "sk-" substring with no 20-char run
    //    behind it) is not falsely caught either.
    let caps = ModelCapabilities {
        tools: true,
        context: 8_000_000, // the 1 MiB input rides history; keep the plan under budget
        ..Default::default()
    };
    let mut huge = String::with_capacity(1024 * 1024 + GHP_SAMPLE.len());
    huge.push_str(&"a".repeat(1024 * 1024));
    huge.push_str(GHP_SAMPLE); // deep past the old 256 KiB window: still caught (whole-payload scan)
    let cases: Vec<(serde_json::Value, Option<&'static str>)> = vec![
        (
            serde_json::json!({
                "payload": {
                    "items": [ { "content": format!("wrap {AKIA_SAMPLE} wrap") } ]
                }
            }),
            Some("aws_key"),
        ),
        (serde_json::json!({ "content": huge }), Some("github_token")),
        (
            serde_json::json!({
                "key_name": "sk",
                "value_body": "-0123456789012345678901234567890123456789",
            }),
            None,
        ),
        (
            serde_json::json!({ "content": "run task-unscheduled now" }),
            None,
        ),
    ];
    for (case_idx, (input, expect_kind)) in cases.into_iter().enumerate() {
        let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let exec = executions.clone();
        let tool = Tool {
            name: "write_file".into(),
            description: "writes".into(),
            input_schema: serde_json::json!({"type": "object"}),
            resource_class: faktor_core::resource::ResourceClass::DiskWrite,
            capability: None,
            recovery_hint: RecoveryHint::WorkspaceWrite,
            path_args: vec!["path".into()],
            execute: Arc::new(move |_ctx, _args| {
                let exec = exec.clone();
                Box::pin(async move {
                    exec.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(ToolOutcome {
                        text: "wrote".into(),
                        exit_code: Some(0),
                        ..Default::default()
                    })
                })
            }),
        };
        let provider = FakeProvider::with_script(
            "fake",
            caps.clone(),
            vec![
                ScriptedResponse::ToolCall {
                    id: format!("h_{case_idx}"),
                    name: "write_file".into(),
                    input: input.clone(),
                },
                ScriptedResponse::End,
                ScriptedResponse::Text("done".into()),
                ScriptedResponse::End,
            ],
        );
        let (deps, _dir) = deps(provider, vec![tool]);
        let runtime = AgentRuntime::new(deps).unwrap();
        let session = new_session(runtime.deps());
        let outcome = runtime.run_turn(session, "write it", &[]).await.unwrap();
        assert_eq!(
            outcome.final_state,
            AgentState::ReadyForNextTurn,
            "case {case_idx}"
        );
        let ran = executions.load(std::sync::atomic::Ordering::SeqCst);
        match expect_kind {
            Some(kind) => {
                assert_eq!(
                    ran, 0,
                    "case {case_idx}: a deep secret must deny before execution"
                );
                let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
                let events = handle.events_range(1, None).unwrap();
                let denied = events.iter().any(|e| {
                    e.kind == faktor_core::event::EventKind::PermissionDenied
                        && e.payload.as_ref().is_some_and(|p| {
                            p["reason"] == format!("secret detected in tool input ({kind})")
                        })
                });
                assert!(
                    denied,
                    "case {case_idx}: the denial must journal the detected kind"
                );
            }
            None => assert_eq!(
                ran, 1,
                "case {case_idx}: bounded scan must not false-positive"
            ),
        }
    }
}

#[tokio::test]
async fn retry_never_replays_after_durable_content() {
    // State-aware (spec §13): once assistant content became durable
    // (parts flushed), a network death must NOT replay — the session
    // fails honestly instead of duplicating content.
    let (mut deps, _dir) = deps(scripted_provider(vec![ScriptedResponse::End]), vec![]);
    deps.retry_policy = faktor_core::retry::RetryPolicy {
        max_attempts: 3,
        base_delay_ms: 1,
        max_delay_ms: 5,
        jitter: 0.0,
        class: faktor_core::retry::RetryClass::Network,
    };
    // The die-mid-stream provider emits one text chunk (durable part)
    // then dies with a retryable network error.
    let dying = FakeProvider::die_mid_stream(
        "fake",
        ModelCapabilities {
            streaming: true,
            ..Default::default()
        },
    );
    let mut registry = ProviderRegistry::new();
    registry.try_register(Arc::new(dying)).unwrap();
    deps.providers = Arc::new(registry);
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let outcome = runtime.run_turn(session, "no replay", &[]).await.unwrap();
    assert_eq!(
        outcome.final_state,
        AgentState::FailedRecoverable,
        "content already durable: never replay, fail honestly"
    );
    // Exactly ONE assistant message row exists (no duplicate from a
    // replay — the partial message is present, nothing was re-created).
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let rows = handle.messages_before(None, 10).unwrap();
    let assistant_rows = rows.iter().filter(|m| m.role == "assistant").count();
    assert_eq!(assistant_rows, 1, "no duplicated assistant content");
}

#[tokio::test]
async fn idempotent_tool_interrupted_replays_exactly_once() {
    // Requirement 2a: an idempotent tool interrupted before completion
    // is re-executed EXACTLY ONCE as a new physical attempt of the SAME
    // logical operation: one ReplayStarted event, the row completes,
    // the outcome effect is journaled, no duplicate messages, no fresh
    // turn record.
    let dir = fresh_store_dir();
    let counter = Arc::new(AtomicUsize::new(0));
    let turn_op: OpId;
    let tool_op: OpId;
    let session: SessionId;
    {
        let manager1 =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let (deps1, _keep) = deps_sharing_session(
            manager1.clone(),
            Arc::new(scripted_provider(vec![])),
            vec![counting_tool(
                "echo",
                RecoveryHint::Idempotent,
                counter.clone(),
            )],
        );
        let runtime1 = AgentRuntime::new(deps1).unwrap();
        let ws = manager1.create_workspace("/w").unwrap();
        let handle = manager1.create_session(ws, "t", "fake", "m").unwrap();
        session = handle.id();
        let receipt = handle.submit_prompt("use echo", &[]).unwrap();
        turn_op = receipt.op_id;
        let mut meta = op_meta(&manager1, session, RecoveryStrategy::Idempotent);
        tool_op = meta.operation_id;
        // The runtime stores the replay descriptor on the run row.
        let desc = ReplayDescriptor {
            tool_name: "echo".into(),
            validated_args: serde_json::json!({"x": 1}),
            workspace_id: WorkspaceId::new(1),
            worktree_id: WorktreeId::new(1),
            task_id: TaskId::new(1),
            original_turn_op_id: turn_op,
            capability: Capability::ReadWorkspace { path: ".".into() },
            recovery_kind: "idempotent".into(),
        };
        meta = meta.with_replay(serde_json::to_value(&desc).unwrap());
        chain_to_streaming(&handle, turn_op);
        crash_tool_start(
            &handle,
            turn_op,
            "echo",
            serde_json::json!({"x": 1}),
            "call_1",
            meta,
        );
        assert_eq!(counter.load(Ordering::SeqCst), 0, "crash before execution");
        assert_eq!(handle.message_count().unwrap(), 2);
        drop(runtime1);
    }
    // The post-restart runtime registers the echo tool again: the replay
    // executes against THIS registry (the counter it observes).
    let counter2 = Arc::new(AtomicUsize::new(0));
    let inner = scripted_provider(vec![
        ScriptedResponse::Text("after replay".into()),
        ScriptedResponse::End,
    ]);
    let (deps2, _keep2) = reopen_runtime(
        &dir,
        Arc::new(inner),
        vec![counting_tool(
            "echo",
            RecoveryHint::Idempotent,
            counter2.clone(),
        )],
    );
    let runtime2 = AgentRuntime::new(deps2).unwrap();
    let outcome = runtime2.continue_turn(session).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_eq!(outcome.op_id, turn_op, "the SAME logical turn completes");
    // Exactly once.
    assert_eq!(
        counter2.load(Ordering::SeqCst),
        1,
        "recovery must execute the idempotent tool exactly once"
    );
    let handle2 = runtime2
        .deps()
        .session
        .get_session(session)
        .unwrap()
        .unwrap();
    let events = handle2.events_range(1, None).unwrap();
    let replays = events
        .iter()
        .filter(|e| e.kind == faktor_core::event::EventKind::ReplayStarted)
        .count();
    assert_eq!(replays, 1, "exactly one ReplayStarted event");
    let replay_ev = events
        .iter()
        .find(|e| e.kind == faktor_core::event::EventKind::ReplayStarted)
        .unwrap();
    assert_eq!(
        replay_ev.op_id,
        Some(tool_op),
        "replay is the SAME logical op"
    );
    assert_eq!(replay_ev.payload.as_ref().unwrap()["attempt"], 1);
    assert_eq!(
        replay_ev.payload.as_ref().unwrap()["turn_op_id"],
        turn_op.raw()
    );
    // The row completed; no duplicate messages (the user prompt once,
    // the model's tool-call message once, the single replay result
    // part, and the resumed turn's own answer).
    assert!(handle2.pending_tool_runs().unwrap().is_empty());
    assert_eq!(handle2.message_count().unwrap(), 4, "no duplicate messages");
    let page = handle2.messages_before(None, 10).unwrap();
    let users = page.iter().filter(|m| m.role == "user").count();
    assert_eq!(users, 1, "user prompt never duplicated");
    let tool_call_msgs = page
        .iter()
        .filter(|m| {
            handle2
                .parts_of(m.id)
                .unwrap()
                .iter()
                .any(|p| p.kind == "tool_call")
        })
        .count();
    assert_eq!(
        tool_call_msgs, 1,
        "the model's tool-call message never duplicated"
    );
    let result_ok = page.iter().any(|m| {
        handle2.parts_of(m.id).unwrap().iter().any(|p| {
            p.kind == "tool_result"
                && p.data.get("tool_call_id").and_then(|v| v.as_str()) == Some("call_1")
        })
    });
    assert!(result_ok, "the replayed outcome links to the original call");
    // One logical turn, one completion; the turn record survived intact.
    let turn_completed = events
        .iter()
        .filter(|e| {
            e.kind == faktor_core::event::EventKind::TurnCompleted && e.op_id == Some(turn_op)
        })
        .count();
    assert_eq!(turn_completed, 1);
    let records = handle2.turn_records().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].turn_op_id, turn_op);
    assert_eq!(records[0].status, "completed");
}

#[tokio::test]
async fn replay_descriptor_survives_reopen_and_replays_once() {
    // Requirement 2c: the descriptor is durable — after a daemon restart
    // (new manager AND new runtime over the same dir) the interrupted
    // idempotent run still replays exactly once.
    let dir = fresh_store_dir();
    let session: SessionId;
    let tool_op: OpId;
    {
        let manager1 =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let counter = Arc::new(AtomicUsize::new(0));
        let (deps1, _keep) = deps_sharing_session(
            manager1.clone(),
            Arc::new(scripted_provider(vec![])),
            vec![counting_tool("echo", RecoveryHint::Idempotent, counter)],
        );
        let runtime1 = AgentRuntime::new(deps1).unwrap();
        let ws = manager1.create_workspace("/w").unwrap();
        let handle = manager1.create_session(ws, "t", "fake", "m").unwrap();
        session = handle.id();
        let receipt = handle.submit_prompt("echo it", &[]).unwrap();
        let mut meta = op_meta(&manager1, session, RecoveryStrategy::Idempotent);
        tool_op = meta.operation_id;
        let desc = ReplayDescriptor {
            tool_name: "echo".into(),
            validated_args: serde_json::json!({"x": 1}),
            workspace_id: WorkspaceId::new(1),
            worktree_id: WorktreeId::new(1),
            task_id: TaskId::new(1),
            original_turn_op_id: receipt.op_id,
            capability: Capability::ReadWorkspace { path: ".".into() },
            recovery_kind: "idempotent".into(),
        };
        meta = meta.with_replay(serde_json::to_value(&desc).unwrap());
        chain_to_streaming(&handle, receipt.op_id);
        crash_tool_start(
            &handle,
            receipt.op_id,
            "echo",
            serde_json::json!({"x": 1}),
            "call_1",
            meta,
        );
        drop(runtime1);
    }
    let counter2 = Arc::new(AtomicUsize::new(0));
    let inner = scripted_provider(vec![
        ScriptedResponse::Text("done".into()),
        ScriptedResponse::End,
    ]);
    let (mut deps2, _keep2) = reopen_runtime(&dir, Arc::new(inner), vec![]);
    let manager2 = deps2.session.clone();
    let mut tools2 = ToolRegistry::new();
    tools2.register(counting_tool(
        "echo",
        RecoveryHint::Idempotent,
        counter2.clone(),
    ));
    deps2.tools = Arc::new(tools2);
    let runtime2 = AgentRuntime::new(deps2).unwrap();
    let outcome = runtime2.continue_turn(session).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_eq!(
        counter2.load(Ordering::SeqCst),
        1,
        "the reopened store must still replay the run exactly once"
    );
    let handle = manager2.get_session(session).unwrap().unwrap();
    let events = handle.events_range(1, None).unwrap();
    let replays = events
        .iter()
        .filter(|e| e.kind == faktor_core::event::EventKind::ReplayStarted)
        .count();
    assert_eq!(replays, 1);
    let starts = events
        .iter()
        .filter(|e| {
            e.kind == faktor_core::event::EventKind::ToolStarted && e.op_id == Some(tool_op)
        })
        .count();
    assert_eq!(starts, 1, "the ORIGINAL row is replayed, never a new row");
    assert!(handle.pending_tool_runs().unwrap().is_empty());
}

#[tokio::test]
async fn unknown_effect_tool_interrupted_is_never_replayed() {
    // Requirement 2b: tools with unknown/destructive external effects
    // are NEVER replayed — the run stays unknown and the interrupted
    // turn ends honestly; the tool's execute never runs again.
    let dir = fresh_store_dir();
    let counter = Arc::new(AtomicUsize::new(0));
    let session: SessionId;
    {
        let manager1 =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let (deps1, _keep) = deps_sharing_session(
            manager1.clone(),
            Arc::new(scripted_provider(vec![])),
            vec![counting_tool(
                "run_cmd",
                RecoveryHint::UnknownEffect,
                counter.clone(),
            )],
        );
        let runtime1 = AgentRuntime::new(deps1).unwrap();
        let ws = manager1.create_workspace("/w").unwrap();
        let handle = manager1.create_session(ws, "t", "fake", "m").unwrap();
        session = handle.id();
        let receipt = handle.submit_prompt("run it", &[]).unwrap();
        let meta = op_meta(&manager1, session, RecoveryStrategy::MarkUnknown);
        chain_to_streaming(&handle, receipt.op_id);
        crash_tool_start(
            &handle,
            receipt.op_id,
            "run_cmd",
            serde_json::json!({"command": "rm -rf x"}),
            "call_1",
            meta,
        );
        drop(runtime1);
    }
    let inner = scripted_provider(vec![ScriptedResponse::End]);
    let (deps2, _keep2) = reopen_runtime(&dir, Arc::new(inner), vec![]);
    let runtime2 = AgentRuntime::new(deps2).unwrap();
    let handle2 = runtime2
        .deps()
        .session
        .get_session(session)
        .unwrap()
        .unwrap();
    let outcome = runtime2.continue_turn(session).await.unwrap();
    // The interrupted turn ends honestly: no replay, no blind re-run.
    assert_eq!(outcome.final_state, AgentState::FailedRecoverable);
    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "unknown-effect tools must never be re-executed"
    );
    assert!(handle2.pending_tool_runs().unwrap().is_empty());
    let events = handle2.events_range(1, None).unwrap();
    let recovery_unknown = events.iter().any(|e| {
        e.kind == faktor_core::event::EventKind::RecoveryApplied
            && e.payload
                .as_ref()
                .is_some_and(|p| p.get("effect").and_then(|v| v.as_str()) == Some("unknown"))
    });
    assert!(recovery_unknown, "effect stays unknown, never applied");
    let replays = events
        .iter()
        .filter(|e| e.kind == faktor_core::event::EventKind::ReplayStarted)
        .count();
    assert_eq!(replays, 0);
}

#[tokio::test]
async fn hostile_replay_descriptor_fails_loudly_never_replays() {
    // Requirement 2d: a hostile stored descriptor (missing args) must be
    // an honest failure — never a blind replay of a half-known call.
    let dir = fresh_store_dir();
    let counter = Arc::new(AtomicUsize::new(0));
    let session: SessionId;
    {
        let manager1 =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let (deps1, _keep) = deps_sharing_session(
            manager1.clone(),
            Arc::new(scripted_provider(vec![])),
            vec![counting_tool(
                "echo",
                RecoveryHint::Idempotent,
                counter.clone(),
            )],
        );
        let runtime1 = AgentRuntime::new(deps1).unwrap();
        let ws = manager1.create_workspace("/w").unwrap();
        let handle = manager1.create_session(ws, "t", "fake", "m").unwrap();
        session = handle.id();
        let receipt = handle.submit_prompt("echo", &[]).unwrap();
        // Tampered descriptor: fields missing (no validated_args).
        let mut meta = op_meta(&manager1, session, RecoveryStrategy::Idempotent);
        meta = meta.with_replay(serde_json::json!({ "tool_name": "echo" }));
        chain_to_streaming(&handle, receipt.op_id);
        crash_tool_start(
            &handle,
            receipt.op_id,
            "echo",
            serde_json::json!({"x": 1}),
            "call_1",
            meta,
        );
        drop(runtime1);
    }
    let inner = scripted_provider(vec![ScriptedResponse::End]);
    let (deps2, _keep2) = reopen_runtime(&dir, Arc::new(inner), vec![]);
    let runtime2 = AgentRuntime::new(deps2).unwrap();
    let err = runtime2.continue_turn(session).await.unwrap_err();
    assert_eq!(
        err.kind,
        ErrorKind::Malformed,
        "a hostile descriptor is a loud honest failure: {err}"
    );
    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "no blind replay of a hostile descriptor"
    );
    // The row was NOT finished or replayed: it stays running so the
    // corruption is visible and fixable, never silently dropped.
    let handle2 = runtime2
        .deps()
        .session
        .get_session(session)
        .unwrap()
        .unwrap();
    assert_eq!(handle2.pending_tool_runs().unwrap().len(), 1);
}

#[tokio::test]
async fn workspace_write_recovery_verifies_recorded_postcondition() {
    // Requirement 3a: recovery verifies the CURRENT file bytes against
    // the RECORDED postcondition (BLAKE3 of the raw bytes as written) —
    // matching files complete WITHOUT re-running the tool, effect
    // applied. The expected hash is blake3("hello"), NOT
    // blake3(serde_json::to_vec("hello")).
    let dir = fresh_store_dir();
    let ws_id: WorkspaceId;
    let session: SessionId;
    let tool_op: OpId;
    {
        let (manager, wid, sid, root) = workspace_env(&dir);
        ws_id = wid;
        session = sid;
        let handle = manager.get_session(session).unwrap().unwrap();
        let receipt = handle.submit_prompt("write hello", &[]).unwrap();
        let meta = op_meta(&manager, session, RecoveryStrategy::MarkUnknown);
        tool_op = meta.operation_id;
        chain_to_streaming(&handle, receipt.op_id);
        crash_tool_start(
            &handle,
            receipt.op_id,
            "write_file",
            serde_json::json!({"path": "a.txt", "content": "hello"}),
            "call_1",
            meta,
        );
        // The write LANDED before the crash (the file holds the raw
        // bytes); the runtime had annotated the row with the tool's
        // postcondition (bytes as written, workspace-relative path).
        std::fs::write(root.join("a.txt"), b"hello").unwrap();
        let expected = FileHash::from(blake3::hash(b"hello").into());
        let pc = serde_json::to_value(FilePostcondition {
            workspace_id: ws_id,
            worktree_id: WorktreeId::new(1),
            relative_path: "a.txt".into(),
            expected_hash: expected,
        })
        .unwrap();
        handle.record_tool_postcondition(tool_op, &pc).unwrap();
        // Drop: the crash residue is swept post-restart by a fresh
        // runtime's recover() (no in-process driver, no stale tracking).
    }
    let write_counter = Arc::new(AtomicUsize::new(0));
    let (deps2, _keep2) = reopen_runtime(
        &dir,
        Arc::new(scripted_provider(vec![])),
        vec![counting_tool(
            "write_file",
            RecoveryHint::WorkspaceWrite,
            write_counter.clone(),
        )],
    );
    let runtime2 = AgentRuntime::new(deps2).unwrap();
    let reports = runtime2.recover().unwrap();
    let report = reports.iter().find(|r| r.session_id == session).unwrap();
    assert_eq!(report.crashed_ops.len(), 1);
    assert_eq!(
        report.crashed_ops[0].status, "completed",
        "matching postcondition completes without re-running"
    );
    assert_eq!(
        report.crashed_ops[0].effect,
        EffectStatus::Verified,
        "reports effect verified/applied"
    );
    assert_eq!(write_counter.load(Ordering::SeqCst), 0, "never re-run");
    let handle2 = runtime2
        .deps()
        .session
        .get_session(session)
        .unwrap()
        .unwrap();
    assert!(handle2.pending_tool_runs().unwrap().is_empty());
    let root = dir.path().join("ws");
    assert_eq!(
        std::fs::read(root.join("a.txt")).unwrap(),
        b"hello",
        "the verified file is untouched"
    );
    // Recover again: idempotent, nothing pending.
    let reports = runtime2.recover().unwrap();
    assert!(reports.iter().all(|r| r.crashed_ops.is_empty()));
}

/// The host-side grammar refuses Windows-absolute shapes on EVERY host:
/// on Unix a literal `C:` directory under the workspace root makes the
/// filesystem resolver ACCEPT `C:/escape.txt` (it is just a relative
/// name there), so only the grammar can refuse it — exactly the shape
/// the durable recovery path must never hash into a completion.
#[tokio::test]
async fn workspace_write_recovery_rejects_windows_absolute_postconditions() {
    let dir = fresh_store_dir();
    let session: SessionId;
    let root: std::path::PathBuf;
    {
        let (manager, wid, sid, root_path) = workspace_env(&dir);
        session = sid;
        root = root_path;
        let handle = manager.get_session(session).unwrap().unwrap();
        let receipt = handle.submit_prompt("write", &[]).unwrap();
        chain_to_streaming(&handle, receipt.op_id);
        // The decoy: a literal `C:` directory under the root. With a
        // resolver-only implementation the row would verifiably COMPLETE
        // against this file on Unix; the host grammar must refuse first.
        #[cfg(not(windows))]
        {
            let decoy = root.join("C:");
            std::fs::create_dir_all(&decoy).unwrap();
            std::fs::write(decoy.join("escape.txt"), b"pwn").unwrap();
        }
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
            workspace_id: wid,
            worktree_id: WorktreeId::new(1),
            relative_path: "C:/escape.txt".into(),
            expected_hash: FileHash::from(blake3::hash(b"pwn").into()),
        })
        .unwrap();
        handle.record_tool_postcondition(win_op, &pc).unwrap();
    }
    let (deps2, _keep2) = reopen_runtime(&dir, Arc::new(scripted_provider(vec![])), vec![]);
    let runtime2 = AgentRuntime::new(deps2).unwrap();
    let err = runtime2.recover().unwrap_err();
    assert_eq!(
        err.kind,
        ErrorKind::Permission,
        "a windows-absolute postcondition must be rejected loudly: {err}"
    );
    // The row stays running (visible), never silently completed.
    let handle2 = runtime2
        .deps()
        .session
        .get_session(session)
        .unwrap()
        .unwrap();
    assert_eq!(handle2.pending_tool_runs().unwrap().len(), 1);
    // The decoy is untouched: recovery never hashed or wrote through it.
    #[cfg(not(windows))]
    assert_eq!(
        std::fs::read(root.join("C:").join("escape.txt")).unwrap(),
        b"pwn"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn post_tool_fail_closed_deny_is_audit_only_and_the_turn_completes() {
    // Adversarial: the PostTool hook fails closed (exits 1, no verdict
    // -> the registry records a Deny). The tool already executed; the
    // deny must NEVER retroactively fail the turn — the turn still
    // completes and the audit log carries the record.
    let (mut deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "echo".into(),
                input: serde_json::json!({"x": 1}),
            },
            ScriptedResponse::Text("after tool".into()),
            ScriptedResponse::End,
        ]),
        vec![echo_tool()],
    );
    let hooks = Arc::new(faktor_hooks::HookRegistry::try_new().expect("standalone supervisor"));
    hooks
        .register(failing_closed_hook(
            "post_deny",
            faktor_hooks::HookEvent::PostTool,
        ))
        .unwrap();
    deps.hooks = Some(hooks.clone());
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let outcome = runtime.run_turn(session, "use echo", &[]).await.unwrap();
    assert_eq!(
        outcome.final_state,
        AgentState::ReadyForNextTurn,
        "a deny AFTER execution is audit-only and must not fail the turn"
    );
    assert_eq!(outcome.turns, 1);
    let audit = hooks.audit();
    assert_eq!(
        audit
            .iter()
            .filter(|r| r.hook_id == "post_deny" && r.event == faktor_hooks::HookEvent::PostTool)
            .count(),
        1,
        "the PostTool run must be audit-logged exactly once: {audit:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn tool_error_hook_fires_and_a_post_hoc_deny_is_audit_only() {
    // The tool EXECUTION fails (Err): the ToolError hook must fire on
    // the failure branch with a bounded error payload. Adversarial
    // best-effort proof: a FailClosed hook that DENIES changes nothing
    // — the turn's outcome and the session state are identical with and
    // without the hook (the failed finish lands the session
    // FailedRecoverable by session-layer design either way), the deny
    // only lands in the audit log, and the session stays promptable.
    let out_dir = tempdir().unwrap();
    let out_hooked = out_dir.path().join("hooked.json");

    let (dir_plain, runtime_plain, session_plain, audit_plain) =
        drive_failing_tool_turn(None).await;
    let state_plain = runtime_plain
        .deps()
        .session
        .get_session(session_plain)
        .unwrap()
        .unwrap()
        .state()
        .unwrap();
    assert!(audit_plain.is_empty(), "no registry wired -> no audit");

    let registry = Arc::new(faktor_hooks::HookRegistry::try_new().expect("standalone supervisor"));
    registry
        .register(file_writing_hook(
            "tool_err",
            faktor_hooks::HookEvent::ToolError,
            &out_hooked,
        ))
        .unwrap();
    registry
        .register(failing_closed_hook(
            "tool_err_deny",
            faktor_hooks::HookEvent::ToolError,
        ))
        .unwrap();
    let (dir_hooked, runtime_hooked, session_hooked, audit_hooked) =
        drive_failing_tool_turn(Some(registry)).await;
    let handle_hooked = runtime_hooked
        .deps()
        .session
        .get_session(session_hooked)
        .unwrap()
        .unwrap();
    let state_hooked = handle_hooked.state().unwrap();
    assert_eq!(
        state_hooked, state_plain,
        "a post-hoc deny must not alter the turn outcome"
    );
    assert_eq!(
        state_hooked,
        AgentState::FailedRecoverable,
        "a failed tool finish ends the turn honestly at FailedRecoverable"
    );
    // The payload reached the hook: event, tool, bounded error snippet.
    let written =
        std::fs::read_to_string(&out_hooked).expect("the hook must have written its input");
    assert!(
        written.contains("\"tool_error\""),
        "payload event: {written}"
    );
    assert!(
        written.contains("\"tool\":\"boom\""),
        "payload tool: {written}"
    );
    assert!(
        written.contains("\"error\":\"tool boom failed\""),
        "bounded error snippet: {written}"
    );
    // Every run is audit-logged, the deny included.
    assert!(
        audit_hooked
            .iter()
            .any(|r| r.hook_id == "tool_err" && r.event == faktor_hooks::HookEvent::ToolError),
        "ToolError must be audit-logged: {audit_hooked:?}"
    );
    assert!(
        audit_hooked
            .iter()
            .any(|r| r.hook_id == "tool_err_deny" && r.event == faktor_hooks::HookEvent::ToolError),
        "the fail-closed deny must still be audit-logged: {audit_hooked:?}"
    );
    // The session stayed usable: a fresh prompt completes normally.
    let outcome = runtime_hooked
        .run_turn(session_hooked, "try again", &[])
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    drop(dir_plain);
    drop(dir_hooked);
}

#[tokio::test]
async fn semantic_restriction_denies_tools_at_the_agent_gate() {
    // An explicit provider capability-axis signal removes the Execute
    // class at the gate: the write tool (capability None => ExecuteShell)
    // is refused with a journaled typed denial and the file is untouched.
    let (deps, _dir, root) = review_env(vec![
        ScriptedResponse::ToolCall {
            id: "c1".into(),
            name: "write_file".into(),
            input: serde_json::json!({
                "path": "src/sandbox_denied.rs",
                "content": "pub fn denied() -> u32 { 0 }\n"
            }),
        },
        ScriptedResponse::Text("done".into()),
        ScriptedResponse::End,
    ]);
    let mut deps = deps;
    deps.semantic = semantic_registry_with(FakeSemanticProvider::affected(vec![
        "src/sandbox_policy.rs".into(),
    ]));
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = session_in_workspace(runtime.deps(), &root);
    let outcome = runtime
        .run_turn(session, "write the file", &[])
        .await
        .unwrap();
    assert_eq!(outcome.semantic_risk, Some(RiskLevel::High));
    assert!(
        !root.join("src/sandbox_denied.rs").exists(),
        "the restricted tool must never execute"
    );
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let denied = handle
        .events_range(1, None)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == faktor_core::event::EventKind::PermissionDenied)
        .expect("the semantic gate journals a PermissionDenied");
    let payload = denied.payload.as_ref().expect("denial payload");
    assert!(
        payload["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("semantic restriction removed capability"),
        "{payload}"
    );
}

/// F5 (adversarial, >old-window): a `JournalFailed` marker whose original
/// event is FAR beyond the old 512-event tail (2400 later legal events)
/// must not replay as a duplicate Failed event.
#[tokio::test]
async fn failed_journal_replay_dedups_beyond_the_old_window() {
    let (deps, _dir) = deps(scripted_provider(vec![]), vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let manager = runtime.deps().session.clone();
    let session = new_session(runtime.deps());
    let handle = manager.get_session(session).unwrap().unwrap();
    let receipt = runtime.submit(session, "run", &[]).unwrap();
    let op = receipt.op_id;
    // The logical turn's record stays ACTIVE (the crash window: the
    // journal write landed, its record close is part of the same intent).
    handle
        .start_turn_record(op, None, None, "fake", "m", None)
        .unwrap();
    handle
        .force_append_event(
            faktor_core::event::EventKind::Failed,
            AgentState::FailedRecoverable,
            Some(op),
            Some(serde_json::json!({"message": "injected"})),
        )
        .unwrap();
    // 2400 legal events afterwards: the old 512-event window can never
    // see the original Failed event.
    for i in 0..600u64 {
        let filler = OpId::new(9_000_000 + i);
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
                faktor_core::event::EventKind::Failed,
                AgentState::FailedRecoverable,
            ),
        ] {
            let payload = (kind == faktor_core::event::EventKind::Failed)
                .then(|| serde_json::json!({ "message": "filler failure" }));
            handle
                .append_event(kind, state, Some(filler), payload)
                .unwrap();
        }
    }
    let before = handle
        .events_range(1, None)
        .unwrap()
        .iter()
        .filter(|e| e.op_id == Some(op) && e.kind == faktor_core::event::EventKind::Failed)
        .count();
    assert_eq!(before, 1);
    let root = manager.store().root().to_path_buf();
    write_raw_marker(
        &root.join(DURABLE_WRITE_MARKER_DIR),
        "dw-dedup-journal.json",
        &serde_json::json!({
            "status": "pending",
            "attempts": 0,
            "site": "test.dedup",
            "session": session.raw(),
            "at_ms": 1,
            "intent": {
                "write": "journal_failed",
                "op_id": op.raw(),
                "state": "FailedRecoverable",
                "payload": {"message": "injected"},
            },
        }),
    );
    runtime.replay_durable_write_failures(&handle);
    let after = handle
        .events_range(1, None)
        .unwrap()
        .iter()
        .filter(|e| e.op_id == Some(op) && e.kind == faktor_core::event::EventKind::Failed)
        .count();
    assert_eq!(
        after, 1,
        "the >window duplicate must never be journaled (complete paged dedup)"
    );
}

/// F5 (adversarial, >old-window): a `LedgerDecision` marker whose
/// original row is beyond the old 256-row tail (600 later rows) must not
/// mint a duplicate decision row.
#[tokio::test]
async fn ledger_decision_replay_dedups_beyond_the_old_window() {
    let (deps, _dir) = deps(scripted_provider(vec![]), vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let manager = runtime.deps().session.clone();
    let session = new_session(runtime.deps());
    let handle = manager.get_session(session).unwrap().unwrap();
    handle
        .ledger_decision("step-dup", "choose A", "because")
        .unwrap();
    for i in 0..600u32 {
        handle
            .ledger_decision(&format!("filler-{i}"), "x", "y")
            .unwrap();
    }
    let root = manager.store().root().to_path_buf();
    write_raw_marker(
        &root.join(DURABLE_WRITE_MARKER_DIR),
        "dw-dedup-ledger.json",
        &serde_json::json!({
            "status": "pending",
            "attempts": 0,
            "site": "test.dedup",
            "session": session.raw(),
            "at_ms": 1,
            "intent": {
                "write": "ledger_decision",
                "step": "step-dup",
                "choice": "choose A",
                "rationale": "because",
            },
        }),
    );
    runtime.replay_durable_write_failures(&handle);
    let rows = manager
        .store()
        .ledger_entries(session, None, 10_000)
        .unwrap();
    let matches = rows
        .iter()
        .filter(|row| {
            row.payload.get("kind").and_then(|k| k.as_str()) == Some("decision")
                && row.payload.get("step").and_then(|v| v.as_str()) == Some("step-dup")
                && row.payload.get("choice").and_then(|v| v.as_str()) == Some("choose A")
                && row.payload.get("rationale").and_then(|v| v.as_str()) == Some("because")
        })
        .count();
    assert_eq!(
        matches, 1,
        "the >window duplicate must never mint a second decision row"
    );
}

/// F5 (adversarial): an `Abort` marker whose target turn STARTED AFTER
/// the intent must never kill a later turn; an Abort that is genuinely
/// still effective applies exactly once.
#[tokio::test]
async fn abort_replay_dedups_by_intent_time() {
    let (deps, _dir) = deps(scripted_provider(vec![]), vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let manager = runtime.deps().session.clone();
    let session = new_session(runtime.deps());
    let handle = manager.get_session(session).unwrap().unwrap();
    let _receipt = runtime.submit(session, "later turn", &[]).unwrap();
    let record = handle.active_turn_record().unwrap().unwrap();
    let root = manager.store().root().to_path_buf();
    let marker_dir = root.join(DURABLE_WRITE_MARKER_DIR);
    // Stale intent (fired BEFORE this turn started): must be skipped.
    write_raw_marker(
        &marker_dir,
        "dw-abort-stale.json",
        &serde_json::json!({
            "status": "pending",
            "attempts": 0,
            "site": "test.dedup",
            "session": session.raw(),
            "at_ms": record.started_at - 1000,
            "intent": {"write": "abort", "op_id": null},
        }),
    );
    runtime.replay_durable_write_failures(&handle);
    assert_eq!(
        handle.active_turn_record().unwrap().map(|r| r.turn_op_id),
        Some(record.turn_op_id),
        "a stale abort intent must never kill a later turn"
    );
    assert_eq!(
        handle.state().unwrap(),
        AgentState::Preparing,
        "the stale intent leaves the later turn running"
    );
    // Fresh intent (fired AFTER this turn started): applies.
    write_raw_marker(
        &marker_dir,
        "dw-abort-fresh.json",
        &serde_json::json!({
            "status": "pending",
            "attempts": 0,
            "site": "test.dedup",
            "session": session.raw(),
            "at_ms": record.started_at + 1000,
            "intent": {"write": "abort", "op_id": null},
        }),
    );
    runtime.replay_durable_write_failures(&handle);
    assert_eq!(
        handle.state().unwrap(),
        AgentState::ReadyForNextTurn,
        "the effective abort applies exactly once"
    );
}

#[tokio::test]
async fn provider_failure_unknown_effect_journal_loss_is_marked_and_replayed() {
    // Adversarial: the Unknown-effect provider-failure path marks the
    // running tool rows (already propagated) and journals Failed. The
    // journal write fails; the classified NeedsUserInput end must survive
    // as the report and the journal must be reconstructed on reopen.
    let (deps, dir) = deps(scripted_provider(vec![ScriptedResponse::End]), vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let manager = runtime.deps().session.clone();
    let session = new_session(runtime.deps());
    let h = manager.get_session(session).unwrap().unwrap();
    let receipt = h.submit_prompt("crash mid-tool", &[]).unwrap();
    // The machine claims the tool run through the legal ToolRequested hop
    // (a bare Preparing -> ExecutingTool hop is refused by the machine).
    h.force_append_event(
        faktor_core::event::EventKind::ToolRequested,
        AgentState::ToolRequested,
        Some(receipt.op_id),
        None,
    )
    .unwrap();
    h.start_tool_run(receipt.op_meta.clone(), "boom", serde_json::json!({}))
        .unwrap();
    h.force_append_event(
        faktor_core::event::EventKind::ModelStarted,
        AgentState::Streaming,
        Some(receipt.op_id),
        None,
    )
    .unwrap();
    assert_eq!(h.pending_tool_runs().unwrap().len(), 1);
    durable_faults_tests::arm(
        runtime.deps().session.store().root(),
        DW_SITE_PROVIDER_FAILURE_JOURNAL,
    );
    let mut outcome = TurnOutcome {
        op_id: receipt.op_id,
        final_state: AgentState::Preparing,
        turns: 0,
        compacted: false,
        loop_stopped: false,
        stalled: false,
        queued: false,
        verification: Vec::new(),
        acceptance: None,
        review: None,
        completion: None,
        stop_reason: None,
        semantic_risk: None,
        evidence_poll: None,
    };
    let ended = runtime
        .handle_provider_failure(
            &h,
            receipt.op_id,
            faktor_provider::ProviderError::new(
                faktor_provider::ProviderErrorKind::Server,
                "stream died",
            ),
            &mut outcome,
        )
        .await
        .unwrap();
    assert_eq!(
        ended.final_state,
        AgentState::NeedsUserInput,
        "the Unknown-effect classification must survive the lost journal"
    );
    assert_eq!(
        h.pending_tool_runs().unwrap()[0].effect_status,
        effect_tag(EffectStatus::Unknown)
    );
    let root = manager.store().root().to_path_buf();
    assert_eq!(
        marker_sites(&root),
        vec![DW_SITE_PROVIDER_FAILURE_JOURNAL.to_string()]
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
        h2.events_range(1, Some(512)).unwrap().iter().any(|e| {
            e.op_id == Some(receipt.op_id) && e.kind == faktor_core::event::EventKind::Failed
        }),
        "the Unknown-effect Failed journal was not reconstructed"
    );
    assert_eq!(h2.state().unwrap(), AgentState::NeedsUserInput);
    assert!(marker_files(manager2.store().root()).is_empty());
}

#[tokio::test]
async fn cancelled_turn_abort_loss_is_marked_and_replayed() {
    // Adversarial: the cancellation cleanup's durable abort write fails.
    // The running tool row must stay running (never a silent claim), the
    // marker must be durable, and the next open must replay the abort
    // (ToolCancelled journal + the running row finished as cancelled).
    let (deps, _dir) = deps(scripted_provider(vec![ScriptedResponse::End]), vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let manager = runtime.deps().session.clone();
    let session = new_session(runtime.deps());
    let h = manager.get_session(session).unwrap().unwrap();
    h.submit_prompt("cancel me", &[]).unwrap();
    let tool_meta = op_meta(&manager, session, RecoveryStrategy::None);
    let tool_op = tool_meta.operation_id;
    h.force_append_event(
        faktor_core::event::EventKind::ToolRequested,
        AgentState::ToolRequested,
        Some(tool_op),
        None,
    )
    .unwrap();
    h.start_tool_run(tool_meta, "boom", serde_json::json!({}))
        .unwrap();
    assert_eq!(h.pending_tool_runs().unwrap().len(), 1);
    durable_faults_tests::arm(
        runtime.deps().session.store().root(),
        DW_SITE_DRIVE_ABORT_CANCEL,
    );
    runtime.dw_note_abort(&h, Some(tool_op), DW_SITE_DRIVE_ABORT_CANCEL);
    assert_eq!(
        h.pending_tool_runs().unwrap().len(),
        1,
        "the lost abort left the row running"
    );
    let root = manager.store().root().to_path_buf();
    assert_eq!(
        marker_sites(&root),
        vec![DW_SITE_DRIVE_ABORT_CANCEL.to_string()]
    );
    // The next open (same daemon process: the op is still tracked)
    // replays the abort durably.
    runtime.recover().unwrap();
    let h = manager.get_session(session).unwrap().unwrap();
    assert!(h.pending_tool_runs().unwrap().is_empty());
    let events = h.events_range(1, Some(512)).unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == faktor_core::event::EventKind::ToolCancelled),
        "the abort was not reconstructed: {events:?}"
    );
    assert!(marker_files(&root).is_empty());
}
