#![allow(clippy::await_holding_lock)]
//! TaskExecutor tests: single/multi-item runs, shadowed drives (mechanically split from `task_executor_tests`).

use super::tests_control::*;
use super::*;

/// Heavy file/CAS/process tests are serialized on the ONE crate-wide guard
/// (`crate::test_support::HEAVY_SUITE`, shared with `runtime_tests`): under
/// intra-binary parallelism their store+DbActor+fsync + CAS-file storms on
/// one disk starve each other past any reasonable wall bound (observed
/// 300 s+ tails on shared machines while every test passes in isolation and
/// serially). A per-module guard was not enough — the heavy suites of
/// different modules overlapped.
use crate::test_support::heavy_guard;

#[tokio::test]
pub(crate) async fn single_item_task_matches_the_direct_prompt_path_byte_for_byte() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    // Executor-driven session A versus the direct daemon drive on session
    // B: same provider scripts, same goal — on SEPARATE stores (two
    // managers must never share one SQLite file). The TaskExecutor must
    // produce the same durable op record + session outcome — a wrapper
    // around the one drive path, never a second architecture.
    let env_a = open_env(&dir.path().join("a"), done_script());
    let env_b = open_env(&dir.path().join("b"), done_script());
    let goal = "analyze the module boundaries";

    // A: through the TaskExecutor (single item = the one-work-item plan).
    let req_a = request(goal, vec![wi("a1", WorkKind::Analysis, &[])], &env_a);
    let receipt_a = env_a
        .executor
        .start_task(env_a.parent, req_a)
        .expect("single-item start");
    assert_eq!(receipt_a.mode, TaskRunMode::InSession);
    assert!(!receipt_a.queued);
    let op_a = receipt_a.op_id.expect("real op id");

    // B: the previous direct path (agent.submit + detached drive).
    let receipt_b = env_b
        .agent
        .submit(env_b.parent, goal, &[])
        .expect("direct submit");
    assert!(!receipt_b.queued);
    let handle_b = env_b.manager.get_session(env_b.parent).unwrap().unwrap();
    let receipt_b2 = receipt_b.clone();
    let agent_b = env_b.agent.clone();
    tokio::spawn(async move {
        let _ = agent_b.drive_receipt(&handle_b, receipt_b2, None).await;
    });

    wait_until(
        || state_of(&env_a, env_a.parent) == faktor_core::state::AgentState::ReadyForNextTurn,
        240,
    )
    .await;
    wait_until(
        || state_of(&env_b, env_b.parent) == faktor_core::state::AgentState::ReadyForNextTurn,
        240,
    )
    .await;
    // The state transition and the turn-record finalization are two
    // separate durable writes; on slower hosts (Windows CI) one path can
    // observe ReadyForNextTurn while its record is still `active`. The
    // record is finalized (`drive_turn` -> `finish_turn_record`) only AFTER
    // the end-of-turn content sync (`sync_task_row` re-goals the executor
    // row to the session ledger) and the final gate write, so waiting for a
    // NON-`active` record is the real quiescent point. Equality alone is
    // NOT enough: two in-flight records are both `active` from the moment
    // each prompt is admitted, so an equality-only wait passes before either
    // drive has synced its row (the executor row still carries the run goal
    // while the direct path's row already carries the ledger goal).
    {
        let ha = env_a.manager.get_session(env_a.parent).unwrap().unwrap();
        let hb = env_b.manager.get_session(env_b.parent).unwrap().unwrap();
        // `is_terminal_turn_record_status` is the store's ONE terminal-status
        // vocabulary (completed | cancelled | failed), never a local
        // re-spelling of `!= "active"`.
        let finished = |s: &Option<String>| {
            matches!(
                s.as_deref(),
                Some(status) if faktor_store::is_terminal_turn_record_status(status)
            )
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(240);
        loop {
            let a = ha.turn_record(op_a).unwrap().map(|r| r.status);
            let b = hb.turn_record(receipt_b.op_id).unwrap().map(|r| r.status);
            if finished(&a) && finished(&b) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "turn records never reached terminal status: a={a:?} b={b:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    // The same durable outcome on both sides.
    let ha = env_a.manager.get_session(env_a.parent).unwrap().unwrap();
    let hb = env_b.manager.get_session(env_b.parent).unwrap().unwrap();
    assert_eq!(
        ha.message_count().unwrap(),
        hb.message_count().unwrap(),
        "same message stream as the direct path"
    );
    let rec_a = ha.turn_record(op_a).unwrap().unwrap();
    let rec_b = hb.turn_record(receipt_b.op_id).unwrap().unwrap();
    assert_eq!(rec_a.status, rec_b.status);
    assert_eq!(rec_a.effective_provider, rec_b.effective_provider);
    assert_eq!(rec_a.effective_model, rec_b.effective_model);

    // TaskExecutor extras: the durable task row exists on A and matches
    // the direct path's row on B (the daemon's end-of-turn content sync
    // converges the row's goal to the session ledger — the run's own goal
    // is preserved in the linkage row).
    let task_a = ha.get_task(TaskId::new(1)).unwrap().expect("task row");
    let task_b = hb.get_task(TaskId::new(1)).unwrap().expect("task row");
    assert_eq!(task_a.goal, task_b.goal);
    assert_eq!(task_a.budget, task_b.budget);
    let facts = ha.memory_facts().unwrap();
    let run_row = facts
        .iter()
        .find(|(kind, key, _)| kind == TASK_RUN_ROW_KIND && key == &receipt_a.run_id)
        .expect("durable linkage row");
    let decoded = TaskRunRow::decode(&run_row.2).unwrap();
    assert_eq!(decoded.op_id, Some(op_a.raw()));
    assert_eq!(decoded.mode, TaskRunMode::InSession);
    assert_eq!(
        decoded.goal, goal,
        "the run's own goal lives in the linkage row"
    );
    assert_eq!(decoded.item_ids, vec!["a1".to_string()]);
}

#[tokio::test]
pub(crate) async fn refused_isolation_record_close_loss_is_marked_and_reconstructed() {
    // Adversarial (injected store fault): the refused-isolation admission
    // already recorded the durable Failed event, but the turn-record close
    // fails. The typed refusal outcome (receipt + linkage row) must be
    // preserved, the loss must surface through the agent's durable retry
    // channel, and the record must be closed by replay at the next open.
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(dir.path(), done_script());
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let req = request(
        "refused isolation",
        vec![wi("a1", WorkKind::Analysis, &[])],
        &env,
    );
    // Every turn-record update aborts: the close cannot land.
    env.manager
        .store()
        .sql_execute(
            "CREATE TRIGGER dw_test_fail_executor_record BEFORE UPDATE ON turn_record \
             BEGIN SELECT RAISE(ABORT, 'injected turn-record corruption'); END",
        )
        .unwrap();
    let receipt = env
        .executor
        .admit_refused_isolation(
            env.parent,
            &h,
            &req,
            ExecError::Oversized("bounded caps refused the candidate".into()),
            None,
            &mut false,
        )
        .expect("the typed refusal outcome survives the lost record close");
    assert_eq!(receipt.mode, TaskRunMode::InSession);
    assert!(!receipt.queued);
    // The original outcome is preserved: landed FailedRecoverable + linkage row.
    assert_eq!(
        state_of(&env, env.parent),
        faktor_core::state::AgentState::FailedRecoverable
    );
    let facts = h.memory_facts().unwrap();
    assert!(
        facts
            .iter()
            .any(|(kind, key, _)| kind == TASK_RUN_ROW_KIND && key == &receipt.run_id),
        "the durable linkage row is written despite the lost close"
    );
    // Surfaced through the agent's durable marker channel.
    let root = env.manager.store().root().to_path_buf();
    let marker_dir = root.join("durable-write-markers");
    let markers: Vec<std::path::PathBuf> = std::fs::read_dir(&marker_dir)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .collect();
    assert_eq!(
        markers.len(),
        1,
        "exactly one compensation marker: {markers:?}"
    );
    let marker: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&markers[0]).unwrap()).unwrap();
    assert_eq!(marker["status"], "pending");
    assert_eq!(marker["intent"]["write"], "finish_turn_record");
    assert!(
        marker["site"]
            .as_str()
            .is_some_and(|site| site.contains("admit_refused_isolation")),
        "the marker names the lost site: {marker}"
    );
    assert!(
        h.active_turn_record().unwrap().is_some(),
        "the lost close left the record active (the loss is not papered over)"
    );
    // Recovery: remove the injected corruption; the next open replays it.
    env.manager
        .store()
        .sql_execute("DROP TRIGGER dw_test_fail_executor_record")
        .unwrap();
    env.agent.recover().unwrap();
    assert!(
        h.active_turn_record().unwrap().is_none(),
        "the marker replayed the record close"
    );
    assert!(
        std::fs::read_dir(&marker_dir)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(true),
        "the consumed marker is removed"
    );
}

#[tokio::test]
pub(crate) async fn multi_item_task_spawns_real_children_and_completes() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(dir.path(), done_script());
    let req = request(
        "split the analysis",
        vec![
            wi("a", WorkKind::Analysis, &[]),
            wi("b", WorkKind::Analysis, &["a"]),
        ],
        &env,
    );
    let receipt = env
        .executor
        .start_task(env.parent, req)
        .expect("multi-item start");
    assert_eq!(receipt.mode, TaskRunMode::Orchestrated);
    assert_eq!(receipt.op_id, None);
    assert!(receipt.run_id.starts_with("run-"));
    // Point 7: the durable ROOT task row exists BEFORE the first child spawn
    // (unconditionally, no criteria/cap/contract required) — no orchestrated
    // run can ever settle without a root row to certify.
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    assert!(
        h.get_task(h.task_id().unwrap()).unwrap().is_some(),
        "every orchestrated run creates its durable root task row up front"
    );

    // Real children appear under the run and drive to terminal success.
    wait_until(
        || {
            OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &receipt.run_id)
                .map(|rows| rows.len() == 2 && rows.iter().all(|c| c.state.is_terminal()))
                .unwrap_or(false)
        },
        60,
    )
    .await;
    let rows = OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &receipt.run_id)
        .unwrap();
    assert_eq!(rows.len(), 2);
    for c in &rows {
        assert_eq!(c.state, crate::ChildState::Done);
        assert_ne!(c.session_id, 0, "real child session");
        assert_ne!(c.operation_id, 0, "child drive recorded its op");
        assert_eq!(
            c.ownership,
            faktor_session::child::ChildOwnership::ReadOnlyShared
        );
    }
    // Read-only children share the OWNER worktree (no isolated worktrees).
    assert_eq!(rows[0].worktree_id, rows[1].worktree_id);
    let owner_wt = env
        .manager
        .get_session(env.parent)
        .unwrap()
        .unwrap()
        .row()
        .unwrap()
        .worktree_id;
    assert_eq!(rows[0].worktree_id, owner_wt.raw());
    // Both children were really driven (provider calls >= 2) and the
    // executor slot freed itself.
    assert!(env.provider.count() >= 2, "driven {}", env.provider.count());
    wait_until(|| env.executor.active_run().is_none(), 240).await;
    // The verification-disabled env parks the run in the explicit Verifying
    // state (never Pending/Running); VerifiedComplete is exercised by the
    // real-tool adversarial suite.
    let state = h.get_task(h.task_id().unwrap()).unwrap().unwrap().state;
    assert_eq!(state, TaskState::Verifying, "root task walked to Verifying");
}

#[tokio::test]
pub(crate) async fn start_refuses_when_a_live_run_was_left_by_a_crashed_executor() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(dir.path(), done_script());
    let parent_row = env
        .manager
        .get_session(env.parent)
        .unwrap()
        .unwrap()
        .row()
        .unwrap();
    // Crashed-executor simulation: execute_task with the BeforeDrive seam
    // leaves a durable child row in Running state under the plan row.
    let plan = TaskPlan {
        goal: "crashed run".into(),
        non_goals: vec![],
        constraints: vec![],
        work_items: vec![wi("a", WorkKind::Analysis, &[])],
    };
    let mut spec = crate::runtime::ChildSpec::new("a");
    spec.child_caps = read_caps();
    spec.task_caps = read_caps();
    let config = crate::runtime::ExecConfig {
        run_id: "run-crash".into(),
        ceilings: crate::runtime::Ceilings::default(),
        parent_caps: read_caps(),
        provider: "fake".into(),
        default_model: "m".into(),
        isolated_root: env.isolated_root.clone(),
        crash_seam: Some(CrashSeam::BeforeDrive),
    };
    let res = tokio::time::timeout(
        Duration::from_secs(30),
        env.orchestrator.execute_task(
            plan,
            crate::runtime::OwnerContext {
                parent_session: env.parent,
                workspace_id: parent_row.workspace_id.raw(),
                worktree_id: parent_row.worktree_id.raw(),
                root: env.owner_root.clone(),
            },
            config,
            &[spec],
        ),
    )
    .await
    .expect("the seam fires fast");
    assert!(
        matches!(res, Err(crate::runtime::ExecError::InjectedCrashSeam(_))),
        "{res:?}"
    );

    // A new task on the same session must REFUSE (typed Conflict naming the
    // live run) instead of clobbering the mirror of the crashed one.
    let req = request(
        "new task over crash residue",
        vec![
            wi("x", WorkKind::Analysis, &[]),
            wi("y", WorkKind::Analysis, &["x"]),
        ],
        &env,
    );
    let err = env
        .executor
        .start_task(env.parent, req.clone())
        .expect_err("live residue blocks a new run");
    assert!(
        matches!(err, crate::runtime::ExecError::Conflict(_)),
        "{err:?}"
    );
    assert!(err.to_string().contains("run-crash"), "{err}");

    // resume_run re-attaches the crashed run and drives it to completion.
    env.executor
        .resume_run(
            env.parent,
            "run-crash",
            crate::runtime::Ceilings::default(),
            read_caps(),
            None,
        )
        .expect("resume accepted");
    wait_until(
        || {
            OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, "run-crash")
                .map(|rows| {
                    rows.first()
                        .is_some_and(|c| c.state == crate::ChildState::Done)
                })
                .unwrap_or(false)
        },
        60,
    )
    .await;

    // After the resumed run is terminal the session accepts new tasks.
    let receipt = env
        .executor
        .start_task(env.parent, req)
        .expect("new run after resume");
    assert_eq!(receipt.mode, TaskRunMode::Orchestrated);
}

