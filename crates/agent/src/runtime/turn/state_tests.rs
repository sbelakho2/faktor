//! `runtime::turn::state_tests`: out-of-line tests.

use super::*;
use crate::runtime::fixtures_tests::*;
use crate::runtime::tests::*;
use crate::*;

/// Token economics (spec §2, release-blocking): a registered but
/// INACTIVE lazy tool leaves the Implement bundle byte-identical to a
/// registry without it; an ordinary prompt never activates it.
#[test]
fn inactive_lazy_registration_is_byte_identical_and_ordinary_prompts_stay_empty() {
    let caps = ModelCapabilities::default();
    let baseline = ToolRegistry::new().bundle_for_phase(RouterPhase::Implement, &caps);
    let with_lazy = lazy_registry();
    let mut activation = ToolActivationSet::new();
    with_lazy.observe_activation("fix the parser and run the tests", &mut activation);
    assert!(
        activation.is_empty(),
        "an ordinary prompt must not activate"
    );
    let inactive =
        with_lazy.bundle_for_phase_with_activation(RouterPhase::Implement, &caps, &activation);
    assert_eq!(
        serde_json::to_vec(&baseline).unwrap(),
        serde_json::to_vec(&inactive).unwrap(),
        "an inactive lazy tool must not change one wire bundle byte"
    );
    assert_eq!(baseline.bundle_hash(), inactive.bundle_hash());
}

/// The guard does not weaken normal tools: in ONE mixed batch the
/// inactive lazy call is refused while a normal sibling executes, and the
/// turn continues lawfully.
#[tokio::test]
async fn membership_guard_refuses_inactive_lazy_call_but_normal_sibling_executes() {
    let market_execs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let echo_execs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut registry = counting_lazy_registry(market_execs.clone());
    registry.register(counting_echo_tool(echo_execs.clone()));
    let (mut deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "market".into(),
                name: "source_market".into(),
                input: serde_json::json!({"op": "search"}),
            },
            ScriptedResponse::ToolCall {
                id: "ok".into(),
                name: "echo".into(),
                input: serde_json::json!({"x": 1}),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ]),
        vec![],
    );
    deps.tools = Arc::new(registry);
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let outcome = runtime.run_turn(session, "use both", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_eq!(
        market_execs.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the inactive lazy call must never execute"
    );
    assert_eq!(
        echo_execs.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the normal sibling must be unaffected by the membership guard"
    );
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let (echo_excerpt, echo_exit) = tool_result_for(&handle, "ok").expect("echo answered");
    assert_eq!(echo_exit, Some(0));
    assert_eq!(echo_excerpt, "echo: {\"x\":1}");
    let (refusal, refusal_exit) = tool_result_for(&handle, "market").expect("refused");
    assert_eq!(refusal_exit, Some(1));
    assert!(
        refusal.contains("tool call denied (not_in_active_bundle)"),
        "{refusal}"
    );
    assert!(
        !refusal.contains("CANARY"),
        "the refusal must not leak schema/description: {refusal}"
    );
    assert!(dangling_tool_calls(&handle).is_empty());
}

#[tokio::test]
async fn repeated_malformed_calls_stop_the_turn() {
    // The provider emits the same broken call three times; the loop
    // detector stops instead of repeating.
    let (deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "bad_1".into(),
                name: "echo".into(),
                input: serde_json::json!({"x": 1}),
            },
            ScriptedResponse::ToolCall {
                id: "bad_2".into(),
                name: "echo".into(),
                input: serde_json::json!({"x": 1}),
            },
            ScriptedResponse::ToolCall {
                id: "bad_3".into(),
                name: "echo".into(),
                input: serde_json::json!({"x": 1}),
            },
            ScriptedResponse::End,
        ]),
        vec![echo_tool()],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let outcome = runtime.run_turn(session, "x", &[]).await.unwrap();
    assert!(
        outcome.loop_stopped,
        "identical repeated calls must trip the detector"
    );
    assert_eq!(outcome.final_state, AgentState::FailedRecoverable);
}

#[tokio::test]
async fn all_denied_multi_call_batch_ends_lawfully() {
    // Adversarial: EVERY call of the batch is denied. While the batch is
    // still open the denials land on the batch-execution edge; the
    // genuine end then walks the legal interior (ExecutingTool ->
    // Validating -> UpdatingMemory -> ReadyForNextTurn) and both calls
    // are answered. Nothing executes and no second request is made.
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
        ],
        &["write_file"],
        vec![counting_write_tool(denied_execs.clone())],
    );
    let outcome = runtime
        .run_turn(session, "refused twice", &[])
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_eq!(denied_execs.load(std::sync::atomic::Ordering::SeqCst), 0);
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    assert!(dangling_tool_calls(&handle).is_empty());
    assert_permission_denial(&handle, "refused_1", "write_file");
    assert_permission_denial(&handle, "refused_2", "write_file");
    assert!(handle.pending_tool_runs().unwrap().is_empty());
    let events = handle.events_range(1, None).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == faktor_core::event::EventKind::PermissionDenied)
            .count(),
        2
    );
    assert_eq!(
        recorder.requests().len(),
        1,
        "nothing executed, so the turn ends without another model request"
    );
}

#[tokio::test]
async fn deny_only_batch_keeps_the_documented_ready_landing() {
    // CONTROL (unchanged): a batch whose ONLY call is denied has no
    // pending sibling — the denial lands `ReadyForNextTurn` directly and
    // the genuine end needs no interior hop.
    let denied_execs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (runtime, _recorder, session, _dir) = mixed_permission_batch(
        vec![(
            "refused".into(),
            "write_file".into(),
            serde_json::json!({"path": "a.txt", "content": "a"}),
        )],
        &["write_file"],
        vec![counting_write_tool(denied_execs.clone())],
    );
    let outcome = runtime.run_turn(session, "refused", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_eq!(denied_execs.load(std::sync::atomic::Ordering::SeqCst), 0);
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    assert_permission_denial(&handle, "refused", "write_file");
    let events = handle.events_range(1, None).unwrap();
    let denial = events
        .iter()
        .find(|e| e.kind == faktor_core::event::EventKind::PermissionDenied)
        .expect("the denial is journaled");
    assert_eq!(denial.state, AgentState::ReadyForNextTurn);
    assert!(!events
        .iter()
        .any(|e| e.kind == faktor_core::event::EventKind::ToolStarted));
    assert!(!events
        .iter()
        .any(|e| e.kind == faktor_core::event::EventKind::FileChanged));
}

#[tokio::test]
async fn approve_only_multi_call_batch_is_unchanged() {
    // CONTROL (unchanged): an all-approved batch takes the documented
    // interior hop and every call is answered with its real output.
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (runtime, recorder, session, _dir) = mixed_permission_batch(
        vec![
            ("ok_1".into(), "echo".into(), serde_json::json!({"x": 1})),
            ("ok_2".into(), "echo".into(), serde_json::json!({"x": 2})),
        ],
        &[],
        vec![counting_echo_tool(executions.clone())],
    );
    let outcome = runtime.run_turn(session, "echo twice", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 2);
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    assert!(dangling_tool_calls(&handle).is_empty());
    for (call, x) in [("ok_1", 1), ("ok_2", 2)] {
        let (excerpt, exit) = tool_result_for(&handle, call).expect("approved result");
        assert_eq!(exit, Some(0));
        assert_eq!(excerpt, format!("echo: {{\"x\":{x}}}"));
    }
    let events = handle.events_range(1, None).unwrap();
    assert!(!events
        .iter()
        .any(|e| e.kind == faktor_core::event::EventKind::PermissionDenied));
    let last_completed = events
        .iter()
        .rposition(|e| {
            e.kind == faktor_core::event::EventKind::ToolCompleted
                && e.state == AgentState::Validating
        })
        .expect("the completed finishes are journaled");
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
    assert_eq!(recorder.requests().len(), 2);
    assert_eq!(
        wire_tool_results(&recorder),
        vec![("ok_1".to_string(), false), ("ok_2".to_string(), false)],
    );
}

#[cfg(unix)]
#[tokio::test]
async fn hook_refusal_is_answered_with_typed_result() {
    // A PreTool lifecycle hook fails closed (Deny): the refusal is
    // journaled and answered with the typed hook denial.
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (mut deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "c_hook".into(),
                name: "echo".into(),
                input: serde_json::json!({"x": 1}),
            },
            ScriptedResponse::End,
        ]),
        vec![counting_echo_tool(executions.clone())],
    );
    let hooks = Arc::new(faktor_hooks::HookRegistry::try_new().expect("standalone supervisor"));
    hooks
        .register(failing_closed_hook(
            "pre_deny",
            faktor_hooks::HookEvent::PreTool,
        ))
        .unwrap();
    deps.hooks = Some(hooks);
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let outcome = runtime.run_turn(session, "use echo", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_refusal_answered(
        &runtime,
        session,
        "c_hook",
        "hook_denied",
        "denied by hook",
        &executions,
    );
}

#[tokio::test]
async fn secret_refusal_is_answered_with_typed_result_and_never_echoes_the_secret() {
    // The denial result carries the detected KIND, never the credential
    // bytes it detected.
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (deps, _dir) = deps(
        scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "leak_1".into(),
                name: "write_file".into(),
                input: serde_json::json!({ "path": "creds.txt", "content": SK_SAMPLE }),
            },
            ScriptedResponse::End,
        ]),
        vec![counting_write_tool(executions.clone())],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let outcome = runtime
        .run_turn(session, "store the key", &[])
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    assert_refusal_answered(
        &runtime,
        session,
        "leak_1",
        "secret_detected",
        "secret detected in tool input (openai_key)",
        &executions,
    );
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let (excerpt, _) = tool_result_for(&handle, "leak_1").unwrap();
    assert!(
        !excerpt.contains(SK_SAMPLE),
        "the denial result must never echo the secret: {excerpt}"
    );
}

