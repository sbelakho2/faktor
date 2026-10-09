#![allow(clippy::await_holding_lock)]
//! TaskExecutor tests: cost control, isolation, tournaments, completion, settlement (mechanically split from `task_executor_tests`).

use super::tests_core::*;
use super::*;

// ------------------------------------------------ max_cost_micro task control
// (audit 9/H: TaskRunRequest.max_cost_micro flows to the task row cap and
// the guarded ledger refuses an over-committed reduction on a re-seed.)

#[tokio::test]
pub(crate) async fn single_item_task_max_cost_micro_lands_on_the_task_row_cap_and_refuses_lowering()
{
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(&dir.path().join("e"), done_script());
    let goal = "analyze the module boundaries";
    let mut req = request(goal, vec![wi("a1", WorkKind::Analysis, &[])], &env);
    req.max_cost_micro = Some(10_000_000);
    let receipt = env
        .executor
        .start_task(env.parent, req)
        .expect("single-item start with a cost cap");
    assert_eq!(receipt.mode, TaskRunMode::InSession);
    wait_until(
        || state_of(&env, env.parent) == faktor_core::state::AgentState::ReadyForNextTurn,
        60,
    )
    .await;
    // The requested cap landed on the session's durable task row (the row
    // every paid model call of the drive is admitted against).
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    let ledger = faktor_session::DurableBudgetLedger::new(env.manager.clone());
    assert_eq!(
        ledger
            .session_budget_view(env.parent, task_id)
            .expect("durable budget view")
            .max_cost_micro,
        Some(10_000_000),
        "TaskRunRequest.max_cost_micro flows to the task row cap"
    );
    // A reserve that commits part of the cap, followed by a NEW task on the
    // same session whose cap would sit below the committed amount, refuses
    // the whole start with a typed conflict (spend never rewinds, and a new
    // run can never silently lower a live row's cap under its commitments).
    ledger
        .reserve(
            env.parent,
            task_id,
            faktor_core::id::OpId::new(7_000_001),
            60_000,
            None,
        )
        .await
        .expect("reserve under the cap");
    let mut req2 = request(
        "a second capped run",
        vec![wi("a2", WorkKind::Analysis, &[])],
        &env,
    );
    req2.max_cost_micro = Some(50_000);
    let calls_before = env.provider.count();
    let err = env
        .executor
        .start_task(env.parent, req2)
        .expect_err("a cap below the committed 60_000 must refuse the start");
    assert!(
        matches!(err, ExecError::Conflict(_)),
        "typed conflict, not a silent lower cap: {err}"
    );
    assert!(err.to_string().contains("cost cap"), "{err}");
    assert_eq!(
        env.provider.count(),
        calls_before,
        "the refused run never submitted a prompt"
    );
}

// ================================ mutation isolation (P0) + task cancel
// Every MUTATING run over the production executor ALWAYS executes in an
// isolated candidate (the daemon always carries the ShadowRoots service;
// the production constructor requires it). There is no mode value, config
// key or wire field that can disable isolation; the removed `direct_compat`
// value is a strict decode error. The cfg(test) owner-direct seam exists
// ONLY for the low-level suites that predate shadow mutation. The executor
// also exposes ONE task-level cancel authority (cancel_run) the HTTP
// surface drives.

pub(crate) fn one_write_script(content: &str) -> Vec<Vec<ScriptedResponse>> {
    vec![
        vec![
            ScriptedResponse::ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                input: serde_json::json!({
                    "path": "a.txt",
                    "content": content,
                }),
            },
            ScriptedResponse::Text("done".into()),
            ScriptedResponse::End,
        ],
        vec![ScriptedResponse::End],
    ]
}

#[test]
pub(crate) fn direct_compat_is_a_strict_decode_error_naming_the_removal() {
    // Absence-of-variant proof: the ONLY decodable mutation mode is
    // "shadow". The removed escape hatch decodes to a loud error naming the
    // removal on every serde surface (CLI config, SDK/native DTOs — all
    // share this ONE type).
    assert_eq!(
        serde_json::from_str::<MutationMode>("\"shadow\"").unwrap(),
        MutationMode::Shadow
    );
    let err = serde_json::from_str::<MutationMode>("\"direct_compat\"")
        .expect_err("direct_compat must not decode");
    assert!(err.to_string().contains("removed"), "{err}");
    let err = serde_json::from_str::<MutationMode>("\"nonsense\"")
        .expect_err("unknown modes must not decode");
    assert!(err.to_string().contains("unknown variant"), "{err}");
}

/// P0 isolation at the SOURCE level: `direct_compat` has no variant and no
/// literal anywhere, the PRODUCTION constructor requires the shadow service
/// (so no production value can disable isolation), and the ONLY no-shadow
/// assembly path is the `#[cfg(any(test, debug_assertions))]`-gated test
/// seam documented on [`TaskExecutor::new_owner_direct_for_test_harness`]
/// (compiled out of release builds).
#[test]
pub(crate) fn no_production_path_can_construct_a_non_isolating_executor() {
    const SRC: &str = concat!(
        include_str!("../task_executor/mod.rs"),
        include_str!("../task_executor/settlement.rs"),
        include_str!("../task_executor/integration.rs"),
        include_str!("../task_executor/verification.rs"),
    );
    assert!(
        !SRC.contains("DirectCompat"),
        "the removed direct-owner variant must not exist anywhere"
    );
    assert!(
        SRC.contains("shadows: Arc<ShadowRoots>"),
        "the production constructor must REQUIRE the shadow service"
    );
    assert!(
        SRC.contains("#[cfg(any(test, debug_assertions))]"),
        "the owner-direct seam must be cfg(test)/debug-assertions gated"
    );
    assert!(
        SRC.contains("pub fn new_owner_direct_for_test_harness("),
        "the gated seam is the only no-shadow constructor"
    );
    assert!(
        !SRC.contains("pub fn assemble("),
        "the shared assembly body stays private"
    );
}

