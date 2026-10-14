//! Production-wiring certification for the Engineering Outcome Ledger
//! producer (economic audit item 7 P1): the DAEMON'S `TaskExecutor` (built
//! by the production `build_daemon_core` path) records the terminal run's
//! ONE `engineering_outcome` row from durable facts at `settle_run`, for a
//! verified completion, a failure AND a cancellation — and a replayed
//! settlement is a no-op (the production-wiring x2 replay). No ledger or
//! task authority is hand-built: the graph's own session/store/executor
//! are the production ones.
//!
//! The P1 economic-truth contracts certified here:
//!
//! - the engineering record is immutable and money-free;
//! - the settlement lifecycle lands `complete` with measured amounts once
//!   the task's reservations are settled, and a `pending` initial event
//!   moves to `complete` when the executor's terminal settlement retries
//!   the conservative budget finalize (late settlement);
//! - BOTH producers (the agent-path `record_terminal_run` and the
//!   executor's `settle_run`) derive the row from the SAME durable
//!   terminal-run facts outbox, so every legal interleaving leaves ONE
//!   durable outcome with identical authoritative facts.

use std::path::Path;
use std::sync::Arc;

use faktor_agent::{
    session_terminal_run_id, FinalVerificationBasis, MeasuredMoney, MoneyState, OutcomeLedger,
    RunVerifiedSample, SettlementFinalize, TerminalRunFacts, TerminalRunFactsOutbox,
};
use faktor_core::id::{OpId, TaskId};
use faktor_core::model::RouterPhase;
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

/// Complete a seeded task into `VerifiedComplete` with one passing check.
fn complete_verified(graph: &harness::DaemonGraph, parent: SessionId, task_id: TaskId) {
    let handle = graph.session.get_session(parent).unwrap().unwrap();
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

fn terminal_facts() -> TerminalRunFacts {
    TerminalRunFacts {
        schema_version: faktor_agent::TERMINAL_FACTS_SCHEMA_V,
        risk_class: Some(faktor_core::model::RiskBucket::Low),
        samples: vec![RunVerifiedSample {
            provider: "provider".into(),
            model: "model".into(),
            phase: RouterPhase::Implement,
            verified_success: true,
            rework_cost_micro: 0,
            rework_turns: 0,
        }],
    }
}

/// A verified completion: the executor's terminal settlement appends the
/// ONE derived record (accepted/integrated/verified, deterministic basis,
/// proof coverage from the durable verification record), its settlement
/// lands `complete` with measured money and the unavailable counterfactual,
/// and the immediate replay (x2) neither duplicates nor rewrites anything.
#[tokio::test]
async fn verified_completion_records_once_and_replays_as_a_noop() {
    let dir = tempfile::tempdir().unwrap();
    let graph = seed_graph(dir.path());
    let (parent, run_id) = workspace_session(&graph, dir.path(), "verified");
    let task_id = seed_task(
        &graph,
        parent,
        &[
            TaskTransition::StartRunning,
            TaskTransition::RequestVerification,
            TaskTransition::StartVerification,
        ],
    );
    complete_verified(&graph, parent, task_id);

    settle(&graph, parent, &run_id).await;
    let ledger = OutcomeLedger::new(graph.session.store());
    assert_eq!(ledger.count().unwrap(), 1, "exactly one terminal record");
    let record = ledger.get(&run_id).unwrap().unwrap();
    assert_eq!(record.session_id, parent.raw());
    assert_eq!(record.task_id, task_id.raw());
    assert!(record.accepted && record.integrated && record.verified());
    assert!(record.verified_first_verification_pass && record.repair_attempts == 0);
    assert_eq!(record.proof.checks_passed, 1);
    assert_eq!(record.proof.checks_required, 1);
    assert_eq!(record.change_scale.files, 1);
    // The money is the settlement's dimension, not the engineering row.
    let settlement = ledger.settlement(&run_id).unwrap().unwrap();
    assert_eq!(settlement.money_state, MoneyState::Complete);
    assert_eq!(settlement.model_spend_micro, Some(0));
    assert_eq!(settlement.verification_spend_micro, Some(0));
    assert_eq!(
        settlement.avoided_hosted,
        MeasuredMoney::Unavailable,
        "no counterfactual authority exists in production: unavailable, never zero"
    );
    assert_eq!(
        settlement.reason,
        "every reservation of the run is settled and measured"
    );
    assert_eq!(settlement.seq, 0, "one settlement event, no late movement");
    // The production-wiring x2 replay: the same terminal settlement again
    // is a deterministic no-op with the identical durable row.
    settle(&graph, parent, &run_id).await;
    let replayed = ledger.get(&run_id).unwrap().unwrap();
    assert_eq!(replayed, record, "the replay never rewrites the record");
    assert_eq!(
        ledger.settlement(&run_id).unwrap().unwrap(),
        settlement,
        "the replay never appends a settlement event"
    );
    assert_eq!(ledger.count().unwrap(), 1);
}

/// Failures and cancellations are completed runs too: both append exactly
/// one record (never `accepted`/`verified`) with a `complete` settlement
/// (no reservations were ever opened), in the same store the native read
/// surface folds.
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
        let settlement = ledger.settlement(&run_id).unwrap().unwrap();
        assert_eq!(settlement.money_state, MoneyState::Complete);
        assert_eq!(settlement.model_spend_micro, Some(0));
        // The settlement replay stays a no-op for failures/cancellations.
        settle(&graph, parent, &run_id).await;
        assert_eq!(ledger.get(&run_id).unwrap().unwrap(), record);
        assert_eq!(
            ledger.settlement(&run_id).unwrap().unwrap(),
            settlement,
            "one settlement event per run"
        );
    }
    assert_eq!(ledger.count().unwrap(), 2, "one record per terminal run");
}

