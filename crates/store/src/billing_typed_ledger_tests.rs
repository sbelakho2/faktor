use super::*;

pub(crate) fn seed_task(
    store: &Store,
    session_id: SessionId,
    task_id: TaskId,
    criteria: Vec<String>,
    state: TaskState,
) -> TaskRow {
    let row = TaskRow {
        task_id,
        session_id,
        goal: "g".into(),
        acceptance_criteria: criteria,
        plan: vec![],
        attachments: vec![],
        contract: Default::default(),
        max_tokens: None,
        max_turns: None,
        spent_tokens: 0,
        spent_turns: 0,
        state,
        revision: TaskRevision::new(1),
        created_ms: 1,
        updated_ms: 1,
    };
    store.upsert_task(&row).unwrap();
    row
}

/// Seed a task and walk the machine into `Verifying` through the legal
/// store edges (a row can never be created completion-relevant).
pub(crate) fn seed_verifying(
    store: &Store,
    session_id: SessionId,
    task_id: TaskId,
    criteria: Vec<String>,
) -> TaskRow {
    let mut row = seed_task(store, session_id, task_id, criteria, TaskState::Pending);
    let mut bump = |state: TaskState| {
        row.state = state;
        row.revision = row.revision.checked_next().unwrap();
        store.upsert_task(&row).unwrap();
    };
    bump(TaskState::Running);
    bump(TaskState::NeedsVerification);
    bump(TaskState::Verifying);
    row
}

pub(crate) fn passing_record(
    task: &TaskRow,
    ws: WorkspaceId,
    wt: WorktreeId,
) -> VerificationRecordRow {
    VerificationRecordRow {
        id: VerificationRecordId::new(1),
        task_id: task.task_id,
        revision: task.revision,
        workspace_id: ws,
        worktree_id: wt,
        tree_hash: None,
        criteria: task
            .acceptance_criteria
            .iter()
            .map(|c| CriterionVerification {
                criterion_key: c.clone(),
                passed: true,
                evidence: Some("exit 0".into()),
                binding: None,
            })
            .collect(),
        checks: vec![],
        changed_files: vec![],
        unrelated_changes: vec![],
        reviewer: None,
        status: VerificationStatus::Passed,
        started_ms: 1,
        completed_ms: None,
    }
}

