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

#[test]
fn appends_never_rewind_below_the_head_checkpoint() {
    // Post-compaction appends keep allocating ABOVE the checkpoint seq
    // (the session fold cursor must never rewind over pruned rows).
    let (_d, store) = tmp_store();
    let s = sid(&store);
    store
        .append_ledger_entry(s, "goal_set", 1, serde_json::json!({"goal": "g"}))
        .unwrap();
    store
        .put_ledger_head(s, serde_json::json!({"schema_ver": 1}), 1, 1)
        .unwrap();
    let deleted = store
        .compact_ledger(s, 2, &[], serde_json::json!({"schema_ver": 1}), 1, 1)
        .unwrap();
    assert_eq!(deleted, 1);
    let seq = store
        .append_ledger_entry(s, "goal_set", 1, serde_json::json!({"goal": "g2"}))
        .unwrap();
    assert!(
        seq > 1,
        "new seq must sit above the pruned checkpoint, got {seq}"
    );
}

#[test]
fn corrupt_queue_row_json_is_refused_typed_never_defaulted() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    store
        .enqueue_prompt(
            s.id,
            OpId::new(1),
            "hi",
            &["a.rs".into()],
            None,
            None,
            None,
            1,
        )
        .unwrap();
    {
        let conn = store.raw_conn();
        conn.execute(
            "UPDATE prompt_queue SET files = 'not json' WHERE session_id = ?1",
            params![s.id.raw() as i64],
        )
        .unwrap();
    }
    match store.queue_head(s.id) {
        Err(StoreError::Corrupt(msgs)) => assert!(
            msgs.iter().any(|m| m.contains("files")),
            "the refusal must name the files column: {msgs:?}"
        ),
        Err(e) => panic!("corrupt queue files must be a typed Corrupt error, got {e:?}"),
        Ok(_) => panic!("corrupt queue files must not silently decode or vanish"),
    }
    match store.admit_queue_head(s.id, &["idle"], "preparing") {
        Err(StoreError::Corrupt(msgs)) => assert!(
            msgs.iter().any(|m| m.contains("files")),
            "the refusal must name the files column: {msgs:?}"
        ),
        other => panic!("admission of a corrupt row must refuse typed: {other:?}"),
    }
    let status: String = store
        .read()
        .unwrap()
        .query_row(
            "SELECT status FROM prompt_queue WHERE session_id = ?1",
            params![s.id.raw() as i64],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        status, "pending",
        "a refused admission must not claim the row"
    );
    assert!(
        store.messages_before(s.id, None, 10).unwrap().is_empty(),
        "a refused admission must not materialize the user message"
    );
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

