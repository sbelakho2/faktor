//! `runtime::retry_tests`: out-of-line tests.

#![allow(unused_imports)]

use super::*;
use crate::runtime::fixtures_tests::*;
use crate::runtime::tests::*;
use crate::*;

#[tokio::test]
async fn environment_fingerprint_is_stable_and_manifest_sensitive() {
    // Audits 94/116/117: identical verification inputs produce an
    // identical fingerprint, and a changed manifest/lockfile hash moves
    // it (the fingerprint is evidence, not decoration).
    let (manager, session, dir) = verified_shared_env();
    let (turn_deps, _d) = verified_turn_deps(&manager, vec![], fake_ok(), 0.65);
    let runtime = AgentRuntime::new(turn_deps).unwrap();
    let handle = manager.get_session(session).unwrap().unwrap();
    let root = dir.path().join("ws");
    let workspace_id = handle.row().unwrap().workspace_id;
    let workspace = runtime
        .deps
        .workspaces
        .open(workspace_id, root.clone())
        .unwrap();
    let task_id = handle.task_id().unwrap();
    let now = handle.now_ms();
    handle
        .create_task(Task {
            task_id,
            session_id: session,
            goal: "fingerprint".into(),
            acceptance_criteria: vec!["goal: fingerprint".into()],
            plan: vec![],
            attachments: Vec::new(),
            budget: Default::default(),
            state: TaskState::Pending,
            created_ms: now,
            updated_ms: now,
        })
        .unwrap();
    let basis = vec![(
        "rust_check".to_string(),
        "cargo".to_string(),
        vec!["check".to_string()],
    )];
    let (fp1, cref1) = runtime
        .verification_fingerprint(&handle, task_id, &basis, &[], None, Some(&workspace))
        .unwrap();
    assert_eq!(fp1.platform, std::env::consts::OS);
    assert_eq!(fp1.arch, std::env::consts::ARCH);
    assert_eq!(fp1.task_contract_hash.len(), 64);
    assert!(fp1.manifest_hashes.iter().any(|m| m.path == "Cargo.toml"));
    assert!(fp1.lockfile_hashes.is_empty(), "no lockfile exists yet");
    let (fp2, cref2) = runtime
        .verification_fingerprint(&handle, task_id, &basis, &[], None, Some(&workspace))
        .unwrap();
    assert_eq!(fp1, fp2, "identical inputs yield an identical fingerprint");
    assert_eq!(
        cref1, cref2,
        "identical inputs yield an identical candidate reference"
    );
    // A lockfile appears: the fingerprint AND the candidate aggregate
    // both move.
    std::fs::write(root.join("Cargo.lock"), "# lock v1\n").unwrap();
    let (fp3, cref3) = runtime
        .verification_fingerprint(&handle, task_id, &basis, &[], None, Some(&workspace))
        .unwrap();
    assert_ne!(fp1, fp3, "a lockfile change moves the fingerprint");
    assert!(fp3.lockfile_hashes.iter().any(|m| m.path == "Cargo.lock"));
    assert_ne!(cref1.candidate_manifest_hash, cref3.candidate_manifest_hash);
    // A manifest change moves it again.
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"x\"\nversion = \"0.2.0\"\n",
    )
    .unwrap();
    let (fp4, _) = runtime
        .verification_fingerprint(&handle, task_id, &basis, &[], None, Some(&workspace))
        .unwrap();
    assert_ne!(fp3, fp4, "a manifest change moves the fingerprint");
    // The candidate reference pins the candidate: a manifest the
    // candidate changed is excluded from the base aggregate, and the
    // evidence folds capture the changed file + the review verdict.
    let changed = vec![FileStateEvidence {
        path: "Cargo.toml".into(),
        digest_hex: "aa".repeat(32),
        size: 12,
    }];
    let (_, cref5) = runtime
        .verification_fingerprint(
            &handle,
            task_id,
            &basis,
            &changed,
            Some(&serde_json::json!({"verdict": "clean"})),
            Some(&workspace),
        )
        .unwrap();
    assert_ne!(
        cref5.base_manifest_hash, cref5.candidate_manifest_hash,
        "a changed manifest is excluded from the base aggregate"
    );
    assert!(cref5.source_diff_evidence.is_some());
    assert!(cref5.risk_report_evidence.is_some());
    assert_eq!(cref5.task_revision, handle.task_revision(task_id).unwrap());
    assert!(cref5
        .accounting_snapshot_digest
        .starts_with("accounting:v1:"));
}

