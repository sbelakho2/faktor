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
//!   `PRAGMA integrity_check` as the structural pre-check, and a canonical
//!   CONTENT digest (schema SQL + every column value of every row; see
//!   [`DbFingerprint`]) compared against the source. A backup that fails
//!   verification is deleted, never kept as a restore point.
//!
//! The public [`doctor_probe`] surface is read-only and serves the CLI
//! `cloud-db` doctor section.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::types::ValueRef;
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
/// A verified rotating backup older than this fails `doctor` loudly: a backup
/// that old no longer bounds the loss window.
pub const BACKUP_MAX_AGE_SECS: u64 = 7 * 24 * 60 * 60;
/// The policy-marker key recording that this database was opened with pending
/// migrations by code that writes a verified pre-migration restore point
/// BEFORE every migration; `doctor` then FAILS when no restore point exists.
pub const RESTORE_POINT_POLICY_KEY: &str = "restore_points";
/// The value of [`RESTORE_POINT_POLICY_KEY`] meaning "a restore point is
/// required and must verify".
pub const RESTORE_POINT_POLICY_REQUIRED: &str = "required";
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

/// Mark (idempotently) that this database requires a pre-migration restore
/// point; `doctor` fails when one is missing. Recorded by the store that
/// actually migrated the database (`faktor-cloud`'s own ladder).
pub fn require_migration_restore_point(conn: &Connection) -> Result<(), CloudStoreError> {
    conn.execute_batch(POLICY_SCHEMA_V5).map_err(sql)?;
    conn.execute(
        "INSERT OR IGNORE INTO cp_durability_policy (key, value, recorded_ms)
         VALUES (?1, ?2, ?3)",
        params![
            RESTORE_POINT_POLICY_KEY,
            RESTORE_POINT_POLICY_REQUIRED,
            now_ms()
        ],
    )
    .map_err(sql)?;
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

/// The canonical CONTENT fingerprint of one commercial database (P1 audit
/// finding): `application_id` + `user_version`, the SQL of every user schema
/// object, and every column value of every row of every content table, folded
/// into one BLAKE3 digest.
///
/// The stream encoding is typed and length-prefixed, so it is unambiguous:
/// NULL is distinct from an empty string and from an empty blob, text is
/// distinct from a blob holding the same bytes, integers are little-endian
/// `i64`, floats are hashed by stored bit pattern (`to_bits`, so infinities
/// and subnormals hash apart; SQLite canonicalizes `-0.0` and NaN before they
/// are ever stored), and text is hashed as the exact stored bytes (interior
/// NUL bytes included, no re-encoding).
///
/// Two databases with the same digest hold the same schema and the same row
/// content — not merely the same schema version, table names and row counts.
/// Rows are streamed one at a time in a canonical order, so hashing never
/// materializes the database in memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbFingerprint {
    /// Hex BLAKE3 digest of the canonical content encoding.
    pub digest: String,
    /// Total rows hashed across all content tables.
    pub rows: i64,
    /// Content tables hashed (`sqlite_sequence` / `sqlite_stat*` included).
    pub tables: usize,
}

/// Domain separator for the content-fingerprint stream. Bump it whenever the
/// encoding changes so digests from different encodings can never compare
/// equal.
const FINGERPRINT_DOMAIN_V2: &[u8] = b"faktor-commercial-db/content/v2\0";

/// Stream tags. Every variable-length payload is length-prefixed
/// (`u64` LE + bytes), so concatenation is unambiguous.
const TAG_SCHEMA_OBJECT: u8 = 0x01;
const TAG_TABLE_START: u8 = 0x02;
const TAG_ROW: u8 = 0x03;
const TAG_TABLE_END: u8 = 0x04;
const TAG_NULL: u8 = 0x10;
const TAG_INTEGER: u8 = 0x11;
const TAG_REAL: u8 = 0x12;
const TAG_TEXT: u8 = 0x13;
const TAG_BLOB: u8 = 0x14;

