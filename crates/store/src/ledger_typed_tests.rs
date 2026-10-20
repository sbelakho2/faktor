use super::*;

pub(crate) fn tmp_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    (dir, store)
}

pub(crate) fn sid(store: &Store) -> SessionId {
    let ws = store.create_workspace("/w").unwrap();
    store.create_session(ws, "t", "p", "m").unwrap().id
}

/// One refused typed-ledger append (the hostile callers want the error).
fn refused_append(store: &Store, session: SessionId) -> StoreError {
    store
        .append_ledger_entry(session, "goal_set", 1, serde_json::json!({"goal": "next"}))
        .unwrap_err()
}

fn assert_ledger_corrupt_contains(err: StoreError, needle: &str) {
    match err {
        StoreError::Corrupt(msgs) => assert!(
            msgs.iter().any(|m| m.contains(needle)),
            "the refusal must name {needle:?}: {msgs:?}"
        ),
        other => panic!("must refuse typed Corrupt naming {needle:?}, got {other:?}"),
    }
}

/// Hostile raw ledger row: no API can mint a seq at/below the signed
/// floor, so a tampered database is the only way to seed one.
fn seed_ledger_entry(store: &Store, session: SessionId, seq: i64) {
    store
        .raw_conn()
        .execute(
            "INSERT INTO ledger_entry(session_id, seq, entry_type, schema_ver, payload, created_ms)
         VALUES (?1, ?2, 'goal_set', 1, '{\"goal\":\"hostile\"}', 1)",
            params![session.raw() as i64, seq],
        )
        .unwrap();
}

#[test]
fn ledger_entries_append_gapless_and_page_bounded() {
    let (_d, store) = tmp_store();
    let s = sid(&store);
    let seq1 = store
        .append_ledger_entry(s, "goal_set", 1, serde_json::json!({"goal": "g"}))
        .unwrap();
    let seq2 = store
        .append_ledger_entry(s, "blocker_opened", 1, serde_json::json!({"reason": "r"}))
        .unwrap();
    assert_eq!((seq1, seq2), (1, 2));
    let page = store.ledger_entries(s, None, 1).unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].entry_type, "goal_set");
    assert_eq!(page[0].schema_ver, 1);
    assert_eq!(page[0].payload, serde_json::json!({"goal": "g"}));
    let rest = store.ledger_entries(s, Some(1), 10).unwrap();
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].entry_type, "blocker_opened");
    // Sessions are isolated.
    let s2 = sid(&store);
    assert!(store.ledger_entries(s2, None, 10).unwrap().is_empty());
    assert_eq!(store.ledger_max_seq(s2).unwrap(), 0);
}

#[test]
fn ledger_append_refuses_at_the_sqlite_signed_ceiling_without_mutation() {
    let (_d, store) = tmp_store();
    let s = sid(&store);
    // Empty entry table, head checkpoint at i64::MAX: the next typed seq
    // would be i64::MAX + 1, outside SQLite's signed range. The append
    // must refuse typed, insert no row and leave the head untouched.
    store
        .put_ledger_head(s, serde_json::json!({"goal": "g"}), i64::MAX, 1)
        .unwrap();
    assert_ledger_corrupt_contains(refused_append(&store, s), "exhausted");
    assert!(
        store.ledger_entries(s, None, 10).unwrap().is_empty(),
        "a refused append must insert no entry"
    );
    let head = store.ledger_head(s).unwrap().unwrap();
    assert_eq!(head.checkpoint_seq, i64::MAX);
    assert_eq!(head.head_json["goal"], "g");
}

