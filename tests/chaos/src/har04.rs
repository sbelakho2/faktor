//! HAR-04 / HAR-06 / HAR-08: deterministic interleaving coverage.
//!
//! ## Coverage table (honest)
//!
//! | scenario | actors / steps | schedules | invariant under test |
//! |---|---|---|---|
//! | double settle | settle × 2 on one reservation | 2 | exactly-one settle; `view.spent` equals the independently computed fixture price once |
//! | refund vs settle | settle / refund | 2 | exactly one winner; final status ∈ {settled, refunded}; money never counted twice |
//! | reserve vs settle | settle(r1) / reserve(r2) | 2 | `view.free = cap − spent − open` (hand arithmetic); rows well-formed |
//! | steering during turn settle | end_session / submit steer | 2 | accepted steer ⇒ durable row (never lost); refused steer ⇒ zero writes |
//! | queue push/pop | 2 pushes / 2 pops | 6 | conservation (pending = pushed − admitted), no double-claim, drain to empty |
//! | session switch during append | 2 appends / get+retitle, list | 6 | journals never bleed across sessions; seq monotone; switch target lands |
//! | crash-seam enumeration | one op × every named durability point | 6 seams | old-or-new world (no partial), row↔event coupling, replay reaches the uninterrupted reference |
//!
//! ## Honest coverage note
//!
//! Steps are whole external operations: the store's single durable writer
//! serializes them, so exhaustive order-preserving interleavings of the
//! step sequences ARE the observable execution set for these API pairs.
//! Interleavings INSIDE a single transaction cannot be stepped from
//! outside the production crates (which this test crate must not modify);
//! those are covered by the last scenario, which drives the store's
//! deterministic `CrashSeam` boundaries at every enumerated point of the
//! session-create and session-command durability classes and proves
//! old-or-new + coupling + replay-to-reference instead. Orchestrator
//! child-control steering (crates/session child.rs) is driven by that
//! crate's own in-crate probes, not here.
// TEST-SEPARATION: oracle=independent-reference isolation=isolated-data-dir

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

use faktor_core::event::{Event, EventKind};
use faktor_core::id::{OpId, TaskId};
use faktor_core::model::{ExecutionClass, MicroUsdPerMillionTokens, PriceQuote, PricingSnapshot};
use faktor_core::state::AgentState;
use faktor_session::{BudgetAuthority, DurableBudgetLedger, SessionHandle, SessionManager, Task};
use faktor_store::{CommandEvent, CrashArm, Store};

use crate::interleaving::{drive_schedule, interleavings};

// Independent price fixture: 1000 uncached-in + 500 out tokens at the
// fixture rates → (1000 × 15_000_000 + 500 × 60_000_000) / 1_000_000
// = 15_000 + 30_000 = 45_000 microUSD — computed HERE, never read back
// from the ledger.
const FIXTURE_SETTLED_MICRO: u64 = 45_000;
const FIXTURE_INPUT_TOKENS: u64 = 1_000;
const FIXTURE_OUTPUT_TOKENS: u64 = 500;
const FIXTURE_PREDICTED: u64 = 400;
const FIXTURE_CAP: u64 = 1_000_000;

fn known_snapshot() -> PricingSnapshot {
    PricingSnapshot::exact(
        PriceQuote {
            input: MicroUsdPerMillionTokens(15_000_000),
            output: MicroUsdPerMillionTokens(60_000_000),
            cache_read: MicroUsdPerMillionTokens(3_000_000),
            cache_write: MicroUsdPerMillionTokens(7_000_000),
        },
        7,
        "h4-fixture".into(),
    )
}

/// Fresh durable world per schedule: temp dir, store, session, task with
/// an explicit cap.
struct BudgetFixture {
    _temp: tempfile::TempDir,
    #[allow(dead_code)]
    manager: Arc<SessionManager>,
    ledger: Arc<DurableBudgetLedger>,
    session: SessionHandle,
    task: TaskId,
}

