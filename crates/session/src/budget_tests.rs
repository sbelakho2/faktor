// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
use super::*;
use crate::handle::tests::{session, test_manager};
use faktor_core::state::TaskState;

/// The read API returns `Result`; the tests that assert the durable
/// picture unwrap it, while the dedicated failure tests below assert the
/// error explicitly.
trait TestView {
    fn view(&self, session: SessionId, task: TaskId) -> BudgetView;
}

impl TestView for DurableBudgetLedger {
    fn view(&self, session: SessionId, task: TaskId) -> BudgetView {
        BudgetAuthority::session_budget_view(self, session, task).expect("durable budget view")
    }
}

impl TestView for NoopBudget {
    fn view(&self, session: SessionId, task: TaskId) -> BudgetView {
        BudgetAuthority::session_budget_view(self, session, task).expect("noop budget view")
    }
}

/// The audit's settlement-truth identity: $15/1M in + $60/1M out ==
/// 15/60 microUSD per token (microUSD-per-million-token quote lines,
/// exactly the price lines the router freezes today).
fn known_snapshot() -> PricingSnapshot {
    use faktor_core::model::{MicroUsdPerMillionTokens, PriceQuote};
    PricingSnapshot::exact(
        PriceQuote {
            input: MicroUsdPerMillionTokens(15_000_000),
            output: MicroUsdPerMillionTokens(60_000_000),
            cache_read: MicroUsdPerMillionTokens(3_000_000),
            cache_write: MicroUsdPerMillionTokens(7_000_000),
        },
        7,
        "budget-test".into(),
    )
}

fn seeded_task(s: &crate::SessionHandle, max_cost: Option<u64>) -> TaskId {
    let task = crate::Task {
        task_id: s.task_id().unwrap(),
        session_id: s.id,
        goal: "budgeted goal".into(),
        created_ms: 1,
        updated_ms: 1,
        ..Default::default()
    };
    let created = s.create_task(task).unwrap();
    let ledger = DurableBudgetLedger::new(s.manager.clone());
    ledger
        .set_task_max_cost(s.id, created.task_id, max_cost)
        .unwrap();
    created.task_id
}

fn fresh_ledger() -> (
    tempfile::TempDir,
    Arc<SessionManager>,
    Arc<DurableBudgetLedger>,
) {
    let (_d, m) = test_manager();
    let ledger = DurableBudgetLedger::new(m.clone());
    (_d, m, ledger)
}

