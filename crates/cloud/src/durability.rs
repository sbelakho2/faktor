//! Commercial control-plane database durability (P1 audit finding).
//!
//! # Why this module exists
//!
//! The commercial databases (control-plane identity/auth/RBAC, the billing
//! usage + credit ledger, the enterprise audit/retention ledger, and the SCM /
//! worker / updater stores) used to open with `synchronous = NORMAL` and no
//! backup/integrity machinery, while the main daemon store participates in
//! the startup-backup + doctor stack. An ACKNOWLEDGED financial or
//! control-plane commit (a credit grant, a usage settle, an approval, an
//! entitlement change) must be at least as durable as the main store: with
//! `NORMAL` a power loss can roll back the WAL tail of an acknowledged
//! commit.
//!
//! # Documented policy choice
//!
//! **Per-connection `synchronous = FULL` at open** (not a per-transaction
//! pragma lift): the commercial stores are single-writer, low-throughput
//! control-plane databases, so paying one WAL fsync per commit is cheap and
//! removes every "did this transaction take the lift?" review surface. Every
//! commit acknowledged by these stores therefore fsyncs the WAL first.
//!
//! # LOCAL deployment authority
//!
//! This database is the **LOCAL commercial deployment authority**: a single
//! daemon owns the file under its data dir. Hosted/multi-tenant deployments
//! MUST move these authorities (identity, auth, billing, credits, audit,
//! retention) to a transactional service; a local SQLite file is not a
//! multi-writer commercial system of record. `doctor` prints this caveat in
//! its `cloud-db` section.
//!
//! # Backup / restore-point machinery
//!
//! Following the main store's conventions (`backup_to` + interval gate +
//! bounded rotation + atomic publication through `faktor_fs::atomic`):
//!
//! - a verified pre-migration restore point is written BEFORE any schema
//!   migration leaves `user_version = 0`; if the restore point cannot be
//!   written and verified, the migration is REFUSED (open fails);
//! - an interval-gated rotating backup runs at open (no backup, stale backup,
//!   or changed database size), bounded by count and total bytes;
//! - every backup is restore-verified immediately: reopened read-only, full
//!   `PRAGMA integrity_check`, and a canonical table/row-count digest compared
//!   against the source. A backup that fails verification is deleted, never
//!   kept as a restore point.
//!
//! The public [`doctor_probe`] surface is read-only and serves the CLI
//! `cloud-db` doctor section.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

use crate::store::CloudStoreError;

/// The acknowledged-durability policy recorded in the database itself.
pub const SYNC_POLICY: &str = "FULL";
/// The marker value naming the rotating-backup policy.
pub const BACKUP_POLICY: &str = "rotating";
/// Version of the durability policy recorded by [`record_open_policy`].
pub const POLICY_VERSION: i64 = 1;
/// Backup directory under the database's data root.
pub const BACKUP_DIR_NAME: &str = "commercial-backups";
/// Retention: at most this many ROTATING backups per database.
pub const MAX_BACKUP_FILES: usize = 8;
/// Retention: at most this many bytes across one database's rotating backups.
pub const MAX_BACKUP_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
/// Retention: at most this many pre-migration restore points per database.
pub const MAX_MIGRATION_BACKUPS: usize = 4;
/// A rotating backup is due when the newest is older than this.
pub const BACKUP_MIN_INTERVAL_MS: u64 = 3_600_000;
/// Interrupted-backup temp files older than this are swept.
pub const STALE_TMP_AGE_SECS: u64 = 3600;
/// The infix naming a pre-migration restore point.
pub const MIGRATION_MARKER: &str = "-pre-migration-v";
/// Bound on integrity-check issues retained per probe (loud, not unbounded).
pub const MAX_INTEGRITY_ISSUES: usize = 64;

/// The migration v5 schema of the commercial durability policy marker. The
/// marker is how `doctor` can observe the policy of the WRITER connection
/// (SQLite pragmas are connection-scoped, so a fresh doctor connection cannot
/// see what the store set).
pub const POLICY_SCHEMA_V5: &str = "
     CREATE TABLE IF NOT EXISTS cp_durability_policy (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL,
        recorded_ms INTEGER NOT NULL
     );";

