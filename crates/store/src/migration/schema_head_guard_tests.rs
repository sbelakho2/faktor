use super::*;

fn restore_points(root: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(root.join(PRE_MIGRATION_BACKUP_DIR))
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("db"))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

fn schema_objects(conn: &Connection) -> Vec<String> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master ORDER BY name")
        .unwrap();
    let mut objects: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|row| row.unwrap())
        .collect();
    objects.sort();
    objects
}

fn assert_verified_restore_point(path: &Path, claimed_version: i64) {
    // Delivered state: one self-contained `.db` file, no WAL sidecars.
    for suffix in ["-wal", "-shm"] {
        assert!(
            !PathBuf::from(format!("{}{suffix}", path.display())).exists(),
            "restore point {} must not depend on WAL sidecars",
            path.display()
        );
    }
    let conn =
        Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        version, claimed_version,
        "a restore point captures the PREDECESSOR cursor, never a post-migration one"
    );
    let verdict: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        verdict,
        "ok",
        "restore point {} must verify",
        path.display()
    );
    drop(conn);
}

/// An OLDER binary must never silently open — let alone migrate — a
/// database a NEWER binary already advanced: the typed refusal names both
/// versions and NOTHING is snapshotted or written.
#[test]
fn a_store_migrated_by_a_newer_binary_refuses_typed_without_writes() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("faktor-plus.db");
    let future_head = SCHEMA_HEAD + 1;
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(&format!(
            "CREATE TABLE future_marker (id INTEGER PRIMARY KEY);
                 PRAGMA user_version = {future_head};"
        ))
        .unwrap();
    }
    let objects_before = schema_objects(&Connection::open(&db).unwrap());
    let err = Store::open(dir.path(), true).expect_err("a newer schema must refuse");
    match &err {
        StoreError::UnsupportedSchema {
            found,
            maximum_supported,
        } => {
            assert_eq!(*found, future_head);
            assert_eq!(*maximum_supported, SCHEMA_HEAD);
        }
        other => panic!("expected typed UnsupportedSchema, got {other:?}"),
    }
    let message = err.to_string();
    for needle in [
        &format!("v{future_head}"),
        &format!("v{SCHEMA_HEAD}"),
        "newer binary",
        "pre-migration snapshot",
    ] {
        assert!(message.contains(needle), "{message}");
    }
    // No migration SQL ran, the newer schema is untouched, and the
    // refusal wrote NO restore point of a database this binary must not
    // touch.
    let conn = Connection::open(&db).unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, future_head, "the cursor is never moved backwards");
    assert_eq!(
        schema_objects(&conn),
        objects_before,
        "no table/index was created, renamed or dropped"
    );
    assert_eq!(
        schema_version_at(&db).unwrap(),
        Some(future_head),
        "the read-only observation sees the newer cursor without opening it"
    );
    assert!(!dir.path().join(PRE_MIGRATION_BACKUP_DIR).exists());
    // A later opener (and each concurrent one) sees the SAME typed refusal.
    match Store::open(dir.path(), false) {
        Err(StoreError::UnsupportedSchema {
            found,
            maximum_supported,
        }) => {
            assert_eq!(found, future_head);
            assert_eq!(maximum_supported, SCHEMA_HEAD);
        }
        other => panic!("a repeat open must refuse identically, got {other:?}"),
    }
}

/// A NEGATIVE cursor is durable corruption (no legitimate open of this
/// ladder ever produces one): refused typed before the restore point and
/// the ladder, never treated as v0.
#[test]
fn a_negative_schema_cursor_is_refused_as_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("faktor-plus.db");
    drop(rusqlite::Connection::open(&db).unwrap());
    let conn = Connection::open(&db).unwrap();
    conn.execute_batch("PRAGMA user_version = -1").unwrap();
    drop(conn);
    match Store::open(dir.path(), true) {
        Err(StoreError::Corrupt(lines)) => {
            assert!(
                lines.iter().any(|line| line.contains("-1")),
                "the refusal must name the cursor: {lines:?}"
            );
        }
        other => panic!("a negative cursor must refuse typed, got {other:?}"),
    }
    assert!(!dir.path().join(PRE_MIGRATION_BACKUP_DIR).exists());
    let conn = Connection::open(&db).unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, -1, "corrupt state is left exactly as found");
}