fn budget_fixture() -> BudgetFixture {
    let temp = tempfile::tempdir().expect("hermetic temp dir");
    let manager = SessionManager::open(temp.path().join("store"), temp.path().join("cas"), true)
        .expect("open store");
    let ws = manager.create_workspace("/w").expect("workspace");
    let session = manager
        .create_session(ws, "h4", "chaos-p", "m")
        .expect("session");
    let task_row = session
        .create_task(Task {
            task_id: session.task_id().expect("default task id"),
            session_id: session.id(),
            goal: "interleaving goal".into(),
            created_ms: 1,
            updated_ms: 1,
            ..Default::default()
        })
        .expect("task");
    let ledger = DurableBudgetLedger::new(manager.clone());
    ledger
        .set_task_max_cost(session.id(), task_row.task_id, Some(FIXTURE_CAP))
        .expect("cap");
    BudgetFixture {
        _temp: temp,
        manager,
        ledger,
        session,
        task: task_row.task_id,
    }
}

/// The settle step shared by the budget scenarios: a priced usage frame
/// at the fixture token counts.
async fn settle_step(fx: &BudgetFixture, reservation: faktor_session::ReservationId) -> bool {
    fx.ledger
        .settle_usage(
            fx.session.id(),
            reservation,
            FIXTURE_INPUT_TOKENS,
            0,
            0,
            FIXTURE_OUTPUT_TOKENS,
            None,
            None,
        )
        .await
        .is_ok()
}

// ---------------------------------------------------------------------------
// Budget: exactly-once settlement
// ---------------------------------------------------------------------------

/// Double-settle race on ONE reservation: both actors race the same
/// settle; exactly one may win and the durable view must show the
/// independently computed fixture price ONCE.
#[tokio::test]
async fn har04_double_settle_interleaving_settles_exactly_once() {
    let schedules = interleavings(&[1, 1]);
    assert_eq!(schedules.len(), 2, "both orders explored: {schedules:?}");
    for (index, schedule) in schedules.iter().enumerate() {
        let fx = budget_fixture();
        let reservation = fx
            .ledger
            .reserve(
                fx.session.id(),
                fx.task,
                OpId::new(10),
                FIXTURE_PREDICTED,
                Some(known_snapshot()),
                ExecutionClass::Unknown,
            )
            .await
            .expect("first reservation fits the cap");
        let outcomes: Vec<Option<u64>> = drive_schedule(schedule, |_actor, _step| {
            let fx = &fx;
            async move {
                match fx
                    .ledger
                    .settle_usage(
                        fx.session.id(),
                        reservation,
                        FIXTURE_INPUT_TOKENS,
                        0,
                        0,
                        FIXTURE_OUTPUT_TOKENS,
                        None,
                        None,
                    )
                    .await
                {
                    Ok(Some(micro)) => Some(micro),
                    Ok(None) => panic!("priced fixture must settle with an amount"),
                    Err(_) => None,
                }
            }
        })
        .await;
        let amounts: Vec<u64> = outcomes.iter().copied().flatten().collect();
        assert_eq!(
            amounts.len(),
            1,
            "schedule {index} {schedule:?}: exactly one settle may win ({outcomes:?})"
        );
        assert_eq!(
            outcomes.len() - amounts.len(),
            1,
            "the loser must get a typed refusal"
        );
        assert_eq!(
            amounts[0], FIXTURE_SETTLED_MICRO,
            "the settled amount must equal the independent fixture arithmetic"
        );
        let view = fx
            .ledger
            .session_budget_view(fx.session.id(), fx.task)
            .expect("view");
        assert_eq!(view.spent_cost_micro, FIXTURE_SETTLED_MICRO);
        assert_eq!(view.settled_count, 1, "exactly one settled row");
        assert_eq!(view.open_reservations, 0, "reservation closed");
    }
}