#[test]
fn completion_vs_reserve_race_never_completes_with_held_reservations() {
    // Adversarial: 1,000 barrier-controlled races of the completion
    // sequence (Running -> NeedsVerification -> Verifying then the
    // exclusive completion transaction) against a concurrent reservation
    // on the SAME task. The race is real: a reserve is admitted while the
    // task is still Running and the completion finds the row in its
    // in-transaction COUNT; or the completion lands first and the late
    // reserve is refused by the task-state gate. The two can never both
    // commit. After EVERY race the invariant is checked directly: a task
    // row that reads VerifiedComplete has zero reserved, zero dispatched
    // and zero uncertain reservation rows; a reserve that landed first
    // leaves the task Verifying with exactly its held row and a typed
    // ReservationsHeld refusal.
    const RACES: usize = 1_000;
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(dir.path(), true).unwrap());
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "race", "p", "m").unwrap();
    let sid = s.id;
    let mut reserve_wins = 0usize;
    let mut complete_wins = 0usize;
    for i in 0..RACES {
        let task_id = TaskId::new(i as u64 + 1);
        // Seed Pending (rev 1) -> Running (rev 2). The completion side
        // then walks Running -> NeedsVerification (rev 3) -> Verifying
        // (rev 4) before its exclusive completion.
        let task = seed_task(&store, sid, task_id, vec![], TaskState::Pending);
        let mut running = task.clone();
        running.state = TaskState::Running;
        running.revision = running.revision.checked_next().unwrap();
        store.upsert_task(&running).unwrap();
        // The passing record certifies the revision the completion side
        // will present (rev 4) — the record can exist before the task
        // reaches Verifying (the completion transaction checks both).
        let verifying_revision = TaskRevision::new(4);
        let record_task = TaskRow {
            revision: verifying_revision,
            ..running.clone()
        };
        let record = passing_record(&record_task, ws, WorktreeId::new(1));
        let rec_id = store.verification_record_put(&record).unwrap();
        let barrier = std::sync::Barrier::new(2);
        // The two racers leave the barrier together. Three fifths of the
        // races run free (true writer-lock arbitration); one fifth gives
        // each side a 1ms head start so BOTH orderings are exercised on
        // every host (the OS wake order alone is not a fair scheduler).
        let fork = i % 5;
        let do_reserve = || {
            barrier.wait();
            if fork == 3 {
                // Completion head start: reserve-first must not be the
                // only ordering this host can produce.
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            store.cost_reserve_priced(
                sid,
                task_id,
                OpId::new(i as u64 + 1),
                100,
                now_ms(),
                None,
                ExecutionClass::Unknown,
            )
        };
        let do_complete = || {
            barrier.wait();
            if fork == 4 {
                // Reserve head start.
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            let mut row = store.get_task(sid, task_id).unwrap().unwrap();
            row.state = TaskState::NeedsVerification;
            row.revision = row.revision.checked_next().unwrap();
            store.upsert_task(&row).unwrap();
            row.state = TaskState::Verifying;
            row.revision = row.revision.checked_next().unwrap();
            store.upsert_task(&row).unwrap();
            store.task_complete_verified(sid, task_id, row.revision, rec_id, now_ms())
        };
        let (reserve_out, complete_out) = std::thread::scope(|scope| {
            let reserve = scope.spawn(do_reserve);
            let complete = scope.spawn(do_complete);
            (reserve.join().unwrap(), complete.join().unwrap())
        });
        let rows = store.cost_reservations_of(sid, task_id, i64::MAX).unwrap();
        let count = |status: &str| rows.iter().filter(|r| r.status == status).count();
        // The outer Result is the store call itself; the inner one is the
        // typed completion refusal (proof/revision/accounting gate).
        match (reserve_out, complete_out.unwrap()) {
            (Ok(CostReserveOutcome::Granted(_)), Ok(_)) => {
                panic!("race {i}: reserve AND completion both committed")
            }
            (Ok(CostReserveOutcome::Granted(_)), Err(refusal)) => {
                assert!(
                    matches!(
                        refusal,
                        TaskCompletionRefusal::ReservationsHeld {
                            reserved: 1,
                            dispatched: 0,
                            uncertain: 0,
                            reserved_micro: 100,
                            ..
                        }
                    ),
                    "race {i}: completion must name the held row: {refusal:?}"
                );
                assert_eq!(count("reserved"), 1);
                assert_eq!(
                    store.get_task(sid, task_id).unwrap().unwrap().state,
                    TaskState::Verifying,
                    "race {i}: a refused completion never transitions"
                );
                reserve_wins += 1;
            }
            (Ok(CostReserveOutcome::Exceeded { .. }), _) => {
                panic!("race {i}: an uncapped task refused a reserve")
            }
            (Err(reserve_err), Ok(_)) => {
                // Completion won the write lock: the reserve must have
                // been refused by the task-state gate, writing nothing.
                assert!(
                    reserve_err.to_string().contains("cost reserve: task state"),
                    "race {i}: a reserve after completion must refuse typed on state: \
                     {reserve_err}"
                );
                assert_eq!(
                    store.get_task(sid, task_id).unwrap().unwrap().state,
                    TaskState::VerifiedComplete
                );
                assert_eq!(count("reserved"), 0, "race {i}: reserved must be 0");
                assert_eq!(count("dispatched"), 0, "race {i}: dispatched must be 0");
                assert_eq!(count("uncertain"), 0, "race {i}: uncertain must be 0");
                complete_wins += 1;
            }
            (Err(reserve_err), Err(refusal)) => {
                panic!("race {i}: neither side committed ({reserve_err} / {refusal:?})")
            }
        }
    }
    eprintln!(
        "completion-vs-reserve races: reserve-first {reserve_wins}, completion-first {complete_wins}"
    );
    assert!(
        reserve_wins > 0,
        "the race never exercised the reserve-first ordering"
    );
    assert!(
        complete_wins > 0,
        "the race never exercised the completion-first ordering"
    );
}

#[test]
fn reserve_refused_once_the_task_permits_no_new_provider_operation() {
    // The reserve transaction's task-state condition: completion-relevant
    // and terminal states refuse typed and write NOTHING; the remaining
    // machine states admit. VerifiedComplete is produced through the
    // completion path (raw row writes cannot mint it).
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    // Permitted: every state that still allows new provider work.
    for (n, state) in [
        TaskState::Pending,
        TaskState::Planning,
        TaskState::Running,
        TaskState::Waiting,
        TaskState::Blocked,
    ]
    .into_iter()
    .enumerate()
    {
        let task_id = TaskId::new(n as u64 + 1);
        seed_task(&store, s.id, task_id, vec![], state);
        let out = store
            .cost_reserve_priced(
                s.id,
                task_id,
                OpId::new(n as u64 + 1),
                1,
                now_ms(),
                None,
                ExecutionClass::Unknown,
            )
            .unwrap();
        assert!(
            matches!(out, CostReserveOutcome::Granted(_)),
            "{state:?} must permit a reserve: {out:?}"
        );
    }
    // Refused: completion-relevant and terminal states.
    let mut n = 100u64;
    for state in [
        TaskState::NeedsVerification,
        TaskState::Verifying,
        TaskState::Failed,
        TaskState::Cancelled,
    ] {
        n += 1;
        let task_id = TaskId::new(n);
        if state == TaskState::NeedsVerification {
            // NeedsVerification is completion-relevant: walk the legal
            // machine edge out of Verifying (never a raw seed).
            let mut row = seed_verifying(&store, s.id, task_id, vec![]);
            row.state = TaskState::NeedsVerification;
            row.revision = row.revision.checked_next().unwrap();
            store.upsert_task(&row).unwrap();
        } else if state == TaskState::Verifying {
            seed_verifying(&store, s.id, task_id, vec![]);
        } else {
            seed_task(&store, s.id, task_id, vec![], state);
        }
        let err = store
            .cost_reserve_priced(
                s.id,
                task_id,
                OpId::new(n),
                1,
                now_ms(),
                None,
                ExecutionClass::Unknown,
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("cost reserve: task state"),
            "{state:?} must refuse typed: {err}"
        );
        assert!(
            store
                .cost_reservations_of(s.id, task_id, 10)
                .unwrap()
                .is_empty(),
            "{state:?}: a refused reserve wrote a row"
        );
    }
    // VerifiedComplete (only the completion transaction may produce it).
    let task_id = TaskId::new(200);
    let task = seed_verifying(&store, s.id, task_id, vec![]);
    let record = passing_record(&task, ws, WorktreeId::new(1));
    let rec_id = store.verification_record_put(&record).unwrap();
    store
        .task_complete_verified(s.id, task_id, task.revision, rec_id, now_ms())
        .unwrap()
        .unwrap();
    let err = store
        .cost_reserve_priced(
            s.id,
            task_id,
            OpId::new(200),
            1,
            now_ms(),
            None,
            ExecutionClass::Unknown,
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("cost reserve: task state"),
        "VerifiedComplete must refuse typed: {err}"
    );
    assert!(store
        .cost_reservations_of(s.id, task_id, 10)
        .unwrap()
        .is_empty());
}

#[test]
fn corrupt_task_state_fails_reserve_and_completion_typed_without_writes() {
    // SQL failure mode: the task row's state column cannot be parsed.
    // Both the reservation transaction and the completion transaction
    // must fail TYPED before writing anything (no partial transition, no
    // reservation row); healing the row restores both paths — the error
    // was the corruption, not a poisoned connection.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    let task = seed_verifying(&store, s.id, TaskId::new(1), vec![]);
    let record = passing_record(&task, ws, WorktreeId::new(1));
    let rec_id = store.verification_record_put(&record).unwrap();
    store
        .raw_conn()
        .execute(
            "UPDATE task SET state = '{not-a-state}' WHERE session_id = ?1 AND task_id = ?2",
            params![s.id.raw() as i64, task.task_id.raw() as i64],
        )
        .unwrap();
    let err = store
        .cost_reserve_priced(
            s.id,
            task.task_id,
            OpId::new(9),
            1,
            now_ms(),
            None,
            ExecutionClass::Unknown,
        )
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Corrupt(_)),
        "corrupt state must be a typed parse failure: {err}"
    );
    assert!(store
        .cost_reservations_of(s.id, task.task_id, 10)
        .unwrap()
        .is_empty());
    let err = store
        .task_complete_verified(s.id, task.task_id, task.revision, rec_id, now_ms())
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Corrupt(_)),
        "corrupt state must fail completion typed: {err}"
    );
    let raw: (String, i64) = store
        .raw_conn()
        .query_row(
            "SELECT state, revision FROM task WHERE session_id = ?1 AND task_id = ?2",
            params![s.id.raw() as i64, task.task_id.raw() as i64],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(raw.0, "{not-a-state}", "the corrupt marker is untouched");
    assert_eq!(
        raw.1,
        task.revision.raw() as i64,
        "the failed completion transaction rolled back whole"
    );
    // Heal the row behind the API's back (a state that permits new
    // provider work): the reserve path works again — the failure was
    // the corruption, not a poisoned connection.
    store
        .raw_conn()
        .execute(
            "UPDATE task SET state = '\"running\"' WHERE session_id = ?1 AND task_id = ?2",
            params![s.id.raw() as i64, task.task_id.raw() as i64],
        )
        .unwrap();
    assert!(matches!(
        store
            .cost_reserve_priced(
                s.id,
                task.task_id,
                OpId::new(10),
                1,
                now_ms(),
                None,
                ExecutionClass::Unknown
            )
            .unwrap(),
        CostReserveOutcome::Granted(_)
    ));
}