#[tokio::test]
async fn progress_turn_resets_durable_loop_window() {
    // A turn that makes real progress closes every loop window. Without
    // the reset, failures on turns 1, 3, 4 would reach the threshold at
    // turn 4; with it, only three CONSECUTIVE failures after the progress
    // turn trip (turn 6).
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
    let ok = Tool {
        name: "write_file".into(),
        description: "w".into(),
        input_schema: serde_json::json!({"type": "object"}),
        resource_class: faktor_core::resource::ResourceClass::DiskWrite,
        capability: None,
        recovery_hint: RecoveryHint::WorkspaceWrite,
        path_args: vec!["path".into()],
        execute: Arc::new(|_ctx, args| {
            Box::pin(async move {
                let _ = args;
                Ok(ToolOutcome {
                    text: "wrote".into(),
                    exit_code: Some(0),
                    ..Default::default()
                })
            })
        }),
    };
    let (seed_deps, _dir0) = deps(scripted_provider(vec![ScriptedResponse::End]), vec![]);
    let (manager, session) = shared_session(&seed_deps);
    let run =
        |tag: &str, tool_calls: Vec<(String, String, serde_json::Value)>, tools: Vec<Tool>| {
            let mut script = Vec::new();
            for (cid, name, input) in tool_calls {
                script.push(ScriptedResponse::ToolCall {
                    id: cid,
                    name,
                    input,
                });
            }
            script.push(ScriptedResponse::End);
            let (turn_deps, _dir) =
                deps_sharing_session(manager.clone(), Arc::new(scripted_provider(script)), tools);
            let runtime = AgentRuntime::new(turn_deps).unwrap();
            let tag = tag.to_string();
            async move { runtime.run_turn(session, &tag, &[]).await }
        };
    let fail = |tag: &str| {
        (
            tag.to_string(),
            vec![(
                format!("c_{tag}"),
                "run_command".to_string(),
                serde_json::json!({"command": "cargo check -p faktor-core"}),
            )],
            vec![boom.clone()],
        )
    };
    // Turn 1: failure (count 1). No trip.
    let (t1, t1calls, t1tools) = fail("t1");
    let o1 = run(&t1, t1calls, t1tools).await.unwrap();
    assert!(!o1.loop_stopped);
    // Turn 2: write_file succeeds — progress resets the window.
    let o2 = run(
        "write the fix",
        vec![(
            "c_ok".into(),
            "write_file".into(),
            serde_json::json!({"path": "src/x.rs", "content": "y"}),
        )],
        vec![ok],
    )
    .await
    .unwrap();
    assert!(!o2.loop_stopped);
    assert!(o2.turns >= 1);
    // Turns 3-4: failures (counts 1, 2 after the reset). No trip yet —
    // WITHOUT the reset count 2 + this would already trip at turn 4.
    for i in 0..2 {
        let (t, calls, tools) = fail(&format!("r{i}"));
        let o = run(&t, calls, tools).await.unwrap();
        assert!(!o.loop_stopped, "progress must have reset the window");
    }
    // Turn 6: third consecutive failure after the reset → trip.
    let (t, calls, tools) = fail("final");
    let o6 = run(&t, calls, tools).await.unwrap();
    assert!(o6.loop_stopped, "3 identical failures after a reset trip");
    assert_eq!(
        o6.stop_reason.as_ref().map(|r| r.code),
        Some(ReasonCode::LoopDetected),
        "loop stops carry the machine code: {:?}",
        o6.stop_reason
    );
}