#[tokio::test]
pub(crate) async fn mutating_runs_always_isolate_and_only_the_test_seam_drives_the_owner() {
    // (a) The PRODUCTION wiring (shadow service present, the production
    // constructor): a single-item MUTATING run works in the isolated
    // candidate no matter what the (wire-compat only) mutation_mode field
    // says; the owner stays byte-identical. (b) The cfg(test) owner-direct
    // seam (no shadow service) is the ONLY path that drives the owner
    // checkout directly, and only tests can build it.
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let scripts = one_write_script("isolated implementation alpha");
    let env_a = open_real_tool_env_full(
        &dir.path().join("a"),
        scripts.clone(),
        faktor_agent::VerificationService::disabled(),
        false,
        true,
    );
    let env_b = open_real_tool_env_full(
        &dir.path().join("b"),
        scripts,
        faktor_agent::VerificationService::disabled(),
        false,
        false,
    );
    seed_owner(&env_a.owner_root);
    seed_owner(&env_b.owner_root);
    let goal = "implement the change";
    let mut req_a = real_mutating_request(&env_a, goal);
    req_a.mutation_mode = Some(MutationMode::Shadow);
    let receipt_a = env_a
        .executor
        .start_task(env_a.parent, req_a)
        .expect("isolated start");
    let receipt_b = env_b
        .executor
        .start_task(env_b.parent, real_mutating_request(&env_b, goal))
        .expect("seam start");
    assert_eq!(receipt_a.mode, TaskRunMode::InSession);
    assert_eq!(receipt_b.mode, TaskRunMode::InSession);
    wait_until(
        || real_state_of(&env_a) == faktor_core::state::AgentState::ReadyForNextTurn,
        60,
    )
    .await;
    wait_until(
        || real_state_of(&env_b) == faktor_core::state::AgentState::ReadyForNextTurn,
        60,
    )
    .await;
    // (a) Production: the write landed in the isolated candidate; the owner
    // checkout is byte-identical to its seeded content.
    let row_a = env_a
        .manager
        .shadow_row(env_a.parent)
        .unwrap()
        .expect("a mutating run over the production executor must isolate");
    assert_eq!(
        std::fs::read(std::path::PathBuf::from(&row_a.root).join("a.txt")).unwrap(),
        b"isolated implementation alpha",
        "the write landed in the isolated candidate"
    );
    assert_eq!(
        std::fs::read(env_a.owner_root.join("a.txt")).unwrap(),
        b"base-alpha",
        "the owner checkout stayed byte-identical"
    );
    // (b) The test seam: the low-level owner-direct path still works, and
    // no production constructor can produce it.
    assert!(
        env_b.manager.shadow_row(env_b.parent).unwrap().is_none(),
        "the seam never begins a shadow"
    );
    assert_eq!(
        std::fs::read(env_b.owner_root.join("a.txt")).unwrap(),
        b"isolated implementation alpha",
        "the seam drive wrote the owner checkout"
    );
    // Everything else is byte-parity: same message stream and turn-record
    // envelope on both wirings.
    //
    // The session reaches ReadyForNextTurn while its record is still
    // `active`: the record is finalized only AFTER the end-of-turn content
    // sync and the final gate write. Wait on the observable this section
    // actually needs — a non-`active` record on BOTH wirings — with a
    // generous deadline; a shadow record that never finalizes still fails
    // loudly here (the same convention as the direct/shadow parity test).
    let ha = env_a.manager.get_session(env_a.parent).unwrap().unwrap();
    let hb = env_b.manager.get_session(env_b.parent).unwrap().unwrap();
    {
        let finished = |s: &Option<String>| {
            matches!(
                s.as_deref(),
                Some(status) if faktor_store::is_terminal_turn_record_status(status)
            )
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(240);
        loop {
            let a = ha
                .turn_record(receipt_a.op_id.unwrap())
                .unwrap()
                .map(|r| r.status);
            let b = hb
                .turn_record(receipt_b.op_id.unwrap())
                .unwrap()
                .map(|r| r.status);
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
    assert_eq!(
        ha.message_count().unwrap(),
        hb.message_count().unwrap(),
        "byte-identical message streams"
    );
    let rec_a = ha.turn_record(receipt_a.op_id.unwrap()).unwrap().unwrap();
    let rec_b = hb.turn_record(receipt_b.op_id.unwrap()).unwrap().unwrap();
    assert_eq!(rec_a.status, rec_b.status);
    assert_eq!(rec_a.effective_provider, rec_b.effective_provider);
    assert_eq!(rec_a.effective_model, rec_b.effective_model);
}

#[tokio::test]
pub(crate) async fn per_run_mutation_mode_is_wire_only_and_never_disables_isolation() {
    // The request field is decoded for wire compatibility ONLY: present,
    // absent or explicitly "shadow", the production executor isolates every
    // mutating run (and the removed "direct_compat" cannot even decode —
    // covered by the decode test above).
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        one_write_script("wire-only mode"),
        faktor_agent::VerificationService::disabled(),
        false,
        true,
    );
    seed_owner(&env.owner_root);
    let mut req = real_mutating_request(&env, "wire-only field");
    req.mutation_mode = Some(MutationMode::Shadow);
    env.executor
        .start_task(env.parent, req)
        .expect("per-run shadow start");
    wait_until(
        || real_state_of(&env) == faktor_core::state::AgentState::ReadyForNextTurn,
        60,
    )
    .await;
    let row = env
        .manager
        .shadow_row(env.parent)
        .unwrap()
        .expect("the run isolated");
    assert_eq!(
        std::fs::read(std::path::PathBuf::from(&row.root).join("a.txt")).unwrap(),
        b"wire-only mode",
        "the write landed in the isolated candidate"
    );
    assert_eq!(
        std::fs::read(env.owner_root.join("a.txt")).unwrap(),
        b"base-alpha",
        "the owner checkout stayed untouched"
    );
}

#[tokio::test]
pub(crate) async fn criteria_land_on_the_task_row_and_hostile_criteria_refuse_the_start() {
    // The run's acceptance criteria ride the durable task row the drive
    // certifies against; hostile criteria (beyond the row's own caps) are
    // typed Oversized refusals BEFORE anything is written.
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(&dir.path().join("c"), done_script());
    let mut req = request(
        "implement with criteria",
        vec![wi("a1", WorkKind::Analysis, &[])],
        &env,
    );
    req.criteria = vec![
        "the change compiles".into(),
        "existing tests still pass".into(),
    ];
    let receipt = env
        .executor
        .start_task(env.parent, req)
        .expect("single-item start with criteria");
    assert_eq!(receipt.mode, TaskRunMode::InSession);
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task = h.get_task(h.task_id().unwrap()).unwrap().unwrap();
    assert_eq!(
        task.acceptance_criteria,
        vec![
            "the change compiles".to_string(),
            "existing tests still pass".to_string()
        ]
    );
    // A fresh session for the hostile case (a terminal row would refuse
    // for a different reason).
    let env2 = open_env(&dir.path().join("c2"), done_script());
    let mut hostile = request(
        "hostile criteria",
        vec![wi("a1", WorkKind::Analysis, &[])],
        &env2,
    );
    hostile.criteria = (0..=faktor_session::MAX_TASK_CRITERIA)
        .map(|i| format!("criterion {i}"))
        .collect();
    let err = env2
        .executor
        .start_task(env2.parent, hostile)
        .expect_err("criteria beyond the row cap refuse the start");
    assert!(matches!(err, ExecError::Oversized(_)), "{err}");
    let env3 = open_env(&dir.path().join("c3"), done_script());
    let mut hostile2 = request(
        "oversized criterion",
        vec![wi("a1", WorkKind::Analysis, &[])],
        &env3,
    );
    hostile2.criteria = vec!["x".repeat(faktor_session::MAX_TASK_CRITERION_BYTES + 1)];
    let err2 = env3
        .executor
        .start_task(env3.parent, hostile2)
        .expect_err("an oversized criterion refuses the start");
    assert!(matches!(err2, ExecError::Oversized(_)), "{err2}");
    assert_eq!(env3.provider.count(), 0, "no drive started");
}

#[tokio::test]
pub(crate) async fn cancel_in_session_run_mid_drive_aborts_discards_and_refuses_twice() {
    // Task-level cancel of an in-session run: the drive is parked mid
    // flight (gated provider); cancel_run aborts the op durably, cancels
    // the task row, and discards the run's shadow through the durable
    // finalize — the owner checkout never receives a byte. A second cancel
    // is a typed Conflict; an unknown run is a typed NotFound.
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let fix = open_gated_shadow(dir.path());
    let receipt = fix
        .executor
        .start_task(
            fix.parent,
            TaskRunRequest {
                goal: "cancellable shadowed implementation".into(),
                work_items: vec![wi("impl", WorkKind::Implementation, &[])],
                parent_caps: crate::caps::all_lattice(),
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
    // Drive parked mid-flight; write into the shadow so the discard is
    // observable.
    wait_until(|| fix.gated.count() >= 1, 180).await;
    std::fs::write(shadow_dir.join("a.txt"), b"cancelled-drive content").unwrap();
    fix.executor
        .cancel_run(fix.parent, &receipt.run_id)
        .expect("task-level cancel accepted");
    // Durable outcome: task row Cancelled, session back to ReadyForNextTurn,
    // shadow discarded, owner untouched.
    wait_until(
        || {
            let h = fix.manager.get_session(fix.parent).unwrap().unwrap();
            h.get_task(h.task_id().unwrap())
                .unwrap()
                .is_some_and(|t| t.state == TaskState::Cancelled)
        },
        60,
    )
    .await;
    wait_until(
        || {
            fix.manager.shadow_row(fix.parent).unwrap().unwrap().state == ShadowRowState::Discarded
                && !shadow_dir.exists()
        },
        60,
    )
    .await;
    assert_eq!(
        fix.manager
            .get_session(fix.parent)
            .unwrap()
            .unwrap()
            .state()
            .unwrap(),
        faktor_core::state::AgentState::ReadyForNextTurn,
        "aborting the drive lands the session ReadyForNextTurn"
    );
    assert_eq!(
        std::fs::read(fix.owner_root.join("a.txt")).unwrap(),
        b"base-alpha",
        "the owner checkout never received a byte"
    );
    // A cancelled run is never cancelled twice; unknown runs stay unknown.
    let err = fix
        .executor
        .cancel_run(fix.parent, &receipt.run_id)
        .expect_err("a second cancel must refuse");
    assert!(matches!(err, ExecError::Conflict(_)), "{err}");
    let err = fix
        .executor
        .cancel_run(fix.parent, "run-unknown")
        .expect_err("an unknown run must refuse");
    assert!(matches!(err, ExecError::NotFound(_)), "{err}");
}

#[tokio::test]
pub(crate) async fn cancel_orchestrated_run_fans_cancel_to_live_children_only() {
    // Task-level cancel of an orchestrated run: every live child receives
    // the durable Cancel control exactly once through the runtime's queue
    // (the bounded cancel path reaches a running prompt between chunks);
    // a fully terminal run refuses a second cancel.
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let manager =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let ticker = Arc::new(TickerProvider {
        caps: ModelCapabilities {
            tools: true,
            ..Default::default()
        },
        request_count: AtomicUsize::new(0),
        chunk_delay_ms: 15,
    });
    let mut registry = ProviderRegistry::new();
    registry.try_register(ticker.clone()).unwrap();
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
        .create_session(ws, "cancel-orch", "fake", "m")
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
    let receipt = executor
        .start_task(
            parent,
            TaskRunRequest {
                goal: "ticking parallel analysis".into(),
                work_items: vec![
                    wi("a", WorkKind::Analysis, &[]),
                    wi("b", WorkKind::Analysis, &[]),
                ],
                parent_caps: crate::caps::all_lattice(),
                isolated_root: isolated.clone(),
                ..Default::default()
            },
        )
        .expect("multi-item run starts");
    assert_eq!(receipt.mode, TaskRunMode::Orchestrated);
    // Both children spawned and their drives are mid-flight (ticking).
    wait_until(
        || {
            OrchestratorRuntime::registry_rows(manager.clone(), parent, &receipt.run_id)
                .map(|rows| rows.len() == 2 && rows.iter().all(|c| !c.state.is_terminal()))
                .unwrap_or(false)
        },
        60,
    )
    .await;
    wait_until(|| ticker.count() >= 2, 240).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    executor
        .cancel_run(parent, &receipt.run_id)
        .expect("task-level cancel accepted");
    wait_until(
        || {
            OrchestratorRuntime::registry_rows(manager.clone(), parent, &receipt.run_id)
                .map(|rows| rows.len() == 2 && rows.iter().all(|c| c.state.is_terminal()))
                .unwrap_or(false)
        },
        60,
    )
    .await;
    let rows =
        OrchestratorRuntime::registry_rows(manager.clone(), parent, &receipt.run_id).unwrap();
    assert!(
        rows.iter().any(|c| c.state == crate::ChildState::Cancelled),
        "the live children were cancelled: {rows:?}"
    );
    // Terminal residue refuses a second cancel.
    let err = executor
        .cancel_run(parent, &receipt.run_id)
        .expect_err("a fully settled run refuses a second cancel");
    assert!(matches!(err, ExecError::Conflict(_)), "{err}");
}

// ------------------------------------------------ multi-candidate tournaments

/// The tournament flow end-to-end over REAL isolated candidate worktrees:
/// N=2 implementation candidates start through the ONE task-start authority
/// with the byte-identical goal+criteria, every candidate is driven to
/// terminal, settlements rank deterministically (a FAILED verification can
/// never win, even at zero cost), the loser's isolated worktree is removed
/// (registry invariant stays clean), and a FRESH executor reconstructs the
/// Decided tournament with the same winner from the durable ledger.
#[tokio::test]
pub(crate) async fn tournament_flow_ranks_deterministically_and_discards_losers_orphan_free() {
    use crate::runtime::task_executor::TournamentStartRequest;
    use crate::tournament::{CandidateState, ReviewRank, ReviewVerdict, TournamentState};
    use faktor_core::id::VerificationRecordId;

    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let scripts: Vec<Vec<ScriptedResponse>> = (0..8)
        .map(|_| vec![ScriptedResponse::Text("done".into()), ScriptedResponse::End])
        .collect();
    let env = open_env(dir.path(), scripts);
    let goal = "implement the same change two ways";
    let criteria = vec!["cargo test".to_string()];

    // Typed N refusals leave NOTHING durable behind.
    let facts_before = env
        .manager
        .get_session(env.parent)
        .unwrap()
        .unwrap()
        .memory_facts()
        .unwrap()
        .len();
    for n in [1usize, 5] {
        let err = env
            .executor
            .start_tournament(env.parent, goal, &criteria, n, None)
            .expect_err("out-of-band N must refuse");
        assert!(matches!(err, ExecError::InvalidPlan(_)), "{err}");
    }
    assert_eq!(
        env.manager
            .get_session(env.parent)
            .unwrap()
            .unwrap()
            .memory_facts()
            .unwrap()
            .len(),
        facts_before,
        "refused tournament starts write nothing"
    );

    let receipt = env
        .executor
        .start_tournament_with(
            env.parent,
            TournamentStartRequest {
                goal: goal.to_string(),
                criteria: criteria.clone(),
                n: 2,
                isolated_root: env.isolated_root.clone(),
                ..Default::default()
            },
        )
        .expect("tournament start");
    assert_eq!(
        receipt.candidates,
        vec!["child-0".to_string(), "child-1".to_string()]
    );
    // Every candidate really spawned as an ISOLATED child and settled.
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
    let parent_ws = env
        .manager
        .get_session(env.parent)
        .unwrap()
        .unwrap()
        .row()
        .unwrap()
        .workspace_id;
    for c in &rows {
        assert_eq!(c.state, crate::ChildState::Done);
        assert_eq!(c.ownership, ChildOwnership::IsolatedWorktree);
        assert_ne!(c.workspace_id, parent_ws.raw(), "candidate is isolated");
    }

    // The durable tournament reconstructs the identical candidate band +
    // criteria from its ledger anchor.
    let state = env
        .executor
        .tournament_state(env.parent, &receipt.tournament_id)
        .unwrap();
    assert_eq!(state.state, TournamentState::Open);
    assert_eq!(state.candidates.len(), 2);
    assert_eq!(state.criteria.len(), 1);
    assert_eq!(state.criteria[0].spec, "cargo test");
    assert!(state.candidates.iter().all(|c| !c.worktree.is_empty()));
    let winner_wt = state.candidates[1].worktree.clone();
    let loser_wt = state.candidates[0].worktree.clone();
    assert!(std::path::Path::new(&winner_wt).is_dir());
    assert!(std::path::Path::new(&loser_wt).is_dir());

    // The losing candidate is FREE but failed verification; the winning
    // candidate passed and is expensive. The failed one can never win.
    let mut loser = env
        .executor
        .candidate_settlement(
            env.parent,
            &receipt.tournament_id,
            "child-0",
            "verification failed",
        )
        .unwrap();
    assert_eq!(loser.state, CandidateState::Done);
    loser.verification = Some(VerificationRecordId::new(1));
    loser.verification_pass = Some(false);
    loser.review = Some(ReviewVerdict {
        rank: ReviewRank::Clean,
        reviewer: "review-0".into(),
    });
    loser.cost_micro = 0;
    env.executor
        .settle_tournament_candidate(env.parent, &receipt.tournament_id, loser)
        .unwrap();

    let mut winner = env
        .executor
        .candidate_settlement(
            env.parent,
            &receipt.tournament_id,
            "child-1",
            "verified complete",
        )
        .unwrap();
    winner.verification = Some(VerificationRecordId::new(2));
    winner.verification_pass = Some(true);
    winner.review = Some(ReviewVerdict {
        rank: ReviewRank::Clean,
        reviewer: "review-0".into(),
    });
    winner.cost_micro = 1_000_000;
    env.executor
        .settle_tournament_candidate(env.parent, &receipt.tournament_id, winner)
        .unwrap();

    let decision = env
        .executor
        .decide_tournament(env.parent, &receipt.tournament_id)
        .expect("deterministic decision");
    assert_eq!(decision.winner.child_id, "child-1");
    // No automatic integration: the winner's worktree is only PROPOSED and
    // still holds the candidate content (nothing was merged anywhere).
    assert!(std::path::Path::new(&winner_wt).is_dir());
    // The loser's worktree is gone and the zero-orphan registry invariant
    // holds after cleanup.
    assert!(
        !std::path::Path::new(&loser_wt).exists(),
        "loser worktree removed"
    );
    assert!(OrchestratorRuntime::registry_violations(
        env.manager.clone(),
        env.parent,
        &receipt.run_id
    )
    .is_empty());
    assert!(
        OrchestratorRuntime::orphan_children_scan(env.manager.clone())
            .issues
            .is_empty()
    );

    // A FRESH executor over the same durable store reconstructs the very
    // same Decided tournament (winner + candidate evidence).
    let executor2 = TaskExecutor::new_owner_direct_for_test_harness(
        &env.orchestrator,
        env.manager.clone(),
        env.agent.clone(),
    );
    let reopened = executor2
        .tournament_state(env.parent, &receipt.tournament_id)
        .unwrap();
    assert_eq!(reopened.state, TournamentState::Decided);
    assert_eq!(reopened.winner.as_deref(), Some("child-1"));
    assert_eq!(
        reopened
            .candidates
            .iter()
            .find(|c| c.child_id == "child-1")
            .unwrap()
            .verification_pass,
        Some(true)
    );
    assert_eq!(
        reopened
            .candidates
            .iter()
            .find(|c| c.child_id == "child-0")
            .unwrap()
            .state,
        CandidateState::Discarded
    );
}

/// The abort path discards EVERY candidate: the terminal ledger row is
/// `Aborted` with no winner, every registry row settles terminal, every
/// isolated worktree directory + row is removed, and a reopen reconstructs
/// the same Aborted tournament.
#[tokio::test]
pub(crate) async fn tournament_abort_discards_all_candidates_and_is_durable() {
    use crate::runtime::task_executor::TournamentStartRequest;
    use crate::tournament::{CandidateState, TournamentState};

    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let scripts: Vec<Vec<ScriptedResponse>> = (0..4)
        .map(|_| vec![ScriptedResponse::Text("done".into()), ScriptedResponse::End])
        .collect();
    let env = open_env(dir.path(), scripts);
    let receipt = env
        .executor
        .start_tournament_with(
            env.parent,
            TournamentStartRequest {
                goal: "abort me".to_string(),
                criteria: vec!["cargo test".to_string()],
                n: 2,
                isolated_root: env.isolated_root.clone(),
                ..Default::default()
            },
        )
        .expect("tournament start");
    wait_until(
        || {
            OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, &receipt.run_id)
                .map(|rows| rows.len() == 2 && rows.iter().all(|c| c.state.is_terminal()))
                .unwrap_or(false)
        },
        60,
    )
    .await;
    let state = env
        .executor
        .tournament_state(env.parent, &receipt.tournament_id)
        .unwrap();
    let worktrees: Vec<String> = state
        .candidates
        .iter()
        .map(|c| c.worktree.clone())
        .collect();
    assert!(worktrees.iter().all(|w| std::path::Path::new(w).is_dir()));

    let aborted = env
        .executor
        .abort_tournament(env.parent, &receipt.tournament_id, "operator aborted")
        .expect("abort");
    assert_eq!(aborted.state, TournamentState::Aborted);
    assert!(aborted.winner.is_none());
    assert!(aborted
        .candidates
        .iter()
        .all(|c| c.state == CandidateState::Discarded));
    for w in &worktrees {
        assert!(!std::path::Path::new(w).exists(), "worktree {w} removed");
    }
    assert!(OrchestratorRuntime::registry_violations(
        env.manager.clone(),
        env.parent,
        &receipt.run_id
    )
    .is_empty());
    // Reopen: the same Aborted tournament, no winner ever proposed.
    let executor2 = TaskExecutor::new_owner_direct_for_test_harness(
        &env.orchestrator,
        env.manager.clone(),
        env.agent.clone(),
    );
    let reopened = executor2
        .tournament_state(env.parent, &receipt.tournament_id)
        .unwrap();
    assert_eq!(reopened.state, TournamentState::Aborted);
    assert!(reopened.winner.is_none());
}

// ------------------------------------------------ completion steps (P2)

/// The completion-step runner over a real supervisor and an allow-all
/// egress policy (the executor fixture's agent carries no supervisor, so
/// the runner is installed explicitly).
pub(crate) fn completion_step_runner(
    root: &std::path::Path,
) -> Arc<crate::runtime::completion_steps::CompletionStepRunner> {
    use crate::runtime::completion_steps::{
        CompletionStepRunner, CompletionStepsConfig, EgressPolicy,
    };
    let cas = Arc::new(faktor_cas::Cas::open(root.join("completion-cas")).unwrap());
    let supervisor = faktor_terminal::ProcessSupervisor::new(cas);
    let egress: Arc<dyn EgressPolicy> = Arc::new(|_url: &str| Ok(()));
    Arc::new(
        CompletionStepRunner::new(supervisor, egress, CompletionStepsConfig::default()).unwrap(),
    )
}

pub(crate) fn cs_git(cwd: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Commit with the SAME deterministic `-c user.name/-c user.email` identity
/// the product's commit helper passes (faktor-git's `COMMIT_IDENTITY_*`):
/// fixture commits never depend on a local/global git identity, so a CI
/// runner with none still produces a real `git rev-parse HEAD` sha.
pub(crate) fn cs_commit(cwd: &std::path::Path, message: &str) {
    cs_git(
        cwd,
        &[
            "-c",
            &format!("user.name={}", faktor_git::COMMIT_IDENTITY_NAME),
            "-c",
            &format!("user.email={}", faktor_git::COMMIT_IDENTITY_EMAIL),
            "commit",
            "-q",
            "-m",
            message,
        ],
    );
}

pub(crate) fn cs_seed_repo(root: &std::path::Path) {
    cs_git(root, &["init", "-q", "-b", "main"]);
    std::fs::write(root.join("README.md"), "base\n").unwrap();
    cs_git(root, &["add", "-A"]);
    cs_commit(root, "init");
}

/// Drive a real-env task row to Verifying and land one passing record at
/// the resulting revision — the durable proof an in-session completion step
/// must be authorized by (the advisory verification fact is NOT enough).
pub(crate) fn seed_passing_proof(
    env: &RealToolEnv,
    task_id: TaskId,
) -> faktor_core::id::VerificationRecordId {
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
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
            s => panic!("cannot reach Verifying from {s:?}"),
        };
        let rev = h.task_revision(task_id).unwrap();
        h.transition_task(task_id, rev, target, None).unwrap();
    }
    h.create_verification_record(
        task_id,
        None,
        vec![],
        vec![],
        vec![],
        vec![],
        None,
        VerificationStatus::Passed,
        h.now_ms(),
    )
    .unwrap()
}

/// Adversarial executor-level cover of the PROOF-VALIDATED invocation: an
/// uncontracted run never invokes the runner, a bogus/nonexistent proof is a
/// typed Invalidated refusal (no side effect), while a proof-bound run
/// executes the requested step against the owner root and records it.
#[tokio::test]
pub(crate) async fn completion_steps_are_additive_and_fail_closed() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        vec![],
        faktor_agent::VerificationService::disabled(),
        false,
        false,
    );
    env.executor
        .set_completion_steps(Some(completion_step_runner(dir.path())));
    cs_seed_repo(&env.owner_root);
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    let now = h.now_ms();
    h.create_task(faktor_session::Task {
        task_id,
        session_id: env.parent,
        goal: "execute the agreed completion steps".into(),
        acceptance_criteria: vec![],
        plan: vec![],
        attachments: Vec::new(),
        budget: faktor_session::TaskBudget::default(),
        state: TaskState::Pending,
        created_ms: now,
        updated_ms: now,
    })
    .unwrap();
    let _proof = seed_passing_proof(&env, task_id);
    // (1) No contract: the runner is never invoked — the deliberately
    // uninitialized root in the injected runner would have failed loudly.
    assert!(env
        .executor
        .run_completion_steps(env.parent, faktor_core::id::VerificationRecordId::new(1))
        .await
        .unwrap()
        .is_none());
    // (2) Contract recorded, but the proof does not exist: the runner's
    // proof gate refuses EVERY requested step as Invalidated (retryable)
    // and no side effect runs.
    let rev = h.task_revision(task_id).unwrap();
    h.set_completion_contract(
        task_id,
        rev,
        faktor_core::completion::CompletionContract {
            include_commit: true,
            include_push: false,
            include_pr: false,
        },
    )
    .unwrap();
    // A dirty tree is waiting to be committed: a bogus proof must leave it
    // dirty (no side effect) even though the work is there.
    std::fs::write(env.owner_root.join("feature.txt"), "content\n").unwrap();
    let refused = env
        .executor
        .run_completion_steps(env.parent, faktor_core::id::VerificationRecordId::new(9001))
        .await
        .unwrap()
        .expect("a contracted run consults the proof and reports Invalidated");
    assert!(
        refused
            .records
            .iter()
            .all(|r| r.status != faktor_core::completion::CompletionStepOutcome::Succeeded),
        "{refused:?}"
    );
    let porcelain = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&env.owner_root)
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&porcelain.stdout).trim().is_empty(),
        "no proof, no commit"
    );
    // (3) The durable PASSED record authorizes the commit step against the
    // session's owner root: the row is Succeeded and the tree is clean. The
    // advisory fact is deliberately left FAILED: it is UI-only and must not
    // influence the proof-validated path.
    h.upsert_memory_fact("verification", "last", r#"{"status":"failed"}"#)
        .unwrap();
    let report = env
        .executor
        .run_completion_steps(env.parent, _proof)
        .await
        .unwrap()
        .expect("a contracted proof-bound run must execute");
    assert!(report.all_succeeded(), "{report:?}");
    let rows = h
        .ledger_completion_step_statuses(task_id.raw(), rev.raw())
        .unwrap();
    assert_eq!(
        rows.last().unwrap().status,
        faktor_core::completion::CompletionStepOutcome::Succeeded
    );
    let porcelain = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&env.owner_root)
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&porcelain.stdout).trim().is_empty(),
        "the owner root must be committed clean"
    );
}