/// The v34 route-time execution class is durable and authority-derived:
/// a tier persists and round-trips exactly, `Unknown` persists as NULL
/// (no class authority — never a fabricated tag), and an unrecognized
/// durable tag is a typed corruption instead of a guessed class.
#[test]
fn execution_class_round_trips_and_a_foreign_tag_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    let snapshot = known_snapshot_json();
    // A known hosted tier round-trips beside its frozen snapshot.
    let task_id = TaskId::new(1);
    seed_task(&store, s.id, task_id, vec![], TaskState::Running);
    let out = store
        .cost_reserve_priced(
            s.id,
            task_id,
            OpId::new(1),
            100,
            now_ms(),
            Some(&snapshot),
            ExecutionClass::Frontier,
        )
        .unwrap();
    assert!(matches!(out, CostReserveOutcome::Granted(_)));
    let rows = store.cost_reservations_of(s.id, task_id, 10).unwrap();
    assert_eq!(
        rows[0].execution_class,
        ExecutionClass::Frontier,
        "the durable tier round-trips exactly"
    );
    // Unknown persists as NULL (no class authority) and reads as Unknown.
    let task_id = TaskId::new(2);
    seed_task(&store, s.id, task_id, vec![], TaskState::Running);
    store
        .cost_reserve_priced(
            s.id,
            task_id,
            OpId::new(2),
            100,
            now_ms(),
            None,
            ExecutionClass::Unknown,
        )
        .unwrap();
    let rows = store.cost_reservations_of(s.id, task_id, 10).unwrap();
    assert_eq!(rows[0].execution_class, ExecutionClass::Unknown);
    let raw: Option<String> = store
        .raw_conn()
        .query_row(
            "SELECT execution_class FROM cost_reservation
             WHERE session_id = ?1 AND task_id = ?2",
            params![s.id.raw() as i64, task_id.raw() as i64],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        raw.is_none(),
        "no class authority must persist NULL, never a fabricated tag: {raw:?}"
    );
    // A foreign tag is a typed corruption, never a guessed class.
    store
        .raw_conn()
        .execute(
            "UPDATE cost_reservation SET execution_class = 'free!'
             WHERE session_id = ?1 AND task_id = ?2",
            params![s.id.raw() as i64, task_id.raw() as i64],
        )
        .unwrap();
    let err = store.cost_reservations_of(s.id, task_id, 10).unwrap_err();
    assert!(
        matches!(err, StoreError::Corrupt(_)),
        "an unknown durable class tag must fail closed: {err}"
    );
}

pub(crate) fn known_snapshot_json() -> String {
    use faktor_core::model::{MicroUsdPerMillionTokens, PriceQuote};
    serde_json::to_string(&PricingSnapshot::exact(
        PriceQuote {
            input: MicroUsdPerMillionTokens(15_000_000),
            output: MicroUsdPerMillionTokens(60_000_000),
            cache_read: MicroUsdPerMillionTokens(3_000_000),
            cache_write: MicroUsdPerMillionTokens(7_000_000),
        },
        7,
        "store-test".into(),
    ))
    .unwrap()
}

/// Free budget of one task = cap - spent - holding predictions
/// (reserved + dispatched + uncertain), the store's own formula.
pub(crate) fn free_micro(store: &Store, session: SessionId, task: TaskId) -> u64 {
    let row = store.cost_task_row(session, task).unwrap().unwrap();
    let max = row.max_cost_micro.unwrap_or(0);
    let rows = store.cost_reservations_of(session, task, i64::MAX).unwrap();
    let held: u64 = rows
        .iter()
        .filter(|r| matches!(r.status.as_str(), "reserved" | "dispatched" | "uncertain"))
        .map(|r| r.predicted_micro)
        .sum();
    max.saturating_sub(row.spent_cost_micro)
        .saturating_sub(held)
}

#[test]
fn refund_is_sql_guarded_after_dispatch_and_after_terminal_states() {
    // (i) The hardened refund: pre-dispatch refunds work; every
    // post-dispatch or terminal state leaves the row UNTOUCHED with the
    // free budget unchanged — enforced by the guarded UPDATE, so even a
    // mis-calling runtime can never free a dispatched reservation.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    let task = seed_task(&store, s.id, TaskId::new(1), vec![], TaskState::Running);
    let tid = task.task_id;
    store.cost_task_cap_set(s.id, tid, Some(1_000)).unwrap();
    let now = now_ms();

    // Refund BEFORE dispatch: applied, money free again.
    let CostReserveOutcome::Granted(r) = store
        .cost_reserve(s.id, tid, OpId::new(1), 400, now)
        .unwrap()
    else {
        panic!("reserve 1 granted")
    };
    assert_eq!(free_micro(&store, s.id, tid), 600);
    assert_eq!(store.cost_refund(r, now).unwrap(), RefundOutcome::Applied);
    assert_eq!(free_micro(&store, s.id, tid), 1_000, "refund frees money");

    // Refund AFTER dispatch: the guarded UPDATE changes zero rows.
    let CostReserveOutcome::Granted(r2) = store
        .cost_reserve(s.id, tid, OpId::new(2), 400, now)
        .unwrap()
    else {
        panic!("reserve 2 granted")
    };
    store.cost_mark_dispatched(r2, now).unwrap();
    assert_eq!(free_micro(&store, s.id, tid), 600);
    let out = store.cost_refund(r2, now + 1).unwrap();
    assert_eq!(
        out,
        RefundOutcome::Blocked {
            current: "dispatched".into(),
            dispatched_ms: Some(now)
        },
        "the refund of a dispatched row is refused with its row truth"
    );
    let rows = store.cost_reservations_of(s.id, tid, 10).unwrap();
    assert_eq!(rows[0].status, "dispatched", "row untouched");
    assert_eq!(rows[0].dispatched_ms, Some(now));
    assert_eq!(free_micro(&store, s.id, tid), 600, "free unchanged");

    // Refund after SETTLE and after REFUND: typed refusals, untouched.
    store
        .cost_settle(r2, 400, Some(400), Some(390), None, now + 2)
        .unwrap();
    let out = store.cost_refund(r2, now + 3).unwrap();
    assert!(matches!(out, RefundOutcome::Blocked { current, .. } if current == "settled"));
    let CostReserveOutcome::Granted(r3) = store
        .cost_reserve(s.id, tid, OpId::new(3), 100, now)
        .unwrap()
    else {
        panic!("reserve 3 granted")
    };
    assert_eq!(
        store.cost_refund(r3, now + 1).unwrap(),
        RefundOutcome::Applied
    );
    let out = store.cost_refund(r3, now + 2).unwrap();
    assert!(matches!(out, RefundOutcome::Blocked { current, .. } if current == "refunded"));
    // Missing reservation.
    assert_eq!(
        store.cost_refund(99_999, now).unwrap(),
        RefundOutcome::Missing
    );
}