#[tokio::test]
pub(crate) async fn resume_run_after_crash_between_assignments_and_first_spawn_reuses_durable_ids()
{
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(dir.path(), done_script());
    // Crash the executor EXACTLY between the atomic assignment commit and
    // the first child spawn (the AfterAssignmentsPersisted seam).
    let mut req = request(
        "crash after compile",
        vec![
            wi("a", WorkKind::Analysis, &[]),
            wi("b", WorkKind::Analysis, &[]),
        ],
        &env,
    );
    req.crash_seam = Some(CrashSeam::AfterAssignmentsPersisted);
    let receipt = env
        .executor
        .start_task(env.parent, req)
        .expect("run starts");
    assert_eq!(receipt.mode, TaskRunMode::Orchestrated);
    let run_id = receipt.run_id.clone();
    // Durable assignments exist; NO child may have spawned; the crashed
    // executor's slot is free again.
    wait_until(
        || {
            !OrchestratorRuntime::assignment_rows(env.manager.clone(), env.parent, &run_id)
                .unwrap_or_default()
                .is_empty()
                && OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &run_id)
                    .map(|rows| rows.is_empty())
                    .unwrap_or(false)
        },
        30,
    )
    .await;
    wait_until(|| env.executor.active_run().is_none(), 180).await;
    let assignments =
        OrchestratorRuntime::assignment_rows(env.manager.clone(), env.parent, &run_id).unwrap();
    assert_eq!(assignments.len(), 2);
    // The assignment-backed residue is LIVE: a new task must REFUSE until
    // the crashed run is resumed (its durable ids must not be orphaned).
    let err = env
        .executor
        .start_task(
            env.parent,
            request(
                "second run over residue",
                vec![wi("x", WorkKind::Analysis, &[])],
                &env,
            ),
        )
        .expect_err("assignment-backed residue blocks a new run");
    assert!(
        matches!(err, crate::runtime::ExecError::Conflict(_)),
        "{err:?}"
    );
    assert!(err.to_string().contains(&run_id), "{err}");
    // resume_run accepts the run even though it has NO child rows yet and
    // re-spawns every item under its DURABLE child id (no re-mint).
    env.executor
        .resume_run(
            env.parent,
            &run_id,
            crate::runtime::Ceilings::default(),
            read_caps(),
            None,
        )
        .expect("resume accepted");
    wait_until(
        || {
            OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &run_id)
                .map(|rows| {
                    rows.len() == 2 && rows.iter().all(|c| c.state == crate::ChildState::Done)
                })
                .unwrap_or(false)
        },
        90,
    )
    .await;
    let rows =
        OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &run_id).unwrap();
    let after =
        OrchestratorRuntime::assignment_rows(env.manager.clone(), env.parent, &run_id).unwrap();
    assert_eq!(
        after, assignments,
        "assignment rows must never be re-minted"
    );
    for a in &assignments {
        let row = rows
            .iter()
            .find(|r| r.item_id == a.item_id)
            .expect("child row per assigned item");
        assert_eq!(row.child_id, a.child_id, "spawn must reuse the durable id");
    }
    // Terminal residue frees the session for new tasks (the new run spawns
    // its own fresh children and completes).
    let fresh = env
        .executor
        .start_task(
            env.parent,
            request(
                "fresh run",
                vec![
                    wi("p", WorkKind::Analysis, &[]),
                    wi("q", WorkKind::Analysis, &[]),
                ],
                &env,
            ),
        )
        .expect("session accepts new tasks after resume");
    assert_eq!(fresh.mode, TaskRunMode::Orchestrated);
    wait_until(
        || {
            OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &fresh.run_id)
                .map(|rows| {
                    rows.len() == 2 && rows.iter().all(|c| c.state == crate::ChildState::Done)
                })
                .unwrap_or(false)
        },
        90,
    )
    .await;
}

#[tokio::test]
pub(crate) async fn second_orchestrated_run_is_refused_while_one_is_active() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    // A gate provider keeps the first run mid-flight so the single
    // execution slot is observably occupied.
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let gated = Arc::new(GatedProvider {
        caps: ModelCapabilities {
            tools: true,
            ..Default::default()
        },
        gate: Arc::new(tokio::sync::Notify::new()),
        open: Arc::new(AtomicUsize::new(0)),
        request_count: AtomicUsize::new(0),
    });
    let mut registry = ProviderRegistry::new();
    registry.try_register(gated.clone()).unwrap();
    let agent = build_agent(manager.clone(), registry);
    let owner_root = dir.path().join("owner");
    std::fs::create_dir_all(&owner_root).unwrap();
    let ws = manager
        .create_workspace(owner_root.to_str().unwrap())
        .unwrap();
    let wt = WorktreeId::new(
        manager
            .put_worktree(ws, owner_root.to_str().unwrap(), "main")
            .unwrap() as u64,
    );
    let parent = manager
        .create_session(ws, "gated", "fake", "m")
        .unwrap()
        .id();
    manager.adopt_identity(parent, wt, TaskId::new(1)).unwrap();
    let isolated = dir.path().join("isolated");
    std::fs::create_dir_all(&isolated).unwrap();
    let orchestrator = OrchestratorRuntime::new(manager.clone(), agent.clone());
    let executor = TaskExecutor::new_owner_direct_for_test_harness(
        &orchestrator,
        manager.clone(),
        agent.clone(),
    );

    let req = || TaskRunRequest {
        goal: "gated run".into(),
        work_items: vec![
            wi("a", WorkKind::Analysis, &[]),
            wi("b", WorkKind::Analysis, &[]),
        ],
        parent_caps: read_caps(),
        isolated_root: isolated.clone(),
        ..Default::default()
    };
    let first = executor
        .start_task(parent, req())
        .expect("first run starts");
    // Wait until the first child exists and its drive is parked on the gate
    // (mid-flight).
    wait_until(
        || {
            OrchestratorRuntime::registry_rows(manager.clone(), parent, &first.run_id)
                .map(|rows| !rows.is_empty())
                .unwrap_or(false)
        },
        30,
    )
    .await;
    wait_until(|| gated.count() >= 1, 180).await;

    // A second orchestrated start is refused while the first is active
    // (typed Conflict — the runtime executes one run at a time).
    let err = executor
        .start_task(parent, req())
        .expect_err("busy refusal");
    assert!(
        matches!(err, crate::runtime::ExecError::Conflict(_)),
        "{err:?}"
    );
    assert!(err.to_string().contains(&first.run_id), "{err}");
    // resume_run of the ACTIVE run is refused too (no double drive).
    let err2 = executor
        .resume_run(
            parent,
            &first.run_id,
            crate::runtime::Ceilings::default(),
            read_caps(),
            None,
        )
        .expect_err("double drive refused");
    assert!(err2.to_string().contains("already being driven"), "{err2}");

    // Releasing the gate lets the first run finish and free the slot.
    gated.open();
    wait_until(
        || {
            OrchestratorRuntime::registry_rows(manager.clone(), parent, &first.run_id)
                .map(|rows| rows.iter().all(|c| c.state.is_terminal()))
                .unwrap_or(false)
        },
        60,
    )
    .await;
    assert!(executor.active_run().is_none());
}

#[tokio::test]
pub(crate) async fn runs_of_two_parent_sessions_proceed_concurrently_past_a_provider_barrier() {
    let _heavy = heavy_guard();
    // (audits 7/8/21/22) TaskExecutor runs are keyed per RUN and indexed
    // per PARENT SESSION: one run per parent session, while runs of
    // DIFFERENT sessions proceed concurrently through the runtime's
    // run-scoped mirrors. A barrier inside the shared provider proves it:
    // both parents' children must be INSIDE their model call before either
    // releases — impossible under any global serialization.
    let dir = tempfile::tempdir().unwrap();
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let barrier = Arc::new(BarrierProvider {
        caps: ModelCapabilities {
            tools: true,
            ..Default::default()
        },
        gate: Arc::new(tokio::sync::Notify::new()),
        open: Arc::new(AtomicUsize::new(0)),
        entered: AtomicUsize::new(0),
        request_count: AtomicUsize::new(0),
    });
    let mut registry = ProviderRegistry::new();
    registry.try_register(barrier.clone()).unwrap();
    let agent = build_agent(manager.clone(), registry);
    // Two independent owner sessions (worktrees + identities) over the SAME
    // manager/agent/executor.
    let make_parent = |dir: &std::path::Path| {
        std::fs::create_dir_all(dir).unwrap();
        let ws = manager.create_workspace(dir.to_str().unwrap()).unwrap();
        let wt = WorktreeId::new(
            manager
                .put_worktree(ws, dir.to_str().unwrap(), "main")
                .unwrap() as u64,
        );
        let parent = manager
            .create_session(ws, "task-owner", "fake", "m")
            .unwrap()
            .id();
        manager.adopt_identity(parent, wt, TaskId::new(1)).unwrap();
        parent
    };
    let p1 = make_parent(&dir.path().join("owner-1"));
    let p2 = make_parent(&dir.path().join("owner-2"));
    let isolated = dir.path().join("isolated");
    std::fs::create_dir_all(&isolated).unwrap();
    let orchestrator = OrchestratorRuntime::new(manager.clone(), agent.clone());
    let executor = TaskExecutor::new_owner_direct_for_test_harness(
        &orchestrator,
        manager.clone(),
        agent.clone(),
    );

    let req_for = |isolated: std::path::PathBuf| TaskRunRequest {
        goal: "parallel analysis run".into(),
        // Two items keep dispatch ORCHESTRATED; b waits for a, so exactly
        // one child per run is mid-call at the barrier at a time.
        work_items: vec![
            wi("a", WorkKind::Analysis, &[]),
            wi("b", WorkKind::Analysis, &["a"]),
        ],
        parent_caps: read_caps(),
        isolated_root: isolated,
        ..Default::default()
    };
    let first = executor
        .start_task(p1, req_for(isolated.clone()))
        .expect("session 1 run starts");
    // The first run's child must be parked INSIDE its model call.
    wait_until(|| barrier.entered() >= 1, 300).await;
    // The second parent's run starts WHILE session 1's run is active — the
    // old single global slot refused this with a typed Conflict.
    let second = executor.start_task(p2, req_for(isolated.clone())).expect(
        "session 2 run starts while session 1 is active: cross-session runs are concurrent",
    );
    assert_ne!(first.run_id, second.run_id);
    // BOTH children are inside their model call while NEITHER has released:
    // the observable witness that no executor-level lock serializes them.
    wait_until(|| barrier.entered() >= 2, 300).await;
    assert!(
        executor.active_runs().len() == 2,
        "both runs active concurrently: {:?}",
        executor.active_runs()
    );
    // The per-parent index still keeps ONE run per parent session: a third
    // run of session 1 is refused while its first run is active.
    let err = executor
        .start_task(p1, req_for(isolated.clone()))
        .expect_err("one orchestrated run per parent session");
    assert!(matches!(err, ExecError::Conflict(_)), "{err:?}");
    assert!(
        err.to_string().contains(&first.run_id),
        "refusal names the active run of the same session: {err}"
    );
    // Release both: each run drives its remaining waves to completion.
    barrier.open();
    for (parent, run) in [(p1, &first.run_id), (p2, &second.run_id)] {
        wait_until(
            || {
                OrchestratorRuntime::registry_rows(manager.clone(), parent, run)
                    .map(|rows| !rows.is_empty() && rows.iter().all(|c| c.state.is_terminal()))
                    .unwrap_or(false)
            },
            300,
        )
        .await;
    }
    wait_until(|| executor.active_runs().is_empty(), 240).await;
}

#[tokio::test]
pub(crate) async fn per_item_ownership_lands_on_the_durable_assignment_rows() {
    let _heavy = heavy_guard();
    // (audits 7/8/21/22, work-entry unification) A MIXED request (read-only
    // Analysis → mutating Implementation with its own path set) carries the
    // ownership ON THE ITEMS. The item's spec is persisted on the wave-A3
    // rows and the spawned child rows carry the compiled mode + paths.
    let dir = tempfile::tempdir().unwrap();
    let scripts: Vec<Vec<ScriptedResponse>> = vec![
        vec![
            ScriptedResponse::Text("analyzed".into()),
            ScriptedResponse::End,
        ],
        vec![
            ScriptedResponse::Text("implemented".into()),
            ScriptedResponse::End,
        ],
    ];
    let env = open_env(dir.path(), scripts);
    std::fs::create_dir_all(env.owner_root.join("src")).unwrap();
    let req = request(
        "mixed ownership run",
        vec![
            wi("analyze", WorkKind::Analysis, &[]),
            path_item(
                "implement",
                WorkKind::Implementation,
                &["analyze"],
                &["src/m.rs"],
            ),
        ],
        &env,
    );
    let receipt = env
        .executor
        .start_task(env.parent, req)
        .expect("mixed run with per-item ownership starts");
    wait_until(
        || {
            OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &receipt.run_id)
                .map(|rows| rows.len() == 2 && rows.iter().all(|c| c.state.is_terminal()))
                .unwrap_or(false)
        },
        120,
    )
    .await;
    let rows = OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &receipt.run_id)
        .unwrap();
    let row_of = |id: &str| rows.iter().find(|c| c.item_id == id).unwrap();
    assert_eq!(row_of("analyze").ownership, ChildOwnership::ReadOnlyShared);
    assert_eq!(
        row_of("implement").ownership,
        ChildOwnership::ExclusivePaths
    );
    assert_eq!(
        row_of("implement").ownership_paths,
        vec!["src/m.rs".to_string()]
    );
    let assignments =
        OrchestratorRuntime::assignment_rows(env.manager.clone(), env.parent, &receipt.run_id)
            .unwrap();
    let a_of = |id: &str| assignments.iter().find(|a| a.item_id == id).unwrap();
    assert_eq!(a_of("analyze").ownership, OwnershipSpec::NoWrites);
    assert_eq!(
        a_of("implement").ownership,
        OwnershipSpec::Paths {
            paths: vec!["src/m.rs".to_string()]
        }
    );
}

