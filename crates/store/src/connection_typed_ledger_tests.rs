use super::*;

pub(crate) fn tmp_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    (dir, store)
}

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

/// Assert a read path refused a corrupt id COLUMN as a typed `Corrupt`
/// naming the row and column — never a panic, a wrap or a minted id.
pub(crate) fn assert_corrupt_id<T>(outcome: StoreResult<T>, what: &str, column: &str) {
    match outcome {
        Err(StoreError::Corrupt(msgs)) => assert!(
            msgs.iter().any(|m| m.contains(column)),
            "{what}: the refusal must name {column}: {msgs:?}"
        ),
        Err(e) => panic!("{what}: a corrupt {column} must refuse typed, got {e}"),
        Ok(_) => panic!("{what}: a corrupt {column} must refuse typed, got a decoded value"),
    }
}

/// Run one raw write with foreign keys and CHECK constraints OFF, then
/// restore them: corrupting a referenced id column (or a `CHECK`-guarded
/// seq) behind the typed API's back is exactly the hand-corrupted
/// database the read-time decodes must survive.
pub(crate) fn corrupt_ignoring_fks(store: &Store, sql: &str, params: &[&dyn rusqlite::ToSql]) {
    let conn = store.raw_conn();
    conn.execute_batch("PRAGMA foreign_keys = OFF; PRAGMA ignore_check_constraints = ON")
        .unwrap();
    conn.execute(sql, params).unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA ignore_check_constraints = OFF")
        .unwrap();
}

