//! `ledger` in-file unit tests (split from `ledger.rs` to stay under the
//! source-size ceiling; module identity and `super::*` visibility unchanged).

use super::*;

pub(crate) fn tmp_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let s = Store::open(dir.path(), true).unwrap();
    (dir, s)
}

#[test]
fn truncated_db_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("faktor-plus.db"), b"SQLite format 3\x00").unwrap();
    match Store::open(dir.path(), true) {
        Err(StoreError::Sqlite(_)) | Err(StoreError::Corrupt(_)) => {}
        other => panic!("truncated db must fail cleanly, got {other:?}"),
    }
}

#[test]
fn prompt_queue_sequence_exhaustion_refuses_typed_without_a_row() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    {
        let conn = store.raw_conn();
        conn.execute(
            "INSERT INTO prompt_queue(session_id, seq, op_id, prompt, files, status, requested_at)
             VALUES (?1, ?2, 1, 'hostile', '[]', 'pending', 0)",
            params![s.id.raw() as i64, i64::MAX],
        )
        .unwrap();
    }
    let err = store
        .enqueue_prompt(s.id, OpId::new(2), "next", &[], None, None, None, 0)
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Corrupt(ref msgs)
            if msgs.iter().any(|m| m.contains("exhausted"))),
        "the i64::MAX queue seq must refuse as exhaustion: {err:?}"
    );
    let rows: i64 = store
        .raw_conn()
        .query_row(
            "SELECT COUNT(*) FROM prompt_queue WHERE session_id = ?1",
            params![s.id.raw() as i64],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 1, "the refused enqueue must insert no row");
}

#[test]
fn workspace_root_roundtrip_and_unknown() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/root/x").unwrap();
    assert_eq!(
        store.workspace_root(ws).unwrap().as_deref(),
        Some("/root/x")
    );
    assert_eq!(store.workspace_root(WorkspaceId::new(999)).unwrap(), None);
}

#[test]
fn workspace_create_is_idempotent() {
    let (_d, store) = tmp_store();
    let a = store.create_workspace("/same").unwrap();
    let b = store.create_workspace("/same").unwrap();
    assert_eq!(a, b);
    let c = store.create_workspace("/other").unwrap();
    assert_ne!(a, c);
}

#[test]
fn adversarial_duplicate_event_append_is_structurally_impossible() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    // Try to insert a duplicate (session, seq) by bypassing the API:
    // the PRIMARY KEY must reject it.
    let conn = store.raw_conn();
    let r = conn.execute(
        "INSERT INTO event(seq, session_id, op_id, kind, state, ts_ms, payload) VALUES (1, ?1, NULL, 'model_started', '\"streaming\"', 0, NULL)",
        params![s.id.raw() as i64],
    );
    assert!(r.is_err(), "duplicate (session,seq) must be rejected by PK");
}

fn event_row_count(store: &Store, sid: SessionId) -> i64 {
    let conn = store.raw_conn();
    conn.query_row(
        "SELECT COUNT(*) FROM event WHERE session_id = ?1",
        params![sid.raw() as i64],
        |r| r.get(0),
    )
    .unwrap()
}

/// Hostile raw write of the session's event seq: `CHECK (seq > 0)` is
/// bypassed the way an attacker with file access (or a database written
/// before the CHECK existed) bypasses it, so the append guard itself must
/// be what refuses.
pub(crate) fn seed_only_event_seq(store: &Store, sid: SessionId, seq: i64) {
    let conn = store.raw_conn();
    conn.execute_batch("PRAGMA ignore_check_constraints = ON")
        .unwrap();
    let changed = conn
        .execute(
            "UPDATE event SET seq = ?2 WHERE session_id = ?1",
            params![sid.raw() as i64, seq],
        )
        .unwrap();
    assert_eq!(changed, 1, "the hostile seed must rewrite the event row");
}