/// Late economic settlement: the agent-path producer records the row while
/// a reservation is still UNCERTAIN (the drive's conservative finalize
/// failed or never ran), typing the initial settlement `pending` with NULL
/// amounts; the executor's terminal settlement then retries the
/// conservative budget finalize (idempotent), and the projection MOVES to
/// `complete` — the immutable engineering row is untouched and VCSC now
/// includes the run.
#[tokio::test]
async fn late_finalization_moves_pending_to_complete() {
    let dir = tempfile::tempdir().unwrap();
    let graph = seed_graph(dir.path());
    let (parent, run_id) = workspace_session(&graph, dir.path(), "late");
    let handle = graph.session.get_session(parent).unwrap().unwrap();
    let task_id = seed_task(&graph, parent, &[TaskTransition::StartRunning]);
    // One dispatched reservation whose provider response never arrived
    // while the task ran: the attempt's spend is UNCERTAIN. (Reservations
    // are only legal while the task is Running.) The task then fails, and
    // the late "response lost" signal marks the reservation uncertain — the
    // exact P1 shape: a terminal run whose money is not yet settled.
    let op_id = OpId::new(1);
    let now = handle.now_ms();
    let faktor_store::CostReserveOutcome::Granted(reservation_id) = graph
        .session
        .store()
        .cost_reserve(parent, task_id, op_id, 123_456, now)
        .unwrap()
    else {
        panic!("the reservation must be granted");
    };
    graph
        .session
        .store()
        .cost_mark_dispatched(reservation_id, now + 1)
        .unwrap();
    let revision = handle.task_revision(task_id).unwrap();
    handle
        .transition_task(task_id, revision, TaskTransition::FailFromRunning, None)
        .unwrap();
    graph
        .session
        .store()
        .cost_mark_uncertain(reservation_id, "resp_lost", None, now + 2)
        .unwrap();
    // The agent-path producer records FIRST (its best-effort finalize did
    // not land): the initial settlement is `pending` with NULL amounts.
    let facts = terminal_facts();
    TerminalRunFactsOutbox::new(graph.session.store())
        .persist(&run_id, &facts)
        .unwrap();
    let ledger = OutcomeLedger::new(graph.session.store());
    assert_eq!(
        ledger.record_terminal_run(&handle, &run_id).unwrap(),
        Some(faktor_agent::LedgerAppend::Recorded)
    );
    let initial = ledger.settlement(&run_id).unwrap().unwrap();
    assert_eq!(initial.money_state, MoneyState::Pending);
    assert_eq!(initial.model_spend_micro, None);
    assert_eq!(initial.verification_spend_micro, None);
    assert_eq!(initial.rework_spend_micro, None);
    // The engineering record is already final and money-free.
    let record = ledger.get(&run_id).unwrap().unwrap();
    assert!(!record.accepted && !record.integrated && !record.verified());
    assert_eq!(record.verified_samples, facts.samples);

    // The executor's terminal settlement retries the conservative finalize
    // and moves the projection to `complete` at the reserved estimate.
    settle(&graph, parent, &run_id).await;
    let settled = ledger.settlement(&run_id).unwrap().unwrap();
    assert_eq!(settled.money_state, MoneyState::Complete);
    assert_eq!(settled.seq, 1, "a late finalization event moved the money");
    assert_eq!(settled.model_spend_micro, Some(123_456));
    assert_eq!(settled.rework_spend_micro, Some(0));
    let record_after = ledger.get(&run_id).unwrap().unwrap();
    assert_eq!(
        record_after, record,
        "the engineering row is immutable: late money never rewrites it"
    );
    // VCSC now includes the run: no money-incomplete exclusion remains.
    let report = ledger.metrics().unwrap();
    assert_eq!(report.metrics.money_complete_runs, 1);
    assert_eq!(report.metrics.money_incomplete_excluded, 0);
}