/// The persisted-id decode class across the id COLUMN types: a zero
/// `session.id` (SessionId), `session.workspace_id` (WorkspaceId),
/// `task.task_id` (TaskId), `task.revision` (TaskRevision) or
/// `event.seq` (EventSeq) could never have been written by the typed API,
/// so every read path refuses it as a typed `Corrupt` naming the row and
/// column — never `Id::new(0)` panicking or a silent `.max(1)` mint.
/// NEGATIVE raw values are the bit-cast upper half of the u64 id space
/// (the store writes every id through `raw as i64`), so they must decode
/// back to the exact u64 — never be rejected as corruption. Repairing the
/// row makes the same read decode it again (valid data round-trips).
#[test]
fn corrupt_id_columns_refuse_typed_never_panic_or_mint() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    let task = seed_task(&store, s.id, TaskId::new(1), vec![], TaskState::Running);

    // Valid rows round-trip before corruption.
    assert_eq!(store.session_ids().unwrap(), vec![s.id]);
    assert_eq!(store.get_session(s.id).unwrap().unwrap().workspace_id, ws);
    assert_eq!(store.list_tasks(s.id).unwrap()[0].task_id, TaskId::new(1));
    assert_eq!(
        store
            .get_task(s.id, TaskId::new(1))
            .unwrap()
            .unwrap()
            .revision,
        task.revision
    );
    assert_eq!(store.last_event_seq(s.id).unwrap(), Some(EventSeq::new(1)));

    // Zero is structurally invalid for every one of these id types: each
    // read path refuses it typed, naming the row and column.
    {
        // (SessionId) `session.id`, decoded by the unscoped id scan.
        corrupt_ignoring_fks(
            &store,
            "UPDATE session SET id = 0 WHERE id = ?1",
            &[&(s.id.raw() as i64)],
        );
        assert_corrupt_id(store.session_ids(), "session_ids", "session");
        corrupt_ignoring_fks(
            &store,
            "UPDATE session SET id = ?1 WHERE id = 0",
            &[&(s.id.raw() as i64)],
        );

        // (WorkspaceId) `session.workspace_id` via session_row_map.
        corrupt_ignoring_fks(
            &store,
            "UPDATE session SET workspace_id = 0 WHERE id = ?1",
            &[&(s.id.raw() as i64)],
        );
        assert_corrupt_id(store.get_session(s.id), "get_session", "workspace_id");
        corrupt_ignoring_fks(
            &store,
            "UPDATE session SET workspace_id = ?2 WHERE id = ?1",
            &[&(s.id.raw() as i64), &(ws.raw() as i64)],
        );

        // (TaskId) `task.task_id` via the per-session task list.
        corrupt_ignoring_fks(
            &store,
            "UPDATE task SET task_id = 0 WHERE session_id = ?1 AND task_id = 1",
            &[&(s.id.raw() as i64)],
        );
        assert_corrupt_id(store.list_tasks(s.id), "list_tasks", "task_id");
        corrupt_ignoring_fks(
            &store,
            "UPDATE task SET task_id = 1 WHERE session_id = ?1 AND task_id = 0",
            &[&(s.id.raw() as i64)],
        );

        // (TaskRevision) `task.revision` via the single-row read.
        corrupt_ignoring_fks(
            &store,
            "UPDATE task SET revision = 0 WHERE session_id = ?1 AND task_id = 1",
            &[&(s.id.raw() as i64)],
        );
        assert_corrupt_id(store.get_task(s.id, TaskId::new(1)), "get_task", "revision");
        corrupt_ignoring_fks(
            &store,
            "UPDATE task SET revision = 1 WHERE session_id = ?1 AND task_id = 1",
            &[&(s.id.raw() as i64)],
        );

        // (EventSeq) `event.seq` via the journal high-water read.
        corrupt_ignoring_fks(
            &store,
            "UPDATE event SET seq = 0 WHERE session_id = ?1 AND seq = 1",
            &[&(s.id.raw() as i64)],
        );
        assert_corrupt_id(store.last_event_seq(s.id), "last_event_seq", "seq");
        corrupt_ignoring_fks(
            &store,
            "UPDATE event SET seq = 1 WHERE session_id = ?1 AND seq = 0",
            &[&(s.id.raw() as i64)],
        );
    }

    // The upper half of the u64 id space (negative i64 encodings) decodes
    // back exactly: u64::MAX (-1), u64::MAX-2 (-3) and 2^63 (i64::MIN).
    for wanted in [u64::MAX, u64::MAX - 2, 2u64.pow(63)] {
        let bad = wanted as i64;
        // (SessionId) `session.id`, decoded by the unscoped id scan.
        corrupt_ignoring_fks(
            &store,
            "UPDATE session SET id = ?2 WHERE id = ?1",
            &[&(s.id.raw() as i64), &bad],
        );
        assert_eq!(
            store.session_ids().unwrap(),
            vec![SessionId::new(wanted)],
            "session.id {wanted} must round-trip exactly"
        );
        corrupt_ignoring_fks(
            &store,
            "UPDATE session SET id = ?2 WHERE id = ?1",
            &[&bad, &(s.id.raw() as i64)],
        );
        assert_eq!(store.session_ids().unwrap(), vec![s.id]);

        // (WorkspaceId) `session.workspace_id` via session_row_map.
        corrupt_ignoring_fks(
            &store,
            "UPDATE session SET workspace_id = ?2 WHERE id = ?1",
            &[&(s.id.raw() as i64), &bad],
        );
        assert_eq!(
            store.get_session(s.id).unwrap().unwrap().workspace_id.raw(),
            wanted,
            "session.workspace_id {wanted} must round-trip exactly"
        );
        corrupt_ignoring_fks(
            &store,
            "UPDATE session SET workspace_id = ?2 WHERE id = ?1",
            &[&(s.id.raw() as i64), &(ws.raw() as i64)],
        );

        // (TaskId) `task.task_id` via the per-session task list.
        corrupt_ignoring_fks(
            &store,
            "UPDATE task SET task_id = ?2 WHERE session_id = ?1 AND task_id = 1",
            &[&(s.id.raw() as i64), &bad],
        );
        assert_eq!(
            store.list_tasks(s.id).unwrap()[0].task_id.raw(),
            wanted,
            "task.task_id {wanted} must round-trip exactly"
        );
        corrupt_ignoring_fks(
            &store,
            "UPDATE task SET task_id = ?2 WHERE session_id = ?1 AND task_id = ?3",
            &[&(s.id.raw() as i64), &1i64, &bad],
        );

        // (TaskRevision) `task.revision` via the single-row read.
        corrupt_ignoring_fks(
            &store,
            "UPDATE task SET revision = ?2 WHERE session_id = ?1 AND task_id = 1",
            &[&(s.id.raw() as i64), &bad],
        );
        assert_eq!(
            store
                .get_task(s.id, TaskId::new(1))
                .unwrap()
                .unwrap()
                .revision
                .raw(),
            wanted,
            "task.revision {wanted} must round-trip exactly"
        );
        corrupt_ignoring_fks(
            &store,
            "UPDATE task SET revision = 1 WHERE session_id = ?1 AND task_id = 1",
            &[&(s.id.raw() as i64)],
        );

        // (EventSeq) `event.seq` via the journal high-water read.
        corrupt_ignoring_fks(
            &store,
            "UPDATE event SET seq = ?2 WHERE session_id = ?1 AND seq = 1",
            &[&(s.id.raw() as i64), &bad],
        );
        assert_eq!(
            store.last_event_seq(s.id).unwrap(),
            Some(EventSeq::new(wanted)),
            "event.seq {wanted} must round-trip exactly"
        );
        corrupt_ignoring_fks(
            &store,
            "UPDATE event SET seq = 1 WHERE session_id = ?1 AND seq = ?2",
            &[&(s.id.raw() as i64), &bad],
        );
        assert_eq!(
            store.last_event_seq(s.id).unwrap(),
            Some(EventSeq::new(1)),
            "repairing event.seq restores the journal"
        );
    }
}