/// A refused append must leave the journal and the session row exactly as
/// they were: no event row appeared and neither `state` nor `updated_ms`
/// moved.
fn assert_append_refused_without_mutation(store: &Store, before: &SessionRow, rows: i64) {
    assert_eq!(
        event_row_count(store, before.id),
        rows,
        "a refused append must insert no event row"
    );
    let after = store.get_session(before.id).unwrap().unwrap();
    assert_eq!(after.state, before.state, "session state must be untouched");
    assert_eq!(
        after.updated_ms, before.updated_ms,
        "session updated_ms must be untouched"
    );
}

fn hostile_append(store: &Store, sid: SessionId) -> StoreError {
    store
        .append_event(
            sid,
            None,
            EventKind::PromptReceived,
            AgentState::Preparing,
            now_ms(),
            None,
        )
        .unwrap_err()
}

#[test]
fn hostile_i64_max_prev_seq_append_refuses_typed_without_mutation() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    seed_only_event_seq(&store, s.id, i64::MAX);
    match hostile_append(&store, s.id) {
        StoreError::Corrupt(msgs) => assert!(
            msgs.iter().any(|m| m.contains("overflow")),
            "the signed-boundary refusal must name the overflow: {msgs:?}"
        ),
        other => panic!("i64::MAX previous seq must refuse typed Corrupt, got {other:?}"),
    }
    assert_eq!(
        store.last_event_seq(s.id).unwrap().unwrap().raw(),
        i64::MAX as u64,
        "the hostile row itself is untouched, never wrapped"
    );
    assert_append_refused_without_mutation(&store, &s, 1);
}

#[test]
fn hostile_negative_prev_seq_append_refuses_typed_without_mutation() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    seed_only_event_seq(&store, s.id, -1);
    match hostile_append(&store, s.id) {
        StoreError::Corrupt(msgs) => assert!(
            msgs.iter().any(|m| m.contains("overflow")),
            "a -1 seq (the u64 upper half) must refuse as overflow: {msgs:?}"
        ),
        other => panic!("a negative previous seq must refuse typed Corrupt, got {other:?}"),
    }
    assert_eq!(
        store.last_event_seq(s.id).unwrap().unwrap().raw(),
        u64::MAX,
        "-1 decodes as u64::MAX, the exact upper-half read is preserved"
    );
    assert_append_refused_without_mutation(&store, &s, 1);
}

#[test]
fn hostile_zero_prev_seq_append_refuses_typed_without_mutation() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    seed_only_event_seq(&store, s.id, 0);
    match hostile_append(&store, s.id) {
        StoreError::Corrupt(msgs) => assert!(
            msgs.iter().any(|m| m.contains("MAX(seq)")),
            "a zero seq must refuse through the decode context: {msgs:?}"
        ),
        other => panic!("a zero previous seq must refuse typed Corrupt, got {other:?}"),
    }
    assert_append_refused_without_mutation(&store, &s, 1);
}

#[test]
fn seq_at_i64_max_minus_one_appends_once_then_refuses_at_the_boundary() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    seed_only_event_seq(&store, s.id, i64::MAX - 1);
    let seq = store
        .append_event(
            s.id,
            None,
            EventKind::PromptReceived,
            AgentState::Preparing,
            now_ms(),
            None,
        )
        .expect("the last signed seq is still usable");
    assert_eq!(seq.raw(), i64::MAX as u64);
    let before = store.get_session(s.id).unwrap().unwrap();
    match hostile_append(&store, s.id) {
        StoreError::Corrupt(msgs) => assert!(
            msgs.iter().any(|m| m.contains("overflow")),
            "the next append must name the overflow: {msgs:?}"
        ),
        other => panic!("the i64::MAX boundary must refuse typed Corrupt, got {other:?}"),
    }
    assert_append_refused_without_mutation(&store, &before, 2);
}

#[test]
fn hostile_prev_ts_type_append_refuses_typed_without_mutation() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    {
        let conn = store.raw_conn();
        conn.execute(
            "UPDATE event SET ts_ms = 'hostile-text' WHERE session_id = ?1",
            params![s.id.raw() as i64],
        )
        .unwrap();
    }
    let err = hostile_append(&store, s.id);
    assert!(
        matches!(err, StoreError::Corrupt(_) | StoreError::Sqlite(_)),
        "a non-integer previous ts must refuse typed, never default to 0: {err:?}"
    );
    assert_append_refused_without_mutation(&store, &s, 1);
}