#[tokio::test]
async fn retryable_pre_accept_failure_retries_under_policy() {
    // Spec §13: a NETWORK failure before any content became durable is
    // retried (bounded, state-aware). Without the retry the turn would
    // land on FailedRecoverable.
    let (mut deps, _dir) = deps(scripted_provider(vec![ScriptedResponse::End]), vec![]);
    deps.retry_policy = faktor_core::retry::RetryPolicy {
        max_attempts: 3,
        base_delay_ms: 1,
        max_delay_ms: 5,
        jitter: 0.0,
        class: faktor_core::retry::RetryClass::Network,
    };
    // A provider that errors BEFORE its first chunk on the first stream
    // call (a network-class failure), then serves normally. The script
    // is consumed per stream, so the retried request sees an empty
    // script — the point is the retry happens at all.
    let flaky = FakeProvider::die_before_stream(
        "fake",
        ModelCapabilities {
            streaming: true,
            tools: true,
            ..Default::default()
        },
        vec![ScriptedResponse::End],
    );
    let mut registry = ProviderRegistry::new();
    registry.try_register(Arc::new(flaky)).unwrap();
    deps.providers = Arc::new(registry);
    let runtime = AgentRuntime::new(deps).unwrap();
    let session = new_session(runtime.deps());
    let outcome = runtime.run_turn(session, "retry me", &[]).await.unwrap();
    assert_eq!(
        outcome.final_state,
        AgentState::ReadyForNextTurn,
        "a retryable pre-accept failure must retry, not fail the turn"
    );
}

/// F3 (adversarial): a PERSISTENT queue-head read failure must not be
/// read as "empty" (which would release the gate immediately) and must
/// not spin forever. The runner retries a bounded number of passes,
/// leaves the durable row pending (it is never dropped), and records the
/// exhausted retries as a durable audit marker.
#[tokio::test]
async fn queue_head_read_failure_is_bounded_loud_and_leaves_rows_pending() {
    let dir = fresh_store_dir();
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let (deps, _keep) =
        deps_sharing_session(manager.clone(), Arc::new(scripted_provider(vec![])), vec![]);
    let runtime = Arc::new(AgentRuntime::new(deps).unwrap());
    runtime.set_turn_budget_ms(60);
    let ws = manager.create_workspace("/w").unwrap();
    let handle = manager.create_session(ws, "t", "fake", "m").unwrap();
    let op = manager.try_next_op_id().unwrap();
    manager
        .store()
        .enqueue_prompt(handle.id(), op, "queued", &[], None, None, None, 1)
        .unwrap();
    // Mid-turn without a turn record: admission declines for the whole
    // pass, so the pass ends in its bounded typed timeout and the gate
    // re-check is what the fault targets.
    let active = manager.try_next_op_id().unwrap();
    handle
        .append_event(
            faktor_core::event::EventKind::PromptReceived,
            AgentState::Preparing,
            Some(active),
            Some(serde_json::json!({ "queued": false })),
        )
        .unwrap();
    durable_faults_tests::arm_repeat(manager.store().root(), DW_SITE_QUEUE_HEAD_READ, u32::MAX);
    let began = Instant::now();
    runtime.run_session_queue(handle.id()).await;
    let elapsed = began.elapsed();
    durable_faults_tests::disarm(manager.store().root(), DW_SITE_QUEUE_HEAD_READ);
    assert!(
        elapsed < Duration::from_secs(30),
        "the retry loop is bounded (took {elapsed:?})"
    );
    assert_eq!(
        handle.queued_prompt_count().unwrap(),
        1,
        "the durable queue head is never dropped by a read failure"
    );
    let events = handle.events_range(1, None).unwrap();
    assert!(
        events.iter().any(|e| {
            e.kind == faktor_core::event::EventKind::CrashDetected
                && e.payload.as_ref().is_some_and(|p| {
                    p.get("durable_write_failure")
                        .and_then(|d| d.get("site"))
                        .and_then(|s| s.as_str())
                        == Some(DW_SITE_QUEUE_HEAD_READ)
                })
        }),
        "the exhausted retries leave a durable audit marker naming the site"
    );
}