#[test]
fn crash_recovery_splits_reserved_from_dispatched_and_uncertain_consumes_free() {
    // (iv) Crash windows: a never-dispatched reservation refunds and
    // restores free; a dispatched one (marker written) becomes UNCERTAIN
    // and KEEPS consuming free.
    let dir = tempfile::tempdir().unwrap();
    let (sid, tid) = {
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let task = seed_task(&store, s.id, TaskId::new(1), vec![], TaskState::Running);
        let tid = task.task_id;
        store.cost_task_cap_set(s.id, tid, Some(1_000)).unwrap();
        let now = now_ms();
        // (a) reserved: dispatch never began.
        let CostReserveOutcome::Granted(_pre) = store
            .cost_reserve(s.id, tid, OpId::new(1), 200, now)
            .unwrap()
        else {
            panic!("pre-marker reserve")
        };
        // (b) dispatched: the request left the process.
        let CostReserveOutcome::Granted(post) = store
            .cost_reserve(s.id, tid, OpId::new(2), 300, now)
            .unwrap()
        else {
            panic!("post-marker reserve")
        };
        store.cost_mark_dispatched(post, now).unwrap();
        (s.id, tid)
    };
    let store = Store::open(dir.path(), true).unwrap();
    let (refunded, uncertain) = store.cost_recover_open_reservations(now_ms()).unwrap();
    assert_eq!(refunded, 1, "the reserved pre-marker row refunds");
    assert_eq!(uncertain, 1, "the dispatched row goes UNCERTAIN");
    let rows = store.cost_reservations_of(sid, tid, 10).unwrap();
    let by_op = |op: u64| rows.iter().find(|r| r.op_id == OpId::new(op)).unwrap();
    assert_eq!(by_op(1).status, "refunded");
    assert_eq!(by_op(1).dispatched_ms, None);
    assert_eq!(by_op(2).status, "uncertain");
    assert!(by_op(2).dispatched_ms.is_some(), "the marker survives");
    assert_eq!(
        by_op(2).failure_reason_code.as_deref(),
        Some("crash_recovery_post_dispatch_marker")
    );
    assert_eq!(
        free_micro(&store, sid, tid),
        700,
        "the refunded prediction is free again; the uncertain hold (300) consumes"
    );
    // Idempotent recovery.
    let (refunded, uncertain) = store.cost_recover_open_reservations(now_ms()).unwrap();
    assert_eq!((refunded, uncertain), (0, 0));
}

#[test]
fn settle_records_an_honest_cost_basis_and_amounts() {
    // (vi) Settlement writes provider_reported_cost_micro /
    // estimated_cost_micro / settled_cost_micro / cost_basis so the
    // ledger reports HOW the settled number was arrived at:
    // ProviderReported when the provider billed, RouteSnapshotEstimate
    // when categories x the frozen snapshot won, Unknown when nothing
    // was folded.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    let task = seed_task(&store, s.id, TaskId::new(1), vec![], TaskState::Running);
    let tid = task.task_id;
    let now = now_ms();
    // Provider-reported wins: basis ProviderReported.
    let CostReserveOutcome::Granted(r1) = store
        .cost_reserve_priced(
            s.id,
            tid,
            OpId::new(1),
            100_000,
            now,
            Some(&known_snapshot_json()),
            ExecutionClass::Unknown,
        )
        .unwrap()
    else {
        panic!("reserve r1")
    };
    store.cost_mark_dispatched(r1, now).unwrap();
    store
        .cost_settle_usage(r1, 100_000, 0, 0, 2_000, Some(9_999), None, now + 1)
        .unwrap();
    let row = store.cost_reservations_of(s.id, tid, 10).unwrap()[0].clone();
    assert_eq!(row.status, "settled");
    assert_eq!(row.provider_reported_cost_micro, Some(9_999));
    assert_eq!(row.provider_reported_micro, Some(9_999));
    assert_eq!(
        row.provider_cost_micro,
        Some(1_620_000),
        "local 100k@15+2k@60"
    );
    assert_eq!(row.settled_cost_micro, Some(9_999), "the folded amount");
    assert_eq!(row.cost_basis.as_deref(), Some("ProviderReported"));
    assert_eq!(row.estimated_cost_micro, Some(100_000));
    assert_eq!(row.delivery_state.as_deref(), Some("completed"));
    assert_eq!(
        store
            .cost_task_row(s.id, tid)
            .unwrap()
            .unwrap()
            .spent_cost_micro,
        9_999
    );

    // No provider report: categories x the frozen snapshot, basis
    // RouteSnapshotEstimate.
    let CostReserveOutcome::Granted(r2) = store
        .cost_reserve_priced(
            s.id,
            tid,
            OpId::new(2),
            2_000_000,
            now,
            Some(&known_snapshot_json()),
            ExecutionClass::Unknown,
        )
        .unwrap()
    else {
        panic!("reserve r2")
    };
    store.cost_mark_dispatched(r2, now).unwrap();
    store
        .cost_settle_usage(r2, 100_000, 0, 0, 2_000, None, None, now + 1)
        .unwrap();
    let row2 = store.cost_reservations_of(s.id, tid, 10).unwrap()[0].clone();
    assert_eq!(row2.cost_basis.as_deref(), Some("RouteSnapshotEstimate"));
    assert_eq!(row2.settled_cost_micro, Some(1_620_000));
    assert_eq!(row2.provider_reported_cost_micro, None);

    // No price authority + no cap: documented Unknown spend, basis
    // Unknown, nothing folded.
    let CostReserveOutcome::Granted(r3) = store
        .cost_reserve(s.id, tid, OpId::new(3), 100, now)
        .unwrap()
    else {
        panic!("reserve r3")
    };
    store.cost_mark_dispatched(r3, now).unwrap();
    let settled = store
        .cost_settle_usage(r3, 1_000, 0, 0, 2_000, None, None, now + 1)
        .unwrap();
    assert!(matches!(settled, CostSettleOutcome::AppliedUnknown));
    let row3 = store.cost_reservations_of(s.id, tid, 10).unwrap()[0].clone();
    assert_eq!(row3.cost_basis.as_deref(), Some("Unknown"));
    assert_eq!(row3.settled_cost_micro, None);
    assert_eq!(row3.estimated_cost_micro, Some(100));
}