#[tokio::test]
pub(crate) async fn overlapping_or_write_capable_per_item_requests_are_refused_at_compile() {
    let _heavy = heavy_guard();
    // (audits 7/8/21/22, work-entry unification) Executor-level refusals
    // BEFORE any durable row, decided from the ITEMS alone: two mutating
    // items whose path sets overlap (even behind a dependency edge) and a
    // read-only item handed write capability are both rejected by the
    // per-item compile.
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(dir.path(), vec![vec![ScriptedResponse::End]]);
    // (a) Overlapping mutating path sets — b depends on a, so the overlap
    // could never be live; disjointness still spans ALL mutating items.
    let req = request(
        "overlapping",
        vec![
            path_item("a", WorkKind::Implementation, &[], &["src"]),
            path_item("b", WorkKind::Implementation, &["a"], &["src/a.rs"]),
        ],
        &env,
    );
    let err = env
        .executor
        .start_task(env.parent, req)
        .expect_err("overlapping mutating path sets must be refused");
    assert!(
        matches!(err, ExecError::InvalidPlan(_))
            && err.to_string().contains("overlapping write ownership"),
        "{err:?}"
    );
    // (b) A read-only item with write ownership is refused (never a write
    // capability on a read-only item — through ANY channel).
    let req = request(
        "write-capable read-only",
        vec![
            WorkItem::with_ownership(
                "analyze",
                "read",
                WorkKind::Analysis,
                OwnershipSpec::Paths {
                    paths: vec!["src".to_string()],
                },
            ),
            path_item(
                "impl",
                WorkKind::Implementation,
                &["analyze"],
                &["src/impl.rs"],
            ),
        ],
        &env,
    );
    let err = env
        .executor
        .start_task(env.parent, req)
        .expect_err("read-only items can never receive write capability");
    assert!(
        matches!(err, ExecError::InvalidPlan(_)) && err.to_string().contains("read-only work item"),
        "{err:?}"
    );
    // Nothing durable was written by either refusal.
    let handle = env.manager.get_session(env.parent).unwrap().unwrap();
    let facts = handle.memory_facts().unwrap();
    assert!(
        !facts.iter().any(|(kind, _, _)| matches!(
            kind.as_str(),
            crate::runtime::PLAN_ROW_KIND
                | crate::runtime::ASSIGNMENT_ROW_KIND
                | crate::runtime::REGISTRY_ROW_KIND
        )),
        "refused plans leave no durable orchestration rows"
    );
}

#[tokio::test]
pub(crate) async fn resume_run_retries_a_failed_child_from_a_durable_row() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    // First provider stream dies permanently; the retry's re-drive succeeds.
    let scripts: Vec<Vec<ScriptedResponse>> = vec![
        vec![ScriptedResponse::Die(ProviderError::new(
            faktor_provider::ProviderErrorKind::Malformed,
            "injected permanent failure",
        ))],
        vec![
            ScriptedResponse::Text("recovered".into()),
            ScriptedResponse::End,
        ],
        vec![ScriptedResponse::End],
    ];
    let env = open_env(dir.path(), scripts);
    // Two items keep the dispatch ORCHESTRATED (a real child session) while
    // only the "a" item actually spawns: "auto" completes without a child.
    let mut req = request(
        "failing run",
        vec![
            wi("a", WorkKind::Analysis, &[]),
            wi("auto", WorkKind::Analysis, &[]),
        ],
        &env,
    );
    // "auto" completes without a child; only "a" spawns.
    req.auto_items = vec!["auto".to_string()];
    let receipt = env
        .executor
        .start_task(env.parent, req.clone())
        .expect("start");
    // Wait for the run's first drive to fail (permanent provider error).
    let mut waited = 0;
    loop {
        let rows =
            OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &receipt.run_id)
                .unwrap_or_default();
        if rows
            .first()
            .is_some_and(|c| c.state == crate::ChildState::Failed)
        {
            break;
        }
        waited += 1;
        assert!(waited < 240, "run1 never Failed: {rows:?}");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    // resume_run WITHOUT a pending Retry row does NOT blindly re-run: the
    // child stays Failed (never an automatic infinite retry loop).
    env.executor
        .resume_run(
            env.parent,
            &receipt.run_id,
            crate::runtime::Ceilings::default(),
            read_caps(),
            None,
        )
        .expect("resume accepted");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let rows = OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &receipt.run_id)
        .unwrap();
    assert_eq!(rows[0].state, crate::ChildState::Failed);
    // A human retry enqueues the durable Retry row; resume_run admits the
    // re-drive exactly once and completes.
    env.orchestrator
        .retry_child("child-0")
        .expect("retry enqueued");
    env.executor
        .resume_run(
            env.parent,
            &receipt.run_id,
            crate::runtime::Ceilings::default(),
            read_caps(),
            None,
        )
        .expect("retry resume");
    wait_until(
        || {
            OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &receipt.run_id)
                .map(|rows| {
                    rows.first()
                        .is_some_and(|c| c.state == crate::ChildState::Done)
                })
                .unwrap_or(false)
        },
        60,
    )
    .await;
    let rows = OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &receipt.run_id)
        .unwrap();
    assert_eq!(rows[0].state, crate::ChildState::Done);
    let session = env
        .manager
        .get_session(SessionId::new(rows[0].session_id))
        .unwrap()
        .unwrap();
    let ctl = session.orchestrator_ctl_all().unwrap();
    let retry = ctl
        .iter()
        .find(|r| matches!(r.control, faktor_session::child::ChildControl::Retry))
        .expect("durable retry row");
    assert!(
        retry.applied(),
        "retry decision durable before the re-drive"
    );
    // A second resume of a fully terminal run is refused (nothing to drive).
    let err = env
        .executor
        .resume_run(
            env.parent,
            &receipt.run_id,
            crate::runtime::Ceilings::default(),
            read_caps(),
            None,
        )
        .expect_err("nothing to resume");
    assert!(err.to_string().contains("nothing to resume"), "{err}");
}

#[test]
pub(crate) fn hostile_requests_are_rejected_before_any_write() {
    let dir = tempfile::tempdir().unwrap();
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let ws = manager.create_workspace("/w").unwrap();
    let parent = manager
        .create_session(ws, "hostile", "fake", "m")
        .unwrap()
        .id();
    let agent = build_agent(manager.clone(), ProviderRegistry::new());
    let orch = OrchestratorRuntime::new(manager.clone(), agent.clone());
    let executor =
        TaskExecutor::new_owner_direct_for_test_harness(&orch, manager.clone(), agent.clone());
    let isolated = dir.path().join("isolated");

    let mut req = TaskRunRequest {
        isolated_root: isolated.clone(),
        work_items: vec![wi("a", WorkKind::Analysis, &[])],
        ..Default::default()
    };

    req.goal = "  ".into();
    let err = req.validate().expect_err("blank goal rejected");
    assert!(err.to_string().contains("goal is empty"));

    req.goal = "x".repeat(crate::MAX_GOAL_CHARS + 1);
    assert!(matches!(
        req.validate().expect_err("overlong goal rejected"),
        crate::runtime::ExecError::Oversized(_)
    ));

    req.goal = "fine".into();
    req.work_items = vec![];
    let err = req.validate().expect_err("no work items rejected");
    assert!(err.to_string().contains("at least one work item"));

    req.work_items = vec![wi("a", WorkKind::Analysis, &[])];
    req.model = Some("m".repeat(129));
    assert!(matches!(
        req.validate().expect_err("overlong model rejected"),
        crate::runtime::ExecError::Oversized(_)
    ));
    req.model = None;

    // A single MUTATING item is legal: it drives the session's own
    // worktree (the current normal path), never a spawn.
    req.work_items = vec![wi("a", WorkKind::Implementation, &[])];
    req.validate()
        .expect("single mutating item = the session's own drive");
    // ... and a multi-item MUTATING plan with NO isolated_root is legal:
    // the DAEMON allocates the candidate root itself (a client never
    // supplies a filesystem path).
    req.work_items = vec![
        path_item("a", WorkKind::Implementation, &[], &["src/a.rs"]),
        path_item("b", WorkKind::Implementation, &["a"], &["src/b.rs"]),
    ];
    req.isolated_root = std::path::PathBuf::new();
    req.validate()
        .expect("multi-item mutating plans allocate their own root");
    req.isolated_root = isolated.clone();
    // A mutating item that still carries NoWrites (missing ownership) is
    // InvalidPlan — nothing defaults a write authority onto it.
    req.work_items = vec![
        WorkItem::with_ownership(
            "a",
            "impl",
            WorkKind::Implementation,
            OwnershipSpec::NoWrites,
        ),
        path_item("b", WorkKind::Implementation, &["a"], &["src/b.rs"]),
    ];
    let err = req
        .validate()
        .expect_err("missing mutator ownership rejected");
    assert!(
        err.to_string().contains("requires write ownership"),
        "{err}"
    );
    // Mixed read-only + mutating items are valid exactly when each item
    // carries its kind-correct explicit ownership.
    req.work_items = vec![
        path_item("a", WorkKind::Implementation, &[], &["src/a.rs"]),
        wi("b", WorkKind::Analysis, &["a"]),
    ];
    req.validate()
        .expect("per-item ownership mixed plan validates");

    // Unknown session: typed NotFound, nothing written.
    let ok = TaskRunRequest {
        goal: "ok".into(),
        work_items: vec![wi("a", WorkKind::Analysis, &[])],
        isolated_root: isolated,
        ..Default::default()
    };
    let err = executor
        .start_task(faktor_core::id::SessionId::new(9999), ok.clone())
        .unwrap_err();
    assert!(matches!(err, crate::runtime::ExecError::NotFound(_)));

    // An orchestrated child session refuses task starts.
    let child = manager
        .create_child_session(
            parent,
            ws,
            WorktreeId::new(1),
            TaskId::new(1),
            "fake",
            "m",
            "child",
            faktor_session::child::ChildOwnership::ReadOnlyShared,
        )
        .unwrap();
    let err = executor.start_task(child.id(), ok).unwrap_err();
    assert!(
        matches!(err, crate::runtime::ExecError::InvalidState(_)),
        "{err:?}"
    );
}

// =========================================================== P0-48 shadowed
// single-agent mutating runs (shadow mutation roots).
//
// The drive itself is the REAL daemon drive (scripted providers, no
// network). The "shadowed drive writes" below are staged as direct writes
// INTO the shadow root — the exact operation the session's file consumers
// perform once they resolve `SessionManager::active_root` (the next-wave
// re-pointing); today those consumers live in the agent crate and resolve
// the durable workspace root directly, so the wiring is verified against
// the durable shadow machinery (begin/finalize/commit) instead.

use faktor_core::state::{TaskState, TaskTransition, VerificationStatus};
use faktor_session::ShadowRowState;

pub(crate) fn seed_owner(root: &std::path::Path) {
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("a.txt"), b"base-alpha").unwrap();
    std::fs::write(root.join("sub/b.txt"), b"base-beta").unwrap();
}

pub(crate) fn shadow_row_of(env: &Env) -> faktor_session::ShadowRow {
    env.manager
        .shadow_row(env.parent)
        .unwrap()
        .expect("an active shadow row exists")
}

pub(crate) fn owner_bytes(env: &Env, rel: &str) -> Vec<u8> {
    std::fs::read(env.owner_root.join(rel)).unwrap()
}

/// The durable "shadowed drive write": stage content inside the shadow root
/// (exactly where `SessionManager::active_root` re-points next wave).
pub(crate) fn shadow_drive_write(env: &Env, rel: &str, bytes: &[u8]) {
    let row = shadow_row_of(env);
    let dst = std::path::PathBuf::from(&row.root).join(rel);
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(dst, bytes).unwrap();
}

pub(crate) fn mutating_request(env: &Env, goal: &str) -> TaskRunRequest {
    TaskRunRequest {
        goal: goal.to_string(),
        work_items: vec![wi("impl", WorkKind::Implementation, &[])],
        parent_caps: read_caps(),
        isolated_root: env.isolated_root.clone(),
        ..Default::default()
    }
}

/// A gated shadowed fixture: the provider parks mid-stream until released,
/// so the drive is deterministically mid-flight while assertions run.
pub(crate) struct GatedShadowFix {
    pub(crate) manager: Arc<SessionManager>,
    pub(crate) executor: Arc<TaskExecutor>,
    pub(crate) gated: Arc<GatedProvider>,
    pub(crate) parent: SessionId,
    pub(crate) owner_root: std::path::PathBuf,
    pub(crate) shadows: Arc<ShadowRoots>,
}

/// The historical gated fixture: verification disabled (the drive can never
/// certify) with the plain text seed. Used by the crash-residue and cancel
/// tests, which never integrate.
pub(crate) fn open_gated_shadow(root: &std::path::Path) -> GatedShadowFix {
    open_gated_shadow_with(root, faktor_agent::VerificationService::disabled(), false)
}

/// A gated fixture that CAN integrate: a fake-ok verifier + a seed Rust
/// project so the executor's candidate verification derives a passing
/// check. Used by the conflict-then-resolve pipeline test.
pub(crate) fn open_gated_shadow_verified(root: &std::path::Path) -> GatedShadowFix {
    open_gated_shadow_with(root, faktor_agent::VerificationService::fake_ok(), true)
}