fn sql(e: rusqlite::Error) -> CloudStoreError {
    CloudStoreError::Backend(e.to_string())
}

/// Process-lifetime monotonic suffix so two backups in the same millisecond
/// never collide.
static BACKUP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Wall-clock milliseconds (file naming + marker timestamps only; never an
/// authority clock).
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The acknowledged-durability open policy: WAL + `synchronous = FULL` +
/// busy-timeout + foreign keys. Called on every open (file-backed and
/// in-memory) BEFORE any migration or query.
pub fn apply_policy(conn: &Connection) -> Result<(), CloudStoreError> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = FULL;
         PRAGMA busy_timeout = 5000;
         PRAGMA foreign_keys = ON;",
    )
    .map_err(sql)
}

/// Record the writer's durability policy in the database itself (the marker
/// `doctor` reads; see [`POLICY_SCHEMA_V5`]). Idempotent.
pub fn record_open_policy(conn: &Connection, recorded_ms: i64) -> Result<(), CloudStoreError> {
    conn.execute_batch(POLICY_SCHEMA_V5).map_err(sql)?;
    let mut stmt = conn
        .prepare(
            "INSERT OR REPLACE INTO cp_durability_policy (key, value, recorded_ms)
             VALUES (?1, ?2, ?3)",
        )
        .map_err(sql)?;
    for (key, value) in [
        ("synchronous", SYNC_POLICY),
        ("journal_mode", "wal"),
        ("backup_policy", BACKUP_POLICY),
        ("policy_version", "1"),
        ("last_open_ms", ""),
    ] {
        let value = if key == "last_open_ms" {
            recorded_ms.to_string()
        } else {
            value.to_string()
        };
        stmt.execute(params![key, value, recorded_ms])
            .map_err(sql)?;
    }
    Ok(())
}

/// The recorded policy rows, or `None` when the marker table does not exist
/// (a database never opened under this policy).
pub fn read_policy(conn: &Connection) -> Result<Option<Vec<(String, String)>>, CloudStoreError> {
    let exists: Option<String> = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'cp_durability_policy'",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(sql)?;
    if exists.is_none() {
        return Ok(None);
    }
    let mut stmt = conn
        .prepare("SELECT key, value FROM cp_durability_policy ORDER BY key")
        .map_err(sql)?;
    let mut rows = stmt.query([]).map_err(sql)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(sql)? {
        out.push((row.get(0).map_err(sql)?, row.get(1).map_err(sql)?));
    }
    Ok(Some(out))
}

/// The canonical content digest of one commercial database: `user_version`,
/// then every non-internal table's name + row count, hashed with BLAKE3. Two
/// databases with the same canonical digest hold the same schema version and
/// the same row counts per table — the restore-verification comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbFingerprint {
    pub digest: String,
    pub rows: i64,
    pub tables: usize,
}

/// Compute the canonical fingerprint (see [`DbFingerprint`]).
pub fn canonical_fingerprint(conn: &Connection) -> Result<DbFingerprint, CloudStoreError> {
    let user_version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(sql)?;
    let names: Vec<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_master
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
                 ORDER BY name",
            )
            .map_err(sql)?;
        let mut rows = stmt.query([]).map_err(sql)?;
        let mut names = Vec::new();
        while let Some(row) = rows.next().map_err(sql)? {
            names.push(row.get(0).map_err(sql)?);
        }
        names
    };
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"faktor-commercial-db/v1\0");
    hasher.update(&user_version.to_le_bytes());
    let mut total_rows = 0i64;
    for name in &names {
        let quoted = name.replace('"', "\"\"");
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM \"{quoted}\""), [], |r| {
                r.get(0)
            })
            .map_err(sql)?;
        total_rows += count;
        hasher.update(&(name.len() as u64).to_le_bytes());
        hasher.update(name.as_bytes());
        hasher.update(&count.to_le_bytes());
    }
    Ok(DbFingerprint {
        digest: hasher.finalize().to_hex().to_string(),
        rows: total_rows,
        tables: names.len(),
    })
}