/// A persisted `prompt_queue.op_id` that is zero can never have been
/// written by the typed API: every read path must refuse it as a typed
/// `Corrupt` naming the row and column — never panic (`OpId::new(0)`) or
/// mint `1`. Negative raw values are the bit-cast upper half and decode
/// back to the exact u64 op id.
#[test]
fn corrupt_queue_op_id_is_refused_typed_never_panics_or_mints() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    store
        .enqueue_prompt(s.id, OpId::new(7), "first", &[], None, None, None, 1)
        .unwrap();
    store
        .enqueue_prompt(s.id, OpId::new(9), "second", &[], None, None, None, 2)
        .unwrap();

    // Valid rows round-trip before corruption.
    let head = store.queue_head(s.id).unwrap().unwrap();
    assert_eq!((head.queue_seq, head.op_id), (1, OpId::new(7)));
    assert_eq!(
        store.queue_op_ids(s.id).unwrap(),
        vec![OpId::new(7), OpId::new(9)]
    );

    // Zero is structurally invalid: every read path refuses it typed and
    // admission changes nothing.
    {
        let conn = store.raw_conn();
        conn.execute(
            "UPDATE prompt_queue SET op_id = 0 WHERE session_id = ?1 AND seq = 1",
            params![s.id.raw() as i64],
        )
        .unwrap();
    }
    assert_corrupt_op_id(store.queue_head(s.id), "queue_head", "cannot be 0");
    assert_corrupt_op_id(store.queue_op_ids(s.id), "queue_op_ids", "cannot be 0");
    assert_corrupt_op_id(
        store.admit_queue_head(s.id, &["idle"], "preparing"),
        "admit_queue_head",
        "cannot be 0",
    );
    let status: String = store
        .read()
        .unwrap()
        .query_row(
            "SELECT status FROM prompt_queue WHERE session_id = ?1 AND seq = 1",
            params![s.id.raw() as i64],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        status, "pending",
        "a refused admission must not claim the row"
    );
    assert!(
        store.messages_before(s.id, None, 10).unwrap().is_empty(),
        "a refused admission must not materialize the user message"
    );
    assert_eq!(
        store.get_session(s.id).unwrap().unwrap().state,
        AgentState::Idle,
        "a refused admission must not move the session"
    );

    // Negative raw values are the bit-cast upper half: the read paths
    // decode them back to the exact u64 op ids instead of refusing.
    for wanted in [u64::MAX, u64::MAX - 2, 2u64.pow(63)] {
        let bad = wanted as i64;
        {
            let conn = store.raw_conn();
            conn.execute(
                "UPDATE prompt_queue SET op_id = ?2 WHERE session_id = ?1 AND seq = 1",
                params![s.id.raw() as i64, bad],
            )
            .unwrap();
        }
        assert_eq!(
            store.queue_head(s.id).unwrap().unwrap().op_id.raw(),
            wanted,
            "queue_head op_id {wanted} must round-trip exactly"
        );
        assert_eq!(
            store.queue_op_ids(s.id).unwrap()[0].raw(),
            wanted,
            "queue_op_ids {wanted} must round-trip exactly"
        );
    }

    // Repair the row: the head still admits exactly once and the queue
    // then advances to the next row (the fix never wedges the FIFO).
    {
        let conn = store.raw_conn();
        conn.execute(
            "UPDATE prompt_queue SET op_id = ?2 WHERE session_id = ?1 AND seq = 1",
            params![s.id.raw() as i64, 7i64],
        )
        .unwrap();
    }
    let (admitted, _) = store
        .admit_queue_head(s.id, &["idle"], "preparing")
        .unwrap()
        .unwrap();
    assert_eq!((admitted.queue_seq, admitted.op_id), (1, OpId::new(7)));
    store.mark_queue_status(s.id, 1, "completed").unwrap();
    let next = store.queue_head(s.id).unwrap().unwrap();
    assert_eq!(
        (next.queue_seq, next.op_id),
        (2, OpId::new(9)),
        "the queue head advances to the next row"
    );
}

/// Crash-residue recovery of the durable queue (adversarial): a
/// `claimed` row returns to pending (re-admitted later); a `running` row
/// whose logical turn already ended (no active turn record) is retired to
/// `done` — re-admitting it would deliver the prompt twice; a `running`
/// row OWNED by an active turn record is left running for the queue
/// runner to resume and consume exactly once; and `abort(None)` can
/// cancel every non-terminal row, running rows included.
#[test]
fn queue_recovery_classifies_running_rows_by_turn_record_ownership() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    // Row 1: admitted but never driven (claimed) — crash between claim
    // and the first drive.
    store
        .enqueue_prompt(s.id, OpId::new(11), "one", &[], None, None, None, 1)
        .unwrap();
    // Row 2: admitted, drive started (running) and its turn record is
    // ACTIVE — the recoverable interrupted turn.
    store
        .enqueue_prompt(s.id, OpId::new(12), "two", &[], None, None, None, 2)
        .unwrap();
    // Row 3: running with NO active turn record — the turn ended but the
    // terminal mark was lost. Rows 2 and 3 both run in one pass.
    store
        .enqueue_prompt(s.id, OpId::new(13), "three", &[], None, None, None, 3)
        .unwrap();
    store
        .raw_conn()
        .execute(
            "UPDATE prompt_queue SET status = CASE seq
                 WHEN 1 THEN 'claimed'
                 WHEN 2 THEN 'running'
                 ELSE 'running' END
             WHERE session_id = ?1",
            params![s.id.raw() as i64],
        )
        .unwrap();
    store
        .start_turn_record(s.id, OpId::new(12), Some(2), None, "p", "m", None)
        .unwrap();

    let reclaimed = store.recover_claimed_queue_rows(s.id).unwrap();
    assert_eq!(reclaimed, 1, "the claimed row returns to pending");
    let statuses: Vec<(i64, String)> = {
        let conn = store.read().unwrap();
        let mut stmt = conn
            .prepare("SELECT seq, status FROM prompt_queue WHERE session_id = ?1 ORDER BY seq")
            .unwrap();
        let rows = stmt
            .query_map(params![s.id.raw() as i64], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    };
    assert_eq!(
        statuses,
        vec![
            (1, "pending".to_string()),
            (2, "running".to_string()),
            (3, "done".to_string()),
        ],
        "claimed -> pending; owned running stays; orphaned running is retired"
    );
    // The runnable marker still lists the session (rows 1/2 remain).
    assert_eq!(
        store.sessions_with_pending_queues().unwrap(),
        vec![s.id],
        "the runnable marker keeps listing the session"
    );
    // Idempotent: a second recovery changes nothing.
    assert_eq!(store.recover_claimed_queue_rows(s.id).unwrap(), 0);
    // abort(None) semantics: every non-terminal row (pending AND
    // running) is cancellable through the op-id scan.
    let ops = store.queue_op_ids(s.id).unwrap();
    assert_eq!(
        ops,
        vec![OpId::new(11), OpId::new(12)],
        "queue_op_ids covers pending and running rows"
    );
    assert_eq!(store.cancel_queued_ops(s.id, &ops).unwrap(), 2);
    assert!(store.sessions_with_pending_queues().unwrap().is_empty());
    assert!(store.queue_op_ids(s.id).unwrap().is_empty());
}