pub(crate) fn open_gated_shadow_with(
    root: &std::path::Path,
    verification: Arc<faktor_agent::VerificationService>,
    rust_project: bool,
) -> GatedShadowFix {
    let manager = SessionManager::open(root.join("store"), root.join("cas"), true).unwrap();
    let gated = Arc::new(GatedProvider {
        caps: ModelCapabilities {
            tools: true,
            ..Default::default()
        },
        gate: Arc::new(tokio::sync::Notify::new()),
        open: Arc::new(AtomicUsize::new(0)),
        request_count: AtomicUsize::new(0),
    });
    let mut registry = ProviderRegistry::new();
    registry.try_register(gated.clone()).unwrap();
    let agent = build_agent_with_verification(manager.clone(), registry, verification);
    let owner_root = root.join("owner");
    std::fs::create_dir_all(&owner_root).unwrap();
    seed_owner(&owner_root);
    if rust_project {
        std::fs::create_dir_all(owner_root.join("src")).unwrap();
        std::fs::write(
            owner_root.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(
            owner_root.join("src/lib.rs"),
            "pub fn value() -> u64 {\n    let base_amount: u64 = 10;\n    let increment: u64 = 32;\n    base_amount.saturating_add(increment)\n}\n",
        )
        .unwrap();
    }
    let ws = manager
        .create_workspace(owner_root.to_str().unwrap())
        .unwrap();
    let wt = WorktreeId::new(
        manager
            .put_worktree(ws, owner_root.to_str().unwrap(), "main")
            .unwrap() as u64,
    );
    let parent = manager
        .create_session(ws, "gated-shadow", "fake", "m")
        .unwrap()
        .id();
    manager.adopt_identity(parent, wt, TaskId::new(1)).unwrap();
    let orchestrator = OrchestratorRuntime::new(manager.clone(), agent.clone());
    let shadows = ShadowRoots::new(manager.clone(), root.join("shadows")).unwrap();
    let executor = TaskExecutor::new(
        &orchestrator,
        manager.clone(),
        agent.clone(),
        shadows.clone(),
    );
    GatedShadowFix {
        manager,
        executor,
        gated,
        parent,
        owner_root,
        shadows,
    }
}

#[tokio::test]
pub(crate) async fn shadowed_mutating_run_writes_never_reach_user_checkout_until_verified_commit() {
    let _heavy = heavy_guard();
    // (a)+(b) over the REAL executor: a shadowed mutating run begins a
    // durable shadow before its drive; staged writes live in the shadow
    // while the user checkout stays byte-identical. With no verifier
    // configured the executor cannot verify the candidate, so the task can
    // never complete: VerifiedComplete is the CONSEQUENCE of a successful
    // owner landing, and the drive's shadow-world certification is never
    // the permission to attempt it.
    let dir = tempfile::tempdir().unwrap();
    let env = open_shadow_env(dir.path(), done_script());
    seed_owner(&env.owner_root);
    let receipt = env
        .executor
        .start_task(env.parent, mutating_request(&env, "implement the change"))
        .expect("shadowed single-item start");
    assert_eq!(receipt.mode, TaskRunMode::InSession);
    // The shadow began synchronously BEFORE the submit.
    let row = shadow_row_of(&env);
    assert_eq!(row.state, ShadowRowState::Active);
    let shadow_dir = std::path::PathBuf::from(&row.root);
    assert!(shadow_dir.is_dir());
    assert_eq!(
        env.manager.active_root(env.parent).unwrap(),
        Some(shadow_dir.clone()),
        "active_root re-points the session at the shadow while live"
    );
    // The drive ends (scripted text, no tools).
    wait_until(
        || state_of(&env, env.parent) == faktor_core::state::AgentState::ReadyForNextTurn,
        30,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        std::fs::read(env.owner_root.join("a.txt")).unwrap(),
        b"base-alpha"
    );
    assert_eq!(row.state, ShadowRowState::Active);
    // Stage the shadowed drive's writes AFTER the drive (what a
    // shadow-aware verification run would have produced).
    shadow_drive_write(&env, "a.txt", b"implemented alpha");
    shadow_drive_write(&env, "new-file.txt", b"implemented new file");
    assert_eq!(
        std::fs::read(env.owner_root.join("a.txt")).unwrap(),
        b"base-alpha"
    );
    assert!(!env.owner_root.join("new-file.txt").exists());
    // Even a verifier seam "certifying" the shadow world cannot complete:
    // a proof without a tree hash predates landing, and a managed live
    // shadow requires the finalized owner integration.
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    let err = complete_shadow_world_or_refuse(&env.manager, env.parent)
        .expect_err("shadow-world certification must be refused");
    assert!(
        matches!(
            err,
            faktor_session::TaskError::IntegrationRecordMissing { .. }
        ),
        "{err}"
    );
    assert_ne!(
        h.get_task(task_id).unwrap().unwrap().state,
        TaskState::VerifiedComplete
    );
    // The executor's own settlement cannot verify either (no verifier):
    // nothing lands, nothing completes, the shadow stays live.
    let outcome = env
        .executor
        .settle_run(RunSettlement::InSession {
            parent: env.parent,
            run_id: format!("tx-session-{}", env.parent.raw()),
        })
        .await
        .expect("settlement is a typed outcome, not a failure");
    assert!(!outcome.completed);
    assert_eq!(
        std::fs::read(env.owner_root.join("a.txt")).unwrap(),
        b"base-alpha"
    );
    assert!(!env.owner_root.join("new-file.txt").exists());
    assert_eq!(
        env.manager.shadow_row(env.parent).unwrap().unwrap().state,
        ShadowRowState::Active
    );
    assert_ne!(
        h.get_task(task_id).unwrap().unwrap().state,
        TaskState::VerifiedComplete,
        "VerifiedComplete is impossible unless the owner integration succeeded"
    );
}

#[tokio::test]
pub(crate) async fn mid_drive_isolation_and_conflict_surfaces_integration_blocked_then_resolves() {
    let _heavy = heavy_guard();
    // (a)+(c) with the drive parked mid-flight: while the drive is live the
    // user checkout is byte-identical; an external user edit during the
    // drive conflicts at the OWNER LANDING — nothing lands, the shadow is
    // retained (`IntegrationBlocked`) with the durable conflict list, and
    // the task stays NON-TERMINAL: the drive's shadow-world completion is
    // refused by the session gate, so VerifiedComplete is impossible until
    // the landing succeeds. Resolving the drift lets the bounded watcher
    // integrate and complete.
    let dir = tempfile::tempdir().unwrap();
    let fix = open_gated_shadow_verified(dir.path());
    let receipt = fix
        .executor
        .start_task(
            fix.parent,
            TaskRunRequest {
                goal: "gate-shadowed implementation".into(),
                work_items: vec![wi("impl", WorkKind::Implementation, &[])],
                parent_caps: read_caps(),
                isolated_root: dir.path().join("isolated"),
                ..Default::default()
            },
        )
        .expect("shadowed start");
    assert_eq!(receipt.mode, TaskRunMode::InSession);
    let row = fix
        .manager
        .shadow_row(fix.parent)
        .unwrap()
        .expect("row at begin");
    let shadow_dir = std::path::PathBuf::from(&row.root);
    // Park the drive mid-flight and write into the shadow while it runs.
    wait_until(|| fix.gated.count() >= 1, 180).await;
    std::fs::write(
        shadow_dir.join("src/lib.rs"),
        b"pub fn value() -> u64 {\n    let base_amount: u64 = 11;\n    let increment: u64 = 31;\n    base_amount.saturating_add(increment)\n}\n",
    )
    .unwrap();
    assert_eq!(
        std::fs::read(fix.owner_root.join("src/lib.rs")).unwrap(),
        b"pub fn value() -> u64 {\n    let base_amount: u64 = 10;\n    let increment: u64 = 32;\n    base_amount.saturating_add(increment)\n}\n",
        "user checkout byte-identical MID-drive"
    );
    assert_eq!(
        fix.manager.active_root(fix.parent).unwrap(),
        Some(shadow_dir.clone()),
        "active_root reports the shadow root while the drive is live"
    );
    // The user edits the same file externally during the drive.
    std::fs::write(
        fix.owner_root.join("src/lib.rs"),
        b"pub fn value() -> u64 { 99 }\n",
    )
    .unwrap();
    let user_edit = std::fs::read(fix.owner_root.join("src/lib.rs")).unwrap();
    fix.gated.open();
    wait_until(
        || {
            fix.manager
                .get_session(fix.parent)
                .unwrap()
                .unwrap()
                .state()
                .unwrap()
                == faktor_core::state::AgentState::ReadyForNextTurn
        },
        30,
    )
    .await;
    // The landing blocks on the drifted owner BEFORE any write; the shadow
    // is retained and the task stays non-terminal.
    wait_until(
        || {
            fix.manager.shadow_row(fix.parent).unwrap().unwrap().state
                == ShadowRowState::IntegrationBlocked
        },
        60,
    )
    .await;
    assert_eq!(
        std::fs::read(fix.owner_root.join("src/lib.rs")).unwrap(),
        user_edit,
        "a conflicted user file is never overwritten"
    );
    assert_eq!(
        std::fs::read(fix.owner_root.join("a.txt")).unwrap(),
        b"base-alpha"
    );
    assert!(shadow_dir.is_dir(), "shadow retained on conflict");
    let h = fix.manager.get_session(fix.parent).unwrap().unwrap();
    let task = h.get_task(h.task_id().unwrap()).unwrap().unwrap();
    assert_ne!(
        task.state,
        TaskState::VerifiedComplete,
        "completion is impossible while the owner landing is blocked"
    );
    assert!(!task.state.is_terminal(), "the run stays recoverable");
    assert_eq!(
        fix.manager.active_root(fix.parent).unwrap(),
        Some(shadow_dir.clone()),
        "the IntegrationBlocked shadow stays the session's root until resolved"
    );
    // The user resolves the drift (reverts to the base content); the bounded
    // watcher re-settles: verify + land + complete + retire.
    std::fs::write(
        fix.owner_root.join("src/lib.rs"),
        b"pub fn value() -> u64 {\n    let base_amount: u64 = 10;\n    let increment: u64 = 32;\n    base_amount.saturating_add(increment)\n}\n",
    )
    .unwrap();
    wait_until(
        || {
            std::fs::read(fix.owner_root.join("src/lib.rs")).unwrap_or_default()
                == b"pub fn value() -> u64 {\n    let base_amount: u64 = 11;\n    let increment: u64 = 31;\n    base_amount.saturating_add(increment)\n}\n"
                && !shadow_dir.exists()
                && fix
                    .manager
                    .get_session(fix.parent)
                    .unwrap()
                    .unwrap()
                    .get_task(fix.manager.get_session(fix.parent).unwrap().unwrap().task_id().unwrap())
                    .unwrap()
                    .is_some_and(|t| t.state == TaskState::VerifiedComplete)
        },
        60,
    )
    .await;
    assert_eq!(
        std::fs::read(fix.owner_root.join("src/lib.rs")).unwrap(),
        b"pub fn value() -> u64 {\n    let base_amount: u64 = 11;\n    let increment: u64 = 31;\n    base_amount.saturating_add(increment)\n}\n"
    );
    assert!(!shadow_dir.exists());
    assert_eq!(
        fix.manager.shadow_row(fix.parent).unwrap().unwrap().state,
        ShadowRowState::Integrated
    );
}

/// The drive's shadow-world completion attempt (the exact shape the agent
/// runtime performs at the genuine end): route the durable task row to
/// Verifying, land an UNBOUND passing record (`tree_hash: None`) and call
/// the completion gate. While a managed shadow is live the gate must refuse:
/// the shadow world is not an owner integration.
pub(crate) fn complete_shadow_world_or_refuse(
    manager: &Arc<SessionManager>,
    session: SessionId,
) -> Result<(), faktor_session::TaskError> {
    let h = manager.get_session(session).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    for _ in 0..8 {
        let task = h.get_task(task_id).unwrap().unwrap();
        let target = match task.state {
            TaskState::Pending => TaskTransition::StartRunning,
            TaskState::Planning => TaskTransition::PlanComplete,
            TaskState::Running => TaskTransition::RequestVerification,
            TaskState::Waiting => TaskTransition::ResumeFromWaiting,
            TaskState::Blocked => TaskTransition::Unblock,
            TaskState::NeedsVerification => TaskTransition::StartVerification,
            TaskState::Verifying => break,
            s => panic!("cannot certify a task at {s:?}"),
        };
        let rev = h.task_revision(task_id).unwrap();
        h.transition_task(task_id, rev, target, None).unwrap();
    }
    let record = h.create_verification_record(
        task_id,
        None,
        vec![],
        vec![],
        vec![],
        vec![],
        None,
        VerificationStatus::Passed,
        h.now_ms(),
    )?;
    let rev = h.task_revision(task_id).unwrap();
    h.complete_verified_task(task_id, rev, record).map(|_| ())
}

#[test]
pub(crate) fn crashed_drive_residue_reopens_and_settles_deterministically() {
    // (d): a daemon "crash" (the parked drive dies with its runtime) leaves
    // the durable shadow row Active + dir on disk; a reopened daemon sees
    // the row, discards the pre-certification residue deterministically on
    // the next shadowed start, and runs a fresh shadowed task to a clean
    // integration.
    let dir = tempfile::tempdir().unwrap();
    // Phase 1 — the crashing daemon: park a shadowed drive mid-flight.
    let parent = {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let fix = open_gated_shadow(dir.path());
            // The resumed run must be verifiable: a Rust project derived
            // checks (the fixture verifier is fake-ok below).
            std::fs::create_dir_all(fix.owner_root.join("src")).unwrap();
            std::fs::write(
                fix.owner_root.join("Cargo.toml"),
                "[package]\nname = \"x\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            )
            .unwrap();
            std::fs::write(
                fix.owner_root.join("src/lib.rs"),
                "pub fn value() -> u64 {\n    let base_amount: u64 = 10;\n    let increment: u64 = 32;\n    base_amount.saturating_add(increment)\n}\n",
            )
            .unwrap();
            let _receipt = fix
                .executor
                .start_task(
                    fix.parent,
                    TaskRunRequest {
                        goal: "crashed shadowed drive".into(),
                        work_items: vec![wi("impl", WorkKind::Implementation, &[])],
                        parent_caps: read_caps(),
                        isolated_root: dir.path().join("isolated"),
                        ..Default::default()
                    },
                )
                .expect("shadowed start");
            let row = fix
                .manager
                .shadow_row(fix.parent)
                .unwrap()
                .expect("row exists");
            assert_eq!(row.state, ShadowRowState::Active);
            let shadow_dir = std::path::PathBuf::from(&row.root);
            wait_until(|| fix.gated.count() >= 1, 180).await;
            std::fs::write(shadow_dir.join("a.txt"), b"crashed-drive content").unwrap();
            assert_eq!(
                std::fs::read(fix.owner_root.join("a.txt")).unwrap(),
                b"base-alpha",
                "crashing daemon never touched the user checkout"
            );
            // Runtime ends here with the drive still parked = the crash.
            // A real crash never runs Drop; forget the service so the
            // graceful-shutdown removal cannot mask the residue.
            std::mem::forget(fix.shadows);
            std::mem::forget(fix.executor);
            fix.parent
        })
    };
    // Phase 2 — the daemon restarts over the same data dir.
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let manager =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let shadows = ShadowRoots::new(manager.clone(), dir.path().join("shadows")).unwrap();
        let row = manager.shadow_row(parent).unwrap().expect("row survives");
        assert_eq!(row.state, ShadowRowState::Active, "crash residue row");
        let residue_dir = std::path::PathBuf::from(&row.root);
        assert!(residue_dir.is_dir(), "crash residue dir");
        let residue_shadow_id = row.shadow_id.clone();
        let registry = {
            let mut r = ProviderRegistry::new();
            r.try_register(Arc::new(PerCallProvider::new(
                "fake",
                ModelCapabilities {
                    tools: true,
                    parallel_tools: true,
                    ..Default::default()
                },
                done_script(),
            )))
            .unwrap();
            r
        };
        let agent = build_agent_with_verification(
            manager.clone(),
            registry,
            faktor_agent::VerificationService::fake_ok(),
        );
        // The real daemon runs crash recovery before the first request; the
        // parked drive's turn is resolved here (the same path serve_impl
        // takes on restart).
        let _ = agent.recover();
        let orchestrator = OrchestratorRuntime::new(manager.clone(), agent.clone());
        let executor = TaskExecutor::new(
            &orchestrator,
            manager.clone(),
            agent.clone(),
            shadows.clone(),
        );
        // The interrupted turn is reconstructed (never blindly re-run): a
        // new shadowed task over a LIVE drive is a typed Conflict naming
        // the residue — resume or cancel first.
        let req = TaskRunRequest {
            goal: "post-crash implementation".into(),
            work_items: vec![wi("impl", WorkKind::Implementation, &[])],
            parent_caps: read_caps(),
            isolated_root: dir.path().join("isolated"),
            ..Default::default()
        };
        let err = executor
            .start_task(parent, req.clone())
            .expect_err("a live interrupted drive refuses a new run");
        assert!(matches!(err, ExecError::Conflict(_)), "{err}");
        assert!(err.to_string().contains("live shadow"), "{err}");
        // The operator discards the residue (the shadowed run's task never
        // certified anything); the next shadowed start begins a fresh
        // shadow over the tombstoned row.
        shadows.discard(parent).unwrap();
        let receipt = executor
            .start_task(parent, req)
            .expect("a new shadowed run starts after deterministic settlement");
        assert_eq!(receipt.mode, TaskRunMode::InSession);
        let row = manager.shadow_row(parent).unwrap().expect("new row");
        assert_eq!(row.state, ShadowRowState::Active);
        assert_ne!(row.shadow_id, residue_shadow_id, "a fresh generation");
        assert!(!residue_dir.exists(), "crash residue directory removed");
        let new_dir = std::path::PathBuf::from(&row.root);
        assert!(new_dir.is_dir());
        wait_until(
            || {
                manager
                    .get_session(parent)
                    .unwrap()
                    .unwrap()
                    .state()
                    .unwrap()
                    == faktor_core::state::AgentState::ReadyForNextTurn
            },
            30,
        )
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        std::fs::write(
            new_dir.join("src/lib.rs"),
            b"pub fn value() -> u64 {\n    let base_amount: u64 = 11;\n    let increment: u64 = 31;\n    base_amount.saturating_add(increment)\n}\n",
        )
        .unwrap();
        // The executor's own settle paths converge to the integration (the
        // post-drive settle + bounded watcher verify the candidate, land it
        // transactionally and complete) — no test-side certification.
        let owner = dir.path().join("owner").join("src/lib.rs");
        // The durable row is the authority and is written before the
        // dir cleanup (record-first); wait on the row, then assert the fs
        // effects — environment-independent on Windows and Unix alike.
        wait_until(
            || {
                manager
                    .shadow_row(parent)
                    .ok()
                    .flatten()
                    .map(|r| r.state == ShadowRowState::Integrated)
                    .unwrap_or(false)
            },
            60,
        )
        .await;
        wait_until(
            || {
                std::fs::read(&owner).unwrap_or_default()
                    == b"pub fn value() -> u64 {\n    let base_amount: u64 = 11;\n    let increment: u64 = 31;\n    base_amount.saturating_add(increment)\n}\n"
                    && !new_dir.exists()
            },
            60,
        )
        .await;
        let retired = manager.shadow_row(parent).unwrap().unwrap();
        assert_eq!(retired.state, ShadowRowState::Integrated);
        let h = manager.get_session(parent).unwrap().unwrap();
        assert_eq!(
            h.get_task(h.task_id().unwrap()).unwrap().unwrap().state,
            TaskState::VerifiedComplete
        );
    });
}