#[test]
fn ledger_append_refuses_at_prev_i64_max_and_corrupt_sequence_bounds() {
    let (_d, store) = tmp_store();
    let s = sid(&store);
    seed_ledger_entry(&store, s, i64::MAX);
    assert_ledger_corrupt_contains(refused_append(&store, s), "exhausted");
    assert_eq!(
        store.ledger_max_seq(s).unwrap(),
        i64::MAX,
        "the hostile row is the only row; the refused append inserted none"
    );

    // prev below 1 is corruption, never a minted non-positive seq.
    let s2 = sid(&store);
    seed_ledger_entry(&store, s2, 0);
    assert_ledger_corrupt_contains(refused_append(&store, s2), "below 1");
    assert_eq!(store.ledger_max_seq(s2).unwrap(), 0, "no row inserted");

    // A negative head checkpoint is corruption too.
    let s3 = sid(&store);
    store
        .put_ledger_head(s3, serde_json::json!({"schema_ver": 1}), -1, 1)
        .unwrap();
    assert_ledger_corrupt_contains(refused_append(&store, s3), "negative");
    assert!(store.ledger_entries(s3, None, 10).unwrap().is_empty());
}

#[test]
fn ledger_page_limits_refuse_above_the_signed_range() {
    let (_d, store) = tmp_store();
    let s = sid(&store);
    store
        .append_ledger_entry(s, "goal_set", 1, serde_json::json!({"goal": "g"}))
        .unwrap();
    // The largest signed limit is a real bounded page; `None` and the
    // legacy `u64::MAX` sentinel stay the honest unbounded page.
    let pages = [
        store.ledger_entries(s, None, i64::MAX as u64).unwrap(),
        store.ledger_entries_desc(s, None, i64::MAX as u64).unwrap(),
        store.ledger_entries_page(s, None, None).unwrap(),
        store.ledger_entries_desc_page(s, None, None).unwrap(),
        store.ledger_entries(s, None, u64::MAX).unwrap(),
        store.ledger_entries_desc(s, None, u64::MAX).unwrap(),
    ];
    assert!(pages.iter().all(|rows| rows.len() == 1), "{pages:?}");
    // Any OTHER out-of-range limit refuses typed on both APIs.
    let oversized =
        |rows: StoreResult<Vec<LedgerEntryRow>>| matches!(rows, Err(StoreError::Oversized(_)));
    for limit in [i64::MAX as u64 + 1, u64::MAX] {
        assert!(oversized(store.ledger_entries_page(s, None, Some(limit))));
        assert!(oversized(store.ledger_entries_desc_page(
            s,
            None,
            Some(limit)
        )));
    }
    assert!(oversized(store.ledger_entries(
        s,
        None,
        i64::MAX as u64 + 1
    )));
    assert!(oversized(store.ledger_entries_desc(
        s,
        None,
        i64::MAX as u64 + 1
    )));
}

#[test]
fn head_roundtrips_and_compaction_is_atomic_with_head_rewrite() {
    let (_d, store) = tmp_store();
    let s = sid(&store);
    assert!(store.ledger_head(s).unwrap().is_none());
    // A corrupt head blob reads back as an error (the session layer
    // maps that to a rebuild-from-entries).
    store
        .append_ledger_entry(s, "goal_set", 1, serde_json::json!({"goal": "g"}))
        .unwrap();
    store
        .append_ledger_entry(
            s,
            "decision",
            1,
            serde_json::json!({"step": "s", "choice": "c", "rationale": "r"}),
        )
        .unwrap();
    store
    .append_ledger_entry(s, "routing_decision", 1, serde_json::json!({"turn": 1, "provider": "p", "model": "m", "reasoning": "why", "cost_micro": 7}))
    .unwrap();
    store
        .put_ledger_head(s, serde_json::json!({"schema_ver": 1, "goal": "g"}), 3, 1)
        .unwrap();
    let head = store.ledger_head(s).unwrap().unwrap();
    assert_eq!(head.checkpoint_seq, 3);
    assert_eq!(head.head_json["goal"], "g");
    // Compaction: delete below 3 except the goal (seq 1): only the
    // decision (seq 2) is removed; head rewritten atomically.
    let deleted = store
        .compact_ledger(
            s,
            3,
            &[1],
            serde_json::json!({"schema_ver": 1, "goal": "g", "pruned": true}),
            3,
            1,
        )
        .unwrap();
    assert_eq!(deleted, 1);
    let rows = store.ledger_entries(s, None, 10).unwrap();
    assert_eq!(
        rows.len(),
        2,
        "goal pinned + seq-3 row kept above watermark"
    );
    assert_eq!(rows[0].entry_type, "goal_set");
    assert_eq!(rows[1].entry_type, "routing_decision");
    let head = store.ledger_head(s).unwrap().unwrap();
    assert_eq!(head.head_json["pruned"], true);
    assert_eq!(head.checkpoint_seq, 3);
}

