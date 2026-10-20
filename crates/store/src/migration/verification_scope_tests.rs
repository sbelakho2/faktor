//! v43 migration discipline for session-scoped verification records:
//! proof-only backfill, verified pre-migration restore point, idempotent
//! replay and concurrent openers.

use super::*;

// v43's predecessor is v42. `SCHEMA_HEAD - 1` is no longer that step
// once a later migration (v44 listing revisions) moved the head.
const PREDECESSOR: i64 = 42;

fn raw_record(conn: &Connection, task_id: i64, workspace_id: i64) {
    conn.execute(
        "INSERT INTO verification_record(
            task_id, revision, workspace_id, worktree_id, tree_hash,
            criteria_json, checks_json, changed_files_json,
            unrelated_changes_json, reviewer_json, status, started_ms, completed_ms)
         VALUES (?1, 1, ?2, 1, NULL, '[]', '[]', '[]', '[]', NULL, '\"passed\"', 1, NULL)",
        params![task_id, workspace_id],
    )
    .unwrap();
}

fn rewind_to_predecessor(conn: &Connection) {
    crate::drop_verification_scope_for_rewind(conn);
    conn.execute_batch(&format!("PRAGMA user_version = {PREDECESSOR}"))
        .unwrap();
}

fn v43_restore_points(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(root.join(PRE_MIGRATION_BACKUP_DIR))
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| {
                    path.extension().and_then(|e| e.to_str()) == Some("db")
                        && path
                            .file_name()
                            .unwrap()
                            .to_string_lossy()
                            .contains(&format!(".pre-migration-v{PREDECESSOR}-"))
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

#[test]
fn upgrade_backfills_only_provable_rows_and_keeps_a_verified_predecessor_backup() {
    let dir = tempfile::tempdir().unwrap();
    let (s1, s2, s3) = {
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let ws2 = store.create_workspace("/w2").unwrap();
        let s1 = store.create_session(ws, "a", "p", "m").unwrap();
        let s2 = store.create_session(ws, "b", "p", "m").unwrap();
        let s3 = store.create_session(ws2, "c", "p", "m").unwrap();
        for (session, task_id) in [(s1.id, 1), (s2.id, 1), (s1.id, 5), (s3.id, 6)] {
            let task = TaskRow {
                task_id: TaskId::new(task_id),
                session_id: session,
                goal: "g".into(),
                acceptance_criteria: vec![],
                plan: vec![],
                attachments: vec![],
                contract: Default::default(),
                max_tokens: None,
                max_turns: None,
                spent_tokens: 0,
                spent_turns: 0,
                state: TaskState::Running,
                revision: TaskRevision::new(1),
                created_ms: 1,
                updated_ms: 1,
            };
            store.upsert_task(&task).unwrap();
        }
        {
            let conn = store.raw_conn();
            // P: task 5 exists ONLY under s1 and its identity matches -> the
            // sole provable owner.
            raw_record(&conn, 5, ws.raw() as i64);
            // A: task 1 exists under BOTH s1 and s2 in the same workspace ->
            // ambiguous, must stay NULL (never guessed).
            raw_record(&conn, 1, ws.raw() as i64);
            // W: task 6 exists under s3 in /w2 but the record claims /w ->
            // workspace disagreement, must stay NULL.
            raw_record(&conn, 6, ws.raw() as i64);
            rewind_to_predecessor(&conn);
        }
        (s1.id, s2.id, s3.id)
    };
    let (p, a, w) = (
        VerificationRecordId::new(1),
        VerificationRecordId::new(2),
        VerificationRecordId::new(3),
    );

    let store = Store::open(dir.path(), true).unwrap();
    // The pending migration wrote exactly ONE verified predecessor backup
    // BEFORE touching the file.
    let backups = v43_restore_points(dir.path());
    assert_eq!(
        backups.len(),
        1,
        "one pre-migration restore point: {backups:?}"
    );
    let backup =
        Connection::open_with_flags(&backups[0], rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    assert_eq!(
        backup
            .query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        PREDECESSOR,
        "the backup captures the PREDECESSOR cursor"
    );
    assert_eq!(
        backup
            .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    let pre_column: i64 = backup
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('verification_record') WHERE name = 'session_id'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pre_column, 0, "the backup predates the scope column");
    drop(backup);

    let live: i64 = store
        .raw_conn()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(live, SCHEMA_HEAD);
    let owner = |id: VerificationRecordId| -> Option<i64> {
        store
            .raw_conn()
            .query_row(
                "SELECT session_id FROM verification_record WHERE id = ?1",
                params![id.raw() as i64],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_eq!(owner(p), Some(s1.raw() as i64), "provable row backfilled");
    assert_eq!(owner(a), None, "ambiguous row never guessed");
    assert_eq!(owner(w), None, "workspace-disagreeing row never guessed");
    let _ = s3;

    // Scoped surfaces agree: s1 sees only P, s2 sees nothing (P is not its
    // row), and the ambiguous/unprovable rows refuse typed.
    assert_eq!(
        store
            .verification_record_list_by_task(s1, TaskId::new(5))
            .unwrap()
            .len(),
        1
    );
    assert!(store
        .verification_record_list_by_task(s2, TaskId::new(5))
        .unwrap()
        .is_empty());
    match store.verification_record_lookup(s1, p).unwrap() {
        VerificationRecordLookup::Found(boxed) => {
            let row = boxed.0;
            assert_eq!(row.session_id, Some(s1));
        }
        other => panic!("provable row must be scoped-readable: {other:?}"),
    }
}

#[test]
fn upgrade_backfill_is_idempotent_and_ambiguous_rows_stay_typed_unbound() {
    let dir = tempfile::tempdir().unwrap();
    let (s1, s2, a) = {
        let store = Store::open(dir.path(), true).unwrap();
        let ws = store.create_workspace("/w").unwrap();
        let s1 = store.create_session(ws, "a", "p", "m").unwrap();
        let s2 = store.create_session(ws, "b", "p", "m").unwrap();
        for session in [s1.id, s2.id] {
            store
                .upsert_task(&TaskRow {
                    task_id: TaskId::new(1),
                    session_id: session,
                    goal: "g".into(),
                    acceptance_criteria: vec![],
                    plan: vec![],
                    attachments: vec![],
                    contract: Default::default(),
                    max_tokens: None,
                    max_turns: None,
                    spent_tokens: 0,
                    spent_turns: 0,
                    state: TaskState::Running,
                    revision: TaskRevision::new(1),
                    created_ms: 1,
                    updated_ms: 1,
                })
                .unwrap();
        }
        {
            let conn = store.raw_conn();
            raw_record(&conn, 1, ws.raw() as i64);
            rewind_to_predecessor(&conn);
        }
        (s1.id, s2.id, VerificationRecordId::new(1))
    };
    let store = Store::open(dir.path(), true).unwrap();
    assert_eq!(
        store
            .raw_conn()
            .query_row(
                "SELECT session_id FROM verification_record WHERE id = ?1",
                params![a.raw() as i64],
                |r| r.get::<_, Option<i64>>(0),
            )
            .unwrap(),
        None,
        "two same-named tasks are not provable"
    );
    // No-op reopen: no second backfill, no rewritten row.
    drop(store);
    let store = Store::open(dir.path(), true).unwrap();
    assert_eq!(
        v43_restore_points(dir.path()).len(),
        1,
        "no-op open snapshots nothing"
    );
    assert_eq!(
        store.verification_record_lookup(s1, a).unwrap(),
        VerificationRecordLookup::UnboundLegacy { record_id: a }
    );
    assert_eq!(
        store.verification_record_lookup(s2, a).unwrap(),
        VerificationRecordLookup::UnboundLegacy { record_id: a }
    );
}

#[test]
fn concurrent_openers_on_a_predecessor_database_migrate_once_and_backfill_once() {
    use std::sync::{Arc, Barrier};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("m.db");
    {
        let mut conn = Connection::open(&path).unwrap();
        migrate(&mut conn, Some(path.as_path())).unwrap();
        conn.execute_batch(
            "INSERT INTO workspace(id, root, created_ms) VALUES (1, '/w', 1);
             INSERT INTO session(id, workspace_id, title, provider, model, state, lifecycle,
                                 created_ms, updated_ms, worktree_id, task_id)
             VALUES (1, 1, 't', 'p', 'm', '\"idle\"', 'open', 1, 1, 1, 1);
             INSERT INTO task(task_id, session_id, goal, acceptance_criteria, plan,
                              spent_tokens, spent_turns, state, revision, created_ms, updated_ms)
             VALUES (1, 1, 'g', '[]', '[]', 0, 0, '\"running\"', 1, 1, 1);",
        )
        .unwrap();
        raw_record(&conn, 1, 1);
        rewind_to_predecessor(&conn);
    }
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let mut conn = Connection::open(&path).unwrap();
                barrier.wait();
                migrate(&mut conn, Some(path.as_path()))
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert!(
        results.iter().any(|r| r.is_ok()),
        "at least one opener migrates: {results:?}"
    );
    let mut conn = Connection::open(&path).unwrap();
    migrate(&mut conn, Some(path.as_path())).unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_HEAD);
    let owner: Option<i64> = conn
        .query_row(
            "SELECT session_id FROM verification_record WHERE id = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(owner, Some(1), "the winner backfilled the row exactly once");
    assert_eq!(
        v43_restore_points(dir.path()).len(),
        1,
        "only the winner snapshotted the predecessor"
    );
}