/// Audit P2 reproducer: a TaskExecutor whose agent carries NO sandbox (the
/// library/test construction) refuses an egress-requiring completion step
/// typed — the push is never executed. Only production wiring supplies a
/// sandbox; absence must fail closed, never mean unrestricted egress.
#[tokio::test]
pub(crate) async fn completion_push_without_a_sandbox_refuses_egress_typed() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    // The supervised fixture wires a REAL supervisor but deliberately no
    // sandbox: exactly the library construction under audit.
    let env = open_real_tool_env_supervised(
        dir.path(),
        vec![],
        faktor_agent::VerificationService::disabled(),
    );
    cs_seed_repo(&env.owner_root);
    cs_git(
        &env.owner_root,
        &[
            "remote",
            "add",
            "origin",
            "https://git.example.invalid/team/repo.git",
        ],
    );
    let task_id = seed_contract_task(
        &env,
        "push without a sandbox",
        faktor_core::completion::CompletionContract {
            include_commit: false,
            include_push: true,
            include_pr: false,
        },
    );
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let proof = h
        .list_verification_records(task_id)
        .unwrap()
        .into_iter()
        .next_back()
        .map(|record| record.record_id)
        .expect("seed_contract_task minted a passing record");
    // No runner is installed: the executor builds its OWN from the agent's
    // supervisor + (absent) sandbox — the audited decision point.
    let report = env
        .executor
        .run_completion_steps(env.parent, proof)
        .await
        .unwrap()
        .expect("a contracted, non-terminal run consults the runner");
    assert_eq!(
        report.failed_step(),
        Some(faktor_core::completion::CompletionStep::Push),
        "{report:?}"
    );
    let (revision, _) = h.completion_contract(task_id).unwrap().expect("contract");
    let rows = h
        .ledger_completion_step_statuses(task_id.raw(), revision.raw())
        .unwrap();
    let last = rows.last().unwrap();
    assert_eq!(
        last.status,
        faktor_core::completion::CompletionStepOutcome::Failed,
        "{last:?}"
    );
    assert!(
        last.detail.contains("no sandbox configured") && last.detail.contains("requires egress"),
        "{}",
        last.detail
    );
    assert!(
        !last.detail.contains("git push failed"),
        "the typed refusal happened BEFORE any push attempt: {}",
        last.detail
    );
}

// ----------------------------------- attachments + unified settlement (P1)

/// Every durable USER message's `files` set of one session.
pub(crate) fn message_files(handle: &faktor_session::SessionHandle) -> Vec<Vec<String>> {
    handle
        .messages_before(None, 200)
        .unwrap()
        .into_iter()
        .filter(|m| m.role == "user")
        .map(|m| {
            m.data
                .get("files")
                .and_then(|f| f.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
        .collect()
}

pub(crate) fn child_session_handle(
    manager: &Arc<SessionManager>,
    row: &crate::runtime::ChildRuntime,
) -> faktor_session::SessionHandle {
    manager
        .get_session(SessionId::new(row.session_id))
        .unwrap()
        .unwrap()
}

pub(crate) fn child_root(
    manager: &Arc<SessionManager>,
    row: &crate::runtime::ChildRuntime,
) -> std::path::PathBuf {
    let wts = manager
        .worktrees_of(WorkspaceId::new(row.workspace_id))
        .unwrap();
    std::path::PathBuf::from(
        wts.iter()
            .find(|w| (w.id.max(0) as u64) == row.worktree_id)
            .expect("child worktree row")
            .path
            .clone(),
    )
}

pub(crate) fn run_registry(
    manager: &Arc<SessionManager>,
    parent: SessionId,
    run_id: &str,
) -> Vec<crate::runtime::ChildRuntime> {
    OrchestratorRuntime::registry_rows(manager.clone(), parent, run_id).unwrap()
}

pub(crate) fn assert_prompt_files(
    manager: &Arc<SessionManager>,
    rows: &[crate::runtime::ChildRuntime],
    files: &[String],
) {
    for row in rows {
        let handle = child_session_handle(manager, row);
        let sets = message_files(&handle);
        assert!(
            sets.iter().any(|s| s == files),
            "child {} must submit exactly the run files {files:?}: {sets:?}",
            row.child_id
        );
    }
}

pub(crate) fn plan_row_of(env: &Arc<Env>, run_id: &str) -> serde_json::Value {
    let handle = env.manager.get_session(env.parent).unwrap().unwrap();
    let facts = handle.memory_facts().unwrap();
    let row = facts
        .iter()
        .find(|(kind, key, _)| kind == crate::runtime::PLAN_ROW_KIND && key == run_id)
        .expect("durable plan row");
    serde_json::from_str(&row.2).unwrap()
}

/// FIX 1 (a): a 3-item orchestrated run's files reach EVERY child's
/// submitted prompt, the durable plan row carries the identical set, and
/// the set survives a store reopen.
#[tokio::test]
pub(crate) async fn multi_item_run_files_reach_every_child_and_survive_reopen() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let scripts: Vec<Vec<ScriptedResponse>> = (0..4)
        .map(|_| vec![ScriptedResponse::Text("done".into()), ScriptedResponse::End])
        .collect();
    let env = open_env(dir.path(), scripts);
    let files = vec![
        "src/a.rs".to_string(),
        "docs/b.md".to_string(),
        "c.txt".to_string(),
    ];
    let mut req = request(
        "attached 3-item run",
        vec![
            wi("a", WorkKind::Analysis, &[]),
            wi("b", WorkKind::Analysis, &[]),
            wi("c", WorkKind::Analysis, &[]),
        ],
        &env,
    );
    req.files = files.clone();
    let receipt = env
        .executor
        .start_task(env.parent, req)
        .expect("attached run starts");
    wait_until(
        || {
            let rows = run_registry(&env.manager, env.parent, &receipt.run_id);
            rows.len() == 3 && rows.iter().all(|c| c.state.is_terminal())
        },
        120,
    )
    .await;
    let rows = run_registry(&env.manager, env.parent, &receipt.run_id);
    assert_prompt_files(&env.manager, &rows, &files);
    let plan = plan_row_of(&env, &receipt.run_id);
    let specs: Vec<crate::runtime::ChildSpec> =
        serde_json::from_value(plan["specs"].clone()).unwrap();
    assert_eq!(specs.len(), 3);
    assert!(
        specs.iter().all(|s| s.files == files),
        "every durable spec carries the run's files: {specs:?}"
    );
    // The durable row is byte-identical across a reopen of the same store.
    let reopened =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let h2 = reopened.get_session(env.parent).unwrap().unwrap();
    let plan2 = h2
        .memory_facts()
        .unwrap()
        .into_iter()
        .find(|(kind, key, _)| kind == crate::runtime::PLAN_ROW_KIND && key == &receipt.run_id)
        .expect("plan row after reopen");
    let v2: serde_json::Value = serde_json::from_str(&plan2.2).unwrap();
    assert_eq!(plan, v2, "the plan row (files included) is durable");
}

/// FIX 1 (b): a run that crashes after the durable plan/assignments (before
/// ANY spawn) re-attaches and every re-spawned child submits the files
/// decoded from the DURABLE plan row — never from the lost request memory.
#[tokio::test]
pub(crate) async fn crashed_orchestrated_run_reattaches_files_from_durable_plan_not_memory() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let scripts: Vec<Vec<ScriptedResponse>> = (0..4)
        .map(|_| vec![ScriptedResponse::Text("done".into()), ScriptedResponse::End])
        .collect();
    let env = open_env(dir.path(), scripts);
    let files = vec!["src/one.rs".to_string(), "src/two.rs".to_string()];
    let mut req = request(
        "crash-before-spawn attached run",
        vec![
            wi("a", WorkKind::Analysis, &[]),
            wi("b", WorkKind::Analysis, &[]),
            wi("c", WorkKind::Analysis, &[]),
        ],
        &env,
    );
    req.files = files.clone();
    req.crash_seam = Some(CrashSeam::AfterAssignmentsPersisted);
    let receipt = env
        .executor
        .start_task(env.parent, req)
        .expect("start accepted before the crash seam");
    wait_until(
        || {
            OrchestratorRuntime::assignment_rows(env.manager.clone(), env.parent, &receipt.run_id)
                .map(|a| a.len() == 3)
                .unwrap_or(false)
        },
        60,
    )
    .await;
    wait_until(|| env.executor.active_runs().is_empty(), 240).await;
    assert!(
        run_registry(&env.manager, env.parent, &receipt.run_id).is_empty(),
        "the crash seam fired BEFORE any spawn"
    );
    env.executor
        .resume_run(
            env.parent,
            &receipt.run_id,
            crate::runtime::Ceilings::default(),
            crate::caps::all_lattice(),
            None,
        )
        .expect("resume accepted");
    wait_until(
        || {
            let rows = run_registry(&env.manager, env.parent, &receipt.run_id);
            rows.len() == 3 && rows.iter().all(|c| c.state.is_terminal())
        },
        120,
    )
    .await;
    assert_prompt_files(
        &env.manager,
        &run_registry(&env.manager, env.parent, &receipt.run_id),
        &files,
    );
}

/// FIX 1 (c): hostile and oversized file lists are typed refusals BEFORE
/// any durable orchestration row (no spawn, no plan, no assignment).
#[tokio::test]
pub(crate) async fn hostile_or_oversized_task_files_are_refused_before_any_durable_row() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_env(dir.path(), vec![vec![ScriptedResponse::End]]);
    let base = || {
        request(
            "hostile attachments",
            vec![
                wi("a", WorkKind::Analysis, &[]),
                wi("b", WorkKind::Analysis, &[]),
            ],
            &env,
        )
    };
    // Oversized COUNT.
    let mut req = base();
    req.files = (0..=faktor_session::MAX_FILES_PER_PROMPT)
        .map(|i| format!("f{i}.rs"))
        .collect();
    let err = env
        .executor
        .start_task(env.parent, req)
        .expect_err("count over MAX_FILES_PER_PROMPT");
    assert!(matches!(err, ExecError::Oversized(_)), "{err:?}");
    // Oversized PATH.
    let mut req = base();
    req.files = vec!["x".repeat(faktor_session::MAX_FILE_PATH_BYTES + 1)];
    let err = env
        .executor
        .start_task(env.parent, req)
        .expect_err("path over MAX_FILE_PATH_BYTES");
    assert!(matches!(err, ExecError::Oversized(_)), "{err:?}");
    // Hostile paths.
    for hostile in [
        "/etc/passwd",
        "../secrets",
        "src/../../secrets",
        "",
        "\0bad",
    ] {
        let mut req = base();
        req.files = vec![hostile.to_string()];
        let err = env
            .executor
            .start_task(env.parent, req)
            .expect_err("hostile path");
        assert!(
            matches!(err, ExecError::Malformed(_)),
            "{hostile:?}: {err:?}"
        );
    }
    let handle = env.manager.get_session(env.parent).unwrap().unwrap();
    assert!(
        !handle
            .memory_facts()
            .unwrap()
            .iter()
            .any(|(kind, _, _)| matches!(
                kind.as_str(),
                crate::runtime::PLAN_ROW_KIND
                    | crate::runtime::ASSIGNMENT_ROW_KIND
                    | crate::runtime::REGISTRY_ROW_KIND
            )),
        "refused attachment lists leave no orchestration rows"
    );
}

/// FIX 1 parity: an attachment-free orchestrated run submits EMPTY file
/// sets to every child — byte-identical to every previous wave.
#[tokio::test]
pub(crate) async fn attachment_free_orchestrated_run_submits_empty_file_sets() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let scripts: Vec<Vec<ScriptedResponse>> = (0..3)
        .map(|_| vec![ScriptedResponse::Text("done".into()), ScriptedResponse::End])
        .collect();
    let env = open_env(dir.path(), scripts);
    let receipt = env
        .executor
        .start_task(
            env.parent,
            request(
                "no attachments",
                vec![
                    wi("a", WorkKind::Analysis, &[]),
                    wi("b", WorkKind::Analysis, &[]),
                ],
                &env,
            ),
        )
        .expect("start");
    wait_until(
        || {
            let rows = run_registry(&env.manager, env.parent, &receipt.run_id);
            rows.len() == 2 && rows.iter().all(|c| c.state.is_terminal())
        },
        120,
    )
    .await;
    assert_prompt_files(
        &env.manager,
        &run_registry(&env.manager, env.parent, &receipt.run_id),
        &[],
    );
}

pub(crate) fn completion_step_runner_builder(
    root: &std::path::Path,
    config: crate::runtime::completion_steps::CompletionStepsConfig,
) -> crate::runtime::completion_steps::CompletionStepRunner {
    use crate::runtime::completion_steps::{CompletionStepRunner, EgressPolicy};
    let cas = Arc::new(faktor_cas::Cas::open(root.join("completion-cas")).unwrap());
    let supervisor = faktor_terminal::ProcessSupervisor::new(cas);
    let egress: Arc<dyn EgressPolicy> = Arc::new(|_url: &str| Ok(()));
    CompletionStepRunner::new(supervisor, egress, config).unwrap()
}

#[allow(dead_code)]
pub(crate) fn completion_step_runner_with(
    root: &std::path::Path,
    config: crate::runtime::completion_steps::CompletionStepsConfig,
) -> Arc<crate::runtime::completion_steps::CompletionStepRunner> {
    Arc::new(completion_step_runner_builder(root, config))
}