#[tokio::test]
pub(crate) async fn failed_drive_keeps_shadow_for_recovery_cancel_discards() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let scripts: Vec<Vec<ScriptedResponse>> =
        vec![vec![ScriptedResponse::Die(ProviderError::new(
            faktor_provider::ProviderErrorKind::Malformed,
            "injected permanent failure",
        ))]];
    let env = open_shadow_env(dir.path(), scripts);
    seed_owner(&env.owner_root);
    let _ = env
        .executor
        .start_task(env.parent, mutating_request(&env, "failing shadowed run"))
        .expect("start");
    let row_before = shadow_row_of(&env);
    let dir_before = std::path::PathBuf::from(&row_before.root);
    // The drive fails (permanent provider error): the session ends
    // FailedRecoverable and the task row is NOT terminal — the shadow is
    // RETAINED for the documented recovery path (never a blind discard of
    // a resumable run).
    wait_until(
        || state_of(&env, env.parent) == faktor_core::state::AgentState::FailedRecoverable,
        30,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let row = shadow_row_of(&env);
    assert_eq!(
        row.state,
        ShadowRowState::Active,
        "recoverable runs keep the shadow"
    );
    assert!(dir_before.is_dir());
    assert_eq!(
        std::fs::read(env.owner_root.join("a.txt")).unwrap(),
        b"base-alpha"
    );
    // The operator cancels the task: the terminal Cancel drives the
    // post-drive settle paths (the bounded watcher re-settles live rows) to
    // DISCARD the shadow.
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    let rev = h.task_revision(task_id).unwrap();
    h.transition_task(task_id, rev, TaskTransition::Cancel, None)
        .unwrap();
    wait_until(
        || {
            env.manager.shadow_row(env.parent).unwrap().unwrap().state == ShadowRowState::Discarded
                && !dir_before.exists()
        },
        60,
    )
    .await;
    let row = shadow_row_of(&env);
    assert_eq!(row.state, ShadowRowState::Discarded);
    assert!(!dir_before.exists());
    assert_eq!(
        owner_bytes(&env, "a.txt"),
        b"base-alpha",
        "a discarded shadow never writes the user checkout"
    );
}

#[test]
pub(crate) fn oversize_shadow_admits_a_recoverable_failure_before_any_mutation() {
    // (g) at the executor: an un-copyable base can never hang the start. The
    // BOUNDED preflight refuses the isolation candidate promptly; the turn is
    // admitted and lands FailedRecoverable with the durable user prompt and
    // the durable run row — no provider call, no durable shadow row, no task
    // row, and no user bytes touched.
    let dir = tempfile::tempdir().unwrap();
    let env = open_env_with_shadows(
        dir.path(),
        done_script(),
        ShadowCopyLimits {
            max_entries: 2,
            max_total_bytes: 1024 * 1024,
        },
        true,
    );
    seed_owner(&env.owner_root);
    std::fs::write(env.owner_root.join("extra.txt"), b"third file").unwrap();
    let receipt = env
        .executor
        .start_task(env.parent, mutating_request(&env, "oversized shadow"))
        .expect("the run is admitted as a recoverable failure");
    assert_eq!(receipt.mode, TaskRunMode::InSession);
    assert!(!receipt.queued);
    assert_eq!(
        env.provider.count(),
        0,
        "no drive ever started (no model call)"
    );
    assert!(env.manager.shadow_row(env.parent).unwrap().is_none());
    assert!(env
        .manager
        .get_session(env.parent)
        .unwrap()
        .unwrap()
        .list_tasks()
        .unwrap()
        .is_empty());
    assert_eq!(
        std::fs::read(env.owner_root.join("a.txt")).unwrap(),
        b"base-alpha"
    );
    // The turn is settled and promptable again; its user prompt and run row
    // are durable.
    let handle = env.manager.get_session(env.parent).unwrap().unwrap();
    assert_eq!(
        handle.state().unwrap(),
        faktor_core::state::AgentState::FailedRecoverable
    );
    let messages = handle.messages_before(None, 10).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].role, "user");
    assert_eq!(messages[0].data["text"], "oversized shadow");
    assert!(
        handle
            .memory_facts()
            .unwrap()
            .iter()
            .any(
                |(kind, key, _)| kind == crate::runtime::task_executor::TASK_RUN_ROW_KIND
                    && key == &receipt.run_id
            ),
        "the admitted run row is durable"
    );
}

// ================================================= P0-48 shadow mutation roots
// end-to-end over REAL tools (the wave-22 flip): with the agent's five
// root-resolution sites consulting `SessionManager::resolve_workspace_root`
// and the workspace-scoped consumers consulting live shadow rows, a
// shadowed drive's write_file/read/verification/repo-knowledge paths all
// resolve the SHADOW root while the drive is live — the user checkout is
// byte-untouched until a VerifiedComplete integration (wave-21
// commit_all), and every un-shadowed path keeps today's behavior.

/// A per-call scripted provider that also records every request's rendered
/// system prompt (where repo map + AGENTS.md rules + instruction rules
/// ride), so a test can prove WHICH root a drive read its context from.
pub(crate) struct RecordingProvider {
    pub(crate) caps: ModelCapabilities,
    pub(crate) calls: StdMutex<Vec<Vec<ScriptedResponse>>>,
    pub(crate) script_index: AtomicUsize,
    pub(crate) request_count: AtomicUsize,
    pub(crate) prompts: StdMutex<Vec<String>>,
}

impl RecordingProvider {
    fn new(caps: ModelCapabilities, per_call_scripts: Vec<Vec<ScriptedResponse>>) -> Self {
        Self {
            caps: caps.clone(),
            calls: StdMutex::new(per_call_scripts),
            script_index: AtomicUsize::new(0),
            request_count: AtomicUsize::new(0),
            prompts: StdMutex::new(Vec::new()),
        }
    }
    fn recorded(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }
}

impl Provider for RecordingProvider {
    fn id(&self) -> &str {
        "fake"
    }
    fn capabilities(&self, model: &str) -> ModelCapabilities {
        let _ = model;
        self.caps.clone()
    }
    fn stream(&self, req: GenericAgentRequest) -> ProviderStream {
        use futures::StreamExt;
        self.request_count.fetch_add(1, Ordering::SeqCst);
        self.prompts.lock().unwrap().push(req.system.clone());
        let i = self.script_index.fetch_add(1, Ordering::SeqCst);
        let script: Vec<ScriptedResponse> = self
            .calls
            .lock()
            .unwrap()
            .get(i)
            .cloned()
            .unwrap_or_else(|| vec![ScriptedResponse::End]);
        let stream = futures::stream::iter(script).map(|s| match s {
            ScriptedResponse::Text(t) => Ok(ProviderChunk::Text { text: t }),
            ScriptedResponse::ToolCall { id, name, input } => Ok(ProviderChunk::ToolCall {
                id,
                name,
                input,
                complete: true,
            }),
            ScriptedResponse::Die(e) => Err(e),
            ScriptedResponse::End => Ok(ProviderChunk::Done),
            ScriptedResponse::Reasoning(_) => unreachable!("no reasoning scripts"),
        });
        Box::pin(stream)
    }
}

/// A real write_file: resolves the RELATIVE path through the session's
/// workspace handle and writes atomically — the exact operation the flipped
/// tool-batch site serves. No postcondition (no crash is simulated here).
pub(crate) fn real_write_tool() -> Tool {
    Tool {
        name: "write_file".into(),
        description: "writes a real file".into(),
        input_schema: serde_json::json!({"type": "object"}),
        resource_class: ResourceClass::DiskWrite,
        capability: None,
        recovery_hint: ToolRecovery::WorkspaceWrite,
        path_args: vec!["path".into()],
        execute: Arc::new(|ctx: ToolRunCtx, args| {
            Box::pin(async move {
                let Some(ws) = &ctx.workspace else {
                    return Err(Error::internal("no workspace wired"));
                };
                let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("");
                let content = args
                    .get("content")
                    .and_then(|c| c.as_str())
                    .unwrap_or_default();
                ws.write_atomic(std::path::Path::new(path), content.as_bytes())
                    .map_err(|e| Error::internal(format!("write {path}: {e}")))?;
                Ok(ToolOutcome {
                    text: format!("wrote {path}"),
                    exit_code: Some(0),
                    ..Default::default()
                })
            })
        }),
    }
}