#[test]
fn reconcile_settles_each_uncertain_attempt_from_its_own_provider_row() {
    // (vii) Two dispatched attempts of ONE logical op crash UNCERTAIN;
    // each attempt's own completed provider-call row settles ITS OWN
    // reservation — never the sibling's — even when the sibling's row is
    // the newest completed row the old op_id join would have picked.
    let dir = tempfile::tempdir().unwrap();
    let (sid, tid, r1, r2, a1, a2) = {
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let task = seed_task(&store, s.id, TaskId::new(1), vec![], TaskState::Running);
        let tid = task.task_id;
        store
            .cost_task_cap_set(s.id, tid, Some(10_000_000))
            .unwrap();
        let now = now_ms();
        let logical = OpId::new(800);
        let a1 = faktor_core::op::ModelCallAttempt::new(logical, OpId::new(801), 0).unwrap();
        let a2 = faktor_core::op::ModelCallAttempt::new(logical, OpId::new(802), 1).unwrap();
        let CostReserveOutcome::Granted(r1) = store
            .cost_reserve_attempt(
                s.id,
                tid,
                &a1,
                50_000,
                now,
                Some(&known_snapshot_json()),
                ExecutionClass::Unknown,
            )
            .unwrap()
        else {
            panic!("reserve a1")
        };
        let CostReserveOutcome::Granted(r2) = store
            .cost_reserve_attempt(
                s.id,
                tid,
                &a2,
                50_000,
                now,
                Some(&known_snapshot_json()),
                ExecutionClass::Unknown,
            )
            .unwrap()
        else {
            panic!("reserve a2")
        };
        store.cost_mark_dispatched(r1, now).unwrap();
        store.cost_mark_dispatched(r2, now).unwrap();
        // Crash: both dispatched rows never settled.
        (s.id, tid, r1, r2, a1, a2)
    };
    let store = Store::open(dir.path(), true).unwrap();
    let (refunded, uncertain) = store.cost_recover_open_reservations(now_ms()).unwrap();
    assert_eq!((refunded, uncertain), (0, 2));
    // The resumed logical op completes BOTH attempts' provider rows, but
    // attempt 2's row lands FIRST and attempt 1's row is the newest
    // completed row overall: the old "latest completed row of the op"
    // join would have settled attempt 2's reservation from attempt 1's
    // tokens. The attempt join must not.
    store
        .record_provider_call_attempt(
            sid,
            &a2,
            Some(r2),
            "fake",
            "m",
            "completed",
            Some(1_000),
            Some(100),
            None,
        )
        .unwrap();
    store
        .record_provider_call_attempt(
            sid,
            &a1,
            Some(r1),
            "fake",
            "m",
            "completed",
            Some(4_000),
            Some(2_000),
            None,
        )
        .unwrap();
    let report = store.cost_reconcile_uncertain(sid, tid, now_ms()).unwrap();
    assert_eq!(report.settled, 2);
    // attempt 1: 4_000 @15 + 2_000 @60 = 60_000 + 120_000 = 180_000.
    // attempt 2: 1_000 @15 + 100 @60 = 15_000 + 6_000 = 21_000.
    assert_eq!(report.charged_micro, 180_000 + 21_000);
    let rows = store.cost_reservations_of(sid, tid, 10).unwrap();
    let row1 = rows.iter().find(|r| r.reservation_id == r1).unwrap();
    assert_eq!(row1.provider_cost_micro, Some(180_000), "a1's own tokens");
    let row2 = rows.iter().find(|r| r.reservation_id == r2).unwrap();
    assert_eq!(
        row2.provider_cost_micro,
        Some(21_000),
        "a2 settles from a2's row, never the sibling's newest row"
    );
    assert_eq!(row1.cost_basis.as_deref(), Some("RouteSnapshotEstimate"));
    assert_eq!(row1.settled_cost_micro, Some(180_000));
    // Idempotent: a second pass settles nothing.
    let report = store.cost_reconcile_uncertain(sid, tid, now_ms()).unwrap();
    assert_eq!(report, CostReconcileReport::default());
}

/// (c) evidence: the uncertain sweeps at a realistic candidate count. The
/// pass must settle/close every candidate EXACTLY as the per-candidate
/// implementation did, in ONE writer job with per-candidate savepoints.
#[test]
fn uncertain_sweep_batches_candidates_into_one_writer_job() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    let task = seed_task(&store, s.id, TaskId::new(1), vec![], TaskState::Running);
    let now = 1_000i64;
    let seed_uncertain = |base: u64, with_call: bool| {
        for i in 0..2_000u64 {
            let op = OpId::new(base + i);
            let CostReserveOutcome::Granted(r) = store
                .cost_reserve(s.id, task.task_id, op, 500, now)
                .unwrap()
            else {
                panic!("reserve")
            };
            store.cost_mark_dispatched(r, now).unwrap();
            assert_eq!(
                store.cost_mark_uncertain(r, "wire", None, now).unwrap(),
                CostReservationState::Applied
            );
            if with_call {
                store
                    .record_provider_call(
                        s.id,
                        op,
                        "fake",
                        "m",
                        "completed",
                        Some(10),
                        Some(2),
                        None,
                    )
                    .unwrap();
            }
        }
    };
    // Reconcile candidates: a completed provider row exists, no price
    // authority -> each closes as an honest Unknown spend.
    seed_uncertain(10_000, true);
    let started = std::time::Instant::now();
    let report = store
        .cost_reconcile_uncertain(s.id, task.task_id, now + 1)
        .unwrap();
    let reconcile = started.elapsed();
    assert_eq!(report.closed_unknown, 2_000, "{report:?}");
    assert_eq!(report.settled, 0);
    assert_eq!(report.left_uncertain, 0);
    // Idempotent: nothing remains uncertain.
    assert_eq!(
        store
            .cost_reconcile_uncertain(s.id, task.task_id, now + 1)
            .unwrap(),
        CostReconcileReport::default()
    );
    // Finalize candidates: still-uncertain rows without a completed call
    // close at the conservative reserved estimate.
    seed_uncertain(20_000, false);
    let started = std::time::Instant::now();
    let finalized = store
        .cost_finalize_uncertain(s.id, task.task_id, now + 2)
        .unwrap();
    let finalize = started.elapsed();
    assert_eq!(finalized.settled, 2_000, "{finalized:?}");
    assert_eq!(finalized.charged_micro, 2_000 * 500);
    println!("uncertain sweep 2k candidates: reconcile {reconcile:?}, finalize {finalize:?}");
}