#[test]
fn compaction_protect_list_is_never_evicted() {
    // The never-FIFO-evict rule is enforced in faktor-session; here the
    // STORE contract is: protect rows survive a below-watermark delete.
    let (_d, store) = tmp_store();
    let s = sid(&store);
    for i in 1..=5 {
        store
            .append_ledger_entry(
                s,
                "decision",
                1,
                serde_json::json!({"step": format!("s{i}"), "choice": "c", "rationale": "r"}),
            )
            .unwrap();
    }
    let deleted = store
        .compact_ledger(s, 6, &[2, 5], serde_json::json!({"schema_ver": 1}), 5, 1)
        .unwrap();
    assert_eq!(deleted, 3);
    let kept: Vec<i64> = store
        .ledger_entries(s, None, 10)
        .unwrap()
        .into_iter()
        .map(|r| r.seq)
        .collect();
    assert_eq!(
        kept,
        vec![2, 5],
        "protected rows survive even below the watermark"
    );
    // A compaction that deletes nothing still rewrites the head.
    let deleted = store
        .compact_ledger(s, 0, &[], serde_json::json!({"schema_ver": 1}), 5, 1)
        .unwrap();
    assert_eq!(deleted, 0);
    assert_eq!(store.ledger_head(s).unwrap().unwrap().checkpoint_seq, 5);
}

#[test]
fn event_payload_versions_stamp_and_read_back() {
    let (_d, store) = tmp_store();
    let s = sid(&store);
    // Legacy append stamps v1 (the historical unversioned writers).
    store
        .append_event(
            s,
            None,
            EventKind::ModelStarted,
            AgentState::Streaming,
            1,
            None,
        )
        .unwrap();
    // Version-aware append stamps its own version.
    store
        .append_event_v(
            s,
            None,
            EventKind::Failed,
            AgentState::FailedRecoverable,
            2,
            Some(serde_json::json!({"message": "x"})),
            2,
        )
        .unwrap();
    let rows = store.events_versioned_range(s, 1, None).unwrap();
    assert_eq!(rows.len(), 3, "seed + two appends");
    assert_eq!(rows[1].1, 1, "legacy writer stamps historical v1");
    assert_eq!(rows[2].1, 2, "versioned writer stamps its schema version");
    assert_eq!(rows[2].0.payload, Some(serde_json::json!({"message": "x"})));
    // The plain reader sees the same events (payload_ver never changes
    // the wire shape).
    let plain = store.events_range(s, 1, None).unwrap();
    assert_eq!(plain.len(), 3);
}
// ------------------------------------------------------- index state rows

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

/// Assert a read path refused a structurally invalid `op_id` as a typed
/// `Corrupt` naming the column and the failure reason.
pub(crate) fn assert_corrupt_op_id<T>(outcome: StoreResult<T>, what: &str, needle: &str) {
    match outcome {
        Err(StoreError::Corrupt(msgs)) => assert!(
            msgs.iter()
                .any(|m| m.contains("op_id") && m.contains(needle)),
            "{what}: the refusal must name op_id and {needle:?}: {msgs:?}"
        ),
        Err(e) => panic!("{what}: a corrupt op_id must refuse typed, got {e}"),
        Ok(_) => panic!("{what}: a corrupt op_id must refuse typed, got a decoded value"),
    }
}