#[tokio::test]
async fn routing_failures_are_all_fail_closed_and_the_tripwire_never_streams() {
    // P0-88 fail-closed matrix + fallback deletion (attempt-accounting
    // audit F): EVERY routing failure is a TYPED terminal error on the
    // turn — there is no RouterUnavailable fallback anymore, and the
    // session-configured model is NEVER a bypass. A provider whose
    // stream PANICS if invoked is the tripwire: with a router that
    // refuses (zero capable candidates => NoCapableModel), the typed
    // route failure must surface and the provider call count must be 0.
    // EconomicRoutingPolicy over an EMPTY candidate set refuses typed
    // (no candidate clears capability/fit), proving the zero-candidate
    // tripwire path is a typed failure, not an empty-handed fallback.
    let policy_empty = crate::EconomicRoutingPolicy::new(
        Arc::new(faktor_router::RouterService::new(vec![])),
        crate::RoutingMode::Economy,
    );
    assert!(
        matches!(
            policy_empty.route(&faktor_router::RouteRequest {
                required_capabilities: vec!["tools".into(), "streaming".into()],
                ..Default::default()
            }),
            Err(crate::RouteFailure::NoCapableModel)
        ),
        "zero candidates => typed NoCapableModel"
    );
    let caps = ModelCapabilities {
        tools: true,
        streaming: true,
        ..Default::default()
    };
    let provider = FakeProvider::with_script(
        "fake",
        caps.clone(),
        vec![
            ScriptedResponse::Text("fallback".into()),
            ScriptedResponse::End,
        ],
    );
    for failure in [
        crate::RouteFailure::BudgetExceeded,
        crate::RouteFailure::NoCapableModel,
        crate::RouteFailure::PolicyDenied,
    ] {
        let probe = Arc::new(provider.clone());
        let (mut adeps, _dir) = deps_with(probe.clone(), vec![]);
        adeps.routing = crate::FixedRoutingPolicy::failing(failure.clone());
        let runtime = AgentRuntime::new(adeps).unwrap();
        let session = new_session(runtime.deps());
        let outcome = runtime.run_turn(session, "hi", &[]).await.unwrap();
        assert_eq!(
            outcome.final_state,
            AgentState::FailedRecoverable,
            "{failure:?}: a routing refusal is a terminal turn error, never a silent fallback"
        );
        if matches!(failure, crate::RouteFailure::BudgetExceeded) {
            assert_eq!(
                outcome.stop_reason.as_ref().map(|r| r.code),
                Some(ReasonCode::BudgetExceeded),
                "{failure:?}: the typed stop reason carries budget_exceeded"
            );
        }
        assert_eq!(
            probe.last_request_model(),
            None,
            "{failure:?}: no model call may reach a provider after a fail-closed denial"
        );
        let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
        let events = handle.events_range(1, None).unwrap();
        assert!(
            events.iter().any(|e| {
                e.kind == faktor_core::event::EventKind::Failed
                    && e.payload.as_ref().is_some_and(|p| {
                        p["message"]
                            .as_str()
                            .is_some_and(|m| m.contains("routing refused the model call"))
                    })
            }),
            "{failure:?}: the journal must carry the typed routing refusal"
        );
    }
    // TRIPWIRE: a provider whose stream panics if invoked, behind a
    // router with ZERO candidates. The typed route failure must surface
    // BEFORE any provider call — a single stream invocation would
    // panic the test.
    let tripwire = CountingPanicProvider::new(caps.clone());
    let tripwire_arc = Arc::new(tripwire.clone());
    let (mut adeps, _dir) = deps_with(tripwire_arc.clone(), vec![]);
    adeps.routing = crate::EconomicRoutingPolicy::new(
        Arc::new(faktor_router::RouterService::new(vec![])),
        crate::RoutingMode::Economy,
    );
    let runtime = AgentRuntime::new(adeps).unwrap();
    let session = new_session(runtime.deps());
    let outcome = runtime.run_turn(session, "hi", &[]).await.unwrap();
    assert_eq!(
        outcome.final_state,
        AgentState::FailedRecoverable,
        "the zero-candidate router refusal is a typed terminal turn error"
    );
    assert_eq!(
        tripwire.calls(),
        0,
        "a provider whose stream panics must never be invoked after a typed route refusal"
    );
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let events = handle.events_range(1, None).unwrap();
    assert!(
        events.iter().any(|e| {
            e.kind == faktor_core::event::EventKind::Failed
                && e.payload.as_ref().is_some_and(|p| {
                    p["message"]
                        .as_str()
                        .is_some_and(|m| m.contains("routing refused the model call"))
                })
        }),
        "the journal must carry the typed routing refusal"
    );
}