#[test]
fn agent_cards_reflect_state() {
    let (deps, _dir) = deps(scripted_provider(vec![ScriptedResponse::End]), vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let cards = runtime.cards().unwrap();
    let card = cards.iter().find(|c| c.session_id == session).unwrap();
    assert!(card.status == "waiting" || card.status == "completed" || card.status == "running");
}

// ---- legacy VerifyHash containment migration (audit P1-F) ----

/// A legacy absolute path that is PROVABLY inside the session's durable
/// workspace migrates ONCE to a normalized relative postcondition and
/// verifies through the workspace handle — never through the raw path.
#[tokio::test]
async fn legacy_verify_inside_workspace_migrates_and_verifies_through_handle() {
    let dir = fresh_store_dir();
    let session: SessionId;
    let tool_op: OpId;
    let root: std::path::PathBuf;
    let expected = FileHash::from(blake3::hash(b"landed").into());
    {
        let (manager, _wid, sid, root_path) = workspace_env(&dir);
        session = sid;
        root = root_path;
        std::fs::write(root.join("target.txt"), b"landed").unwrap();
        let handle = manager.get_session(session).unwrap().unwrap();
        let receipt = handle.submit_prompt("crash verify", &[]).unwrap();
        chain_to_streaming(&handle, receipt.op_id);
        let meta = op_meta(
            &manager,
            session,
            RecoveryStrategy::VerifyHash {
                path: root.join("target.txt").to_string_lossy().to_string(),
                expected,
            },
        );
        tool_op = meta.operation_id;
        crash_tool_start(
            &handle,
            receipt.op_id,
            "write_file",
            serde_json::json!({}),
            "call_1",
            meta,
        );
    }
    let (deps2, _keep2) = reopen_runtime(&dir, Arc::new(scripted_provider(vec![])), vec![]);
    let runtime2 = AgentRuntime::new(deps2).unwrap();
    let reports = runtime2.recover().unwrap();
    let report = reports.iter().find(|r| r.session_id == session).unwrap();
    assert_eq!(report.crashed_ops.len(), 1);
    assert_eq!(report.crashed_ops[0].status, "completed");
    assert_eq!(report.crashed_ops[0].effect, EffectStatus::Verified);
    let handle2 = runtime2
        .deps()
        .session
        .get_session(session)
        .unwrap()
        .unwrap();
    assert!(handle2.pending_tool_runs().unwrap().is_empty());
    // The migration is durable audit evidence: the RecoveryApplied
    // journal names the RELATIVE identity the run was verified through.
    let events = handle2.events_range(1, None).unwrap();
    let migrated = events.iter().any(|e| {
        e.kind == faktor_core::event::EventKind::RecoveryApplied
            && e.op_id == Some(tool_op)
            && e.payload.as_ref().is_some_and(|p| {
                p.get("legacy_migrated_to").and_then(|v| v.as_str()) == Some("target.txt")
                    && p.get("status").and_then(|v| v.as_str()) == Some("completed")
            })
    });
    assert!(migrated, "the one-time migration must be journaled");
    // The verified file is untouched: verification is a read, nothing
    // re-ran.
    assert_eq!(std::fs::read(root.join("target.txt")).unwrap(), b"landed");
    // Second sweep: the row is terminal — nothing re-derived, nothing
    // re-verified.
    let reports = runtime2.recover().unwrap();
    assert!(reports.iter().all(|r| r.crashed_ops.is_empty()));
}

/// Legacy paths that cannot be PROVEN inside the workspace — a `..`
/// climb, a symlink pointing out, a different root — classify the effect
/// failed/Unknown and demand a human decision: NEVER Verified. The
/// outside files deliberately hold bytes matching `expected`, so the old
/// raw-path hash would have "verified" them; containment proof is the
/// only thing standing between the row and a false completion.
#[tokio::test]
async fn legacy_verify_paths_outside_workspace_are_unknown_never_verified() {
    let dir = fresh_store_dir();
    let session: SessionId;
    let outside: Vec<std::path::PathBuf>;
    let expected = FileHash::from(blake3::hash(b"outside-marker").into());
    let other_root = tempdir().unwrap();
    {
        let (manager, _wid, sid, root) = workspace_env(&dir);
        session = sid;
        let handle = manager.get_session(session).unwrap().unwrap();
        let receipt = handle.submit_prompt("crash verify", &[]).unwrap();
        chain_to_streaming(&handle, receipt.op_id);
        // (1) A `..` climb out of the workspace.
        let climb = root.join("..").join("outside.txt");
        std::fs::write(&climb, b"outside-marker").unwrap();
        // (2) A different root entirely.
        let different = other_root.path().join("other.txt");
        std::fs::write(&different, b"outside-marker").unwrap();
        let mut outside_paths = vec![climb.clone(), different.clone()];
        let mut paths = vec![
            climb.to_string_lossy().to_string(),
            different.to_string_lossy().to_string(),
        ];
        // (3) A symlink inside the root pointing OUT of it.
        #[cfg(unix)]
        {
            let out_dir = dir.path().join("outside-dir");
            std::fs::create_dir_all(&out_dir).unwrap();
            let target = out_dir.join("secret.txt");
            std::fs::write(&target, b"outside-marker").unwrap();
            std::os::unix::fs::symlink(&out_dir, root.join("link")).unwrap();
            outside_paths.push(target.clone());
            paths.push(
                root.join("link")
                    .join("secret.txt")
                    .to_string_lossy()
                    .to_string(),
            );
        }
        outside = outside_paths;
        for (i, path) in paths.iter().enumerate() {
            let meta = op_meta(
                &manager,
                session,
                RecoveryStrategy::VerifyHash {
                    path: path.clone(),
                    expected,
                },
            );
            crash_tool_start(
                &handle,
                receipt.op_id,
                "write_file",
                serde_json::json!({}),
                &format!("call_{i}"),
                meta,
            );
        }
    }
    let (deps2, _keep2) = reopen_runtime(&dir, Arc::new(scripted_provider(vec![])), vec![]);
    let runtime2 = AgentRuntime::new(deps2).unwrap();
    let reports = runtime2.recover().unwrap();
    let report = reports.iter().find(|r| r.session_id == session).unwrap();
    assert_eq!(report.crashed_ops.len(), outside.len());
    for op in &report.crashed_ops {
        assert_eq!(op.status, "failed", "{op:?}");
        assert_eq!(op.effect, EffectStatus::Unknown, "{op:?}");
        assert_eq!(op.action, RecoveryAction::NeedsHuman, "{op:?}");
    }
    let handle2 = runtime2
        .deps()
        .session
        .get_session(session)
        .unwrap()
        .unwrap();
    assert!(
        handle2.pending_tool_runs().unwrap().is_empty(),
        "unverifiable legacy rows are terminally failed, never re-scanned"
    );
    // No completion was ever journaled for these rows.
    assert!(!handle2.events_range(1, None).unwrap().iter().any(|e| {
        e.kind == faktor_core::event::EventKind::RecoveryApplied
            && e.payload
                .as_ref()
                .is_some_and(|p| p.get("effect").and_then(|v| v.as_str()) == Some("verified"))
    }));
    // The outside markers are untouched: no outside file was hashed into
    // a completion and none was written.
    for path in &outside {
        assert_eq!(std::fs::read(path).unwrap(), b"outside-marker");
    }
    // A second sweep must not re-litigate the terminal rows.
    let reports = runtime2.recover().unwrap();
    assert!(reports.iter().all(|r| r.crashed_ops.is_empty()));
}

/// A swap AFTER the one-time migration (the migration write landed, the
/// verification did not) cannot redirect the read: verification goes
/// through the handle's relative, symlink-bounded open, so a parent
/// directory swapped for a symlink out of the workspace is refused
/// loudly — the outside file whose bytes match `expected` is never
/// hashed into a completion.
#[cfg(unix)]
#[tokio::test]
async fn legacy_verify_swap_after_migration_uses_handle_relative_read() {
    let dir = fresh_store_dir();
    let session: SessionId;
    let tool_op: OpId;
    let root: std::path::PathBuf;
    let outside_dir: std::path::PathBuf;
    let expected = FileHash::from(blake3::hash(b"payload").into());
    {
        let (manager, _wid, sid, root_path) = workspace_env(&dir);
        session = sid;
        root = root_path;
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub").join("a.txt"), b"payload").unwrap();
        outside_dir = dir.path().join("outside");
        std::fs::create_dir_all(&outside_dir).unwrap();
        // Equal bytes outside: a raw absolute-path read would "verify".
        std::fs::write(outside_dir.join("a.txt"), b"payload").unwrap();
        let handle = manager.get_session(session).unwrap().unwrap();
        let receipt = handle.submit_prompt("crash verify", &[]).unwrap();
        chain_to_streaming(&handle, receipt.op_id);
        let legacy_path = root.join("sub").join("a.txt");
        let meta = op_meta(
            &manager,
            session,
            RecoveryStrategy::VerifyHash {
                path: legacy_path.to_string_lossy().to_string(),
                expected,
            },
        );
        tool_op = meta.operation_id;
        crash_tool_start(
            &handle,
            receipt.op_id,
            "write_file",
            serde_json::json!({}),
            "call_1",
            meta,
        );
        // Deterministic crash MID-migration: run the migration helper
        // (the durable midpoint of recover()'s legacy arm) and drop the
        // runtime before the verification/finish.
        let (deps_x, keep_x) =
            deps_sharing_session(manager.clone(), Arc::new(scripted_provider(vec![])), vec![]);
        let runtime_x = AgentRuntime::new(deps_x).unwrap();
        let pending = handle.pending_tool_runs().unwrap();
        let row = pending.iter().find(|r| r.op_id == tool_op).unwrap();
        let migrated = runtime_x
            .migrate_legacy_verify_row(&handle, row, &legacy_path.to_string_lossy(), expected)
            .unwrap()
            .expect("a provably-inside path must migrate");
        assert_eq!(migrated.relative_path, "sub/a.txt");
        drop(runtime_x);
        drop(keep_x);
        // The race: the verified path's parent becomes a symlink to the
        // outside directory AFTER the migration proof.
        std::fs::remove_dir_all(root.join("sub")).unwrap();
        std::os::unix::fs::symlink(&outside_dir, root.join("sub")).unwrap();
    }
    let (deps2, _keep2) = reopen_runtime(&dir, Arc::new(scripted_provider(vec![])), vec![]);
    let runtime2 = AgentRuntime::new(deps2).unwrap();
    let err = runtime2.recover().unwrap_err();
    assert_eq!(
        err.kind,
        ErrorKind::Permission,
        "the swapped parent must be refused by handle-relative resolution: {err}"
    );
    // The row stays running (visible, never silently completed) and the
    // outside file was never hashed into a completion.
    let handle2 = runtime2
        .deps()
        .session
        .get_session(session)
        .unwrap()
        .unwrap();
    assert_eq!(handle2.pending_tool_runs().unwrap().len(), 1);
    assert!(!handle2
        .events_range(1, None)
        .unwrap()
        .iter()
        .any(|e| { e.kind == faktor_core::event::EventKind::RecoveryApplied }));
    assert_eq!(
        std::fs::read(outside_dir.join("a.txt")).unwrap(),
        b"payload"
    );
}

/// The one-time migration is durable BEFORE any verification: a crash
/// after the migration write but before the finish resumes through the
/// modern relative postcondition exactly once (idempotent on re-sweep);
/// the legacy string is never consulted again.
#[tokio::test]
async fn legacy_verify_migration_midpoint_resumes_once_and_is_idempotent() {
    let dir = fresh_store_dir();
    let session: SessionId;
    let tool_op: OpId;
    let root: std::path::PathBuf;
    let expected = FileHash::from(blake3::hash(b"landed").into());
    {
        let (manager, wid, sid, root_path) = workspace_env(&dir);
        session = sid;
        root = root_path;
        std::fs::write(root.join("target.txt"), b"landed").unwrap();
        let handle = manager.get_session(session).unwrap().unwrap();
        let receipt = handle.submit_prompt("crash verify", &[]).unwrap();
        chain_to_streaming(&handle, receipt.op_id);
        let legacy_path = root.join("target.txt");
        let meta = op_meta(
            &manager,
            session,
            RecoveryStrategy::VerifyHash {
                path: legacy_path.to_string_lossy().to_string(),
                expected,
            },
        );
        tool_op = meta.operation_id;
        crash_tool_start(
            &handle,
            receipt.op_id,
            "write_file",
            serde_json::json!({}),
            "call_1",
            meta,
        );
        // Crash mid-migration: the durable postcondition write landed,
        // the finish did not.
        let (deps_x, keep_x) =
            deps_sharing_session(manager.clone(), Arc::new(scripted_provider(vec![])), vec![]);
        let runtime_x = AgentRuntime::new(deps_x).unwrap();
        let pending = handle.pending_tool_runs().unwrap();
        let row = pending.iter().find(|r| r.op_id == tool_op).unwrap();
        let migrated = runtime_x
            .migrate_legacy_verify_row(&handle, row, &legacy_path.to_string_lossy(), expected)
            .unwrap()
            .expect("a provably-inside path must migrate");
        assert_eq!(migrated.workspace_id, wid);
        assert_eq!(migrated.relative_path, "target.txt");
        // The durable midpoint: the running row now carries the modern
        // relative postcondition.
        let pending = handle.pending_tool_runs().unwrap();
        assert_eq!(pending.len(), 1);
        let stored: FilePostcondition =
            serde_json::from_value(pending[0].postcondition.clone().unwrap()).unwrap();
        assert_eq!(stored.relative_path, "target.txt");
        assert_eq!(stored.expected_hash, expected);
        drop(runtime_x);
        drop(keep_x);
    }
    let (deps2, _keep2) = reopen_runtime(&dir, Arc::new(scripted_provider(vec![])), vec![]);
    let runtime2 = AgentRuntime::new(deps2).unwrap();
    let reports = runtime2.recover().unwrap();
    let report = reports.iter().find(|r| r.session_id == session).unwrap();
    assert_eq!(report.crashed_ops.len(), 1);
    assert_eq!(report.crashed_ops[0].status, "completed");
    assert_eq!(report.crashed_ops[0].effect, EffectStatus::Verified);
    let handle2 = runtime2
        .deps()
        .session
        .get_session(session)
        .unwrap()
        .unwrap();
    assert!(handle2.pending_tool_runs().unwrap().is_empty());
    // The resume went through the modern postcondition: no second
    // legacy migration was journaled.
    assert!(!handle2.events_range(1, None).unwrap().iter().any(|e| {
        e.payload
            .as_ref()
            .is_some_and(|p| p.get("legacy_migrated_to").is_some())
    }));
    // Idempotent: a second sweep finds nothing pending.
    let reports = runtime2.recover().unwrap();
    assert!(reports.iter().all(|r| r.crashed_ops.is_empty()));
}

#[tokio::test]
async fn reasoning_persists_separately_and_roundtrips_as_reasoning() {
    // Turn 1: the provider streams thinking then text. The durable
    // parts must keep them separate (reasoning row before text row) and
    // the second request must reconstruct ContentKind::Reasoning —
    // never merged into the assistant text.
    let fake = Arc::new(scripted_provider(vec![
        ScriptedResponse::Reasoning("let me think".into()),
        ScriptedResponse::Text("the answer".into()),
        ScriptedResponse::End,
    ]));
    let hook = |n: usize, req: &GenericAgentRequest| -> Result<(), String> {
        if n == 0 {
            return Ok(());
        }
        let assistant = req
            .messages
            .iter()
            .find(|m| m.role == Role::Assistant)
            .ok_or_else(|| "assistant history missing".to_string())?;
        let kinds: Vec<&str> = assistant
            .content
            .iter()
            .map(|c| match &c.kind {
                faktor_provider::ContentKind::Reasoning { .. } => "reasoning",
                faktor_provider::ContentKind::Text { .. } => "text",
                other => panic!("unexpected content kind {other:?}"),
            })
            .collect();
        if kinds != vec!["reasoning", "text"] {
            return Err(format!(
                "reasoning and text must roundtrip in order, got {kinds:?}"
            ));
        }
        match &assistant.content[0].kind {
            faktor_provider::ContentKind::Reasoning { text } if text == "let me think" => {}
            other => return Err(format!("reasoning content lost or merged, got {other:?}")),
        }
        match &assistant.content[1].kind {
            faktor_provider::ContentKind::Text { text } if text == "the answer" => {}
            other => return Err(format!("assistant text corrupted, got {other:?}")),
        }
        Ok(())
    };
    let wrapper = Arc::new(InspectingProvider::new(fake.clone(), hook));
    let (deps, _dir) = deps_with(wrapper, vec![]);
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let receipt = handle.submit_prompt("t1", &[]).unwrap();
    let outcome = runtime
        .drive_turn(
            &handle,
            receipt.op_id,
            receipt.op_meta.cancellation.clone(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);

    let page = handle.messages_page(None, 20).unwrap();
    let assistant = page
        .messages
        .iter()
        .find(|m| m.role == "assistant")
        .expect("assistant message exists");
    let part_kinds: Vec<&str> = assistant
        .parts
        .iter()
        .map(|p| match p {
            faktor_protocol::native::Part::Reasoning { .. } => "reasoning",
            faktor_protocol::native::Part::Text { .. } => "text",
            other => panic!("unexpected durable part {other:?}"),
        })
        .collect();
    assert_eq!(
        part_kinds,
        vec!["reasoning", "text"],
        "thinking rows precede text rows in the durable part order"
    );
    match &assistant.parts[0] {
        faktor_protocol::native::Part::Reasoning { text } => {
            assert_eq!(text, "let me think");
        }
        other => panic!("wrong part {other:?}"),
    }
    match &assistant.parts[1] {
        faktor_protocol::native::Part::Text { text } => {
            assert_eq!(text, "the answer", "reasoning must never leak into text");
        }
        other => panic!("wrong part {other:?}"),
    }

    // Turn 2: reseed the script; the wrapper inspects the request and
    // refuses the stream unless reasoning roundtrips as its own kind.
    *fake.script.lock().unwrap() = vec![
        ScriptedResponse::Text("second reply".into()),
        ScriptedResponse::End,
    ];
    let receipt = handle.submit_prompt("t2", &[]).unwrap();
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
        "the second turn only completes when reasoning roundtrips as ContentKind::Reasoning"
    );
}

#[tokio::test]
async fn scheduler_panic_returns_the_original_typed_error_and_resolves_open_rows() {
    // P1 error-collapse: a spawned scheduler task panic
    // (`ErrorKind::Internal`) used to be swallowed by
    // `unwrap_or_default()` into an empty done set — every submitted op
    // was then finished as an ordinary failed tool and `run_tool_calls`
    // continued its normal path. The typed scheduler failure must
    // surface, and every still-open durable row must be resolved before
    // it does (never left `running`).
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
            ScriptedResponse::Text("unreachable".into()),
            ScriptedResponse::End,
        ]),
        vec![echo_tool()],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    scheduler_faults_tests::arm(
        runtime.deps().session.store().root(),
        scheduler_faults_tests::Point::BeforeRun,
        ErrorKind::Internal,
        "scheduler task failed unexpectedly",
    );
    let err = runtime
        .run_turn(session, "do work", &[])
        .await
        .expect_err("a scheduler task panic must not collapse into Ok");
    assert_eq!(err.kind, ErrorKind::Internal, "{err:?}");
    assert!(err.message.contains("scheduler task failed"), "{err:?}");
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    assert!(
        handle.pending_tool_runs().unwrap().is_empty(),
        "every submitted op must be durably resolved before the error returns"
    );
    let statuses = tool_completed_statuses(&handle);
    assert_eq!(statuses.len(), 2, "both open runs resolved: {statuses:?}");
    for (_, status) in statuses {
        assert_eq!(status, "failed");
    }
}