fn hash_len_prefixed(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// One content table and the canonical row order derived from its schema.
struct ContentTable {
    name: String,
    without_rowid: bool,
    columns: Vec<String>,
    /// PRIMARY KEY columns in key order; empty for a rowid table that
    /// declares no primary key.
    pk: Vec<String>,
}

impl ContentTable {
    /// The `ORDER BY` clause giving this table a stable canonical row order:
    /// PRIMARY KEY order when declared (for `WITHOUT ROWID` this is the only
    /// legal identity), else `rowid` order. For rowid tables with a PRIMARY
    /// KEY the rowid is appended as a final tie-break (a rowid-table PRIMARY
    /// KEY may legally contain NULLs, which are not a total order). In the
    /// pathological case where every rowid alias is shadowed by a declared
    /// column, EVERY remaining column is appended instead: the result is a
    /// total order over row CONTENT (identical rows are interchangeable, so
    /// their relative order cannot change the digest) — never the bare PK,
    /// which a nullable/duplicate PK would leave ambiguous.
    fn canonical_order(&self) -> Result<String, CloudStoreError> {
        let pk = self
            .pk
            .iter()
            .map(|c| quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ");
        if self.without_rowid {
            // SQLite requires every WITHOUT ROWID table to declare a PK.
            return Ok(pk);
        }
        if !self.pk.is_empty() {
            return Ok(match self.rowid_alias() {
                Some(alias) => format!("{pk}, {alias}"),
                None => {
                    let rest = self
                        .columns
                        .iter()
                        .filter(|c| !self.pk.iter().any(|p| p.eq_ignore_ascii_case(c)))
                        .map(|c| quote_ident(c))
                        .collect::<Vec<_>>()
                        .join(", ");
                    if rest.is_empty() {
                        pk
                    } else {
                        format!("{pk}, {rest}")
                    }
                }
            });
        }
        match self.rowid_alias() {
            Some(alias) => Ok(alias.to_string()),
            None => Err(CloudStoreError::Backend(format!(
                "content fingerprint: table \"{}\" declares no PRIMARY KEY and every rowid \
                 alias (rowid/_rowid_/oid) is shadowed by a column, so no stable canonical \
                 row order exists — refusing to hash content unordered",
                self.name
            ))),
        }
    }

    /// A rowid alias not shadowed by a declared column, if one exists.
    fn rowid_alias(&self) -> Option<&'static str> {
        ["rowid", "_rowid_", "oid"]
            .into_iter()
            .find(|alias| !self.columns.iter().any(|c| c.eq_ignore_ascii_case(alias)))
    }
}

/// Content tables in canonical (name) order. `PRAGMA table_list` classifies
/// real vs virtual tables; shadow tables of virtual tables are real content
/// and are included, virtual tables themselves contribute schema SQL only.
/// `sqlite_sequence` and `sqlite_stat*` are hashed when present: they are
/// durable state, not derived schema.
fn content_tables(conn: &Connection) -> Result<Vec<ContentTable>, CloudStoreError> {
    let mut stmt = conn
        .prepare(
            "SELECT name, wr FROM pragma_table_list
             WHERE schema = 'main' AND type IN ('table', 'shadow')
             ORDER BY name",
        )
        .map_err(sql)?;
    let mut out = Vec::new();
    let mut rows = stmt.query([]).map_err(sql)?;
    while let Some(row) = rows.next().map_err(sql)? {
        let name: String = row.get(0).map_err(sql)?;
        if name == "sqlite_schema" {
            continue;
        }
        let wr: i64 = row.get(1).map_err(sql)?;
        let mut col_stmt = conn
            .prepare("SELECT name, pk FROM pragma_table_info(?1) ORDER BY cid")
            .map_err(sql)?;
        let mut col_rows = col_stmt.query([&name]).map_err(sql)?;
        let mut columns = Vec::new();
        let mut pk = Vec::new();
        while let Some(col) = col_rows.next().map_err(sql)? {
            let col_name: String = col.get(0).map_err(sql)?;
            let pk_ord: i64 = col.get(1).map_err(sql)?;
            if pk_ord > 0 {
                pk.push((pk_ord, col_name.clone()));
            }
            columns.push(col_name);
        }
        pk.sort();
        out.push(ContentTable {
            name,
            without_rowid: wr != 0,
            columns,
            pk: pk.into_iter().map(|(_, name)| name).collect(),
        });
    }
    Ok(out)
}

/// Hash the database identity pragmas (`application_id`, `user_version`)
/// followed by the SQL of every user schema object (table, index, trigger,
/// view — ordered by type then name). Auto-indexes and internal `sqlite_%`
/// schemas are excluded: they are derived, not declared. Schema drift
/// therefore changes the digest even when every row count is unchanged.
fn hash_schema(conn: &Connection, hasher: &mut blake3::Hasher) -> Result<(), CloudStoreError> {
    let application_id: i64 = conn
        .query_row("PRAGMA application_id", [], |r| r.get(0))
        .map_err(sql)?;
    let user_version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(sql)?;
    hasher.update(&application_id.to_le_bytes());
    hasher.update(&user_version.to_le_bytes());
    let mut stmt = conn
        .prepare(
            "SELECT type, name, sql FROM sqlite_master
             WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%'
             ORDER BY type, name",
        )
        .map_err(sql)?;
    let mut rows = stmt.query([]).map_err(sql)?;
    while let Some(row) = rows.next().map_err(sql)? {
        let kind: String = row.get(0).map_err(sql)?;
        let name: String = row.get(1).map_err(sql)?;
        let definition: String = row.get(2).map_err(sql)?;
        hasher.update(&[TAG_SCHEMA_OBJECT]);
        hash_len_prefixed(hasher, kind.as_bytes());
        hash_len_prefixed(hasher, name.as_bytes());
        hash_len_prefixed(hasher, definition.as_bytes());
    }
    Ok(())
}

/// Stream one table's rows in canonical order into `hasher`, returning the
/// row count. One row (and one value) is live at a time: bounded memory, no
/// whole-table materialization. Values are fetched through [`ValueRef`] so no
/// per-value Rust allocation/copy is needed.
fn hash_table_content(
    conn: &Connection,
    table: &ContentTable,
    hasher: &mut blake3::Hasher,
) -> Result<i64, CloudStoreError> {
    let order = table.canonical_order()?;
    let query = format!(
        "SELECT * FROM {} ORDER BY {order}",
        quote_ident(&table.name)
    );
    let mut stmt = conn.prepare(&query).map_err(sql)?;
    let columns = stmt.column_count();
    hasher.update(&[TAG_TABLE_START]);
    hash_len_prefixed(hasher, table.name.as_bytes());
    let mut rows = stmt.query([]).map_err(sql)?;
    let mut count = 0i64;
    while let Some(row) = rows.next().map_err(sql)? {
        hasher.update(&[TAG_ROW]);
        for i in 0..columns {
            match row.get_ref(i).map_err(sql)? {
                ValueRef::Null => {
                    hasher.update(&[TAG_NULL]);
                }
                ValueRef::Integer(v) => {
                    hasher.update(&[TAG_INTEGER]);
                    hasher.update(&v.to_le_bytes());
                }
                ValueRef::Real(v) => {
                    hasher.update(&[TAG_REAL]);
                    hasher.update(&v.to_bits().to_le_bytes());
                }
                ValueRef::Text(bytes) => {
                    hasher.update(&[TAG_TEXT]);
                    hash_len_prefixed(hasher, bytes);
                }
                ValueRef::Blob(bytes) => {
                    hasher.update(&[TAG_BLOB]);
                    hash_len_prefixed(hasher, bytes);
                }
            }
        }
        count += 1;
    }
    hasher.update(&[TAG_TABLE_END]);
    Ok(count)
}

/// Compute the canonical content fingerprint (see [`DbFingerprint`]).
///
/// Tables are hashed in name order, rows in schema-derived canonical order
/// (PRIMARY KEY order, else rowid order; see [`ContentTable::canonical_order`]),
/// so a backup that reorders pages but not rows verifies equal. This is a
/// maintenance path (backup/restore verification and `doctor`), not a hot
/// path: it reads every row once, bounded streaming.
pub fn canonical_fingerprint(conn: &Connection) -> Result<DbFingerprint, CloudStoreError> {
    let tables = content_tables(conn)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(FINGERPRINT_DOMAIN_V2);
    hash_schema(conn, &mut hasher)?;
    let mut total_rows = 0i64;
    for table in &tables {
        total_rows += hash_table_content(conn, table, &mut hasher)?;
    }
    Ok(DbFingerprint {
        digest: hasher.finalize().to_hex().to_string(),
        rows: total_rows,
        tables: tables.len(),
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
/// `PRAGMA integrity_check` as the structural pre-check, and compare the
/// canonical content fingerprint against `expected`. The integrity check
/// alone cannot see a logically corrupted but structurally sound page, so the
/// content digest is what actually binds the backup to the source: a matching
/// digest with different values is a typed refusal — the file is never kept
/// as a restore point.
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

/// One snapshot-time change signal: the live main DB size/mtime plus the WAL
/// size/mtime. In WAL mode a committed transaction can leave the main `.db`
/// byte-identical (the frames live in `-wal`), so the main file alone is NOT a
/// change signal; the WAL is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChangeProbe {
    db_len: u64,
    db_mtime_ns: u128,
    wal_len: u64,
    wal_mtime_ns: u128,
}

fn mtime_ns(path: &Path) -> u128 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn wal_path(db_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}-wal", db_path.display()))
}

fn change_probe(db_path: &Path) -> ChangeProbe {
    let db_meta = std::fs::metadata(db_path).ok();
    let wal = wal_path(db_path);
    let wal_meta = std::fs::metadata(&wal).ok();
    ChangeProbe {
        db_len: db_meta.as_ref().map(|m| m.len()).unwrap_or(0),
        db_mtime_ns: mtime_ns(db_path),
        wal_len: wal_meta.as_ref().map(|m| m.len()).unwrap_or(0),
        wal_mtime_ns: if wal_meta.is_some() {
            mtime_ns(&wal)
        } else {
            0
        },
    }
}

/// The sidecar recording the probe of the live source at snapshot time. A
/// non-`.db` name so [`list_backups`] never mistakes it for a restore point.
fn change_marker_path(backup: &Path) -> PathBuf {
    let name = backup
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("backup");
    backup.with_file_name(format!("{name}.change"))
}

fn write_change_marker(dest: &Path, probe: ChangeProbe) {
    let body = format!(
        "{}\n{}\n{}\n{}\n",
        probe.db_len, probe.db_mtime_ns, probe.wal_len, probe.wal_mtime_ns
    );
    if let Err(e) = std::fs::write(change_marker_path(dest), body) {
        // Best effort: a missing/stale marker makes the next tick back up
        // again (the safe direction), never hides a change.
        tracing::warn!("commercial backup change marker not written: {e}");
    }
}

fn read_change_marker(backup: &Path) -> Option<ChangeProbe> {
    let body = std::fs::read_to_string(change_marker_path(backup)).ok()?;
    let mut lines = body.lines();
    Some(ChangeProbe {
        db_len: lines.next()?.parse().ok()?,
        db_mtime_ns: lines.next()?.parse().ok()?,
        wal_len: lines.next()?.parse().ok()?,
        wal_mtime_ns: lines.next()?.parse().ok()?,
    })
}

fn remove_change_marker(backup: &Path) {
    let _ = std::fs::remove_file(change_marker_path(backup));
}

/// Interval + staleness gate (main store convention): due when no rotating
/// backup exists, the newest is older than [`BACKUP_MIN_INTERVAL_MS`], or the
/// source CHANGED since the snapshot. The change signal compares the
/// snapshot-time probe (sidecar) against the live probe; when no marker exists
/// (a legacy snapshot) the main-file size fallback applies.
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
    let changed = match read_change_marker(&newest_path) {
        Some(recorded) => recorded != change_probe(db_path),
        None => meta.len() != db_meta.len(),
    };
    stale || changed
}

fn unique_db_name(stem: &str, infix: &str) -> String {
    let seq = BACKUP_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{stem}{infix}-{}-{}-{seq}.db", now_ms(), std::process::id())
}

/// Fingerprint + raw snapshot from ONE SQLite read snapshot: the deferred
/// read transaction pins the snapshot at the first read, so a concurrent
/// commit can no longer make a correct backup look wrong (and be deleted).
fn snapshot_once(conn: &Connection, tmp: &Path) -> Result<DbFingerprint, CloudStoreError> {
    let tx = conn.unchecked_transaction().map_err(sql)?;
    let fingerprint = match canonical_fingerprint(&tx) {
        Ok(fingerprint) => fingerprint,
        Err(e) => {
            let _ = tx.rollback();
            return Err(e);
        }
    };
    if let Err(e) = backup_to(&tx, tmp) {
        let _ = tx.rollback();
        let _ = std::fs::remove_file(tmp);
        return Err(e);
    }
    tx.rollback().map_err(sql)?;
    Ok(fingerprint)
}

/// Write one rotating backup NOW (no interval gate): snapshot, restore-verify,
/// publish atomically, rotate within bounds. A verification failure removes
/// the temp file; ONE retry is allowed when the verify disagrees with the
/// snapshot fingerprint (a concurrent writer that slipped past the read
/// transaction), so a correct snapshot is never deleted on the first
/// disagreement.
pub fn force_backup(conn: &Connection, db_path: &Path) -> Result<PathBuf, CloudStoreError> {
    let dir = backup_dir(db_path);
    std::fs::create_dir_all(&dir)
        .map_err(|e| CloudStoreError::Backend(format!("backup dir {}: {e}", dir.display())))?;
    let name = unique_db_name(&db_stem(db_path), "");
    let dest = dir.join(&name);
    let tmp = dir.join(format!(".{name}.tmp"));
    let mut last: Option<CloudStoreError> = None;
    for attempt in 0..2u8 {
        match snapshot_once(conn, &tmp) {
            Ok(fingerprint) => match verify_and_publish(&tmp, &dest, &fingerprint, None) {
                Ok(()) => {
                    write_change_marker(&dest, change_probe(db_path));
                    rotate_rotating(db_path);
                    sweep_stale_tmp(&dir);
                    return Ok(dest);
                }
                Err(e) => {
                    tracing::warn!(
                        "commercial backup attempt {} failed verification, retrying once: {e}",
                        attempt + 1
                    );
                    last = Some(e);
                }
            },
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| {
        CloudStoreError::Backend(format!("backup of {} failed", db_path.display()))
    }))
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

/// The schema version stored in one (restore-point) database file.
fn snapshot_user_version(path: &Path) -> Result<i64, CloudStoreError> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(sql)?;
    conn.query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(sql)
}

/// Write the verified pre-migration restore point for a database leaving
/// `user_version = from_version`. The caller MUST refuse the migration when
/// this errors: a schema transition without a verified restore point is the
/// exact failure this policy exists to prevent.
///
/// Self-consistency is enforced twice, so a mislabeled snapshot is impossible:
/// the LIVE connection must still be at `from_version` when the snapshot is
/// taken, and the published file's own `user_version` must equal the version
/// its name claims.
pub fn migration_backup(
    conn: &Connection,
    db_path: &Path,
    from_version: i64,
) -> Result<PathBuf, CloudStoreError> {
    let dir = backup_dir(db_path);
    std::fs::create_dir_all(&dir)
        .map_err(|e| CloudStoreError::Backend(format!("backup dir {}: {e}", dir.display())))?;
    let live_version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(sql)?;
    if live_version != from_version {
        return Err(CloudStoreError::Backend(format!(
            "refusing to label a restore point pre-migration-v{from_version}: the live database \
             is at schema v{live_version} (a concurrent opener already migrated it)"
        )));
    }
    let fingerprint = canonical_fingerprint(conn)?;
    let name = unique_db_name(
        &db_stem(db_path),
        &format!("{MIGRATION_MARKER}{from_version}"),
    );
    let dest = dir.join(&name);
    let tmp = dir.join(format!(".{name}.tmp"));
    if let Err(e) = backup_to(conn, &tmp) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    verify_and_publish(&tmp, &dest, &fingerprint, Some(from_version))?;
    rotate_migration(db_path);
    sweep_stale_tmp(&dir);
    Ok(dest)
}

/// Snapshot -> verify -> adopt step. Any failure removes the temp and returns
/// the error; `dest` is only ever published whole. `expected_version` makes
/// the pre-migration claim self-checking: the file must HOLD the version its
/// name claims.
fn verify_and_publish(
    tmp: &Path,
    dest: &Path,
    fingerprint: &DbFingerprint,
    expected_version: Option<i64>,
) -> Result<(), CloudStoreError> {
    if let Err(e) = restore_verify(tmp, fingerprint) {
        let _ = std::fs::remove_file(tmp);
        return Err(e);
    }
    if let Some(expected) = expected_version {
        match snapshot_user_version(tmp) {
            Ok(actual) if actual == expected => {}
            Ok(actual) => {
                let _ = std::fs::remove_file(tmp);
                return Err(CloudStoreError::Backend(format!(
                    "restore point self-check failed for {}: its name claims pre-migration v{expected} \
                     but its content is schema v{actual}",
                    dest.display()
                )));
            }
            Err(e) => {
                let _ = std::fs::remove_file(tmp);
                return Err(e);
            }
        }
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
        remove_change_marker(victim);
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
        remove_change_marker(&victim);
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
    /// A typed restore-point problem (`None` = verified, or not required for
    /// this database): missing while required, unopenable, integrity-broken,
    /// a name/content version claim mismatch, or a claimed version from the
    /// future. `doctor` fails the run on `Some`.
    pub migration_restore_point_issue: Option<String>,
    /// Live schema version (a restore point must predate it).
    pub user_version: i64,
    /// Canonical content fingerprint (digest + row/table counts) of the LIVE
    /// database, the same comparison a restore verification performs.
    pub fingerprint: DbFingerprint,
}

/// Re-verify one pre-migration restore point: reopen it READ-ONLY, run the
/// integrity check, and check that the version its NAME claims is exactly the
/// version its CONTENT holds. `live_version` additionally refuses a point
/// claiming a schema newer than the live database (it cannot protect it).
fn verify_migration_restore_point(
    point: &Path,
    live_version: i64,
    deep: bool,
) -> Result<(), String> {
    let conn = Connection::open_with_flags(point, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("restore point {} cannot be reopened: {e}", point.display()))?;
    let issues = integrity_check(&conn, deep).map_err(|e| {
        format!(
            "restore point {} integrity check failed: {e}",
            point.display()
        )
    })?;
    if !issues.is_empty() {
        return Err(format!(
            "restore point {} is corrupt: {}",
            point.display(),
            issues.join("; ")
        ));
    }
    let actual: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| {
            format!(
                "restore point {} user_version unreadable: {e}",
                point.display()
            )
        })?;
    let claimed = point
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.rsplit_once(MIGRATION_MARKER))
        .and_then(|(_, rest)| {
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            digits.parse::<i64>().ok()
        })
        .ok_or_else(|| {
            format!(
                "restore point {} does not carry a parseable pre-migration version",
                point.display()
            )
        })?;
    if claimed != actual {
        return Err(format!(
            "restore point {} is mislabeled: its name claims schema v{claimed} but its content \
             is schema v{actual} — it cannot be trusted as a way back",
            point.display()
        ));
    }
    if claimed > live_version {
        return Err(format!(
            "restore point {} claims schema v{claimed}, newer than the live schema v{live_version}: \
             it does not belong to this database",
            point.display()
        ));
    }
    Ok(())
}