/// A pending migration writes ONE verified restore point of the actual
/// predecessor state before any migration SQL; a no-op open writes none.
#[test]
fn a_pending_migration_writes_a_verified_pre_migration_restore_point() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("faktor-plus.db");
    // First open: v0 -> head, the empty predecessor is snapshotted.
    drop(Store::open(dir.path(), true).unwrap());
    let first = restore_points(dir.path());
    assert_eq!(first.len(), 1, "the first open snapshots v0: {first:?}");
    let first_name = first[0].file_name().unwrap().to_string_lossy().into_owned();
    assert!(first_name.contains(".pre-migration-v0-"), "{first_name}");
    assert_verified_restore_point(&first[0], 0);
    // A no-op reopen at the head writes NO restore point.
    drop(Store::open(dir.path(), true).unwrap());
    assert_eq!(
        restore_points(dir.path()).len(),
        1,
        "an open that migrates nothing must not snapshot"
    );
    // Rewind to the v38 predecessor: the reopen must snapshot the EXACT
    // predecessor (no session-deletion tombstone) before applying v38 and
    // every later step. `SCHEMA_HEAD - 1` moved past v38 when v39 landed.
    const TOMBSTONE_PREDECESSOR: i64 = 37;
    {
        let conn = Connection::open(&db).unwrap();
        crate::drop_verification_scope_for_rewind(&conn);
        conn.execute_batch(&format!(
            "DROP TABLE IF EXISTS session_deletion;
                 PRAGMA user_version = {TOMBSTONE_PREDECESSOR};"
        ))
        .unwrap();
    }
    let store = Store::open(dir.path(), true).unwrap();
    let live: i64 = store
        .raw_conn()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(live, SCHEMA_HEAD, "the reopen completes the chain");
    drop(store);
    let newest = restore_points(dir.path())
        .into_iter()
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .contains(&format!(".pre-migration-v{TOMBSTONE_PREDECESSOR}-"))
        })
        .expect("the pending migration must write its predecessor restore point");
    assert_verified_restore_point(&newest, TOMBSTONE_PREDECESSOR);
    let snapshot =
        Connection::open_with_flags(&newest, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let pre_table: i64 = snapshot
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'session_deletion'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        pre_table, 0,
        "the restore point is PRE-migration: the pending step's table must not exist in it"
    );
    // The predecessor already carries the v33 settlement objects.
    let pre_settlement: i64 = snapshot
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'economic_settlement'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pre_settlement, 1);
    let live = Connection::open(&db).unwrap();
    let post_table: i64 = live
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'session_deletion'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(post_table, 1, "the live database finished the chain");
    for table in [
        "engineering_outcome",
        "economic_settlement",
        "terminal_run_facts",
    ] {
        let count: i64 = live
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = ?1",
                rusqlite::params![table],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "the live database finished the chain: {table}");
    }
}

/// Restore points are BOUNDED: an arbitrarily long upgrade history must
/// not grow the backups directory without bound, and the newest points —
/// the ones recent transitions need — always survive.
#[test]
fn restore_points_are_bounded_and_the_newest_survive() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("faktor-plus.db");
    // First open: snapshot #1 (v0).
    drop(Store::open(dir.path(), true).unwrap());
    // Five rewind-reopens: snapshots #2..#6 of the predecessor cursor.
    for round in 0..5 {
        let conn = Connection::open(&db).unwrap();
        let _ = conn.execute(
            "ALTER TABLE cost_reservation DROP COLUMN execution_class",
            [],
        );
        crate::drop_verification_scope_for_rewind(&conn);
        conn.execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_HEAD - 1))
            .unwrap();
        drop(conn);
        drop(Store::open(dir.path(), true).unwrap());
        assert!(
            restore_points(dir.path()).len() <= MAX_PRE_MIGRATION_RESTORE_POINTS,
            "round {round} must stay bounded"
        );
    }
    let points = restore_points(dir.path());
    assert_eq!(
        points.len(),
        MAX_PRE_MIGRATION_RESTORE_POINTS,
        "six transitions leave exactly the bound: {points:?}"
    );
    assert!(
        !points.iter().any(|path| path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains(".pre-migration-v0-")),
        "the oldest restore points are the pruning victims: {points:?}"
    );
    for point in &points {
        assert_verified_restore_point(point, SCHEMA_HEAD - 1);
    }
}

/// A snapshot that cannot be written REFUSES the migration: no ladder
/// step may run without a verified way back to the predecessor.
#[test]
fn a_restore_point_that_cannot_be_written_refuses_the_migration() {
    let dir = tempfile::tempdir().unwrap();
    // A FILE named `backups` makes every restore-point write impossible.
    std::fs::write(
        dir.path().join(PRE_MIGRATION_BACKUP_DIR),
        b"not a directory",
    )
    .unwrap();
    let err = Store::open(dir.path(), true).expect_err("no restore point, no migration");
    assert!(
        matches!(err, StoreError::Io(_)),
        "an unwritable restore point refuses typed: {err:?}"
    );
    let conn = Connection::open(dir.path().join("faktor-plus.db")).unwrap();
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        version, 0,
        "the cursor is untouched when the snapshot fails"
    );
    let migrated: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'session'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(migrated, 0, "no migration SQL ran without a restore point");
}