#[tokio::test]
async fn scheduler_deadlock_is_typed_and_never_masks_a_completed_op() {
    // A deadlock classification after a PARTIALLY completed batch: the
    // op that genuinely finished stays recorded `completed` (the live
    // scheduler status is the truth), the op that failed on its own is
    // resolved as a failed tool, and the returned error is the ORIGINAL
    // `ErrorKind::Deadlock` — never an empty done set and never Ok.
    // The FAILING call is submitted FIRST on purpose: resolving strictly
    // in submit order would finish it first (`FailedRecoverable`) and
    // then reject the completed op's finish (`FailedRecoverable ->
    // Validating` is illegal), masking the scheduler error with a bogus
    // `InvalidState`.
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
            ScriptedResponse::Text("unreachable".into()),
            ScriptedResponse::End,
        ]),
        vec![echo_tool(), failing_tool()],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    scheduler_faults_tests::arm(
        runtime.deps().session.store().root(),
        scheduler_faults_tests::Point::AfterRun,
        ErrorKind::Deadlock,
        "no ready tasks and work remains; cycle or unscheduled dependency",
    );
    let err = runtime
        .run_turn(session, "do work", &[])
        .await
        .expect_err("a deadlock must surface typed");
    assert_eq!(err.kind, ErrorKind::Deadlock, "{err:?}");
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    assert!(handle.pending_tool_runs().unwrap().is_empty());
    let statuses = tool_completed_statuses(&handle);
    assert_eq!(statuses.len(), 2, "{statuses:?}");
    assert!(
        statuses.iter().any(|(_, s)| s == "completed"),
        "the completed op must stay completed: {statuses:?}"
    );
    assert!(
        statuses.iter().any(|(_, s)| s == "failed"),
        "the failed op must be resolved: {statuses:?}"
    );
}

#[tokio::test]
async fn scheduler_validation_refusal_is_typed_and_rows_still_resolve() {
    // A scheduler validation refusal (e.g. the DAG bound) is
    // infrastructure, not a per-tool failure: it must surface with its
    // own kind and still leave no open durable row behind.
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
    scheduler_faults_tests::arm(
        runtime.deps().session.store().root(),
        scheduler_faults_tests::Point::BeforeRun,
        ErrorKind::Oversized,
        "task 1 declares 9 dependencies; cap is 8",
    );
    let err = runtime
        .run_turn(session, "do work", &[])
        .await
        .expect_err("a validation refusal must surface typed");
    assert_eq!(err.kind, ErrorKind::Oversized, "{err:?}");
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    assert!(handle.pending_tool_runs().unwrap().is_empty());
    let statuses = tool_completed_statuses(&handle);
    assert_eq!(statuses.len(), 1, "{statuses:?}");
    assert_eq!(statuses[0].1, "failed");
}

#[tokio::test]
async fn scheduler_success_path_still_returns_the_done_set() {
    // Control: with no injected failure the operational path is
    // unchanged — every submitted op is in the scheduler's done set and
    // is recorded `completed` (the old resolution behavior byte-for-byte).
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
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ]),
        vec![echo_tool()],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let outcome = runtime.run_turn(session, "do work", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let statuses = tool_completed_statuses(&handle);
    assert_eq!(statuses.len(), 2, "{statuses:?}");
    for (_, status) in statuses {
        assert_eq!(status, "completed");
    }
    assert!(handle.pending_tool_runs().unwrap().is_empty());
    // The all-success interior hop is UNCHANGED by the mixed-batch fix:
    // the completed finishes leave `Validating`, then the documented
    // Validating -> UpdatingMemory -> WaitingForModel hops run.
    let events = handle.events_range(1, None).unwrap();
    let last_completed = events
        .iter()
        .rposition(|e| {
            e.kind == faktor_core::event::EventKind::ToolCompleted
                && e.state == AgentState::Validating
        })
        .expect("the completed finishes are journaled");
    let after: Vec<AgentState> = events[last_completed + 1..]
        .iter()
        .take(2)
        .map(|e| e.state)
        .collect();
    assert_eq!(
        after,
        vec![AgentState::UpdatingMemory, AgentState::WaitingForModel],
        "the all-success interior hop must stay byte-identical"
    );
}

/// Audit: a parked orchestrated child (durable Pause applied, no Resume
/// row yet) is BOUNDED by the turn budget. On exhaustion the drive
/// boundary returns a typed timeout (the executor re-drives it), and a
/// later Resume still ends the park immediately — the bound never
/// destroys the durable park state.
#[tokio::test]
async fn parked_child_wait_is_bounded_and_still_resumable() {
    let (deps, _dir) = deps(
        scripted_provider(vec![ScriptedResponse::Text("a".into())]),
        vec![],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    runtime.set_turn_budget_ms(60);
    let session = new_session(runtime.deps());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    handle
        .orchestrator_child_identity_put(&faktor_session::child::ChildIdentity::default())
        .unwrap();
    handle
        .orchestrator_ctl_enqueue(faktor_session::child::ChildControl::Pause)
        .unwrap();
    let op = runtime.deps.session.try_next_op_id().unwrap();
    let cancel = faktor_core::cancellation::CancellationToken::new();
    let mut model = "m".to_string();
    let began = Instant::now();
    let err = runtime
        .drive_boundary_controls(&handle, op, &cancel, &mut model)
        .await
        .expect_err("a park without Resume must time out");
    assert_eq!(err.kind, faktor_core::ErrorKind::Timeout, "{err:?}");
    assert!(err.message.contains("parked"), "{err:?}");
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "the park is bounded by the turn budget"
    );
    // The durable park is intact: a Resume row ends it immediately.
    handle
        .orchestrator_ctl_enqueue(faktor_session::child::ChildControl::Resume)
        .unwrap();
    let mut model2 = "m".to_string();
    runtime
        .drive_boundary_controls(&handle, op, &cancel, &mut model2)
        .await
        .expect("a pending Resume must end the park");
}

/// The frozen legacy first-match surface
/// ([`faktor_verify::detect_project_type`] + the single-project
/// [`faktor_verify::derive_checks`]) exists ONLY as the compatibility
/// contract and its own unit tests in `crates/verify`. No production
/// caller may consume it: this static scan walks every workspace crate's
/// production region (everything before the first `#[cfg(test)]`) and
/// fails on a legacy call. The multi-component `derive::derive_checks`
/// surface stays legal.
#[test]
fn no_production_caller_uses_the_frozen_legacy_project_surface() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let crates_dir = manifest.parent().expect("crates dir");
    let mut stack = vec![crates_dir.to_path_buf()];
    let mut scanned = 0usize;
    while let Some(dir) = stack.pop() {
        // `crates/verify` owns the frozen definitions and their tests.
        if dir.file_name().and_then(|name| name.to_str()) == Some("verify") {
            continue;
        }
        for entry in std::fs::read_dir(&dir).expect("readable source dir") {
            let path = entry.expect("readable dir entry").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }
            let src = std::fs::read_to_string(&path).expect("source is readable");
            let production = match src.find("#[cfg(test)]") {
                Some(index) => &src[..index],
                None => src.as_str(),
            };
            for (offset, line) in production.lines().enumerate() {
                assert!(
                    !line.contains("detect_project_type("),
                    "{}:{} uses the frozen detect_project_type",
                    path.display(),
                    offset + 1
                );
                if let Some(position) = line.find("derive_checks(") {
                    assert!(
                        line[..position].ends_with("derive::"),
                        "{}:{} uses the frozen single-project derive_checks",
                        path.display(),
                        offset + 1
                    );
                }
            }
            scanned += 1;
        }
    }
    assert!(
        scanned >= 40,
        "workspace scan must cover every crate, scanned {scanned}"
    );
}

#[tokio::test]
async fn scripted_backend_runs_background_policy_checks_inline_without_an_override_note() {
    // Adversarial (P0-10 migration, audit P0-5/26): the wave-17 typed
    // derivation classifies the CTest run as a FULL-repository check
    // whose policy budget says "task-owned background operation". The
    // scripted command backend (this test seam — instantaneous,
    // deterministic verdicts) still executes such decisions inline, but
    // the P0-10 inline-override note is DELETED: the durable
    // background-job machinery exists now and belongs to the real
    // supervisor-backed executor alone (see the background-job tests).
    // The gate still lands VerifiedComplete — a passing check is never
    // degraded and no universal wall cap hides anywhere.
    let dir = tempdir().unwrap();
    let root = dir.path().join("ws");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::write(
        root.join("CMakeLists.txt"),
        "cmake_minimum_required(VERSION 3.16)\nproject(x C)\nadd_executable(x main.c)\n",
    )
    .unwrap();
    std::fs::write(
            root.join("src/main.c"),
            "#include <stdio.h>\nint main(void) {\n    int base = 40;\n    int step = 2;\n    printf(\"%d\\n\", base + step);\n    return 0;\n}\n",
        )
        .unwrap();
    std::fs::write(
        root.join("tests/CMakeLists.txt"),
        "add_test(NAME t COMMAND x)\n",
    )
    .unwrap();
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let ws = manager.create_workspace(root.to_str().unwrap()).unwrap();
    let session = manager
        .create_session(ws, "cmake gating", "fake", "m")
        .unwrap()
        .id();
    let (mut deps, _d) = deps_sharing_session(
        manager.clone(),
        Arc::new(scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/main.c", "content": "#include <stdio.h>\nint main(void) {\n    int base = 40;\n    int step = 2;\n    printf(\"%d\\n\", base + step);\n    return 0;\n}\n"}),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ])),
        vec![real_write_tool()],
    );
    deps.verification = fake_ok();
    deps.compact_at_usage = 0.65;
    let runtime = AgentRuntime::new(deps).unwrap();
    let outcome = runtime
        .run_turn(session, "write main.c", &[])
        .await
        .unwrap();
    assert_eq!(outcome.acceptance, Some(faktor_verify::Acceptance::Pass));
    assert_eq!(outcome.completion, Some(CompletionGate::VerifiedComplete));
    assert_eq!(
        outcome.verification.len(),
        3,
        "cmake configure + build + ctest all ran: {:?}",
        outcome.verification
    );
    let h = manager.get_session(session).unwrap().unwrap();
    let records = h.list_verification_records(h.task_id().unwrap()).unwrap();
    assert_eq!(records.len(), 1);
    let checks = &records[0].checks;
    let configure = checks
        .iter()
        .find(|c| c.check == "cmake_configure")
        .expect("configure execution row");
    let build = checks
        .iter()
        .find(|c| c.check == "cmake_build")
        .expect("build execution row");
    let ctest = checks
        .iter()
        .find(|c| c.check == "cmake_ctest")
        .expect("ctest execution row");
    for row in [configure, build, ctest] {
        assert_eq!(row.status, VerificationStatus::Passed);
        assert_eq!(row.exit, Some(0));
        assert!(row.required);
    }
    assert_eq!(configure.program, "cmake");
    assert_eq!(build.program, "cmake");
    assert_eq!(ctest.program, "ctest");
    assert!(
        configure.summary.is_none() && build.summary.is_none() && ctest.summary.is_none(),
        "scripted checks carry plain summaries — the inline-override note is gone: \
              {configure:?} {build:?} {ctest:?}"
    );
}

#[tokio::test]
async fn daemon_executor_resolves_a_background_check_asynchronously_and_completion_waits() {
    // E2E (audit P0-5/26 production wiring): the exact primitive the
    // daemon verification executor runs every tick
    // ([`AgentRuntime::execute_open_verification_jobs`]) resolves the
    // queued check asynchronously. Until a genuine end consumes the
    // exact attempt, the task stays Verifying (completion WAITS) even
    // though the job row is already terminal; the next text turn then
    // settles from the durable result and completes.
    if !std::process::Command::new("make")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        eprintln!("skipping daemon-executor async resolution: no make on this host");
        return;
    }
    let (manager, session, _dir) =
        make_background_env("\t@true\n\techo executor-ran > executor-marker.txt\n", None);
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
    assert_eq!(
        outcome.completion,
        Some(CompletionGate::VerificationPending),
        "the mutating turn parks on the queued job"
    );
    let h = manager.get_session(session).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    let attempt = h
        .current_verification_attempt(task_id.raw())
        .unwrap()
        .unwrap();
    // ---- the daemon executor primitive: claim -> execute -> resolve ----
    let resolved = runtime
        .execute_open_verification_jobs(&h)
        .await
        .expect("the executor primitive resolves the queued job");
    assert_eq!(resolved, 1);
    let root = manager
        .resolve_workspace_root(session)
        .unwrap()
        .expect("workspace root");
    assert!(
        root.join("executor-marker.txt").exists(),
        "the executor really ran the check process"
    );
    let job = h
        .verification_attempt_jobs(task_id.raw(), attempt.op_id)
        .unwrap()
        .remove(0);
    assert_eq!(job.state, faktor_session::VerificationJobState::Passed);
    // Completion still WAITS: the terminal result belongs to the exact
    // attempt and only a genuine end may consume it.
    let task = h.get_task(task_id).unwrap().unwrap();
    assert_eq!(
        task.state,
        TaskState::Verifying,
        "an executed job alone never completes the task"
    );
    // ---- a later text turn consumes the exact attempt and completes ----
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
        "the async result is consumed exactly once: {settled:?}"
    );
    let h2 = manager.get_session(session).unwrap().unwrap();
    let task = h2.get_task(task_id).unwrap().unwrap();
    assert_eq!(task.state, TaskState::VerifiedComplete);
}