/// READ-ONLY doctor probe: pragma state, recorded policy, integrity, backup
/// age, migration restore-point presence AND verification, and the canonical
/// fingerprint. Never writes to the database.
pub fn doctor_probe(db_path: &Path, deep: bool) -> Result<CommercialDbReport, CloudStoreError> {
    let conn =
        Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(sql)?;
    let journal_mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .map_err(sql)?;
    let user_version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(sql)?;
    let policy = read_policy(&conn)?;
    let integrity = integrity_check(&conn, deep)?;
    let fingerprint = canonical_fingerprint(&conn)?;
    let restore_points_required = policy
        .as_ref()
        .map(|rows| {
            rows.iter().any(|(key, value)| {
                key == RESTORE_POINT_POLICY_KEY && value == RESTORE_POINT_POLICY_REQUIRED
            })
        })
        .unwrap_or(false);
    let migration_restore_point = latest_migration_backup(db_path);
    let migration_restore_point_issue = match &migration_restore_point {
        Some((point, _)) => verify_migration_restore_point(point, user_version, deep).err(),
        None if restore_points_required => Some(format!(
            "no pre-migration restore point exists although the recorded policy \
             ({RESTORE_POINT_POLICY_KEY}={RESTORE_POINT_POLICY_REQUIRED}) requires one"
        )),
        None => None,
    };
    Ok(CommercialDbReport {
        path: db_path.to_path_buf(),
        journal_mode,
        policy,
        integrity,
        last_backup: latest_backup(db_path),
        backup_count: list_rotating_backups(db_path).len(),
        migration_restore_point,
        migration_restore_point_issue,
        user_version,
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
        // The live schema is at v4; the restore point must hold exactly what
        // its name claims.
        conn.execute_batch("PRAGMA user_version = 4").unwrap();
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
        assert_eq!(
            snapshot_user_version(&point).unwrap(),
            4,
            "the restore point content holds the version its name claims"
        );
    }

    /// Adversarial: a caller may not label post-migration content as a
    /// pre-migration restore point (the concurrent-opener mislabel). The
    /// mismatch between the claimed version and the live schema is refused.
    #[test]
    fn migration_backup_refuses_a_version_claim_that_does_not_match_the_live_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control-plane.db");
        let conn = open_db(&path);
        conn.execute_batch("PRAGMA user_version = 4").unwrap();
        let err = migration_backup(&conn, &path, 3).unwrap_err();
        assert!(
            err.to_string()
                .contains("refusing to label a restore point pre-migration-v3"),
            "{err}"
        );
        assert!(
            latest_migration_backup(&path).is_none(),
            "nothing published"
        );
    }

    #[test]
    fn doctor_probe_on_a_non_database_file_is_a_typed_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control-plane.db");
        std::fs::write(&path, b"this is not a sqlite database at all").unwrap();
        assert!(doctor_probe(&path, false).is_err());
    }

    /// A source database with one mixed-content table plus its verified backup.
    fn source_with_backup(dir: &Path, name: &str) -> (PathBuf, Connection, DbFingerprint, PathBuf) {
        let path = dir.join(name);
        let conn = open_db(&path);
        conn.execute_batch(
            "CREATE TABLE t (id TEXT PRIMARY KEY, amount INTEGER, note TEXT, payload BLOB);",
        )
        .unwrap();
        for (id, amount, note, payload) in [
            ("a", 10i64, "one", &b"aaa"[..]),
            ("b", 20, "two", &b"bbb"[..]),
            ("c", 30, "three", &b"ccc"[..]),
        ] {
            conn.execute(
                "INSERT INTO t (id, amount, note, payload) VALUES (?1, ?2, ?3, ?4)",
                params![id, amount, note, payload],
            )
            .unwrap();
        }
        let expected = canonical_fingerprint(&conn).unwrap();
        let backup = force_backup(&conn, &path).unwrap();
        (path, conn, expected, backup)
    }

    /// Adversarial (the P1 false-verification defect): change ONE value in the
    /// backup without touching any row count or the schema. `integrity_check`
    /// stays clean and the old row-count-only digest accepted this; the
    /// content digest must reject it.
    #[test]
    fn a_value_change_with_unchanged_row_counts_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let (_path, _conn, expected, backup) = source_with_backup(dir.path(), "control-plane.db");
        {
            let tamper = Connection::open(&backup).unwrap();
            tamper
                .execute("UPDATE t SET amount = amount + 1 WHERE id = 'b'", [])
                .unwrap();
            let after = canonical_fingerprint(&tamper).unwrap();
            assert_eq!(after.rows, expected.rows, "row count is unchanged");
            assert_eq!(after.tables, expected.tables, "table set is unchanged");
            assert_eq!(after.digest.len(), expected.digest.len());
            assert_ne!(
                after.digest, expected.digest,
                "a value change must change the content digest"
            );
            // The digest binds to the tampered content: rejection below is
            // content-derived, not an unconditional mismatch.
            restore_verify(&backup, &after).unwrap();
        }
        let conn = Connection::open_with_flags(&backup, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert!(
            integrity_check(&conn, true).unwrap().is_empty(),
            "the structural check cannot see the logical tamper"
        );
        drop(conn);
        let err = restore_verify(&backup, &expected).unwrap_err();
        assert!(
            err.to_string().contains("digest mismatch"),
            "typed refusal: {err}"
        );
    }

    /// Adversarial: schema-only drift (an added column + an added index) with
    /// byte-identical row counts must change the digest and refuse the backup.
    #[test]
    fn a_schema_only_change_with_unchanged_row_counts_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let (_path, _conn, expected, backup) = source_with_backup(dir.path(), "billing.db");
        {
            let mutated = Connection::open(&backup).unwrap();
            mutated
                .execute_batch(
                    "ALTER TABLE t ADD COLUMN extra TEXT NOT NULL DEFAULT '';
                     CREATE INDEX idx_t_note ON t(note);",
                )
                .unwrap();
            let after = canonical_fingerprint(&mutated).unwrap();
            assert_eq!(after.rows, expected.rows, "row count is unchanged");
            assert_eq!(after.tables, expected.tables, "table set is unchanged");
            assert_ne!(
                after.digest, expected.digest,
                "schema drift must change the content digest"
            );
        }
        let conn = Connection::open_with_flags(&backup, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert!(integrity_check(&conn, true).unwrap().is_empty());
        drop(conn);
        assert!(restore_verify(&backup, &expected).is_err());
    }

    /// Adversarial: the digest is canonical in logical row order, not in
    /// physical rowid assignment. The same rows inserted in a different order
    /// into tables with explicit PRIMARY KEYs (including WITHOUT ROWID) must
    /// produce the same digest, and a backup of one database must verify
    /// against the other's fingerprint.
    #[test]
    fn digest_is_canonical_across_insertion_order() {
        let dir = tempfile::tempdir().unwrap();
        let a_path = dir.path().join("a.db");
        let b_path = dir.path().join("b.db");
        let a = open_db(&a_path);
        let b = open_db(&b_path);
        for conn in [&a, &b] {
            conn.execute_batch(
                "CREATE TABLE t (id TEXT PRIMARY KEY, v INTEGER);
                 CREATE TABLE c (k1 TEXT, k2 INTEGER, v TEXT, PRIMARY KEY (k1, k2));
                 CREATE TABLE w (a TEXT, b INTEGER, v TEXT, PRIMARY KEY (a, b)) WITHOUT ROWID;",
            )
            .unwrap();
        }
        let rows = [
            ("alpha", 0i64, "x"),
            ("beta", 1, "y"),
            ("gamma", 2, "z"),
            ("delta", 3, "w"),
        ];
        for (i, (id, key, text)) in rows.iter().enumerate() {
            a.execute("INSERT INTO t (id, v) VALUES (?1, ?2)", params![id, key])
                .unwrap();
            a.execute(
                "INSERT INTO c (k1, k2, v) VALUES (?1, ?2, ?3)",
                params![id, key, text],
            )
            .unwrap();
            a.execute(
                "INSERT INTO w (a, b, v) VALUES (?1, ?2, ?3)",
                params![id, key, text],
            )
            .unwrap();
            let (id, key, text) = rows[rows.len() - 1 - i];
            b.execute("INSERT INTO t (id, v) VALUES (?1, ?2)", params![id, key])
                .unwrap();
            b.execute(
                "INSERT INTO c (k1, k2, v) VALUES (?1, ?2, ?3)",
                params![id, key, text],
            )
            .unwrap();
            b.execute(
                "INSERT INTO w (a, b, v) VALUES (?1, ?2, ?3)",
                params![id, key, text],
            )
            .unwrap();
        }
        let fp_a = canonical_fingerprint(&a).unwrap();
        let fp_b = canonical_fingerprint(&b).unwrap();
        assert_eq!(
            fp_a.digest, fp_b.digest,
            "row order must not depend on insertion history"
        );
        assert_eq!(fp_a.rows, fp_b.rows);
        assert_eq!(fp_a.tables, fp_b.tables);
        let dest = force_backup(&a, &a_path).unwrap();
        restore_verify(&dest, &fp_b).unwrap();
    }

    /// Adversarial: NULL, empty text, empty blob, text-vs-blob with the same
    /// bytes, INTEGER zero and REAL zero are seven distinct cell states; the
    /// digest must not collapse any pair.
    #[test]
    fn null_empty_text_and_empty_blob_are_all_distinct() {
        use std::collections::HashSet;
        let dir = tempfile::tempdir().unwrap();
        let cases: [(&str, &str); 7] = [
            ("null", "INSERT INTO t (id) VALUES ('k')"),
            ("empty-text", "INSERT INTO t (id, val) VALUES ('k', '')"),
            ("empty-blob", "INSERT INTO t (id, val) VALUES ('k', x'')"),
            ("text-x", "INSERT INTO t (id, val) VALUES ('k', 'x')"),
            ("blob-x", "INSERT INTO t (id, val) VALUES ('k', x'78')"),
            ("int-zero", "INSERT INTO t (id, val) VALUES ('k', 0)"),
            ("real-zero", "INSERT INTO t (id, val) VALUES ('k', 0.0)"),
        ];
        let mut digests = HashSet::new();
        for (name, insert) in cases {
            let conn = open_db(&dir.path().join(format!("{name}.db")));
            conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY, val);")
                .unwrap();
            conn.execute(insert, []).unwrap();
            let fp = canonical_fingerprint(&conn).unwrap();
            assert!(
                digests.insert(fp.digest.clone()),
                "{name} collides with an earlier case"
            );
        }
    }

    /// Adversarial: replacing a NULL with an empty string in a verified backup
    /// changes no count and passes `integrity_check`, yet must be refused.
    #[test]
    fn replacing_a_null_with_an_empty_string_in_a_backup_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control-plane.db");
        let conn = open_db(&path);
        conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY, val TEXT);")
            .unwrap();
        conn.execute("INSERT INTO t (id) VALUES ('k')", []).unwrap();
        let expected = canonical_fingerprint(&conn).unwrap();
        let backup = force_backup(&conn, &path).unwrap();
        {
            let tamper = Connection::open(&backup).unwrap();
            tamper
                .execute("UPDATE t SET val = '' WHERE id = 'k'", [])
                .unwrap();
            let after = canonical_fingerprint(&tamper).unwrap();
            assert_eq!(after.rows, expected.rows);
            assert_eq!(after.tables, expected.tables);
            assert_ne!(after.digest, expected.digest);
        }
        let verify =
            Connection::open_with_flags(&backup, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert!(integrity_check(&verify, true).unwrap().is_empty());
        drop(verify);
        assert!(restore_verify(&backup, &expected).is_err());
    }

    /// Adversarial: typed edge values are hashed exactly and never collide —
    /// i64 extremes, infinities, subnormals, f64 extremes, text with interior
    /// NUL bytes, and the same bytes stored as text vs blob. (SQLite itself
    /// canonicalizes `-0.0` to `0.0` and NaN to NULL before storage, so those
    /// never reach the digest as distinct bit patterns.)
    #[test]
    fn typed_edge_values_do_not_collide() {
        use std::collections::HashSet;
        let dir = tempfile::tempdir().unwrap();
        let mut digests = HashSet::new();
        let digest_for = |name: &str, bind: &dyn rusqlite::ToSql| -> String {
            let conn = open_db(&dir.path().join(format!("{name}.db")));
            conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY, val);")
                .unwrap();
            conn.execute("INSERT INTO t (id, val) VALUES ('k', ?1)", [bind])
                .unwrap();
            canonical_fingerprint(&conn).unwrap().digest
        };
        for (i, v) in [i64::MIN, i64::MAX, -1i64, 0, 1].iter().enumerate() {
            assert!(
                digests.insert(digest_for(&format!("int{i}"), v)),
                "integer {v} collided"
            );
        }
        for (i, v) in [
            f64::INFINITY,
            f64::NEG_INFINITY,
            5e-324,
            -5e-324,
            f64::MAX,
            f64::MIN,
            0.0_f64,
        ]
        .iter()
        .enumerate()
        {
            assert!(
                digests.insert(digest_for(&format!("real{i}"), v)),
                "float {v} collided"
            );
        }
        for (i, bytes) in [&b"a"[..], b"a\0b", b"a\0"].iter().enumerate() {
            let text = String::from_utf8_lossy(bytes).into_owned();
            assert!(
                digests.insert(digest_for(&format!("text{i}"), &text)),
                "text {bytes:?} collided"
            );
            let blob = bytes.to_vec();
            assert!(
                digests.insert(digest_for(&format!("blob{i}"), &blob)),
                "blob {bytes:?} collided"
            );
        }
        let empty_blob: Vec<u8> = Vec::new();
        assert!(digests.insert(digest_for("blob-empty", &empty_blob)));
    }

    /// Adversarial: WITHOUT ROWID tables (PRIMARY KEY order) and rowid tables
    /// without a PRIMARY KEY (rowid order) are both content-sensitive.
    #[test]
    fn without_rowid_and_primary_key_less_tables_are_content_sensitive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scm.db");
        let conn = open_db(&path);
        conn.execute_batch(
            "CREATE TABLE wr (a TEXT, b INTEGER, v TEXT, PRIMARY KEY (a, b)) WITHOUT ROWID;
             CREATE TABLE plain (msg TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO wr (a, b, v) VALUES ('x', 1, 'one'), ('y', 2, 'two')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO plain (msg) VALUES ('m1'), ('m2')", [])
            .unwrap();
        let expected = canonical_fingerprint(&conn).unwrap();

        let wr_backup = force_backup(&conn, &path).unwrap();
        {
            let tamper = Connection::open(&wr_backup).unwrap();
            tamper
                .execute("UPDATE wr SET v = 'tampered' WHERE a = 'y'", [])
                .unwrap();
            let after = canonical_fingerprint(&tamper).unwrap();
            assert_eq!(after.rows, expected.rows);
            assert_ne!(after.digest, expected.digest);
        }
        assert!(restore_verify(&wr_backup, &expected).is_err());

        let plain_backup = force_backup(&conn, &path).unwrap();
        {
            let tamper = Connection::open(&plain_backup).unwrap();
            tamper
                .execute("UPDATE plain SET msg = 'tampered' WHERE rowid = 1", [])
                .unwrap();
            let after = canonical_fingerprint(&tamper).unwrap();
            assert_eq!(after.rows, expected.rows);
            assert_ne!(after.digest, expected.digest);
        }
        assert!(restore_verify(&plain_backup, &expected).is_err());
    }

    /// Adversarial: a rowid table that declares no PRIMARY KEY and shadows
    /// every rowid alias cannot be hashed in a stable order. The fingerprint
    /// refuses typed instead of silently hashing an unordered scan (which
    /// would reintroduce false verification).
    #[test]
    fn a_rowid_table_with_all_aliases_shadowed_is_refused() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE weird (rowid TEXT, _rowid_ TEXT, oid TEXT);")
            .unwrap();
        let err = canonical_fingerprint(&conn).unwrap_err();
        assert!(
            err.to_string().contains("no stable canonical row order"),
            "{err}"
        );
    }

    /// F10: a NULLABLE primary key on a rowid table is not a total order, and
    /// when every rowid alias is shadowed the internal rowid cannot be named
    /// either. The canonical order falls back to every remaining column, so
    /// shuffled insertion order (different physical rowids) still hashes the
    /// same content identically.
    #[test]
    fn nullable_primary_key_with_shadowed_aliases_is_canonical() {
        let dir = tempfile::tempdir().unwrap();
        let a = open_db(&dir.path().join("a.db"));
        let b = open_db(&dir.path().join("b.db"));
        for conn in [&a, &b] {
            conn.execute_batch(
                "CREATE TABLE t (
                     rowid TEXT, _rowid_ TEXT, oid TEXT,
                     k TEXT, payload TEXT,
                     PRIMARY KEY (k)
                 );",
            )
            .unwrap();
        }
        let rows = [("r1", "p1"), ("r2", "p2"), ("r3", "p3"), ("r4", "p4")];
        for (rowid, payload) in rows {
            a.execute(
                "INSERT INTO t (rowid, _rowid_, oid, k, payload) VALUES (?1, ?2, ?3, NULL, ?4)",
                params![rowid, rowid, rowid, payload],
            )
            .unwrap();
        }
        for (rowid, payload) in rows.into_iter().rev() {
            b.execute(
                "INSERT INTO t (rowid, _rowid_, oid, k, payload) VALUES (?1, ?2, ?3, NULL, ?4)",
                params![rowid, rowid, rowid, payload],
            )
            .unwrap();
        }
        let fp_a = canonical_fingerprint(&a).unwrap();
        let fp_b = canonical_fingerprint(&b).unwrap();
        assert_eq!(
            fp_a.digest, fp_b.digest,
            "a nullable PK with shadowed rowid aliases must still be canonical"
        );
        assert_eq!(fp_a.rows, fp_b.rows);
        let dest = force_backup(&a, &dir.path().join("a.db")).unwrap();
        restore_verify(&dest, &fp_b).unwrap();
    }

    /// F8: in WAL mode a committed transaction can leave the main `.db` file
    /// byte-identical. The change probe must still see it (WAL size/mtime), so
    /// a WAL-only commit makes a rotating backup due and the next gated backup
    /// picks the committed row up.
    #[test]
    fn a_wal_only_commit_makes_a_backup_due() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control-plane.db");
        let conn = open_db(&path);
        conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY);")
            .unwrap();
        force_backup(&conn, &path).unwrap();
        assert!(
            !backup_due(&path),
            "fresh verified snapshot + unchanged source: not due"
        );
        let before = std::fs::metadata(&path).unwrap().len();
        conn.execute("INSERT INTO t (id) VALUES ('wal-only')", [])
            .unwrap();
        let after = std::fs::metadata(&path).unwrap().len();
        assert_eq!(
            before, after,
            "the commit lives in the WAL, not in the main file"
        );
        assert!(
            backup_due(&path),
            "a WAL-only commit must make a backup due"
        );
        let dest = rotate_backup(&conn, &path).unwrap().expect("due");
        let fp = canonical_fingerprint(&conn).unwrap();
        restore_verify(&dest, &fp).unwrap();
    }

    /// F9: a writer committing WHILE a backup runs must not make the correct
    /// snapshot fail verification and get deleted. Every forced backup under
    /// concurrent commits must publish (one retry is allowed, a correct
    /// snapshot is never removed on the first disagreement).
    #[test]
    fn a_concurrent_writer_never_makes_a_correct_snapshot_false_reject() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control-plane.db");
        let conn = open_db(&path);
        conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY, v INTEGER);")
            .unwrap();
        let writer = open_db(&path);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_writer = stop.clone();
        let handle = std::thread::spawn(move || {
            let mut i = 0i64;
            while !stop_writer.load(Ordering::SeqCst) {
                writer
                    .execute(
                        "INSERT INTO t (id, v) VALUES (?1, ?2)",
                        params![format!("row-{i}"), i],
                    )
                    .unwrap();
                i += 1;
                std::thread::sleep(std::time::Duration::from_micros(100));
            }
            drop(writer);
        });
        for attempt in 0..6 {
            let dest = force_backup(&conn, &path)
                .unwrap_or_else(|e| panic!("backup {attempt} under load: {e}"));
            assert!(
                dest.exists(),
                "a verified snapshot is published, never deleted"
            );
        }
        stop.store(true, Ordering::SeqCst);
        handle.join().unwrap();
        let (newest, _) = latest_backup(&path).unwrap();
        let snapshot =
            Connection::open_with_flags(&newest, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        assert!(
            integrity_check(&snapshot, true).unwrap().is_empty(),
            "the published snapshot is a sound database"
        );
        // The freshly written rows are still in the live database.
        let live: i64 = conn
            .query_row("SELECT COUNT(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert!(live >= 6, "concurrent commits landed: {live}");
    }

    /// F11: `doctor_probe` re-verifies the newest restore point and reports a
    /// typed issue for missing-while-required, mislabeled and unopenable
    /// points (the CLI turns the issue into a loud line naming the database).
    #[test]
    fn doctor_flags_missing_and_mislabeled_restore_points() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control-plane.db");
        let conn = open_db(&path);
        conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY); PRAGMA user_version = 5;")
            .unwrap();
        require_migration_restore_point(&conn).unwrap();

        // Missing while the recorded policy requires one.
        let report = doctor_probe(&path, false).unwrap();
        let issue = report
            .migration_restore_point_issue
            .expect("missing restore point is an issue");
        assert!(issue.contains("no pre-migration restore point"), "{issue}");

        // A truthful pre-migration point verifies: content matches the claim.
        conn.execute_batch("PRAGMA user_version = 4").unwrap();
        migration_backup(&conn, &path, 4).unwrap();
        let report = doctor_probe(&path, false).unwrap();
        assert_eq!(report.migration_restore_point_issue, None);
        assert_eq!(report.user_version, 4);

        // A full copy of the LIVE database under a lower claimed version is
        // mislabeled and must fail verification.
        let mislabeled = backup_dir(&path).join("control-plane-pre-migration-v2-1-1.db");
        backup_to(&conn, &mislabeled).unwrap();
        let report = doctor_probe(&path, false).unwrap();
        let issue = report
            .migration_restore_point_issue
            .expect("mislabeled point is an issue");
        assert!(issue.contains("mislabeled"), "{issue}");

        // A point from a NEWER schema than the live database cannot protect it.
        let future = backup_dir(&path).join("control-plane-pre-migration-v99-1-3.db");
        {
            let future_conn = Connection::open(&future).unwrap();
            future_conn
                .execute_batch("CREATE TABLE f (id TEXT PRIMARY KEY); PRAGMA user_version = 99;")
                .unwrap();
        }
        let report = doctor_probe(&path, false).unwrap();
        let issue = report
            .migration_restore_point_issue
            .expect("future point is an issue");
        assert!(issue.contains("newer than the live schema"), "{issue}");

        // A corrupt point is unverifiable.
        std::fs::write(
            backup_dir(&path).join("control-plane-pre-migration-v1-1-2.db"),
            b"not a database",
        )
        .unwrap();
        let report = doctor_probe(&path, false).unwrap();
        let issue = report
            .migration_restore_point_issue
            .expect("corrupt point is an issue");
        assert!(
            issue.contains("integrity check failed") || issue.contains("cannot be reopened"),
            "{issue}"
        );
    }
}