/// `PRAGMA integrity_check` (deep) or `quick_check` (bounded), capped at
/// [`MAX_INTEGRITY_ISSUES`] retained issues. Empty = healthy.
pub fn integrity_check(conn: &Connection, deep: bool) -> Result<Vec<String>, CloudStoreError> {
    let pragma = if deep {
        "PRAGMA integrity_check"
    } else {
        "PRAGMA quick_check"
    };
    let mut stmt = conn.prepare(pragma).map_err(sql)?;
    let mut rows = stmt.query([]).map_err(sql)?;
    let mut issues = Vec::new();
    while let Some(row) = rows.next().map_err(sql)? {
        let line: String = row.get(0).map_err(sql)?;
        if line != "ok" {
            issues.push(line);
            if issues.len() >= MAX_INTEGRITY_ISSUES {
                issues.push(format!("... more than {MAX_INTEGRITY_ISSUES} issues"));
                break;
            }
        }
    }
    Ok(issues)
}

/// Online backup through the SQLite backup API into `dest` (the main store's
/// `Store::backup_to` convention).
pub fn backup_to(conn: &Connection, dest: &Path) -> Result<(), CloudStoreError> {
    let mut dst = Connection::open(dest).map_err(sql)?;
    let backup = rusqlite::backup::Backup::new(conn, &mut dst).map_err(sql)?;
    backup
        .run_to_completion(50, std::time::Duration::from_millis(100), None)
        .map_err(sql)?;
    Ok(())
}

/// Restore-verify one backup: reopen READ-ONLY, run the full
/// `PRAGMA integrity_check`, and compare the canonical fingerprint against
/// `expected`. Any deviation is a typed refusal — the file is never kept as
/// a restore point.
pub fn restore_verify(path: &Path, expected: &DbFingerprint) -> Result<(), CloudStoreError> {
    let fail = |e: CloudStoreError| {
        CloudStoreError::Backend(format!(
            "restore verification failed for {}: {e}",
            path.display()
        ))
    };
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| fail(sql(e)))?;
    let issues = integrity_check(&conn, true).map_err(fail)?;
    if !issues.is_empty() {
        return Err(fail(CloudStoreError::Backend(format!(
            "integrity_check {}",
            issues.join("; ")
        ))));
    }
    let actual = canonical_fingerprint(&conn).map_err(fail)?;
    if actual != *expected {
        return Err(fail(CloudStoreError::Backend(format!(
            "canonical digest mismatch (expected {} rows/{} tables/{}; got {} rows/{} tables/{})",
            expected.digest,
            expected.rows,
            expected.tables,
            actual.digest,
            actual.rows,
            actual.tables
        ))));
    }
    Ok(())
}

/// The rotating-backup directory for `db_path` (same data root).
pub fn backup_dir(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(BACKUP_DIR_NAME)
}

fn db_stem(db_path: &Path) -> String {
    db_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("commercial")
        .to_string()
}

fn age_secs(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| m.elapsed().ok())
        .map(|e| e.as_secs())
        .unwrap_or(0)
}

/// Every backup of `db_path` (rotating + migration restore points), newest
/// by mtime first. In-progress snapshots carry a `.tmp-` suffix and are
/// invisible here by construction.
pub fn list_backups(db_path: &Path) -> Vec<PathBuf> {
    let dir = backup_dir(db_path);
    let stem = db_stem(db_path);
    let prefix = format!("{stem}-");
    let Ok(files) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = files
        .flatten()
        .map(|f| f.path())
        .filter(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            name.starts_with(&prefix) && name.ends_with(".db") && !name.contains(".tmp-")
        })
        .collect();
    out.sort_by_key(|p| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH)
    });
    out.reverse();
    out
}

/// The newest rotating backup (migration restore points excluded).
pub fn list_rotating_backups(db_path: &Path) -> Vec<PathBuf> {
    list_backups(db_path)
        .into_iter()
        .filter(|p| {
            !p.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .contains(MIGRATION_MARKER)
        })
        .collect()
}