#[tokio::test]
async fn reverify_transition_supersedes_the_old_attempt_and_late_results_are_refused() {
    // Adversarial (audit P0-5/26): a NEW mutating turn supersedes the
    // open attempt of the previous content (typed Cancelled rows), and a
    // late result for the OLD attempt is typed-refused: it can never
    // resolve the fresh attempt, and the task never completes from it.
    if !std::process::Command::new("make")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        eprintln!("skipping reverify supersede: no make on this host");
        return;
    }
    let (manager, session, _dir) =
        make_background_env("\tsleep 2\n\techo superseded > never.txt\n", None);
    let (mut deps, _d) = deps_sharing_session(
        manager.clone(),
        Arc::new(scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/main.c", "content": "int main(void) {\n    int base = 40;\n    int step = 1;\n    printf(\"%d\\n\", base + step);\n    return 0;\n}\n"}),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ])),
        vec![real_write_tool()],
    );
    deps.verification = real_background_verifier();
    let runtime = AgentRuntime::new(deps).unwrap();
    let first = runtime
        .run_turn(session, "change main.c", &[])
        .await
        .unwrap();
    assert_eq!(first.completion, Some(CompletionGate::VerificationPending));
    let h = manager.get_session(session).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    let old_op = h
        .current_verification_attempt(task_id.raw())
        .unwrap()
        .unwrap()
        .op_id;
    // ---- reverify transition: a NEW mutating turn, new content ----
    let (mut deps2, _d2) = deps_sharing_session(
        manager.clone(),
        Arc::new(scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "c2".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/main.c", "content": "int main(void) {\n    int base = 40;\n    int step = 3;\n    printf(\"%d\\n\", base + step);\n    return 0;\n}\n"}),
            },
            ScriptedResponse::Text("changed again".into()),
            ScriptedResponse::End,
        ])),
        vec![real_write_tool()],
    );
    deps2.verification = real_background_verifier();
    let runtime2 = AgentRuntime::new(deps2).unwrap();
    let second = runtime2
        .run_turn(session, "change main.c again", &[])
        .await
        .unwrap();
    // The fresh derivation enqueued the NEW attempt: the turn parks.
    assert_eq!(second.completion, Some(CompletionGate::VerificationPending));
    let new_attempt = h
        .current_verification_attempt(task_id.raw())
        .unwrap()
        .unwrap();
    assert_ne!(new_attempt.op_id, old_op, "a FRESH attempt began");
    // Every job of the OLD attempt is typed Cancelled (never dropped).
    let old_rows = h.verification_attempt_jobs(task_id.raw(), old_op).unwrap();
    assert!(!old_rows.is_empty());
    assert!(
        old_rows
            .iter()
            .all(|j| j.state == faktor_session::VerificationJobState::Cancelled),
        "old attempt frozen as cancelled: {old_rows:?}"
    );
    // A LATE result for the superseded attempt is typed-refused and can
    // never resolve the new attempt.
    let late = h.resolve_verification_job(
        task_id.raw(),
        "make_test",
        old_op,
        faktor_session::VerificationJobState::Passed,
        None,
        Some("{}".into()),
    );
    let err = late.expect_err("a superseded attempt must refuse late results");
    assert!(
        matches!(err, faktor_session::SessionError::Conflict(_)),
        "{err:?}"
    );
    assert!(err.to_string().contains("superseded"), "{err}");
    let new_rows = h
        .verification_attempt_jobs(task_id.raw(), new_attempt.op_id)
        .unwrap();
    assert!(
        new_rows
            .iter()
            .all(|j| j.state == faktor_session::VerificationJobState::Queued),
        "the new attempt's jobs are untouched by the old result: {new_rows:?}"
    );
    let task = h.get_task(task_id).unwrap().unwrap();
    assert_eq!(task.state, TaskState::Verifying);
    // The executor resolves the FRESH attempt; a genuine end completes.
    assert_eq!(
        runtime2.execute_open_verification_jobs(&h).await.unwrap(),
        1
    );
    let (mut deps3, _d3) = deps_sharing_session(
        manager.clone(),
        Arc::new(scripted_provider(vec![
            ScriptedResponse::Text("status?".into()),
            ScriptedResponse::End,
        ])),
        vec![real_write_tool()],
    );
    deps3.verification = real_background_verifier();
    let runtime3 = AgentRuntime::new(deps3).unwrap();
    let settled = runtime3.run_turn(session, "status?", &[]).await.unwrap();
    assert_eq!(settled.completion, Some(CompletionGate::VerifiedComplete));
    let h3 = manager.get_session(session).unwrap().unwrap();
    assert_eq!(
        h3.get_task(task_id).unwrap().unwrap().state,
        TaskState::VerifiedComplete
    );
}

#[tokio::test]
async fn root_attempt_consumption_is_exact_and_starts_fresh_when_superseded() {
    // Audit P0-5/26 production wiring: the attempt-based integrated-root
    // verification consumes EXACTLY the recorded attempt — it never
    // re-derives/re-runs while the attempt is pending, rebuilds the
    // verdict from the settled rows when terminal, and starts a FRESH
    // attempt (structurally superseding the old one) when a newer
    // attempt exists.
    if !std::process::Command::new("make")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        eprintln!("skipping root attempt consumption: no make on this host");
        return;
    }
    let (manager, session, dir) =
        make_background_env("\t@true\n\techo root-attempt > root-marker.txt\n", None);
    let root = dir.path().join("ws");
    let (mut deps, _d) = deps_sharing_session(
        manager.clone(),
        Arc::new(scripted_provider(vec![
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ])),
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
        goal: "root attempt".into(),
        acceptance_criteria: vec![],
        plan: vec![],
        attachments: Vec::new(),
        budget: Default::default(),
        state: TaskState::Running,
        created_ms: now,
        updated_ms: now,
    })
    .unwrap();
    let changed = vec!["src/main.c".to_string()];
    let cancel = CancellationToken::new();
    // First call: derives once and enqueues the expensive check.
    let attempt_op = manager.try_next_op_id().unwrap().raw();
    let first = runtime
        .verify_integrated_root_attempt(&h, &root, &changed, &[], &cancel, attempt_op)
        .await
        .unwrap();
    assert_eq!(first.attempt_op, attempt_op);
    assert!(first.pending, "an enqueued job parks the run");
    assert!(first.verification.is_none());
    let task_id = h.task_id().unwrap();
    let attempt = h
        .verification_attempt(task_id.raw(), attempt_op)
        .unwrap()
        .unwrap();
    assert!(attempt
        .checks
        .iter()
        .any(|c| c.check_id == "make_test" && c.inline.is_none()));
    // A second call with the SAME op while the job is open is still
    // pending — nothing is re-derived or re-run inline.
    let second = runtime
        .verify_integrated_root_attempt(&h, &root, &changed, &[], &cancel, attempt_op)
        .await
        .unwrap();
    assert!(second.pending);
    assert!(second.verification.is_none());
    assert!(
        !root.join("root-marker.txt").exists(),
        "the root path never executes the queued job inline"
    );
    // The executor primitive resolves the exact attempt.
    assert_eq!(runtime.execute_open_verification_jobs(&h).await.unwrap(), 1);
    // Consumption rebuilds the verdict from the EXACT attempt rows.
    let settled = runtime
        .verify_integrated_root_attempt(&h, &root, &changed, &[], &cancel, attempt_op)
        .await
        .unwrap();
    assert!(!settled.pending, "the terminal attempt is consumed");
    let verification = settled.verification.expect("terminal verdict");
    assert_eq!(verification.status, VerificationStatus::Passed);
    assert!(
        verification.checks.iter().any(|c| c.check == "make_test"),
        "the settled job's execution row rides the verdict: {:?}",
        verification.checks
    );
    // A newer attempt makes the old op structurally superseded: the
    // consumer starts a FRESH attempt and never consumes the stale one.
    let newer_op = manager.try_next_op_id().unwrap().raw();
    h.begin_verification_attempt(
        task_id.raw(),
        h.task_revision(task_id).unwrap().raw(),
        newer_op,
        root.to_str().unwrap(),
        &changed,
        &[faktor_session::VerificationAttemptCheck {
            check_id: "make_test".into(),
            command: "make test".into(),
            inline: None,
        }],
        &[faktor_session::VerificationJobInput {
            check_id: "make_test".into(),
            kind: "test".into(),
            command: "make test".into(),
            program: "make".into(),
            args: vec!["test".into()],
            spec_json: "{}".into(),
            budget_ms: 10_000,
        }],
    )
    .unwrap();
    let superseded = runtime
        .verify_integrated_root_attempt(&h, &root, &changed, &[], &cancel, attempt_op)
        .await
        .unwrap();
    assert_ne!(
        superseded.attempt_op, attempt_op,
        "a superseded attempt can never settle the run again"
    );
    assert!(superseded.pending, "the fresh attempt's jobs are open");
    // A late result for the old attempt is typed-refused and never
    // resolves the fresh one.
    let late = h.resolve_verification_job(
        task_id.raw(),
        "make_test",
        attempt_op,
        faktor_session::VerificationJobState::Passed,
        None,
        Some("{}".into()),
    );
    assert!(late.is_err(), "old attempt rows are frozen");
}