/// Refund vs settle race: exactly one winner; the final row is never a
/// blend of both (spent XOR refunded), free budget always re-derives.
#[tokio::test]
async fn har04_refund_vs_settle_interleaving_has_exactly_one_winner() {
    let schedules = interleavings(&[1, 1]);
    assert_eq!(schedules.len(), 2);
    for (index, schedule) in schedules.iter().enumerate() {
        let fx = budget_fixture();
        let reservation = fx
            .ledger
            .reserve(
                fx.session.id(),
                fx.task,
                OpId::new(11),
                FIXTURE_PREDICTED,
                Some(known_snapshot()),
                ExecutionClass::Unknown,
            )
            .await
            .expect("reservation");
        let outcomes: Vec<&str> = drive_schedule(schedule, |actor, _step| {
            let fx = &fx;
            async move {
                if actor == 0 {
                    if settle_step(fx, reservation).await {
                        "settle"
                    } else {
                        "settle-refused"
                    }
                } else if fx.ledger.refund(fx.session.id(), reservation).await.is_ok() {
                    "refund"
                } else {
                    "refund-refused"
                }
            }
        })
        .await;
        let winners = outcomes
            .iter()
            .filter(|o| **o == "settle" || **o == "refund")
            .count();
        assert_eq!(
            winners, 1,
            "schedule {index} {schedule:?}: settle/refund must be exactly-one-winner \
             ({outcomes:?})"
        );
        let view = fx
            .ledger
            .session_budget_view(fx.session.id(), fx.task)
            .expect("view");
        if outcomes.contains(&"settle") {
            assert_eq!(view.spent_cost_micro, FIXTURE_SETTLED_MICRO);
            assert_eq!(view.settled_count, 1);
            assert_eq!(view.free(), FIXTURE_CAP - FIXTURE_SETTLED_MICRO);
        } else {
            assert_eq!(view.spent_cost_micro, 0, "refund must never charge");
            assert_eq!(view.settled_count, 0);
            assert_eq!(view.free(), FIXTURE_CAP, "refund frees the full cap");
        }
        assert_eq!(view.open_reservations, 0, "one row, closed, never blended");
    }
}

/// Reserve vs settle: a new in-flight reservation racing an existing
/// settlement must leave the view exactly re-derivable in BOTH orders.
#[tokio::test]
async fn har04_reserve_vs_settle_interleaving_keeps_the_view_consistent() {
    let schedules = interleavings(&[1, 1]);
    assert_eq!(schedules.len(), 2);
    for (index, schedule) in schedules.iter().enumerate() {
        let fx = budget_fixture();
        let r1 = fx
            .ledger
            .reserve(
                fx.session.id(),
                fx.task,
                OpId::new(12),
                FIXTURE_PREDICTED,
                Some(known_snapshot()),
                ExecutionClass::Unknown,
            )
            .await
            .expect("r1");
        let outcomes: Vec<&str> = drive_schedule(schedule, |actor, _step| {
            let fx = &fx;
            async move {
                if actor == 0 {
                    if settle_step(fx, r1).await {
                        "settled"
                    } else {
                        "settle-refused"
                    }
                } else {
                    match fx
                        .ledger
                        .reserve(
                            fx.session.id(),
                            fx.task,
                            OpId::new(13),
                            FIXTURE_PREDICTED,
                            Some(known_snapshot()),
                            ExecutionClass::Unknown,
                        )
                        .await
                    {
                        Ok(_) => "reserved",
                        Err(_) => "reserve-refused",
                    }
                }
            }
        })
        .await;
        assert!(
            outcomes.contains(&"settled"),
            "schedule {index}: r1 must settle ({outcomes:?})"
        );
        assert!(
            outcomes.contains(&"reserved"),
            "schedule {index}: the cap fits both ({outcomes:?})"
        );
        let view = fx
            .ledger
            .session_budget_view(fx.session.id(), fx.task)
            .expect("view");
        assert_eq!(
            view.spent_cost_micro, FIXTURE_SETTLED_MICRO,
            "schedule {index}: settlement folded exactly once"
        );
        assert_eq!(view.open_reserved_micro, FIXTURE_PREDICTED, "r2 open");
        assert_eq!(view.open_reservations, 1, "one in-flight row");
        assert_eq!(view.settled_count, 1);
        assert_eq!(
            view.free(),
            FIXTURE_CAP - FIXTURE_SETTLED_MICRO - FIXTURE_PREDICTED,
            "schedule {index}: view must re-derive (hand arithmetic)"
        );
    }
}

// ---------------------------------------------------------------------------
// Steering during turn settle
// ---------------------------------------------------------------------------

/// Session fixture with a LIVE (streaming) first turn so further prompts
/// queue — the durable steering channel.
fn busy_session_fixture(title: &str) -> (tempfile::TempDir, Arc<SessionManager>, SessionHandle) {
    let temp = tempfile::tempdir().expect("temp");
    let manager = SessionManager::open(temp.path().join("store"), temp.path().join("cas"), true)
        .expect("open");
    let ws = manager.create_workspace("/w").expect("ws");
    let session = manager
        .create_session(ws, title, "chaos-p", "m")
        .expect("session");
    let first = session
        .submit_prompt("first prompt", &[])
        .expect("first prompt accepted");
    assert!(
        !first.queued,
        "the first prompt on an idle session is the active turn"
    );
    // Drive the turn into Streaming so the next submits are steers
    // (queued) rather than new active turns.
    session
        .append_event(
            EventKind::ContextPrepared,
            AgentState::BuildingContext,
            Some(first.op_id),
            None,
        )
        .expect("context prepared");
    session
        .append_event(
            EventKind::ModelStarted,
            AgentState::WaitingForModel,
            Some(first.op_id),
            None,
        )
        .expect("model started");
    session
        .append_event(
            EventKind::ModelChunkReceived,
            AgentState::Streaming,
            Some(first.op_id),
            None,
        )
        .expect("streaming");
    (temp, manager, session)
}