/// (c) per-candidate SAVEPOINT atomicity inside the batched sweep: a
/// no-write candidate (LeftUncertain / task row gone / Skipped) is undone
/// row-locally while its MUTATING sibling in the SAME pass still commits —
/// and a vanished task's candidate never leaks its UPDATE into the store.
#[test]
fn uncertain_sweep_undoes_only_the_no_write_candidates() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    let now = 1_000i64;
    let task = seed_task(&store, s.id, TaskId::new(1), vec![], TaskState::Running);
    let uncertain = |op_base: u64, with_call: bool| {
        let op = OpId::new(op_base);
        let CostReserveOutcome::Granted(r) = store
            .cost_reserve(s.id, task.task_id, op, 700, now)
            .unwrap()
        else {
            panic!("reserve")
        };
        store.cost_mark_dispatched(r, now).unwrap();
        assert_eq!(
            store.cost_mark_uncertain(r, "wire", None, now).unwrap(),
            CostReservationState::Applied
        );
        if with_call {
            store
                .record_provider_call(s.id, op, "fake", "m", "completed", Some(1), Some(1), None)
                .unwrap();
        }
        r
    };
    // ONE pass over the task: the candidate WITH a completed call closes as
    // an unknown spend; the row WITHOUT one is not a candidate at all (the
    // candidate read is bounded to rows an honest settlement exists for) and
    // stays uncertain with NOTHING written.
    let r_settles = uncertain(1, true);
    let r_stays = uncertain(2, false);
    let report = store
        .cost_reconcile_uncertain(s.id, task.task_id, now + 1)
        .unwrap();
    assert_eq!(report.closed_unknown, 1, "{report:?}");
    assert_eq!(report.left_uncertain, 0, "{report:?}");
    assert_eq!(report.settled, 0);
    let rows = store.cost_reservations_of(s.id, task.task_id, 10).unwrap();
    let settled_row = rows.iter().find(|r| r.reservation_id == r_settles).unwrap();
    assert_eq!(settled_row.status, "settled");
    assert!(settled_row.cost_basis.as_deref().is_some());
    let stayed = rows.iter().find(|r| r.reservation_id == r_stays).unwrap();
    assert_eq!(
        stayed.status, "uncertain",
        "the no-write candidate is undone"
    );
    assert_eq!(stayed.provider_cost_micro, None);
    assert_eq!(stayed.settled_cost_micro, None);
    // Finalize closes the still-uncertain candidate at its estimate.
    let finalized = store
        .cost_finalize_uncertain(s.id, task.task_id, now + 2)
        .unwrap();
    assert_eq!(finalized.settled, 1, "{finalized:?}");
    assert_eq!(finalized.charged_micro, 700);
    // A candidate whose task row vanished writes nothing and stays
    // uncertain for doctor's dangling scan (savepoint undo).
    let gone = seed_task(&store, s.id, TaskId::new(2), vec![], TaskState::Running);
    let op = OpId::new(9);
    // PRICED, so the settled branch really runs its task fold and finds the
    // task row gone (the unpriced close writes no task row at all).
    let CostReserveOutcome::Granted(r_gone) = store
        .cost_reserve_priced(
            s.id,
            gone.task_id,
            op,
            300,
            now,
            Some(&known_snapshot_json()),
            ExecutionClass::Unknown,
        )
        .unwrap()
    else {
        panic!("reserve")
    };
    store.cost_mark_dispatched(r_gone, now).unwrap();
    store
        .cost_mark_uncertain(r_gone, "wire", None, now)
        .unwrap();
    store
        .record_provider_call(s.id, op, "fake", "m", "completed", Some(1), Some(1), None)
        .unwrap();
    {
        let conn = store.raw_conn();
        conn.execute(
            "DELETE FROM task WHERE session_id = ?1 AND task_id = ?2",
            params![s.id.raw() as i64, gone.task_id.raw() as i64],
        )
        .unwrap();
    }
    let gone_report = store
        .cost_reconcile_uncertain(s.id, gone.task_id, now + 3)
        .unwrap();
    assert_eq!(gone_report.left_uncertain, 1, "{gone_report:?}");
    assert_eq!(gone_report.closed_unknown, 0);
    assert_eq!(gone_report.settled, 0);
    let gone_rows = store.cost_reservations_of(s.id, gone.task_id, 10).unwrap();
    let row = gone_rows
        .iter()
        .find(|r| r.reservation_id == r_gone)
        .unwrap();
    assert_eq!(
        row.status, "uncertain",
        "the undone candidate stays uncertain"
    );
    assert_eq!(row.provider_cost_micro, None);
    assert_eq!(row.settled_cost_micro, None);
}

// ------------------------------------------------- model outcome stats
// (migration v18 / schema target 19, audit items 13/14/L)