/// The dual-producer invariant: for every legal interleaving of the
/// agent-path producer (`record_terminal_run`, the runtime completion gate)
/// and the TaskExecutor-path producer (`settle_run`'s terminal settlement),
/// the final durable outcome is identical in authoritative facts. The
/// non-derivable terminal-turn facts (risk class + explicit verified
/// samples) are persisted FIRST into the durable outbox, so every producer
/// derives the SAME row; the store's exactly-once append freezes whichever
/// producer wins and the others observe a no-op.
#[tokio::test]
async fn dual_producers_converge_for_every_interleaving() {
    fn comparable(record: &faktor_agent::TerminalEngineeringOutcome) -> serde_json::Value {
        serde_json::json!({
            "task_class": record.task_class,
            "risk_class": record.risk_class,
            "candidate_snapshot": record.candidate_snapshot,
            "final_snapshot": record.final_snapshot,
            "accepted": record.accepted,
            "integrated": record.integrated,
            "verified": record.verified(),
            "verified_first_verification_pass": record.verified_first_verification_pass,
            "repair_attempts": record.repair_attempts,
            "model_sequence": record.model_sequence,
            "human_interventions": record.human_interventions,
            "proof": record.proof,
            "change_scale": record.change_scale,
            "rollback_events": record.rollback_events,
            "recovery_events": record.recovery_events,
            "final_basis": record.final_basis,
            "frontier_escalations": record.frontier_escalations,
            "verified_samples": record.verified_samples,
        })
    }

    async fn seed_ready(dir: &Path, name: &str) -> (harness::DaemonGraph, SessionId, String) {
        let graph = seed_graph(dir);
        let (parent, run_id) = workspace_session(&graph, dir, name);
        let task_id = seed_task(
            &graph,
            parent,
            &[
                TaskTransition::StartRunning,
                TaskTransition::RequestVerification,
                TaskTransition::StartVerification,
            ],
        );
        complete_verified(&graph, parent, task_id);
        TerminalRunFactsOutbox::new(graph.session.store())
            .persist(&run_id, &terminal_facts())
            .unwrap();
        (graph, parent, run_id)
    }

    // (a) Agent path wins the append; the executor's settlement replays.
    let dir_a = tempfile::tempdir().unwrap();
    let (graph, parent, run_id) = seed_ready(dir_a.path(), "agent-first").await;
    let handle = graph.session.get_session(parent).unwrap().unwrap();
    let ledger = OutcomeLedger::new(graph.session.store());
    assert_eq!(
        ledger.record_terminal_run(&handle, &run_id).unwrap(),
        Some(faktor_agent::LedgerAppend::Recorded)
    );
    settle(&graph, parent, &run_id).await;
    assert_eq!(ledger.count().unwrap(), 1);
    let agent_first = ledger.get(&run_id).unwrap().unwrap();
    assert_eq!(
        ledger.record_terminal_run(&handle, &run_id).unwrap(),
        None,
        "the executor's win leaves the agent path a no-op"
    );

    // (b) Executor path wins the append; the agent path replays.
    let dir_b = tempfile::tempdir().unwrap();
    let (graph, parent, run_id) = seed_ready(dir_b.path(), "executor-first").await;
    let handle = graph.session.get_session(parent).unwrap().unwrap();
    let ledger = OutcomeLedger::new(graph.session.store());
    settle(&graph, parent, &run_id).await;
    assert_eq!(ledger.count().unwrap(), 1);
    let executor_first = ledger.get(&run_id).unwrap().unwrap();
    assert_eq!(ledger.record_terminal_run(&handle, &run_id).unwrap(), None);

    // (c) Genuinely concurrent producers: the store's immediate transaction
    // picks exactly one winner and every loser observes a no-op.
    let dir_c = tempfile::tempdir().unwrap();
    let (graph, parent, run_id) = seed_ready(dir_c.path(), "concurrent").await;
    let ledger = OutcomeLedger::new(graph.session.store());
    let tasks = Arc::clone(&graph.tasks);
    let settle_run_id = run_id.clone();
    let settling = tokio::spawn(async move {
        tasks
            .settle_run(RunSettlement::InSession {
                parent,
                run_id: settle_run_id,
            })
            .await
            .expect("the concurrent settlement converges")
    });
    let raced = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for _ in 0..4 {
            let ledger = ledger.clone();
            let graph = &graph;
            let run_id = run_id.as_str();
            handles.push(scope.spawn(move || {
                let handle = graph.session.get_session(parent).unwrap().unwrap();
                ledger
                    .record_terminal_run(&handle, run_id)
                    .expect("a losing producer observes a replay, never an error")
            }));
        }
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    let _ = settling.await.unwrap();
    let recorded = raced
        .iter()
        .filter(|outcome| **outcome == Some(faktor_agent::LedgerAppend::Recorded))
        .count();
    assert!(
        recorded <= 1,
        "exactly one producer may append the row, observed {recorded}"
    );
    assert_eq!(ledger.count().unwrap(), 1);
    let concurrent = ledger.get(&run_id).unwrap().unwrap();

    // The invariant: all three interleavings left the SAME authoritative
    // engineering facts, and the terminal-run facts are exactly the durable
    // ones (risk class + samples) — nothing was producer-dependent.
    assert_eq!(comparable(&agent_first), comparable(&executor_first));
    assert_eq!(comparable(&agent_first), comparable(&concurrent));
    assert_eq!(concurrent.verified_samples, terminal_facts().samples);
    assert_eq!(concurrent.risk_class, faktor_core::model::RiskBucket::Low);
    // Every interleaving also leaves exactly ONE complete settlement event.
    let settlement = ledger.settlement(&run_id).unwrap().unwrap();
    assert_eq!(settlement.money_state, MoneyState::Complete);
    assert_eq!(settlement.seq, 0);
    assert_eq!(
        ledger
            .finalize_economic_settlement(&handle, &run_id)
            .unwrap(),
        SettlementFinalize::AlreadySettled
    );
    // End-to-end single feed: whichever producer won the race absorbed the
    // terminal-turn sample into the router registry EXACTLY once (the
    // store's atomic append path), and every loser's reconciliation resolved
    // coverage against that durable record.
    let stats = graph
        .session
        .store()
        .model_outcome_stats_get(
            "provider",
            "model",
            RouterPhase::Implement,
            faktor_core::model::TaskClass::Medium,
            faktor_core::model::RiskBucket::Low,
        )
        .unwrap()
        .expect("the terminal facts sample was absorbed by the winning append");
    assert_eq!(
        stats.sample_count, 1,
        "the shared terminal-turn sample is fed exactly once"
    );
}