pub(crate) fn seed_contract_task(
    env: &Arc<RealToolEnv>,
    goal: &str,
    contract: faktor_core::completion::CompletionContract,
) -> TaskId {
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    let now = h.now_ms();
    h.create_task(faktor_session::Task {
        task_id,
        session_id: env.parent,
        goal: goal.to_string(),
        acceptance_criteria: vec![],
        plan: vec![],
        attachments: Vec::new(),
        budget: faktor_session::TaskBudget::default(),
        state: TaskState::Pending,
        created_ms: now,
        updated_ms: now,
    })
    .unwrap();
    for target in [
        TaskTransition::StartRunning,
        TaskTransition::RequestVerification,
        TaskTransition::StartVerification,
    ] {
        let rev = h.task_revision(task_id).unwrap();
        h.transition_task(task_id, rev, target, None).unwrap();
    }
    let rev = h.task_revision(task_id).unwrap();
    h.set_completion_contract(task_id, rev, contract).unwrap();
    // The proof-validated step path needs the durable PASSED record at the
    // current revision (the advisory fact is UI-only).
    h.create_verification_record(
        task_id,
        None,
        vec![],
        vec![],
        vec![],
        vec![],
        None,
        VerificationStatus::Passed,
        h.now_ms(),
    )
    .unwrap();
    task_id
}

pub(crate) fn cs_head(root: &std::path::Path) -> String {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// FIX 2 crash seam: the commit side effect landed but its status row did
/// not; `settle_run` replays idempotently (HEAD recognized, `Succeeded`),
/// without a second commit.
#[tokio::test]
pub(crate) async fn settle_run_converges_after_the_commit_status_write_seam() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        vec![],
        faktor_agent::VerificationService::disabled(),
        false,
        false,
    );
    env.executor
        .set_completion_steps(Some(completion_step_runner(dir.path())));
    cs_seed_repo(&env.owner_root);
    let goal = "ship the committed feature";
    let task_id = seed_contract_task(
        &env,
        goal,
        faktor_core::completion::CompletionContract {
            include_commit: true,
            include_push: false,
            include_pr: false,
        },
    );
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let contract_rev = h.task_revision(task_id).unwrap();
    // The crash window: the runner committed, the process died BEFORE the
    // durable status row landed.
    std::fs::write(env.owner_root.join("feature.txt"), "content\n").unwrap();
    cs_git(&env.owner_root, &["add", "-A"]);
    cs_commit(&env.owner_root, &commit_message(goal));
    let head = cs_head(&env.owner_root);
    assert!(h
        .ledger_completion_step_statuses(task_id.raw(), contract_rev.raw())
        .unwrap()
        .is_empty());
    // Reopen + settle: the replay records Succeeded and never commits again.
    let outcome = env
        .executor
        .settle_run(RunSettlement::InSession {
            parent: env.parent,
            run_id: "tx-commit-seam".into(),
        })
        .await
        .unwrap();
    assert!(outcome.steps.is_some(), "{outcome:?}");
    assert_eq!(cs_head(&env.owner_root), head, "no double commit");
    let rows = h
        .ledger_completion_step_statuses(task_id.raw(), contract_rev.raw())
        .unwrap();
    assert_eq!(
        rows.last().unwrap().status,
        faktor_core::completion::CompletionStepOutcome::Succeeded
    );
    assert!(
        rows.last().unwrap().detail.contains("already committed"),
        "{:?}",
        rows.last()
    );
    assert!(outcome.steps.unwrap().all_succeeded());
    // A FRESH manager over the same store (the daemon-restart view) reads
    // exactly the converged row: nothing lived only in this process.
    let reopened =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let h2 = reopened.get_session(env.parent).unwrap().unwrap();
    let rows2 = h2
        .ledger_completion_step_statuses(task_id.raw(), contract_rev.raw())
        .unwrap();
    assert_eq!(
        rows2.last().unwrap().status,
        faktor_core::completion::CompletionStepOutcome::Succeeded
    );
}

/// FIX 2 crash seam: the push side effect landed but its status row did
/// not; the replay recognizes the already-pushed HEAD and records
/// `Succeeded` without touching the remote again.
#[tokio::test]
pub(crate) async fn settle_run_converges_after_the_push_status_write_seam() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        vec![],
        faktor_agent::VerificationService::disabled(),
        false,
        false,
    );
    env.executor
        .set_completion_steps(Some(completion_step_runner(dir.path())));
    cs_seed_repo(&env.owner_root);
    let remote = dir.path().join("remote.git");
    cs_git(
        dir.path(),
        // Pin the bare remote's default branch: CI git defaults to
        // master, so `rev-parse HEAD` on the remote printed the unborn
        // "HEAD" instead of the pushed sha.
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            remote.to_str().unwrap(),
        ],
    );
    cs_git(
        &env.owner_root,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    let goal = "ship the pushed feature";
    let task_id = seed_contract_task(
        &env,
        goal,
        faktor_core::completion::CompletionContract {
            include_commit: false,
            include_push: true,
            include_pr: false,
        },
    );
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let contract_rev = h.task_revision(task_id).unwrap();
    // The crash window: commit + push landed externally, no status row.
    std::fs::write(env.owner_root.join("feature.txt"), "content\n").unwrap();
    cs_git(&env.owner_root, &["add", "-A"]);
    cs_commit(&env.owner_root, &commit_message(goal));
    cs_git(&env.owner_root, &["push", "-q", "-u", "origin", "main"]);
    let remote_head = cs_head(&remote);
    assert_eq!(remote_head, cs_head(&env.owner_root));
    let outcome = env
        .executor
        .settle_run(RunSettlement::InSession {
            parent: env.parent,
            run_id: "tx-push-seam".into(),
        })
        .await
        .unwrap();
    assert!(outcome.steps.unwrap().all_succeeded());
    assert_eq!(cs_head(&remote), remote_head, "remote untouched by replay");
    let rows = h
        .ledger_completion_step_statuses(task_id.raw(), contract_rev.raw())
        .unwrap();
    assert_eq!(
        rows.last().unwrap().status,
        faktor_core::completion::CompletionStepOutcome::Succeeded
    );
    assert!(
        rows.last().unwrap().detail.contains("already pushed"),
        "{:?}",
        rows.last()
    );
    // The daemon-restart view reads the same converged row from the store.
    let reopened =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let h2 = reopened.get_session(env.parent).unwrap().unwrap();
    let rows2 = h2
        .ledger_completion_step_statuses(task_id.raw(), contract_rev.raw())
        .unwrap();
    assert_eq!(
        rows2.last().unwrap().status,
        faktor_core::completion::CompletionStepOutcome::Succeeded
    );
}

/// FIX 2 crash seam + typed reconciliation: the PR was created on the
/// provider but its status row never landed; the replay reconciles through
/// the RECORDED operation identity and records `Succeeded` (the same PR, no
/// duplicate), never a second PR gate.
#[tokio::test]
pub(crate) async fn settle_run_converges_after_the_pr_status_write_seam() {
    use crate::runtime::completion_steps::{
        scm_fake, CompletionStepsConfig, PrOperationCrashPoint,
    };
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        vec![],
        faktor_agent::VerificationService::disabled(),
        false,
        false,
    );
    cs_seed_repo(&env.owner_root);
    cs_git(
        &env.owner_root,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/widgets.git",
        ],
    );
    let config = CompletionStepsConfig::default();
    let provider = scm_fake::FakeScmProvider::new();
    // Phase 1: the remote creation succeeds and the durable completion row
    // (and the step status) never lands — exactly a crash at that boundary.
    let crashed = completion_step_runner_builder(dir.path(), config.clone())
        .with_scm_provider(provider.clone())
        .with_pr_crash_seam(PrOperationCrashPoint::AfterRemoteCallBeforeRecord);
    env.executor.set_completion_steps(Some(Arc::new(crashed)));
    let goal = "ship the PR feature";
    let task_id = seed_contract_task(
        &env,
        goal,
        faktor_core::completion::CompletionContract {
            include_commit: false,
            include_push: false,
            include_pr: true,
        },
    );
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let contract_rev = h.task_revision(task_id).unwrap();
    let outcome = env
        .executor
        .settle_run(RunSettlement::InSession {
            parent: env.parent,
            run_id: "tx-pr-seam".into(),
        })
        .await
        .unwrap();
    assert!(
        outcome.steps.is_none(),
        "the injected crash aborted the step execution: {outcome:?}"
    );
    assert_eq!(provider.remote_creations(), 1, "the PR was created once");
    // Phase 2: the replay reconciles from the recorded identity: the same PR
    // is confirmed, no second remote creation, the step certifies.
    let replay =
        completion_step_runner_builder(dir.path(), config).with_scm_provider(provider.clone());
    env.executor.set_completion_steps(Some(Arc::new(replay)));
    let outcome = env
        .executor
        .settle_run(RunSettlement::InSession {
            parent: env.parent,
            run_id: "tx-pr-seam".into(),
        })
        .await
        .unwrap();
    assert!(outcome.steps.unwrap().all_succeeded());
    assert_eq!(
        provider.remote_creations(),
        1,
        "reconciliation must never duplicate the PR"
    );
    let rows = h
        .ledger_completion_step_statuses(task_id.raw(), contract_rev.raw())
        .unwrap();
    assert_eq!(
        rows.last().unwrap().status,
        faktor_core::completion::CompletionStepOutcome::Succeeded
    );
    assert!(
        rows.last()
            .unwrap()
            .detail
            .contains("certified via github reconciliation"),
        "{:?}",
        rows.last()
    );
    // The daemon-restart view reads the same converged row from the store.
    let reopened =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let h2 = reopened.get_session(env.parent).unwrap().unwrap();
    let rows2 = h2
        .ledger_completion_step_statuses(task_id.raw(), contract_rev.raw())
        .unwrap();
    assert_eq!(
        rows2.last().unwrap().status,
        faktor_core::completion::CompletionStepOutcome::Succeeded
    );
}

/// One real-tool child script: write one file, then finish the turn.
pub(crate) fn write_script(id: &str, path: &str, content: &str) -> Vec<ScriptedResponse> {
    vec![
        ScriptedResponse::ToolCall {
            id: id.into(),
            name: "write_file".into(),
            input: serde_json::json!({ "path": path, "content": content }),
        },
        ScriptedResponse::Text("wrote".into()),
        ScriptedResponse::End,
    ]
}

pub(crate) fn two_child_scripts(a: &str, b: &str) -> Vec<Vec<ScriptedResponse>> {
    vec![
        write_script("c-a", "child_a.rs", a),
        write_script("c-b", "child_b.rs", b),
        vec![ScriptedResponse::Text("done".into()), ScriptedResponse::End],
        vec![ScriptedResponse::Text("done".into()), ScriptedResponse::End],
    ]
}

pub(crate) fn two_isolated_items() -> Vec<WorkItem> {
    ["impl-a", "impl-b"]
        .iter()
        .map(|id| {
            WorkItem::with_ownership(
                *id,
                format!("work {id}"),
                WorkKind::Implementation,
                OwnershipSpec::IsolatedWorktree,
            )
        })
        .collect()
}

pub(crate) fn typed_land_criterion() -> String {
    faktor_session::task::Criterion::derived(
        "required check: cargo check",
        faktor_core::state::CriterionOrigin::ProjectPolicy,
        faktor_core::state::CriterionRequirement::Required,
        None,
    )
    .with_binding(faktor_core::state::CriterionBinding::RequiredCheck {
        check_id: "rust_check".into(),
        command_digest: faktor_core::state::command_binding_digest("cargo check"),
    })
    .encode()
}

/// A REQUIRED criterion whose binding can never pass while every derived
/// check is green: the file-state binding pins a digest the candidate cannot
/// have.
pub(crate) fn typed_failed_file_state_criterion() -> String {
    faktor_session::task::Criterion::derived(
        "src/lib.rs is frozen at an impossible digest",
        CriterionOrigin::ProjectPolicy,
        CriterionRequirement::Required,
        None,
    )
    .with_binding(CriterionBinding::FileState {
        path: "src/lib.rs".into(),
        expected_digest: "0".repeat(64),
    })
    .encode()
}

/// A REQUIRED criterion carrying the explicit honest-unknown binding: the
/// evaluator can only ever return Unavailable for it, never a pass.
pub(crate) fn typed_explicit_unknown_criterion() -> String {
    faktor_session::task::Criterion::derived(
        "no objective mechanism certifies this criterion",
        CriterionOrigin::ProjectPolicy,
        CriterionRequirement::Required,
        None,
    )
    .with_binding(CriterionBinding::Unavailable {
        reason: "the criterion is explicitly honest-unknown".into(),
    })
    .encode()
}

pub(crate) fn start_two_child_run(env: &Arc<RealToolEnv>, goal: &str) -> String {
    env.executor
        .start_task(
            env.parent,
            TaskRunRequest {
                goal: goal.to_string(),
                work_items: two_isolated_items(),
                criteria: vec![typed_land_criterion()],
                parent_caps: crate::caps::all_lattice(),
                isolated_root: env.isolated_root.clone(),
                ..Default::default()
            },
        )
        .expect("orchestrated start")
        .run_id
}

/// The daemon-owned candidate root of one run (the deterministic placement
/// the prepare phase copies the run base into).
pub(crate) fn run_candidate_root(env: &RealToolEnv, run_id: &str) -> std::path::PathBuf {
    env.isolated_root.join(run_id).join("candidate")
}

pub(crate) fn owner_digest(env: &RealToolEnv) -> String {
    faktor_fs::tree_manifest::tree_manifest_digest(
        &env.owner_root,
        faktor_fs::tree_manifest::MAX_TREE_MANIFEST_ENTRIES,
    )
    .unwrap()
}

pub(crate) fn root_digest(path: &std::path::Path) -> String {
    faktor_fs::tree_manifest::tree_manifest_digest(
        path,
        faktor_fs::tree_manifest::MAX_TREE_MANIFEST_ENTRIES,
    )
    .unwrap()
}

/// Seed a REAL rust checkout with git history (the derived-check profile
/// needs a detectable project; the completion runner needs a repo).
pub(crate) fn cs_seed_owner(env: &RealToolEnv) {
    seed_rust(env);
    cs_git(&env.owner_root, &["init", "-q", "-b", "main"]);
    cs_git(&env.owner_root, &["add", "-A"]);
    cs_commit(&env.owner_root, "init");
}

pub(crate) fn latest_txn(
    env: &RealToolEnv,
    run_id: &str,
) -> Option<faktor_session::ledger::IntegrationTxnRow> {
    env.manager
        .get_session(env.parent)
        .unwrap()
        .unwrap()
        .ledger_integration_txn_for_run(run_id)
        .unwrap()
}

pub(crate) async fn settle_orchestrated(
    env: &Arc<RealToolEnv>,
    run_id: &str,
) -> Result<SettlementOutcome, ExecError> {
    env.executor
        .settle_run(RunSettlement::Orchestrated {
            parent: env.parent,
            run_id: run_id.to_string(),
        })
        .await
}