#[tokio::test]
async fn explicitly_uncapped_budget_read_failure_proceeds_to_the_provider() {
    // The ONE case allowed to proceed under unavailable accounting: the
    // read PROVED the task explicitly uncapped. Telemetry is emitted;
    // the provider axis is touched (the review runs), never a write.
    let costly = CostReportingProvider::new(vec![(None, false)]);
    let (mut deps, _dir) = deps_with(costly.clone(), vec![]);
    let failing = ReadFailingBudget::new(BudgetCapEvidence::Uncapped);
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
    assert_eq!(
        costly.stream_count(),
        1,
        "an explicitly uncapped task proceeds despite unavailable accounting"
    );
    assert_eq!(
        failing.reserve_calls(),
        1,
        "the proceeded review still reserves durably"
    );
    assert!(
        outcome
            .refused
            .as_deref()
            .map(|r| !r.contains("budget accounting unavailable"))
            .unwrap_or(true),
        "the refusal, if any, is not the accounting gate: {:?}",
        outcome.refused
    );
}

#[tokio::test]
async fn silent_provider_stream_stalls_and_stops_the_turn() {
    // Adversarial (runtime level): the provider emits NOTHING for far
    // longer than the stall budget — in SKEW time, so the machine's
    // scheduling state cannot suppress or fabricate the verdict. The
    // mid-stream watchdog must stop the turn with a stall verdict
    // (never wait forever, never replay). Wall-clock dependency is
    // bounded to ONE watchdog tick (≤ ~250 ms) plus the 20 s guard.
    let fed = FedProvider::new();
    let (deps, _dir, clock) = deps_with_skew(fed.clone());
    let runtime = AgentRuntime::new(deps).unwrap();
    runtime.set_stall_silence_ms(200);
    let session = new_session(runtime.deps());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let receipt = runtime.submit(session, "work", &[]).unwrap();
    let agent = runtime.clone();
    let handle2 = runtime.deps.session.get_session(session).unwrap().unwrap();
    let drive = tokio::spawn(async move { agent.drive_receipt(&handle2, receipt, None).await });
    // Wait until the stream is genuinely open (durable Streaming state),
    // then freeze real silence for 7.5x the budget in skew time.
    // Environmental margin (documented bound): waits for the DRIVE task
    // under full-suite load; never a timing assertion.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(240);
    loop {
        if handle.state().unwrap() == AgentState::Streaming {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the drive never reached the stream"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    // Nothing is ever sent: total silence.
    wait_fed_sender(&fed).await;
    clock.advance(1500);
    let outcome = tokio::time::timeout(Duration::from_secs(20), drive)
        .await
        .expect("stall detection must terminate the turn")
        .unwrap()
        .unwrap();
    assert!(
        outcome.stalled,
        "silence past the budget must stall: {outcome:?}"
    );
    assert_eq!(
        outcome.stop_reason.as_ref().unwrap().code,
        ReasonCode::Stalled
    );
    assert_eq!(outcome.final_state, AgentState::FailedRecoverable);
    assert_eq!(handle.state().unwrap(), AgentState::FailedRecoverable);
    // The session stays promptable: nothing is stranded.
    let progress = runtime.progress_view(session).unwrap();
    assert_eq!(progress["inFlightOp"], serde_json::Value::Null);
    assert_eq!(progress["stalled"], serde_json::Value::Bool(false));
}

#[tokio::test]
async fn output_every_quarter_budget_never_stalls_across_many_budgets() {
    // The long-running legitimate op: text chunks every 50 skew-ms
    // while the stall budget is 200 skew-ms — the stream runs 10x the
    // budget (40 chunks) and must NEVER be marked stalled. Chunk
    // delivery is test-fed and the stall tracker reads the skew clock,
    // so machine load can neither widen a chunk gap nor delay evidence:
    // every watchdog poll sees at most 50 ms of skew silence. The turn
    // completes normally.
    let fed = FedProvider::new();
    let (deps, _dir, clock) = deps_with_skew(fed.clone());
    let runtime = AgentRuntime::new(deps).unwrap();
    runtime.set_stall_silence_ms(200);
    let session = new_session(runtime.deps());
    let handle = runtime.deps.session.get_session(session).unwrap().unwrap();
    let receipt = runtime.submit(session, "work", &[]).unwrap();
    let agent = runtime.clone();
    let handle2 = runtime.deps.session.get_session(session).unwrap().unwrap();
    let drive = tokio::spawn(async move { agent.drive_receipt(&handle2, receipt, None).await });
    // Wait until the stream is open so the sender exists (bounded).
    // Environmental margin (documented bound): waits for the DRIVE task
    // under full-suite load; never a timing assertion.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(240);
    loop {
        if handle.state().unwrap() == AgentState::Streaming {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the drive never reached the stream"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    wait_fed_sender(&fed).await;
    let sender = fed.sender();
    for i in 0..40 {
        clock.advance(50);
        let _ = sender.send(ProviderChunk::Text {
            text: format!("tick {i}"),
        });
        // Deterministic evidence cadence: the chunk must be consumed
        // (tracker stamped) before the clock moves on.
        wait_output_at(&runtime, session, clock.now_ms(), &format!("tick {i}")).await;
    }
    let _ = sender.send(ProviderChunk::Done);
    let outcome = tokio::time::timeout(Duration::from_secs(20), drive)
        .await
        .expect("the run must terminate")
        .unwrap()
        .unwrap();
    assert!(
        !outcome.stalled && !outcome.loop_stopped,
        "periodic output must never stall: {outcome:?}"
    );
    assert_eq!(outcome.final_state, AgentState::ReadyForNextTurn);
    let progress = runtime.progress_view(session).unwrap();
    assert_eq!(progress["inFlightOp"], serde_json::Value::Null);
    assert_eq!(progress["stalled"], serde_json::Value::Bool(false));
    assert!(progress["lastOutputAt"].is_i64());
    assert!(progress["lastOpCompletedAt"].is_i64());
}

#[tokio::test]
async fn pre_dispatch_dispatch_marker_failure_refunds_hold_and_never_streams() {
    let recorder = DebitRecorder::new(true);
    let budget = DebitBudget::refusing_dispatch_marker(Some(260));
    let (mut deps, _dir) = deps_with(debit_aware_provider(&recorder), vec![]);
    deps.budgets = budget.clone();
    let runtime = AgentRuntime::new(deps).unwrap();
    runtime
        .set_provider_debits(Some(recorder.clone() as Arc<dyn ProviderAttemptDebits>))
        .expect("install debit authority");
    let (manager, session) = shared_session(runtime.deps());
    let _ = manager;
    let err = runtime.run_turn(session, "do the thing", &[]).await;
    assert!(err.is_err(), "a failed dispatch marker fails the turn");
    let events = recorder.events();
    assert!(
        events.iter().any(|e| e == "refund:dispatch_marker_failed"),
        "the never-dispatched hold must refund: {events:?}"
    );
    assert!(
        !events.iter().any(|e| e == "stream"),
        "the provider must never be streamed: {events:?}"
    );
    assert!(recorder.settles.lock().unwrap().is_empty());
    assert_eq!(budget.refunds.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn drive_start_fact_write_failure_propagates_typed_without_marker() {
    // Adversarial: a drive-start fact mirror the caller CAN fail safely
    // on propagates the typed store error (no compensation marker — the
    // failure is loud) and drive_receipt lands the honest failed turn.
    let (manager, session, _dir) = verified_shared_env();
    // An EXISTING task row: the drive-start mirror runs its fact writes
    // only against a row (a first-sighting drive creates the row instead).
    let h = manager.get_session(session).unwrap().unwrap();
    let now = h.now_ms();
    h.create_task(Task {
        task_id: h.task_id().unwrap(),
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
    let (turn_deps, _d) = verified_turn_deps(
        &manager,
        vec![ScriptedResponse::Text("hi".into()), ScriptedResponse::End],
        fake_ok(),
        0.65,
    );
    let runtime = AgentRuntime::new(turn_deps).unwrap();
    durable_faults_tests::arm(
        runtime.deps().session.store().root(),
        DW_SITE_RESTORE_STATE_FACT,
    );
    let err = runtime
        .run_turn(session, "hello", &[])
        .await
        .expect_err("the lost fact mirror must fail the drive typed");
    assert!(
        err.message.contains(DW_SITE_RESTORE_STATE_FACT),
        "the typed error must name the failed write: {err:?}"
    );
    assert!(
        marker_files(manager.store().root()).is_empty(),
        "a propagated failure needs no compensation marker"
    );
    let h = manager.get_session(session).unwrap().unwrap();
    assert_eq!(h.state().unwrap(), AgentState::FailedRecoverable);
    assert!(h.active_turn_record().unwrap().is_none());
}