#[test]
fn append_ts_is_monotonic_and_seq_is_gapless() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    let seed_ts = store.events_range(s.id, 1, None).unwrap()[0].ts_ms;
    let seq2 = store
        .append_event(
            s.id,
            None,
            EventKind::PromptReceived,
            AgentState::Preparing,
            seed_ts - 60_000,
            None,
        )
        .unwrap();
    assert_eq!(seq2.raw(), 2);
    let seq3 = store
        .append_event(
            s.id,
            None,
            EventKind::ModelStarted,
            AgentState::Streaming,
            seed_ts + 60_000,
            None,
        )
        .unwrap();
    assert_eq!(seq3.raw(), 3);
    let events = store.events_range(s.id, 1, None).unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0].ts_ms, seed_ts);
    assert_eq!(
        events[1].ts_ms, seed_ts,
        "a backward clock clamps to the previous ts"
    );
    assert_eq!(events[2].ts_ms, seed_ts + 60_000);
    for (i, e) in events.iter().enumerate() {
        assert_eq!(e.seq.raw(), (i + 1) as u64, "gapless seq at {i}");
    }
}

#[test]
fn memory_facts_upsert_and_query() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    store
        .upsert_memory_fact(s.id, "decision", "framework", "rust")
        .unwrap();
    store
        .upsert_memory_fact(s.id, "decision", "framework", "rust+tokio")
        .unwrap();
    let facts = store.memory_facts(s.id).unwrap();
    assert_eq!(facts.len(), 1, "upsert must not duplicate");
    assert_eq!(facts[0].2, "rust+tokio");
}

#[test]
fn corrupt_event_kind_returns_error_not_panic() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    {
        let conn = store.raw_conn();
        conn.execute(
            "INSERT INTO event(seq, session_id, op_id, kind, state, ts_ms, payload)
             VALUES (2, ?1, NULL, 'bogus', '\"idle\"', 0, NULL)",
            params![s.id.raw() as i64],
        )
        .unwrap();
    }
    match store.events_range(s.id, 1, None) {
        Err(StoreError::Corrupt(_)) => {}
        other => panic!("unknown event kind must error, not panic: {other:?}"),
    }
}

#[test]
fn corrupt_event_state_returns_error_not_panic() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    {
        let conn = store.raw_conn();
        conn.execute(
            "INSERT INTO event(seq, session_id, op_id, kind, state, ts_ms, payload)
             VALUES (2, ?1, NULL, 'model_started', '\"not_a_state\"', 0, NULL)",
            params![s.id.raw() as i64],
        )
        .unwrap();
    }
    match store.events_range(s.id, 1, None) {
        Err(StoreError::Corrupt(_)) => {}
        other => panic!("corrupt event state must error, not panic: {other:?}"),
    }
}

pub(crate) fn end_transition() -> SessionTransition {
    SessionTransition {
        expected_lifecycle: Some(SessionLifecycle::Open),
        new_lifecycle: Some(SessionLifecycle::Closed),
        expected_state: None,
        new_state: AgentState::Completed,
        event_kind: EventKind::SessionEnded,
        event_payload: None,
        event_payload_ver: 1,
    }
}

#[test]
fn expected_state_mismatch_same() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    store
        .append_event(
            s.id,
            None,
            EventKind::PromptReceived,
            AgentState::Preparing,
            now_ms(),
            None,
        )
        .unwrap();
    let mut t = end_transition();
    // The turn machine is mid-turn (Preparing), not Idle.
    t.expected_state = Some(AgentState::Idle);
    let err = store.transition_session(s.id, None, t).unwrap_err();
    assert!(
        matches!(err, StoreError::Conflict(_)),
        "expected state mismatch must be Conflict, got {err:?}"
    );
    // No SessionEnded row, lifecycle still Open, state still Preparing.
    assert_eq!(store.last_event_seq(s.id).unwrap().unwrap().raw(), 2);
    let row = store.get_session(s.id).unwrap().unwrap();
    assert_eq!(row.lifecycle, SessionLifecycle::Open);
    assert_eq!(row.state, AgentState::Preparing);
}