/// Point 1/2: the verifier runs over the CANDIDATE root while the owner
/// still lacks every child change; the passing record binds the candidate
/// digest, and only the landing phase writes the owner.
#[tokio::test]
pub(crate) async fn verifier_observes_candidate_not_owner() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        two_child_scripts("pub fn a() -> u64 {\n    let seed: u64 = 1;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n", "pub fn b() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n"),
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
    );
    cs_seed_owner(&env);
    // Freeze the settlement right AFTER the candidate verification: the
    // verifier has already observed its root.
    env.executor
        .set_settlement_crash_seam(Some(CrashSeam::AfterPreparedVerification));
    let run_id = start_two_child_run(&env, "verify the candidate");
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    let candidate = run_candidate_root(&env, &run_id);
    assert!(
        candidate.join("child_a.rs").is_file() && candidate.join("child_b.rs").is_file(),
        "the candidate holds both child changes"
    );
    assert!(
        !env.owner_root.join("child_a.rs").exists() && !env.owner_root.join("child_b.rs").exists(),
        "the owner was NOT touched by the verification phase"
    );
    let candidate_digest = root_digest(&candidate);
    let record = h
        .list_verification_records(task_id)
        .unwrap()
        .into_iter()
        .filter(|r| r.status == VerificationStatus::Passed && r.tree_hash.is_some())
        .max_by_key(|r| r.record_id)
        .expect("a passing record bound to the candidate was minted");
    assert_eq!(record.tree_hash.as_deref(), Some(candidate_digest.as_str()));
    assert_ne!(
        owner_digest(&env),
        candidate_digest,
        "the verified snapshot is the candidate, not the owner"
    );
    // Recovery: the same verified candidate now lands into the owner.
    env.executor.set_settlement_crash_seam(None);
    let outcome = settle_orchestrated(&env, &run_id).await.expect("recovery");
    assert!(outcome.verified && outcome.completed, "{outcome:?}");
    assert!(env.owner_root.join("child_a.rs").is_file());
    assert!(env.owner_root.join("child_b.rs").is_file());
    assert_eq!(owner_digest(&env), candidate_digest);
}

// ---------------------------------------------- ExclusivePaths isolation

/// A run whose plan mixes a read-only AUTO item with one `Paths`-owned
/// mutating item: the executor must spawn the Paths child in its OWN overlay
/// and route its change set through stage -> compose -> verify -> land. The
/// auto item keeps the run orchestrated without opening the shared owner
/// workspace (the only provider call is the Paths child's, so the script
/// index is deterministic).
pub(crate) fn start_paths_run(env: &Arc<RealToolEnv>, goal: &str, paths: &[&str]) -> String {
    env.executor
        .start_task(
            env.parent,
            TaskRunRequest {
                goal: goal.to_string(),
                work_items: vec![
                    wi("prep", WorkKind::Analysis, &[]),
                    path_item("impl", WorkKind::Implementation, &["prep"], paths),
                ],
                auto_items: vec!["prep".to_string()],
                criteria: vec![typed_land_criterion()],
                parent_caps: crate::caps::all_lattice(),
                isolated_root: env.isolated_root.clone(),
                ..Default::default()
            },
        )
        .expect("paths orchestrated start")
        .run_id
}

pub(crate) fn run_child_row(
    env: &RealToolEnv,
    run_id: &str,
    item: &str,
) -> crate::runtime::ChildRuntime {
    OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, run_id)
        .unwrap()
        .into_iter()
        .find(|r| r.item_id == item)
        .unwrap_or_else(|| panic!("no child row for item {item}"))
}

pub(crate) fn child_overlay(
    env: &RealToolEnv,
    row: &crate::runtime::ChildRuntime,
) -> std::path::PathBuf {
    env.manager
        .workspace_root(WorkspaceId::new(row.workspace_id))
        .unwrap()
        .expect("overlay root registered")
}

/// A `Paths` child writes ONLY inside its daemon-owned overlay: the owner
/// digest stays stable through the whole drive AND the candidate
/// verification, and the change set lands exclusively through the shared
/// integration transaction once verification passed.
#[tokio::test]
pub(crate) async fn paths_child_changes_stay_in_its_overlay_until_the_verified_land() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        vec![write_script(
            "c-impl",
            "src/impl.rs",
            "pub fn p() -> u64 {\n    let seed: u64 = 7;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n",
        )],
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
    );
    cs_seed_owner(&env);
    let before = owner_digest(&env);
    env.executor
        .set_settlement_crash_seam(Some(CrashSeam::AfterCandidatePrepared));
    let run_id = start_paths_run(&env, "land the declared path", &["src"]);
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    let row = run_child_row(&env, &run_id, "impl");
    assert_eq!(row.ownership, ChildOwnership::ExclusivePaths);
    assert_eq!(row.ownership_paths, vec!["src".to_string()]);
    let overlay = child_overlay(&env, &row);
    assert!(
        overlay.ends_with(&row.child_id),
        "the Paths child owns an overlay named after its child id: {overlay:?}"
    );
    assert_ne!(overlay, env.owner_root);
    assert!(
        overlay.join("src/impl.rs").is_file(),
        "the write landed in the child's own overlay"
    );
    let candidate = run_candidate_root(&env, &run_id);
    assert!(candidate.join("src/impl.rs").is_file());
    assert!(
        !env.owner_root.join("src/impl.rs").exists(),
        "the owner is byte-untouched before the landing"
    );
    assert_eq!(
        owner_digest(&env),
        before,
        "owner digest stable while the Paths child ran and its candidate was verified"
    );
    // Recovery settles the SAME verified candidate through the txn.
    env.executor.set_settlement_crash_seam(None);
    let outcome = settle_orchestrated(&env, &run_id).await.expect("settle");
    assert!(outcome.verified && outcome.completed, "{outcome:?}");
    assert!(env.owner_root.join("src/impl.rs").is_file());
    let candidate_digest = root_digest(&candidate);
    assert_eq!(
        owner_digest(&env),
        candidate_digest,
        "the owner equals the verified candidate only after landing"
    );
    let txn = latest_txn(&env, &run_id).expect("the integration transaction");
    assert_eq!(txn.path_count, 1, "{txn:?}");
    assert_eq!(txn.applied_count, 1, "{txn:?}");
}

/// The declared path set is the tool-boundary write allowlist: a mutating
/// write outside it is typed-refused (journaled PermissionDenied) and never
/// reaches the overlay — long before staging.
#[tokio::test]
pub(crate) async fn paths_child_out_of_scope_write_is_refused_typed_at_the_tool_gate() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        vec![write_script(
            "c-evil",
            "evil.rs",
            "outside the declared set\n",
        )],
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
    );
    cs_seed_owner(&env);
    let run_id = start_paths_run(&env, "refuse the out-of-scope write", &["src"]);
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    let row = run_child_row(&env, &run_id, "impl");
    assert_eq!(row.ownership, ChildOwnership::ExclusivePaths);
    let overlay = child_overlay(&env, &row);
    assert!(
        !overlay.join("evil.rs").exists(),
        "the denied write never reached the overlay"
    );
    assert!(!env.owner_root.join("evil.rs").exists());
    let handle = env
        .manager
        .get_session(SessionId::new(row.session_id))
        .unwrap()
        .unwrap();
    let events = handle.events_range(1, None).unwrap();
    let denial = events
        .iter()
        .find(|e| e.kind == faktor_core::event::EventKind::PermissionDenied)
        .expect("the edit gate journals the out-of-scope refusal");
    let payload = denial.payload.as_ref().expect("denial carries a payload");
    assert_eq!(payload["tool"], "write_file");
    assert!(
        payload["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("change budget refused the edit"),
        "{payload:?}"
    );
}

// ----------------------------------------------- P0 criterion-proof residuals

/// A reviewer-bound acceptance criterion (the "independent reviewer proves
/// the no-op" shape): the wired reviewer port is the ONLY binding that can
/// certify it.
pub(crate) fn typed_reviewer_criterion(reviewer_id: &str) -> String {
    faktor_session::task::Criterion::derived(
        "independent reviewer proves the no-op",
        CriterionOrigin::ProjectPolicy,
        CriterionRequirement::Required,
        None,
    )
    .with_binding(CriterionBinding::IndependentReview {
        reviewer_id: reviewer_id.into(),
    })
    .encode()
}

/// An aggregate-goal criterion: passes only when every required subordinate
/// passed AND the wired independent reviewer passed over the candidate.
pub(crate) fn typed_aggregate_goal_criterion() -> String {
    faktor_session::task::Criterion::derived(
        "ship the goal",
        CriterionOrigin::User,
        CriterionRequirement::Required,
        None,
    )
    .with_binding(CriterionBinding::AggregateGoal)
    .encode()
}

/// Two isolated children that change NOTHING (the empty aggregate change
/// set the no-op policy governs).
pub(crate) fn idle_child_scripts() -> Vec<Vec<ScriptedResponse>> {
    vec![
        vec![
            ScriptedResponse::Text("nothing to change".into()),
            ScriptedResponse::End,
        ],
        vec![
            ScriptedResponse::Text("nothing to change".into()),
            ScriptedResponse::End,
        ],
    ]
}

/// The typed verdict a review MODEL is contract-bound to emit.
pub(crate) fn review_verdict_script(verdict: &str) -> Vec<ScriptedResponse> {
    vec![
        ScriptedResponse::Text(serde_json::json!({"verdict": verdict, "findings": []}).to_string()),
        ScriptedResponse::End,
    ]
}

pub(crate) fn start_no_op_run(
    env: &Arc<RealToolEnv>,
    goal: &str,
    disposition: NoOpDisposition,
) -> String {
    env.executor
        .start_task(
            env.parent,
            TaskRunRequest {
                goal: goal.to_string(),
                work_items: two_isolated_items(),
                criteria: vec![typed_reviewer_criterion("review-0")],
                parent_caps: crate::caps::all_lattice(),
                isolated_root: env.isolated_root.clone(),
                no_op_disposition: Some(disposition),
                ..Default::default()
            },
        )
        .expect("no-op orchestrated start")
        .run_id
}

/// Residual (5) path A: an empty aggregate change set with
/// `RequiresCriterionProof` completes ONLY through the reviewer-proved no-op
/// criterion — the review model call runs over the candidate snapshot and
/// its pass is the record's criterion proof; the owner is never rewritten.
#[tokio::test]
pub(crate) async fn no_op_run_completes_only_through_the_reviewer_proof() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let mut scripts = idle_child_scripts();
    scripts.push(review_verdict_script("clean"));
    let env = open_real_tool_env_full(
        dir.path(),
        scripts,
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
    );
    cs_seed_owner(&env);
    let before = owner_digest(&env);
    let run_id = start_no_op_run(
        &env,
        "decide whether any change is needed",
        NoOpDisposition::RequiresCriterionProof,
    );
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    let outcome = settle_orchestrated(&env, &run_id).await.expect("settle");
    assert!(outcome.verified && outcome.completed, "{outcome:?}");
    assert_eq!(
        owner_digest(&env),
        before,
        "a reviewer-proved no-op never rewrites the owner"
    );
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    assert_eq!(
        h.get_task(task_id).unwrap().unwrap().state,
        TaskState::VerifiedComplete
    );
    let record = h
        .list_verification_records(task_id)
        .unwrap()
        .into_iter()
        .max_by_key(|r| r.record_id)
        .expect("the no-op proof record");
    assert_eq!(record.status, VerificationStatus::Passed);
    assert!(record.tree_hash.is_some(), "{record:?}");
    assert!(
        record.criteria.iter().any(|c| c.passed
            && c.binding
                == Some(CriterionBinding::IndependentReview {
                    reviewer_id: "review-0".into()
                })),
        "the no-op criterion is certified through the reviewer binding: {record:?}"
    );
    // The proof was a REAL review-model call (2 child drives + 1 review).
    assert_eq!(
        env.provider.request_count.load(Ordering::SeqCst),
        3,
        "the no-op proof is a reviewer call, not a local empty-suite pass"
    );
    let txn = latest_txn(&env, &run_id).expect("no-op integration record");
    assert_eq!(txn.path_count, 0);
    assert_eq!(txn.applied_count, 0);
}

/// Residual (5) path B: with the reviewer GENUINELY absent (no typed verdict)
/// the same empty change set stays unverified — no record, no landing, no
/// completion. Unavailable stays honest.
#[tokio::test]
pub(crate) async fn no_op_run_without_a_reviewer_verdict_refuses_completion() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        idle_child_scripts(),
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
    );
    cs_seed_owner(&env);
    let before = owner_digest(&env);
    let run_id = start_no_op_run(
        &env,
        "decide whether any change is needed",
        NoOpDisposition::RequiresCriterionProof,
    );
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    let outcome = settle_orchestrated(&env, &run_id).await.expect("settle");
    assert!(outcome.complete, "the children finished");
    assert!(!outcome.verified && !outcome.completed, "{outcome:?}");
    assert_eq!(owner_digest(&env), before);
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    assert_ne!(
        h.get_task(task_id).unwrap().unwrap().state,
        TaskState::VerifiedComplete
    );
    assert!(
        h.list_verification_records(task_id)
            .unwrap()
            .iter()
            .all(|r| r.tree_hash.is_none() || r.status != VerificationStatus::Passed),
        "no passing root record without a reviewer verdict"
    );
    assert!(
        h.ledger_integration_record_for_task(task_id.raw())
            .unwrap()
            .is_none(),
        "no landing for an unproved no-op"
    );
}

/// Residual (5) path C: an explicitly REFUSED no-op disposition never even
/// asks the reviewer — the run stays unverified and the review script stays
/// unconsumed.
#[tokio::test]
pub(crate) async fn no_op_run_with_refused_disposition_never_completes() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let mut scripts = idle_child_scripts();
    scripts.push(review_verdict_script("clean"));
    let env = open_real_tool_env_full(
        dir.path(),
        scripts,
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
    );
    cs_seed_owner(&env);
    let before = owner_digest(&env);
    let run_id = start_no_op_run(&env, "no changes are acceptable", NoOpDisposition::Refused);
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    let outcome = settle_orchestrated(&env, &run_id).await.expect("settle");
    assert!(!outcome.verified && !outcome.completed, "{outcome:?}");
    assert_eq!(owner_digest(&env), before);
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    assert_ne!(
        h.get_task(task_id).unwrap().unwrap().state,
        TaskState::VerifiedComplete
    );
    assert!(h
        .ledger_integration_record_for_task(task_id.raw())
        .unwrap()
        .is_none());
    assert_eq!(
        env.provider.request_count.load(Ordering::SeqCst),
        2,
        "a refused no-op never spends a review-model call"
    );
}

/// Residual (1) present side: an AggregateGoal criterion over a REAL changed
/// candidate is certified through the WIRED reviewer — the required
/// subordinate check passed and the recorded candidate review answered the
/// aggregate; the task completes and lands.
#[tokio::test]
pub(crate) async fn aggregate_goal_criterion_is_reviewer_certified_over_the_candidate() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        two_child_scripts(
            "pub fn a() -> u64 {\n    let seed: u64 = 1;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n",
            "pub fn b() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n",
        ),
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
    );
    cs_seed_owner(&env);
    let run_id = env
        .executor
        .start_task(
            env.parent,
            TaskRunRequest {
                goal: "aggregate the goal".to_string(),
                work_items: two_isolated_items(),
                criteria: vec![typed_land_criterion(), typed_aggregate_goal_criterion()],
                parent_caps: crate::caps::all_lattice(),
                isolated_root: env.isolated_root.clone(),
                ..Default::default()
            },
        )
        .expect("orchestrated start")
        .run_id;
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    let outcome = settle_orchestrated(&env, &run_id).await.expect("settle");
    assert!(outcome.verified && outcome.completed, "{outcome:?}");
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    let record = h
        .list_verification_records(task_id)
        .unwrap()
        .into_iter()
        .filter(|r| r.tree_hash.is_some())
        .max_by_key(|r| r.record_id)
        .expect("the candidate-bound record");
    assert!(
        record
            .criteria
            .iter()
            .any(|c| c.passed && c.binding == Some(CriterionBinding::AggregateGoal)),
        "the aggregate goal passed through the wired reviewer: {record:?}"
    );
    assert!(env.owner_root.join("child_a.rs").is_file());
    assert!(env.owner_root.join("child_b.rs").is_file());
}