#[tokio::test]
async fn reserve_over_budget_is_typed_and_writes_nothing() {
    let (_d, m, ledger) = fresh_ledger();
    let s = session(&m);
    let task = seeded_task(&s, Some(100));
    let err = ledger
        .reserve(
            s.id,
            task,
            OpId::new(1),
            101,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap_err();
    assert_eq!(
        err,
        BudgetError::BudgetExceeded {
            free: 100,
            predicted: 101
        }
    );
    // No row, no spend: a refusal never leaves a trace.
    assert!(ledger.reservations_of(s.id, task, 10).unwrap().is_empty());
    assert_eq!(ledger.view(s.id, task).spent_cost_micro, 0);
    // Unlimited (cap cleared) grants anything.
    ledger.set_task_max_cost(s.id, task, None).unwrap();
    let granted = ledger
        .reserve(
            s.id,
            task,
            OpId::new(1),
            u64::MAX / 2,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    assert!(granted.raw() > 0);
    ledger.refund(s.id, granted).await.unwrap();
}

#[tokio::test]
async fn settle_usage_updates_spent_exactly_once_and_refuses_a_second_settle() {
    let (_d, m, ledger) = fresh_ledger();
    let s = session(&m);
    // Cap 2_000_000 microUSD ($2): the exactness example settles 1_620_000
    // within it and still leaves a tested refusal band.
    let task = seeded_task(&s, Some(2_000_000));
    let r1 = ledger
        .reserve(
            s.id,
            task,
            OpId::new(10),
            400,
            Some(known_snapshot()),
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    let r2 = ledger
        .reserve(
            s.id,
            task,
            OpId::new(11),
            400,
            Some(known_snapshot()),
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    // No provider report: categories x the stored snapshot settle the
    // local actual (100k uncached in @15 + 2k out @60 == 1_500_000 +
    // 120_000 == 1_620_000 microUSD == $1.62 — exact multiply-add).
    let actual = ledger
        .settle_usage(s.id, r1, 100_000, 0, 0, 2_000, None, None)
        .await
        .unwrap();
    assert_eq!(actual, Some(1_620_000), "exact category x snapshot math");
    let view = ledger.view(s.id, task);
    assert_eq!(view.spent_cost_micro, 1_620_000);
    assert_eq!(view.open_reservations, 1);
    assert_eq!(view.open_reserved_micro, 400);
    // Double settle: typed NotOpen, nothing changes.
    let err = ledger
        .settle_usage(s.id, r1, 0, 0, 0, 1, None, None)
        .await
        .unwrap_err();
    assert!(matches!(err, BudgetError::NotOpen { .. }));
    assert_eq!(ledger.view(s.id, task).spent_cost_micro, 1_620_000);
    // Reserve over the remaining cap is refused (free = 2_000_000 -
    // 1_620_000 - 400 = 379_600).
    let err = ledger
        .reserve(
            s.id,
            task,
            OpId::new(12),
            400_000,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap_err();
    assert_eq!(
        err,
        BudgetError::BudgetExceeded {
            free: 379_600,
            predicted: 400_000
        }
    );
    ledger
        .settle_usage(s.id, r2, 0, 0, 0, 0, Some(400), None)
        .await
        .unwrap();
    assert_eq!(ledger.view(s.id, task).spent_cost_micro, 1_620_400);
}

#[tokio::test]
async fn provider_under_report_never_undercuts_local_usage_and_both_columns_are_recorded() {
    let (_d, m, ledger) = fresh_ledger();
    let s = session(&m);
    let task = seeded_task(&s, Some(50_000));
    let r = ledger
        .reserve(
            s.id,
            task,
            OpId::new(20),
            100,
            Some(known_snapshot()),
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    // Locally calculated: 100k x 15 + 2k x 60 == 1_620_000 microUSD
    // ($1.62). The provider reports 9_999: a provider report can raise an
    // invoice but never lower it below its own token categories priced at
    // the frozen snapshot (a spoofed under-report must not make a paid call
    // free).
    let chosen = ledger
        .settle_usage(
            s.id,
            r,
            100_000,
            0,
            0,
            2_000,
            Some(9_999),
            Some(r#"{"provider":"p","model":"m"}"#.into()),
        )
        .await
        .unwrap();
    assert_eq!(chosen, Some(1_620_000));
    let rows = ledger.reservations_of(s.id, task, 10).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "settled");
    assert_eq!(rows[0].provider_cost_micro, Some(1_620_000));
    assert_eq!(rows[0].provider_reported_micro, Some(9_999));
    assert_eq!(rows[0].pricing_snapshot, Some(known_snapshot()));
    assert_eq!(
        rows[0].route_decision_json.as_deref(),
        Some(r#"{"provider":"p","model":"m"}"#)
    );
    // The CHOSEN actual is the local token-priced floor.
    assert_eq!(
        ledger.view(s.id, task).spent_cost_micro,
        1_620_000,
        "a provider under-report cannot undercut its own priced token usage"
    );
}

#[tokio::test]
async fn cache_lines_bill_at_their_own_snapshot_lines() {
    let (_d, m, ledger) = fresh_ledger();
    let s = session(&m);
    let task = seeded_task(&s, Some(50_000));
    let r = ledger
        .reserve(
            s.id,
            task,
            OpId::new(21),
            100,
            Some(known_snapshot()),
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    // 10k cache reads @3 + 4k cache writes @7 + 2k out @60 == 30_000 +
    // 28_000 + 120_000 == 178_000 microUSD; no provider report.
    let chosen = ledger
        .settle_usage(s.id, r, 0, 10_000, 4_000, 2_000, None, None)
        .await
        .unwrap();
    assert_eq!(chosen, Some(178_000));
    assert_eq!(ledger.view(s.id, task).spent_cost_micro, 178_000);
}

#[tokio::test]
async fn price_less_settle_under_a_cap_is_a_typed_refusal_that_writes_nothing() {
    // Unknown price source (or no snapshot at all) + no provider cost +
    // a hard budget: typed UnknownPrice, NOTHING written — the row stays
    // OPEN and keeps consuming (recovery/finalize resolve it).
    let (_d, m, ledger) = fresh_ledger();
    let s = session(&m);
    let task = seeded_task(&s, Some(1_000));
    // No snapshot was persisted at reserve (passthrough route).
    let r = ledger
        .reserve(
            s.id,
            task,
            OpId::new(30),
            600,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    let err = ledger
        .settle_usage(s.id, r, 1_000, 0, 0, 2_000, None, None)
        .await
        .unwrap_err();
    assert_eq!(
        err,
        BudgetError::UnknownPrice {
            reservation: r.raw()
        }
    );
    // Nothing was written: the row is still RESERVED, still consuming.
    let rows = ledger.reservations_of(s.id, task, 10).unwrap();
    assert_eq!(rows[0].status, "reserved");
    assert_eq!(rows[0].provider_cost_micro, None);
    let view = ledger.view(s.id, task);
    assert_eq!(view.spent_cost_micro, 0);
    assert_eq!(view.open_reservations, 1);
    assert_eq!(view.open_reserved_micro, 600);
    // An Unknown-SOURCE snapshot behaves identically (never a number).
    let r2 = ledger
        .reserve(
            s.id,
            task,
            OpId::new(31),
            100,
            Some(PricingSnapshot::unknown(1, "budget-test-unknown".into())),
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    let err = ledger
        .settle_usage(s.id, r2, 1_000, 0, 0, 2_000, None, None)
        .await
        .unwrap_err();
    assert_eq!(
        err,
        BudgetError::UnknownPrice {
            reservation: r2.raw()
        }
    );
    ledger.refund(s.id, r).await.unwrap();
    ledger.refund(s.id, r2).await.unwrap();
}

#[tokio::test]
async fn price_less_settle_without_a_cap_records_an_unknown_spend_never_a_number() {
    // No price authority + no provider cost + NO hard cap: the row
    // settles as a documented Unknown spend — amount columns NULL,
    // nothing folded into the task total, never a fabricated 0 or 1.
    let (_d, m, ledger) = fresh_ledger();
    let s = session(&m);
    let task = seeded_task(&s, None);
    let r = ledger
        .reserve(
            s.id,
            task,
            OpId::new(40),
            100,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    let chosen = ledger
        .settle_usage(s.id, r, 1_000, 0, 0, 2_000, None, None)
        .await
        .unwrap();
    assert_eq!(chosen, None, "Ok(None) = unknown spend, nothing folded");
    let view = ledger.view(s.id, task);
    assert_eq!(view.spent_cost_micro, 0, "nothing was folded");
    let rows = ledger.reservations_of(s.id, task, 10).unwrap();
    assert_eq!(rows[0].status, "settled");
    assert_eq!(rows[0].provider_cost_micro, None);
    assert_eq!(rows[0].provider_reported_micro, None);
}

#[tokio::test]
async fn refund_is_exactly_once_and_releases_without_spending() {
    let (_d, m, ledger) = fresh_ledger();
    let s = session(&m);
    let task = seeded_task(&s, Some(100));
    let r = ledger
        .reserve(
            s.id,
            task,
            OpId::new(30),
            60,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    ledger.refund(s.id, r).await.unwrap();
    assert_eq!(ledger.view(s.id, task).spent_cost_micro, 0);
    // A refunded reservation refuses settle AND a second refund.
    assert!(matches!(
        ledger
            .settle_usage(s.id, r, 1, 0, 0, 0, None, None)
            .await
            .unwrap_err(),
        BudgetError::NotOpen { .. }
    ));
    assert!(matches!(
        ledger.refund(s.id, r).await.unwrap_err(),
        BudgetError::NotOpen { .. }
    ));
    // Space is available again.
    let again = ledger
        .reserve(
            s.id,
            task,
            OpId::new(31),
            90,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    ledger.refund(s.id, again).await.unwrap();
}

#[tokio::test]
async fn settle_overshoot_is_recorded_honestly_and_the_next_reserve_refuses() {
    let (_d, m, ledger) = fresh_ledger();
    let s = session(&m);
    let task = seeded_task(&s, Some(100));
    // Reserve 60 (fits), provider reports 150: settlement is NOT
    // refused — the money was spent — the spent total honestly exceeds
    // the cap and the NEXT reserve refuses.
    let r = ledger
        .reserve(
            s.id,
            task,
            OpId::new(40),
            60,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    ledger
        .settle_usage(s.id, r, 0, 0, 0, 0, Some(150), None)
        .await
        .unwrap();
    let view = ledger.view(s.id, task);
    assert_eq!(
        view.spent_cost_micro, 150,
        "overspend is recorded, not hidden"
    );
    let err = ledger
        .reserve(
            s.id,
            task,
            OpId::new(41),
            1,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap_err();
    assert_eq!(
        err,
        BudgetError::BudgetExceeded {
            free: 0,
            predicted: 1
        }
    );
}

#[tokio::test]
async fn crash_before_the_dispatch_marker_is_refunded_and_free_is_restored() {
    // Crash BEFORE the provider request was sent (the durable marker
    // was never written): recovery REFUNDS the open reservation — the
    // provider was provably never contacted — and the free budget is
    // fully restored. Idempotent.
    let dir = tempfile::tempdir().unwrap();
    let (sid, task);
    {
        let m = crate::SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
            .unwrap();
        let s = session(&m);
        let ledger = DurableBudgetLedger::new(m.clone());
        task = seeded_task(&s, Some(1_000));
        sid = s.id;
        // Reserved but NEVER marked dispatched: the crash hit before
        // the marker (the runtime writes it immediately before the
        // provider transport call).
        let open = ledger
            .reserve(
                sid,
                task,
                OpId::new(50),
                200,
                None,
                faktor_core::model::ExecutionClass::Unknown,
            )
            .await
            .unwrap();
        assert!(open.raw() > 0);
    }
    let m2 = crate::SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
        .unwrap();
    let ledger2 = DurableBudgetLedger::new(m2.clone());
    let view_before = ledger2.view(sid, task);
    assert_eq!(view_before.open_reservations, 1);
    ledger2.recover_after_restart();
    let view = ledger2.view(sid, task);
    assert_eq!(view.open_reservations, 0, "no OPEN rows survive recovery");
    assert_eq!(view.open_reserved_micro, 0);
    assert_eq!(
        view.uncertain_reservations, 0,
        "pre-marker rows are REFUNDED, never UNCERTAIN"
    );
    assert_eq!(view.spent_cost_micro, 0);
    let rows = ledger2.reservations_of(sid, task, 10).unwrap();
    assert_eq!(rows[0].status, "refunded");
    assert_eq!(rows[0].dispatched_ms, None);
    // The refunded prediction is free again: a 900-micro reservation
    // fits the 1000 cap.
    let again = ledger2
        .reserve(
            sid,
            task,
            OpId::new(51),
            900,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    ledger2.refund(sid, again).await.unwrap();
    // Idempotent recovery: a second run finds nothing open.
    ledger2.recover_after_restart();
    assert_eq!(ledger2.view(sid, task).open_reservations, 0);
}

#[tokio::test]
async fn crash_after_the_dispatch_marker_is_uncertain_and_keeps_consuming_free() {
    // Crash AFTER the provider request was sent (marker written) but
    // before the local settle: the provider may have billed. Recovery
    // marks the reservation UNCERTAIN and it KEEPS consuming the
    // reserved amount — the next reservation correctly refuses.
    let dir = tempfile::tempdir().unwrap();
    let (sid, task);
    {
        let m = crate::SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
            .unwrap();
        let s = session(&m);
        let ledger = DurableBudgetLedger::new(m.clone());
        task = seeded_task(&s, Some(1_000));
        sid = s.id;
        let open = ledger
            .reserve(
                sid,
                task,
                OpId::new(60),
                800,
                None,
                faktor_core::model::ExecutionClass::Unknown,
            )
            .await
            .unwrap();
        ledger.mark_dispatched(sid, open).await.unwrap();
        // Crash: the reservation never settled.
    }
    let m2 = crate::SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
        .unwrap();
    let ledger2 = DurableBudgetLedger::new(m2.clone());
    ledger2.recover_after_restart();
    let view = ledger2.view(sid, task);
    assert_eq!(view.open_reservations, 0);
    assert_eq!(view.uncertain_reservations, 1);
    assert_eq!(
        view.uncertain_reserved_micro, 800,
        "the may-have-billed prediction keeps consuming free"
    );
    assert_eq!(view.spent_cost_micro, 0);
    // Free = 1000 - 800 = 200: a 201-micro reserve refuses.
    let err = ledger2
        .reserve(
            sid,
            task,
            OpId::new(61),
            201,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap_err();
    assert_eq!(
        err,
        BudgetError::BudgetExceeded {
            free: 200,
            predicted: 201
        }
    );
    // The row carries the durable marker and its snapshot verbatim.
    let rows = ledger2.reservations_of(sid, task, 10).unwrap();
    assert_eq!(rows[0].status, "uncertain");
    assert!(rows[0].dispatched_ms.is_some());
}

#[tokio::test]
async fn reconcile_settles_an_uncertain_reservation_from_the_ops_completed_call() {
    // (iii) A later settle for the same op id settles the crashed
    // attempt: once the op's durable provider_call row completes, the
    // reconcile closes the UNCERTAIN reservation FROM that row — tokens
    // at the reservation's own frozen snapshot.
    let dir = tempfile::tempdir().unwrap();
    let (sid, task, op);
    {
        let m = crate::SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
            .unwrap();
        let s = session(&m);
        let ledger = DurableBudgetLedger::new(m.clone());
        task = seeded_task(&s, Some(50_000));
        sid = s.id;
        op = OpId::new(70);
        let r = ledger
            .reserve(
                sid,
                task,
                op,
                10_000,
                Some(known_snapshot()),
                faktor_core::model::ExecutionClass::Unknown,
            )
            .await
            .unwrap();
        ledger.mark_dispatched(sid, r).await.unwrap();
        // Crash mid-flight: the reservation never settled.
    }
    let m2 = crate::SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
        .unwrap();
    let ledger2 = DurableBudgetLedger::new(m2.clone());
    ledger2.recover_after_restart();
    let view = ledger2.view(sid, task);
    assert_eq!(view.uncertain_reservations, 1);
    // Reconcile before anything completed: the row has no completed
    // provider-call row to settle FROM, so it stays UNCERTAIN for the
    // task-completion finalize (the report counts only rows it examined).
    let report = ledger2.reconcile_uncertain(sid, task).await.unwrap();
    assert_eq!(report, faktor_store::CostReconcileReport::default());
    assert_eq!(
        ledger2.view(sid, task).uncertain_reservations,
        1,
        "nothing to reconcile: the row is still UNCERTAIN and consuming"
    );
    assert_eq!(ledger2.view(sid, task).spent_cost_micro, 0);
    // The resumed op completes: its durable provider_call row records
    // 100k in + 2k out (a later settle for the same op id).
    let s2 = m2.get_session(sid).unwrap().unwrap();
    s2.settle_usage(op, "p", "m", "completed", Some(100_000), Some(2_000), None)
        .await
        .unwrap();
    let report = ledger2.reconcile_uncertain(sid, task).await.unwrap();
    assert_eq!(report.settled, 1);
    assert_eq!(
        report.charged_micro, 1_620_000,
        "100k @15 + 2k @60 == 1_620_000 micro at the frozen snapshot"
    );
    let view = ledger2.view(sid, task);
    assert_eq!(view.spent_cost_micro, 1_620_000);
    assert_eq!(view.uncertain_reservations, 0);
    let rows = ledger2.reservations_of(sid, task, 10).unwrap();
    assert_eq!(rows[0].status, "settled");
    assert_eq!(rows[0].provider_cost_micro, Some(1_620_000));
    // Idempotent: a second pass settles nothing more.
    let report = ledger2.reconcile_uncertain(sid, task).await.unwrap();
    assert_eq!(report, faktor_store::CostReconcileReport::default());
    assert_eq!(ledger2.view(sid, task).spent_cost_micro, 1_620_000);
}

#[tokio::test]
async fn reconcile_without_a_completed_call_leaves_the_row_for_the_finalize() {
    // A crashed dispatch whose op NEVER completed (no provider_call
    // row, no later settle) stays UNCERTAIN consuming free until the
    // task-completion finalize charges the reserved estimate.
    let dir = tempfile::tempdir().unwrap();
    let (sid, task);
    {
        let m = crate::SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
            .unwrap();
        let s = session(&m);
        let ledger = DurableBudgetLedger::new(m.clone());
        task = seeded_task(&s, Some(10_000));
        sid = s.id;
        let r = ledger
            .reserve(
                sid,
                task,
                OpId::new(80),
                7_500,
                Some(known_snapshot()),
                faktor_core::model::ExecutionClass::Unknown,
            )
            .await
            .unwrap();
        ledger.mark_dispatched(sid, r).await.unwrap();
        let _ = ledger
            .reserve(
                sid,
                task,
                OpId::new(81),
                1_000,
                None,
                faktor_core::model::ExecutionClass::Unknown,
            )
            .await
            .unwrap();
        // Crash: both reservations never settled.
    }
    let m2 = crate::SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
        .unwrap();
    let ledger2 = DurableBudgetLedger::new(m2.clone());
    ledger2.recover_after_restart();
    // Reconcile finds no completed provider_call row to settle FROM:
    // the report is empty (rows without a completed row are never
    // examined) and the row stays UNCERTAIN, still consuming free. (The
    // unmarked second reservation was REFUNDED by recovery — dispatch
    // never provably began.)
    let report = ledger2.reconcile_uncertain(sid, task).await.unwrap();
    assert_eq!(report, faktor_store::CostReconcileReport::default());
    let view = ledger2.view(sid, task);
    assert_eq!(view.uncertain_reservations, 1);
    assert_eq!(view.uncertain_reserved_micro, 7_500);
    assert_eq!(view.spent_cost_micro, 0);
}

#[tokio::test]
async fn completion_finalize_charges_the_estimate_exactly_once() {
    // (iv) Task completion: outstanding UNCERTAIN rows settle
    // conservatively AT their reserved estimates — once, idempotently.
    let dir = tempfile::tempdir().unwrap();
    let (sid, task);
    {
        let m = crate::SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
            .unwrap();
        let s = session(&m);
        let ledger = DurableBudgetLedger::new(m.clone());
        task = seeded_task(&s, Some(50_000));
        sid = s.id;
        // A settled call that DID run carries its own row.
        let settled = ledger
            .reserve(
                sid,
                task,
                OpId::new(90),
                500,
                Some(known_snapshot()),
                faktor_core::model::ExecutionClass::Unknown,
            )
            .await
            .unwrap();
        ledger.mark_dispatched(sid, settled).await.unwrap();
        ledger
            .settle_usage(sid, settled, 0, 0, 0, 0, Some(120), None)
            .await
            .unwrap();
        // Two dispatched-but-never-settled attempts of crashed runs.
        for (i, predicted) in [(91, 2_000u64), (92, 3_000u64)] {
            let r = ledger
                .reserve(
                    sid,
                    task,
                    OpId::new(i),
                    predicted,
                    None,
                    faktor_core::model::ExecutionClass::Unknown,
                )
                .await
                .unwrap();
            ledger.mark_dispatched(sid, r).await.unwrap();
        }
        // Crash: both outstanding reservations never settled.
    }
    let m2 = crate::SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
        .unwrap();
    let ledger2 = DurableBudgetLedger::new(m2.clone());
    ledger2.recover_after_restart();
    let view = ledger2.view(sid, task);
    assert_eq!(view.spent_cost_micro, 120);
    assert_eq!(view.uncertain_reservations, 2);
    assert_eq!(view.uncertain_reserved_micro, 5_000);
    // Task completion: the finalize charges each at its estimate.
    let report = ledger2.finalize_uncertain(sid, task).await.unwrap();
    assert_eq!(report.settled, 2);
    assert_eq!(report.charged_micro, 5_000);
    let view = ledger2.view(sid, task);
    assert_eq!(
        view.spent_cost_micro, 5_120,
        "120 settled + 2_000 + 3_000 finalized at the estimates"
    );
    assert_eq!(view.uncertain_reservations, 0);
    assert_eq!(view.uncertain_reserved_micro, 0);
    let rows = ledger2.reservations_of(sid, task, 10).unwrap();
    for row in &rows {
        match row.op_id.raw() {
            91 | 92 => {
                assert_eq!(row.status, "settled");
                assert_eq!(row.provider_cost_micro, Some(row.predicted_micro));
                assert_eq!(row.provider_reported_micro, None);
            }
            _ => {}
        }
    }
    // Idempotent: a second finalize finds nothing UNCERTAIN and never
    // double-charges.
    let report = ledger2.finalize_uncertain(sid, task).await.unwrap();
    assert_eq!(report.settled, 0);
    assert_eq!(report.charged_micro, 0);
    assert_eq!(ledger2.view(sid, task).spent_cost_micro, 5_120);
}

#[tokio::test]
async fn dispatch_marker_is_written_before_the_provider_call_and_second_mark_is_idempotent() {
    let (_d, m, ledger) = fresh_ledger();
    let s = session(&m);
    let task = seeded_task(&s, Some(1_000));
    let r = ledger
        .reserve(
            s.id,
            task,
            OpId::new(95),
            10,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    // A second mark of the same still-dispatchable row is idempotent.
    ledger.mark_dispatched(s.id, r).await.unwrap();
    ledger.mark_dispatched(s.id, r).await.unwrap();
    let rows = ledger.reservations_of(s.id, task, 10).unwrap();
    assert_eq!(rows[0].status, "dispatched");
    assert!(rows[0].dispatched_ms.is_some());
    // A marked reservation refuses settle-after-refund etc. exactly
    // once: settle closes it normally.
    ledger
        .settle_usage(s.id, r, 0, 0, 0, 0, Some(5), None)
        .await
        .unwrap();
    assert!(matches!(
        ledger.mark_dispatched(s.id, r).await.unwrap_err(),
        BudgetError::NotOpen { .. }
    ));
}

#[tokio::test]
async fn hostile_reservation_ids_and_oversized_route_json_are_typed() {
    let (_d, m, ledger) = fresh_ledger();
    let s = session(&m);
    let task = seeded_task(&s, Some(1_000));
    assert!(matches!(
        ledger
            .settle_usage(s.id, ReservationId::new(99_999), 1, 0, 0, 0, None, None)
            .await
            .unwrap_err(),
        BudgetError::UnknownReservation(_)
    ));
    assert!(matches!(
        ledger
            .refund(s.id, ReservationId::new(99_999))
            .await
            .unwrap_err(),
        BudgetError::UnknownReservation(_)
    ));
    assert!(matches!(
        ledger
            .mark_dispatched(s.id, ReservationId::new(99_999))
            .await
            .unwrap_err(),
        BudgetError::UnknownReservation(_)
    ));
    let big = "x".repeat(MAX_ROUTE_DECISION_JSON_BYTES + 1);
    let r = ledger
        .reserve(
            s.id,
            task,
            OpId::new(60),
            5,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    assert!(matches!(
        ledger
            .settle_usage(s.id, r, 1, 0, 0, 0, None, Some(big))
            .await
            .unwrap_err(),
        BudgetError::Malformed(_)
    ));
    ledger.refund(s.id, r).await.unwrap();
    // Missing task row: typed, and reserve refuses before any write.
    let err = ledger
        .reserve(
            s.id,
            TaskId::new(777),
            OpId::new(61),
            1,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, BudgetError::MissingTask { .. }));
}

#[test]
fn noop_budget_never_refuses_and_views_are_unlimited() {
    let b = NoopBudget;
    let v = b.view(SessionId::new(1), TaskId::new(1));
    assert_eq!(v.max_cost_micro, None);
    assert_eq!(v.free(), u64::MAX);
    b.recover_after_restart();
}

#[tokio::test]
async fn budget_read_failure_is_not_unlimited() {
    // Adversarial: a budget read that cannot be served is a TYPED
    // Unavailable — never a synthesized unlimited/zero view. The failed
    // read must fail closed: only a read that PROVED the task carries no
    // cap may proceed with accounting-unavailable telemetry.
    let (_d, m, ledger) = fresh_ledger();
    let s = session(&m);
    let task = seeded_task(&s, Some(1_000));
    assert!(ledger.view(s.id, task).spent_cost_micro == 0);
    // Kill the bounded read pool the manager's async view uses. The
    // durable ledger itself is untouched (the failure is the read
    // surface, not corruption): its sync view still answers.
    assert!(
        m.read_service()
            .shutdown(std::time::Duration::from_secs(10))
            .await,
        "the read pool shuts down"
    );
    let err = m.budget_view(s.id, task).await.unwrap_err();
    assert!(
        matches!(
            err,
            BudgetError::Unavailable {
                cap: BudgetCapEvidence::Unknown,
                ..
            }
        ),
        "a pool failure carries no cap evidence: {err:?}"
    );
    assert!(
        !err.read_failure_is_explicitly_uncapped(),
        "a failed read never proves a task is unlimited"
    );
    let state = m.budget_state(s.id, task).await;
    assert!(!state.is_known(), "a failed read is never Known");
    assert!(state.known().is_none());
    assert!(state.reason().is_some());
    // The policy predicate is exact: Uncapped only.
    assert!(BudgetError::Unavailable {
        cap: BudgetCapEvidence::Uncapped,
        reason: "r".into(),
    }
    .read_failure_is_explicitly_uncapped());
    assert!(!BudgetError::Unavailable {
        cap: BudgetCapEvidence::HardCap(7),
        reason: "r".into(),
    }
    .read_failure_is_explicitly_uncapped());
    // The direct sync view (a different read surface sharing the same
    // store) still produces the honest durable picture.
    assert_eq!(ledger.view(s.id, task).max_cost_micro, Some(1_000));
}

#[tokio::test]
async fn reserve_refused_once_task_permits_no_new_provider_operation() {
    // Adversarial: once a task enters the verification/completion path
    // (or a terminal state) the reserve transaction refuses a new
    // provider operation typed and writes NOTHING — no reservation can
    // be created after the completion gate has started closing books.
    use faktor_core::state::TaskTransition;
    let (_d, m, ledger) = fresh_ledger();
    let s = session(&m);
    let task = seeded_task(&s, Some(1_000_000));
    // Pending permits a reserve (the baseline).
    let r = ledger
        .reserve(
            s.id,
            task,
            OpId::new(1),
            5,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    ledger.refund(s.id, r).await.unwrap();
    // Drive Pending -> Running -> NeedsVerification -> Verifying.
    let rev = s.task_revision(task).unwrap();
    s.transition_task(task, rev, TaskTransition::StartRunning, None)
        .unwrap();
    let rev = s.task_revision(task).unwrap();
    s.transition_task(task, rev, TaskTransition::RequestVerification, None)
        .unwrap();
    let rev = s.task_revision(task).unwrap();
    s.transition_task(task, rev, TaskTransition::StartVerification, None)
        .unwrap();
    let verifying_revision = s.task_revision(task).unwrap();
    for op in [10u64, 11] {
        let err = ledger
            .reserve(
                s.id,
                task,
                OpId::new(op),
                5,
                None,
                faktor_core::model::ExecutionClass::Unknown,
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, BudgetError::TaskStateForbidsReserve { .. }),
            "{err}"
        );
    }
    let attempt = ModelCallAttempt::new(OpId::new(20), OpId::new(21), 0).unwrap();
    let err = ledger
        .reserve_attempt(
            s.id,
            task,
            attempt,
            5,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, BudgetError::TaskStateForbidsReserve { .. }),
        "{err}"
    );
    // Nothing new was written (the baseline row is the refunded one) and
    // the task row did not move.
    let rows = ledger.reservations_of(s.id, task, 10).unwrap();
    assert!(
        rows.iter()
            .all(|r| !matches!(r.status.as_str(), "reserved" | "dispatched" | "uncertain")),
        "a refused reserve wrote a holding row: {rows:?}"
    );
    let row = s.get_task(task).unwrap().unwrap();
    assert_eq!(row.state, TaskState::Verifying);
    assert_eq!(s.task_revision(task).unwrap(), verifying_revision);
    // A terminal state refuses just the same.
    s.transition_task(
        task,
        verifying_revision,
        TaskTransition::FailFromVerifying,
        None,
    )
    .unwrap();
    let err = ledger
        .reserve(
            s.id,
            task,
            OpId::new(30),
            5,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(err, BudgetError::TaskStateForbidsReserve { .. }),
        "{err}"
    );
}

#[tokio::test]
async fn refund_before_dispatch_ok_after_dispatch_sql_refused_and_money_never_frees() {
    // (i) The hardened refund contract at the ledger surface: refund
    // before dispatch works; refund after mark_dispatched is the typed
    // CannotRefundDispatched with the row untouched and free unchanged
    // (the store's guarded UPDATE changed zero rows — the money stays
    // put even against a mis-calling runtime); refund after settle and
    // after a refund is typed NotOpen.
    let (_d, m, ledger) = fresh_ledger();
    let s = session(&m);
    let task = seeded_task(&s, Some(1_000));
    let free = || ledger.view(s.id, task).free();

    // Pre-dispatch refund: applied, state REFUNDED, money free.
    let r = ledger
        .reserve(
            s.id,
            task,
            OpId::new(1),
            400,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    assert_eq!(free(), 600);
    ledger.refund(s.id, r).await.unwrap();
    assert_eq!(free(), 1_000, "a pre-dispatch refund frees the money");
    assert_eq!(
        ledger.reservation_state(s.id, r).unwrap(),
        ReservationState::Refunded
    );
    assert!(matches!(
        ledger.refund(s.id, r).await.unwrap_err(),
        BudgetError::NotOpen { status, .. } if status == "refunded"
    ));

    // Dispatched: refund is refused with the typed error, nothing moves.
    let r2 = ledger
        .reserve(
            s.id,
            task,
            OpId::new(2),
            400,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    ledger.mark_dispatched(s.id, r2).await.unwrap();
    assert_eq!(free(), 600);
    let err = ledger.refund(s.id, r2).await.unwrap_err();
    assert!(
        matches!(err, BudgetError::CannotRefundDispatched { reservation } if reservation == r2.raw())
    );
    assert_eq!(
        ledger.reservation_state(s.id, r2).unwrap(),
        ReservationState::Dispatched
    );
    let rows = ledger.reservations_of(s.id, task, 10).unwrap();
    assert_eq!(rows[0].status, "dispatched", "row untouched");
    assert!(rows[0].dispatched_ms.is_some());
    assert_eq!(free(), 600, "free unchanged: the SQL guard freed nothing");
    // The mis-call pattern of the CURRENT runtime (refund on a
    // post-dispatch error) cannot free money even repeated.
    assert!(matches!(
        ledger.refund(s.id, r2).await.unwrap_err(),
        BudgetError::CannotRefundDispatched { .. }
    ));
    assert_eq!(free(), 600);

    // Settled-after-dispatch: refund is refused — the row was dispatched
    // (marker) and closed; typed, row untouched.
    ledger
        .settle_usage(s.id, r2, 0, 0, 0, 0, Some(5), None)
        .await
        .unwrap();
    assert!(matches!(
        ledger.refund(s.id, r2).await.unwrap_err(),
        BudgetError::CannotRefundDispatched { reservation } if reservation == r2.raw()
    ));
    assert_eq!(
        ledger.reservation_state(s.id, r2).unwrap(),
        ReservationState::Settled
    );
}

#[tokio::test]
async fn post_dispatch_failure_mark_uncertain_records_reason_request_id_and_keeps_free() {
    // (v) mark_uncertain moves a DISPATCHED reservation to UNCERTAIN
    // with the failure reason code + provider request id recorded, the
    // delivery state marked failed, and the hold KEEPING consumption
    // until the finalize charges the estimate. Never-dispatched rows
    // refuse (refund them); reasons/ids are bounded.
    let (_d, m, ledger) = fresh_ledger();
    let s = session(&m);
    let task = seeded_task(&s, Some(5_000));
    let r = ledger
        .reserve(
            s.id,
            task,
            OpId::new(11),
            2_000,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();

    // A never-dispatched row refuses (it is refundable instead).
    let err = ledger
        .mark_uncertain(s.id, r, "stream_error".into(), Some("req-1".into()))
        .await
        .unwrap_err();
    assert!(matches!(err, BudgetError::NotOpen { status, .. } if status == "reserved"));

    ledger.mark_dispatched(s.id, r).await.unwrap();
    ledger
        .mark_uncertain(s.id, r, "stall_verdict".into(), Some("req-42".into()))
        .await
        .unwrap();
    // Exactly-once reason capture: a second mark is a typed refusal.
    assert!(matches!(
        ledger
            .mark_uncertain(s.id, r, "other".into(), None)
            .await
            .unwrap_err(),
        BudgetError::NotOpen { status, .. } if status == "uncertain"
    ));
    let rows = ledger.reservations_of(s.id, task, 10).unwrap();
    assert_eq!(rows[0].status, "uncertain");
    assert_eq!(
        rows[0].failure_reason_code.as_deref(),
        Some("stall_verdict")
    );
    assert_eq!(rows[0].request_id.as_deref(), Some("req-42"));
    assert_eq!(rows[0].delivery_state.as_deref(), Some("failed"));
    let view = ledger.view(s.id, task);
    assert_eq!(
        view.uncertain_reserved_micro, 2_000,
        "uncertain keeps consuming"
    );
    assert_eq!(view.free(), 3_000);
    // Refund of an uncertain row is impossible and typed.
    assert!(matches!(
        ledger.refund(s.id, r).await.unwrap_err(),
        BudgetError::CannotRefundDispatched { reservation } if reservation == r.raw()
    ));
    // The task-end finalize closes it at the reserved estimate.
    let report = ledger.finalize_uncertain(s.id, task).await.unwrap();
    assert_eq!(report.settled, 1);
    assert_eq!(report.charged_micro, 2_000);
    assert_eq!(ledger.view(s.id, task).spent_cost_micro, 2_000);

    // Bounded reason/request id: hostile input is Malformed pre-write.
    let r2 = ledger
        .reserve(
            s.id,
            task,
            OpId::new(12),
            10,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    ledger.mark_dispatched(s.id, r2).await.unwrap();
    let big_reason = "x".repeat(MAX_FAILURE_REASON_CODE_BYTES + 1);
    assert!(matches!(
        ledger
            .mark_uncertain(s.id, r2, big_reason, None)
            .await
            .unwrap_err(),
        BudgetError::Malformed(_)
    ));
    let big_id = "x".repeat(MAX_REQUEST_ID_BYTES + 1);
    assert!(matches!(
        ledger
            .mark_uncertain(s.id, r2, "ok".into(), Some(big_id))
            .await
            .unwrap_err(),
        BudgetError::Malformed(_)
    ));
    assert!(matches!(
        ledger
            .mark_uncertain(s.id, r2, "".into(), None)
            .await
            .unwrap_err(),
        BudgetError::Malformed(_)
    ));
    // Nothing moved: still dispatched, and its refund stays SQL-refused.
    assert_eq!(
        ledger.reservation_state(s.id, r2).unwrap(),
        ReservationState::Dispatched
    );
    assert!(matches!(
        ledger.refund(s.id, r2).await.unwrap_err(),
        BudgetError::CannotRefundDispatched { .. }
    ));
    assert!(matches!(
        ledger
            .mark_uncertain(s.id, ReservationId::new(99_999), "x".into(), None)
            .await
            .unwrap_err(),
        BudgetError::UnknownReservation(_)
    ));
}

#[tokio::test]
async fn two_attempts_of_one_logical_op_hold_distinct_reservations_and_rows() {
    // (iii) The attempt surface end to end: two physical attempts of ONE
    // logical op get distinct attempt ids, independent reservations
    // (each holding its own prediction), and separate provider-call rows
    // keyed by attempt — written through the new attempt APIs.
    let (_d, m, ledger) = fresh_ledger();
    let s = session(&m);
    let task = seeded_task(&s, Some(10_000));
    let logical = m.try_next_op_id().unwrap();
    let a1 = ModelCallAttempt::new(logical, m.try_next_op_id().unwrap(), 0).unwrap();
    let a2 = ModelCallAttempt::new(logical, m.try_next_op_id().unwrap(), 1).unwrap();
    assert_ne!(a1.attempt_op_id, a2.attempt_op_id);

    let r1 = ledger
        .reserve_attempt(
            s.id,
            task,
            a1,
            1_000,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    let r2 = ledger
        .reserve_attempt(
            s.id,
            task,
            a2,
            2_000,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    assert_ne!(r1, r2, "one reservation per physical attempt");
    let view = ledger.view(s.id, task);
    assert_eq!(view.open_reservations, 2);
    assert_eq!(view.open_reserved_micro, 3_000, "both holds count");

    let rows = ledger.reservations_of(s.id, task, 10).unwrap();
    let row1 = rows.iter().find(|r| r.reservation_id == r1.raw()).unwrap();
    let row2 = rows.iter().find(|r| r.reservation_id == r2.raw()).unwrap();
    assert_eq!(row1.attempt_op_id, Some(a1.attempt_op_id));
    assert_eq!(row1.parent_op_id, Some(logical));
    assert_eq!(row2.attempt_op_id, Some(a2.attempt_op_id));
    assert_eq!(row2.parent_op_id, Some(logical));

    // Dispatch both, then write one provider-call row per attempt.
    ledger.mark_dispatched(s.id, r1).await.unwrap();
    ledger.mark_dispatched(s.id, r1).await.unwrap(); // idempotent
    ledger.mark_dispatched(s.id, r2).await.unwrap();
    let p1 = s
        .record_provider_call_attempt(a1, Some(r1), "fake", "m", "started", None, None, None)
        .unwrap();
    let p2 = s
        .record_provider_call_attempt(
            a2,
            Some(r2),
            "fake",
            "m",
            "completed",
            Some(30),
            Some(40),
            None,
        )
        .unwrap();
    assert_ne!(p1, p2, "two attempts = two distinct provider-call rows");
    assert!(p1 > 0 && p2 > 0);

    // Attempt rows are independent: settle attempt 1 from ITS row cost;
    // attempt 2's dispatched reservation refuses refunds (SQL guard).
    ledger
        .settle_usage(s.id, r1, 0, 0, 0, 0, Some(99), None)
        .await
        .unwrap();
    assert_eq!(ledger.view(s.id, task).spent_cost_micro, 99);
    assert!(matches!(
        ledger.refund(s.id, r2).await.unwrap_err(),
        BudgetError::CannotRefundDispatched { .. }
    ));
    // A crash of attempt 2 alone leaves only IT uncertain after restart.
    let sid = s.id;
    drop(ledger);
    drop(s);
    drop(m);
    let m2 = SessionManager::open(_d.path().join("store"), _d.path().join("cas"), true).unwrap();
    let ledger2 = DurableBudgetLedger::new(m2.clone());
    ledger2.recover_after_restart();
    let rows = ledger2.reservations_of(sid, task, 10).unwrap();
    // Re-find the attempt-2 row under its own attempt id after recovery.
    let a2_row = rows
        .iter()
        .find(|r| r.attempt_op_id == Some(a2.attempt_op_id))
        .unwrap();
    assert_eq!(
        a2_row.status, "uncertain",
        "only attempt 2's hold is uncertain"
    );
    let a1_row = rows
        .iter()
        .find(|r| r.attempt_op_id == Some(a1.attempt_op_id))
        .unwrap();
    assert_eq!(
        a1_row.status, "settled",
        "attempt 1 stayed settled across restart"
    );
}

#[tokio::test]
async fn settle_records_an_honest_cost_basis_read_back_on_the_row() {
    // (vi) settle_usage records the v18 cost columns: the honest basis
    // behind the folded amount (ProviderReported when the provider
    // billed, RouteSnapshotEstimate when categories x the frozen
    // snapshot won, Unknown when nothing was folded), the folded amount,
    // the provider-reported amount and the frozen reserve-time estimate.
    let (_d, m, ledger) = fresh_ledger();

    // Provider under-report: the local token-priced amount is the floor.
    let s1 = session(&m);
    let task1 = seeded_task(&s1, Some(5_000_000));
    let r1 = ledger
        .reserve(
            s1.id,
            task1,
            OpId::new(41),
            100_000,
            Some(known_snapshot()),
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    ledger.mark_dispatched(s1.id, r1).await.unwrap();
    ledger
        .settle_usage(s1.id, r1, 100_000, 0, 0, 2_000, Some(9_999), None)
        .await
        .unwrap();
    let row = ledger.reservations_of(s1.id, task1, 10).unwrap()[0].clone();
    assert_eq!(row.cost_basis.as_deref(), Some("RouteSnapshotEstimate"));
    assert_eq!(row.settled_cost_micro, Some(1_620_000));
    assert_eq!(row.provider_reported_cost_micro, Some(9_999));
    assert_eq!(row.estimated_cost_micro, Some(100_000));
    assert_eq!(
        row.provider_cost_micro,
        Some(1_620_000),
        "local actual kept too"
    );

    // No provider report: categories x the frozen snapshot wins.
    let s2 = session(&m);
    let task2 = seeded_task(&s2, Some(5_000_000));
    let r2 = ledger
        .reserve(
            s2.id,
            task2,
            OpId::new(42),
            2_000_000,
            Some(known_snapshot()),
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    ledger.mark_dispatched(s2.id, r2).await.unwrap();
    let chosen = ledger
        .settle_usage(s2.id, r2, 100_000, 0, 0, 2_000, None, None)
        .await
        .unwrap();
    assert_eq!(chosen, Some(1_620_000));
    let row = ledger.reservations_of(s2.id, task2, 10).unwrap()[0].clone();
    assert_eq!(row.cost_basis.as_deref(), Some("RouteSnapshotEstimate"));
    assert_eq!(row.settled_cost_micro, Some(1_620_000));
    assert_eq!(row.provider_reported_cost_micro, None);

    // No price authority and no cap: a documented Unknown spend.
    let s3 = session(&m);
    let task3 = seeded_task(&s3, None);
    let r3 = ledger
        .reserve(
            s3.id,
            task3,
            OpId::new(43),
            100,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    ledger.mark_dispatched(s3.id, r3).await.unwrap();
    let chosen = ledger
        .settle_usage(s3.id, r3, 1_000, 0, 0, 2_000, None, None)
        .await
        .unwrap();
    assert_eq!(chosen, None, "nothing was folded");
    let row = ledger.reservations_of(s3.id, task3, 10).unwrap()[0].clone();
    assert_eq!(row.cost_basis.as_deref(), Some("Unknown"));
    assert_eq!(row.settled_cost_micro, None);
    assert_eq!(ledger.view(s3.id, task3).spent_cost_micro, 0);

    // Under a hard cap the same no-authority settle is a typed refusal.
    let s4 = session(&m);
    let task4 = seeded_task(&s4, Some(1_000));
    let r4 = ledger
        .reserve(
            s4.id,
            task4,
            OpId::new(44),
            100,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    ledger.mark_dispatched(s4.id, r4).await.unwrap();
    assert!(matches!(
        ledger
            .settle_usage(s4.id, r4, 1_000, 0, 0, 2_000, None, None)
            .await
            .unwrap_err(),
        BudgetError::UnknownPrice { .. }
    ));
}

#[tokio::test]
async fn reconcile_settles_each_crashed_attempt_from_its_own_provider_row() {
    // (vii) Reconciliation joins BY ATTEMPT: two dispatched attempts of
    // one logical op crash UNCERTAIN; after restart each attempt's
    // completed provider-call row settles ITS OWN reservation — exactly
    // the right rows, even though both rows share the logical op (the
    // old op_id join would have settled the older reservation from the
    // newest completed sibling row).
    let (_d, m, ledger) = fresh_ledger();
    let s = session(&m);
    let task = seeded_task(&s, Some(10_000_000));
    let logical = m.try_next_op_id().unwrap();
    let a1 = ModelCallAttempt::new(logical, m.try_next_op_id().unwrap(), 0).unwrap();
    let a2 = ModelCallAttempt::new(logical, m.try_next_op_id().unwrap(), 1).unwrap();
    let r1 = ledger
        .reserve_attempt(
            s.id,
            task,
            a1,
            500_000,
            Some(known_snapshot()),
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    let r2 = ledger
        .reserve_attempt(
            s.id,
            task,
            a2,
            500_000,
            Some(known_snapshot()),
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    ledger.mark_dispatched(s.id, r1).await.unwrap();
    ledger.mark_dispatched(s.id, r2).await.unwrap();
    // Crash: both dispatched rows never settled.
    let sid = s.id;
    drop(ledger);
    drop(s);
    drop(m);
    let m2 = SessionManager::open(_d.path().join("store"), _d.path().join("cas"), true).unwrap();
    let ledger2 = DurableBudgetLedger::new(m2.clone());
    ledger2.recover_after_restart();
    let s2 = m2.get_session(sid).unwrap().unwrap();
    let view = ledger2.view(s2.id, task);
    assert_eq!(view.uncertain_reservations, 2);

    // Both attempts complete after the restart; attempt 2's row lands
    // FIRST and attempt 1's row becomes the newest completed row overall.
    // a2: 1_000 in + 100 out @15/60 per M == 15_000 + 6_000 == 21_000.
    s2.record_provider_call_attempt(
        a2,
        Some(r2),
        "fake",
        "m",
        "completed",
        Some(1_000),
        Some(100),
        None,
    )
    .unwrap();
    // a1: 4_000 in + 2_000 out == 60_000 + 120_000 == 180_000.
    s2.record_provider_call_attempt(
        a1,
        Some(r1),
        "fake",
        "m",
        "completed",
        Some(4_000),
        Some(2_000),
        None,
    )
    .unwrap();

    let report = ledger2.reconcile_uncertain(s2.id, task).await.unwrap();
    assert_eq!(report.settled, 2, "both crashed attempts settled");
    assert_eq!(report.charged_micro, 21_000 + 180_000);
    let rows = ledger2.reservations_of(s2.id, task, 10).unwrap();
    let row1 = rows.iter().find(|r| r.reservation_id == r1.raw()).unwrap();
    let row2 = rows.iter().find(|r| r.reservation_id == r2.raw()).unwrap();
    assert_eq!(row1.status, "settled");
    assert_eq!(row1.provider_cost_micro, Some(180_000), "a1 from a1's row");
    assert_eq!(row2.provider_cost_micro, Some(21_000), "a2 from a2's row");
    assert_eq!(row1.cost_basis.as_deref(), Some("RouteSnapshotEstimate"));
    assert_eq!(row2.cost_basis.as_deref(), Some("RouteSnapshotEstimate"));
    assert_eq!(row1.settled_cost_micro, Some(180_000));
    assert_eq!(row2.settled_cost_micro, Some(21_000));
    // The legacy op join is NOT what settled r2: its charged amount is
    // exactly a2's tokens, never the newest sibling row's 180_000.
    // Idempotent: a second pass settles nothing more.
    let report = ledger2.reconcile_uncertain(s2.id, task).await.unwrap();
    assert_eq!(report, faktor_store::CostReconcileReport::default());
    assert_eq!(ledger2.view(s2.id, task).spent_cost_micro, 21_000 + 180_000);
}

// ----------------------------------------- max_cost_micro task control
// (audit 9/H: additive cap guard + hierarchical subscope admission.)

/// An orchestrated child session under `parent` with its durable
/// identity row (the runtime's own spawn writes exactly this row).
fn child_session(m: &Arc<SessionManager>, parent: SessionId, item: &str) -> crate::SessionHandle {
    let ws = m.create_workspace(&format!("/w-{item}")).unwrap();
    let s = m.create_session(ws, item, "fake", "m").unwrap();
    s.orchestrator_child_identity_put(&crate::child::ChildIdentity {
        parent_session_id: parent,
        workspace_id: ws.raw(),
        worktree_id: 1,
        item_id: item.to_string(),
        task_goal: format!("child {item}"),
        model: String::new(),
        operation_id: 0,
        ownership: crate::child::ChildOwnership::ReadOnlyShared,
        require_semantic_delta: false,
        permissions: faktor_core::CapabilitySet::ALL,
        created_ms: 1,
    })
    .unwrap();
    s
}

#[tokio::test]
async fn cap_reduction_below_committed_is_a_typed_refusal_writing_nothing() {
    let (_d, m, ledger) = fresh_ledger();
    let s = session(&m);
    let task = seeded_task(&s, Some(100));
    let r0 = ledger
        .reserve(
            s.id,
            task,
            OpId::new(70),
            60,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    // Lowering under the committed 60 (open reservation) is a typed
    // conflict and writes NOTHING.
    let err = ledger.set_task_max_cost(s.id, task, Some(50)).unwrap_err();
    assert_eq!(
        err,
        BudgetError::CapBelowCommitted {
            new_cap: Some(50),
            committed: 60
        }
    );
    assert_eq!(
        ledger.view(s.id, task).max_cost_micro,
        Some(100),
        "a refused reduction leaves the cap untouched"
    );
    // A reduction exactly at the bound — and above it — succeeds; the
    // guard is new_cap >= settled + open + uncertain, spend never
    // rewinds.
    ledger.set_task_max_cost(s.id, task, Some(60)).unwrap();
    assert_eq!(ledger.view(s.id, task).max_cost_micro, Some(60));
    ledger.set_task_max_cost(s.id, task, Some(61)).unwrap();
    // Settled spend binds too: release the first reservation, settle a
    // new one at 61, then a cap below the settled spend refuses even
    // with nothing open.
    ledger.refund(s.id, r0).await.unwrap();
    let r1 = ledger
        .reserve(
            s.id,
            task,
            OpId::new(71),
            61,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    ledger
        .settle_usage(s.id, r1, 0, 0, 0, 0, Some(61), None)
        .await
        .unwrap();
    let err = ledger.set_task_max_cost(s.id, task, Some(60)).unwrap_err();
    assert_eq!(
        err,
        BudgetError::CapBelowCommitted {
            new_cap: Some(60),
            committed: 61
        }
    );
    // Clearing (None = unlimited) is always legal.
    ledger.set_task_max_cost(s.id, task, None).unwrap();
    assert_eq!(ledger.view(s.id, task).max_cost_micro, None);
    // A missing row keeps its typed missing-row refusal (never a
    // phantom guard pass).
    let err = ledger
        .set_task_max_cost(s.id, TaskId::new(999), Some(1))
        .unwrap_err();
    assert!(matches!(err, BudgetError::MissingTask { .. }), "{err}");
}

#[tokio::test]
async fn child_scope_admission_bounds_child_and_root_remaining_atomically() {
    // (ii) parent $10 with children A $5 + B $5: A spends $4, B reserves
    // $5, A's next $2 is refused by the CHILD remaining although the
    // root still has $1, and B's next $2 is refused by the ROOT
    // remaining although B's own cap has $3 left. A+B can never
    // collectively spend more than the root.
    let (_d, m, ledger) = fresh_ledger();
    let root = session(&m);
    let root_task = seeded_task(&root, Some(10_000_000));
    let a = child_session(&m, root.id, "a");
    let b = child_session(&m, root.id, "b");
    let a_task = a.task_id().unwrap();
    let b_task = b.task_id().unwrap();
    ledger
        .change_child_scope_cap(a.id, "child-a", Some(5_000_000))
        .unwrap();
    ledger
        .change_child_scope_cap(b.id, "child-b", Some(5_000_000))
        .unwrap();
    // The enrollment rows live under the ROOT with typed mirrors.
    let scope_a = ledger.scope_of(a.id).unwrap().unwrap();
    assert_eq!(scope_a.root_session_id, root.id);
    assert_eq!(scope_a.root_task_id, root_task);
    assert_eq!(scope_a.consumer_session_id, a.id);
    assert_eq!(scope_a.consumer_task_id, a_task);
    assert_eq!(scope_a.child_id, "child-a");
    assert_eq!(scope_a.child_cap_micro, Some(5_000_000));
    // A settles $4 of spend.
    let ra = ledger
        .reserve(
            a.id,
            a_task,
            OpId::new(80),
            4_000_000,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    ledger
        .settle_usage(a.id, ra, 0, 0, 0, 0, Some(4_000_000), None)
        .await
        .unwrap();
    // B reserves $5: root remaining is exactly $1 afterwards.
    ledger
        .reserve(
            b.id,
            b_task,
            OpId::new(81),
            5_000_000,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    // A's next $2: refused by A's OWN remaining ($1) — the store's
    // atomic per-row admission — even though the root still has $1.
    let err = ledger
        .reserve(
            a.id,
            a_task,
            OpId::new(82),
            2_000_000,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap_err();
    assert_eq!(
        err,
        BudgetError::BudgetExceeded {
            free: 1_000_000,
            predicted: 2_000_000
        }
    );
    // B's next $2: B's own cap has $3 left but the ROOT remaining ($1,
    // A's settled $4 + B's open $5 consume the $10) refuses first.
    let err = ledger
        .reserve(
            b.id,
            b_task,
            OpId::new(83),
            2_000_000,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap_err();
    assert_eq!(
        err,
        BudgetError::BudgetExceeded {
            free: 1_000_000,
            predicted: 2_000_000
        }
    );
    // The root-level refusal wrote NOTHING: B still holds exactly its
    // one granted reservation and A nothing beyond its settled row.
    assert_eq!(ledger.reservations_of(b.id, b_task, 10).unwrap().len(), 1);
    assert_eq!(ledger.view(a.id, a_task).spent_cost_micro, 4_000_000);
    assert_eq!(ledger.view(a.id, a_task).open_reservations, 0);
    // The cap-reduction guard of the ROOT spans its live enrolled
    // children: lowering $10 under the children's committed $9 is a
    // typed conflict; lowering exactly to the committed bound succeeds.
    let err = ledger
        .set_task_max_cost(root.id, root_task, Some(3_999_999))
        .unwrap_err();
    assert_eq!(
        err,
        BudgetError::CapBelowCommitted {
            new_cap: Some(3_999_999),
            committed: 9_000_000
        }
    );
    ledger
        .set_task_max_cost(root.id, root_task, Some(9_000_000))
        .unwrap();
    // A reserve past the lowered root remaining ($0 left) is refused by
    // the ROOT gate even though A's own row still has $1 of its own cap.
    let err = ledger
        .reserve(
            a.id,
            a_task,
            OpId::new(84),
            1_000_000,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap_err();
    assert_eq!(
        err,
        BudgetError::BudgetExceeded {
            free: 0,
            predicted: 1_000_000
        }
    );
    // Raising the root back to $10 re-opens the exact $1 remaining.
    ledger
        .set_task_max_cost(root.id, root_task, Some(10_000_000))
        .unwrap();
    ledger
        .reserve(
            a.id,
            a_task,
            OpId::new(85),
            1_000_000,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn child_scopes_survive_reopen_and_a_cleared_child_cap_stops_consuming_the_root() {
    // (iv) reopen keeps root + child caps and the enrollment rows; a
    // cleared child cap tombstones its enrollment (None mirror) and the
    // child stops consuming root budget.
    let dir = tempfile::tempdir().unwrap();
    let (root_id, a_id, b_id, root_task, a_task, b_task) = {
        let m =
            SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
        let ledger = DurableBudgetLedger::new(m.clone());
        let root = session(&m);
        let root_task = seeded_task(&root, Some(10_000_000));
        let a = child_session(&m, root.id, "a");
        let b = child_session(&m, root.id, "b");
        ledger
            .change_child_scope_cap(a.id, "child-a", Some(5_000_000))
            .unwrap();
        ledger
            .change_child_scope_cap(b.id, "child-b", Some(5_000_000))
            .unwrap();
        let a_task = a.task_id().unwrap();
        let b_task = b.task_id().unwrap();
        let ra = ledger
            .reserve(
                a.id,
                a_task,
                OpId::new(90),
                4_000_000,
                None,
                faktor_core::model::ExecutionClass::Unknown,
            )
            .await
            .unwrap();
        ledger
            .settle_usage(a.id, ra, 0, 0, 0, 0, Some(4_000_000), None)
            .await
            .unwrap();
        ledger
            .reserve(
                b.id,
                b_task,
                OpId::new(91),
                5_000_000,
                None,
                faktor_core::model::ExecutionClass::Unknown,
            )
            .await
            .unwrap();
        (root.id, a.id, b.id, root_task, a_task, b_task)
    };
    let m = SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true).unwrap();
    let ledger = DurableBudgetLedger::new(m.clone());
    // Caps and enrollments are durable: after the reopen A's next $2 is
    // still refused at the $1 remaining of the $10 root (A spent $4 and
    // B holds $5) — the reopened root gate and child row agree on the
    // same $1 bound.
    let err = ledger
        .reserve(
            a_id,
            a_task,
            OpId::new(92),
            2_000_000,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap_err();
    assert_eq!(
        err,
        BudgetError::BudgetExceeded {
            free: 1_000_000,
            predicted: 2_000_000
        }
    );
    // Clearing A's cap tombstones its enrollment: A's own row is
    // unlimited again (the store grants), while B alone still binds B
    // through its OWN full cap ($5 open of $5 -> $0 free).
    ledger
        .change_child_scope_cap(a_id, "child-a", None)
        .unwrap();
    assert_eq!(
        ledger.scope_of(a_id).unwrap().unwrap().child_cap_micro,
        None,
        "cleared cap tombstones the enrollment mirror"
    );
    assert_eq!(ledger.view(a_id, a_task).max_cost_micro, None);
    ledger
        .reserve(
            a_id,
            a_task,
            OpId::new(93),
            2_000_000,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    let err = ledger
        .reserve(
            b_id,
            b_task,
            OpId::new(94),
            2_000_000,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap_err();
    assert_eq!(
        err,
        BudgetError::BudgetExceeded {
            free: 0,
            predicted: 2_000_000
        },
        "B's own cap is exhausted by its open $5"
    );
    let _ = root_task;
    let _ = a_id;
    let _ = root_id;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_child_reserves_cannot_overshoot_the_root_cap() {
    // Adversarial race: two enrolled children of one $10 root reserve
    // $6 each CONCURRENTLY. Their own rows would each admit $6 — the
    // root-subscope gate serializes admission + insert, so exactly ONE
    // grant lands and the other is a typed refusal (never $12 against
    // a $10 root).
    let (_d, m, ledger) = fresh_ledger();
    let root = session(&m);
    seeded_task(&root, Some(10_000_000));
    let a = child_session(&m, root.id, "a");
    let b = child_session(&m, root.id, "b");
    ledger
        .change_child_scope_cap(a.id, "child-a", Some(10_000_000))
        .unwrap();
    ledger
        .change_child_scope_cap(b.id, "child-b", Some(10_000_000))
        .unwrap();
    let (a_task, b_task) = (a.task_id().unwrap(), b.task_id().unwrap());
    let (fa, fb) = (
        tokio::spawn({
            let ledger = ledger.clone();
            async move {
                ledger
                    .reserve(
                        a.id,
                        a_task,
                        OpId::new(100),
                        6_000_000,
                        None,
                        faktor_core::model::ExecutionClass::Unknown,
                    )
                    .await
            }
        }),
        tokio::spawn({
            let ledger = ledger.clone();
            async move {
                ledger
                    .reserve(
                        b.id,
                        b_task,
                        OpId::new(101),
                        6_000_000,
                        None,
                        faktor_core::model::ExecutionClass::Unknown,
                    )
                    .await
            }
        }),
    );
    let out_a = fa.await.unwrap();
    let out_b = fb.await.unwrap();
    let grants = [&out_a, &out_b].iter().filter(|r| r.is_ok()).count();
    let refusals: Vec<&BudgetError> = [&out_a, &out_b]
        .iter()
        .filter_map(|r| r.as_ref().err())
        .collect();
    assert_eq!(
        grants, 1,
        "exactly one of two $6 reserves lands: {out_a:?} {out_b:?}"
    );
    assert_eq!(refusals.len(), 1);
    assert_eq!(
        refusals[0],
        &BudgetError::BudgetExceeded {
            free: 4_000_000,
            predicted: 6_000_000
        }
    );
}

#[tokio::test]
async fn child_scope_changes_validate_identity_and_bounds() {
    let (_d, m, ledger) = fresh_ledger();
    let root = session(&m);
    seeded_task(&root, Some(10_000_000));
    // A session without an identity row is not an orchestrated child.
    let plain = session(&m);
    let err = ledger
        .change_child_scope_cap(plain.id, "plain", Some(1_000))
        .unwrap_err();
    assert!(matches!(
        err,
        BudgetError::NotAnOrchestratedChild(s) if s == plain.id
    ));
    // Bounded child ids refuse before anything is written.
    let a = child_session(&m, root.id, "a");
    assert!(matches!(
        ledger.change_child_scope_cap(a.id, "", Some(1_000)),
        Err(BudgetError::Malformed(_))
    ));
    assert!(matches!(
        ledger.change_child_scope_cap(a.id, &"x".repeat(65), Some(1_000)),
        Err(BudgetError::Malformed(_))
    ));
    assert!(ledger.scope_of(a.id).unwrap().is_none(), "nothing enrolled");
    // Re-running the same change is an idempotent upsert.
    ledger
        .change_child_scope_cap(a.id, "child-a", Some(1_000))
        .unwrap();
    ledger
        .change_child_scope_cap(a.id, "child-a", Some(2_000))
        .unwrap();
    assert_eq!(
        ledger.scope_of(a.id).unwrap().unwrap().child_cap_micro,
        Some(2_000)
    );
}

// ------------------------------------- change budget (audits 57/105)

#[test]
fn change_budget_path_refusal_inside_acceptance_and_absent_parity() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let task = seeded_task(&s, None);
    let outside = ChangeObservations {
        changed_paths: vec!["anywhere/x.rs".into()],
        ..Default::default()
    };
    // No budget = today's behavior: any path passes.
    assert!(s.enforce_change_budget(task, &outside).is_ok());
    assert_eq!(s.change_budget().unwrap(), None);
    // With a budget: an outside path refuses TYPED (all causes named).
    let budget = ChangeBudget {
        allowed_paths: vec!["src".into()],
        ..Default::default()
    };
    s.set_change_budget(Some(&budget)).unwrap();
    match s.enforce_change_budget(task, &outside).unwrap_err() {
        crate::task::TaskError::ChangeBudgetRefused {
            task_id,
            violations,
        } => {
            assert_eq!(task_id, task);
            assert_eq!(
                violations,
                vec![ChangeBudgetViolation::PathOutsideAllowed {
                    path: "anywhere/x.rs".into()
                }]
            );
        }
        other => panic!("typed change-budget refusal expected, got {other:?}"),
    }
    // Inside the allowed prefix: accepted.
    let inside = ChangeObservations {
        changed_paths: vec!["src/a.rs".into()],
        ..Default::default()
    };
    assert!(s.enforce_change_budget(task, &inside).is_ok());
    // Clearing the budget restores parity.
    s.set_change_budget(None).unwrap();
    assert!(s.enforce_change_budget(task, &outside).is_ok());
}

#[test]
fn change_budget_semantic_unknown_and_flags_have_typed_refusal_paths() {
    let (_d, m) = test_manager();
    let s = session(&m);
    let task = seeded_task(&s, None);
    let budget = ChangeBudget {
        allowed_semantic_entities: vec!["entity-a".into()],
        max_blast_radius: Some(1),
        ..Default::default()
    };
    s.set_change_budget(Some(&budget)).unwrap();
    // Unknown semantic data under a constrained budget => the documented
    // stronger-verification refusal, never a silent pass.
    let unknown = ChangeObservations {
        changed_paths: vec!["src/a.rs".into()],
        ..Default::default()
    };
    let violations = match s.enforce_change_budget(task, &unknown).unwrap_err() {
        crate::task::TaskError::ChangeBudgetRefused { violations, .. } => violations,
        other => panic!("typed change-budget refusal expected, got {other:?}"),
    };
    assert_eq!(violations, vec![ChangeBudgetViolation::SemanticDataUnknown]);
    // Data exists and is inside: accepted.
    let inside = ChangeObservations {
        changed_paths: vec!["src/a.rs".into()],
        semantic_entities: Some(vec!["entity-a".into()]),
        ..Default::default()
    };
    assert!(s.enforce_change_budget(task, &inside).is_ok());
    // Outside entities and an exceeded blast radius are BOTH named.
    let outside = ChangeObservations {
        changed_paths: vec!["src/a.rs".into(), "src/b.rs".into()],
        semantic_entities: Some(vec!["entity-b".into()]),
        ..Default::default()
    };
    let violations = match s.enforce_change_budget(task, &outside).unwrap_err() {
        crate::task::TaskError::ChangeBudgetRefused { violations, .. } => violations,
        other => panic!("typed change-budget refusal expected, got {other:?}"),
    };
    assert!(
        violations.contains(&ChangeBudgetViolation::SemanticEntityOutsideAllowed {
            entity: "entity-b".into()
        })
    );
    assert!(violations.contains(&ChangeBudgetViolation::BlastRadiusExceeded { changed: 2, max: 1 }));
    // Flags: an observed forbidden increase refuses; Unknown is not a
    // hard violation of `check` but IS a typed refusal of the strict
    // "policy requires stronger verification" path.
    let flags = ChangeBudget::default();
    let increased = ChangeObservations {
        unsafe_increase: Some(true),
        ..Default::default()
    };
    assert_eq!(
        check_change_budget(&flags, &increased).unwrap_err(),
        vec![ChangeBudgetViolation::ForbiddenIncrease {
            feature: "unsafe_increase"
        }]
    );
    let unknown_flags = ChangeObservations::default();
    assert!(check_change_budget(&flags, &unknown_flags).is_ok());
    assert!(check_change_budget_strict(&flags, &unknown_flags)
        .unwrap_err()
        .contains(&ChangeBudgetViolation::FeatureUnknown {
            feature: "unsafe_increase"
        }));
    // A forbidden increase is enforced only when observed Some(true):
    // Some(false) and None are both accepted by `check`.
    let not_increased = ChangeObservations {
        unsafe_increase: Some(false),
        ..Default::default()
    };
    assert!(check_change_budget(&flags, &not_increased).is_ok());
}

#[test]
fn change_budget_fact_is_bounded_and_hostile_safe() {
    let (_d, m) = test_manager();
    let s = session(&m);
    seeded_task(&s, None);
    assert_eq!(s.change_budget().unwrap(), None);
    let budget = ChangeBudget {
        allowed_paths: vec!["src".into()],
        max_blast_radius: Some(3),
        ..Default::default()
    };
    s.set_change_budget(Some(&budget)).unwrap();
    assert_eq!(s.change_budget().unwrap(), Some(budget.clone()));
    // Bounds are enforced before any write (bounded everything).
    let overlong = ChangeBudget {
        allowed_paths: vec!["p".repeat(MAX_CHANGE_BUDGET_ENTRY_CHARS + 1)],
        ..Default::default()
    };
    assert!(matches!(
        s.set_change_budget(Some(&overlong)),
        Err(crate::task::TaskError::Malformed(_))
    ));
    assert_eq!(
        s.change_budget().unwrap(),
        Some(budget),
        "a rejected budget left no trace"
    );
    // A corrupted/hostile fact is a typed refusal, never ignored.
    s.upsert_memory_fact("change_budget", "0", "{not json")
        .unwrap();
    assert!(matches!(
        s.change_budget(),
        Err(crate::task::TaskError::Malformed(_))
    ));
    // The duplicate count/serialized bound also refuses.
    let many = ChangeBudget {
        allowed_paths: (0..=MAX_CHANGE_BUDGET_ENTRIES)
            .map(|i| format!("p{i}"))
            .collect(),
        ..Default::default()
    };
    assert!(matches!(
        s.set_change_budget(Some(&many)),
        Err(crate::task::TaskError::Oversized(_))
    ));
}

/// Per-tool-loop change-budget read is a POINT lookup, not a fact-space walk:
/// 20k per-turn churn rows must not change the cost or the result.
#[test]
fn change_budget_read_is_point_addressed_under_turn_churn() {
    let (_d, m) = test_manager();
    let s = session(&m);
    for index in 0..20_000 {
        s.upsert_memory_fact("taskexec_run", &format!("churn-{index:05}"), "{}")
            .unwrap();
    }
    let budget = ChangeBudget {
        allowed_paths: vec!["src".into()],
        ..Default::default()
    };
    s.set_change_budget(Some(&budget)).unwrap();
    let started = std::time::Instant::now();
    for _ in 0..1_000 {
        assert_eq!(s.change_budget().unwrap().as_ref(), Some(&budget));
    }
    let elapsed = started.elapsed();
    eprintln!("1000 change_budget reads with 20k churn: {elapsed:?}");
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "change budget paid for unrelated fact churn: {elapsed:?}"
    );
}

/// Every durable cost-cap entry point enforces the SAME domain: a real cap
/// of 1 microUSD is accepted and admits exactly 1 micro, while 0 (ambiguous
/// with the store's unlimited NULL/0), a value above `i64::MAX` (which the
/// signed task column would silently clamp) and fractional/negative wire
/// forms are refused typed. The child-cap entry point validates the same
/// domain before it touches any child row.
#[tokio::test]
async fn every_cost_cap_entry_refuses_zero_and_over_domain_and_enforces_one_micro() {
    let (_d, m, ledger) = fresh_ledger();
    let s = session(&m);
    let task = seeded_task(&s, Some(1));
    // A 1-micro cap admits exactly 1 micro and refuses the next paid call.
    let r = ledger
        .reserve(
            s.id,
            task,
            OpId::new(700),
            1,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    let err = ledger
        .reserve(
            s.id,
            task,
            OpId::new(701),
            1,
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap_err();
    assert_eq!(
        err,
        BudgetError::BudgetExceeded {
            free: 0,
            predicted: 1,
        }
    );
    ledger
        .settle_usage(s.id, r, 0, 0, 0, 0, Some(1), None)
        .await
        .unwrap();
    assert_eq!(ledger.view(s.id, task).spent_cost_micro, 1);

    // Zero is not "clear" and not "unlimited": the typed Malformed names the
    // ambiguity; `None` is the only clear form.
    let zero = ledger.set_task_max_cost(s.id, task, Some(0)).unwrap_err();
    assert!(
        matches!(zero, BudgetError::Malformed(ref m) if m.contains(">= 1")),
        "{zero:?}"
    );
    ledger.set_task_max_cost(s.id, task, None).unwrap();
    assert_eq!(ledger.view(s.id, task).max_cost_micro, None);

    // The durable maximum is i64::MAX; a larger cap is refused, never
    // clamped, and the boundary itself is accepted.
    let over = ledger
        .set_task_max_cost(s.id, task, Some(i64::MAX as u64 + 1))
        .unwrap_err();
    assert!(
        matches!(over, BudgetError::Malformed(ref m) if m.contains("i64::MAX")),
        "{over:?}"
    );
    ledger
        .set_task_max_cost(s.id, task, Some(i64::MAX as u64))
        .unwrap();
    assert_eq!(
        ledger.view(s.id, task).max_cost_micro,
        Some(i64::MAX as u64)
    );
    ledger.set_task_max_cost(s.id, task, None).unwrap();
}

/// The cap domain is enforced BEFORE a child scope is seeded or enrolled, so
/// a refused change cannot leave a half-written child row behind. A valid
/// cap proceeds to the ordinary "not an orchestrated child" refusal.
#[tokio::test]
async fn child_cost_cap_entry_validates_the_domain_before_any_child_effect() {
    let (_d, m, ledger) = fresh_ledger();
    let s = session(&m);
    for bad in [Some(0u64), Some(i64::MAX as u64 + 1)] {
        let err = ledger
            .change_child_scope_cap(s.id, "child-x", bad)
            .unwrap_err();
        assert!(matches!(err, BudgetError::Malformed(_)), "{bad:?}: {err:?}");
    }
    let err = ledger
        .change_child_scope_cap(s.id, "child-x", Some(1))
        .unwrap_err();
    assert!(
        matches!(err, BudgetError::NotAnOrchestratedChild(_)),
        "a valid cap reaches the real child-identity check: {err:?}"
    );
    assert!(
        ledger.scope_of(s.id).unwrap().is_none(),
        "refused caps enroll nothing"
    );
}

/// Micro-unit rounding loop: a 1-microUSD-per-million price over 1 token
/// still costs 1 microUSD (never a free zero), every remainder rounds UP
/// once, `i64::MAX` tokens stays exact, and repeated halving never
/// understates or overstates the independent ceiling formula.
#[tokio::test]
async fn settlement_rounding_loop_never_truncates_to_zero_or_understates() {
    use faktor_core::model::{MicroUsdPerMillionTokens, PriceQuote};
    let (_d, m, ledger) = fresh_ledger();
    let s = session(&m);
    let tiny = PriceQuote {
        input: MicroUsdPerMillionTokens(1),
        output: MicroUsdPerMillionTokens(1),
        cache_read: MicroUsdPerMillionTokens(1),
        cache_write: MicroUsdPerMillionTokens(1),
    };
    let snapshot = PricingSnapshot::exact(tiny, 1, "tiny-1ppm".into());
    let task = seeded_task(&s, Some(i64::MAX as u64));
    // The ceiling oracle is independent of the SUT: ceil(tokens * 1 / 1e6).
    let expect =
        |tokens: u64| -> u64 { u64::try_from((tokens as u128).div_ceil(1_000_000)).unwrap() };
    let mut cases = vec![1u64, 2, 999_999, 1_000_000, 1_000_001];
    let mut halving = i64::MAX as u64 / 4;
    while halving > 0 {
        cases.push(halving);
        halving /= 2;
    }
    let mut total = 0u64;
    for (i, tokens) in cases.into_iter().enumerate() {
        let r = ledger
            .reserve(
                s.id,
                task,
                OpId::new(800 + i as u64),
                1,
                Some(snapshot.clone()),
                faktor_core::model::ExecutionClass::Unknown,
            )
            .await
            .unwrap();
        let actual = ledger
            .settle_usage(s.id, r, tokens, 0, 0, 0, None, None)
            .await
            .unwrap()
            .expect("an exact quote always settles a number");
        let want = expect(tokens);
        assert_eq!(actual, want, "tokens={tokens} must round up exactly once");
        if tokens > 0 {
            assert!(
                actual >= 1,
                "positive usage at a positive price is never free"
            );
        }
        total = total.checked_add(want).expect("case sum fits u64");
        assert_eq!(ledger.view(s.id, task).spent_cost_micro, total);
    }
    // The hostile maximum fits the u64 aggregate and its own i64::MAX cap.
    let r = ledger
        .reserve(
            s.id,
            task,
            OpId::new(900),
            1,
            Some(snapshot),
            faktor_core::model::ExecutionClass::Unknown,
        )
        .await
        .unwrap();
    let actual = ledger
        .settle_usage(s.id, r, i64::MAX as u64, 0, 0, 0, None, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(actual, expect(i64::MAX as u64));
    total = total.checked_add(actual).unwrap();
    assert_eq!(ledger.view(s.id, task).spent_cost_micro, total);
}

/// Crash loop between reservation and settlement: a dispatched-but-unsettled
/// reservation is never lost (it survives reopen), recovery never charges it
/// twice (it becomes UNCERTAIN and keeps consuming free), and the completion
/// finalize charges the estimate exactly once. A never-dispatched sibling is
/// refunded, never charged.
#[tokio::test]
async fn crash_recovery_loop_never_double_charges_and_never_loses_a_reservation() {
    for round in 0..12u64 {
        let dir = tempfile::tempdir().unwrap();
        let amount = 100 + round * 7;
        let (sid, task, op_base);
        {
            let m =
                crate::SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
                    .unwrap();
            let s = session(&m);
            let ledger = DurableBudgetLedger::new(m.clone());
            task = seeded_task(&s, Some(10_000));
            sid = s.id;
            op_base = 1_000 + round * 10;
            let dispatched = ledger
                .reserve(
                    sid,
                    task,
                    OpId::new(op_base),
                    amount,
                    None,
                    faktor_core::model::ExecutionClass::Unknown,
                )
                .await
                .unwrap();
            ledger.mark_dispatched(sid, dispatched).await.unwrap();
            let never_sent = ledger
                .reserve(
                    sid,
                    task,
                    OpId::new(op_base + 1),
                    amount,
                    None,
                    faktor_core::model::ExecutionClass::Unknown,
                )
                .await
                .unwrap();
            assert!(never_sent.raw() > 0);
            // "Crash": both rows stay open on disk.
        }
        let m2 =
            crate::SessionManager::open(dir.path().join("store"), dir.path().join("cas"), true)
                .unwrap();
        let ledger2 = DurableBudgetLedger::new(m2.clone());
        // Nothing is lost before recovery: both rows are still present and
        // still hold their money.
        let before = ledger2.view(sid, task);
        assert_eq!(before.open_reservations, 2, "round {round}: rows survived");
        assert_eq!(before.open_reserved_micro, amount * 2);
        ledger2.recover_after_restart();
        let after = ledger2.view(sid, task);
        assert_eq!(after.open_reservations, 0);
        assert_eq!(after.uncertain_reservations, 1);
        assert_eq!(after.uncertain_reserved_micro, amount);
        assert_eq!(
            after.spent_cost_micro, 0,
            "recovery never charges a may-have-billed row"
        );
        // A second idempotent recovery changes nothing.
        ledger2.recover_after_restart();
        assert_eq!(ledger2.view(sid, task).spent_cost_micro, 0);
        let report = ledger2.finalize_uncertain(sid, task).await.unwrap();
        assert_eq!(report.settled, 1);
        assert_eq!(report.charged_micro, amount);
        let view = ledger2.view(sid, task);
        assert_eq!(view.spent_cost_micro, amount, "charged exactly once");
        assert_eq!(view.uncertain_reservations, 0);
        // A repeated finalize is a no-op: never a double charge.
        let again = ledger2.finalize_uncertain(sid, task).await.unwrap();
        assert_eq!(again.settled, 0);
        assert_eq!(again.charged_micro, 0);
        assert_eq!(ledger2.view(sid, task).spent_cost_micro, amount);
        let rows = ledger2.reservations_of(sid, task, 10).unwrap();
        assert_eq!(rows.len(), 2, "neither reservation was lost");
        assert_eq!(
            rows.iter().filter(|r| r.status == "refunded").count(),
            1,
            "the never-dispatched sibling is refunded"
        );
    }
}

// ------------------- spawn-time cost-scope enrollment (audit 9/H wiring)
//
// The reported hole: orchestrated children reserved against a NULL child
// cap (unlimited), so a run-level cap only bounded the root row. The ledger
// enrollment is the fix the orchestrator now calls for every child.

#[tokio::test]
async fn spawn_enrollment_makes_children_consume_a_finite_root_cap() {
    let (_d, m, ledger) = fresh_ledger();
    let root = session(&m);
    let root_task = seeded_task(&root, Some(10_000_000));
    ledger
        .set_task_max_cost(root.id, root_task, Some(10_000_000))
        .unwrap();
    let a = child_session(&m, root.id, "a");
    let a_task = a.task_id().unwrap();
    // The hole before enrollment: no child row cap, no root enrollment.
    assert!(m.store().cost_task_row(a.id, a_task).unwrap().is_none());
    assert!(ledger.scope_of(a.id).unwrap().is_none());

    ledger
        .enroll_child_in_root_scope(a.id, "child-a", None)
        .unwrap();

    let row = m
        .store()
        .cost_task_row(a.id, a_task)
        .unwrap()
        .expect("enrollment seeds the child task row");
    assert_eq!(
        row.max_cost_micro,
        Some(10_000_000),
        "the child must inherit the finite run cap, never a NULL (unlimited) cap"
    );
    let scope = ledger
        .scope_of(a.id)
        .unwrap()
        .expect("root subscope enrollment");
    assert_eq!(scope.root_session_id, root.id);
    assert_eq!(scope.consumer_session_id, a.id);
    assert_eq!(scope.consumer_task_id, a_task);
    assert_eq!(scope.child_cap_micro, Some(10_000_000));

    // Re-attach / crash resume runs the same enrollment again: idempotent.
    ledger
        .enroll_child_in_root_scope(a.id, "child-a", None)
        .unwrap();
    assert_eq!(
        m.store()
            .cost_task_row(a.id, a_task)
            .unwrap()
            .unwrap()
            .max_cost_micro,
        Some(10_000_000)
    );
}

#[tokio::test]
async fn an_unlimited_root_enrolls_nothing_and_children_stay_null() {
    let (_d, m, ledger) = fresh_ledger();
    let root = session(&m);
    let root_task = seeded_task(&root, None);
    ledger.set_task_max_cost(root.id, root_task, None).unwrap();
    let a = child_session(&m, root.id, "a");
    let a_task = a.task_id().unwrap();

    ledger
        .enroll_child_in_root_scope(a.id, "child-a", None)
        .unwrap();

    assert!(
        m.store().cost_task_row(a.id, a_task).unwrap().is_none(),
        "an unlimited run has nothing to bound; no child row is written"
    );
    assert!(ledger.scope_of(a.id).unwrap().is_none());
}

#[tokio::test]
async fn explicit_child_cap_wins_and_non_children_are_documented_no_ops() {
    let (_d, m, ledger) = fresh_ledger();
    let root = session(&m);
    let root_task = seeded_task(&root, Some(10_000_000));
    ledger
        .set_task_max_cost(root.id, root_task, Some(10_000_000))
        .unwrap();
    let a = child_session(&m, root.id, "a");
    let a_task = a.task_id().unwrap();

    ledger
        .enroll_child_in_root_scope(a.id, "child-a", Some(3_000_000))
        .unwrap();
    assert_eq!(
        m.store()
            .cost_task_row(a.id, a_task)
            .unwrap()
            .unwrap()
            .max_cost_micro,
        Some(3_000_000),
        "an explicit per-child budget overrides the inherited root cap"
    );
    assert_eq!(
        ledger.scope_of(a.id).unwrap().unwrap().child_cap_micro,
        Some(3_000_000)
    );

    // A session without an orchestrated child identity has no root scope:
    // documented no-op, never a refusal, and it must not enroll itself.
    ledger
        .enroll_child_in_root_scope(root.id, "root-as-child", None)
        .unwrap();
    assert!(ledger.scope_of(root.id).unwrap().is_none());

    // Hostile child id: the same bounded validation as the direct API.
    let too_long = "x".repeat(crate::budget::MAX_SCOPE_CHILD_ID_CHARS + 1);
    let err = ledger
        .enroll_child_in_root_scope(a.id, &too_long, None)
        .unwrap_err();
    assert!(
        matches!(err, crate::budget::BudgetError::Malformed(_)),
        "{err}"
    );
}