#[test]
fn alloc_op_ids_rejects_zero_count_and_sequence_exhaustion() {
    let (_d, store) = tmp_store();
    let hw0 = store.op_id_seq_high_water().unwrap();
    assert!(store.alloc_op_ids(SessionId::new(1), 0).is_err());
    // A reservation crossing the signed INTEGER column ceiling is
    // refused and writes nothing (checked before the UPDATE).
    let overflow = i64::MAX as u64 - hw0 + 1;
    assert!(store.alloc_op_ids(SessionId::new(1), overflow).is_err());
    assert_eq!(
        store.op_id_seq_high_water().unwrap(),
        hw0,
        "failed reservations must not move the sequence"
    );
}

#[test]
fn op_id_seq_ranges_never_overlap_across_live_instances() {
    // Two LIVE stores over the same file (a restart racing its
    // predecessor before the old connection is gone): every reservation
    // must be an atomic read+update, so no two ranges overlap and the
    // global order is preserved under contention.
    let dir = tempfile::tempdir().unwrap();
    let a = Arc::new(Store::open(dir.path(), true).unwrap());
    let b = Arc::new(Store::open(dir.path(), true).unwrap());
    let ranges = Arc::new(std::sync::Mutex::new(Vec::<(u64, u64)>::new()));
    let mut handles = Vec::new();
    for store in [
        a.clone(),
        a.clone(),
        a.clone(),
        b.clone(),
        b.clone(),
        b.clone(),
    ] {
        let ranges = ranges.clone();
        handles.push(std::thread::spawn(move || {
            for _ in 0..25 {
                let (start, n) = store.alloc_op_ids(SessionId::new(1), 40).unwrap();
                ranges.lock().unwrap().push((start, n));
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let mut ranges = ranges.lock().unwrap().clone();
    ranges.sort_by_key(|(start, _)| *start);
    assert_eq!(ranges.len(), 150, "6 threads x 25 reservations");
    for w in ranges.windows(2) {
        let (s0, n0) = w[0];
        let (s1, _n1) = w[1];
        assert!(s1 > s0, "reservation starts strictly increase");
        assert!(
            s1 >= s0 + n0,
            "ranges never overlap: [{s0}, {}) vs [{s1}, {})",
            s0 + n0,
            s1 + _n1
        );
    }
}

pub(crate) fn prefix_hash(seed: u8) -> [u8; 32] {
    [seed; 32]
}

/// One strict observation payload: `n` distinct 64-char hex digests and
/// one token count per segment.
pub(crate) fn segments_json(n: usize, tokens: &[u64]) -> String {
    let hashes: Vec<String> = (0..n)
        .map(|i| format!("{:02x}", i as u8).repeat(32))
        .collect();
    serde_json::json!({
        "segment_hashes": hashes,
        "segment_token_counts": tokens,
        "cache_read_tokens": 7u64,
    })
    .to_string()
}

#[test]
fn malformed_prefix_segments_writes_are_refused_before_any_row_lands() {
    // The typed API is the first gate: malformed/oversized/shape-suspect payloads never
    // touch a row; every refusal is typed.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    let write = |json: &str| {
        store.record_provider_call_with_prefix_segments(
            s.id,
            OpId::new(1),
            "p",
            "m",
            "completed",
            None,
            None,
            None,
            Some(prefix_hash(1)),
            Some(1),
            None,
            Some(json),
        )
    };
    for (json, what) in [
        ("not json", "garbage"),
        ("{", "truncated"),
        (
            r#"{"segment_hashes":[],"segment_token_counts":[1],"cache_read_tokens":0}"#,
            "length mismatch",
        ),
        (
            r#"{"segment_hashes":["zz"],"segment_token_counts":[1],"cache_read_tokens":0}"#,
            "non-hex digest",
        ),
        (
            r#"{"segment_hashes":[],"segment_token_counts":[],"cache_read_tokens":0,"extra":1}"#,
            "unknown field",
        ),
    ] {
        assert!(
            matches!(write(json), Err(StoreError::Malformed(_))),
            "{what} must be a loud Malformed"
        );
    }
    // Over the byte bound: Oversized, never stored.
    let oversized = format!(
        r#"{{"segment_hashes":[],"segment_token_counts":[],"cache_read_tokens":0,"pad":"{}"}}"#,
        "x".repeat(MAX_PREFIX_SEGMENTS_JSON)
    );
    assert!(matches!(write(&oversized), Err(StoreError::Oversized(_))));
    // 65 segments: over the segment bound.
    let too_many = segments_json(
        MAX_PREFIX_SEGMENTS + 1,
        &vec![1u64; MAX_PREFIX_SEGMENTS + 1],
    );
    assert!(matches!(write(&too_many), Err(StoreError::Malformed(_))));
    // Nothing landed.
    assert!(store.provider_call_prefix_rows(s.id).unwrap().is_empty());
}

#[test]
fn oversized_and_malformed_writes_are_rejected_loudly() {
    // (d) The typed API refuses oversized token counts and malformed
    // stability values BEFORE anything touches the row.
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    let err = store
        .record_provider_call_with_prefix(
            s.id,
            OpId::new(1),
            "p",
            "m",
            "completed",
            None,
            None,
            None,
            Some(prefix_hash(1)),
            Some(u32::MAX as u64 + 1),
            None,
        )
        .unwrap_err();
    assert!(
        matches!(err, StoreError::Oversized(_)),
        "oversized tokens must be rejected loudly: {err:?}"
    );
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.01, 1.0001] {
        let err = store
            .record_provider_call_with_prefix(
                s.id,
                OpId::new(1),
                "p",
                "m",
                "completed",
                None,
                None,
                None,
                Some(prefix_hash(1)),
                Some(100),
                Some(bad),
            )
            .unwrap_err();
        assert!(
            matches!(err, StoreError::Malformed(_)),
            "stability {bad} must be rejected loudly: {err:?}"
        );
    }
    // The u32 boundary itself is accepted.
    store
        .record_provider_call_with_prefix(
            s.id,
            OpId::new(2),
            "p",
            "m",
            "completed",
            None,
            None,
            None,
            Some(prefix_hash(2)),
            Some(u32::MAX as u64),
            Some(0.0),
        )
        .unwrap();
    // Nothing was recorded by the rejected attempts.
    let rows = store.provider_call_prefix_rows(s.id).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].prompt_tokens, u32::MAX);
    assert_eq!(rows[0].prefix_stability, Some(0.0));
}

#[test]
fn quick_check_and_deep_check_agree_on_healthy_stores() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    store
        .append_event(
            s.id,
            None,
            EventKind::ModelChunkReceived,
            AgentState::Streaming,
            now_ms(),
            None,
        )
        .unwrap();
    assert!(store.quick_integrity_check().unwrap().is_empty());
    assert!(store.deep_integrity_check().unwrap().is_empty());
    let d = store.diagnostics_quick().unwrap();
    assert_eq!(d["journal_mode"], "wal");
    assert_eq!(d["sessions"], 1);
    assert_eq!(d["integrity"], serde_json::json!([]));
}