/// The newest pre-migration restore point and its age in seconds.
pub fn latest_migration_backup(db_path: &Path) -> Option<(PathBuf, u64)> {
    list_backups(db_path)
        .into_iter()
        .find(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .contains(MIGRATION_MARKER)
        })
        .map(|p| {
            let age = age_secs(&p);
            (p, age)
        })
}

/// The newest rotating backup and its age in seconds.
pub fn latest_backup(db_path: &Path) -> Option<(PathBuf, u64)> {
    list_rotating_backups(db_path).into_iter().next().map(|p| {
        let age = age_secs(&p);
        (p, age)
    })
}

/// Interval + staleness gate (main store convention): due when no rotating
/// backup exists, the newest is older than [`BACKUP_MIN_INTERVAL_MS`], or its
/// size no longer matches the live database file (the database changed).
pub fn backup_due(db_path: &Path) -> bool {
    let Ok(db_meta) = std::fs::metadata(db_path) else {
        return false;
    };
    let newest = list_rotating_backups(db_path).into_iter().next();
    let Some(newest_path) = newest else {
        return true;
    };
    let Ok(meta) = std::fs::metadata(&newest_path) else {
        return true;
    };
    let stale = meta
        .modified()
        .ok()
        .and_then(|m| m.elapsed().ok())
        .map(|e| e.as_millis() as u64 >= BACKUP_MIN_INTERVAL_MS)
        .unwrap_or(true);
    stale || meta.len() != db_meta.len()
}

fn unique_db_name(stem: &str, infix: &str) -> String {
    let seq = BACKUP_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{stem}{infix}-{}-{}-{seq}.db", now_ms(), std::process::id())
}

/// Write one rotating backup NOW (no interval gate): snapshot, restore-verify,
/// publish atomically, rotate within bounds. A verification failure deletes
/// the snapshot and refuses (never an unverified restore point).
pub fn force_backup(conn: &Connection, db_path: &Path) -> Result<PathBuf, CloudStoreError> {
    let dir = backup_dir(db_path);
    std::fs::create_dir_all(&dir)
        .map_err(|e| CloudStoreError::Backend(format!("backup dir {}: {e}", dir.display())))?;
    let fingerprint = canonical_fingerprint(conn)?;
    let name = unique_db_name(&db_stem(db_path), "");
    let dest = dir.join(&name);
    let tmp = dir.join(format!(".{name}.tmp"));
    write_verified_backup(conn, &tmp, &dest, &fingerprint)?;
    rotate_rotating(db_path);
    sweep_stale_tmp(&dir);
    Ok(dest)
}

/// The interval-gated backup: `Ok(None)` when not due.
pub fn rotate_backup(
    conn: &Connection,
    db_path: &Path,
) -> Result<Option<PathBuf>, CloudStoreError> {
    if !backup_due(db_path) {
        return Ok(None);
    }
    force_backup(conn, db_path).map(Some)
}

/// Write the verified pre-migration restore point for a database leaving
/// `user_version = from_version`. The caller MUST refuse the migration when
/// this errors: a schema transition without a verified restore point is the
/// exact failure this policy exists to prevent.
pub fn migration_backup(
    conn: &Connection,
    db_path: &Path,
    from_version: i64,
) -> Result<PathBuf, CloudStoreError> {
    let dir = backup_dir(db_path);
    std::fs::create_dir_all(&dir)
        .map_err(|e| CloudStoreError::Backend(format!("backup dir {}: {e}", dir.display())))?;
    let fingerprint = canonical_fingerprint(conn)?;
    let name = unique_db_name(
        &db_stem(db_path),
        &format!("{MIGRATION_MARKER}{from_version}"),
    );
    let dest = dir.join(&name);
    let tmp = dir.join(format!(".{name}.tmp"));
    write_verified_backup(conn, &tmp, &dest, &fingerprint)?;
    rotate_migration(db_path);
    sweep_stale_tmp(&dir);
    Ok(dest)
}