/// The deterministic authoritative-facts projection of one engineering row
/// (identity and wall-clock columns excluded: twin runs legitimately differ
/// there).
fn authoritative_facts(record: &faktor_agent::TerminalEngineeringOutcome) -> serde_json::Value {
    serde_json::json!({
        "task_class": record.task_class,
        "risk_class": record.risk_class,
        "candidate_snapshot": record.candidate_snapshot,
        "final_snapshot": record.final_snapshot,
        "accepted": record.accepted,
        "integrated": record.integrated,
        "verified": record.verified(),
        "verified_first_verification_pass": record.verified_first_verification_pass,
        "repair_attempts": record.repair_attempts,
        "model_sequence": record.model_sequence,
        "human_interventions": record.human_interventions,
        "proof": record.proof,
        "change_scale": record.change_scale,
        "rollback_events": record.rollback_events,
        "recovery_events": record.recovery_events,
        "final_basis": record.final_basis,
        "frontier_escalations": record.frontier_escalations,
        "verified_samples": record.verified_samples,
    })
}

/// Dual-producer byte-identity with NON-ZERO reservations: a run whose
/// reservation settled to a measured non-zero spend BEFORE both producers
/// derive. Both orders (agent path first vs executor first) must leave the
/// ONE byte-identical engineering row and the identical `complete` initial
/// projection at seq 0 with the exact measured amount — the invariant the
/// zero-money interleaving test could not prove.
#[tokio::test]
async fn nonzero_settled_reservations_are_byte_identical_across_producer_orders() {
    async fn seeded(dir: &Path, name: &str) -> (harness::DaemonGraph, SessionId, String) {
        let graph = seed_graph(dir);
        let (parent, run_id) = workspace_session(&graph, dir, name);
        let handle = graph.session.get_session(parent).unwrap().unwrap();
        let task_id = seed_task(&graph, parent, &[TaskTransition::StartRunning]);
        let now = handle.now_ms();
        let faktor_store::CostReserveOutcome::Granted(reservation_id) = graph
            .session
            .store()
            .cost_reserve(parent, task_id, OpId::new(1), 123_456, now)
            .unwrap()
        else {
            panic!("the reservation must be granted");
        };
        graph
            .session
            .store()
            .cost_mark_dispatched(reservation_id, now + 1)
            .unwrap();
        graph
            .session
            .store()
            .cost_settle(reservation_id, 123_456, Some(123_456), None, None, now + 2)
            .unwrap();
        for transition in [
            TaskTransition::RequestVerification,
            TaskTransition::StartVerification,
        ] {
            let revision = handle.task_revision(task_id).unwrap();
            handle
                .transition_task(task_id, revision, transition, None)
                .unwrap();
        }
        complete_verified(&graph, parent, task_id);
        TerminalRunFactsOutbox::new(graph.session.store())
            .persist(&run_id, &terminal_facts())
            .unwrap();
        (graph, parent, run_id)
    }

    // (a) The agent path records first; the executor's settlement replays.
    let dir_a = tempfile::tempdir().unwrap();
    let (graph_a, parent_a, run_a) = seeded(dir_a.path(), "nonzero-agent-first").await;
    let handle_a = graph_a.session.get_session(parent_a).unwrap().unwrap();
    let ledger_a = OutcomeLedger::new(graph_a.session.store());
    assert_eq!(
        ledger_a.record_terminal_run(&handle_a, &run_a).unwrap(),
        Some(faktor_agent::LedgerAppend::Recorded)
    );
    settle(&graph_a, parent_a, &run_a).await;
    let record_a = ledger_a.get(&run_a).unwrap().unwrap();
    let settlement_a = ledger_a.settlement(&run_a).unwrap().unwrap();
    assert_eq!(settlement_a.money_state, MoneyState::Complete);
    assert_eq!(settlement_a.model_spend_micro, Some(123_456));
    assert_eq!(
        settlement_a.seq, 0,
        "the money was settled before the record"
    );
    assert_eq!(record_a.model_sequence.len(), 1, "{record_a:?}");

    // (b) The executor records first; the agent path replays.
    let dir_b = tempfile::tempdir().unwrap();
    let (graph_b, parent_b, run_b) = seeded(dir_b.path(), "nonzero-executor-first").await;
    let handle_b = graph_b.session.get_session(parent_b).unwrap().unwrap();
    let ledger_b = OutcomeLedger::new(graph_b.session.store());
    settle(&graph_b, parent_b, &run_b).await;
    assert_eq!(
        ledger_b.record_terminal_run(&handle_b, &run_b).unwrap(),
        None
    );
    let record_b = ledger_b.get(&run_b).unwrap().unwrap();
    let settlement_b = ledger_b.settlement(&run_b).unwrap().unwrap();
    assert_eq!(settlement_b.money_state, MoneyState::Complete);
    assert_eq!(settlement_b.model_spend_micro, Some(123_456));
    assert_eq!(settlement_b.seq, 0);
    assert_eq!(record_b.model_sequence.len(), 1, "{record_b:?}");

    // The measured non-zero money is the settlement's dimension, so both
    // orders agree byte-for-byte on the engineering facts AND the money.
    assert_eq!(
        authoritative_facts(&record_a),
        authoritative_facts(&record_b),
        "non-zero money must not make the producers diverge"
    );
    assert_eq!(record_a.model_sequence, record_b.model_sequence);
    assert_eq!(ledger_a.count().unwrap(), 1);
    assert_eq!(ledger_b.count().unwrap(), 1);
}

