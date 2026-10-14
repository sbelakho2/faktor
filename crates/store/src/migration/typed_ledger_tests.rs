use super::*;

pub(crate) fn tmp_store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path(), true).unwrap();
    (dir, store)
}

#[test]
fn migration_v14_replays_cleanly_on_a_v13_store() {
    // Simulate a v13 store (typed task rows WITHOUT the revision column,
    // no verification_record table): reopen must add the column
    // (backfilling every legacy row to revision 1), create the record
    // table, and keep legacy rows readable.
    let dir = tempfile::tempdir().unwrap();
    let (sid, tid) = {
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let row = TaskRow {
            task_id: TaskId::new(1),
            session_id: s.id,
            goal: "legacy goal".into(),
            acceptance_criteria: vec!["cargo check".into()],
            plan: vec![],
            attachments: vec![],
            contract: Default::default(),
            max_tokens: None,
            max_turns: None,
            spent_tokens: 0,
            spent_turns: 0,
            state: TaskState::Running,
            revision: TaskRevision::new(1),
            created_ms: 5,
            updated_ms: 5,
        };
        store.upsert_task(&row).unwrap();
        {
            let conn = store.raw_conn();
            conn.execute("ALTER TABLE task DROP COLUMN revision", [])
                .unwrap();
            conn.execute("DROP TABLE verification_record", []).unwrap();
            // The v15 cost-ledger objects are post-this-version too:
            // drop them so the full chain (past v15) replays cleanly.
            conn.execute("DROP TABLE cost_reservation", []).unwrap();
            conn.execute("ALTER TABLE task DROP COLUMN max_cost_micro", [])
                .unwrap();
            conn.execute("ALTER TABLE task DROP COLUMN spent_cost_micro", [])
                .unwrap();
            // The v17 (attempt-identity) provider_call columns are
            // post-this-version too.
            conn.execute("DROP INDEX IF EXISTS idx_provider_call_session_attempt", [])
                .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_op_id", [])
                .unwrap();
            conn.execute(
                "ALTER TABLE provider_call DROP COLUMN parent_model_call_op_id",
                [],
            )
            .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_ordinal", [])
                .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN reservation_id", [])
                .unwrap();
            // The v19 segment-observation column is post-this-version
            // too: drop it so the full chain (past v19) replays cleanly.
            conn.execute(
                "ALTER TABLE provider_call DROP COLUMN prefix_segments_json",
                [],
            )
            .unwrap();
            // v24 attachments are post-this-version: drop them (tolerantly,
            // some legacy shapes lack the table/column) so the full chain
            // replays cleanly on reopen.
            let _ = conn.execute("ALTER TABLE task DROP COLUMN attachments", []);
            let _ = conn.execute("ALTER TABLE task_ledger DROP COLUMN attachments", []);
            let _ = conn.execute("DROP TABLE IF EXISTS attachment", []);
            // The v30 shell-attribution column is post-this-version too:
            // drop it (tolerantly) so the full chain replays cleanly.
            let _ = conn.execute("ALTER TABLE tool_run DROP COLUMN pre_manifest", []);
            let _ = conn.execute("ALTER TABLE task DROP COLUMN contract", []);
            conn.execute("PRAGMA user_version = 14", []).unwrap();
        }
        (s.id, TaskId::new(1))
    };
    let store = Store::open(dir.path(), true).unwrap();
    // The pre-v14 row survived and reads back at revision 1 (DEFAULT
    // backfill), with its content intact.
    let back = store.get_task(sid, tid).unwrap().unwrap();
    assert_eq!(back.state, TaskState::Running);
    assert_eq!(back.revision, TaskRevision::new(1));
    assert_eq!(back.acceptance_criteria, vec!["cargo check".to_string()]);
    assert_eq!(back.created_ms, 5);
    // The new surface is writable and readable.
    let mut row = back.clone();
    row.revision = TaskRevision::new(2);
    store.upsert_task(&row).unwrap();
    assert_eq!(
        store.get_task(sid, tid).unwrap().unwrap().revision,
        TaskRevision::new(2)
    );
    let rec = VerificationRecordRow {
        id: VerificationRecordId::new(1),
        task_id: tid,
        revision: TaskRevision::new(2),
        workspace_id: WorkspaceId::new(1),
        worktree_id: WorktreeId::new(1),
        tree_hash: None,
        criteria: vec![],
        checks: vec![],
        changed_files: vec![],
        unrelated_changes: vec![],
        reviewer: None,
        status: VerificationStatus::Running,
        started_ms: 1,
        completed_ms: None,
    };
    let rec_id = store.verification_record_put(&rec).unwrap();
    assert_eq!(rec_id, VerificationRecordId::new(1));
    // Reopen again: migration is a no-op; both rows survive.
    drop(store);
    let store = Store::open(dir.path(), true).unwrap();
    assert_eq!(
        store.get_task(sid, tid).unwrap().unwrap().revision,
        TaskRevision::new(2)
    );
    assert_eq!(
        store
            .verification_record_get(rec_id)
            .unwrap()
            .unwrap()
            .task_id,
        tid
    );
}