/// A CPU tool whose FIRST invocation parks the drive (mid-iteration, at
/// ExecutingTool) until the gate opens: the deterministic mid-drive window
/// of a shadowed run. Fired counts invocations that reached the park.
pub(crate) fn parking_tool(
    name: &str,
    gate: Arc<tokio::sync::Notify>,
    fired: Arc<AtomicUsize>,
) -> Tool {
    Tool {
        name: name.into(),
        description: "parks once".into(),
        input_schema: serde_json::json!({"type": "object"}),
        resource_class: ResourceClass::Cpu,
        capability: None,
        recovery_hint: ToolRecovery::Idempotent,
        path_args: vec![],
        execute: Arc::new(move |_ctx, _args| {
            let gate = gate.clone();
            let fired = fired.clone();
            Box::pin(async move {
                if fired.fetch_add(1, Ordering::SeqCst) == 0 {
                    gate.notified().await;
                }
                Ok(ToolOutcome {
                    text: "parked".into(),
                    exit_code: Some(0),
                    ..Default::default()
                })
            })
        }),
    }
}

/// A write_file whose FIRST invocation parks AFTER the write landed (the
/// drive is mid-flight with the file already inside the resolved root).
pub(crate) fn parked_write_tool(gate: Arc<tokio::sync::Notify>, fired: Arc<AtomicUsize>) -> Tool {
    Tool {
        name: "write_file".into(),
        description: "writes a real file, parking once".into(),
        input_schema: serde_json::json!({"type": "object"}),
        resource_class: ResourceClass::DiskWrite,
        capability: None,
        recovery_hint: ToolRecovery::WorkspaceWrite,
        path_args: vec!["path".into()],
        execute: Arc::new(move |ctx: ToolRunCtx, args| {
            let gate = gate.clone();
            let fired = fired.clone();
            Box::pin(async move {
                let Some(ws) = &ctx.workspace else {
                    return Err(Error::internal("no workspace wired"));
                };
                let path = args.get("path").and_then(|p| p.as_str()).unwrap_or("");
                let content = args
                    .get("content")
                    .and_then(|c| c.as_str())
                    .unwrap_or_default();
                ws.write_atomic(std::path::Path::new(path), content.as_bytes())
                    .map_err(|e| Error::internal(format!("write {path}: {e}")))?;
                if fired.fetch_add(1, Ordering::SeqCst) == 0 {
                    gate.notified().await;
                }
                Ok(ToolOutcome {
                    text: format!("wrote {path}"),
                    exit_code: Some(0),
                    ..Default::default()
                })
            })
        }),
    }
}

/// Workspace-scoped root provider over the REAL SessionManager (mirror of
/// the daemon's `SessionWorkspaceRoots`): the live shadow of a shadowed
/// workspace re-points instruction loading at the shadow root.
pub(crate) struct RealRoots(Arc<SessionManager>);

impl faktor_instructions::WorkspaceRootProvider for RealRoots {
    fn workspace_root(&self, workspace_id: u64) -> Option<std::path::PathBuf> {
        if workspace_id == 0 {
            return None;
        }
        let ws = WorkspaceId::new(workspace_id);
        match self.0.live_workspace_shadow_root(ws) {
            Ok(Some(root)) => Some(root),
            Ok(None) | Err(_) => self.0.workspace_root(ws).ok().flatten(),
        }
    }
}

pub(crate) fn real_resolver(
    manager: &Arc<SessionManager>,
) -> Arc<faktor_instructions::InstructionResolver> {
    Arc::new(faktor_instructions::InstructionResolver::new(
        Arc::new(RealRoots(manager.clone())),
        faktor_instructions::DEFAULT_RESOLVER_CACHE_ENTRIES,
    ))
}

/// An executor env whose drive runs REAL tools (write_file over the
/// session's resolved workspace root) with a REAL instructions resolver and
/// the given verification service. `parked_write` registers the parking
/// write tool; the gate/fired pair exposes the mid-drive window.
pub(crate) struct RealToolEnv {
    pub(crate) manager: Arc<SessionManager>,
    pub(crate) provider: Arc<RecordingProvider>,
    pub(crate) executor: Arc<TaskExecutor>,
    pub(crate) parent: SessionId,
    pub(crate) owner_root: std::path::PathBuf,
    pub(crate) isolated_root: std::path::PathBuf,
    pub(crate) gate: Arc<tokio::sync::Notify>,
    pub(crate) fired: Arc<AtomicUsize>,
    /// The daemon shadow service of the production-wired fixture (`None`
    /// for the owner-direct seam), so tests can arm the preflight seam.
    pub(crate) shadows: Option<Arc<ShadowRoots>>,
}
pub(crate) fn real_state_of(env: &RealToolEnv) -> faktor_core::state::AgentState {
    env.manager
        .get_session(env.parent)
        .unwrap()
        .unwrap()
        .state()
        .unwrap()
}

pub(crate) fn real_mutating_request(env: &RealToolEnv, goal: &str) -> TaskRunRequest {
    TaskRunRequest {
        goal: goal.to_string(),
        work_items: vec![wi("impl", WorkKind::Implementation, &[])],
        parent_caps: read_caps(),
        isolated_root: env.isolated_root.clone(),
        ..Default::default()
    }
}

pub(crate) fn open_real_tool_env(
    root: &std::path::Path,
    scripts: Vec<Vec<ScriptedResponse>>,
    verification: Arc<faktor_agent::VerificationService>,
    parked_write: bool,
) -> Arc<RealToolEnv> {
    open_real_tool_env_full(root, scripts, verification, parked_write, true)
}

/// [`open_real_tool_env`] with the executor's shadow wiring spelled out:
/// `service` = carry the daemon's [`ShadowRoots`] service and build the
/// executor through the production constructor (mutating runs isolate), or
/// build through the cfg(test) owner-direct seam. The daemon graph's
/// production wiring always carries the service; the seam exists only for
/// the low-level suites that predate shadow mutation.
pub(crate) fn open_real_tool_env_full(
    root: &std::path::Path,
    scripts: Vec<Vec<ScriptedResponse>>,
    verification: Arc<faktor_agent::VerificationService>,
    parked_write: bool,
    service: bool,
) -> Arc<RealToolEnv> {
    open_real_tool_env_inner(root, scripts, verification, parked_write, service, false)
}

/// [`open_real_tool_env_full`] with the daemon's process supervisor wired
/// (the proof-basis probe path in production always has one; the default
/// fixture deliberately runs without it so probes degrade explicitly).
pub(crate) fn open_real_tool_env_supervised(
    root: &std::path::Path,
    scripts: Vec<Vec<ScriptedResponse>>,
    verification: Arc<faktor_agent::VerificationService>,
) -> Arc<RealToolEnv> {
    open_real_tool_env_inner(root, scripts, verification, true, true, true)
}