#[tokio::test]
async fn partial_repository_inventory_never_derives_a_passing_suite() {
    // Adversarial (inventory completeness): a repository exceeding the
    // bounded inventory caps can never certify completion. The turn
    // classifies Unavailable BEFORE any review or derivation and mints
    // no verification record — a truncated view never yields a smaller
    // passing suite.
    let (manager, session, dir) = make_background_env("\t@true\n", None);
    let root = dir.path().join("ws");
    // A tree deeper than the inventory's default depth budget: discovery
    // is typed non-Complete (the correctness ceiling is a RESOURCE
    // budget now) and no smaller passing suite may be derived from the
    // bounded view.
    let mut deep = root.join("gen");
    for i in 0..=faktor_verify::MAX_INVENTORY_DEPTH {
        deep = deep.join(format!("d{i:02}"));
    }
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::write(deep.join("filler.rs"), b"// filler\n").unwrap();
    let (mut deps, _d) = deps_sharing_session(
        manager.clone(),
        Arc::new(scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/main.c", "content": "int main(void) {\n    return 0;\n}\n"}),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ])),
        vec![real_write_tool()],
    );
    deps.verification = crate::VerificationService::fake_ok();
    let runtime = AgentRuntime::new(deps).unwrap();
    let outcome = runtime
        .run_turn(session, "change main.c", &[])
        .await
        .unwrap();
    assert_eq!(
        outcome.completion,
        Some(CompletionGate::Unverified),
        "a partial inventory can never pass: {outcome:?}"
    );
    assert!(
        outcome.verification.is_empty(),
        "no checks were derived from the truncated view"
    );
    let h = manager.get_session(session).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    assert!(
        h.list_verification_records(task_id).unwrap().is_empty(),
        "a partial inventory mints no proof record"
    );
    assert_ne!(
        h.get_task(task_id).unwrap().unwrap().state,
        TaskState::VerifiedComplete
    );
}

#[tokio::test]
async fn two_required_jobs_one_fails_so_the_task_never_completes() {
    // Adversarial (audit P0-5/26 test c): an attempt with TWO required
    // background jobs where ONE fails gates FailedVerification — the
    // task NEVER reaches VerifiedComplete, from the settlement turn or
    // any later genuine end.
    if !std::process::Command::new("make")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        eprintln!("skipping two-required-jobs failure: no make on this host");
        return;
    }
    let (manager, session, _dir) = make_background_env(
        "\texit 7\n",      // `make test` RAN and failed
        Some("\t@true\n"), // `make check` passes
    );
    let (mut deps, _d) = deps_sharing_session(
        manager.clone(),
        Arc::new(scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                input: serde_json::json!({"path": "src/main.c", "content": "int main(void) {\n    int base = 40;\n    int step = 2;\n    printf(\"%d\\n\", (base + step) * 2);\n    return 0;\n}\n"}),
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
        Some(CompletionGate::VerificationPending),
        "both full checks are queued before any verdict exists"
    );
    let h = manager.get_session(session).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    assert_eq!(
        h.open_verification_jobs(task_id.raw()).unwrap().len(),
        2,
        "make_test AND make_check are both durable jobs"
    );
    // Settlement turn: make_test fails, make_check passes → the attempt
    // gates FailedVerification — never VerifiedComplete.
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
    let settled = runtime2.run_turn(session, "status?", &[]).await.unwrap();
    assert!(
        matches!(
            &settled.completion,
            Some(CompletionGate::FailedVerification { .. })
        ),
        "a failed required job gates FailedVerification: {:?}",
        settled.completion
    );
    assert_eq!(settled.acceptance, Some(faktor_verify::Acceptance::Fail));
    assert_eq!(
        settled.verification,
        vec![
            ("make_build".to_string(), true),
            ("make_test".to_string(), false),
            ("make_check".to_string(), true)
        ],
        "the full result set is rebuilt from the settled jobs"
    );
    let h2 = manager.get_session(session).unwrap().unwrap();
    let task = h2.get_task(task_id).unwrap().unwrap();
    assert_ne!(
        task.state,
        TaskState::VerifiedComplete,
        "a failed attempt never completes"
    );
    // No later genuine end can complete it: no open jobs remain, and a
    // text turn after the failure settles nothing.
    let (mut deps3, _d3) = deps_sharing_session(
        manager.clone(),
        Arc::new(scripted_provider(vec![
            ScriptedResponse::Text("again?".into()),
            ScriptedResponse::End,
        ])),
        vec![real_write_tool()],
    );
    deps3.verification = real_background_verifier();
    let runtime3 = AgentRuntime::new(deps3).unwrap();
    let later = runtime3.run_turn(session, "again?", &[]).await.unwrap();
    assert_ne!(later.completion, Some(CompletionGate::VerifiedComplete));
    let h3 = manager.get_session(session).unwrap().unwrap();
    let facts = h3.memory_facts().unwrap();
    assert!(
        !facts
            .iter()
            .any(|(k, key, v)| k == "task_state" && key == "state" && v == "verified_complete"),
        "the task never reaches VerifiedComplete: {facts:?}"
    );
    let task = h3.get_task(task_id).unwrap().unwrap();
    assert_ne!(task.state, TaskState::VerifiedComplete);
}

#[tokio::test]
async fn strict_criteria_divergence_refuses_claims_then_converges_deterministically() {
    // Audit 92: Strict quality verifies the durable criteria fact
    // against the typed task row at every mutating genuine end. Drive
    // start heals FACT-from-ROW (the typed row is the system of
    // record); a hostile ROW tamper therefore survives to the finish
    // boundary of the NEXT turn: the finish re-derives the row to the
    // canonical criteria while the fact (healed to the tampered row)
    // still disagrees — the completion claim is refused with a machine
    // criteria_inconsistent reason. A failing check keeps its
    // FailedVerification kind and CARRIES the divergence (never
    // silently overwritten by it). The refusal is deterministic: the
    // next drive heals the fact from the canonical row and a clean turn
    // re-verifies VerifiedComplete — identical to an untampered run,
    // also across a restart.
    //
    // Audit P0-7 (this migration): the hostile tamper can no longer
    // target a VerifiedComplete row — completion is terminal and its
    // row is frozen (update_task refuses TerminalTask, asserted at the
    // end). The adversarial window therefore moves BEFORE completion:
    // the row is tampered while it carries the retryable machine state
    // (NeedsVerification after a failed verification), where content
    // edits are legal; the divergence semantics are byte-identical.
    let (manager, session, dir) = verified_shared_env();
    let ok = fake_ok();
    let ok2 = ok.clone();
    let failing = fake(|_cmd: &str| Err("type error".to_string()));
    // Turn 1 FAILS its check: the row lands NeedsVerification (the
    // retryable machine state — mutable for the hostile tamper), the
    // criteria fact is seeded from the SAME first derivation.
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
        failing.clone(),
        0.65,
    );
    let runtime1 = AgentRuntime::new(deps1).unwrap();
    let o1 = runtime1
        .run_turn(session, "gating task", &[])
        .await
        .unwrap();
    drop(runtime1);
    assert!(matches!(
        o1.completion,
        Some(CompletionGate::FailedVerification { .. })
    ));
    let h = manager.get_session(session).unwrap().unwrap();
    let clean_criteria = criteria_fact(&h).expect("criteria fact seeded");
    let row1 = &h.list_tasks().unwrap()[0];
    assert_eq!(
        row1.state,
        TaskState::NeedsVerification,
        "the failed turn lands the retryable machine state (tamperable)"
    );
    // Hostile tamper of the typed task row's acceptance criteria while
    // the row is mutable (NeedsVerification). A tamper against the
    // frozen terminal row would be refused by the machine itself —
    // asserted at the end of this test.
    let task_id = row1.task_id;
    let tamper = |h: &faktor_session::SessionHandle| {
        h.update_task(
            task_id,
            TaskPatch {
                acceptance_criteria: Some(vec!["tampered criteria".into()]),
                ..Default::default()
            },
        )
        .unwrap();
    };
    tamper(&h);
    drop(h);
    // First finish after the tamper: the claim must be refused (the
    // divergence cannot be silently verified over). The row keeps the
    // machine state (NeedsVerification has no legal edge to Blocked).
    let (deps2, _d2) = verified_turn_deps(
        &manager,
        vec![
            ScriptedResponse::ToolCall {
                id: "c2".into(),
                name: "write_file".into(),
                input: serde_json::json!({
                    "path": "src/b.rs",
                    "content": "pub fn b() -> u32 {\n    let base: u32 = 41;\n    let step: u32 = 1;\n    base.saturating_add(step).saturating_mul(2)\n}\n",
                }),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
        ok2,
        0.65,
    );
    let runtime2 = AgentRuntime::new(deps2).unwrap();
    let o2 = runtime2.run_turn(session, "write b", &[]).await.unwrap();
    drop(runtime2);
    match &o2.completion {
        Some(CompletionGate::BlockedVerification { reasons }) => {
            assert!(
                reasons
                    .iter()
                    .any(|r| r.code == ReasonCode::CriteriaInconsistent
                        && r.detail.contains("tampered criteria")),
                "divergence must refuse the claim: {reasons:?}"
            );
        }
        other => panic!("criteria divergence must block VerifiedComplete, got {other:?}"),
    }
    let h2 = manager.get_session(session).unwrap().unwrap();
    assert_eq!(
            h2.list_tasks().unwrap()[0].state,
            TaskState::NeedsVerification,
            "a NeedsVerification row has no legal edge to Blocked: the machine keeps the claim awaiting re-verification"
        );
    drop(h2);
    // The divergence healed at that drive boundary: a FAILING turn now
    // carries only its own verdict (kind preserved, no stale reason).
    let (deps3, _d3) = verified_turn_deps(
        &manager,
        vec![
            ScriptedResponse::ToolCall {
                id: "c3".into(),
                name: "write_file".into(),
                input: serde_json::json!({
                    "path": "src/c.rs",
                    "content": "pub fn c() -> u32 {\n    let base: u32 = 2;\n    let step: u32 = 41;\n    base.saturating_add(base).saturating_mul(step)\n}\n",
                }),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
        failing.clone(),
        0.65,
    );
    let runtime3 = AgentRuntime::new(deps3).unwrap();
    let o3 = runtime3.run_turn(session, "write c", &[]).await.unwrap();
    drop(runtime3);
    match &o3.completion {
        Some(CompletionGate::FailedVerification { reasons }) => {
            assert!(
                reasons.iter().any(|r| r.code == ReasonCode::CheckFailed),
                "the check verdict survives: {reasons:?}"
            );
            assert!(
                !reasons
                    .iter()
                    .any(|r| r.code == ReasonCode::CriteriaInconsistent),
                "healed rows carry no stale divergence: {reasons:?}"
            );
        }
        other => panic!("failed gate must keep its kind, got {other:?}"),
    }
    // A fresh tamper for the restart leg: the refusal must be
    // deterministic across a restart (same durable inputs).
    let h = manager.get_session(session).unwrap().unwrap();
    tamper(&h);
    drop(h);
    drop(manager);
    let manager4 =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let (deps4, _d4) = verified_turn_deps(
        &manager4,
        vec![
            ScriptedResponse::ToolCall {
                id: "c4".into(),
                name: "write_file".into(),
                input: serde_json::json!({
                    "path": "src/d.rs",
                    "content": "pub fn d() -> u32 {\n    let base: u32 = 7;\n    let step: u32 = 41;\n    base.saturating_add(base).saturating_mul(step)\n}\n",
                }),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
        ok.clone(),
        0.65,
    );
    let runtime4 = AgentRuntime::new(deps4).unwrap();
    let o4 = runtime4.run_turn(session, "write d", &[]).await.unwrap();
    drop(runtime4);
    match &o4.completion {
        Some(CompletionGate::BlockedVerification { reasons }) => {
            assert!(
                reasons
                    .iter()
                    .any(|r| r.code == ReasonCode::CriteriaInconsistent
                        && r.detail.contains("tampered criteria")),
                "restart must refuse identically: {reasons:?}"
            );
        }
        other => panic!("restart must refuse identically, got {other:?}"),
    }
    // Deterministic convergence: the healed re-derivation now verifies
    // cleanly — byte-identical to an untampered run.
    let (deps5, _d5) = verified_turn_deps(
        &manager4,
        vec![
            ScriptedResponse::ToolCall {
                id: "c5".into(),
                name: "write_file".into(),
                input: serde_json::json!({
                    "path": "src/e.rs",
                    "content": "pub fn e() -> u32 {\n    let base: u32 = 11;\n    let step: u32 = 41;\n    base.saturating_add(base).saturating_mul(step)\n}\n",
                }),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
        ok,
        0.65,
    );
    let runtime5 = AgentRuntime::new(deps5).unwrap();
    let o5 = runtime5.run_turn(session, "write e", &[]).await.unwrap();
    drop(runtime5);
    assert_eq!(
        o5.completion,
        Some(CompletionGate::VerifiedComplete),
        "an untampered re-derivation converges to the same gate as an untampered run"
    );
    let h5 = manager4.get_session(session).unwrap().unwrap();
    let fact5 = criteria_fact(&h5).expect("criteria fact present");
    assert_eq!(
        fact5, clean_criteria,
        "the once-only criteria row converged to its canonical text"
    );
    let row5 = &h5.list_tasks().unwrap()[0];
    assert_eq!(
        criteria_canonical_text(&row5.acceptance_criteria),
        clean_criteria,
        "typed row and criteria fact agree after convergence"
    );
    assert_eq!(row5.state, TaskState::VerifiedComplete);
    // The machine backstop: the now-terminal row refuses ANY further
    // content mutation — the hostile-write surface that existed on a
    // mutable VerifiedComplete row is structurally gone.
    assert!(
        matches!(
            h5.update_task(
                row5.task_id,
                TaskPatch {
                    acceptance_criteria: Some(vec!["late tamper".into()]),
                    ..Default::default()
                }
            ),
            Err(faktor_session::TaskError::TerminalTask { .. })
        ),
        "a terminal VerifiedComplete row must refuse post-completion tampering"
    );
}

#[tokio::test]
async fn provider_profile_switch_keeps_the_task_row() {
    // (d) provider switch: a new runtime over the SAME store/session
    // resolves a DIFFERENT provider profile (different capabilities
    // object and request shape) and keeps driving the same durable task.
    let (manager, session, _dir) = verified_shared_env();
    let (deps1, _d1) = verified_turn_deps(
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
    let runtime1 = AgentRuntime::new(deps1).unwrap();
    let o1 = runtime1
        .run_turn(session, "write src/a.rs", &[])
        .await
        .unwrap();
    assert_eq!(o1.completion, Some(CompletionGate::VerifiedComplete));
    let before = {
        let h = manager.get_session(session).unwrap().unwrap();
        task_snapshot(&h)
    };
    // The "switched" provider: same id, a different capability profile
    // (tools disabled, no context override) — the runtime must rebuild
    // against it without touching any durable row.
    let switched = FakeProvider::with_script(
        "fake",
        ModelCapabilities {
            tools: false,
            context: 64_000,
            ..Default::default()
        },
        vec![
            ScriptedResponse::Text("switched provider".into()),
            ScriptedResponse::End,
        ],
    );
    let (mut deps2, _d2) = deps_sharing_session(manager.clone(), Arc::new(switched), vec![]);
    deps2.verification = fake_ok();
    let runtime2 = AgentRuntime::new(deps2).unwrap();
    let o2 = runtime2
        .run_turn(session, "continue under the switched profile", &[])
        .await
        .unwrap();
    assert_eq!(o2.final_state, AgentState::ReadyForNextTurn);
    let h = manager.get_session(session).unwrap().unwrap();
    assert_eq!(
        task_snapshot(&h),
        before,
        "provider switch must not move the task row"
    );
    assert_eq!(h.list_tasks().unwrap().len(), 1);
}

#[tokio::test]
async fn failing_test_command_lands_in_tests_failed() {
    let cmd = Tool {
        name: "run_command".into(),
        description: "t".into(),
        input_schema: serde_json::json!({"type": "object"}),
        resource_class: faktor_core::resource::ResourceClass::Cpu,
        capability: None,
        recovery_hint: RecoveryHint::UnknownEffect,
        path_args: vec![],
        execute: Arc::new(|_ctx, args| {
            Box::pin(async move {
                let _ = args;
                Ok(ToolOutcome {
                    text: "FAILED".into(),
                    exit_code: Some(1),
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
                input: serde_json::json!({"command": "pytest -q"}),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ]),
        vec![cmd],
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    runtime
        .run_turn(session, "run the tests", &[])
        .await
        .unwrap();
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let ledger: faktor_context::ledger::TaskLedger =
        serde_json::from_value(handle.get_task_ledger().unwrap().unwrap()).unwrap();
    assert!(
        ledger.tests_failed.iter().any(|t| t.contains("pytest -q")),
        "failing test command must be recorded: {:?}",
        ledger.tests_failed
    );
}

#[tokio::test]
async fn same_failing_call_across_turns_trips_durable_loop_detection() {
    // Spec §28: a LoopDetector that dies with the turn cannot see "the
    // same command failed for 40 turns". The durable signal must trip on
    // the THIRD consecutive all-failing turn of ONE session.
    let boom = Tool {
        name: "run_command".into(),
        description: "t".into(),
        input_schema: serde_json::json!({"type": "object"}),
        resource_class: faktor_core::resource::ResourceClass::Cpu,
        capability: None,
        recovery_hint: RecoveryHint::UnknownEffect,
        path_args: vec![],
        execute: Arc::new(|_ctx, args| {
            Box::pin(async move {
                let _ = args;
                Ok(ToolOutcome {
                    text: "boom".into(),
                    exit_code: Some(2),
                    ..Default::default()
                })
            })
        }),
    };
    let (seed_deps, _dir0) = deps(
        scripted_provider(vec![ScriptedResponse::End]),
        vec![boom.clone()],
    );
    let (manager, session) = shared_session(&seed_deps);
    let mut outcomes = Vec::new();
    for i in 0..3 {
        let (turn_deps, _dir) = deps_sharing_session(
            manager.clone(),
            Arc::new(scripted_provider(vec![
                ScriptedResponse::ToolCall {
                    id: format!("c_{i}"),
                    name: "run_command".into(),
                    input: serde_json::json!({"command": "cargo check -p faktor-core"}),
                },
                ScriptedResponse::End,
            ])),
            vec![boom.clone()],
        );
        let runtime = AgentRuntime::new(turn_deps).unwrap();
        let outcome = runtime
            .run_turn(session, &format!("fix it attempt {i}"), &[])
            .await
            .unwrap();
        outcomes.push(outcome);
    }
    assert!(!outcomes[0].loop_stopped && !outcomes[1].loop_stopped);
    assert_eq!(outcomes[0].final_state, AgentState::ReadyForNextTurn);
    assert_eq!(outcomes[1].final_state, AgentState::ReadyForNextTurn);
    assert!(
        outcomes[2].loop_stopped,
        "the third identical all-failing turn must trip"
    );
    assert_eq!(outcomes[2].final_state, AgentState::FailedRecoverable);
}

#[tokio::test]
async fn repo_map_and_project_rules_reach_the_wire() {
    // Spec §8/§26: repository knowledge must ride the context. The
    // request the model receives carries the bounded file map and the
    // workspace AGENTS.md rules — they were silently empty before.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join("src").join("a.rs"),
        "fn x() {}
",
    )
    .unwrap();
    std::fs::write(root.join("AGENTS.md"), "Rules: no unsafe in src\n").unwrap();
    std::fs::create_dir_all(root.join("target")).unwrap();
    std::fs::write(root.join("target").join("junk.rs"), "junk").unwrap();
    let provider = scripted_provider(vec![
        ScriptedResponse::Text("ok".into()),
        ScriptedResponse::End,
    ]);
    let fake = Arc::new(provider.clone());
    let (mut adeps, _adir) = deps_with(Arc::new(provider), vec![]);
    let ws = adeps
        .session
        .create_workspace(root.to_str().unwrap())
        .unwrap();
    let sid = adeps
        .session
        .create_session(ws, "repo test", "fake", "m")
        .unwrap()
        .id();
    let seen = Arc::new(std::sync::Mutex::new(None::<String>));
    let hook = {
        let seen = seen.clone();
        move |_n: usize, req: &GenericAgentRequest| -> Result<(), String> {
            *seen.lock().unwrap() = Some(req.system.clone());
            Ok(())
        }
    };
    // Wrap the provider with a request inspector.
    let inspected = Arc::new(InspectingProvider::new(fake, hook));
    let mut registry = ProviderRegistry::new();
    registry.try_register(inspected).unwrap();
    adeps.providers = Arc::new(registry);
    let runtime = AgentRuntime::new(adeps).unwrap();
    runtime
        .run_turn(sid, "inspect the repo", &[])
        .await
        .unwrap();
    let system = seen.lock().unwrap().clone().expect("request sent");
    assert!(
        system.contains("## Repository map") && system.contains("src/a.rs"),
        "repo map must ride the wire: {system}"
    );
    assert!(
        !system.contains("junk.rs"),
        "skipped dirs never appear in the repo map"
    );
    assert!(
        system.contains("## Project rules") && system.contains("no unsafe"),
        "AGENTS.md rules must ride the wire: {system}"
    );
}

#[tokio::test]
async fn workspace_write_content_mismatch_fails_loudly() {
    // Requirement 3b: when the file holds DIFFERENT bytes (e.g. the
    // JSON-quoted encoding the old buggy code hashed), verification
    // FAILS loudly — never silently "applied".
    let dir = fresh_store_dir();
    let ws_id: WorkspaceId;
    let session: SessionId;
    let tool_op: OpId;
    let json_quoted: Vec<u8>;
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
        // The old (wrong) runtime hashed serde_json::to_vec("hello") —
        // the bytes `"hello"` WITH quotes. Simulate a crash where only
        // THAT content landed: verification must reject it against the
        // recorded postcondition of the raw bytes.
        json_quoted = serde_json::to_vec(&serde_json::json!("hello")).unwrap();
        std::fs::write(root.join("a.txt"), &json_quoted).unwrap();
        let expected = FileHash::from(blake3::hash(b"hello").into());
        let pc = serde_json::to_value(FilePostcondition {
            workspace_id: ws_id,
            worktree_id: WorktreeId::new(1),
            relative_path: "a.txt".into(),
            expected_hash: expected,
        })
        .unwrap();
        handle.record_tool_postcondition(tool_op, &pc).unwrap();
    }
    let (deps2, _keep2) = reopen_runtime(&dir, Arc::new(scripted_provider(vec![])), vec![]);
    let runtime2 = AgentRuntime::new(deps2).unwrap();
    let reports = runtime2.recover().unwrap();
    let report = reports.iter().find(|r| r.session_id == session).unwrap();
    assert_eq!(
        report.crashed_ops[0].status, "failed",
        "mismatching bytes must fail loudly"
    );
    assert_eq!(
        report.crashed_ops[0].effect,
        EffectStatus::Failed,
        "never silently applied"
    );
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
        json_quoted,
        "recovery never rewrites or re-runs the write"
    );
}

#[tokio::test]
async fn non_usd_reported_cost_is_refused_as_an_authoritative_override() {
    // Audit Phase-1 item C: reported costs are marked with their
    // currency; ONLY USD-compatible values may override the
    // route-snapshot estimate. A EUR report must be refused — the
    // settlement stays at the exact category estimate and the EUR
    // micro number never lands on the row.
    let provider: Arc<dyn faktor_provider::Provider> = Arc::new(CacheSplitProvider {
        reported: Some(ReportedCost {
            micro_usd: 9_999_999,
            currency: ReportedCurrency::Other { code: "eur".into() },
            source: faktor_provider::ReportedCostSource::ProviderUsage,
            request_id: None,
        }),
    });
    let (runtime, session, ledger) = routed_settlement_runtime(provider).await;
    let outcome = runtime.run_turn(session, "hi", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
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
        "the route-snapshot estimate stands; the non-USD report never overrides"
    );
    let rows = ledger.reservations_of(session, task_id, 10).unwrap();
    assert_eq!(rows[0].provider_reported_micro, None);
    assert_eq!(rows[0].provider_cost_micro, Some(750));
    drop(runtime);
}

/// (e) A panicking or stuck evidence provider cannot block the turn
/// (audit 14/26, async + bounded): when the IndexService cannot be
/// hosted (its data root is blocked), the drive degrades to
/// `deps.evidence` on EVERY turn — the provider under attack is awaited
/// off the turn thread under [`LEGACY_EVIDENCE_MAX_WAIT`]. Turn 1 feeds
/// a provider that PARKS FOREVER on its first poll (a completed package
/// is impossible, so the turn can only return because the ~2 s wall
/// budget fired — the structural proof, immune to machine load); turn 2
/// feeds a provider that PANICS on the first poll (the panic happens on
/// the detached polling thread and degrades to an empty package instead
/// of aborting the turn). Both turns still reach the model (a request
/// is captured, with no retrieved-evidence section) and complete within
/// a hard sanity bound that no working budget could ever need — the
/// bound is an environmental margin (this harness's real-root turns
/// cost seconds, more under full-suite parallel load), not a timing
/// assertion; the `started` flags carry the load-independent proof.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_or_panicking_legacy_provider_cannot_block_the_turn() {
    let captured: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let hook = {
        let captured = captured.clone();
        move |_n: usize, req: &GenericAgentRequest| -> Result<(), String> {
            captured.lock().unwrap().push(req.system.clone());
            Ok(())
        }
    };
    let scripted = scripted_provider(vec![
        ScriptedResponse::Text("bounded".into()),
        ScriptedResponse::End,
        ScriptedResponse::Text("bounded".into()),
        ScriptedResponse::End,
    ]);
    let inspected: Arc<dyn faktor_provider::Provider> =
        Arc::new(InspectingProvider::new(Arc::new(scripted), hook));
    let (mut adeps, dir) = deps_with(inspected.clone(), vec![]);
    // Block index hosting: the runtime derives its index data root from
    // the session store root, so a FILE named `index_data` there makes
    // IndexService::open fail — the drive must degrade to `deps.evidence`
    // (the provider under attack) on every turn of the test.
    std::fs::write(
        dir.path().join("store").join("index_data"),
        b"not a directory",
    )
    .unwrap();
    let manager = adeps.session.clone();
    let ws_root = dir.path().join("repo");
    std::fs::create_dir_all(ws_root.join("src")).unwrap();
    std::fs::write(
        ws_root.join("src").join("lib.rs"),
        "pub fn balance_account() -> i64 { 42 }\n",
    )
    .unwrap();
    let ws = manager.create_workspace(ws_root.to_str().unwrap()).unwrap();
    let sid = manager
        .create_session(ws, "ev-deadline", "fake", "m")
        .unwrap()
        .id();
    // Turn 1: the forever-parked provider must be CUT at the ~2 s wall
    // budget. Its future cannot complete by construction, so the turn
    // returning at all — with the provider's `started` flag set — is
    // the load-independent proof that the budget fired: a runtime that
    // awaited the provider to completion could never return.
    let parked_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    adeps.evidence = Arc::new(ParkedEvidence {
        started: parked_started.clone(),
    });
    let runtime = AgentRuntime::new(adeps).unwrap();
    let started = std::time::Instant::now();
    runtime.run_turn(sid, "inspect", &[]).await.unwrap();
    let slow_elapsed = started.elapsed();
    assert!(
        slow_elapsed < Duration::from_secs(240),
        "a stuck evidence provider must be cut at its wall budget; the turn took {slow_elapsed:?}"
    );
    assert!(
        parked_started.load(std::sync::atomic::Ordering::SeqCst),
        "the hostile provider was never even polled: the degrade ladder under attack did not run"
    );
    // Turn 2 (fresh runtime over the SAME manager/store): the
    // poll-time panicker is swallowed on the detached polling thread and
    // degrades to empty evidence — the panic never aborts the turn.
    drop(runtime);
    let panic_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (mut adeps2, _dir2) = deps_sharing_session(manager, inspected, vec![]);
    adeps2.evidence = Arc::new(PanickingEvidence {
        started: panic_started.clone(),
    });
    let runtime = AgentRuntime::new(adeps2).unwrap();
    let started = std::time::Instant::now();
    runtime.run_turn(sid, "inspect", &[]).await.unwrap();
    let panic_elapsed = started.elapsed();
    assert!(
            panic_elapsed < Duration::from_secs(240),
            "a panicking evidence provider must degrade, not stall or abort; the turn took {panic_elapsed:?}"
        );
    assert!(
        panic_started.load(std::sync::atomic::Ordering::SeqCst),
        "the panicking provider was never even polled: the degrade ladder under attack did not run"
    );
    // Both turns reached the model with EMPTY evidence (the degrade
    // ladder): exactly two requests, neither carrying a
    // retrieved-evidence section from either hostile provider.
    let systems = captured.lock().unwrap();
    assert_eq!(systems.len(), 2, "both turns must reach the model");
    for system in systems.iter() {
        assert!(
            !system.contains("## Retrieved evidence"),
            "hostile evidence must degrade to an empty package, got: {system}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_poll_panic_is_durable_provider_panicked() {
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (deps, _dir, sid, ws, task_id) = poll_drive_deps(
        Arc::new(scripted_provider(one_text_turn())),
        Arc::new(PanickingEvidence {
            started: started.clone(),
        }),
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let outcome = runtime.run_turn(sid, "inspect", &[]).await.unwrap();
    assert!(started.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    match decode_evidence_poll_status(outcome.evidence_poll.as_deref().unwrap()) {
        Some(crate::EvidencePollStatus::ProviderPanicked { message }) => {
            assert!(message.contains("adversarial"), "{message}");
        }
        other => panic!("expected ProviderPanicked, got {other:?}"),
    }
    let durable = archived_poll_statuses(runtime.evidence_authority(), sid, ws, task_id);
    assert!(
        matches!(
            durable.as_slice(),
            [crate::EvidencePollStatus::ProviderPanicked { .. }]
        ),
        "{durable:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_poll_timeout_is_durable_timed_out() {
    let started = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (deps, _dir, sid, ws, task_id) = poll_drive_deps(
        Arc::new(scripted_provider(one_text_turn())),
        Arc::new(ParkedEvidence {
            started: started.clone(),
        }),
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let outcome = runtime.run_turn(sid, "inspect", &[]).await.unwrap();
    assert!(started.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    let expected = crate::EvidencePollStatus::TimedOut {
        budget_ms: LEGACY_EVIDENCE_MAX_WAIT.as_millis() as u64,
    };
    assert_eq!(
        decode_evidence_poll_status(outcome.evidence_poll.as_deref().unwrap()),
        Some(expected.clone())
    );
    let durable = archived_poll_statuses(runtime.evidence_authority(), sid, ws, task_id);
    assert_eq!(durable, vec![expected]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_poll_served_keeps_ranking_and_is_durable_served() {
    let captured: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let hook = {
        let captured = captured.clone();
        move |_n: usize, req: &GenericAgentRequest| -> Result<(), String> {
            captured.lock().unwrap().push(req.system.clone());
            Ok(())
        }
    };
    let inspected: Arc<dyn faktor_provider::Provider> = Arc::new(InspectingProvider::new(
        Arc::new(scripted_provider(one_text_turn())),
        hook,
    ));
    let (deps, _dir, sid, ws, task_id) = poll_drive_deps(
        inspected,
        Arc::new(ServingEvidence {
            package: vec![
                Evidence {
                    path: "svc/low".into(),
                    snippet: "low hit".into(),
                    score: 0.2,
                },
                Evidence {
                    path: "svc/high".into(),
                    snippet: "high hit".into(),
                    score: 0.9,
                },
            ],
        }),
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let outcome = runtime.run_turn(sid, "inspect", &[]).await.unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    // The served package reaches the model with its ranking intact
    // (highest score first) — the typed poll changed no ordering.
    let systems = captured.lock().unwrap();
    assert_eq!(systems.len(), 1, "one wire request for the turn");
    let system = &systems[0];
    let high = system
        .find("### svc/high")
        .unwrap_or_else(|| panic!("served evidence missing: {system}"));
    let low = system
        .find("### svc/low")
        .unwrap_or_else(|| panic!("served evidence missing: {system}"));
    assert!(high < low, "highest score must render first: {system}");
    // The durable status is Served, never degraded.
    assert_eq!(
        decode_evidence_poll_status(outcome.evidence_poll.as_deref().unwrap()),
        Some(crate::EvidencePollStatus::Served)
    );
    let durable = archived_poll_statuses(runtime.evidence_authority(), sid, ws, task_id);
    assert_eq!(durable, vec![crate::EvidencePollStatus::Served]);
    assert!(!durable[0].is_degraded());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_poll_hostile_status_fields_are_bounded_durably() {
    let (deps, _dir, sid, ws, task_id) = poll_drive_deps(
        Arc::new(scripted_provider(one_text_turn())),
        Arc::new(FailingEvidence {
            error: Error::new(
                ErrorKind::Provider {
                    code: "C".repeat(64 * 1024),
                    retryable: false,
                },
                "m".repeat(64 * 1024),
            ),
        }),
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let outcome = runtime.run_turn(sid, "inspect", &[]).await.unwrap();
    let diagnostic = outcome.evidence_poll.as_deref().unwrap();
    assert!(
        diagnostic.len() <= EVIDENCE_POLL_DURABLE_MAX_BYTES,
        "the turn diagnostic must be bounded, got {} bytes",
        diagnostic.len()
    );
    let ctx = faktor_context::compiler::EvidenceAccessContext::new(
        sid.raw(),
        ws.raw(),
        Some(task_id.raw()),
    );
    let bodies: Vec<String> = runtime
        .evidence_authority()
        .list_scoped_envelopes(&ctx, 64)
        .unwrap()
        .into_iter()
        .filter(|env| {
            env.kind == EvidenceKind::GenericText
                && env
                    .source_revision
                    .as_deref()
                    .is_some_and(|revision| revision.starts_with("evidence-poll:"))
        })
        .map(|env| env.compact.body)
        .collect();
    assert_eq!(bodies.len(), 1);
    assert!(
        bodies[0].len() <= EVIDENCE_POLL_DURABLE_MAX_BYTES,
        "the durable row must be bounded, got {} bytes",
        bodies[0].len()
    );
    match decode_evidence_poll_status(&bodies[0]) {
        Some(crate::EvidencePollStatus::RetrievalFailed {
            code,
            retryable,
            message,
        }) => {
            assert!(!retryable, "retryability survives truncation");
            assert!(code.len() <= EVIDENCE_POLL_DURABLE_CODE_MAX_BYTES);
            assert!(message.len() <= EVIDENCE_POLL_DURABLE_MESSAGE_MAX_BYTES);
        }
        other => panic!("expected a bounded RetrievalFailed, got {other:?}"),
    }
}

/// The P1 circuit-open refusal is a typed, durable fact: its
/// abandoned/cap counters survive the canonical encoding (and are
/// required to decode), so a reopened store can never turn an open
/// circuit into "no evidence".
#[test]
fn durable_circuit_open_encoding_round_trips_with_counters() {
    let status = crate::EvidencePollStatus::CircuitOpen {
        abandoned: 4,
        cap: 4,
        message: "circuit open".into(),
    };
    let encoded = encode_evidence_poll_status(&status);
    assert_eq!(decode_evidence_poll_status(&encoded), Some(status.clone()));
    assert!(status.is_degraded());
    assert_ne!(
        encoded,
        encode_evidence_poll_status(&crate::EvidencePollStatus::NoEvidence)
    );
    // A hostile payload missing the typed counters is not decodable
    // (never a guessed circuit state).
    assert!(decode_evidence_poll_status(
        "{\"schema\":1,\"status\":\"circuit_open\",\"degraded\":true,\"message\":\"x\"}"
    )
    .is_none());
}

#[test]
fn durable_poll_encoding_round_trips_and_bounds_multibyte_fields() {
    let status = crate::EvidencePollStatus::RetrievalFailed {
        code: "K".repeat(EVIDENCE_POLL_DURABLE_CODE_MAX_BYTES + 16),
        retryable: true,
        message: "\u{e9}".repeat(EVIDENCE_POLL_DURABLE_MESSAGE_MAX_BYTES),
    };
    let encoded = encode_evidence_poll_status(&status);
    assert!(encoded.len() <= EVIDENCE_POLL_DURABLE_MAX_BYTES);
    match decode_evidence_poll_status(&encoded) {
        Some(crate::EvidencePollStatus::RetrievalFailed {
            code,
            retryable,
            message,
        }) => {
            assert!(retryable);
            assert_eq!(code.len(), EVIDENCE_POLL_DURABLE_CODE_MAX_BYTES);
            assert!(message.len() <= EVIDENCE_POLL_DURABLE_MESSAGE_MAX_BYTES);
            assert!(message.is_char_boundary(message.len()));
        }
        other => panic!("expected a bounded RetrievalFailed, got {other:?}"),
    }
    // Hostile bytes never panic and never become a typed status.
    assert!(decode_evidence_poll_status("").is_none());
    assert!(decode_evidence_poll_status("not json").is_none());
    assert!(decode_evidence_poll_status(
        "{\"schema\":999,\"status\":\"served\",\"degraded\":false}"
    )
    .is_none());
    assert!(
        decode_evidence_poll_status("{\"schema\":1,\"status\":\"made_up\",\"degraded\":true}")
            .is_none()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_poll_status_survives_store_reopen() {
    let (deps, dir, sid, ws, task_id) = poll_drive_deps(
        Arc::new(scripted_provider(one_text_turn())),
        Arc::new(FailingEvidence {
            error: Error::new(
                ErrorKind::Provider {
                    code: "E_REOPEN".into(),
                    retryable: false,
                },
                "provider died mid-retrieval",
            ),
        }),
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let outcome = runtime.run_turn(sid, "inspect", &[]).await.unwrap();
    let canonical = outcome
        .evidence_poll
        .clone()
        .expect("poll diagnostics on the polled turn");
    drop(runtime);
    // A fresh manager over the SAME store directory: the archived row is
    // the durable truth, not an in-memory artifact.
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let authority =
        DurableEvidenceAuthority::for_store(manager.store(), EVIDENCE_BACKING_CAP_BYTES);
    let durable = archived_poll_statuses(&authority, sid, ws, task_id);
    assert_eq!(
        durable,
        vec![crate::EvidencePollStatus::RetrievalFailed {
            code: "E_REOPEN".into(),
            retryable: false,
            message: "provider died mid-retrieval".into(),
        }]
    );
    let ctx = faktor_context::compiler::EvidenceAccessContext::new(
        sid.raw(),
        ws.raw(),
        Some(task_id.raw()),
    );
    let bodies: Vec<String> = authority
        .list_scoped_envelopes(&ctx, 64)
        .unwrap()
        .into_iter()
        .filter(|env| {
            env.kind == EvidenceKind::GenericText
                && env
                    .source_revision
                    .as_deref()
                    .is_some_and(|revision| revision.starts_with("evidence-poll:"))
        })
        .map(|env| env.compact.body)
        .collect();
    assert_eq!(bodies.len(), 1);
    assert_eq!(
        bodies[0], canonical,
        "the reopened row must be byte-identical to the turn diagnostic"
    );
}

/// Audit 102 end-to-end: a scripted provider reports cache_read 0/N/0/0
/// across four turns with the cacheable prefix REWRITTEN at turn 3; the
/// route input (the durable `TurnPrefix` history) must carry the
/// observed cache state and the cost estimate must respond to it —
/// production (no `CacheState`) charges the churn premium at turn 4's
/// consult, and a consult fed the OBSERVED cache state discounts turn
/// 3's estimate by exactly the reported N, zeroes that discount under
/// the turn-4 churn, and adds the churn premium.
#[tokio::test]
async fn prefix_cache_economics_flow_through_agentruntime_turns() {
    const N_CACHE: u64 = 4_321;
    let dir = fresh_store_dir();
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let ws = manager.create_workspace("/w").unwrap();
    let session = manager
        .create_session(ws, "cache-e2e", "fake", "m")
        .unwrap()
        .id();
    let provider = CacheReportingProvider::new(vec![0, N_CACHE, 0, 0]);
    let service = Arc::new(faktor_router::RouterService::new(vec![
        cache_route_candidate(),
    ]));
    let inner = crate::EconomicRoutingPolicy::new(service.clone(), RoutingMode::Economy);
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let policy = Arc::new(ObservingCachePolicy {
        inner,
        service,
        seen: seen.clone(),
    });

    // Turns 1-2 share the blue prefix; turn 3 REWRITES the cacheable
    // head (same-length different bytes) and turn 4 keeps the new one.
    let (mut deps1, _keep1) = deps_sharing_session(manager.clone(), provider.clone(), vec![]);
    deps1.instructions = "You are a blue agent.".into();
    deps1.routing = policy.clone();
    let runtime1 = AgentRuntime::new(deps1).unwrap();
    for prompt in ["first", "second"] {
        let outcome = runtime1.run_turn(session, prompt, &[]).await.unwrap();
        assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    }
    drop(runtime1);

    let (mut deps2, _keep2) = deps_sharing_session(manager.clone(), provider.clone(), vec![]);
    deps2.instructions = "You are a gold agent.".into();
    deps2.routing = policy.clone();
    let runtime2 = AgentRuntime::new(deps2).unwrap();
    for prompt in ["third", "fourth"] {
        let outcome = runtime2.run_turn(session, prompt, &[]).await.unwrap();
        assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    }
    drop(runtime2);

    // Every completed call landed a durable prefix row with the observed
    // cache reads.
    let rows = manager.store().provider_call_prefix_rows(session).unwrap();
    assert_eq!(rows.len(), 4, "one prefix row per completed call");

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 4, "one route consult per turn");
    for (i, consult) in seen.iter().enumerate() {
        assert_eq!(
            consult.history.len(),
            i,
            "turn {} must see exactly the durable observations of turns < {i}",
            i + 1
        );
    }
    // The route INPUT receives the observed cache state: turn 2's
    // consult sees turn 1's 0; turn 3's sees turn 2's N; turn 4's sees
    // turns 2 (N) and 3 (0, the rewritten prefix).
    assert_eq!(
        seen[1].history[0]
            .segments
            .as_ref()
            .expect("v19 segments in the route input")
            .cache_read_tokens,
        0
    );
    assert_eq!(
        seen[2].history[1]
            .segments
            .as_ref()
            .expect("v19 segments in the route input")
            .cache_read_tokens,
        N_CACHE
    );
    assert_eq!(
        seen[3].history[1]
            .segments
            .as_ref()
            .expect("v19 segments in the route input")
            .cache_read_tokens,
        N_CACHE
    );
    assert_eq!(
        seen[3].history[2]
            .segments
            .as_ref()
            .expect("v19 segments in the route input")
            .cache_read_tokens,
        0
    );

    // The measured stability of the route input: turns 1-2 are stable;
    // turn 3 rewrote the cacheable head (0.0).
    let stabilities = faktor_router::stability::turn_stabilities(&seen[2].history);
    assert!(
        (stabilities[1] - 1.0).abs() < 1e-12,
        "turn 2 must be cache-stable: {stabilities:?}"
    );
    let static_semi_stable: u64 = {
        let seg = seen[2].history[1].segments.as_ref().expect("segments");
        seg.segment_token_counts
            .iter()
            .take(faktor_context::wire_plan::PROMPT_CACHEABLE_PREFIX_SEGMENTS)
            .sum()
    };
    assert!(static_semi_stable > 0);
    assert_eq!(
        seen[2].history[1].stable_leading_tokens,
        Some(static_semi_stable),
        "the stable leading prefix of turn 2 is the cacheable static+semistable total"
    );
    let stabilities4 = faktor_router::stability::turn_stabilities(&seen[3].history);
    assert!(
        (stabilities4[2] - 0.0).abs() < 1e-12,
        "the rewritten cacheable head must measure 0 stability: {stabilities4:?}"
    );
    assert_eq!(seen[3].history[2].stable_leading_tokens, Some(0));

    // Cost estimates change accordingly. The production policy supplies
    // no CacheState: turns 1-3 are the plain uncached base, and turn 4's
    // decision carries the churn premium the prefix rewrite caused.
    let econ = cache_route_candidate().economics;
    let base = |c: &ObservedCacheConsult| {
        faktor_router::estimated_call_cost(&econ, c.context_tokens, c.output_tokens, 0, 0)
    };
    for consult in seen.iter().take(3) {
        assert_eq!(consult.production_cost, base(consult));
    }
    let base4 = base(&seen[3]);
    let penalized4 = faktor_router::stability::apply_churn_penalty(
        base4,
        0.0,
        faktor_router::stability::DEFAULT_STABILITY_FLOOR,
    );
    assert!(
        penalized4 > base4,
        "the churn premium must raise the turn-4 estimate"
    );
    assert_eq!(
        seen[3].production_cost, penalized4,
        "turn 4's production estimate must carry the churn premium"
    );

    // The observed cache state prices: turn 3's consult discounts
    // exactly the reported N cache reads; turn 4's churn zeroes the
    // discount and adds the premium on the uncached base.
    let cached3 = faktor_router::estimated_call_cost(
        &econ,
        seen[2].context_tokens,
        seen[2].output_tokens,
        N_CACHE.min(seen[2].context_tokens),
        0,
    );
    assert_eq!(seen[2].enriched_cost, cached3);
    assert!(
        cached3 < seen[2].production_cost,
        "observed cache hits must lower the turn-3 estimate"
    );
    assert_eq!(
        seen[3].enriched_cost, penalized4,
        "churn must zero the observed cache discount and charge the premium"
    );
    assert_eq!(seen[0].enriched_cost, base(&seen[0]));
    assert_eq!(seen[1].enriched_cost, base(&seen[1]));
}

#[tokio::test]
async fn deleted_test_file_is_structured_blocking_and_rides_the_package() {
    // P0-12 row 2 + (f): a deleted test file appears in deleted_tests /
    // the inventory delta, classifies the change RISKY, and blocks even
    // when the independent review model says clean (deletion is a local
    // hard signal).
    let (manager, session, cas, snapshots, _dir) = snapshot_review_env(&[
        (
            "tests/calc.rs",
            "#[test]\nfn adds() {\n    assert_eq!(calc(1, 2), 3);\n}\n",
        ),
        (
            "src/calc.rs",
            "pub fn calc(a: i32, b: i32) -> i32 { a.saturating_add(b) }\n",
        ),
    ]);
    let script = vec![
        ScriptedResponse::ToolCall {
            id: "c1".into(),
            name: "delete_file".into(),
            input: serde_json::json!({"path": "tests/calc.rs"}),
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
        vec![checkpoint_delete_tool()],
        routing,
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let outcome = runtime
        .run_turn(session, "clean up the calc tests", &[])
        .await
        .unwrap();
    let review = outcome.review.expect("review runs");
    assert_eq!(review["verdict"], "block", "{review}");
    let structured = review_evidence_structured(&review);
    assert_eq!(
        structured["deleted_tests"][0], "tests/calc.rs",
        "{structured}"
    );
    assert_eq!(
        structured["inventory"]["removed_tests"][0], "tests/calc.rs",
        "{structured}"
    );
    assert_eq!(structured["risk"]["level"], "high", "{structured}");
    assert_eq!(
        structured["review_model"]["verdict"], "clean",
        "the model said clean — the local deletion signal still blocks: {structured}"
    );
    match outcome.completion {
        Some(CompletionGate::BlockedVerification { reasons }) => {
            assert!(
                reasons.iter().any(|r| r.code == ReasonCode::ReviewBlocked
                    && r.detail.contains("deleted test file")),
                "{reasons:?}"
            );
        }
        other => panic!("deleted test must gate Blocked, got {other:?}"),
    }
}

#[tokio::test]
async fn content_equal_rename_surfaces_in_the_structured_statuses() {
    // P0-12 row 4: a delete+recreate with identical content is surfaced
    // as a rename in the structured statuses (never a bare delete+add).
    let shared = "pub fn moved() -> u32 {\n    let base: u32 = 41;\n    base.saturating_mul(2).saturating_add(1)\n}\n";
    let (manager, session, cas, snapshots, _dir) = snapshot_review_env(&[("src/old.rs", shared)]);
    let script = vec![
        ScriptedResponse::ToolCall {
            id: "c1".into(),
            name: "delete_file".into(),
            input: serde_json::json!({"path": "src/old.rs"}),
        },
        ScriptedResponse::ToolCall {
            id: "c2".into(),
            name: "write_file".into(),
            input: serde_json::json!({"path": "src/new.rs", "content": shared}),
        },
        ScriptedResponse::Text("done".into()),
        ScriptedResponse::End,
    ];
    let (deps, _d) = snapshot_review_deps(
        &manager,
        &snapshots,
        &cas,
        vec![Arc::new(scripted_provider(script))],
        vec![checkpoint_delete_tool(), checkpoint_write_tool()],
        crate::FixedRoutingPolicy::passthrough(),
    );
    let runtime = AgentRuntime::new(deps).unwrap();
    let outcome = runtime
        .run_turn(session, "move the helper to its new home", &[])
        .await
        .unwrap();
    let review = outcome.review.expect("review runs");
    let structured = review_evidence_structured(&review);
    let files = structured["files"].as_array().unwrap();
    let old = files
        .iter()
        .find(|f| f["path"] == "src/old.rs")
        .expect("deleted side present");
    assert_eq!(old["status"], "renamed", "{structured}");
    assert_eq!(old["renamed_to"], "src/new.rs", "{structured}");
    let new = files
        .iter()
        .find(|f| f["path"] == "src/new.rs")
        .expect("added side present");
    assert_eq!(new["status"], "renamed", "{structured}");
    assert_eq!(new["renamed_from"], "src/old.rs", "{structured}");
    assert_eq!(outcome.completion, Some(CompletionGate::VerifiedComplete));
}

#[tokio::test]
async fn crash_reopen_reconciles_each_uncertain_attempt_from_its_own_completed_row() {
    // Crash reopen -> reconciliation joins BY ATTEMPT ID exactly: two
    // dispatched attempts of the same logical op crash (never settle);
    // on reopen both become UNCERTAIN, and once the runtime's settle
    // sites write EACH attempt's own completed row (the exact
    // record_provider_call_attempt shape this wave wired in), the
    // reconcile settles each reservation FROM ITS OWN row's canonical
    // usage at its own frozen snapshot — a sibling's row never pays for
    // another attempt, and an attempt with no completed row stays
    // UNCERTAIN for the task-end finalize.
    let dir = fresh_store_dir();
    let snapshot = faktor_core::model::PricingSnapshot::exact(
        faktor_core::model::PriceQuote {
            input: faktor_core::model::MicroUsdPerMillionTokens::from_dollars_per_million(15),
            output: faktor_core::model::MicroUsdPerMillionTokens::from_dollars_per_million(60),
            cache_read: faktor_core::model::MicroUsdPerMillionTokens::ZERO,
            cache_write: faktor_core::model::MicroUsdPerMillionTokens::ZERO,
        },
        1,
        "fake".to_string(),
    );
    let (sid, task_id);
    let attempt1 = ModelCallAttempt::new(OpId::new(100), OpId::new(101), 0).unwrap();
    let attempt2 = ModelCallAttempt::new(OpId::new(100), OpId::new(102), 1).unwrap();
    let attempt3 = ModelCallAttempt::new(OpId::new(100), OpId::new(103), 2).unwrap();
    let (r1, r2, r3);
    {
        let manager =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let ledger = faktor_session::DurableBudgetLedger::new(manager.clone());
        let s = manager
            .create_session(manager.create_workspace("/w").unwrap(), "t", "fake", "m")
            .unwrap();
        sid = s.id();
        let handle = manager.get_session(sid).unwrap().unwrap();
        let now = handle.now_ms();
        let tid = handle.task_id().unwrap();
        task_id = tid;
        handle
            .create_task(faktor_session::Task {
                task_id: tid,
                session_id: sid,
                goal: "crash-reconcile".into(),
                acceptance_criteria: vec![],
                plan: vec![],
                attachments: Vec::new(),
                budget: faktor_session::TaskBudget::default(),
                state: faktor_core::state::TaskState::Pending,
                created_ms: now,
                updated_ms: now,
            })
            .unwrap();
        r1 = ledger
            .reserve_attempt(sid, tid, attempt1, 2_000, Some(snapshot.clone()))
            .await
            .unwrap();
        ledger.mark_dispatched(sid, r1).await.unwrap();
        r2 = ledger
            .reserve_attempt(sid, tid, attempt2, 3_000, Some(snapshot.clone()))
            .await
            .unwrap();
        ledger.mark_dispatched(sid, r2).await.unwrap();
        r3 = ledger
            .reserve_attempt(sid, tid, attempt3, 900, Some(snapshot.clone()))
            .await
            .unwrap();
        ledger.mark_dispatched(sid, r3).await.unwrap();
        // Crash: all three dispatched reservations never settled.
    }
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let ledger = faktor_session::DurableBudgetLedger::new(manager.clone());
    ledger.recover_after_restart();
    let view = ledger
        .session_budget_view(sid, task_id)
        .expect("durable budget view");
    assert_eq!(view.uncertain_reservations, 3);
    // Reconcile before any row completes: nothing to join.
    let report = ledger.reconcile_uncertain(sid, task_id).await.unwrap();
    assert_eq!(report, faktor_store::CostReconcileReport::default());
    // The resumed daemon's settle sites write the ATTEMPT-KEYED
    // completed rows (the runtime's new record_provider_call_attempt
    // shape): attempts 1 and 2 completed with canonical usage, attempt
    // 3 never completed (no row).
    let s = manager.get_session(sid).unwrap().unwrap();
    s.record_provider_call_attempt(
        attempt1,
        Some(r1),
        "fake",
        "m",
        "completed",
        Some(100_000),
        Some(2_000),
        None,
    )
    .unwrap();
    s.record_provider_call_attempt(
        attempt2,
        Some(r2),
        "fake",
        "m",
        "completed",
        Some(50_000),
        Some(1_000),
        None,
    )
    .unwrap();
    let report = ledger.reconcile_uncertain(sid, task_id).await.unwrap();
    assert_eq!(
        report.settled, 2,
        "exactly the two attempts with completed rows reconcile"
    );
    assert_eq!(
        report.charged_micro,
        1_620_000 + 810_000,
        "100k@15 + 2k@60 == 1_620_000 and 50k@15 + 1k@60 == 810_000 at the frozen snapshot"
    );
    let rows = ledger.reservations_of(sid, task_id, 10).unwrap();
    let by_attempt = |op: OpId| {
        rows.iter()
            .find(|r| r.attempt_op_id == Some(op))
            .unwrap()
            .clone()
    };
    assert_eq!(
        by_attempt(OpId::new(101)).provider_cost_micro,
        Some(1_620_000),
        "attempt 1's row settled attempt 1's reservation from ATTEMPT 1's tokens"
    );
    assert_eq!(
        by_attempt(OpId::new(102)).provider_cost_micro,
        Some(810_000),
        "attempt 2's row settled attempt 2's reservation from ATTEMPT 2's tokens"
    );
    assert_eq!(
        by_attempt(OpId::new(103)).status,
        "uncertain",
        "an attempt whose own row never completed stays UNCERTAIN for the finalize"
    );
    assert_eq!(rows.iter().filter(|r| r.status == "settled").count(), 2);
    // Idempotent: a second pass settles nothing more.
    let report = ledger.reconcile_uncertain(sid, task_id).await.unwrap();
    assert_eq!(report, faktor_store::CostReconcileReport::default());
}

/// (1) ModelCallIntent production tripwire — Implement: the RouteRequest
/// a REAL turn hands the policy carries the ACTUAL planned wire plan's
/// measured token total (not a guess), the configured execution output
/// reserve and the real call role.
#[tokio::test]
async fn model_call_intent_tripwire_implement_matches_the_planned_wire() {
    let caps = ModelCapabilities {
        tools: true,
        streaming: true,
        context: 200_000,
        ..Default::default()
    };
    assert_eq!(caps.max_output, 4096, "the execution reserve this pins");
    let planner = Arc::new(FakeProvider::with_script(
        "fake",
        caps.clone(),
        vec![ScriptedResponse::Text("ok".into()), ScriptedResponse::End],
    ));
    type WireCapture = (String, Vec<RequestMessage>, Vec<faktor_provider::ToolSpec>);
    let captured: Arc<std::sync::Mutex<Vec<WireCapture>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = captured.clone();
    let provider = Arc::new(InspectingProvider::new(
        planner,
        move |_i, req: &GenericAgentRequest| {
            sink.lock().unwrap().push((
                req.system.clone(),
                req.messages.clone(),
                req.tools.clone(),
            ));
            Ok(())
        },
    ));
    let spy = RecordingRouter::new();
    let (mut deps, _dir) = deps_with(provider, vec![]);
    deps.routing = spy.clone();
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let outcome = runtime
        .run_turn(session, "implement the parser", &[])
        .await
        .unwrap();
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    let requests = spy.requests();
    assert_eq!(requests.len(), 1, "one routed Implement call");
    let req = &requests[0];
    let wires = captured.lock().unwrap().clone();
    assert_eq!(wires.len(), 1, "one provider request for the routed call");
    let planned =
        faktor_context::measure_wire_request(&wires[0].0, &wires[0].1, &wires[0].2) as u64;
    assert!(planned > 0);
    assert_eq!(req.phase, RouterPhase::Implement, "phase == the real role");
    assert_eq!(
        req.context_tokens, planned,
        "context_tokens must equal the ACTUAL planned WirePlan token count"
    );
    assert_eq!(
        req.estimated_output_tokens,
        u64::try_from(caps.max_output).unwrap(),
        "output tokens must equal the configured execution reserve"
    );
}

/// (1) Every call role: the production conversion from a
/// `ModelCallIntent` to the wire `RouteRequest` preserves the phase, the
/// planned input and the configured reserve for Implement/Review/Compact
/// (production builders) and Summarize/Debug/TestAnalysis (the same
/// public conversion the runtime uses when it routes those roles).
#[test]
fn model_call_intent_tripwire_covers_every_call_role() {
    let other = |phase: RouterPhase, reserve: u64| crate::ModelCallIntent {
        phase,
        required_capabilities: vec!["streaming".into()],
        quality: crate::QualityRequirement::Hard { minimum: 60 },
        expected_output_tokens: reserve,
        semantic_risk: 0,
        output_trust: crate::OutputTrust::Implementation,
    };
    let cases: Vec<(RouterPhase, crate::ModelCallIntent, u64, u64)> = vec![
        (
            RouterPhase::Implement,
            {
                let mut intent = crate::ModelCallIntent::implement_main();
                intent.expected_output_tokens = 4096;
                intent
            },
            9_000,
            4096,
        ),
        (
            RouterPhase::Review,
            crate::ModelCallIntent::review(),
            5_000,
            2048,
        ),
        (
            RouterPhase::Compact,
            crate::ModelCallIntent::compact(),
            12_000,
            4096,
        ),
        (
            RouterPhase::Summarize,
            other(RouterPhase::Summarize, 4096),
            8_000,
            4096,
        ),
        (
            RouterPhase::Debug,
            other(RouterPhase::Debug, 2048),
            7_000,
            2048,
        ),
        (
            RouterPhase::TestAnalysis,
            other(RouterPhase::TestAnalysis, 2048),
            6_000,
            2048,
        ),
    ];
    for (phase, intent, planned, reserve) in cases {
        let router = RecordingRouter::new();
        assert_eq!(intent.phase, phase);
        let req = intent.route_request(planned, reserve, 0);
        router.route(&req).unwrap();
        let seen = router.requests();
        assert_eq!(seen.len(), 1, "{phase:?}: one captured request");
        assert_eq!(seen[0].phase, phase, "phase == the actual call role");
        assert_eq!(seen[0].context_tokens, planned, "{phase:?}: planned input");
        assert_eq!(
            seen[0].estimated_output_tokens, reserve,
            "{phase:?}: output tokens == the configured reserve"
        );
        assert_eq!(
            seen[0].quality_floor,
            intent.quality_floor(),
            "{phase:?}: the intent's own floor crosses verbatim"
        );
    }
}

/// P2 coordination boundary: an orchestrated child's wire request carries
/// ONE bounded `coordination: N unread; use board_read` line, memoized by
/// board revision (1 vs 10 000 unread is the same line modulo digits),
/// never a board body, and non-children get no notice at all.
#[tokio::test]
async fn child_boundary_coordination_notice_is_bounded_never_a_body_and_memoized() {
    let (seed_deps, _dir0) = deps(scripted_provider(vec![ScriptedResponse::End]), vec![]);
    let (manager, root) = shared_session(&seed_deps);
    let root_handle = manager.get_session(root).unwrap().unwrap();
    let ws = root_handle.row().unwrap().workspace_id;
    let child = manager
        .create_child_session(
            root,
            ws,
            faktor_core::id::WorktreeId::new(1),
            TaskId::new(1),
            "fake",
            "m",
            "coordination child",
            faktor_session::child::ChildOwnership::IsolatedWorktree,
        )
        .unwrap();
    let secret = "SECRET BOARD BODY MUST NEVER ENTER THE PROMPT";
    root_handle.board_post("status", secret, &[]).unwrap();

    let captured: Arc<std::sync::Mutex<Vec<GenericAgentRequest>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let cap = captured.clone();
    let inspected = Arc::new(InspectingProvider::new(
        Arc::new(FakeProvider::with_script(
            "fake",
            ModelCapabilities {
                tools: true,
                ..Default::default()
            },
            vec![ScriptedResponse::Text("ok".into()), ScriptedResponse::End],
        )),
        move |_n, req| {
            cap.lock().unwrap().push(req.clone());
            Ok(())
        },
    ));
    let (deps1, _dir1) = deps_sharing_session(manager.clone(), inspected, vec![]);
    let runtime = AgentRuntime::new(deps1).unwrap();

    // One unread => the exact single line; the unchanged revision is
    // served from the memo (byte-identical) and the root gets nothing.
    let first = runtime.child_coordination_notice(&child).unwrap();
    assert_eq!(
        first.as_deref(),
        Some("coordination: 1 unread; use board_read")
    );
    assert_eq!(
        runtime.child_coordination_notice(&child).unwrap(),
        first,
        "unchanged revision => byte-identical memoized notice"
    );
    assert_eq!(
        runtime.child_coordination_notice(&root_handle).unwrap(),
        None,
        "only orchestrated children carry the notice"
    );

    // A second post bumps the revision: the notice recomputes to the
    // same single line with the new count (coalesced, still O(1)).
    root_handle
        .board_post("again", "another body", &[])
        .unwrap();
    assert_eq!(
        runtime
            .child_coordination_notice(&child)
            .unwrap()
            .as_deref(),
        Some("coordination: 2 unread; use board_read")
    );

    // The child's turn injects the notice into the volatile steering
    // slot and never a board body.
    let outcome = runtime.run_turn(child.id(), "go", &[]).await.unwrap();
    assert!(
        !matches!(
            outcome.final_state,
            AgentState::FailedRecoverable | AgentState::FailedPermanent
        ),
        "{:?}",
        outcome.final_state
    );
    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 1, "one wire request for the turn");
    let system = &requests[0].system;
    assert!(
        system.contains("coordination: 2 unread; use board_read"),
        "the bounded notice must ride the child prompt: {system}"
    );
    assert!(
        !system.contains(secret) && !system.contains("another body"),
        "board bodies never enter the prompt automatically"
    );
}

#[tokio::test]
async fn superseding_cancel_loss_is_marked_and_reconstructed_on_reopen() {
    // Adversarial: a mutating turn supersedes the still-open verification
    // attempt; the typed cancel write fails, so the open job must stay
    // open (never silently certified), the marker must be durable, and
    // the reopen replay must cancel the superseded attempt's jobs.
    if !std::process::Command::new("make")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
    {
        eprintln!("skipping cancel-replay test: no make on this host");
        return;
    }
    let (manager, session, dir) = make_background_env("\t@true\n", None);
    let root = dir.path().join("ws");
    let (mut deps, _d) = deps_sharing_session(
        manager.clone(),
        Arc::new(scripted_provider(vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                input: serde_json::json!({
                    "path": "src/main.c",
                    "content": "int main(void) {\n    return 0;\n}\n"
                }),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ])),
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
        goal: "supersede".into(),
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
    assert!(attempt.pending, "the first attempt enqueued open jobs");
    durable_faults_tests::arm(
        runtime.deps().session.store().root(),
        DW_SITE_SUPERSEDE_CANCEL,
    );
    let outcome = runtime
        .run_turn(session, "change main.c", &[])
        .await
        .unwrap();
    assert!(
        outcome.completion.is_some(),
        "the turn must classify honestly, not fail on the lost cancel"
    );
    let rows = h
        .verification_attempt_jobs(task_id.raw(), attempt.attempt_op)
        .unwrap();
    assert!(
        rows.iter().any(|job| job.state.is_open()),
        "the lost cancel left the superseded job open: {rows:?}"
    );
    assert_eq!(
        marker_sites(manager.store().root()),
        vec![DW_SITE_SUPERSEDE_CANCEL.to_string()]
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
        rows.iter()
            .all(|job| job.state == faktor_session::VerificationJobState::Cancelled),
        "the supersede cancel was not reconstructed: {rows:?}"
    );
    assert!(marker_files(manager2.store().root()).is_empty());
}
