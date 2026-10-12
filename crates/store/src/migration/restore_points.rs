//! Pre-migration restore points: naming, the read-only schema-cursor
//! observation and the independently verified online-backup snapshot.

use super::*;

/// Directory (beside the database) holding VERIFIED pre-migration restore
/// points. Named `backups` deliberately: the CLI's online-backup vocabulary
/// and the `doctor` database-discovery skip list both treat a `backups`
/// directory as snapshots, never as a live authority.
pub const PRE_MIGRATION_BACKUP_DIR: &str = "backups";

/// One process-local monotonic disambiguator for restore-point names: a
/// millisecond stamp alone can repeat, and two restore points must never
/// overwrite each other (the first is the predecessor of the second).
fn backup_sequence() -> u64 {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// READ-ONLY observation of one database file's schema cursor
/// (`PRAGMA user_version`) WITHOUT opening or migrating it: a store whose
/// version is ahead of this binary must stay observable even though opening
/// it refuses ([`StoreError::UnsupportedSchema`]). `Ok(None)` = the file does
/// not exist; a file that exists but cannot be read is a typed error, never a
/// guessed version. The updater's rollback guard and tests use it.
pub fn schema_version_at(path: &Path) -> StoreResult<Option<i64>> {
    if !path.is_file() {
        return Ok(None);
    }
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    Ok(Some(version))
}

/// Write and independently VERIFY a pre-migration restore point for the
/// database at `db_path`, which is leaving `from_version`. The caller MUST
/// refuse the migration when this errors: a schema transition without a
/// verified restore point is the exact failure this policy prevents.
///
/// The copy runs through the SQLite online-backup API on a SEPARATE read-only
/// connection (the caller holds a write transaction; the backup API cannot
/// run on one, and the held `BEGIN IMMEDIATE` lock pins every reader to
/// exactly this pre-migration state). Self-consistency is enforced twice, so
/// a mislabeled snapshot is impossible: the live file must still be at
/// `from_version` when the copy starts, and the published file must pass
/// `PRAGMA integrity_check` AND its own `user_version` must equal the version
/// its name claims. Any failure removes the partial copy and refuses the
/// migration. The result is one self-contained `.db` file (WAL sidecars of
/// the copy are removed).
pub(crate) fn verified_pre_migration_backup(
    db_path: &Path,
    from_version: i64,
) -> StoreResult<PathBuf> {
    let parent = db_path.parent().unwrap_or_else(|| Path::new("."));
    let backup_dir = parent.join(PRE_MIGRATION_BACKUP_DIR);
    std::fs::create_dir_all(&backup_dir)?;
    let name = db_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "faktor-plus.db".to_string());
    let stamp = now_ms();
    let seq = backup_sequence();
    let dest = backup_dir.join(format!(
        "{name}.pre-migration-v{from_version}-{stamp}-{pid}-{seq}.db",
        pid = std::process::id()
    ));
    let result = write_and_verify_snapshot(db_path, &dest, from_version);
    if result.is_err() {
        let _ = std::fs::remove_file(&dest);
    }
    result.map(|()| {
        prune_old_pre_migration_backups(&backup_dir, &name, &dest);
        dest
    })
}
fn write_and_verify_snapshot(db_path: &Path, dest: &Path, from_version: i64) -> StoreResult<()> {
    let src = Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    // The name claims `pre-migration-v{from_version}`: the live file must
    // still BE at that version when the copy starts.
    let live: i64 = src.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if live != from_version {
        return Err(StoreError::Migration(format!(
            "refusing to label a restore point pre-migration-v{from_version}: the live database \
             is at schema v{live} (a concurrent opener already migrated it)"
        )));
    }
    {
        let mut dst = Connection::open(dest)?;
        let backup = rusqlite::backup::Backup::new(&src, &mut dst)?;
        backup
            .run_to_completion(64, Duration::from_millis(2), None)
            .map_err(|e| {
                StoreError::Migration(format!(
                    "pre-migration restore point {} failed: {e}",
                    dest.display()
                ))
            })?;
    }
    drop(src);
    let check = Connection::open_with_flags(dest, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let verdict: String = check.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if verdict != "ok" {
        return Err(StoreError::Migration(format!(
            "pre-migration restore point {} failed integrity_check: {verdict}",
            dest.display()
        )));
    }
    let copied: i64 = check.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if copied != from_version {
        return Err(StoreError::Migration(format!(
            "pre-migration restore point {} holds schema v{copied}, not the claimed \
             pre-migration-v{from_version}",
            dest.display()
        )));
    }
    drop(check);
    // The copy inherits WAL mode; opening it above may have created sidecar
    // files. A restore point is exactly one self-contained `.db` file.
    for suffix in ["-wal", "-shm"] {
        let sidecar = PathBuf::from(format!("{}{suffix}", dest.display()));
        if sidecar.exists() {
            let _ = std::fs::remove_file(&sidecar);
        }
    }
    Ok(())
}