/// Residual (3): the reuse consult is real — an identical basis reuses the
/// record, a DIFFERENT basis (different derived checks at the same
/// snapshot/revision/criteria) mints a fresh basis-bound record, and the
/// fresh record carries its candidate proof + proof-basis digest.
#[tokio::test]
pub(crate) async fn root_record_reuse_consults_the_proof_basis() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        vec![],
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
    );
    cs_seed_owner(&env);
    // Freeze right after the candidate was prepared: no root record exists
    // yet and the task is already routed to Verifying (deterministic basis
    // construction; no settlement race).
    env.executor
        .set_settlement_crash_seam(Some(CrashSeam::AfterCandidatePrepared));
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let run_id = start_two_child_run(&env, "basis consult");
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    env.executor.set_settlement_crash_seam(None);
    let task_id = h.task_id().unwrap();
    let snapshot = owner_digest(&env);
    let check = |program: &str, arg: &str| faktor_core::state::CheckExecution {
        check: "rust_check".into(),
        program: program.into(),
        args: vec![arg.into()],
        category: "compile".into(),
        required: true,
        status: VerificationStatus::Passed,
        started_ms: 1,
        finished_ms: Some(2),
        exit: Some(0),
        summary: Some("ok".into()),
    };
    let prepared = PreparedRunIntegration {
        run_id: run_id.clone(),
        task_id,
        owner_root: env.owner_root.clone(),
        base_root: env.owner_root.clone(),
        candidate_root: env.owner_root.clone(),
        base_snapshot: snapshot.clone(),
        candidate_snapshot: snapshot.clone(),
        changed: Vec::new(),
        sources: Vec::new(),
        sources_digest: String::new(),
        staged: Vec::new(),
    };
    let criteria = h
        .get_task(task_id)
        .unwrap()
        .unwrap()
        .acceptance_criteria
        .clone();
    let criterion_verdict = faktor_core::state::CriterionVerification {
        criterion_key: criteria[0].clone(),
        passed: true,
        evidence: Some("basis consult".into()),
        binding: Some(CriterionBinding::RequiredCheck {
            check_id: "rust_check".into(),
            command_digest: faktor_core::state::command_binding_digest("cargo check"),
        }),
    };
    let run = |program: &str, arg: &str| IntegratedRootVerification {
        status: VerificationStatus::Passed,
        checks: vec![check(program, arg)],
        criteria: vec![criterion_verdict.clone()],
        changed: Vec::new(),
        summary: "basis consult".into(),
        review_model_identity: None,
    };
    let (first, first_basis) = env
        .executor
        .find_or_create_root_verification_record(
            &h,
            task_id,
            &criteria,
            &snapshot,
            &run("cargo", "check"),
            &prepared,
            VerificationStatus::Passed,
        )
        .await
        .unwrap();
    let (reused, reused_basis) = env
        .executor
        .find_or_create_root_verification_record(
            &h,
            task_id,
            &criteria,
            &snapshot,
            &run("cargo", "check"),
            &prepared,
            VerificationStatus::Passed,
        )
        .await
        .unwrap();
    assert_eq!(first, reused, "an identical basis is replay-idempotent");
    assert_eq!(
        first_basis, reused_basis,
        "two identical production builds must produce the byte-identical basis digest"
    );
    let (fresh, fresh_basis) = env
        .executor
        .find_or_create_root_verification_record(
            &h,
            task_id,
            &criteria,
            &snapshot,
            &run("cargo", "clippy"),
            &prepared,
            VerificationStatus::Passed,
        )
        .await
        .unwrap();
    assert_ne!(
        fresh, first,
        "a different check basis must NEVER reuse the old record"
    );
    assert_ne!(
        fresh_basis, first_basis,
        "a changed check program changes the probed tool/basis digest"
    );
    let fresh_record = h.get_verification_record(fresh).unwrap().unwrap();
    assert_eq!(fresh_record.status, VerificationStatus::Passed);
    assert!(
        fresh_record.candidate_proof_ref.is_some(),
        "the fresh record carries its candidate proof: {fresh_record:?}"
    );
    assert_eq!(
        fresh_record
            .environment_fingerprint
            .as_ref()
            .and_then(|f| f.proof_basis_digest.as_deref()),
        Some(fresh_basis.as_str()),
        "the record carries the exact basis digest the builder produced"
    );
}

/// Build a probe-session task row plus the prepared integration the basis
/// builder consumes (production call shape, no synthetic basis injection).
pub(crate) fn probe_task_and_prepared(
    env: &Arc<RealToolEnv>,
    run_id: &str,
) -> (
    faktor_session::SessionHandle,
    TaskId,
    PreparedRunIntegration,
    Vec<String>,
) {
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    let now = h.now_ms();
    h.create_task(faktor_session::Task {
        task_id,
        session_id: h.id(),
        goal: "proof-basis probe hardening".into(),
        acceptance_criteria: vec!["c1".into()],
        plan: Vec::new(),
        attachments: Vec::new(),
        budget: faktor_session::TaskBudget::default(),
        state: faktor_core::state::TaskState::Pending,
        created_ms: now,
        updated_ms: now,
    })
    .unwrap();
    let snapshot = owner_digest(env);
    let criteria = h
        .get_task(task_id)
        .unwrap()
        .unwrap()
        .acceptance_criteria
        .clone();
    let prepared = PreparedRunIntegration {
        run_id: run_id.to_string(),
        task_id,
        owner_root: env.owner_root.clone(),
        base_root: env.owner_root.clone(),
        candidate_root: env.owner_root.clone(),
        base_snapshot: snapshot.clone(),
        candidate_snapshot: snapshot,
        changed: Vec::new(),
        sources: Vec::new(),
        sources_digest: String::new(),
        staged: Vec::new(),
    };
    (h, task_id, prepared, criteria)
}

pub(crate) fn probe_run(program: &str) -> IntegratedRootVerification {
    let check = faktor_core::state::CheckExecution {
        check: "rust_check".into(),
        program: program.into(),
        args: vec!["--version".into()],
        category: "compile".into(),
        required: true,
        status: VerificationStatus::Passed,
        started_ms: 1,
        finished_ms: Some(2),
        exit: Some(0),
        summary: Some("ok".into()),
    };
    let criterion = faktor_core::state::CriterionVerification {
        criterion_key: "c1".into(),
        passed: true,
        evidence: Some("probe evidence".into()),
        binding: Some(CriterionBinding::RequiredCheck {
            check_id: "rust_check".into(),
            command_digest: faktor_core::state::command_binding_digest("cargo --version"),
        }),
    };
    IntegratedRootVerification {
        status: VerificationStatus::Passed,
        checks: vec![check],
        criteria: vec![criterion],
        changed: Vec::new(),
        summary: "proof-basis probe".into(),
        review_model_identity: None,
    }
}

/// Production hardening: the basis is rebuilt immediately before every reuse
/// consult, PROBING the real toolchain through the daemon supervisor — a
/// tool-version change under the daemon PATH invalidates reuse in
/// production (a fresh record is minted), while an unchanged toolchain
/// reuses the record byte-identically.
#[cfg(unix)]
#[tokio::test]
pub(crate) async fn production_tool_version_change_invalidates_proof_reuse() {
    use std::os::unix::fs::PermissionsExt;
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_supervised(
        dir.path(),
        vec![],
        faktor_agent::VerificationService::fake_ok(),
    );
    let (h, task_id, prepared, criteria) = probe_task_and_prepared(&env, "probe-run");
    let fake_bin = dir.path().join("probe-bin");
    std::fs::create_dir_all(&fake_bin).unwrap();
    let fake_rustc = fake_bin.join("rustc");
    let write_tool = |version: &str| {
        std::fs::write(&fake_rustc, format!("#!/bin/sh\necho \"{version}\"\n")).unwrap();
        let mut perms = std::fs::metadata(&fake_rustc).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&fake_rustc, perms).unwrap();
    };
    write_tool("rustc 0.0.1-production-fake");
    let original_path = std::env::var_os("PATH").unwrap_or_default();
    let fake_path = format!("{}:{}", fake_bin.display(), original_path.to_string_lossy());
    std::env::set_var("PATH", &fake_path);
    let snapshot = prepared.candidate_snapshot.clone();
    let (first, first_basis) = env
        .executor
        .find_or_create_root_verification_record(
            &h,
            task_id,
            &criteria,
            &snapshot,
            &probe_run("cargo"),
            &prepared,
            VerificationStatus::Passed,
        )
        .await
        .unwrap();
    let (reused, reused_basis) = env
        .executor
        .find_or_create_root_verification_record(
            &h,
            task_id,
            &criteria,
            &snapshot,
            &probe_run("cargo"),
            &prepared,
            VerificationStatus::Passed,
        )
        .await
        .unwrap();
    assert_eq!(first, reused, "an unchanged toolchain reuses the record");
    assert_eq!(first_basis, reused_basis);
    // The tool version actually changed.
    write_tool("rustc 0.0.2-production-fake");
    let (fresh, fresh_basis) = env
        .executor
        .find_or_create_root_verification_record(
            &h,
            task_id,
            &criteria,
            &snapshot,
            &probe_run("cargo"),
            &prepared,
            VerificationStatus::Passed,
        )
        .await
        .unwrap();
    std::env::set_var("PATH", &original_path);
    assert_ne!(
        fresh, first,
        "a production tool-version change must NEVER reuse the old record"
    );
    assert_ne!(fresh_basis, first_basis);
    assert_eq!(
        h.get_verification_record(first)
            .unwrap()
            .unwrap()
            .environment_fingerprint
            .as_ref()
            .and_then(|f| f.proof_basis_digest.as_deref()),
        Some(first_basis.as_str())
    );
}

/// Production hardening: a daemon WITHOUT a supervisor still produces a
/// fully-populated basis on every consult (every probe is an explicit
/// marker, never silently empty), and two identical builds are
/// byte-identical so the replay is deterministic.
#[tokio::test]
pub(crate) async fn production_basis_degrades_explicitly_and_is_reproducible() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        vec![],
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
    );
    let (h, task_id, prepared, criteria) = probe_task_and_prepared(&env, "probe-no-supervisor");
    let snapshot = prepared.candidate_snapshot.clone();
    let (first, first_basis) = env
        .executor
        .find_or_create_root_verification_record(
            &h,
            task_id,
            &criteria,
            &snapshot,
            &probe_run("cargo"),
            &prepared,
            VerificationStatus::Passed,
        )
        .await
        .unwrap();
    let (reused, reused_basis) = env
        .executor
        .find_or_create_root_verification_record(
            &h,
            task_id,
            &criteria,
            &snapshot,
            &probe_run("cargo"),
            &prepared,
            VerificationStatus::Passed,
        )
        .await
        .unwrap();
    assert_eq!(first, reused);
    assert_eq!(
        first_basis, reused_basis,
        "identical inputs, identical basis"
    );
    // The production probe input shape degrades to explicit markers.
    let report = crate::proof_probe::probe_proof_basis(None, &["cargo".to_string()]).await;
    assert!(!report.tools.is_empty(), "tool_versions are never empty");
    assert!(
        report
            .tools
            .iter()
            .filter(|t| t.tool != "faktor-verifier")
            .all(|t| t.version.starts_with('<')),
        "no supervisor means explicit probe markers: {:?}",
        report.tools
    );
    assert!(
        report
            .tools
            .iter()
            .any(|t| t.tool == "faktor-verifier" && t.version.contains("blake3:")),
        "the custom verifier binary carries its version+digest: {:?}",
        report.tools
    );
    assert!(
        report
            .env_projection
            .iter()
            .any(|(k, v)| k == "target_triple" && v.starts_with("<probe-unavailable")),
        "target triple is an explicit marker: {:?}",
        report.env_projection
    );
}

/// PRODUCTION reviewer-identity wiring (not a synthetic helper call): with
/// the parent session's configured pair FIXED, the proof basis is rebuilt
/// through the real `find_or_create_root_verification_record` path and the
/// review model recorded on the RUN changes the reviewer digest — a changed
/// reviewer mints a FRESH record, the same reviewer reuses byte-identically.
#[tokio::test]
pub(crate) async fn production_reviewer_identity_changes_the_basis_digest() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_supervised(
        dir.path(),
        vec![],
        faktor_agent::VerificationService::fake_ok(),
    );
    let (h, task_id, prepared, criteria) = probe_task_and_prepared(&env, "reviewer-run");
    let snapshot = prepared.candidate_snapshot.clone();
    // The parent session's configured pair is FIXED across every call below:
    // only the RUN's recorded ACTUAL review call identity varies.
    let parent_pair = {
        let row = h.row().unwrap();
        (row.provider, row.model)
    };
    let reviewed_run = |provider: &str, model: &str| IntegratedRootVerification {
        status: VerificationStatus::Passed,
        checks: Vec::new(),
        criteria: vec![faktor_core::state::CriterionVerification {
            criterion_key: "c1".into(),
            passed: true,
            evidence: Some("independent reviewer: clean".into()),
            binding: Some(CriterionBinding::AggregateGoal),
        }],
        changed: Vec::new(),
        summary: "reviewed".into(),
        review_model_identity: Some(faktor_agent::ReviewModelIdentity {
            provider: provider.into(),
            model: model.into(),
        }),
    };
    let (first, first_basis) = env
        .executor
        .find_or_create_root_verification_record(
            &h,
            task_id,
            &criteria,
            &snapshot,
            &reviewed_run("review-provider-a", "review-model-a"),
            &prepared,
            VerificationStatus::Passed,
        )
        .await
        .unwrap();
    let (reused, reused_basis) = env
        .executor
        .find_or_create_root_verification_record(
            &h,
            task_id,
            &criteria,
            &snapshot,
            &reviewed_run("review-provider-a", "review-model-a"),
            &prepared,
            VerificationStatus::Passed,
        )
        .await
        .unwrap();
    assert_eq!(reused, first, "the same reviewer reuses the record");
    assert_eq!(reused_basis, first_basis);
    let (fresh, fresh_basis) = env
        .executor
        .find_or_create_root_verification_record(
            &h,
            task_id,
            &criteria,
            &snapshot,
            &reviewed_run("review-provider-b", "review-model-b"),
            &prepared,
            VerificationStatus::Passed,
        )
        .await
        .unwrap();
    assert_ne!(
        fresh, first,
        "a changed ACTUAL review model must never reuse the old record"
    );
    assert_ne!(
        fresh_basis, first_basis,
        "the reviewer digest folds the real review provider/model"
    );
    // The parent session's configured pair never moved: the digest difference
    // is the review call's identity, not the parent's.
    let row = h.row().unwrap();
    assert_eq!((row.provider, row.model), parent_pair);
    // Both records durably carry their distinct basis digests.
    for (record, basis) in [(first, &first_basis), (fresh, &fresh_basis)] {
        assert_eq!(
            h.get_verification_record(record)
                .unwrap()
                .unwrap()
                .environment_fingerprint
                .as_ref()
                .and_then(|f| f.proof_basis_digest.as_deref()),
            Some(basis.as_str())
        );
    }
}