fn prompt_received_count(session: &SessionHandle) -> usize {
    session
        .events_range(1, None)
        .expect("events")
        .iter()
        .filter(|e| e.kind == EventKind::PromptReceived)
        .count()
}

/// A steer racing the TURN-settle. The settle program is the real
/// end-of-turn chain split into fixed, conditional steps (each step is
/// ONE API call, a no-op when its precondition is already satisfied —
/// legality, not scheduling, decides):
///
/// 1. abort the streaming turn → `ReadyForNextTurn`;
/// 2. suspend the session → `Suspended`;
/// 3. end the session if legal (terminal now);
/// 4. mark a reopened (Preparing) steer's turn failed;
/// 5. recover `FailedRecoverable → Idle`;
/// 6. end the session if legal.
///
/// The steer lands at one of seven interleaved points. Invariants at
/// every interleaving:
/// - an ACCEPTED steer is durable exactly once (PromptReceived journaled
///   exactly once; a QUEUED steer keeps its queue row through the rest of
///   the settle chain — never silently dropped);
/// - a REFUSED steer (after the session reached terminal) writes
///   NOTHING — no queue row, no journal entry.
#[tokio::test]
async fn har04_steering_during_turn_settle_never_loses_an_accepted_steer() {
    fn end_eligible(state: AgentState) -> bool {
        matches!(
            state,
            AgentState::Idle
                | AgentState::ReadyForNextTurn
                | AgentState::Suspended
                | AgentState::Validating
                | AgentState::UpdatingMemory
        )
    }

    let schedules = interleavings(&[6, 1]);
    assert_eq!(schedules.len(), 7, "{schedules:?}");
    for (index, schedule) in schedules.iter().enumerate() {
        let (_temp, _manager, session) = busy_session_fixture("h4-steer");
        let prompts_before = prompt_received_count(&session);
        let rows_before = session.queue_rows(16).expect("rows");
        assert_eq!(
            rows_before.len(),
            0,
            "the streaming turn has not parked any queued steer yet"
        );
        let turn_op = session
            .events_range(1, None)
            .expect("events")
            .iter()
            .find_map(|e| e.op_id)
            .expect("the fixture turn op id");

        let outcomes: Vec<Option<(OpId, bool)>> = drive_schedule(schedule, |actor, step| {
            let session = &session;
            async move {
                if actor == 0 {
                    // Conditional settle steps: legal by construction.
                    match step {
                        0 => {
                            if session.state().expect("state") == AgentState::Streaming {
                                session.abort(Some(turn_op)).expect("abort");
                            }
                        }
                        1 => {
                            let state = session.state().expect("state");
                            if !state.is_terminal() && state != AgentState::Suspended {
                                session.suspend().expect("suspend");
                            }
                        }
                        2 => {
                            if end_eligible(session.state().expect("state")) {
                                session.end_session().expect("end");
                            }
                        }
                        3 => {
                            if session.state().expect("state") == AgentState::Preparing {
                                session
                                    .mark_failed(false, "settle reopened steer turn")
                                    .expect("mark_failed");
                            }
                        }
                        4 => {
                            if session.state().expect("state") == AgentState::FailedRecoverable {
                                session.reset().expect("reset to idle");
                            }
                        }
                        5 => {
                            if end_eligible(session.state().expect("state")) {
                                session.end_session().expect("end");
                            }
                        }
                        other => panic!("unexpected settle step {other}"),
                    }
                    None
                } else {
                    match session.submit_prompt("steering prompt", &[]) {
                        Ok(receipt) => {
                            assert!(receipt.accepted, "accepted receipt");
                            Some((receipt.op_id, receipt.queued))
                        }
                        Err(_) => None,
                    }
                }
            }
        })
        .await;

        // The steer is actor 1's single step: its schedule position is
        // the only place its outcome lives.
        let steer_position = schedule
            .iter()
            .position(|(actor, _)| *actor == 1)
            .expect("steer step in schedule");
        let steered = outcomes[steer_position];
        let rows_after = session.queue_rows(64).expect("rows");
        let prompts_after = prompt_received_count(&session);
        match steered {
            None => {
                // The settle ran to terminal first: the steer was refused
                // and must have left ZERO durable trace.
                assert!(
                    session.state().expect("state").is_terminal(),
                    "schedule {index}: a refusal must come from the terminal session"
                );
                assert_eq!(
                    rows_after.len(),
                    0,
                    "schedule {index}: a refused steer must not write a queue row"
                );
                assert_eq!(
                    prompts_after, prompts_before,
                    "schedule {index}: a refused steer must not journal a prompt"
                );
            }
            Some((op, was_queued)) => {
                assert_eq!(
                    prompts_after,
                    prompts_before + 1,
                    "schedule {index}: an accepted steer journals exactly once"
                );
                if was_queued {
                    assert!(
                        rows_after.iter().any(|row| row.op_id == op),
                        "schedule {index}: accepted QUEUED steer {op} lost its durable \
                         row during the settle chain; rows={:?}",
                        rows_after.iter().map(|r| r.op_id).collect::<Vec<_>>()
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Queue push / pop
// ---------------------------------------------------------------------------

/// Push × 2 vs pop × 2 over the durable prompt queue, with the pop
/// actor settling the live turn first (admission only runs from an
/// eligible state — that IS the real system, and the interleavings
/// enumerate every ordering of settle/push/pop).
///
/// Invariants (order-agnostic):
/// - every QUEUED receipt has exactly one durable row (no lost push, no
///   phantom row);
/// - no pop ever claims the same op twice, and only queued ops are
///   claimable;
/// - `pending = queued_receipts − admitted` (admission re-statuses,
///   never deletes);
/// - when rows still wait after the drain pass, the session must be in a
///   NON-eligible state — rows are blocked, never stranded silently.
#[tokio::test]
async fn har04_queue_push_pop_interleaving_keeps_queue_conservation() {
    /// The admission-eligible states from the handle's queue contract.
    fn admit_eligible(state: AgentState) -> bool {
        matches!(
            state,
            AgentState::Idle
                | AgentState::ReadyForNextTurn
                | AgentState::Cancelled
                | AgentState::FailedRecoverable
        )
    }

    let schedules = interleavings(&[2, 2]);
    assert_eq!(schedules.len(), 6, "{schedules:?}");
    for (index, schedule) in schedules.iter().enumerate() {
        let (_temp, _manager, session) = busy_session_fixture("h4-queue");
        let seed = session
            .submit_prompt("seed queued prompt", &[])
            .expect("seed accepted");
        assert!(seed.queued, "streaming session parks prompts in queue");

        let mut queued_receipts: Vec<OpId> = vec![seed.op_id];
        let step_results: Vec<Option<(OpId, bool)>> = drive_schedule(schedule, |actor, step| {
            let session = &session;
            async move {
                match (actor, step) {
                    (0, _) => {
                        let receipt = session
                            .submit_prompt(&format!("push {step}"), &[])
                            .expect("push accepted");
                        Some((receipt.op_id, receipt.queued))
                    }
                    (1, 0) => {
                        // SETTLE the live turn so the queue can drain:
                        // a failed-recoverable turn stays promptable
                        // and is admission-eligible.
                        session
                            .mark_failed(false, "interleaving settle")
                            .expect("settle");
                        session
                            .admit_next_queued()
                            .expect("admit")
                            .map(|claim| (claim.op_id, true))
                    }
                    (1, 1) => session
                        .admit_next_queued()
                        .expect("admit")
                        .map(|claim| (claim.op_id, true)),
                    other => panic!("unexpected schedule step {other:?}"),
                }
            }
        })
        .await;

        let mut admitted: Vec<OpId> = Vec::new();
        for (position, outcome) in step_results.iter().enumerate() {
            match schedule[position].0 {
                0 => {
                    let (op, was_queued) = outcome.expect("push step yields its receipt");
                    if was_queued {
                        queued_receipts.push(op);
                    }
                }
                1 => {
                    if let Some((op, _)) = outcome {
                        admitted.push(*op);
                    }
                }
                _ => unreachable!("two actors"),
            }
        }

        // One durable row per QUEUED receipt (direct receipts never write
        // a row; admission re-statuses, never deletes).
        let rows = session.queue_rows(64).expect("rows");
        assert_eq!(
            rows.len(),
            queued_receipts.len(),
            "schedule {index} {schedule:?}: rows must equal queued receipts              (rows={}, queued={}, admitted={})",
            rows.len(),
            queued_receipts.len(),
            admitted.len()
        );
        // No double claim; only queued ops are claimable.
        let mut admitted_unique = admitted.clone();
        admitted_unique.sort_unstable();
        admitted_unique.dedup();
        assert_eq!(
            admitted_unique.len(),
            admitted.len(),
            "schedule {index}: no op may be claimed twice"
        );
        for op in &admitted {
            assert!(
                queued_receipts.contains(op),
                "schedule {index}: admitted {op} must be one of the queued receipts"
            );
        }
        let pending = rows.iter().filter(|row| row.status == "pending").count();
        assert_eq!(
            pending,
            queued_receipts.len() - admitted.len(),
            "schedule {index} {schedule:?}: pending = queued − admitted (conservation)"
        );

        // Drain pass: keep admitting until the queue refuses; a refusal
        // with rows still pending is only consistent when the state is
        // non-eligible (blocked, never stranded).
        let mut final_drains = 0;
        while session.admit_next_queued().expect("drain admit").is_some() {
            final_drains += 1;
            assert!(
                final_drains <= queued_receipts.len(),
                "schedule {index}: drain must terminate"
            );
        }
        let rows = session.queue_rows(64).expect("rows");
        let remaining_pending = rows.iter().filter(|row| row.status == "pending").count();
        if remaining_pending > 0 {
            let state = session.state().expect("state");
            assert!(
                !admit_eligible(state),
                "schedule {index}: {remaining_pending} rows still pending while the                  state {state:?} is admission-eligible — stranded queue"
            );
        } else {
            assert_eq!(
                admitted_unique.len() + final_drains,
                admitted.len() + final_drains,
                "sanity: drain only ever admits distinct rows"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Session switch during append
// ---------------------------------------------------------------------------

/// Appends to session A racing a switch (get + retitle of session B, then
/// the picker listing): journals never bleed across sessions, sequences
/// stay monotone, and the switch target lands its state exactly.
#[tokio::test]
async fn har04_session_switch_during_append_never_bleeds_journals() {
    let schedules = interleavings(&[2, 2]);
    assert_eq!(schedules.len(), 6);
    for (index, schedule) in schedules.iter().enumerate() {
        let temp = tempfile::tempdir().expect("temp");
        let manager =
            SessionManager::open(temp.path().join("store"), temp.path().join("cas"), true)
                .expect("open");
        let ws = manager.create_workspace("/w").expect("ws");
        let s1 = manager
            .create_session(ws, "append-target", "chaos-p", "m")
            .expect("s1");
        let s2 = manager
            .create_session(ws, "switch-target", "chaos-p", "m")
            .expect("s2");
        let s2_id = s2.id();
        let turn = s1.submit_prompt("append turn", &[]).expect("prompt");
        s1.append_event(
            EventKind::ContextPrepared,
            AgentState::BuildingContext,
            Some(turn.op_id),
            None,
        )
        .expect("prep");
        s1.append_event(
            EventKind::ModelStarted,
            AgentState::WaitingForModel,
            Some(turn.op_id),
            None,
        )
        .expect("started");
        let s1_title_before = s1.title().expect("s1 title");

        let step_results: Vec<String> = drive_schedule(schedule, |actor, step| {
            let s1 = &s1;
            let manager = &manager;
            async move {
                match (actor, step) {
                    (0, step) => {
                        s1.append_event(
                            EventKind::ModelChunkReceived,
                            AgentState::Streaming,
                            Some(turn.op_id),
                            Some(serde_json::json!({"text": step})),
                        )
                        .expect("append during switch");
                        format!("appended-{step}")
                    }
                    (1, 0) => {
                        // The SWITCH: open the other session and adopt it
                        // as the visible target.
                        let target = manager
                            .get_session(s2_id)
                            .expect("get switch target")
                            .expect("switch target exists");
                        target.update_session_title("switched").expect("retitle");
                        "switched".to_string()
                    }
                    (1, 1) => {
                        // The picker refresh DURING the append stream.
                        let listed = manager.list_sessions(None).expect("list");
                        assert!(
                            listed.iter().any(|h| h.id() == s2_id
                                && h.title().ok().as_deref() == Some("switched")),
                            "listing must observe the switch already applied"
                        );
                        "listed".to_string()
                    }
                    other => panic!("unexpected schedule step {other:?}"),
                }
            }
        })
        .await;
        assert_eq!(step_results.len(), 4, "all steps ran");

        // Invariants after EVERY interleaving.
        let s1_events = s1.events_range(1, None).expect("s1 events");
        let chunks: Vec<&Event> = s1_events
            .iter()
            .filter(|e| e.kind == EventKind::ModelChunkReceived)
            .collect();
        assert_eq!(
            chunks.len(),
            2,
            "schedule {index}: both appended chunks land exactly once, never duplicated"
        );
        let mut last_seq = 0u64;
        for event in &s1_events {
            assert!(
                event.seq.raw() > last_seq,
                "schedule {index}: journal sequence must be strictly monotone"
            );
            last_seq = event.seq.raw();
        }
        let s2_again = manager.get_session(s2_id).expect("reopen s2").expect("s2");
        let s2_events = s2_again.events_range(1, None).expect("s2 events");
        assert!(
            !s2_events
                .iter()
                .any(|e| e.kind == EventKind::ModelChunkReceived),
            "schedule {index}: appends must never bleed into the switch target"
        );
        assert_eq!(
            s2_again.title().expect("title"),
            "switched",
            "schedule {index}: the switch target state must land"
        );
        assert_eq!(
            s1.title().expect("s1 title"),
            s1_title_before,
            "schedule {index}: the append target's identity is untouched by the switch"
        );
        assert!(s1.id() != s2_id, "distinct sessions");
    }
}

// ---------------------------------------------------------------------------
// Crash-seam enumeration (honest fallback for intra-transaction yields)
// ---------------------------------------------------------------------------

const SESSION_SEAMS: &[&str] = &[
    "create_session_row",
    "create_session_precommit",
    "create_session_committed",
];
const COMMAND_SEAMS: &[&str] = &[
    "session_command_side_row",
    "session_command_precommit",
    "session_command_committed",
];

fn reopen(store_dir: &std::path::Path) -> Store {
    Store::open(store_dir, true).expect("reopen store")
}

/// SQL-level coupling counters: sessions vs their `session_created`
/// events, permission rows vs their `tool_requested` events.
fn coupling_counts(db_dir: &std::path::Path) -> ((usize, usize), (usize, usize)) {
    let conn = rusqlite::Connection::open_with_flags(
        db_dir.join("faktor-plus.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("read-only");
    let sessions: i64 = conn
        .query_row("SELECT COUNT(*) FROM session", [], |row| row.get(0))
        .expect("sessions");
    let created: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM event WHERE kind = 'session_created'",
            [],
            |row| row.get(0),
        )
        .expect("created events");
    let permissions: i64 = conn
        .query_row("SELECT COUNT(*) FROM permission", [], |row| row.get(0))
        .expect("permissions");
    let tool_events: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM event WHERE kind = 'tool_requested'",
            [],
            |row| row.get(0),
        )
        .expect("tool events");
    (
        (sessions as usize, created as usize),
        (permissions as usize, tool_events as usize),
    )
}

fn permission_event() -> CommandEvent {
    CommandEvent {
        kind: EventKind::ToolRequested,
        state: AgentState::WaitingForPermission,
        op_id: Some(OpId::new(1)),
        ts_ms: 1_000,
        payload: Some(serde_json::json!({"cmd": "true"})),
        payload_ver: 1,
    }
}

/// Crash at EVERY enumerated durability point of two real operation
/// classes; each crash must land old-or-new (never partial), keep the
/// row↔event coupling, and a replay of a missing op must reach the
/// uninterrupted reference world.
#[test]
fn har04_crash_seam_enumeration_old_or_new_then_replay() {
    // ---- reference worlds (never crashed), one per class -------------
    let reference_session_dir = tempfile::tempdir().expect("temp");
    {
        let store = Store::open(reference_session_dir.path().join("store"), true).expect("open");
        let ws = store.create_workspace("/w").expect("ws");
        store
            .create_session(ws, "seam", "p", "m")
            .expect("ref session");
    }
    let reference_command_dir = tempfile::tempdir().expect("temp");
    {
        let store = Store::open(reference_command_dir.path().join("store"), true).expect("open");
        let ws = store.create_workspace("/w").expect("ws");
        let row = store.create_session(ws, "seam", "p", "m").expect("ref");
        store
            .insert_permission_and_event(
                row.id,
                OpId::new(1),
                "shell.run",
                AgentState::Idle,
                permission_event(),
            )
            .expect("ref permission");
    }
    let ((reference_sessions, reference_created), _) =
        coupling_counts(&reference_session_dir.path().join("store"));
    let (_, (reference_permits, reference_tool_events)) =
        coupling_counts(&reference_command_dir.path().join("store"));
    assert_eq!(reference_sessions, reference_created);
    assert_eq!(reference_permits, reference_tool_events);

    // ---- session-create seams ----------------------------------------
    for point in SESSION_SEAMS {
        let temp = tempfile::tempdir().expect("temp");
        let store_dir = temp.path().join("store");
        let store = Store::open(&store_dir, true).expect("open");
        let ws = store.create_workspace("/w").expect("ws");
        store.crash_arm(CrashArm { point, ordinal: 0 });
        let caught = catch_unwind(AssertUnwindSafe(|| {
            store.create_session(ws, "seam", "p", "m")
        }));
        assert!(
            store.seam_crash_observed(&caught),
            "seam {point} never fired — a boundary that did not fire cannot be \
             mistaken for a clean run"
        );
        drop(store);
        let (before_replay, _) = coupling_counts(&store_dir);
        assert!(
            before_replay.0 == 0 || before_replay.0 == 1,
            "seam {point}: world must be OLD or NEW, got {} sessions",
            before_replay.0
        );
        assert_eq!(
            before_replay.0, before_replay.1,
            "seam {point}: row↔event coupling must hold even mid-crash \
             ({} rows vs {} events)",
            before_replay.0, before_replay.1
        );
        if before_replay.0 == 0 {
            // Old world: replay must complete exactly once.
            let reopened = reopen(&store_dir);
            let replayed = catch_unwind(AssertUnwindSafe(|| {
                reopened.create_session(ws, "seam", "p", "m")
            }));
            assert!(
                replayed.is_ok(),
                "seam {point}: replay must not crash the reopened store"
            );
        }
        let (final_counts, _) = coupling_counts(&store_dir);
        assert_eq!(
            final_counts,
            (reference_sessions, reference_created),
            "seam {point}: crashed world + replay must equal the reference"
        );
    }

    // ---- session-command seams ---------------------------------------
    for point in COMMAND_SEAMS {
        let temp = tempfile::tempdir().expect("temp");
        let store_dir = temp.path().join("store");
        let store = Store::open(&store_dir, true).expect("open");
        let ws = store.create_workspace("/w").expect("ws");
        let row = store.create_session(ws, "seam", "p", "m").expect("session");
        store.crash_arm(CrashArm { point, ordinal: 0 });
        let caught = catch_unwind(AssertUnwindSafe(|| {
            store.insert_permission_and_event(
                row.id,
                OpId::new(1),
                "shell.run",
                AgentState::Idle,
                permission_event(),
            )
        }));
        assert!(
            store.seam_crash_observed(&caught),
            "seam {point} never fired"
        );
        drop(store);
        let (_, before_replay) = coupling_counts(&store_dir);
        assert_eq!(
            before_replay.0, before_replay.1,
            "seam {point}: permission row and ToolRequested event are coupled — \
             never one without the other ({} vs {})",
            before_replay.0, before_replay.1
        );
        assert!(
            before_replay.0 <= 1,
            "seam {point}: old-or-new, got {}",
            before_replay.0
        );
        if before_replay.0 == 0 {
            let reopened = reopen(&store_dir);
            let replayed = catch_unwind(AssertUnwindSafe(|| {
                reopened.insert_permission_and_event(
                    row.id,
                    OpId::new(1),
                    "shell.run",
                    AgentState::Idle,
                    permission_event(),
                )
            }));
            assert!(
                replayed.is_ok(),
                "seam {point}: replay must not crash the reopened store"
            );
        }
        let (_, final_counts) = coupling_counts(&store_dir);
        assert_eq!(
            final_counts,
            (reference_permits, reference_tool_events),
            "seam {point}: crashed world + replay must equal the reference"
        );
    }
}