#[test]
fn corrupt_session_state_refuses_admission_typed_without_claiming() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    store
        .enqueue_prompt(s.id, OpId::new(1), "hi", &[], None, None, None, 1)
        .unwrap();
    {
        let conn = store.raw_conn();
        conn.execute(
            "UPDATE session SET state = 'not json' WHERE id = ?1",
            params![s.id.raw() as i64],
        )
        .unwrap();
    }
    match store.admit_queue_head(s.id, &["idle"], "preparing") {
        Err(StoreError::Corrupt(msgs)) => assert!(
            msgs.iter().any(|m| m.contains("state")),
            "the refusal must name the state column: {msgs:?}"
        ),
        other => panic!("corrupt session state must refuse admission typed: {other:?}"),
    }
    let status: String = store
        .read()
        .unwrap()
        .query_row(
            "SELECT status FROM prompt_queue WHERE session_id = ?1",
            params![s.id.raw() as i64],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        status, "pending",
        "a refused admission must not claim the row"
    );
}

fn pending_queue_row_with_files(store: &Store, files_json: &str) -> SessionId {
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    store
        .enqueue_prompt(s.id, OpId::new(1), "hi", &[], None, None, None, 1)
        .unwrap();
    store
        .raw_conn()
        .execute(
            "UPDATE prompt_queue SET files = ?2 WHERE session_id = ?1 AND seq = 1",
            params![s.id.raw() as i64, files_json],
        )
        .unwrap();
    s.id
}