/// [`open_real_tool_env_inner_with_resolver`] over the fake verification
/// service with the resolver as the only explicit input: the fail-closed
/// proof-basis suites inject `NoRoots`, hostile roots and alternating roots
/// here.
pub(crate) fn open_real_tool_env_with_resolver(
    root: &std::path::Path,
    resolver: Arc<faktor_instructions::InstructionResolver>,
) -> Arc<RealToolEnv> {
    open_real_tool_env_inner_with_resolver(
        root,
        vec![],
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
        false,
        Some(resolver),
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn open_real_tool_env_inner(
    root: &std::path::Path,
    scripts: Vec<Vec<ScriptedResponse>>,
    verification: Arc<faktor_agent::VerificationService>,
    parked_write: bool,
    service: bool,
    with_supervisor: bool,
) -> Arc<RealToolEnv> {
    open_real_tool_env_inner_with_resolver(
        root,
        scripts,
        verification,
        parked_write,
        service,
        with_supervisor,
        None,
    )
}

/// [`open_real_tool_env_inner`] with an explicit instruction resolver (the
/// fail-closed proof-basis suites inject `NoRoots`, hostile roots and
/// alternating roots here).
#[allow(clippy::too_many_arguments)]
pub(crate) fn open_real_tool_env_inner_with_resolver(
    root: &std::path::Path,
    scripts: Vec<Vec<ScriptedResponse>>,
    verification: Arc<faktor_agent::VerificationService>,
    parked_write: bool,
    service: bool,
    with_supervisor: bool,
    resolver: Option<Arc<faktor_instructions::InstructionResolver>>,
) -> Arc<RealToolEnv> {
    let manager = SessionManager::open(root.join("store"), root.join("cas"), true).unwrap();
    let caps = ModelCapabilities {
        tools: true,
        parallel_tools: true,
        ..Default::default()
    };
    let provider = Arc::new(RecordingProvider::new(caps, scripts));
    let mut registry = ProviderRegistry::new();
    registry.try_register(provider.clone()).unwrap();
    let gate = Arc::new(tokio::sync::Notify::new());
    let fired = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    if parked_write {
        tools.register(parked_write_tool(gate.clone(), fired.clone()));
    } else {
        tools.register(real_write_tool());
    }
    tools.register(parking_tool("pause", gate.clone(), fired.clone()));
    let resolver = resolver.unwrap_or_else(|| real_resolver(&manager));
    let supervisor = if with_supervisor {
        Some(faktor_terminal::ProcessSupervisor::new(manager.cas()))
    } else {
        None
    };
    let agent = AgentRuntime::new(AgentDeps {
        session: manager.clone(),
        providers: Arc::new(registry),
        chunk_sink: None,
        permission_requester: Arc::new(AlwaysAllow),
        evidence: Arc::new(NoEvidence),
        tools: Arc::new(tools),
        cas: None,
        workspaces: faktor_fs::WorkspaceFileService::new(),
        edit: None,
        snapshots: None,
        sandbox: None,
        supervisor,
        verification,
        hooks: None,
        instructions_resolver: resolver,
        routing: faktor_agent::FixedRoutingPolicy::passthrough(),
        budgets: Arc::new(faktor_session::NoopBudget),
        model: "m".into(),
        compaction_model: None,
        compact_at_usage: 0.65,
        instructions: "You are a test agent.".into(),
        clock: Arc::new(SystemClock),
        tool_call_mode: ToolCallMode::Native,
        tool_deadline_ms: 120_000,
        retry_policy: faktor_core::retry::RetryPolicy::default(),
        semantic: faktor_agent::fallback_semantic_registry(),
        context_prior: None,
        secret_registry: None,
        efficiency: Default::default(),
    })
    .unwrap();
    let owner_root = root.join("owner");
    std::fs::create_dir_all(&owner_root).unwrap();
    let ws = manager
        .create_workspace(owner_root.to_str().unwrap())
        .unwrap();
    let wt = WorktreeId::new(
        manager
            .put_worktree(ws, owner_root.to_str().unwrap(), "main")
            .unwrap() as u64,
    );
    let parent = manager
        .create_session(ws, "real-shadow-tools", "fake", "m")
        .unwrap()
        .id();
    manager.adopt_identity(parent, wt, TaskId::new(1)).unwrap();
    let orchestrator = OrchestratorRuntime::new(manager.clone(), agent.clone());
    let (shadows, executor) = if service {
        let shadows = ShadowRoots::new(manager.clone(), root.join("shadows")).unwrap();
        let executor = TaskExecutor::new(
            &orchestrator,
            manager.clone(),
            agent.clone(),
            shadows.clone(),
        );
        (Some(shadows), executor)
    } else {
        (
            None,
            TaskExecutor::new_owner_direct_for_test_harness(
                &orchestrator,
                manager.clone(),
                agent.clone(),
            ),
        )
    };
    let isolated_root = root.join("isolated");
    std::fs::create_dir_all(&isolated_root).unwrap();
    Arc::new(RealToolEnv {
        manager,
        provider,
        executor,
        parent,
        owner_root,
        isolated_root,
        gate,
        fired,
        shadows,
    })
}

pub(crate) fn real_env_task_row(env: &RealToolEnv) -> faktor_session::Task {
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    h.get_task(h.task_id().unwrap()).unwrap().unwrap()
}

/// Settle a real-tool shadowed drive through the executor's OWN pipeline:
/// the shadow-world completion the drive attempts is never the permission
/// to complete (the session gate refuses an unbound proof while a managed
/// shadow is live), so the post-drive settlement + bounded watcher verify
/// the candidate, land it transactionally, complete the task and retire the
/// shadow. The test polls the durable outcome and never certifies by hand.
pub(crate) async fn settle_verified_integrate(env: &Arc<RealToolEnv>) {
    wait_until(
        || real_state_of(env) == faktor_core::state::AgentState::ReadyForNextTurn,
        60,
    )
    .await;
    // The durable outcome converges through the executor's own settle
    // paths (post-drive settle, then the bounded watcher) — never a manual
    // executor call from the test.
    wait_until(
        || {
            env.manager
                .shadow_row(env.parent)
                .ok()
                .flatten()
                .is_some_and(|r| {
                    r.state == ShadowRowState::Integrated
                        && !std::path::PathBuf::from(&r.root).exists()
                })
                && real_env_task_row(env).state == TaskState::VerifiedComplete
        },
        60,
    )
    .await;
    let row = env
        .manager
        .shadow_row(env.parent)
        .unwrap()
        .expect("row exists");
    assert_eq!(row.state, ShadowRowState::Integrated);
    assert!(
        !std::path::PathBuf::from(&row.root).exists(),
        "clean integration removes the shadow directory"
    );
    assert_eq!(real_env_task_row(env).state, TaskState::VerifiedComplete);
}

pub(crate) fn seed_rust(env: &RealToolEnv) {
    std::fs::create_dir_all(env.owner_root.join("src")).unwrap();
    std::fs::write(
        env.owner_root.join("Cargo.toml"),
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        env.owner_root.join("src/lib.rs"),
        "pub fn value() -> u64 {\n    let base_amount: u64 = 10;\n    let increment: u64 = 32;\n    base_amount.saturating_add(increment)\n}\n",
    )
    .unwrap();
}

#[tokio::test]
pub(crate) async fn real_write_drive_writes_the_shadow_and_verified_complete_integrates_it() {
    let _heavy = heavy_guard();
    // (a) over the REAL executor + REAL tools: a shadowed mutating drive
    // executes write_file against the SHADOW root (the flipped tool-batch
    // site); the user checkout stays byte-untouched MID-drive; a
    // VerifiedComplete integration (wave-21 commit_all through the
    // post-drive finalize) lands the content in the user checkout and
    // removes the shadow.
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env(
        dir.path(),
        vec![
            vec![
                ScriptedResponse::ToolCall {
                    id: "c1".into(),
                    name: "write_file".into(),
                    input: serde_json::json!({
                        "path": "src/lib.rs",
                        "content": "pub fn value() -> u64 {\n    let base_amount: u64 = 10;\n    let increment: u64 = 32;\n    base_amount.saturating_add(increment)\n}\n",
                    }),
                },
                ScriptedResponse::ToolCall {
                    id: "c2".into(),
                    name: "write_file".into(),
                    input: serde_json::json!({
                        "path": "util.rs",
                        "content": "pub fn fresh() -> u64 {\n    let seed: u64 = 7;\n    let factor: u64 = 3;\n    seed.saturating_mul(factor)\n}\n",
                    }),
                },
                ScriptedResponse::Text("done".into()),
                ScriptedResponse::End,
            ],
            vec![ScriptedResponse::End],
        ],
        faktor_agent::VerificationService::fake_ok(),
        true,
    );
    seed_rust(&env);
    let receipt = env
        .executor
        .start_task(
            env.parent,
            real_mutating_request(&env, "implement the change"),
        )
        .expect("shadowed real-tool start");
    assert_eq!(receipt.mode, TaskRunMode::InSession);
    let row = env
        .manager
        .shadow_row(env.parent)
        .unwrap()
        .expect("shadow row at begin");
    let shadow_dir = std::path::PathBuf::from(&row.root);
    assert_eq!(row.state, ShadowRowState::Active);
    // Mid-drive: the FIRST write landed inside the shadow and parked the
    // drive at ExecutingTool — the user checkout is byte-untouched.
    wait_until(|| env.fired.load(Ordering::SeqCst) >= 1, 300).await;
    assert_eq!(
        std::fs::read(shadow_dir.join("src/lib.rs")).unwrap(),
        b"pub fn value() -> u64 {\n    let base_amount: u64 = 10;\n    let increment: u64 = 32;\n    base_amount.saturating_add(increment)\n}\n",
        "the write landed in the SHADOW"
    );
    assert_eq!(
        std::fs::read(env.owner_root.join("src/lib.rs")).unwrap(),
        b"pub fn value() -> u64 {\n    let base_amount: u64 = 10;\n    let increment: u64 = 32;\n    base_amount.saturating_add(increment)\n}\n",
        "user checkout untouched mid-drive"
    );
    assert!(!env.owner_root.join("util.rs").exists());
    assert_eq!(
        env.manager.active_root(env.parent).unwrap(),
        Some(shadow_dir.clone()),
        "active_root re-points while the drive is live"
    );
    // Release the drive: second write may or may not have landed before the
    // park, but whatever the shadow holds now must not touch the user
    // checkout until the verified integration.
    env.gate.notify_waiters();
    settle_verified_integrate(&env).await;
    assert_eq!(
        std::fs::read(env.owner_root.join("src/lib.rs")).unwrap(),
        b"pub fn value() -> u64 {\n    let base_amount: u64 = 10;\n    let increment: u64 = 32;\n    base_amount.saturating_add(increment)\n}\n",
        "VerifiedComplete integration lands the changed file"
    );
    assert_eq!(
        std::fs::read(env.owner_root.join("util.rs")).unwrap(),
        b"pub fn fresh() -> u64 {\n    let seed: u64 = 7;\n    let factor: u64 = 3;\n    seed.saturating_mul(factor)\n}\n",
        "VerifiedComplete integration lands the created file"
    );
    assert!(
        env.manager.active_root(env.parent).unwrap().is_none(),
        "a retired shadow stops re-pointing"
    );
}

#[tokio::test]
pub(crate) async fn real_shadowed_prompt_survives_a_base_file_vanishing_in_the_preflight_window() {
    let _heavy = heavy_guard();
    // (REQUIRED, end to end) The prompt path over the REAL executor: a
    // checkout file deleted between its enumeration and metadata read while
    // the shadow preflight runs must not fail the prompt. The drive proceeds
    // in a shadow that is the stable post-vanish tree, and the verified
    // integration lands it.
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env(
        dir.path(),
        vec![
            vec![
                ScriptedResponse::ToolCall {
                    id: "c1".into(),
                    name: "write_file".into(),
                    input: serde_json::json!({
                        "path": "util.rs",
                        "content": "pub fn fresh() -> u64 {\n    let seed: u64 = 7;\n    let factor: u64 = 3;\n    seed.saturating_mul(factor)\n}\n",
                    }),
                },
                ScriptedResponse::Text("done".into()),
                ScriptedResponse::End,
            ],
            vec![ScriptedResponse::End],
        ],
        faktor_agent::VerificationService::fake_ok(),
        true,
    );
    seed_rust(&env);
    let obsolete = env.owner_root.join("obsolete.txt");
    std::fs::write(&obsolete, b"stale").unwrap();
    let obsolete_canonical = env.owner_root.canonicalize().unwrap().join("obsolete.txt");
    env.shadows
        .as_ref()
        .expect("production shadow wiring")
        .arm_preflight_seam(move |path| {
            if path == obsolete_canonical.as_path() {
                std::fs::remove_file(path).unwrap();
            }
        });
    let receipt = env
        .executor
        .start_task(
            env.parent,
            real_mutating_request(&env, "implement the change"),
        )
        .expect("the prompt is accepted despite the concurrent vanish");
    assert_eq!(receipt.mode, TaskRunMode::InSession);
    // Mid-drive: the shadow exists and holds the stable post-vanish tree.
    wait_until(|| env.fired.load(Ordering::SeqCst) >= 1, 300).await;
    let row = env
        .manager
        .shadow_row(env.parent)
        .unwrap()
        .expect("shadow row at begin");
    assert_eq!(row.state, ShadowRowState::Active);
    let shadow_dir = std::path::PathBuf::from(&row.root);
    assert!(
        !shadow_dir.join("obsolete.txt").exists(),
        "the shadow is the stable post-vanish tree"
    );
    assert!(
        shadow_dir.join("util.rs").exists(),
        "the drive wrote inside the shadow"
    );
    env.gate.notify_waiters();
    settle_verified_integrate(&env).await;
    assert!(
        !obsolete.exists(),
        "the vanish is the only change to the owner"
    );
    assert!(env.owner_root.join("util.rs").exists(), "verified landing");
}

#[tokio::test]
pub(crate) async fn shadowed_drive_reads_repo_knowledge_and_rules_from_the_shadow() {
    let _heavy = heavy_guard();
    // (b): repo knowledge + instructions of a shadowed drive resolve from
    // the SHADOW root — a rules file and AGENTS.md marker placed inside the
    // shadow AFTER begin (never in the user checkout) show up in the NEXT
    // iteration's rendered context, and the user checkout's own rules never
    // leak into the drive.
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("owner")).unwrap();
    let env = open_real_tool_env(
        dir.path(),
        vec![
            vec![
                ScriptedResponse::ToolCall {
                    id: "p1".into(),
                    name: "pause".into(),
                    input: serde_json::json!({}),
                },
                ScriptedResponse::End,
            ],
            vec![
                ScriptedResponse::Text("conclude the change".into()),
                ScriptedResponse::End,
            ],
            vec![ScriptedResponse::End],
        ],
        faktor_agent::VerificationService::disabled(),
        false,
    );
    std::fs::write(
        env.owner_root.join("AGENTS.md"),
        "user-root-marker-4f1: follow user checkout conventions\n",
    )
    .unwrap();
    std::fs::write(env.owner_root.join("a.txt"), b"base-alpha").unwrap();
    let receipt = env
        .executor
        .start_task(env.parent, real_mutating_request(&env, "convention task"))
        .expect("shadowed start");
    assert_eq!(receipt.mode, TaskRunMode::InSession);
    let row = env.manager.shadow_row(env.parent).unwrap().expect("row");
    let shadow_dir = std::path::PathBuf::from(&row.root);
    // Mid-flight (the pause tool parks the drive): write the shadow-only
    // world — a new file and a REWRITTEN AGENTS.md — into the shadow.
    wait_until(|| env.fired.load(Ordering::SeqCst) >= 1, 300).await;
    std::fs::write(
        shadow_dir.join("AGENTS.md"),
        "shadow-world-marker-7c1: drive inside the shadow world\n",
    )
    .unwrap();
    std::fs::write(shadow_dir.join("only-shadow-notes.md"), b"shadow notes\n").unwrap();
    assert!(
        std::fs::read_to_string(env.owner_root.join("AGENTS.md"))
            .unwrap()
            .contains("user-root-marker-4f1"),
        "the user checkout rules are untouched"
    );
    env.gate.notify_waiters();
    wait_until(
        || real_state_of(&env) == faktor_core::state::AgentState::ReadyForNextTurn,
        60,
    )
    .await;
    let prompts = env.provider.recorded();
    assert!(prompts.len() >= 2, "two requests expected: {prompts:?}");
    assert!(
        !prompts[0].contains("shadow-world-marker-7c1"),
        "the first context predates the shadow writes"
    );
    assert!(
        !prompts[0].contains("only-shadow-notes.md"),
        "the first repo map cannot see the shadow-only file"
    );
    let second = &prompts[1];
    assert!(
        second.contains("shadow-world-marker-7c1"),
        "the rewritten shadow AGENTS.md must ride the next context"
    );
    assert!(
        second.contains("only-shadow-notes.md"),
        "the repo file map must come from the shadow root"
    );
    assert!(
        second.contains("always, loaded: always"),
        "the instruction resolver must append the shadow's rule tree"
    );
    assert!(
        !second.contains("user-root-marker-4f1"),
        "user-checkout rules must never leak into a shadowed drive"
    );
    // The drive is a no-change turn: the shadow is retained (nothing was
    // certified), and the user checkout is untouched.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !std::fs::read_to_string(env.owner_root.join("AGENTS.md"))
            .unwrap()
            .contains("shadow-world-marker-7c1"),
        "no shadow byte ever lands without a verified integration"
    );
    assert_eq!(
        env.manager.shadow_row(env.parent).unwrap().unwrap().state,
        ShadowRowState::Active
    );
}

#[tokio::test]
pub(crate) async fn real_write_drive_user_drift_conflicts_at_integration_then_resolves() {
    let _heavy = heavy_guard();
    // (d): the conflict path end-to-end at the agent + executor level — the
    // drive's REAL write lands in the shadow; a mid-drive USER edit of the
    // same file blocks the OWNER LANDING before any write (the owner stays
    // byte-identical and the task stays non-terminal: the drive's own
    // completion claim is refused while the managed shadow is live), and
    // resolving the drift lets the bounded watcher verify, land and
    // complete.
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env(
        dir.path(),
        vec![
            vec![
                ScriptedResponse::ToolCall {
                    id: "c1".into(),
                    name: "write_file".into(),
                    input: serde_json::json!({
                        "path": "src/lib.rs",
                        "content": "pub fn value() -> u64 {\n    let base_amount: u64 = 11;\n    let increment: u64 = 31;\n    base_amount.saturating_add(increment)\n}\n",
                    }),
                },
                ScriptedResponse::Text("done".into()),
                ScriptedResponse::End,
            ],
            vec![ScriptedResponse::End],
        ],
        faktor_agent::VerificationService::fake_ok(),
        true,
    );
    seed_rust(&env);
    let base_src = std::fs::read(env.owner_root.join("src/lib.rs")).unwrap();
    let receipt = env
        .executor
        .start_task(
            env.parent,
            real_mutating_request(&env, "implement src/lib.rs"),
        )
        .expect("shadowed start");
    assert_eq!(receipt.mode, TaskRunMode::InSession);
    let row = env.manager.shadow_row(env.parent).unwrap().expect("row");
    let shadow_dir = std::path::PathBuf::from(&row.root);
    // Mid-drive: the agent's write landed in the shadow; the user then
    // edits the file externally.
    wait_until(|| env.fired.load(Ordering::SeqCst) >= 1, 300).await;
    assert_eq!(
        std::fs::read(shadow_dir.join("src/lib.rs")).unwrap(),
        b"pub fn value() -> u64 {\n    let base_amount: u64 = 11;\n    let increment: u64 = 31;\n    base_amount.saturating_add(increment)\n}\n",
        "the agent wrote the shadow"
    );
    assert_eq!(
        std::fs::read(env.owner_root.join("src/lib.rs")).unwrap(),
        base_src
    );
    std::fs::write(
        env.owner_root.join("src/lib.rs"),
        b"pub fn value() -> u64 { 99 }\n",
    )
    .unwrap();
    let user_edit = std::fs::read(env.owner_root.join("src/lib.rs")).unwrap();
    env.gate.notify_waiters();
    wait_until(
        || real_state_of(&env) == faktor_core::state::AgentState::ReadyForNextTurn,
        60,
    )
    .await;
    // The executor's own settle paths converge to the durable
    // IntegrationBlocked outcome (the bounded post-drive watcher re-settles
    // the live shadow) — no manual finalize call, and no completion: the
    // drifted owner blocks landing before any write.
    wait_until(
        || {
            env.manager.shadow_row(env.parent).unwrap().unwrap().state
                == ShadowRowState::IntegrationBlocked
        },
        60,
    )
    .await;
    assert_eq!(
        std::fs::read(env.owner_root.join("src/lib.rs")).unwrap(),
        user_edit,
        "a conflicted user file is never overwritten"
    );
    assert!(shadow_dir.is_dir(), "the shadow is retained on conflict");
    let task = real_env_task_row(&env);
    assert_ne!(task.state, TaskState::VerifiedComplete);
    assert!(!task.state.is_terminal());
    assert_eq!(
        env.manager.active_root(env.parent).unwrap(),
        Some(shadow_dir.clone()),
        "the blocked shadow stays the session root until resolved"
    );
    // The user resolves the drift (reverts to the base content); the same
    // auto decision integrates through the executor's own settle paths.
    std::fs::write(env.owner_root.join("src/lib.rs"), &base_src).unwrap();
    wait_until(
        || {
            std::fs::read(env.owner_root.join("src/lib.rs")).unwrap_or_default()
                == b"pub fn value() -> u64 {\n    let base_amount: u64 = 11;\n    let increment: u64 = 31;\n    base_amount.saturating_add(increment)\n}\n"
                && !shadow_dir.exists()
                && real_env_task_row(&env).state == TaskState::VerifiedComplete
        },
        60,
    )
    .await;
    assert_eq!(
        std::fs::read(env.owner_root.join("src/lib.rs")).unwrap(),
        b"pub fn value() -> u64 {\n    let base_amount: u64 = 11;\n    let increment: u64 = 31;\n    base_amount.saturating_add(increment)\n}\n"
    );
    assert!(!shadow_dir.exists());
    assert_eq!(
        env.manager.shadow_row(env.parent).unwrap().unwrap().state,
        ShadowRowState::Integrated
    );
}

