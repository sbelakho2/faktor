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

/// Create one dummy restore-point FILE with a frozen mtime (pruning never
/// opens the file, so ordering tests need no real database content).
fn frozen_dummy_point(dir: &Path, name: &str, frozen: std::time::SystemTime) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, b"restore point").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(frozen)
        .unwrap();
    path
}

/// Every file name in `dir`, sorted (test assertions on whole sets).
fn backup_file_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// ADVERSARIAL (equal mtimes, sequence >= 10): six restore points with
/// IDENTICAL mtimes and identical version/stamp/pid prefixes differ only
/// in the numeric sequence 6..=11. The old lexicographic tie-break sorted
/// `...-10.db` and `...-11.db` BEFORE `...-6.db`..`...-9.db`, so pruning
/// could victimize the just-written newest point. The numeric parse must
/// prune 6 then 7 (oldest first) and keep 8..=11, never touching the
/// protected just-written point.
#[test]
fn equal_mtime_pruning_orders_sequences_numerically_and_never_prunes_the_newest() {
    let dir = tempfile::tempdir().unwrap();
    let backups = dir.path().join(PRE_MIGRATION_BACKUP_DIR);
    std::fs::create_dir_all(&backups).unwrap();
    let frozen = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    let mut paths: Vec<PathBuf> = Vec::new();
    for seq in 6..=11u64 {
        paths.push(frozen_dummy_point(
            &backups,
            &format!("faktor-plus.db.pre-migration-v31-1700000000000-4242-{seq}.db"),
            frozen,
        ));
    }
    let keep = paths.last().unwrap().clone();
    prune_old_pre_migration_backups(&backups, "faktor-plus.db", &keep);
    let names = backup_file_names(&backups);
    assert_eq!(
        names.len(),
        MAX_PRE_MIGRATION_RESTORE_POINTS,
        "retention bound respected under mtime ties: {names:?}"
    );
    assert!(keep.exists(), "the just-written newest point must survive");
    for seq in [6u64, 7] {
        let name = format!("faktor-plus.db.pre-migration-v31-1700000000000-4242-{seq}.db");
        assert!(!backups.join(&name).exists(), "oldest pruned first: {name}");
    }
    for seq in [8u64, 9, 10, 11] {
        let name = format!("faktor-plus.db.pre-migration-v31-1700000000000-4242-{seq}.db");
        assert!(
            backups.join(&name).exists(),
            "numeric order keeps the newer points: {name}"
        );
    }
}

/// Legacy (pid/seq-less) names stay recognized and are ordered by their
/// own numeric components: an equal-mtime sweep must prune the 500-stamp
/// legacy point and the lowest current sequence, never the 2000-stamp
/// legacy point (a lexicographic order would have pruned the `-10`/`-11`
/// tails instead).
#[test]
fn legacy_restore_point_names_are_recognized_and_ordered_numerically() {
    let dir = tempfile::tempdir().unwrap();
    let backups = dir.path().join(PRE_MIGRATION_BACKUP_DIR);
    std::fs::create_dir_all(&backups).unwrap();
    let frozen = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    let cur = |seq: u64| format!("faktor-plus.db.pre-migration-v0-1000-4242-{seq}.db");
    let legacy_old = "faktor-plus.db.pre-migration-v0-500.db";
    let legacy_new = "faktor-plus.db.pre-migration-v0-2000.db";
    for seq in [8u64, 9, 10] {
        frozen_dummy_point(&backups, &cur(seq), frozen);
    }
    frozen_dummy_point(&backups, legacy_old, frozen);
    let keep = frozen_dummy_point(&backups, &cur(11), frozen);
    frozen_dummy_point(&backups, legacy_new, frozen);
    prune_old_pre_migration_backups(&backups, "faktor-plus.db", &keep);
    let names = backup_file_names(&backups);
    assert_eq!(names.len(), MAX_PRE_MIGRATION_RESTORE_POINTS, "{names:?}");
    assert!(
        !backups.join(legacy_old).exists(),
        "the oldest legacy stamp is pruned first"
    );
    assert!(
        !backups.join(cur(8)).exists(),
        "the lowest numeric sequence is the next victim, not 10/11"
    );
    assert!(
        backups.join(legacy_new).exists(),
        "a legacy name with the NEWEST stamp outranks every older current-form point"
    );
    assert!(keep.exists(), "the just-written point survives");
}

/// Names carrying the marker that match no known form are still
/// candidates, but sort after every recognized point: pruning never
/// deletes a point it cannot order ahead of one it can. A sibling
/// database whose name merely PREFIXES this one must not be counted or
/// pruned at all.
#[test]
fn unrecognized_and_foreign_names_are_pruned_last_and_never_touched_early() {
    let dir = tempfile::tempdir().unwrap();
    let backups = dir.path().join(PRE_MIGRATION_BACKUP_DIR);
    std::fs::create_dir_all(&backups).unwrap();
    let frozen = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    let cur = |seq: u64| format!("faktor-plus.db.pre-migration-v0-1000-4242-{seq}.db");
    for seq in [8u64, 9, 10, 11] {
        frozen_dummy_point(&backups, &cur(seq), frozen);
    }
    let odd = frozen_dummy_point(
        &backups,
        "faktor-plus.db.pre-migration-v0-not-a-stamp.db",
        frozen,
    );
    let foreign = frozen_dummy_point(
        &backups,
        "faktor-plus.db2.pre-migration-v0-1000-4242-6.db",
        frozen,
    );
    let keep = backups.join(cur(11));
    prune_old_pre_migration_backups(&backups, "faktor-plus.db", &keep);
    assert!(
        !backups.join(cur(8)).exists(),
        "recognized oldest is the victim"
    );
    assert!(
        odd.exists(),
        "an unparseable name is never pruned while a recognized older point exists"
    );
    assert!(
        foreign.exists(),
        "a sibling database's restore point is not this database's candidate"
    );
    for seq in [9u64, 10, 11] {
        assert!(backups.join(cur(seq)).exists(), "{}", cur(seq));
    }
}