/// Child-table id columns read by UNSCOPED doctor scans (no WHERE on the
/// corrupt column, so the decode is really exercised): a zero
/// `turn_record.session_id` and `cost_reservation.session_id/task_id`
/// refuse typed, negative bit-cast raws decode back to the exact u64, and
/// repairing the row re-enables the scan unchanged.
#[test]
fn corrupt_child_id_columns_refuse_typed_in_unscoped_scans() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    let task = seed_task(&store, s.id, TaskId::new(1), vec![], TaskState::Running);
    let turn_id = store
        .start_turn_record(s.id, OpId::new(5), None, None, "p", "m", None)
        .unwrap();
    let CostReserveOutcome::Granted(reservation) = store
        .cost_reserve(s.id, task.task_id, OpId::new(6), 10, now_ms())
        .unwrap()
    else {
        panic!("reservation must be granted");
    };

    // Baseline: both scans decode the valid rows.
    assert_eq!(store.all_active_turns().unwrap()[0].session_id, s.id);
    assert!(store
        .active_turn_ownership_invariants()
        .unwrap()
        .unrecoverable
        .iter()
        .any(|row| row.session_id == s.id));

    // Make the reservation dangling (no task row matches it): doctor's
    // unscoped dangling scan then decodes its id columns directly.
    corrupt_ignoring_fks(
        &store,
        "DELETE FROM task WHERE session_id = ?1 AND task_id = ?2",
        &[&(s.id.raw() as i64), &(task.task_id.raw() as i64)],
    );
    let dangling = store.cost_reservation_invariants().unwrap().dangling;
    assert_eq!(dangling.len(), 1);
    assert_eq!(dangling[0].session_id, s.id);
    assert_eq!(dangling[0].task_id, task.task_id);

    // Zero is structurally invalid: both unscoped scans refuse it typed.
    corrupt_ignoring_fks(
        &store,
        "UPDATE turn_record SET session_id = 0 WHERE id = ?1",
        &[&turn_id],
    );
    assert_corrupt_id(store.all_active_turns(), "all_active_turns", "session_id");
    assert_corrupt_id(
        store.active_turn_ownership_invariants(),
        "active_turn_ownership_invariants",
        "session_id",
    );
    corrupt_ignoring_fks(
        &store,
        "UPDATE turn_record SET session_id = ?2 WHERE id = ?1",
        &[&(s.id.raw() as i64), &turn_id],
    );
    corrupt_ignoring_fks(
        &store,
        "UPDATE cost_reservation SET session_id = 0 WHERE reservation_id = ?1",
        &[&reservation],
    );
    assert_corrupt_id(
        store.cost_reservation_invariants(),
        "cost_reservation_invariants",
        "session_id",
    );
    corrupt_ignoring_fks(
        &store,
        "UPDATE cost_reservation SET session_id = ?2 WHERE reservation_id = ?1",
        &[&(s.id.raw() as i64), &reservation],
    );
    corrupt_ignoring_fks(
        &store,
        "UPDATE cost_reservation SET task_id = 0 WHERE reservation_id = ?1",
        &[&reservation],
    );
    assert_corrupt_id(
        store.cost_reservation_invariants(),
        "cost_reservation_invariants",
        "task_id",
    );
    corrupt_ignoring_fks(
        &store,
        "UPDATE cost_reservation SET task_id = ?2 WHERE reservation_id = ?1",
        &[&(task.task_id.raw() as i64), &reservation],
    );

    // Negative raw values are the bit-cast upper half: the same scans
    // decode them back to the exact u64 (u64::MAX, u64::MAX-2, 2^63).
    for wanted in [u64::MAX, u64::MAX - 2, 2u64.pow(63)] {
        let bad = wanted as i64;
        corrupt_ignoring_fks(
            &store,
            "UPDATE turn_record SET session_id = ?2 WHERE id = ?1",
            &[&turn_id, &bad],
        );
        assert_eq!(
            store.all_active_turns().unwrap()[0].session_id.raw(),
            wanted,
            "turn_record.session_id {wanted} must round-trip exactly"
        );
        assert!(store
            .active_turn_ownership_invariants()
            .unwrap()
            .unrecoverable
            .iter()
            .any(|row| row.session_id.raw() == wanted));
        corrupt_ignoring_fks(
            &store,
            "UPDATE turn_record SET session_id = ?2 WHERE id = ?1",
            &[&(s.id.raw() as i64), &turn_id],
        );
        assert_eq!(store.all_active_turns().unwrap()[0].session_id, s.id);

        corrupt_ignoring_fks(
            &store,
            "UPDATE cost_reservation SET session_id = ?2 WHERE reservation_id = ?1",
            &[&reservation, &bad],
        );
        let by_session = store.cost_reservation_invariants().unwrap().dangling;
        assert_eq!(
            by_session
                .iter()
                .find(|row| row.reservation_id == reservation)
                .unwrap()
                .session_id
                .raw(),
            wanted,
            "cost_reservation.session_id {wanted} must round-trip exactly"
        );
        corrupt_ignoring_fks(
            &store,
            "UPDATE cost_reservation SET session_id = ?2 WHERE reservation_id = ?1",
            &[&(s.id.raw() as i64), &reservation],
        );

        corrupt_ignoring_fks(
            &store,
            "UPDATE cost_reservation SET task_id = ?2 WHERE reservation_id = ?1",
            &[&reservation, &bad],
        );
        let by_task = store.cost_reservation_invariants().unwrap().dangling;
        assert_eq!(
            by_task
                .iter()
                .find(|row| row.reservation_id == reservation)
                .unwrap()
                .task_id
                .raw(),
            wanted,
            "cost_reservation.task_id {wanted} must round-trip exactly"
        );
        corrupt_ignoring_fks(
            &store,
            "UPDATE cost_reservation SET task_id = ?2 WHERE reservation_id = ?1",
            &[&(task.task_id.raw() as i64), &reservation],
        );
        let repaired = store.cost_reservation_invariants().unwrap().dangling;
        assert_eq!(repaired.len(), 1);
        assert_eq!(repaired[0].task_id, task.task_id);
    }
}