#[test]
fn migration_v16_renames_legacy_abandoned_reservations_to_uncertain_and_adds_the_marker() {
    // Simulate a v15 store (cost_reservation WITHOUT dispatched_ms /
    // pricing_snapshot_json and with the legacy 'abandoned' state), then
    // reopen: the v16 reservation-table migration must rebuild the table
    // — legacy 'abandoned' rows become 'uncertain' (their prediction
    // KEEPS consuming the reserved amount), pre-v17 rows read as
    // never-dispatched and unpriced, and the CHECK now forbids
    // 'abandoned' outright. The store reopens through the CURRENT
    // migration chain (v16 then v17), so the legacy v15 `open` state
    // ends at the v17 `reserved` vocabulary.
    let dir = tempfile::tempdir().unwrap();
    let (sid, tid) = {
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let task = seed_task(&store, s.id, TaskId::new(1), vec![], TaskState::Running);
        let tid = task.task_id;
        store.cost_task_cap_set(s.id, tid, Some(1_000)).unwrap();
        {
            let conn = store.raw_conn();
            // Downgrade the post-v16 objects this rewind replays (the
            // v18 attempt columns on provider_call; the v16 migration
            // itself rebuilds cost_reservation from the v15 shape
            // below).
            conn.execute("DROP INDEX IF EXISTS idx_provider_call_session_attempt", [])
                .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_op_id", [])
                .unwrap();
            conn.execute(
                "ALTER TABLE provider_call DROP COLUMN parent_model_call_op_id",
                [],
            )
            .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_ordinal", [])
                .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN reservation_id", [])
                .unwrap();
            // The v19 segment-observation column is post-this-version
            // too: drop it so the full chain (past v19) replays cleanly.
            conn.execute(
                "ALTER TABLE provider_call DROP COLUMN prefix_segments_json",
                [],
            )
            .unwrap();
            // The v20 verification-record evidence columns are
            // post-this-version too (the table itself predates neither
            // test): drop them so the full chain (past v20) replays.
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
            // Rebuild the table in its v15 shape (old CHECK, no marker,
            // no snapshot column) and seed one row per legacy status.
            conn.execute("DROP TABLE cost_reservation", []).unwrap();
            conn.execute(
                    "CREATE TABLE cost_reservation (
                        reservation_id INTEGER PRIMARY KEY AUTOINCREMENT,
                        session_id INTEGER NOT NULL,
                        task_id INTEGER NOT NULL,
                        op_id INTEGER NOT NULL,
                        predicted_micro INTEGER NOT NULL,
                        status TEXT NOT NULL CHECK (status IN ('open', 'settled', 'refunded', 'abandoned')),
                        created_ms INTEGER NOT NULL,
                        settled_ms INTEGER,
                        provider_cost_micro INTEGER,
                        provider_reported_micro INTEGER,
                        route_decision_json TEXT
                     )",
                    [],
                )
                .unwrap();
            conn.execute(
                "INSERT INTO cost_reservation
                        (reservation_id, session_id, task_id, op_id, predicted_micro, status,
                         created_ms, settled_ms, provider_cost_micro, provider_reported_micro,
                         route_decision_json)
                     VALUES
                        (1, ?1, ?2, 1, 800, 'abandoned', 1, NULL, NULL, NULL, NULL),
                        (2, ?1, ?2, 2, 100, 'open', 1, NULL, NULL, NULL, NULL),
                        (3, ?1, ?2, 3, 300, 'settled', 1, 9, 250, 250, '{\"provider\":\"p\"}'),
                        (4, ?1, ?2, 4, 50, 'refunded', 1, 2, NULL, NULL, NULL)",
                params![s.id.raw() as i64, tid.raw() as i64],
            )
            .unwrap();
            // Rewind the cursor: the v16 (index 16, target 17) and v17
            // (index 17, target 18) blocks replay on reopen.
            // v24 attachments are post-this-version: drop them (tolerantly,
            // some legacy shapes lack the table/column) so the full chain
            // replays cleanly on reopen.
            let _ = conn.execute("ALTER TABLE task DROP COLUMN attachments", []);
            let _ = conn.execute("ALTER TABLE task_ledger DROP COLUMN attachments", []);
            let _ = conn.execute("DROP TABLE IF EXISTS attachment", []);
            // The v30 shell-attribution column is post-this-version too:
            // drop it (tolerantly) so the full chain replays cleanly.
            let _ = conn.execute("ALTER TABLE tool_run DROP COLUMN pre_manifest", []);
            let _ = conn.execute("ALTER TABLE task DROP COLUMN contract", []);
            conn.execute("PRAGMA user_version = 16", []).unwrap();
        }
        (s.id, tid)
    };
    let store = Store::open(dir.path(), true).unwrap();
    let rows = store.cost_reservations_of(sid, tid, 10).unwrap();
    assert_eq!(rows.len(), 4, "every legacy row survived the rebuilds");
    let by_op = |op: i64| {
        rows.iter()
            .find(|r| r.op_id == OpId::new(op as u64))
            .unwrap()
            .clone()
    };
    let abandoned = by_op(1);
    assert_eq!(
        abandoned.status, "uncertain",
        "legacy 'abandoned' rows migrate to 'uncertain'"
    );
    assert_eq!(
        abandoned.predicted_micro, 800,
        "the migrated prediction is preserved"
    );
    assert_eq!(
        abandoned.dispatched_ms, None,
        "pre-v17 rows read as never-dispatched (no marker column existed)"
    );
    assert_eq!(
        abandoned.pricing_snapshot, None,
        "pre-v17 rows read as unpriced (no snapshot column existed)"
    );
    assert_eq!(
        by_op(2).status,
        "reserved",
        "the v17 migration renames legacy 'open' rows to 'reserved'"
    );
    assert_eq!(
        by_op(2).attempt_op_id,
        None,
        "legacy rows carry no attempt identity (op_id was their only op)"
    );
    assert_eq!(by_op(2).parent_op_id, Some(OpId::new(2)));
    assert_eq!(by_op(2).estimated_cost_micro, Some(100));
    assert_eq!(by_op(2).provider_reported_cost_micro, None);
    let settled = by_op(3);
    assert_eq!(settled.status, "settled");
    assert_eq!(settled.provider_cost_micro, Some(250));
    assert_eq!(settled.provider_reported_micro, Some(250));
    assert_eq!(
        settled.provider_reported_cost_micro,
        Some(250),
        "the v18 canonical provider-reported column backfills losslessly"
    );
    assert_eq!(
        settled.settled_cost_micro, None,
        "pre-v18 settlements never recorded which amount was folded: an honest NULL"
    );
    assert_eq!(
        settled.cost_basis, None,
        "pre-v18 settlements never recorded a basis: an honest NULL"
    );
    assert_eq!(settled.estimated_cost_micro, Some(300));
    assert_eq!(by_op(4).status, "refunded");
    // The migrated UNCERTAIN row's prediction KEEPS consuming free:
    // 1000 - 800 (uncertain) - 100 (reserved) = 100 free — a 101
    // reserve refuses with the typed exceeded outcome.
    let out = store
        .cost_reserve_priced(
            sid,
            tid,
            OpId::new(5),
            101,
            now_ms(),
            None,
            faktor_core::model::ExecutionClass::Unknown,
        )
        .unwrap();
    assert!(
        matches!(out, CostReserveOutcome::Exceeded { free: 100 }),
        "the migrated uncertain hold consumes the reserved amount: {out:?}"
    );
    // The new CHECK forbids the legacy vocabulary outright (both
    // 'abandoned' and 'open' are gone from the v18 vocabulary).
    for hostile in ["abandoned", "open"] {
        let insert = store.raw_conn().execute(
            "INSERT INTO cost_reservation
                    (session_id, task_id, op_id, predicted_micro, status, created_ms)
                 VALUES (1, 1, 9, 1, ?1, 1)",
            [hostile],
        );
        assert!(
            matches!(&insert, Err(rusqlite::Error::SqliteFailure(..))),
            "the rebuilt CHECK rejects legacy state {hostile:?}: {insert:?}"
        );
    }
    // Reopen again: the migrations are a no-op and the data is stable.
    let store = Store::open(dir.path(), true).unwrap();
    let rows = store.cost_reservations_of(sid, tid, 10).unwrap();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows.iter().filter(|r| r.status == "uncertain").count(), 1);
    assert_eq!(rows.iter().filter(|r| r.status == "reserved").count(), 1);
    assert_eq!(rows.iter().filter(|r| r.status == "settled").count(), 1);
    assert_eq!(rows.iter().filter(|r| r.status == "refunded").count(), 1);
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
fn verification_record_evidence_columns_roundtrip_and_legacy_put_stays_null() {
    // Schema v20 (audits 94/116/117): the additive evidence columns carry
    // whatever opaque bounded JSON the caller validated; the legacy put
    // path writes SQL NULL and reads back as an honest absence. Both
    // survive a reopen.
    let dir = tempfile::tempdir().unwrap();
    let (legacy, with_evidence) = {
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let task = seed_verifying(&store, s.id, TaskId::new(1), vec!["c1".into()]);
        let rec = passing_record(&task, ws, WorktreeId::new(1));
        let legacy = store.verification_record_put(&rec).unwrap();
        let fingerprint = r#"{"platform":"macos","arch":"aarch64"}"#;
        let candidate = r#"{"task_revision":1,"accounting_snapshot_digest":"accounting:v1:0"}"#;
        let with_evidence = store
            .verification_record_put_with_evidence(&rec, Some(fingerprint), Some(candidate))
            .unwrap();
        (legacy, with_evidence)
    };
    let store = Store::open(dir.path(), true).unwrap();
    let (row, fingerprint, candidate) = store
        .verification_record_get_with_evidence(legacy)
        .unwrap()
        .unwrap();
    assert_eq!(row.task_id.raw(), 1);
    assert!(fingerprint.is_none(), "legacy put writes NULL evidence");
    assert!(candidate.is_none(), "legacy put writes NULL evidence");
    let (_, fingerprint, candidate) = store
        .verification_record_get_with_evidence(with_evidence)
        .unwrap()
        .unwrap();
    assert_eq!(
        fingerprint.as_deref(),
        Some(r#"{"platform":"macos","arch":"aarch64"}"#)
    );
    assert_eq!(
        candidate.as_deref(),
        Some(r#"{"task_revision":1,"accounting_snapshot_digest":"accounting:v1:0"}"#)
    );
    let list = store
        .verification_record_list_by_task_with_evidence(TaskId::new(1))
        .unwrap();
    assert_eq!(list.len(), 2);
    assert!(list[0].1.is_none());
    assert!(list[1].1.is_some());
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

/// Run one raw write with foreign keys OFF, then restore them: corrupting
/// a referenced id column behind the typed API's back is exactly the
/// hand-corrupted database the read-time decodes must survive.
pub(crate) fn corrupt_ignoring_fks(store: &Store, sql: &str, params: &[&dyn rusqlite::ToSql]) {
    let conn = store.raw_conn();
    conn.execute_batch("PRAGMA foreign_keys = OFF").unwrap();
    conn.execute(sql, params).unwrap();
    conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
}

/// PIN (P2): a negative `i64` observed in a real id COLUMN is the
/// intentional SQLite two's-complement legacy of a high-half `u64` id —
/// not corruption — while zero stays typed `Corrupt`. Reads the column
/// back through a real row mapper (`all_active_turns`), not just the
/// decode helper.
#[test]
fn negative_legacy_id_column_decodes_to_the_high_half_not_corruption() {
    let (_d, store) = tmp_store();
    let ws = store.create_workspace("/w").unwrap();
    let s = store.create_session(ws, "t", "p", "m").unwrap();
    let turn_id = store
        .start_turn_record(s.id, OpId::new(5), None, None, "p", "m", None)
        .unwrap();
    for wanted in [u64::MAX, 2u64.pow(63), u64::MAX - 1] {
        let raw = wanted as i64;
        assert!(raw < 0, "the fixture must exercise a negative legacy raw");
        corrupt_ignoring_fks(
            &store,
            "UPDATE turn_record SET session_id = ?2 WHERE id = ?1",
            &[&turn_id, &raw],
        );
        assert_eq!(
            store.all_active_turns().unwrap()[0].session_id.raw(),
            wanted,
            "{raw} is the bit-cast of the high-half id {wanted}: intentional, not corruption"
        );
    }
    corrupt_ignoring_fks(
        &store,
        "UPDATE turn_record SET session_id = 0 WHERE id = ?1",
        &[&turn_id],
    );
    assert_corrupt_id(store.all_active_turns(), "all_active_turns", "session_id");
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
fn migration_v17_turns_v16_open_rows_into_reserved_and_keeps_the_marker_truth() {
    // (ii + legacy crash window) A v16 store wrote dispatch markers
    // WITHOUT changing the row's `open` status. After the v17 migration
    // those rows read `reserved` + marker — still refund-impossible (the
    // guarded SQL requires a NULL marker) and still recovered as
    // UNCERTAIN, never as a $0 refund.
    let dir = tempfile::tempdir().unwrap();
    let (sid, tid) = {
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s = store.create_session(ws, "t", "p", "m").unwrap();
        let task = seed_task(&store, s.id, TaskId::new(1), vec![], TaskState::Running);
        let tid = task.task_id;
        store.cost_task_cap_set(s.id, tid, Some(1_000)).unwrap();
        {
            let conn = store.raw_conn();
            conn.execute("DROP INDEX IF EXISTS idx_provider_call_session_attempt", [])
                .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_op_id", [])
                .unwrap();
            conn.execute(
                "ALTER TABLE provider_call DROP COLUMN parent_model_call_op_id",
                [],
            )
            .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN attempt_ordinal", [])
                .unwrap();
            conn.execute("ALTER TABLE provider_call DROP COLUMN reservation_id", [])
                .unwrap();
            // The v19 segment-observation column is post-this-version
            // too: drop it so the full chain (past v19) replays cleanly.
            conn.execute(
                "ALTER TABLE provider_call DROP COLUMN prefix_segments_json",
                [],
            )
            .unwrap();
            // The v20 verification-record evidence columns are
            // post-this-version too: drop them so the full chain
            // (past v20) replays cleanly.
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
            // Rebuild the table in its EXACT v16 shape (the schema the
            // v16 writer produced: dispatched_ms + pricing_snapshot_json
            // present, vocabulary open/settled/refunded/uncertain) and
            // seed the two crash shapes the v16 writer could leave: an
            // `open` row dispatch never began and an `open` row whose
            // marker was written (dispatch may have reached the
            // provider).
            conn.execute("DROP TABLE cost_reservation", []).unwrap();
            conn.execute(
                    "CREATE TABLE cost_reservation (
                        reservation_id INTEGER PRIMARY KEY AUTOINCREMENT,
                        session_id INTEGER NOT NULL,
                        task_id INTEGER NOT NULL,
                        op_id INTEGER NOT NULL,
                        predicted_micro INTEGER NOT NULL,
                        status TEXT NOT NULL CHECK (status IN ('open', 'settled', 'refunded', 'uncertain')),
                        created_ms INTEGER NOT NULL,
                        settled_ms INTEGER,
                        dispatched_ms INTEGER,
                        pricing_snapshot_json TEXT,
                        provider_cost_micro INTEGER,
                        provider_reported_micro INTEGER,
                        route_decision_json TEXT
                     )",
                    [],
                )
                .unwrap();
            conn.execute(
                "INSERT INTO cost_reservation
                        (reservation_id, session_id, task_id, op_id, predicted_micro, status,
                         created_ms, settled_ms, dispatched_ms, pricing_snapshot_json,
                         provider_cost_micro, provider_reported_micro, route_decision_json)
                     VALUES
                        (1, ?1, ?2, 1, 200, 'open', 1, NULL, NULL, NULL, NULL, NULL, NULL),
                        (2, ?1, ?2, 2, 300, 'open', 1, NULL, 555, NULL, NULL, NULL, NULL),
                        (3, ?1, ?2, 3, 400, 'settled', 1, 9, 9, NULL, 250, 250, NULL)",
                params![s.id.raw() as i64, tid.raw() as i64],
            )
            .unwrap();
            // Rewind the cursor to the v16 schema target: ONLY the v17
            // block (index 17, target 18) replays on reopen.
            // v24 attachments are post-this-version: drop them (tolerantly,
            // some legacy shapes lack the table/column) so the full chain
            // replays cleanly on reopen.
            let _ = conn.execute("ALTER TABLE task DROP COLUMN attachments", []);
            let _ = conn.execute("ALTER TABLE task_ledger DROP COLUMN attachments", []);
            let _ = conn.execute("DROP TABLE IF EXISTS attachment", []);
            // The v30 shell-attribution column is post-this-version too:
            // drop it (tolerantly) so the full chain replays cleanly.
            let _ = conn.execute("ALTER TABLE tool_run DROP COLUMN pre_manifest", []);
            let _ = conn.execute("ALTER TABLE task DROP COLUMN contract", []);
            conn.execute("PRAGMA user_version = 17", []).unwrap();
        }
        (s.id, tid)
    };
    let store = Store::open(dir.path(), true).unwrap();
    let rows = store.cost_reservations_of(sid, tid, 10).unwrap();
    let by_op = |op: i64| {
        rows.iter()
            .find(|r| r.op_id == OpId::new(op as u64))
            .unwrap()
            .clone()
    };
    let pre_marker = by_op(1);
    assert_eq!(pre_marker.status, "reserved");
    assert_eq!(pre_marker.dispatched_ms, None);
    let legacy_dispatched = by_op(2);
    assert_eq!(
        legacy_dispatched.status, "reserved",
        "v16 open + marker migrates to reserved + marker (nothing lossy)"
    );
    assert_eq!(legacy_dispatched.dispatched_ms, Some(555));
    assert_eq!(legacy_dispatched.parent_op_id, Some(OpId::new(2)));
    assert_eq!(legacy_dispatched.attempt_op_id, None);
    assert_eq!(by_op(3).status, "settled");
    // The refund guard reads the MARKER, not the status name: the
    // migrated dispatched row is unrefundable even though it reads
    // `reserved`.
    let out = store
        .cost_refund(by_op(2).reservation_id, now_ms())
        .unwrap();
    assert_eq!(
        out,
        RefundOutcome::Blocked {
            current: "reserved".into(),
            dispatched_ms: Some(555)
        },
        "a migrated v16 dispatch (reserved + marker) can never refund"
    );
    // Recovery reads it as may-have-dispatched -> UNCERTAIN.
    let (refunded, uncertain) = store.cost_recover_open_reservations(now_ms()).unwrap();
    assert_eq!(refunded, 1, "only the truly pre-dispatch row refunds");
    assert_eq!(uncertain, 1, "the migrated dispatched row goes UNCERTAIN");
    let rows = store.cost_reservations_of(sid, tid, 10).unwrap();
    assert_eq!(
        rows.iter()
            .find(|r| r.reservation_id == legacy_dispatched.reservation_id)
            .unwrap()
            .status,
        "uncertain"
    );
    assert_eq!(
        free_micro(&store, sid, tid),
        700,
        "cap 1000 - 0 folded spend - 300 uncertain hold = 700 (the 200 refund is free)"
    );
    // Reopen again: the migration is a no-op and every row is stable.
    drop(store);
    let store = Store::open(dir.path(), true).unwrap();
    let rows = store.cost_reservations_of(sid, tid, 10).unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows.iter().filter(|r| r.status == "reserved").count(), 0);
    assert_eq!(rows.iter().filter(|r| r.status == "settled").count(), 1);
    assert_eq!(rows.iter().filter(|r| r.status == "refunded").count(), 1);
    assert_eq!(rows.iter().filter(|r| r.status == "uncertain").count(), 1);
}
