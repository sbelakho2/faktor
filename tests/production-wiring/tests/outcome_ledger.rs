//! Production-wiring certification for the Engineering Outcome Ledger
//! producer (economic audit item 7 P1): the DAEMON'S `TaskExecutor` (built
//! by the production `build_daemon_core` path) records the terminal run's
//! ONE `engineering_outcome` row from durable facts at `settle_run`, for a
//! verified completion, a failure AND a cancellation — and a replayed
//! settlement is a no-op (the production-wiring x2 replay). No ledger or
//! task authority is hand-built: the graph's own session/store/executor
//! are the production ones.

use std::path::Path;

use faktor_agent::{session_terminal_run_id, FinalVerificationBasis, OutcomeLedger};
use faktor_core::id::TaskId;
use faktor_core::state::{
    CheckExecution, FileStateEvidence, TaskState, TaskTransition, VerificationStatus,
};
use faktor_core::SessionId;
use faktor_orchestrator::runtime::task_executor::{RunSettlement, SettlementOutcome};
use faktor_session::{Task, TaskBudget};
use faktor_tests_production_wiring::harness::{self, Config};

fn seed_graph(dir: &Path) -> harness::DaemonGraph {
    harness::build_production_graph(&dir.join("data"), Config::default())
        .expect("production daemon graph")
}

fn seed_task(
    graph: &harness::DaemonGraph,
    parent: SessionId,
    transitions: &[TaskTransition],
) -> TaskId {
    let handle = graph.session.get_session(parent).unwrap().unwrap();
    let task_id = handle.task_id().unwrap();
    let now = handle.now_ms();
    handle
        .create_task(Task {
            task_id,
            session_id: parent,
            goal: "outcome wiring".into(),
            acceptance_criteria: vec![],
            plan: vec![],
            attachments: Vec::new(),
            budget: TaskBudget::default(),
            contract: Default::default(),
            state: TaskState::Pending,
            created_ms: now,
            updated_ms: now,
        })
        .unwrap();
    for transition in transitions {
        let revision = handle.task_revision(task_id).unwrap();
        handle
            .transition_task(task_id, revision, *transition, None)
            .unwrap();
    }
    task_id
}

async fn settle(
    graph: &harness::DaemonGraph,
    parent: SessionId,
    run_id: &str,
) -> SettlementOutcome {
    graph
        .tasks
        .settle_run(RunSettlement::InSession {
            parent,
            run_id: run_id.to_string(),
        })
        .await
        .expect("the production settlement converges")
}

fn workspace_session(graph: &harness::DaemonGraph, dir: &Path, name: &str) -> (SessionId, String) {
    let root = dir.join(format!("ws-{name}"));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("README.md"), "owner\n").unwrap();
    let workspace = graph
        .session
        .create_workspace(root.to_str().unwrap())
        .unwrap();
    let handle = graph
        .session
        .create_session(workspace, name, "provider", "model")
        .unwrap();
    graph.session.ensure_owner_worktree(handle.id()).unwrap();
    (handle.id(), session_terminal_run_id(handle.id()))
}

/// A verified completion: the executor's terminal settlement appends the
/// ONE derived record (accepted/integrated/verified, deterministic basis,
/// proof coverage from the durable verification record), and the immediate
/// replay (x2) neither duplicates nor rewrites it.
#[tokio::test]
async fn verified_completion_records_once_and_replays_as_a_noop() {
    let dir = tempfile::tempdir().unwrap();
    let graph = seed_graph(dir.path());
    let (parent, run_id) = workspace_session(&graph, dir.path(), "verified");
    let handle = graph.session.get_session(parent).unwrap().unwrap();
    let task_id = seed_task(
        &graph,
        parent,
        &[
            TaskTransition::StartRunning,
            TaskTransition::RequestVerification,
            TaskTransition::StartVerification,
        ],
    );
    let now = handle.now_ms();
    handle
        .create_verification_record(
            task_id,
            None,
            vec![],
            vec![CheckExecution {
                check: "cargo check".into(),
                program: "cargo".into(),
                args: vec!["check".into()],
                category: "build".into(),
                required: true,
                status: VerificationStatus::Passed,
                started_ms: now,
                finished_ms: Some(now + 1),
                exit: Some(0),
                summary: None,
            }],
            vec![FileStateEvidence {
                path: "README.md".into(),
                digest_hex: "0".repeat(64),
                size: 6,
            }],
            vec![],
            None,
            VerificationStatus::Passed,
            now,
        )
        .unwrap();
    let record_id = handle
        .list_verification_records(task_id)
        .unwrap()
        .last()
        .unwrap()
        .record_id;
    let revision = handle.task_revision(task_id).unwrap();
    handle
        .complete_verified_task(task_id, revision, record_id)
        .unwrap();

    settle(&graph, parent, &run_id).await;
    let ledger = OutcomeLedger::new(graph.session.store());
    assert_eq!(ledger.count().unwrap(), 1, "exactly one terminal record");
    let record = ledger.get(&run_id).unwrap().unwrap();
    assert_eq!(record.session_id, parent.raw());
    assert_eq!(record.task_id, task_id.raw());
    assert!(record.accepted && record.integrated && record.verified());
    assert!(record.verified_first_attempt && record.repair_attempts == 0);
    assert_eq!(record.proof.checks_passed, 1);
    assert_eq!(record.proof.checks_required, 1);
    assert_eq!(record.change_scale.files, 1);
    // The production-wiring x2 replay: the same terminal settlement again
    // is a deterministic no-op with the identical durable row.
    settle(&graph, parent, &run_id).await;
    let replayed = ledger.get(&run_id).unwrap().unwrap();
    assert_eq!(replayed, record, "the replay never rewrites the record");
    assert_eq!(ledger.count().unwrap(), 1);
}

/// Failures and cancellations are completed runs too: both append exactly
/// one record (never `accepted`/`verified`), in the same store the native
/// read surface folds.
#[tokio::test]
async fn failure_and_cancellation_each_record_once() {
    let dir = tempfile::tempdir().unwrap();
    let graph = seed_graph(dir.path());
    let ledger = OutcomeLedger::new(graph.session.store());

    for (name, transitions, expected) in [
        (
            "failed",
            vec![
                TaskTransition::StartRunning,
                TaskTransition::FailFromRunning,
            ],
            TaskState::Failed,
        ),
        (
            "cancelled",
            vec![TaskTransition::StartRunning, TaskTransition::Cancel],
            TaskState::Cancelled,
        ),
    ] {
        let (parent, run_id) = workspace_session(&graph, dir.path(), name);
        let handle = graph.session.get_session(parent).unwrap().unwrap();
        let task_id = seed_task(&graph, parent, &transitions);
        assert_eq!(handle.get_task(task_id).unwrap().unwrap().state, expected);
        settle(&graph, parent, &run_id).await;
        let record = ledger.get(&run_id).unwrap().unwrap();
        assert!(!record.accepted && !record.integrated, "{record:?}");
        assert!(!record.verified(), "{record:?}");
        assert_eq!(record.final_basis, FinalVerificationBasis::Unverified);
        // The settlement replay stays a no-op for failures/cancellations.
        settle(&graph, parent, &run_id).await;
        assert_eq!(ledger.get(&run_id).unwrap().unwrap(), record);
    }
    assert_eq!(ledger.count().unwrap(), 2, "one record per terminal run");
}