/// The just-written point is protected by identity, not only by ordering:
/// even a pathological tie that placed it FIRST can never make it the
/// prune victim.
#[test]
fn the_just_written_restore_point_is_never_a_prune_victim() {
    let dir = tempfile::tempdir().unwrap();
    let backups = dir.path().join(PRE_MIGRATION_BACKUP_DIR);
    std::fs::create_dir_all(&backups).unwrap();
    let frozen = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    let name = |seq: u64| format!("faktor-plus.db.pre-migration-v0-1000-4242-{seq}.db");
    let keep = frozen_dummy_point(&backups, &name(5), frozen);
    for seq in 6..=10u64 {
        frozen_dummy_point(&backups, &name(seq), frozen);
    }
    prune_old_pre_migration_backups(&backups, "faktor-plus.db", &keep);
    assert!(keep.exists(), "identity protection is unconditional");
    assert!(!backups.join(name(6)).exists());
    assert!(!backups.join(name(7)).exists());
    assert_eq!(
        backup_file_names(&backups).len(),
        MAX_PRE_MIGRATION_RESTORE_POINTS
    );
}

/// End-to-end through the real writer: after the predecessor points are
/// frozen to one mtime and renamed to sequences 9/10/11, one more
/// `verified_pre_migration_backup` call must prune exactly sequence 9.
#[test]
fn real_writer_prunes_the_numeric_oldest_under_equal_mtimes() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("faktor-plus.db");
    drop(Store::open(dir.path(), true).unwrap());
    for _ in 0..3 {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_HEAD - 1))
            .unwrap();
        drop(conn);
        drop(Store::open(dir.path(), true).unwrap());
    }
    let mut points = restore_points(dir.path());
    assert_eq!(points.len(), 4, "four verified restore points: {points:?}");
    points.sort();
    let frozen = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    let renamed_name = |seq: u64| {
        format!(
            "faktor-plus.db.pre-migration-v{}-1700000000000-4242-{seq}.db",
            SCHEMA_HEAD - 1
        )
    };
    for (i, path) in points.iter().enumerate() {
        let renamed = path.with_file_name(renamed_name(9 + i as u64));
        std::fs::rename(path, &renamed).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&renamed)
            .unwrap()
            .set_modified(frozen)
            .unwrap();
    }
    {
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_HEAD - 1))
            .unwrap();
    }
    let newest = verified_pre_migration_backup(&db, SCHEMA_HEAD - 1).unwrap();
    assert!(
        newest.exists(),
        "the just-written point survives its own sweep"
    );
    let names = backup_file_names(&dir.path().join(PRE_MIGRATION_BACKUP_DIR));
    assert_eq!(names.len(), MAX_PRE_MIGRATION_RESTORE_POINTS, "{names:?}");
    assert!(
        !dir.path()
            .join(PRE_MIGRATION_BACKUP_DIR)
            .join(renamed_name(9))
            .exists(),
        "the numeric-oldest sequence 9 is the victim: {names:?}"
    );
    for seq in [10u64, 11, 12] {
        assert!(
            dir.path()
                .join(PRE_MIGRATION_BACKUP_DIR)
                .join(renamed_name(seq))
                .exists(),
            "sequence {seq} must survive: {names:?}"
        );
    }
}

/// Concurrent sweeps (two openers each writing and pruning their own
/// restore point) never delete either just-written point: victims are
/// always a prefix of the equal-mtime numeric order, and a lost removal
/// race only stops that sweep.
#[test]
fn concurrent_prunes_never_delete_a_just_written_point() {
    use std::sync::{Arc, Barrier};
    let dir = tempfile::tempdir().unwrap();
    let backups = dir.path().join(PRE_MIGRATION_BACKUP_DIR);
    std::fs::create_dir_all(&backups).unwrap();
    let frozen = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
    for seq in 0..10u64 {
        frozen_dummy_point(
            &backups,
            &format!("faktor-plus.db.pre-migration-v0-1000-4242-{seq}.db"),
            frozen,
        );
    }
    let keep_a = frozen_dummy_point(
        &backups,
        "faktor-plus.db.pre-migration-v0-2000-4242-10.db",
        frozen,
    );
    let keep_b = frozen_dummy_point(
        &backups,
        "faktor-plus.db.pre-migration-v0-2000-4242-11.db",
        frozen,
    );
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = [keep_a.clone(), keep_b.clone()]
        .into_iter()
        .map(|keep| {
            let barrier = Arc::clone(&barrier);
            let backups = backups.clone();
            std::thread::spawn(move || {
                barrier.wait();
                prune_old_pre_migration_backups(&backups, "faktor-plus.db", &keep);
            })
        })
        .collect();
    for handle in handles {
        handle.join().expect("a concurrent sweep must not panic");
    }
    assert!(
        keep_a.exists() && keep_b.exists(),
        "both just-written points survive"
    );
    for seq in [8u64, 9] {
        assert!(
            backups
                .join(format!(
                    "faktor-plus.db.pre-migration-v0-1000-4242-{seq}.db"
                ))
                .exists(),
            "victims under ties are a prefix, never a newer sequence"
        );
    }
}