/// Shared snapshot -> verify -> adopt step. Any failure removes the temp and
/// returns the error; `dest` is only ever published whole.
fn write_verified_backup(
    conn: &Connection,
    tmp: &Path,
    dest: &Path,
    fingerprint: &DbFingerprint,
) -> Result<(), CloudStoreError> {
    if let Err(e) = backup_to(conn, tmp) {
        let _ = std::fs::remove_file(tmp);
        return Err(e);
    }
    if let Err(e) = restore_verify(tmp, fingerprint) {
        let _ = std::fs::remove_file(tmp);
        return Err(e);
    }
    if let Err(e) = faktor_fs::atomic::atomic_adopt(tmp, dest) {
        let _ = std::fs::remove_file(tmp);
        return Err(CloudStoreError::Backend(format!(
            "backup publication failed for {}: {e}",
            dest.display()
        )));
    }
    Ok(())
}

/// Retention for rotating backups: newest [`MAX_BACKUP_FILES`] and at most
/// [`MAX_BACKUP_TOTAL_BYTES`] total (the just-written snapshot is newest and
/// never a victim). Migration restore points are never touched here.
fn rotate_rotating(db_path: &Path) {
    let files = list_rotating_backups(db_path);
    let mut total: u64 = files
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    let mut kept = files.len();
    for victim in files.iter().rev() {
        let over_count = kept > MAX_BACKUP_FILES;
        let over_bytes = total > MAX_BACKUP_TOTAL_BYTES && kept > 1;
        if !over_count && !over_bytes {
            break;
        }
        if let Ok(m) = std::fs::metadata(victim) {
            total = total.saturating_sub(m.len());
        }
        if std::fs::remove_file(victim).is_err() {
            break;
        }
        kept -= 1;
    }
}

/// Retention for pre-migration restore points: newest
/// [`MAX_MIGRATION_BACKUPS`].
fn rotate_migration(db_path: &Path) {
    let mut points: Vec<PathBuf> = list_backups(db_path)
        .into_iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .contains(MIGRATION_MARKER)
        })
        .collect();
    let mut kept = points.len();
    while kept > MAX_MIGRATION_BACKUPS {
        let Some(victim) = points.pop() else {
            break;
        };
        if std::fs::remove_file(&victim).is_err() {
            break;
        }
        kept -= 1;
    }
}

/// Remove interrupted-backup temp files older than [`STALE_TMP_AGE_SECS`].
fn sweep_stale_tmp(dir: &Path) {
    let Ok(files) = std::fs::read_dir(dir) else {
        return;
    };
    for f in files.flatten() {
        let p = f.path();
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.contains(".tmp") && age_secs(&p) >= STALE_TMP_AGE_SECS {
            let _ = std::fs::remove_file(&p);
        }
    }
}

/// The read-only report `doctor` prints for one commercial database.
#[derive(Debug, Clone)]
pub struct CommercialDbReport {
    pub path: PathBuf,
    pub journal_mode: String,
    /// The writer-recorded policy rows (`None` = never opened under policy).
    pub policy: Option<Vec<(String, String)>>,
    /// Integrity issues; empty = healthy.
    pub integrity: Vec<String>,
    pub last_backup: Option<(PathBuf, u64)>,
    pub backup_count: usize,
    pub migration_restore_point: Option<(PathBuf, u64)>,
    /// Canonical content fingerprint (row counts + digest) of the LIVE
    /// database, the same comparison a restore verification performs.
    pub fingerprint: DbFingerprint,
}