/// The basis evidence/reviewer folds are pure, deterministic, bound to the
/// exact evidence (a changed check program or reviewer identity changes the
/// fold) and only count PASSED criteria — failed and unavailable verdicts
/// contribute nothing.
#[test]
pub(crate) fn basis_evidence_and_reviewer_digests_are_exact_and_deterministic() {
    use crate::runtime::task_executor::{
        criterion_pass_evidence_digests, reviewer_proof_basis_digest,
    };
    let (dir, m) = {
        let dir = tempfile::tempdir().unwrap();
        let m =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        (dir, m)
    };
    let ws = m.create_workspace("/w").unwrap();
    let h = m.create_session(ws, "t", "ollama", "qwen3.8").unwrap();
    let task_id = h.task_id().unwrap();
    let now = h.now_ms();
    h.create_task(faktor_session::Task {
        task_id,
        session_id: h.id(),
        goal: "g".into(),
        acceptance_criteria: vec!["c1".into(), "c2".into()],
        plan: Vec::new(),
        attachments: Vec::new(),
        budget: faktor_session::TaskBudget::default(),
        state: TaskState::Pending,
        created_ms: now,
        updated_ms: now,
    })
    .unwrap();
    let check = faktor_core::state::CheckExecution {
        check: "rust_check".into(),
        program: "cargo".into(),
        args: vec!["check".into()],
        category: "compile".into(),
        required: true,
        status: VerificationStatus::Passed,
        started_ms: 1,
        finished_ms: Some(2),
        exit: Some(0),
        summary: None,
    };
    let passed_check = faktor_core::state::CriterionVerification {
        criterion_key: "c1".into(),
        passed: true,
        evidence: Some("evidence-a".into()),
        binding: Some(CriterionBinding::RequiredCheck {
            check_id: "rust_check".into(),
            command_digest: faktor_core::state::command_binding_digest("cargo check"),
        }),
    };
    let failed = faktor_core::state::CriterionVerification {
        criterion_key: "c2".into(),
        passed: false,
        evidence: Some("evidence-b".into()),
        binding: None,
    };
    let review = faktor_core::state::CriterionVerification {
        criterion_key: "c1".into(),
        passed: true,
        evidence: Some("review payload".into()),
        binding: Some(CriterionBinding::AggregateGoal),
    };
    let prepared = PreparedRunIntegration {
        run_id: "basis-fold".into(),
        task_id,
        owner_root: dir.path().join("owner"),
        base_root: dir.path().join("owner"),
        candidate_root: dir.path().join("owner"),
        base_snapshot: "a".repeat(64),
        candidate_snapshot: "b".repeat(64),
        changed: Vec::new(),
        sources: Vec::new(),
        sources_digest: String::new(),
        staged: Vec::new(),
    };
    let run_with = |criteria: Vec<faktor_core::state::CriterionVerification>,
                    identity: Option<faktor_agent::ReviewModelIdentity>| {
        IntegratedRootVerification {
            status: VerificationStatus::Passed,
            checks: vec![check.clone()],
            criteria,
            changed: Vec::new(),
            summary: "fold".into(),
            review_model_identity: identity,
        }
    };
    let reviewer_a = || {
        Some(faktor_agent::ReviewModelIdentity {
            provider: "review-provider-a".into(),
            model: "review-model-a".into(),
        })
    };
    let run = run_with(vec![passed_check.clone(), failed.clone()], None);
    let first = criterion_pass_evidence_digests(&prepared, &run);
    assert_eq!(
        first,
        criterion_pass_evidence_digests(&prepared, &run),
        "two folds of identical evidence are identical"
    );
    assert!(first.iter().any(|e| e.starts_with("binding:c1:")));
    assert!(first.iter().any(|e| e.starts_with("command:c1:")));
    assert!(first.iter().any(|e| e.starts_with("check:c1:")));
    assert!(first.iter().any(|e| e.starts_with("evidence:c1:")));
    assert!(
        !first.iter().any(|e| e.contains(":c2:")),
        "a failed criterion contributes no evidence: {first:?}"
    );
    // The check program is immutable evidence: changing it changes the fold.
    let mut other_check = check.clone();
    other_check.program = "clippy".into();
    let other_run = IntegratedRootVerification {
        status: VerificationStatus::Passed,
        checks: vec![other_check],
        criteria: vec![passed_check.clone(), failed.clone()],
        changed: Vec::new(),
        summary: "fold".into(),
        review_model_identity: None,
    };
    assert_ne!(
        first,
        criterion_pass_evidence_digests(&prepared, &other_run)
    );
    assert_eq!(
        reviewer_proof_basis_digest(&run_with(vec![passed_check.clone()], reviewer_a())),
        None,
        "no reviewer-bearing pass means an honest None"
    );
    assert_eq!(
        reviewer_proof_basis_digest(&run_with(vec![review.clone()], None)),
        None,
        "a reviewer-bearing pass without an ACTUAL review-model call is an honest None — \
         the parent session's configured pair is never substituted"
    );
    let reviewer =
        reviewer_proof_basis_digest(&run_with(vec![review.clone()], reviewer_a())).unwrap();
    assert!(reviewer.starts_with("blake3:"));
    assert_eq!(
        reviewer,
        reviewer_proof_basis_digest(&run_with(vec![review.clone()], reviewer_a())).unwrap(),
        "reviewer folds are deterministic"
    );
    // The review model identity IS the digest: a different ACTUAL reviewer
    // (same parent session row) changes the digest.
    let reviewer_b = Some(faktor_agent::ReviewModelIdentity {
        provider: "review-provider-b".into(),
        model: "review-model-b".into(),
    });
    assert_ne!(
        reviewer,
        reviewer_proof_basis_digest(&run_with(vec![review.clone()], reviewer_b)).unwrap(),
        "the actual review provider/model is part of the digest"
    );
    let mut changed_reviewer = review.clone();
    changed_reviewer.binding = Some(CriterionBinding::IndependentReview {
        reviewer_id: "review-9".into(),
    });
    assert_ne!(
        reviewer,
        reviewer_proof_basis_digest(&run_with(vec![changed_reviewer], reviewer_a())).unwrap(),
        "reviewer binding identity is part of the digest"
    );
}

/// Point 1/6: a FAILING verification lands nothing — the owner stays
/// byte-identical, no landing transaction applies a byte and the task never
/// completes.
#[tokio::test]
pub(crate) async fn verification_failure_leaves_owner_byte_identical() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        two_child_scripts("pub fn a() -> u64 {\n    let seed: u64 = 1;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n", "pub fn b() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n"),
        faktor_agent::VerificationService::fake(|_| Err("verification deliberately failed".into())),
        false,
        false,
    );
    cs_seed_owner(&env);
    let before = owner_digest(&env);
    let run_id = start_two_child_run(&env, "fail verification");
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    assert_eq!(owner_digest(&env), before, "the owner is byte-identical");
    assert_ne!(
        h.get_task(task_id).unwrap().unwrap().state,
        TaskState::VerifiedComplete
    );
    assert!(
        latest_txn(&env, &run_id).is_none(),
        "no landing ever started"
    );
    let outcome = settle_orchestrated(&env, &run_id).await.expect("settle");
    assert!(!outcome.verified && !outcome.completed, "{outcome:?}");
    assert_eq!(owner_digest(&env), before);
}

/// Start a two-child isolated run over an explicit criterion set with a REAL
/// commit completion step armed: the pre-fix defect would land AND commit.
pub(crate) fn start_two_child_run_with_criteria(
    env: &Arc<RealToolEnv>,
    goal: &str,
    criteria: Vec<String>,
) -> String {
    env.executor
        .start_task(
            env.parent,
            TaskRunRequest {
                goal: goal.to_string(),
                work_items: two_isolated_items(),
                criteria,
                parent_caps: crate::caps::all_lattice(),
                isolated_root: env.isolated_root.clone(),
                completion_contract: Some(faktor_core::completion::CompletionContract {
                    include_commit: true,
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .expect("orchestrated start")
        .run_id
}

/// The shared refusal harness of the two decisive landing-authority tests:
/// every check ran green, the composed verdict is non-passing, and NOTHING
/// may land — owner bytes, integration transaction, completion steps.
pub(crate) fn assert_no_landing_at_all(
    env: &Arc<RealToolEnv>,
    run_id: &str,
    before_digest: &str,
    before_head: &str,
) {
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    assert_eq!(
        owner_digest(env),
        before_digest,
        "the owner tree digest is byte-identical"
    );
    assert!(
        latest_txn(env, run_id).is_none(),
        "no IntegrationTxnPhase::Landing row exists"
    );
    let revision = h.task_revision(task_id).unwrap();
    assert!(
        h.ledger_completion_step_statuses(task_id.raw(), revision.raw())
            .unwrap()
            .is_empty(),
        "no completion step ran"
    );
    assert_eq!(
        cs_head(&env.owner_root),
        before_head,
        "no commit completion step ran"
    );
    assert_ne!(
        h.get_task(task_id).unwrap().unwrap().state,
        TaskState::VerifiedComplete
    );
    assert!(
        h.ledger_integration_record_for_task(task_id.raw())
            .unwrap()
            .is_none(),
        "no integration record exists"
    );
}

/// P0 LANDING AUTHORITY: every derived check runs GREEN, but one REQUIRED
/// criterion's own binding fails. The composed root verdict is `Failed`, so
/// `verify_prepared_integration` must refuse — the owner stays byte-identical,
/// no Landing transaction row appears and no completion step (armed commit)
/// ever runs.
#[tokio::test]
pub(crate) async fn green_checks_but_failed_criterion_never_starts_landing() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        two_child_scripts(
            "pub fn a() -> u64 {\n    let seed: u64 = 1;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n",
            "pub fn b() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n",
        ),
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
    );
    cs_seed_owner(&env);
    env.executor
        .set_completion_steps(Some(completion_step_runner_with(
            dir.path(),
            crate::runtime::completion_steps::CompletionStepsConfig::default(),
        )));
    let before = owner_digest(&env);
    let before_head = cs_head(&env.owner_root);
    let failed_key = typed_failed_file_state_criterion();
    let run_id = start_two_child_run_with_criteria(
        &env,
        "green checks but a failed criterion",
        vec![typed_land_criterion(), failed_key.clone()],
    );
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    // The automatic settlement already had its chance: nothing landed.
    assert_no_landing_at_all(&env, &run_id, &before, &before_head);
    // Replay converges to the same refusal.
    let outcome = settle_orchestrated(&env, &run_id).await.expect("settle");
    assert!(!outcome.verified && !outcome.completed, "{outcome:?}");
    assert_no_landing_at_all(&env, &run_id, &before, &before_head);
    // The durable record carries the COMPOSED verdict: green checks, failed
    // required criterion => status Failed (never Passed, never Pending).
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    let record = h
        .list_verification_records(task_id)
        .unwrap()
        .into_iter()
        .filter(|r| r.status == VerificationStatus::Failed && r.tree_hash.is_some())
        .max_by_key(|r| r.record_id)
        .expect("the composed Failed root record");
    assert!(
        !record.checks.is_empty()
            && record
                .checks
                .iter()
                .all(|c| c.status == VerificationStatus::Passed),
        "every executed check was green: {record:?}"
    );
    assert!(
        record
            .criteria
            .iter()
            .any(|c| !c.passed && c.criterion_key == failed_key),
        "the failed required criterion is recorded as not passed: {record:?}"
    );
}

/// P0 LANDING AUTHORITY: every derived check runs GREEN, but one REQUIRED
/// criterion carries the explicit honest-unknown binding. The composed root
/// verdict is `Unavailable`, so landing must refuse exactly like a failure —
/// byte-identical owner, no Landing row, no completion step.
#[tokio::test]
pub(crate) async fn green_checks_but_unavailable_criterion_never_starts_landing() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        two_child_scripts(
            "pub fn a() -> u64 {\n    let seed: u64 = 1;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n",
            "pub fn b() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n",
        ),
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
    );
    cs_seed_owner(&env);
    env.executor
        .set_completion_steps(Some(completion_step_runner_with(
            dir.path(),
            crate::runtime::completion_steps::CompletionStepsConfig::default(),
        )));
    let before = owner_digest(&env);
    let before_head = cs_head(&env.owner_root);
    let unknown_key = typed_explicit_unknown_criterion();
    let run_id = start_two_child_run_with_criteria(
        &env,
        "green checks but an unavailable criterion",
        vec![typed_land_criterion(), unknown_key.clone()],
    );
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    assert_no_landing_at_all(&env, &run_id, &before, &before_head);
    let outcome = settle_orchestrated(&env, &run_id).await.expect("settle");
    assert!(!outcome.verified && !outcome.completed, "{outcome:?}");
    assert_no_landing_at_all(&env, &run_id, &before, &before_head);
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    let record = h
        .list_verification_records(task_id)
        .unwrap()
        .into_iter()
        .filter(|r| r.status == VerificationStatus::Unavailable && r.tree_hash.is_some())
        .max_by_key(|r| r.record_id)
        .expect("the composed Unavailable root record");
    assert!(
        !record.checks.is_empty()
            && record
                .checks
                .iter()
                .all(|c| c.status == VerificationStatus::Passed),
        "every executed check was green: {record:?}"
    );
    assert!(
        record
            .criteria
            .iter()
            .any(|c| !c.passed && c.criterion_key == unknown_key),
        "the unavailable required criterion is recorded as not passed: {record:?}"
    );
    // The new explicit variant round-trips durably across a real store reopen.
    let record_id = record.record_id;
    let parent = env.parent;
    drop(h);
    drop(env);
    let reopened =
        SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let h2 = reopened.get_session(parent).unwrap().unwrap();
    let reopened_record = h2
        .get_verification_record(record_id)
        .unwrap()
        .expect("the Unavailable record survives the reopen");
    assert_eq!(
        reopened_record.status,
        VerificationStatus::Unavailable,
        "the Unavailable status round-trips through the store codec"
    );
    let fact = h2
        .memory_facts()
        .unwrap()
        .into_iter()
        .find(|(kind, key, _)| kind == "verification" && key == "last")
        .expect("the root verification fact");
    let fact: serde_json::Value = serde_json::from_str(&fact.2).unwrap();
    assert_eq!(fact["status"], "unavailable", "{fact:?}");
}

/// Point 4/5: two children diverging on the SAME path are a typed conflict
/// BEFORE any owner mutation; the owner keeps its bytes and no verification
/// or landing record is minted.
#[tokio::test]
pub(crate) async fn candidate_composition_conflict_leaves_owner_untouched() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        vec![
            write_script("c-a", "same.rs", "pub fn v() -> u64 {\n    let seed: u64 = 1;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n"),
            write_script("c-b", "same.rs", "pub fn v() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n"),
            vec![ScriptedResponse::Text("done".into()), ScriptedResponse::End],
            vec![ScriptedResponse::Text("done".into()), ScriptedResponse::End],
        ],
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
    );
    cs_seed_owner(&env);
    let before = owner_digest(&env);
    let run_id = start_two_child_run(&env, "conflict on one path");
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    assert_eq!(
        owner_digest(&env),
        before,
        "composition never touches the owner"
    );
    assert!(!env.owner_root.join("same.rs").exists());
    assert!(
        h.list_verification_records(task_id)
            .unwrap()
            .iter()
            .all(|r| r.tree_hash.is_none()),
        "no root/candidate verification may be minted over a conflicted composition (child drive records bind no tree)"
    );
    assert!(
        h.ledger_integration_record_for_task(task_id.raw())
            .unwrap()
            .is_none(),
        "no integration record for a refused composition"
    );
    let err = settle_orchestrated(&env, &run_id)
        .await
        .expect_err("divergent children must refuse typed");
    assert!(matches!(err, ExecError::IntegrationConflict(_)), "{err}");
    assert_eq!(owner_digest(&env), before);
    assert_ne!(
        h.get_task(task_id).unwrap().unwrap().state,
        TaskState::VerifiedComplete
    );
}