pub(crate) fn hot_session(store: &Store) -> SessionId {
    let ws = store.create_workspace("/w").unwrap();
    store.create_session(ws, "t", "p", "m").unwrap().id
}

pub(crate) fn hot_session_sids(store: &Store, n: u64) -> Vec<SessionId> {
    let ws = store.create_workspace("/w").unwrap();
    (1..=n)
        .map(|_| store.create_session(ws, "t", "p", "m").unwrap().id)
        .collect()
}

#[test]
fn batch_hot_writes_commit_in_one_group_and_fsync_before_returning() {
    let (_d, store) = tmp_store();
    let sid = hot_session(&store);
    // 1 event (session seed is seq 1) + 1 message + 2 parts on it + one
    // usage-settlement row.
    let writes = vec![
        HotWrite::AppendEvent {
            session_id: sid,
            op_id: Some(OpId::new(7)),
            kind: EventKind::ModelStarted,
            state: AgentState::Streaming,
            ts_ms: 100,
            payload: None,
            payload_ver: 1,
        },
        HotWrite::PutMessage {
            session_id: sid,
            // The runtime aligns message seqs with journal event seqs.
            seq: 2,
            role: "assistant".into(),
            data: serde_json::json!({ "parts": [] }),
        },
    ];
    let (out, timing) = store.batch_hot_writes(&writes).unwrap();
    assert_eq!(out.len(), 2);
    assert!(timing.commit_us > 0, "fsync commit is measured");
    let out = out;
    let seq = match &out[0] {
        Ok(HotWriteOutcome::EventSeq(s)) => s.raw(),
        other => panic!("expected event seq, got {other:?}"),
    };
    assert_eq!(seq, 2, "second journal event of the session");
    let mid = match &out[1] {
        Ok(HotWriteOutcome::RowId(id)) => *id,
        other => panic!("expected row id, got {other:?}"),
    };
    // The whole group was one transaction: the message and the event are
    // both durable and coherent (message seq == journal seq 2).
    assert!(store.get_session(sid).unwrap().is_some());
    let msgs = store.messages_before(sid, None, 10).unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].seq, 2);
    // A follow-up batch can reference the row of an earlier one.
    let parts = vec![
        HotWrite::PutPart {
            message_id: mid,
            kind: "text".into(),
            data: serde_json::json!({ "text": "hello" }),
        },
        HotWrite::RecordProviderCall {
            session_id: sid,
            op_id: OpId::new(7),
            provider: "ollama".into(),
            model: "qwen3.8".into(),
            status: "completed".into(),
            tokens_in: Some(100),
            tokens_out: Some(50),
            error: None,
        },
    ];
    let (out, _) = store.batch_hot_writes(&parts).unwrap();
    assert!(out.iter().all(|r| r.is_ok()), "parts + usage must commit");
    assert_eq!(store.parts_of(mid).unwrap().len(), 1);
    assert_eq!(store.session_usage_tokens(sid).unwrap(), 150);
}