/// READ-ONLY doctor probe: pragma state, recorded policy, integrity, backup
/// age, migration restore-point presence and the canonical fingerprint.
/// Never writes to the database.
pub fn doctor_probe(db_path: &Path, deep: bool) -> Result<CommercialDbReport, CloudStoreError> {
    let conn =
        Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(sql)?;
    let journal_mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .map_err(sql)?;
    let policy = read_policy(&conn)?;
    let integrity = integrity_check(&conn, deep)?;
    let fingerprint = canonical_fingerprint(&conn)?;
    Ok(CommercialDbReport {
        path: db_path.to_path_buf(),
        journal_mode,
        policy,
        integrity,
        last_backup: latest_backup(db_path),
        backup_count: list_rotating_backups(db_path).len(),
        migration_restore_point: latest_migration_backup(db_path),
        fingerprint,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_db(path: &Path) -> Connection {
        let conn = Connection::open(path).unwrap();
        apply_policy(&conn).unwrap();
        conn
    }

    #[test]
    fn policy_is_full_and_recorded_and_doctor_sees_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control-plane.db");
        {
            let conn = open_db(&path);
            record_open_policy(&conn, now_ms()).unwrap();
            let sync: i64 = conn
                .query_row("PRAGMA synchronous", [], |r| r.get(0))
                .unwrap();
            assert_eq!(sync, 2, "the writer connection must be synchronous=FULL");
        }
        let report = doctor_probe(&path, false).unwrap();
        assert_eq!(report.journal_mode, "wal");
        let policy = report.policy.expect("marker recorded");
        let sync = policy
            .iter()
            .find(|(k, _)| k == "synchronous")
            .map(|(_, v)| v.as_str());
        assert_eq!(sync, Some("FULL"));
        assert!(report.integrity.is_empty());
    }

    #[test]
    fn backup_roundtrip_is_restore_verified_and_rotation_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control-plane.db");
        let conn = open_db(&path);
        conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY, v INTEGER);")
            .unwrap();
        for i in 0..3 {
            conn.execute(
                "INSERT INTO t (id, v) VALUES (?1, ?2)",
                params![i.to_string(), i],
            )
            .unwrap();
        }
        // More snapshots than the retention bound: rotation must bound them.
        for _ in 0..(MAX_BACKUP_FILES + 4) {
            let dest = force_backup(&conn, &path).unwrap();
            let fp = canonical_fingerprint(&conn).unwrap();
            restore_verify(&dest, &fp).unwrap();
        }
        assert_eq!(list_rotating_backups(&path).len(), MAX_BACKUP_FILES);
        let (newest, _) = latest_backup(&path).unwrap();
        let fp = canonical_fingerprint(&conn).unwrap();
        restore_verify(&newest, &fp).unwrap();
    }

    #[test]
    fn a_corrupted_backup_fails_restore_verification_and_is_never_adopted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control-plane.db");
        let conn = open_db(&path);
        conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY);")
            .unwrap();
        conn.execute("INSERT INTO t (id) VALUES ('a')", []).unwrap();
        let fp = canonical_fingerprint(&conn).unwrap();
        let dir_backups = backup_dir(&path);
        std::fs::create_dir_all(&dir_backups).unwrap();
        let tmp = dir_backups.join(".corrupt.db.tmp");
        backup_to(&conn, &tmp).unwrap();
        // Truncate the snapshot: the canonical digest must refuse it.
        let bytes = std::fs::read(&tmp).unwrap();
        std::fs::write(&tmp, &bytes[..bytes.len() / 2]).unwrap();
        let err = restore_verify(&tmp, &fp).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("restore verification failed"),
            "typed refusal names verification: {msg}"
        );
    }

    #[test]
    fn migration_restore_point_is_verified_and_listed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("billing.db");
        let conn = open_db(&path);
        conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY);")
            .unwrap();
        conn.execute("INSERT INTO t (id) VALUES ('a')", []).unwrap();
        let point = migration_backup(&conn, &path, 4).unwrap();
        let fp = canonical_fingerprint(&conn).unwrap();
        restore_verify(&point, &fp).unwrap();
        let (latest, _) = latest_migration_backup(&path).unwrap();
        assert_eq!(latest, point);
        assert!(latest
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .contains("-pre-migration-v4-"));
    }

    #[test]
    fn doctor_probe_on_a_non_database_file_is_a_typed_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control-plane.db");
        std::fs::write(&path, b"this is not a sqlite database at all").unwrap();
        assert!(doctor_probe(&path, false).is_err());
    }
}