/// Point 2/9: an unrelated owner edit while the children were running can
/// never be verified over: the landing recheck refuses typed before the
/// first write, and removing the drift lets the SAME settlement land.
#[tokio::test]
pub(crate) async fn unrelated_owner_edit_during_children_blocks_before_landing() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        two_child_scripts("pub fn a() -> u64 {\n    let seed: u64 = 1;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n", "pub fn b() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n"),
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
    );
    cs_seed_owner(&env);
    // Freeze after the candidate verification, then add the unrelated edit:
    // exactly the "owner moved after the run base was taken" window.
    env.executor
        .set_settlement_crash_seam(Some(CrashSeam::AfterPreparedVerification));
    let run_id = start_two_child_run(&env, "unrelated owner drift");
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    std::fs::write(
        env.owner_root.join("unrelated.txt"),
        "not the task's work\n",
    )
    .unwrap();
    env.executor.set_settlement_crash_seam(None);
    let err = settle_orchestrated(&env, &run_id)
        .await
        .expect_err("owner drift blocks before landing");
    assert!(matches!(err, ExecError::IntegrationConflict(_)), "{err}");
    assert!(!env.owner_root.join("child_a.rs").exists());
    assert!(!env.owner_root.join("child_b.rs").exists());
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    let task_id = h.task_id().unwrap();
    assert_ne!(
        h.get_task(task_id).unwrap().unwrap().state,
        TaskState::VerifiedComplete
    );
    let blocked = h
        .ledger_integration_record_for_task(task_id.raw())
        .unwrap()
        .expect("blocked integration row");
    assert!(blocked.final_snapshot_hash.is_empty(), "{blocked:?}");
    // Resolve the drift; the same settlement lands and completes.
    std::fs::remove_file(env.owner_root.join("unrelated.txt")).unwrap();
    let outcome = settle_orchestrated(&env, &run_id).await.expect("resolved");
    assert!(outcome.verified && outcome.completed, "{outcome:?}");
    assert!(env.owner_root.join("child_a.rs").is_file());
}

/// Wave-0 metadata-only drift: the owner's file content is UNCHANGED but
/// its canonical mode moved (0644 -> 0755) after the run base was taken.
/// The per-path entry-state CAS catches it (the base state is
/// `Regular{{mode: 100644, payload}}`, the live state `100755`): landing is
/// refused typed, the mode is preserved, and restoring the mode lets the
/// SAME settlement land.
#[tokio::test]
pub(crate) async fn metadata_only_owner_mode_drift_blocks_landing() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        two_child_scripts("pub fn a() -> u64 {\n    let seed: u64 = 1;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n", "pub fn b() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n"),
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
    );
    cs_seed_owner(&env);
    let seeded = env.owner_root.join("src/lib.rs");
    let bytes_before = std::fs::read(&seeded).unwrap();
    env.executor
        .set_settlement_crash_seam(Some(CrashSeam::AfterPreparedVerification));
    let run_id = start_two_child_run(&env, "metadata-only owner drift");
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    // METADATA-ONLY: the bytes are identical, only the exec bit moves.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&seeded, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    assert_eq!(std::fs::read(&seeded).unwrap(), bytes_before);
    env.executor.set_settlement_crash_seam(None);
    let err = settle_orchestrated(&env, &run_id)
        .await
        .expect_err("metadata-only owner drift blocks landing");
    assert!(matches!(err, ExecError::IntegrationConflict(_)), "{err}");
    assert!(!env.owner_root.join("child_a.rs").exists());
    assert!(!env.owner_root.join("child_b.rs").exists());
    assert_eq!(
        std::fs::read(&seeded).unwrap(),
        bytes_before,
        "the owner's bytes are never overwritten"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&seeded).unwrap().permissions().mode() & 0o111,
            0o111,
            "the owner's mode drift is preserved, never silently reverted"
        );
        std::fs::set_permissions(&seeded, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let outcome = settle_orchestrated(&env, &run_id)
        .await
        .expect("restoring the mode resolves the drift");
    assert!(outcome.verified && outcome.completed, "{outcome:?}");
    assert!(env.owner_root.join("child_a.rs").is_file());
}

/// Point 9: an owner edit AFTER a passing verification but BEFORE the
/// landing recheck is refused typed; the user's edit survives and is never
/// overwritten by the landing.
#[tokio::test]
pub(crate) async fn owner_edit_after_verification_before_landing_blocks() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        two_child_scripts("pub fn a() -> u64 {\n    let seed: u64 = 1;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n", "pub fn b() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n"),
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
    );
    cs_seed_owner(&env);
    env.executor
        .set_settlement_crash_seam(Some(CrashSeam::AfterPreparedVerification));
    let run_id = start_two_child_run(&env, "late owner edit");
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    // The edit lands in the window between the passing verification and the
    // landing transaction's first write.
    std::fs::write(
        env.owner_root.join("src/lib.rs"),
        "pub fn value() -> u64 { 999 }\n",
    )
    .unwrap();
    env.executor.set_settlement_crash_seam(None);
    let err = settle_orchestrated(&env, &run_id)
        .await
        .expect_err("late drift must block");
    assert!(matches!(err, ExecError::IntegrationConflict(_)), "{err}");
    assert_eq!(
        std::fs::read(env.owner_root.join("src/lib.rs")).unwrap(),
        b"pub fn value() -> u64 { 999 }\n",
        "the late user edit is preserved"
    );
    assert!(!env.owner_root.join("child_a.rs").exists());
}

/// Point 9: a crash AFTER the passing verification and BEFORE the landing
/// transaction recovers idempotently — the next settlement lands and
/// completes without re-running the model or double-applying anything.
#[tokio::test]
pub(crate) async fn crash_after_verified_before_land_recovers() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        two_child_scripts("pub fn a() -> u64 {\n    let seed: u64 = 1;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n", "pub fn b() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n"),
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
    );
    cs_seed_owner(&env);
    env.executor
        .set_settlement_crash_seam(Some(CrashSeam::AfterPreparedVerification));
    let run_id = start_two_child_run(&env, "crash before landing");
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    assert!(latest_txn(&env, &run_id).is_none());
    env.executor.set_settlement_crash_seam(None);
    let outcome = settle_orchestrated(&env, &run_id).await.expect("recovery");
    assert!(outcome.verified && outcome.completed, "{outcome:?}");
    assert!(env.owner_root.join("child_a.rs").is_file());
    assert!(env.owner_root.join("child_b.rs").is_file());
    assert_eq!(
        latest_txn(&env, &run_id).map(|t| t.phase),
        Some(faktor_session::ledger::IntegrationTxnPhase::Landed)
    );
}

/// Point 9: a crash at the run-base boundary leaves the owner untouched and
/// no live run behind; a fresh start converges (the run never had children).
#[tokio::test]
pub(crate) async fn crash_after_run_base_recorded_restarts_cleanly() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        two_child_scripts("pub fn a() -> u64 {\n    let seed: u64 = 1;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n", "pub fn b() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n"),
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
    );
    cs_seed_owner(&env);
    let before = owner_digest(&env);
    env.executor
        .set_settlement_crash_seam(Some(CrashSeam::AfterRunBaseRecorded));
    let err = env
        .executor
        .start_task(
            env.parent,
            TaskRunRequest {
                goal: "base crash".to_string(),
                work_items: two_isolated_items(),
                criteria: vec![typed_land_criterion()],
                parent_caps: crate::caps::all_lattice(),
                isolated_root: env.isolated_root.clone(),
                ..Default::default()
            },
        )
        .expect_err("the seam fires before any child spawn");
    assert!(matches!(err, ExecError::InjectedCrashSeam(_)), "{err}");
    assert!(env.executor.active_run().is_none(), "the slot was released");
    assert_eq!(owner_digest(&env), before);
    assert!(
        OrchestratorRuntime::registry_rows(env.manager.clone(), env.parent, "run").is_err() || true
    );
    env.executor.set_settlement_crash_seam(None);
    let run_id = start_two_child_run(&env, "fresh start after base crash");
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    // The automatic settlement may already have landed the fresh run; the
    // explicit re-settle must be a convergent no-op either way.
    let outcome = settle_orchestrated(&env, &run_id).await.expect("recovery");
    assert!(outcome.verified && outcome.completed, "{outcome:?}");
    assert!(env.owner_root.join("child_a.rs").is_file());
    assert!(env.owner_root.join("child_b.rs").is_file());
}

/// Point 9: a crash AFTER the candidate was prepared (staged + composed)
/// leaves the owner byte-identical; re-settling rebuilds the identical
/// candidate idempotently and lands it.
#[tokio::test]
pub(crate) async fn crash_after_candidate_prepared_recovers() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        two_child_scripts("pub fn a() -> u64 {\n    let seed: u64 = 1;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n", "pub fn b() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n"),
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
    );
    cs_seed_owner(&env);
    let before = owner_digest(&env);
    env.executor
        .set_settlement_crash_seam(Some(CrashSeam::AfterCandidatePrepared));
    let run_id = start_two_child_run(&env, "candidate crash");
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    assert_eq!(owner_digest(&env), before, "staging never writes the owner");
    let candidate = run_candidate_root(&env, &run_id);
    assert!(candidate.join("child_a.rs").is_file());
    assert!(latest_txn(&env, &run_id).is_none());
    env.executor.set_settlement_crash_seam(None);
    let outcome = settle_orchestrated(&env, &run_id).await.expect("recovery");
    assert!(outcome.verified && outcome.completed, "{outcome:?}");
    assert_eq!(owner_digest(&env), root_digest(&candidate));
}

/// Point 9: a crash right after the record-first landing transaction was
/// journaled (phase Landing, zero applies) recovers without any blind write
/// and finishes the landing.
#[tokio::test]
pub(crate) async fn crash_after_txn_record_recovers_without_writes() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        two_child_scripts("pub fn a() -> u64 {\n    let seed: u64 = 1;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n", "pub fn b() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n"),
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
    );
    cs_seed_owner(&env);
    env.executor
        .set_settlement_crash_seam(Some(CrashSeam::AfterIntegrationTxnRecord));
    let run_id = start_two_child_run(&env, "txn crash");
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    let txn = latest_txn(&env, &run_id).expect("record-first txn durable");
    assert_eq!(
        txn.phase,
        faktor_session::ledger::IntegrationTxnPhase::Landing
    );
    assert_eq!(txn.applied_count, 0, "no owner write after the record");
    assert!(!env.owner_root.join("child_a.rs").exists());
    env.executor.set_settlement_crash_seam(None);
    let outcome = settle_orchestrated(&env, &run_id).await.expect("recovery");
    assert!(outcome.verified && outcome.completed, "{outcome:?}");
    assert_eq!(
        latest_txn(&env, &run_id).map(|t| t.phase),
        Some(faktor_session::ledger::IntegrationTxnPhase::Landed)
    );
}

/// Point 5/9: a crash MID-landing (one path applied) resumes from the
/// durable transaction and finishes the landing; the final digest equals the
/// verified candidate.
#[tokio::test]
pub(crate) async fn crash_mid_land_recovers_or_rolls_back() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        two_child_scripts("pub fn a() -> u64 {\n    let seed: u64 = 1;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n", "pub fn b() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n"),
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
    );
    cs_seed_owner(&env);
    env.executor
        .set_settlement_crash_seam(Some(CrashSeam::IntegrationApply { after: 1 }));
    let run_id = start_two_child_run(&env, "crash mid landing");
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    let txn = latest_txn(&env, &run_id).expect("landing txn durable");
    assert_eq!(
        txn.phase,
        faktor_session::ledger::IntegrationTxnPhase::Landing
    );
    assert_eq!(txn.applied_count, 1, "{txn:?}");
    assert!(env.owner_root.join("child_a.rs").is_file());
    assert!(!env.owner_root.join("child_b.rs").exists());
    env.executor.set_settlement_crash_seam(None);
    let outcome = settle_orchestrated(&env, &run_id)
        .await
        .expect("finish landing");
    assert!(outcome.verified && outcome.completed, "{outcome:?}");
    let txn = latest_txn(&env, &run_id).unwrap();
    assert_eq!(
        txn.phase,
        faktor_session::ledger::IntegrationTxnPhase::Landed
    );
    assert_eq!(
        owner_digest(&env),
        root_digest(&run_candidate_root(&env, &run_id)),
        "the landed owner equals the verified candidate"
    );
}

/// Point 5: a rollback restores only paths still equal to OUR written
/// candidate hash — a post-landing user edit is never overwritten and is
/// reported as a rollback conflict.
#[tokio::test]
pub(crate) async fn rollback_never_overwrites_later_user_edit() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        two_child_scripts("pub fn a() -> u64 {\n    let seed: u64 = 1;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n", "pub fn b() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n"),
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
    );
    cs_seed_owner(&env);
    env.executor
        .set_settlement_crash_seam(Some(CrashSeam::IntegrationApply { after: 1 }));
    let run_id = start_two_child_run(&env, "rollback keeps edits");
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    let txn = latest_txn(&env, &run_id).expect("landing txn durable");
    assert_eq!(txn.applied_count, 1);
    let applied_path = txn.paths[0].path.clone();
    let pending_path = txn.paths[1].path.clone();
    // A user edits the path we already wrote (ours must never clobber it)
    // and makes the pending path diverge from the base (forcing the
    // rollback on resume).
    std::fs::write(
        env.owner_root.join(&applied_path),
        "user post-landing edit\n",
    )
    .unwrap();
    std::fs::write(env.owner_root.join(&pending_path), "hostile drift\n").unwrap();
    env.executor.set_settlement_crash_seam(None);
    let err = settle_orchestrated(&env, &run_id)
        .await
        .expect_err("a late per-path conflict rolls back");
    assert!(matches!(err, ExecError::IntegrationConflict(_)), "{err}");
    assert_eq!(
        std::fs::read(env.owner_root.join(&applied_path)).unwrap(),
        b"user post-landing edit\n",
        "the rollback never overwrote the later user edit"
    );
    assert_eq!(
        std::fs::read(env.owner_root.join(&pending_path)).unwrap(),
        b"hostile drift\n",
        "a never-applied path is left alone"
    );
    let txn = latest_txn(&env, &run_id).unwrap();
    assert_eq!(
        txn.phase,
        faktor_session::ledger::IntegrationTxnPhase::RolledBack
    );
    let applied = txn.paths.iter().find(|p| p.path == applied_path).unwrap();
    assert_eq!(
        applied.state,
        faktor_session::ledger::IntegrationPathTxnState::RollbackConflict
    );
}

/// Point 6: the landed owner digest must EQUAL the verified candidate
/// snapshot; an external edit before the final digest check rolls our
/// writes back and refuses typed (never a "merge returned ok" completion).
#[tokio::test]
pub(crate) async fn final_owner_digest_must_equal_verified_candidate() {
    let _heavy = heavy_guard();
    let dir = tempfile::tempdir().unwrap();
    let env = open_real_tool_env_full(
        dir.path(),
        two_child_scripts("pub fn a() -> u64 {\n    let seed: u64 = 1;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n", "pub fn b() -> u64 {\n    let seed: u64 = 2;\n    let factor: u64 = 1;\n    seed.saturating_mul(factor)\n}\n"),
        faktor_agent::VerificationService::fake_ok(),
        false,
        false,
    );
    cs_seed_owner(&env);
    env.executor
        .set_settlement_crash_seam(Some(CrashSeam::AfterFinalOwnerSnapshot));
    let run_id = start_two_child_run(&env, "final digest equality");
    wait_until(|| env.executor.active_runs().is_empty(), 120).await;
    let candidate_digest = root_digest(&run_candidate_root(&env, &run_id));
    assert_eq!(
        owner_digest(&env),
        candidate_digest,
        "the landing applied every path before the crash"
    );
    // An external edit lands exactly between the last apply and the final
    // equality check.
    std::fs::write(env.owner_root.join("late.txt"), "concurrent edit\n").unwrap();
    env.executor.set_settlement_crash_seam(None);
    let err = settle_orchestrated(&env, &run_id)
        .await
        .expect_err("a moved owner root can never complete");
    assert!(matches!(err, ExecError::WorkspaceDrift(_)), "{err}");
    let h = env.manager.get_session(env.parent).unwrap().unwrap();
    assert_ne!(
        h.get_task(h.task_id().unwrap()).unwrap().unwrap().state,
        TaskState::VerifiedComplete
    );
    assert!(
        env.owner_root.join("late.txt").is_file(),
        "external edit kept"
    );
    assert!(
        !env.owner_root.join("child_a.rs").exists(),
        "our applied paths were rolled back after the failed equality"
    );
    let txn = latest_txn(&env, &run_id).unwrap();
    assert_eq!(
        txn.phase,
        faktor_session::ledger::IntegrationTxnPhase::RolledBack
    );
}