#[test]
fn batch_hot_writes_isolate_a_failing_write_to_its_savepoint() {
    // One hostile write in the group (duplicate (session, seq) message)
    // must fail ONLY itself: neighbors commit, per-write errors are
    // reported, and the writer connection is left transaction-free.
    let (_d, store) = tmp_store();
    let sids = hot_session_sids(&store, 2);
    let writes = vec![
        HotWrite::PutMessage {
            session_id: sids[0],
            seq: 1,
            role: "user".into(),
            data: serde_json::json!({ "text": "a" }),
        },
        // Duplicate (session, seq) on the SAME session: constraint hit.
        HotWrite::PutMessage {
            session_id: sids[0],
            seq: 1,
            role: "user".into(),
            data: serde_json::json!({ "text": "b" }),
        },
        // Foreign key violation: no such message row.
        HotWrite::PutPart {
            message_id: i64::MAX,
            kind: "text".into(),
            data: serde_json::json!({ "text": "orphan" }),
        },
        // Unrelated session must still land.
        HotWrite::PutMessage {
            session_id: sids[1],
            seq: 1,
            role: "user".into(),
            data: serde_json::json!({ "text": "c" }),
        },
    ];
    let (out, _) = store.batch_hot_writes(&writes).unwrap();
    assert!(out[0].is_ok(), "first insert must commit");
    assert!(out[1].is_err(), "duplicate seq must fail in its savepoint");
    assert!(out[2].is_err(), "orphan part must fail in its savepoint");
    assert!(
        out[3].is_ok(),
        "the unrelated session must survive the group"
    );
    assert_eq!(
        store.message_count(sids[0]).unwrap(),
        1,
        "only the first (session, seq) row exists"
    );
    assert_eq!(store.message_count(sids[1]).unwrap(), 1);
    // Empty groups are a no-op and never touch the writer.
    assert_eq!(store.batch_hot_writes(&[]).unwrap().0.len(), 0);
}