/// Every u64-backed id type decodes its own full range through the
/// SQLite bit-cast: u64::MAX, u64::MAX-2 and 2^63 (i64::MIN as raw) all
/// round-trip exactly, while zero stays typed corruption.
#[test]
fn sql_id_bit_cast_round_trips_every_id_type() {
    macro_rules! round_trip {
        ($ty:ty, $raw:expr, $wanted:expr) => {
            assert_eq!(
                id_field::<$ty>("unit-test id column", $raw).unwrap().raw(),
                $wanted,
                "{} must round-trip {}",
                stringify!($ty),
                $wanted
            );
            assert!(
                id_field::<$ty>("unit-test id column", 0).is_err(),
                "{} zero must stay typed corruption",
                stringify!($ty)
            );
        };
    }
    for wanted in [u64::MAX, u64::MAX - 2, u64::MAX / 2 + 1] {
        let raw = wanted as i64;
        round_trip!(SessionId, raw, wanted);
        round_trip!(WorkspaceId, raw, wanted);
        round_trip!(WorktreeId, raw, wanted);
        round_trip!(TaskId, raw, wanted);
        round_trip!(TaskRevision, raw, wanted);
        round_trip!(OpId, raw, wanted);
        round_trip!(EventSeq, raw, wanted);
        round_trip!(VerificationRecordId, raw, wanted);
    }
}