/// The `files` column is a string array or it is corrupt: JSON1's
/// `json_each.value` dequotes text elements, so `json_type(value)` parses
/// `src/lib.rs` as JSON and the old predicate raised "malformed JSON" for
/// EVERY non-empty list (and silently passed `[null]`). The admission
/// validation must classify on `json_each.type`, accept exactly the empty
/// array and string arrays, and refuse everything else typed with zero
/// mutation.
#[test]
fn queue_admission_files_column_accepts_only_json_string_arrays() {
    for (files_json, accepted) in [
        ("[]", Some(Vec::<String>::new())),
        (r#"["src/lib.rs"]"#, Some(vec!["src/lib.rs".to_string()])),
        (r#"["a","b"]"#, Some(vec!["a".to_string(), "b".to_string()])),
        ("[1]", None),
        ("[null]", None),
        (r#"{"x":"a"}"#, None),
        ("not-json", None),
    ] {
        let (_d, store) = tmp_store();
        let s = pending_queue_row_with_files(&store, files_json);
        match (store.admit_queue_head(s, &["idle"], "preparing"), accepted) {
            (Ok(Some((admitted, _))), Some(expected)) => {
                assert_eq!(
                    admitted.files, expected,
                    "files {files_json} must round-trip"
                );
                let status: String = store
                    .read()
                    .unwrap()
                    .query_row(
                        "SELECT status FROM prompt_queue WHERE session_id = ?1",
                        params![s.raw() as i64],
                        |r| r.get(0),
                    )
                    .unwrap();
                assert_eq!(status, "claimed");
                assert_eq!(
                    store.messages_before(s, None, 10).unwrap().len(),
                    1,
                    "exactly one user message per admitted prompt"
                );
                assert!(
                    store
                        .admit_queue_head(s, &["idle"], "preparing")
                        .unwrap()
                        .is_none(),
                    "a re-admit must be a no-op: the head was claimed"
                );
                assert_eq!(store.messages_before(s, None, 10).unwrap().len(), 1);
            }
            (Err(StoreError::Corrupt(msgs)), None) => {
                assert!(
                    msgs.iter().any(|m| m.contains("files")),
                    "{files_json}: the refusal must name the files column: {msgs:?}"
                );
                assert_admission_untouched(&store, s);
                assert_eq!(
                    store.get_session(s).unwrap().unwrap().state,
                    AgentState::Idle,
                    "a refused admission must not move the session"
                );
                assert!(
                    store.admit_queue_head(s, &["idle"], "preparing").is_err(),
                    "the refusal is deterministic: the corrupt row is still refused"
                );
            }
            (other, _) => panic!("files {files_json} classified {other:?}"),
        }
    }
}

/// Assert a refused admission left the durable world exactly as it was:
/// the head is still pending and no message was materialized.
fn assert_admission_untouched(store: &Store, s: SessionId) {
    let status: String = store
        .read()
        .unwrap()
        .query_row(
            "SELECT status FROM prompt_queue WHERE session_id = ?1 ORDER BY seq LIMIT 1",
            params![s.raw() as i64],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(status, "pending", "a refused admission must not claim");
    assert!(
        store.messages_before(s, None, 10).unwrap().is_empty(),
        "a refused admission must not materialize a message"
    );
}

/// The ineligible branch decodes the durable session text as a TYPED
/// `AgentState`: `"zombie"` is well-formed JSON but not a state, so it
/// must surface `Corrupt` (never a silent `Ok(None)` "not eligible"),
/// with zero queue/message/session mutation. A valid but genuinely
/// ineligible state is still the intended `Ok(None)`.
#[test]
fn admit_queue_head_decodes_durable_session_state_typed() {
    for (corrupt_state, needle) in [("\"zombie\"", "state"), ("not-json", "state")] {
        let (_d, store) = tmp_store();
        let s = pending_queue_row_with_files(&store, "[]");
        store
            .raw_conn()
            .execute(
                "UPDATE session SET state = ?2 WHERE id = ?1",
                params![s.raw() as i64, corrupt_state],
            )
            .unwrap();
        match store.admit_queue_head(s, &["idle"], "preparing") {
            Err(StoreError::Corrupt(msgs)) => assert!(
                msgs.iter().any(|m| m.contains(needle)),
                "{corrupt_state}: the refusal must name the state column: {msgs:?}"
            ),
            other => panic!("state {corrupt_state} must refuse typed, got {other:?}"),
        }
        let status: String = store
            .read()
            .unwrap()
            .query_row(
                "SELECT status FROM prompt_queue WHERE session_id = ?1",
                params![s.raw() as i64],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "pending");
        assert!(store.messages_before(s, None, 10).unwrap().is_empty());
        let durable: String = store
            .read()
            .unwrap()
            .query_row(
                "SELECT state FROM session WHERE id = ?1",
                params![s.raw() as i64],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(durable, corrupt_state, "the corrupt row is never rewritten");
    }
    let (_d, store) = tmp_store();
    let s = pending_queue_row_with_files(&store, "[]");
    store
        .raw_conn()
        .execute(
            "UPDATE session SET state = '\"completed\"' WHERE id = ?1",
            params![s.raw() as i64],
        )
        .unwrap();
    assert!(
        store
            .admit_queue_head(s, &["idle"], "preparing")
            .unwrap()
            .is_none(),
        "a valid ineligible state is a clean no-op"
    );
    assert_admission_untouched(&store, s);
    assert_eq!(
        store.get_session(s).unwrap().unwrap().state,
        AgentState::Completed,
        "a valid ineligible state is never rewritten"
    );
}

/// End-to-end queued-files case (the regression that was missing): while
/// a turn is active, a prompt with a real file list queues; the store's
/// public admission path (exactly the call `SessionHandle::
/// admit_next_queued` makes) then returns the SAME files, claims the row
/// and materializes the user message exactly once, and a re-admit is a
/// no-op.
#[test]
fn queued_prompt_with_files_admits_exactly_once_after_turn() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    for (from, to, kind) in [
        (
            AgentState::Idle,
            AgentState::Preparing,
            EventKind::PhaseChanged,
        ),
        (
            AgentState::Preparing,
            AgentState::BuildingContext,
            EventKind::PhaseChanged,
        ),
        (
            AgentState::BuildingContext,
            AgentState::WaitingForModel,
            EventKind::PhaseChanged,
        ),
        (
            AgentState::WaitingForModel,
            AgentState::Streaming,
            EventKind::PhaseChanged,
        ),
    ] {
        store
            .transition_session(
                s.id,
                Some(OpId::new(9)),
                SessionTransition {
                    expected_lifecycle: None,
                    new_lifecycle: None,
                    expected_state: Some(from),
                    new_state: to,
                    event_kind: kind,
                    event_payload: None,
                    event_payload_ver: 1,
                },
            )
            .unwrap();
    }
    store
        .enqueue_prompt(
            s.id,
            OpId::new(10),
            "queued with files",
            &["src/lib.rs".to_string()],
            None,
            None,
            None,
            7,
        )
        .unwrap();
    let handle_eligible = [
        "idle",
        "ready_for_next_turn",
        "cancelled",
        "failed_recoverable",
    ];
    assert!(
        store
            .admit_queue_head(s.id, &handle_eligible, "preparing")
            .unwrap()
            .is_none(),
        "a prompt queued during a streaming turn must not admit mid-turn"
    );
    assert_eq!(
        store.queue_status_counts(s.id).unwrap()["pending"].as_i64(),
        Some(1)
    );
    assert!(store.messages_before(s.id, None, 10).unwrap().is_empty());
    store
        .transition_session(
            s.id,
            Some(OpId::new(9)),
            SessionTransition {
                expected_lifecycle: None,
                new_lifecycle: None,
                expected_state: Some(AgentState::Streaming),
                new_state: AgentState::ReadyForNextTurn,
                event_kind: EventKind::TurnCompleted,
                event_payload: None,
                event_payload_ver: 1,
            },
        )
        .unwrap();
    let (admitted, message_seq) = store
        .admit_queue_head(s.id, &handle_eligible, "preparing")
        .unwrap()
        .expect("the queued prompt must admit once the turn ended");
    assert_eq!(admitted.op_id, OpId::new(10));
    assert_eq!(admitted.queue_seq, 1);
    assert_eq!(admitted.prompt, "queued with files");
    assert_eq!(admitted.files, vec!["src/lib.rs".to_string()]);
    assert_eq!(message_seq, admitted.message_seq);
    assert_eq!(
        store.get_session(s.id).unwrap().unwrap().state,
        AgentState::Preparing
    );
    assert_eq!(
        store.queue_status_counts(s.id).unwrap()["claimed"].as_i64(),
        Some(1)
    );
    let messages = store.messages_before(s.id, None, 10).unwrap();
    assert_eq!(messages.len(), 1, "materialized exactly once");
    assert_eq!(messages[0].seq, admitted.message_seq);
    assert_eq!(messages[0].role, "user");
    assert_eq!(messages[0].data["text"], "queued with files");
    assert!(
        store
            .admit_queue_head(s.id, &handle_eligible, "preparing")
            .unwrap()
            .is_none(),
        "re-admit must be a no-op: no pending head remains"
    );
    assert_eq!(store.messages_before(s.id, None, 10).unwrap().len(), 1);
    assert_eq!(
        store.queue_status_counts(s.id).unwrap()["claimed"].as_i64(),
        Some(1)
    );
}

// ------------------------------------------- v17 attempt-identity ledger
// (schema v18): refund-after-dispatch is SQL-impossible, reservations and
// provider-call rows key by attempt_op_id, delivery/cost-basis columns
// ride the row, crash recovery splits reserved-vs-dispatched.

/// Chunk-boundary correctness for the IN-list cancel: 1,201 ops cross
/// two 500-op chunks plus a partial one; only non-terminal rows cancel
/// (whatever their status), the count is exact, and a replay is a no-op.
#[test]
fn cancel_queued_ops_chunks_large_lists_exactly() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    const N: i64 = 1_201;
    {
        let conn = store.raw_conn();
        let tx = conn.unchecked_transaction().unwrap();
        let statuses = ["pending", "claimed", "running", "done", "cancelled"];
        for i in 0..N {
            tx.execute(
                "INSERT INTO prompt_queue(session_id, seq, op_id, prompt, status, requested_at)
                 VALUES (?1, ?2, ?3, 'p', ?4, 1)",
                params![
                    s.id.raw() as i64,
                    i + 1,
                    5_000 + i,
                    statuses[(i % statuses.len() as i64) as usize]
                ],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }
    let ops: Vec<OpId> = (0..N).map(|i| OpId::new(5_000 + i as u64)).collect();
    let expected_cancelled = (0..N).filter(|i| matches!(i % 5, 0..=2)).count() as i64;
    assert_eq!(
        store.cancel_queued_ops(s.id, &ops).unwrap(),
        expected_cancelled
    );
    assert_eq!(store.cancel_queued_ops(s.id, &ops).unwrap(), 0, "replay");
    let conn = store.raw_conn();
    let cancelled: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM prompt_queue WHERE session_id = ?1 AND status = 'cancelled'",
            params![s.id.raw() as i64],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cancelled, expected_cancelled + N / 5);
    assert_eq!(store.queue_op_ids(s.id).unwrap().len(), 0);
    assert!(store.sessions_with_pending_queues().unwrap().is_empty());
}