pub(crate) fn outcome_sample(
    verified_success: bool,
    rework_cost: u64,
    rework_turns: u64,
) -> ModelOutcomeSample {
    ModelOutcomeSample {
        verified_success,
        rework_cost_micro: rework_cost,
        rework_turns,
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn assert_row(
    row: &ModelOutcomeStatsRow,
    provider: &str,
    model: &str,
    phase: RouterPhase,
    task_class: TaskClass,
    risk_bucket: RiskBucket,
    successes: u64,
    failures: u64,
    cost: u64,
    turns: u64,
) {
    assert_eq!(row.provider, provider);
    assert_eq!(row.model, model);
    assert_eq!(row.phase, phase);
    assert_eq!(row.task_class, task_class);
    assert_eq!(row.risk_bucket, risk_bucket);
    assert_eq!(row.successes_first_pass, successes);
    assert_eq!(row.failures_first_pass, failures);
    assert_eq!(row.rework_cost_micro_sum, cost);
    assert_eq!(row.rework_turns_sum, turns);
    assert_eq!(row.sample_count, successes + failures);
}

#[test]
fn model_outcome_stats_survive_reopen_and_fold_per_phase() {
    // Append facts + projection: exact-key reads stay exact, the phase
    // fold sums every class/risk bucket, and a drop/reopen returns
    // byte-identical rows (durable stats surviving reopen).
    let dir = tempfile::tempdir().unwrap();
    let (s_id, t_id, phase) = {
        let store = Store::open(dir.path().join("store"), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let tid = seed_task(&store, s.id, TaskId::new(1), vec![], TaskState::Running).task_id;
        // Medium/Low: 55 clean + 45 failing (2.4M cost / 3 turns each).
        for _ in 0..55 {
            store
                .model_outcome_stats_append(
                    "cheap",
                    "m1",
                    RouterPhase::Implement,
                    TaskClass::Medium,
                    RiskBucket::Low,
                    outcome_sample(true, 0, 0),
                )
                .unwrap();
        }
        for _ in 0..45 {
            store
                .model_outcome_stats_append(
                    "cheap",
                    "m1",
                    RouterPhase::Implement,
                    TaskClass::Medium,
                    RiskBucket::Low,
                    outcome_sample(false, 2_400_000, 3),
                )
                .unwrap();
        }
        // Hard/High: the same totals again, exercising the fold.
        for _ in 0..55 {
            store
                .model_outcome_stats_append(
                    "cheap",
                    "m1",
                    RouterPhase::Implement,
                    TaskClass::Hard,
                    RiskBucket::High,
                    outcome_sample(true, 0, 0),
                )
                .unwrap();
        }
        for _ in 0..45 {
            store
                .model_outcome_stats_append(
                    "cheap",
                    "m1",
                    RouterPhase::Implement,
                    TaskClass::Hard,
                    RiskBucket::High,
                    outcome_sample(false, 2_400_000, 3),
                )
                .unwrap();
        }
        // Strong model + a different phase: must NOT leak into folds.
        for _ in 0..94 {
            store
                .model_outcome_stats_append(
                    "strong",
                    "m2",
                    RouterPhase::Implement,
                    TaskClass::Medium,
                    RiskBucket::Low,
                    outcome_sample(true, 0, 0),
                )
                .unwrap();
        }
        for _ in 0..6 {
            store
                .model_outcome_stats_append(
                    "strong",
                    "m2",
                    RouterPhase::Implement,
                    TaskClass::Medium,
                    RiskBucket::Low,
                    outcome_sample(false, 960_000, 2),
                )
                .unwrap();
        }
        for _ in 0..3 {
            store
                .model_outcome_stats_append(
                    "strong",
                    "m2",
                    RouterPhase::Review,
                    TaskClass::Easy,
                    RiskBucket::Low,
                    outcome_sample(true, 0, 0),
                )
                .unwrap();
        }
        let exact = store
            .model_outcome_stats_get(
                "cheap",
                "m1",
                RouterPhase::Implement,
                TaskClass::Medium,
                RiskBucket::Low,
            )
            .unwrap()
            .unwrap();
        assert_row(
            &exact,
            "cheap",
            "m1",
            RouterPhase::Implement,
            TaskClass::Medium,
            RiskBucket::Low,
            55,
            45,
            45 * 2_400_000,
            45 * 3,
        );
        let fold = store
            .model_outcome_stats_phase("cheap", "m1", RouterPhase::Implement)
            .unwrap()
            .unwrap();
        assert_eq!(fold.provider, "cheap");
        assert_eq!(fold.model, "m1");
        assert_eq!(fold.phase, RouterPhase::Implement);
        assert_eq!(fold.successes_first_pass, 110);
        assert_eq!(fold.failures_first_pass, 90);
        assert_eq!(fold.rework_cost_micro_sum, 90 * 2_400_000);
        assert_eq!(fold.rework_turns_sum, 90 * 3);
        assert_eq!(fold.sample_count, 200);
        // The fold's class/risk columns are the first folded row's (the
        // PK text order is deterministic); only the sums are meaningful.
        assert!(matches!(
            fold.task_class,
            TaskClass::Medium | TaskClass::Hard
        ));
        assert!(matches!(
            fold.risk_bucket,
            RiskBucket::Low | RiskBucket::High
        ));
        let strong = store
            .model_outcome_stats_phase("strong", "m2", RouterPhase::Implement)
            .unwrap()
            .unwrap();
        assert_row(
            &strong,
            "strong",
            "m2",
            RouterPhase::Implement,
            TaskClass::Medium,
            RiskBucket::Low,
            94,
            6,
            6 * 960_000,
            6 * 2,
        );
        // The Review samples never leak into the Implement fold.
        assert!(store
            .model_outcome_stats_phase("strong", "m2", RouterPhase::Review)
            .unwrap()
            .is_some());
        (s.id, tid, RouterPhase::Implement)
    };
    // Reopen: migrations are a no-op (only the v18 block could replay,
    // and CREATE IF NOT EXISTS keeps rows), every row reads identical.
    let store = Store::open(dir.path().join("store"), true).unwrap();
    let v: i64 = {
        let conn = store.read().unwrap();
        conn.query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(
        v,
        crate::schema::SCHEMA_HEAD,
        "a fresh database carries the full migration head"
    );
    let fold = store
        .model_outcome_stats_phase("cheap", "m1", phase)
        .unwrap()
        .unwrap();
    assert_eq!(fold.successes_first_pass, 110);
    assert_eq!(fold.failures_first_pass, 90);
    assert_eq!(fold.rework_cost_micro_sum, 90 * 2_400_000);
    assert_eq!(fold.rework_turns_sum, 90 * 3);
    assert_eq!(fold.sample_count, 200);
    let exact = store
        .model_outcome_stats_get("strong", "m2", phase, TaskClass::Medium, RiskBucket::Low)
        .unwrap()
        .unwrap();
    assert_row(
        &exact,
        "strong",
        "m2",
        phase,
        TaskClass::Medium,
        RiskBucket::Low,
        94,
        6,
        6 * 960_000,
        6 * 2,
    );
    assert!(store
        .model_outcome_stats_get("nobody", "m", phase, TaskClass::Medium, RiskBucket::Low,)
        .unwrap()
        .is_none());
    drop(store);
    // Rewind to the previous schema target: the v18 block replays and
    // existing rows SURVIVE (CREATE IF NOT EXISTS is idempotent), while
    // the v19 column must be dropped first because its ADDITIVE ALTER is
    // not idempotent by design (the full migration chain owns the
    // column's existence).
    {
        let conn =
            rusqlite::Connection::open(dir.path().join("store").join("faktor-plus.db")).unwrap();
        conn.execute(
            "ALTER TABLE provider_call DROP COLUMN prefix_segments_json",
            [],
        )
        .unwrap();
        // The v20 verification-record evidence columns are
        // post-this-version too: drop them so the full chain (past v20)
        // replays cleanly.
        conn.execute(
            "ALTER TABLE verification_record DROP COLUMN environment_fingerprint_json",
            [],
        )
        .unwrap();
        conn.execute(
            "ALTER TABLE verification_record DROP COLUMN candidate_proof_ref_json",
            [],
        )
        .unwrap();
        // v24 attachments are post-this-version: drop them (tolerantly,
        // some legacy shapes lack the table/column) so the full chain
        // replays cleanly on reopen.
        let _ = conn.execute("ALTER TABLE task DROP COLUMN attachments", []);
        let _ = conn.execute("ALTER TABLE task_ledger DROP COLUMN attachments", []);
        let _ = conn.execute("DROP TABLE IF EXISTS attachment", []);
        // The v30 shell-attribution column is post-this-version too.
        let _ = conn.execute("ALTER TABLE tool_run DROP COLUMN pre_manifest", []);
        // The v31 engineering-contract column is post-this-version too.
        let _ = conn.execute("ALTER TABLE task DROP COLUMN contract", []);
        // The v35 route-time execution class is post-this-version too.
        let _ = conn.execute(
            "ALTER TABLE cost_reservation DROP COLUMN execution_class",
            [],
        );
        conn.execute("PRAGMA user_version = 18", []).unwrap();
    }
    let store = Store::open(dir.path().join("store"), true).unwrap();
    let fold = store
        .model_outcome_stats_phase("cheap", "m1", phase)
        .unwrap()
        .unwrap();
    assert_eq!(fold.successes_first_pass, 110);
    assert_eq!(fold.failures_first_pass, 90);
    assert_eq!(fold.rework_cost_micro_sum, 90 * 2_400_000);
    assert_eq!(fold.rework_turns_sum, 90 * 3);
    assert_eq!(fold.sample_count, 200);
    assert_eq!(store.get_session(s_id).unwrap().unwrap().id, s_id);
    assert_eq!(store.get_task(s_id, t_id).unwrap().unwrap().task_id, t_id);
}

#[test]
fn model_outcome_stats_race_writers_never_lose_a_sample() {
    // N threads hammering the SAME key through the shared store: the
    // single writer owner serializes the transactional read-modify-write,
    // so every sample lands exactly once — no lost updates, no broken
    // invariant (adversarial duplicate-replay shape: each thread is a
    // distinct "caller" and 10 identical appends must count 10).
    let dir = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(Store::open(dir.path(), true).unwrap());
    let mut handles = Vec::new();
    for t in 0..8u64 {
        let store = store.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..25u64 {
                let ok = (t + i) % 3 != 0;
                store
                    .model_outcome_stats_append(
                        "p",
                        "m",
                        RouterPhase::Implement,
                        TaskClass::Hard,
                        RiskBucket::High,
                        outcome_sample(ok, if ok { 0 } else { 1_000 }, 1),
                    )
                    .unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let row = store
        .model_outcome_stats_get(
            "p",
            "m",
            RouterPhase::Implement,
            TaskClass::Hard,
            RiskBucket::High,
        )
        .unwrap()
        .unwrap();
    assert_eq!(row.sample_count, 8 * 25, "every appended sample lands once");
    assert_eq!(
        row.successes_first_pass + row.failures_first_pass,
        row.sample_count
    );
    // Cross-thread writes never bleed into another key of the same
    // phase fold.
    let fold = store
        .model_outcome_stats_phase("p", "m", RouterPhase::Implement)
        .unwrap()
        .unwrap();
    assert_eq!(fold.sample_count, 8 * 25);
    assert_eq!(fold.failures_first_pass, row.failures_first_pass);
}

#[test]
fn model_outcome_stats_success_samples_never_carry_rework_and_hostile_rows_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    // A verified first-pass success can NEVER carry rework, even when a
    // hostile caller hands hostile magnitudes (u64::MAX clamps at the
    // SQLite INTEGER ceiling instead of overflowing or wrapping).
    store
        .model_outcome_stats_append(
            "p",
            "m",
            RouterPhase::Implement,
            TaskClass::Easy,
            RiskBucket::Low,
            outcome_sample(true, u64::MAX, u64::MAX),
        )
        .unwrap();
    store
        .model_outcome_stats_append(
            "p",
            "m",
            RouterPhase::Implement,
            TaskClass::Easy,
            RiskBucket::Low,
            outcome_sample(false, u64::MAX, u64::MAX),
        )
        .unwrap();
    let row = store
        .model_outcome_stats_get(
            "p",
            "m",
            RouterPhase::Implement,
            TaskClass::Easy,
            RiskBucket::Low,
        )
        .unwrap()
        .unwrap();
    assert_eq!(row.successes_first_pass, 1);
    assert_eq!(row.failures_first_pass, 1);
    assert_eq!(
        row.rework_cost_micro_sum,
        i64::MAX as u64,
        "failure rework clamps at i64::MAX"
    );
    assert_eq!(row.rework_turns_sum, i64::MAX as u64);
    // CHECKs hold the projection invariant at the SQL level: a direct
    // (API-bypassing) INSERT whose sample_count contradicts its
    // successes + failures is refused outright.
    let err = store
        .sql_execute(
            "INSERT INTO model_outcome_stats VALUES (
                'bad1','m','\"implement\"','\"easy\"','\"low\"', 5, 0, 0, 0, 1, 1)",
        )
        .unwrap_err();
    assert!(err.to_string().contains("CHECK"), "{err}");
    // Enum text corruption is refused by the typed readers whenever the
    // row stays addressable: corrupting a KEY dimension renames the row
    // out of every typed lookup (Ok(None), never a guessed enum), so
    // the remaining corrupt-shape attack is the arithmetic invariant —
    // hostile DDL can drop the CHECKs, but the read path still refuses
    // the invariant-broken row it smuggles in.
    store
        .sql_execute(
            "DROP TABLE model_outcome_stats;
             CREATE TABLE model_outcome_stats (
                provider TEXT NOT NULL, model TEXT NOT NULL,
                phase TEXT NOT NULL, task_class TEXT NOT NULL,
                risk_bucket TEXT NOT NULL,
                successes_first_pass INTEGER NOT NULL,
                failures_first_pass INTEGER NOT NULL,
                rework_cost_micro_sum INTEGER NOT NULL,
                rework_turns_sum INTEGER NOT NULL,
                sample_count INTEGER NOT NULL,
                updated_ms INTEGER NOT NULL,
                PRIMARY KEY (provider, model, phase, task_class, risk_bucket)
             ) WITHOUT ROWID;
             INSERT INTO model_outcome_stats VALUES (
                'bad3','m','\"implement\"','\"easy\"','\"low\"', 5, 0, 0, 0, 1, 1)",
        )
        .unwrap();
    match store.model_outcome_stats_get(
        "bad3",
        "m",
        RouterPhase::Implement,
        TaskClass::Easy,
        RiskBucket::Low,
    ) {
        Err(StoreError::Corrupt(_)) => {}
        other => panic!("invariant-broken row must be Corrupt, got {other:?}"),
    }
    match store.model_outcome_stats_phase("bad3", "m", RouterPhase::Implement) {
        Err(StoreError::Corrupt(_)) => {}
        other => panic!("the phase fold must refuse the same row, got {other:?}"),
    }
}