/// Sequential DIVERGENT producers: the agent path records the run while its
/// reservation is still UNCERTAIN (`pending`, no measured step); the
/// executor's later terminal settlement moves the money (`complete` at the
/// reserved estimate) and would now derive a DIFFERENT engineering row (a
/// measured step exists). The append-only identity must win: the frozen
/// record is never rewritten, the money repair lands as the NEXT settlement
/// event, and every later pass is a deterministic no-op.
#[tokio::test]
async fn sequential_divergent_producers_repair_money_without_rewriting_the_record() {
    let dir = tempfile::tempdir().unwrap();
    let graph = seed_graph(dir.path());
    let (parent, run_id) = workspace_session(&graph, dir.path(), "sequential-divergent");
    let handle = graph.session.get_session(parent).unwrap().unwrap();
    let task_id = seed_task(&graph, parent, &[TaskTransition::StartRunning]);
    let now = handle.now_ms();
    let faktor_store::CostReserveOutcome::Granted(reservation_id) = graph
        .session
        .store()
        .cost_reserve(parent, task_id, OpId::new(1), 321_000, now)
        .unwrap()
    else {
        panic!("the reservation must be granted");
    };
    graph
        .session
        .store()
        .cost_mark_dispatched(reservation_id, now + 1)
        .unwrap();
    let revision = handle.task_revision(task_id).unwrap();
    handle
        .transition_task(task_id, revision, TaskTransition::FailFromRunning, None)
        .unwrap();
    graph
        .session
        .store()
        .cost_mark_uncertain(reservation_id, "resp_lost", None, now + 2)
        .unwrap();
    TerminalRunFactsOutbox::new(graph.session.store())
        .persist(&run_id, &terminal_facts())
        .unwrap();
    let ledger = OutcomeLedger::new(graph.session.store());
    assert_eq!(
        ledger.record_terminal_run(&handle, &run_id).unwrap(),
        Some(faktor_agent::LedgerAppend::Recorded)
    );
    let frozen = ledger.get(&run_id).unwrap().unwrap();
    assert!(
        frozen.model_sequence.is_empty(),
        "no settled spend existed at record time: {frozen:?}"
    );
    assert_eq!(
        ledger.settlement(&run_id).unwrap().unwrap().money_state,
        MoneyState::Pending
    );

    // The executor's terminal settlement retries the conservative finalize
    // (the money MOVES) and re-derives a now-divergent projection; the
    // frozen record stays authoritative.
    settle(&graph, parent, &run_id).await;
    assert_eq!(ledger.count().unwrap(), 1);
    let after = ledger.get(&run_id).unwrap().unwrap();
    assert_eq!(
        after, frozen,
        "a divergent later producer never rewrites the frozen engineering row"
    );
    assert!(after.model_sequence.is_empty());
    let settled = ledger.settlement(&run_id).unwrap().unwrap();
    assert_eq!(settled.money_state, MoneyState::Complete);
    assert_eq!(settled.seq, 1, "the money repair is an append-only event");
    assert_eq!(settled.model_spend_micro, Some(321_000));
    assert!(
        !settled.steps.is_empty(),
        "the settled projection carries the measured step the frozen row cannot"
    );
    assert_eq!(ledger.record_terminal_run(&handle, &run_id).unwrap(), None);
    // A third pass converges: no record movement, no settlement movement.
    settle(&graph, parent, &run_id).await;
    assert_eq!(ledger.get(&run_id).unwrap().unwrap(), frozen);
    assert_eq!(ledger.settlement(&run_id).unwrap().unwrap(), settled);
    assert_eq!(ledger.count().unwrap(), 1);
}