#[tokio::test]
pub(crate) async fn shadow_crash_mid_land_recovers_from_the_durable_txn_phase() {
    // Crash recovery of a shadowed landing: the seam stops the transaction
    // after ONE applied path (phase `Landing`, journaled). The resume reads
    // the durable phase, re-applies ONLY the pending paths, verifies whole-
    // root equality with the candidate and completes — the already-applied
    // path is never rewritten blind.
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env(
        dir.path(),
        vec![
            vec![
                ScriptedResponse::ToolCall {
                    id: "c1".into(),
                    name: "write_file".into(),
                    input: serde_json::json!({
                        "path": "src/lib.rs",
                        "content": "pub fn value() -> u64 {\n    let base_amount: u64 = 11;\n    let increment: u64 = 31;\n    base_amount.saturating_add(increment)\n}\n",
                    }),
                },
                ScriptedResponse::Text("done".into()),
                ScriptedResponse::End,
            ],
            vec![ScriptedResponse::End],
        ],
        faktor_agent::VerificationService::fake_ok(),
        true,
    );
    seed_rust(&env);
    std::fs::write(env.owner_root.join("a.txt"), b"base-alpha").unwrap();
    env.executor
        .set_settlement_crash_seam(Some(CrashSeam::IntegrationApply { after: 1 }));
    let receipt = env
        .executor
        .start_task(
            env.parent,
            real_mutating_request(&env, "implement the change"),
        )
        .expect("shadowed start");
    assert_eq!(receipt.mode, TaskRunMode::InSession);
    let row = env.manager.shadow_row(env.parent).unwrap().expect("row");
    let shadow_dir = std::path::PathBuf::from(&row.root);
    wait_until(|| env.fired.load(Ordering::SeqCst) >= 1, 300).await;
    // The second changed path is staged in the shadow while the drive parks.
    std::fs::write(shadow_dir.join("a.txt"), b"agent a").unwrap();
    env.gate.notify_waiters();
    wait_until(
        || real_state_of(&env) == faktor_core::state::AgentState::ReadyForNextTurn,
        60,
    )
    .await;
    // The first settle applies a.txt then the seam stops it (durable
    // `Landing` phase with the pending src/lib.rs journaled).
    wait_until(
        || std::fs::read(env.owner_root.join("a.txt")).unwrap_or_default() == b"agent a",
        60,
    )
    .await;
    assert_eq!(
        std::fs::read(env.owner_root.join("src/lib.rs")).unwrap(),
        b"pub fn value() -> u64 {\n    let base_amount: u64 = 10;\n    let increment: u64 = 32;\n    base_amount.saturating_add(increment)\n}\n",
        "the pending path has not landed yet"
    );
    let candidate = root_digest(&shadow_dir);
    // Resume from the durable phase.
    env.executor.set_settlement_crash_seam(None);
    let outcome = env
        .executor
        .settle_run(RunSettlement::InSession {
            parent: env.parent,
            run_id: format!("tx-session-{}", env.parent.raw()),
        })
        .await
        .expect("resumed settlement");
    assert!(outcome.completed, "{outcome:?}");
    assert_eq!(
        owner_digest(&env),
        candidate,
        "the resumed landing reached the exact candidate"
    );
    assert_eq!(
        std::fs::read(env.owner_root.join("src/lib.rs")).unwrap(),
        b"pub fn value() -> u64 {\n    let base_amount: u64 = 11;\n    let increment: u64 = 31;\n    base_amount.saturating_add(increment)\n}\n"
    );
    assert_eq!(real_env_task_row(&env).state, TaskState::VerifiedComplete);
    assert_eq!(
        env.manager.shadow_row(env.parent).unwrap().unwrap().state,
        ShadowRowState::Integrated
    );
}

#[tokio::test]
pub(crate) async fn single_item_verified_complete_implies_owner_integration_landed() {
    // THE ordering invariant (P0-49): a single-item shadowed run may only
    // reach VerifiedComplete AFTER a successful owner landing. The crash
    // seam is placed after candidate verification, BEFORE the landing
    // transaction's first durable row: the task must still be non-terminal
    // and the owner byte-identical. Only the resumed settlement lands, and
    // the completion proof is bound to the finalized integration record of
    // that exact landed snapshot.
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env(
        dir.path(),
        vec![
            vec![
                ScriptedResponse::ToolCall {
                    id: "c1".into(),
                    name: "write_file".into(),
                    input: serde_json::json!({
                        "path": "src/lib.rs",
                        "content": "pub fn value() -> u64 {\n    let base_amount: u64 = 11;\n    let increment: u64 = 31;\n    base_amount.saturating_add(increment)\n}\n",
                    }),
                },
                ScriptedResponse::Text("done".into()),
                ScriptedResponse::End,
            ],
            vec![ScriptedResponse::End],
        ],
        faktor_agent::VerificationService::fake_ok(),
        false,
    );
    seed_rust(&env);
    let owner_before = owner_digest(&env);
    // The seam is armed BEFORE the drive ends: the FIRST settlement (the
    // post-drive hook or the watcher) verifies the candidate, then stops.
    env.executor
        .set_settlement_crash_seam(Some(CrashSeam::AfterPreparedVerification));
    let receipt = env
        .executor
        .start_task(
            env.parent,
            real_mutating_request(&env, "implement the change"),
        )
        .expect("shadowed start");
    assert_eq!(receipt.mode, TaskRunMode::InSession);
    let row = env.manager.shadow_row(env.parent).unwrap().expect("row");
    let shadow_dir = std::path::PathBuf::from(&row.root);
    wait_until(
        || real_state_of(&env) == faktor_core::state::AgentState::ReadyForNextTurn,
        60,
    )
    .await;
    // Wait until the seam actually fired: a candidate-bound verification
    // record exists (the drive's own claim carries no tree hash) while the
    // shadow is still live and the owner is untouched.
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    wait_until(
        || {
            h.list_verification_records(task_id)
                .map(|records| records.iter().any(|r| r.tree_hash.is_some()))
                .unwrap_or(false)
                && env
                    .manager
                    .shadow_row(env.parent)
                    .unwrap()
                    .is_some_and(|r| r.state == ShadowRowState::Active)
                && owner_digest(&env) == owner_before
        },
        60,
    )
    .await;
    let task = real_env_task_row(&env);
    assert_ne!(
        task.state,
        TaskState::VerifiedComplete,
        "completion cannot precede the owner landing"
    );
    assert!(!task.state.is_terminal());
    assert_eq!(
        owner_digest(&env),
        owner_before,
        "owner untouched pre-landing"
    );
    let candidate = root_digest(&shadow_dir);
    // Resume: the explicit settlement re-drives the durable phases and lands.
    env.executor.set_settlement_crash_seam(None);
    let outcome = env
        .executor
        .settle_run(RunSettlement::InSession {
            parent: env.parent,
            run_id: format!("tx-session-{}", env.parent.raw()),
        })
        .await
        .expect("resumed settlement");
    assert!(outcome.completed, "{outcome:?}");
    assert_eq!(
        owner_digest(&env),
        candidate,
        "the candidate landed exactly"
    );
    let task = real_env_task_row(&env);
    assert_eq!(task.state, TaskState::VerifiedComplete);
    // The completion proof is bound to the finalized integration record of
    // the landed snapshot (the ledger sequence completion requires).
    let integration = h
        .ledger_integration_record_for_task(task_id.raw())
        .unwrap()
        .expect("a finalized integration record exists");
    assert_eq!(integration.final_snapshot_hash, candidate);
    assert_eq!(
        integration.landed_snapshot.as_deref(),
        Some(candidate.as_str())
    );
    assert!(h
        .list_verification_records(task_id)
        .unwrap()
        .iter()
        .any(|r| r.tree_hash.as_deref() == Some(integration.final_snapshot_hash.as_str())));
    assert_eq!(
        env.manager.shadow_row(env.parent).unwrap().unwrap().state,
        ShadowRowState::Integrated
    );
}

#[tokio::test]
pub(crate) async fn later_file_conflict_mid_land_rolls_back_earlier_applies_byte_identically() {
    // The transactional landing invariant (P0-49): a conflict on a LATER
    // path (after an earlier path was applied) rolls the earlier apply BACK,
    // leaving the owner BYTE-IDENTICAL to its pre-landing state — never a
    // partial landing. The task stays non-terminal and completion remains
    // impossible.
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env(
        dir.path(),
        vec![
            vec![
                ScriptedResponse::ToolCall {
                    id: "c1".into(),
                    name: "write_file".into(),
                    input: serde_json::json!({
                        "path": "src/lib.rs",
                        "content": "pub fn value() -> u64 {\n    let base_amount: u64 = 11;\n    let increment: u64 = 31;\n    base_amount.saturating_add(increment)\n}\n",
                    }),
                },
                ScriptedResponse::Text("done".into()),
                ScriptedResponse::End,
            ],
            vec![ScriptedResponse::End],
        ],
        faktor_agent::VerificationService::fake_ok(),
        true,
    );
    seed_rust(&env);
    std::fs::write(env.owner_root.join("a.txt"), b"base-alpha").unwrap();
    // Stop the FIRST settlement after ONE applied path ("a.txt" sorts
    // before "src/lib.rs").
    env.executor
        .set_settlement_crash_seam(Some(CrashSeam::IntegrationApply { after: 1 }));
    let receipt = env
        .executor
        .start_task(
            env.parent,
            real_mutating_request(&env, "implement the change"),
        )
        .expect("shadowed start");
    assert_eq!(receipt.mode, TaskRunMode::InSession);
    let row = env.manager.shadow_row(env.parent).unwrap().expect("row");
    let shadow_dir = std::path::PathBuf::from(&row.root);
    // Park the drive after its shadow write; stage the SECOND changed path
    // directly in the shadow while it runs.
    wait_until(|| env.fired.load(Ordering::SeqCst) >= 1, 300).await;
    std::fs::write(shadow_dir.join("a.txt"), b"agent a").unwrap();
    assert_eq!(
        std::fs::read(env.owner_root.join("a.txt")).unwrap(),
        b"base-alpha"
    );
    env.gate.notify_waiters();
    wait_until(
        || real_state_of(&env) == faktor_core::state::AgentState::ReadyForNextTurn,
        60,
    )
    .await;
    // The first settlement applies a.txt, then the seam stops it (leaving a
    // recoverable partial landing journaled by phase).
    wait_until(
        || std::fs::read(env.owner_root.join("a.txt")).unwrap_or_default() == b"agent a",
        60,
    )
    .await;
    // The user edits the LATER path before the resume.
    std::fs::write(
        env.owner_root.join("src/lib.rs"),
        b"pub fn value() -> u64 { 99 }\n",
    )
    .unwrap();
    // Reference pre-landing tree: the base content + the user edit.
    let reference = dir.path().join("reference");
    std::fs::create_dir_all(reference.join("src")).unwrap();
    std::fs::write(
        reference.join("Cargo.toml"),
        std::fs::read(env.owner_root.join("Cargo.toml")).unwrap(),
    )
    .unwrap();
    std::fs::write(reference.join("a.txt"), b"base-alpha").unwrap();
    std::fs::write(
        reference.join("src/lib.rs"),
        b"pub fn value() -> u64 { 99 }\n",
    )
    .unwrap();
    let expected = root_digest(&reference);
    // Resume: the later-path CAS conflicts, every applied path rolls back.
    env.executor.set_settlement_crash_seam(None);
    let outcome = env
        .executor
        .settle_run(RunSettlement::InSession {
            parent: env.parent,
            run_id: format!("tx-session-{}", env.parent.raw()),
        })
        .await
        .expect("a conflict is a typed settle outcome, not a failure");
    let finalize = outcome.finalize.as_ref().expect("a finalize outcome");
    assert_eq!(
        finalize.action,
        ShadowFinalizeAction::IntegrationBlocked,
        "typed conflict, not a partial landing: {outcome:?}"
    );
    assert!(
        finalize
            .conflicts
            .iter()
            .any(|(_, detail)| detail.contains("src/lib.rs") && detail.contains("rolled back")),
        "{outcome:?}"
    );
    assert!(!outcome.completed);
    assert_eq!(
        owner_digest(&env),
        expected,
        "the owner is byte-identical to its pre-landing state (earlier applies rolled back, the user edit preserved)"
    );
    assert_eq!(
        std::fs::read(env.owner_root.join("a.txt")).unwrap(),
        b"base-alpha"
    );
    assert_eq!(
        std::fs::read(env.owner_root.join("src/lib.rs")).unwrap(),
        b"pub fn value() -> u64 { 99 }\n"
    );
    assert_eq!(
        env.manager.shadow_row(env.parent).unwrap().unwrap().state,
        ShadowRowState::IntegrationBlocked
    );
    assert!(shadow_dir.is_dir(), "shadow retained on conflict");
    let task = real_env_task_row(&env);
    assert_ne!(task.state, TaskState::VerifiedComplete);
    assert!(!task.state.is_terminal());
    // Even the shadow-world completion is refused while the landing is
    // blocked: no completion without a successful owner integration.
    let err = complete_shadow_world_or_refuse(&env.manager, env.parent)
        .expect_err("shadow-world completion must stay impossible");
    let _ = err;
    assert_ne!(
        real_env_task_row(&env).state,
        TaskState::VerifiedComplete,
        "no completion without a successful owner integration"
    );
}