/// The same persisted `op_id` decode class on the other read paths:
/// permission, event, tool_run and turn_record rows with a zero `op_id`
/// refuse typed instead of panicking or minting an id, while negative
/// bit-cast raws decode back to the exact u64.
#[test]
fn corrupt_op_id_columns_refuse_typed_across_read_paths() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    let pid = store
        .insert_permission(s.id, OpId::new(3), "fs.write")
        .unwrap()
        .0;
    assert_eq!(
        store.pending_permission(pid).unwrap().unwrap().1,
        OpId::new(3)
    );
    let trid = store
        .start_tool_run(
            s.id,
            OpId::new(4),
            "write_file",
            serde_json::json!({"path": "/a"}),
            serde_json::json!({"strategy": "verify_hash"}),
            None,
            None,
        )
        .unwrap();
    let turid = store
        .start_turn_record(s.id, OpId::new(5), None, None, "p", "m", None)
        .unwrap();
    assert_eq!(
        store.pending_tool_runs(s.id).unwrap()[0].op_id,
        OpId::new(4)
    );
    assert_eq!(
        store.active_turn_record(s.id).unwrap().unwrap().turn_op_id,
        OpId::new(5)
    );
    assert_eq!(store.events_range(s.id, 1, None).unwrap()[0].op_id, None);

    // Zero is structurally invalid on every one of these columns.
    {
        let conn = store.raw_conn();
        conn.execute(
            "UPDATE permission SET op_id = 0 WHERE id = ?1",
            params![pid],
        )
        .unwrap();
        conn.execute(
            "UPDATE event SET op_id = 0 WHERE session_id = ?1 AND seq = 1",
            params![s.id.raw() as i64],
        )
        .unwrap();
        conn.execute("UPDATE tool_run SET op_id = 0 WHERE id = ?1", params![trid])
            .unwrap();
        conn.execute(
            "UPDATE turn_record SET turn_op_id = 0 WHERE id = ?1",
            params![turid],
        )
        .unwrap();
    }
    assert_corrupt_op_id(store.pending_permission(pid), "permission", "cannot be 0");
    assert_corrupt_op_id(store.events_range(s.id, 1, None), "event", "cannot be 0");
    assert_corrupt_op_id(store.pending_tool_runs(s.id), "tool_run", "cannot be 0");
    assert_corrupt_op_id(store.active_turn_record(s.id), "turn_record", "cannot be 0");

    // Negative raw values are the bit-cast upper half: every read path
    // decodes them back to the exact u64 op id (u64::MAX, -3, 2^63).
    for wanted in [u64::MAX, u64::MAX - 2, 2u64.pow(63)] {
        let bad = wanted as i64;
        {
            let conn = store.raw_conn();
            conn.execute(
                "UPDATE permission SET op_id = ?2 WHERE id = ?1",
                params![pid, bad],
            )
            .unwrap();
            conn.execute(
                "UPDATE event SET op_id = ?2 WHERE session_id = ?1 AND seq = 1",
                params![s.id.raw() as i64, bad],
            )
            .unwrap();
            conn.execute(
                "UPDATE tool_run SET op_id = ?2 WHERE id = ?1",
                params![trid, bad],
            )
            .unwrap();
            conn.execute(
                "UPDATE turn_record SET turn_op_id = ?2 WHERE id = ?1",
                params![turid, bad],
            )
            .unwrap();
        }
        assert_eq!(
            store.pending_permission(pid).unwrap().unwrap().1.raw(),
            wanted,
            "permission op_id {wanted} must round-trip exactly"
        );
        assert_eq!(
            store.events_range(s.id, 1, None).unwrap()[0]
                .op_id
                .unwrap()
                .raw(),
            wanted,
            "event op_id {wanted} must round-trip exactly"
        );
        assert_eq!(
            store.pending_tool_runs(s.id).unwrap()[0].op_id.raw(),
            wanted,
            "tool_run op_id {wanted} must round-trip exactly"
        );
        assert_eq!(
            store
                .active_turn_record(s.id)
                .unwrap()
                .unwrap()
                .turn_op_id
                .raw(),
            wanted,
            "turn_record turn_op_id {wanted} must round-trip exactly"
        );
    }
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
fn mark_uncertain_records_reason_and_request_id_on_a_dispatched_row() {
    // (v) A post-dispatch failure marks the row UNCERTAIN with the
    // failure reason code and provider request id recorded durably; the
    // row keeps consuming free until the finalize charges the estimate.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    let task = seed_task(&store, s.id, TaskId::new(1), vec![], TaskState::Running);
    let tid = task.task_id;
    store.cost_task_cap_set(s.id, tid, Some(5_000)).unwrap();
    let now = now_ms();
    let CostReserveOutcome::Granted(r) = store
        .cost_reserve(s.id, tid, OpId::new(9), 2_000, now)
        .unwrap()
    else {
        panic!("reserve granted")
    };
    // mark_uncertain on a never-dispatched row is a typed refusal.
    assert!(matches!(
        store.cost_mark_uncertain(r, "stream_error", Some("req-1"), now),
        Ok(CostReservationState::NotOpen { current }) if current == "reserved"
    ));
    store.cost_mark_dispatched(r, now).unwrap();
    assert_eq!(
        store
            .cost_mark_uncertain(r, "stall_verdict", Some("req-42"), now + 1)
            .unwrap(),
        CostReservationState::Applied
    );
    assert_eq!(
        store.cost_mark_uncertain(r, "x", None, now + 2).unwrap(),
        CostReservationState::NotOpen {
            current: "uncertain".into()
        },
        "an already-uncertain row refuses a second reason (exactly-once reason capture)"
    );
    let rows = store.cost_reservations_of(s.id, tid, 10).unwrap();
    assert_eq!(rows[0].status, "uncertain");
    assert_eq!(
        rows[0].failure_reason_code.as_deref(),
        Some("stall_verdict")
    );
    assert_eq!(rows[0].request_id.as_deref(), Some("req-42"));
    assert_eq!(rows[0].delivery_state.as_deref(), Some("failed"));
    assert_eq!(
        free_micro(&store, s.id, tid),
        3_000,
        "uncertain keeps consuming"
    );
    // The task-completion finalize closes it at the reserved estimate.
    let report = store.cost_finalize_uncertain(s.id, tid, now + 3).unwrap();
    assert_eq!(report.settled, 1);
    assert_eq!(report.charged_micro, 2_000);
    let rows = store.cost_reservations_of(s.id, tid, 10).unwrap();
    assert_eq!(rows[0].status, "settled");
    assert_eq!(
        rows[0].cost_basis.as_deref(),
        Some("ConservativeReservation")
    );
    assert_eq!(rows[0].settled_cost_micro, Some(2_000));
    assert_eq!(rows[0].estimated_cost_micro, Some(2_000));
}
