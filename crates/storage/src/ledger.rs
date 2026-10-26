//! Durable governor ledger: usage, reservations, object index, pins, loss
//! markers, eviction index and refusal counters.
//!
//! Every admission writes its reservation in one transaction BEFORE the
//! caller publishes bytes, and every publish commits that reservation in a
//! second transaction. A crash between the two leaves either a stale
//! reservation (expired by reconciliation) or an on-disk object that the next
//! full scan indexes — accounting is reconstructed from durable state, never
//! trusted from RAM.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};

use crate::class::StorageClass;
use crate::config::StorageGovernorConfig;
use crate::error::{GovernorError, GovernorResult};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS usage (
    class TEXT PRIMARY KEY,
    bytes INTEGER NOT NULL DEFAULT 0,
    objects INTEGER NOT NULL DEFAULT 0,
    pinned_bytes INTEGER NOT NULL DEFAULT 0,
    updated_ms INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS reservation (
    id TEXT PRIMARY KEY,
    class TEXT NOT NULL,
    key TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    created_ms INTEGER NOT NULL,
    lease_expires_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_reservation_class ON reservation(class, lease_expires_ms);
CREATE TABLE IF NOT EXISTS object_index (
    class TEXT NOT NULL,
    key TEXT NOT NULL,
    bytes INTEGER NOT NULL,
    digest TEXT,
    family TEXT NOT NULL DEFAULT 'main',
    created_ms INTEGER NOT NULL,
    last_used_ms INTEGER NOT NULL,
    required INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (class, key)
);
CREATE INDEX IF NOT EXISTS idx_object_lru
    ON object_index(class, required, last_used_ms, key);
CREATE INDEX IF NOT EXISTS idx_object_global_lru
    ON object_index(required, last_used_ms, key);
CREATE TABLE IF NOT EXISTS pin (
    class TEXT NOT NULL,
    key TEXT NOT NULL,
    reason TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    PRIMARY KEY (class, key)
);
CREATE TABLE IF NOT EXISTS loss_marker (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    class TEXT NOT NULL,
    key TEXT NOT NULL,
    digest TEXT,
    bytes INTEGER NOT NULL,
    reason TEXT NOT NULL,
    created_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_loss_marker_object ON loss_marker(class, key);
CREATE TABLE IF NOT EXISTS eviction (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    class TEXT NOT NULL,
    key TEXT NOT NULL,
    digest TEXT,
    bytes INTEGER NOT NULL,
    verified INTEGER NOT NULL,
    reason TEXT NOT NULL,
    created_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_eviction_object ON eviction(class, key);
CREATE TABLE IF NOT EXISTS refusal (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    class TEXT NOT NULL,
    requested INTEGER NOT NULL,
    available INTEGER NOT NULL,
    reason TEXT NOT NULL,
    detail TEXT NOT NULL,
    created_ms INTEGER NOT NULL
);
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectRow {
    pub class: StorageClass,
    pub key: String,
    pub bytes: u64,
    pub digest: Option<String>,
    pub last_used_ms: i64,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReservationRow {
    pub id: String,
    pub class: StorageClass,
    pub key: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsageRow {
    pub bytes: u64,
    pub objects: u64,
    pub pinned_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Counters {
    pub evictions: u64,
    pub refusals: u64,
    pub loss_markers: u64,
}

pub struct Ledger {
    conn: Connection,
}

fn to_i64(value: u64) -> i64 {
    value.min(i64::MAX as u64) as i64
}

fn from_i64(value: i64) -> u64 {
    value.max(0) as u64
}

impl Ledger {
    pub fn open(path: &Path) -> GovernorResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             PRAGMA busy_timeout = 5000;
             PRAGMA foreign_keys = ON;
             PRAGMA wal_autocheckpoint = 1000;
             PRAGMA journal_size_limit = 16777216;",
        )?;
        conn.execute_batch(SCHEMA)?;
        // Same-root digest uniqueness (main CAS logical classes) while the
        // evidence CAS keeps its own physical digest namespace. The ALTER is
        // a no-op on fresh databases (the column is already declared).
        let _ = conn.execute(
            "ALTER TABLE object_index ADD COLUMN family TEXT NOT NULL DEFAULT 'main'",
            [],
        );
        conn.execute_batch(
            "DROP INDEX IF EXISTS idx_object_digest;
             CREATE UNIQUE INDEX IF NOT EXISTS idx_object_digest_family
                 ON object_index(family, digest) WHERE digest IS NOT NULL;",
        )?;
        Ok(Self { conn })
    }

    pub fn load_config(&self) -> GovernorResult<Option<StorageGovernorConfig>> {
        let raw: Option<String> = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = 'config'", [], |row| {
                row.get(0)
            })
            .optional()?;
        match raw {
            Some(raw) => serde_json::from_str(&raw)
                .map(Some)
                .map_err(|e| GovernorError::Corrupt(format!("stored governor config: {e}"))),
            None => Ok(None),
        }
    }

    pub fn store_config(&self, config: &StorageGovernorConfig) -> GovernorResult<()> {
        let raw = serde_json::to_string(config)
            .map_err(|e| GovernorError::Corrupt(format!("governor config encode: {e}")))?;
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES ('config', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![raw],
        )?;
        Ok(())
    }

    pub fn meta(&self, key: &str) -> GovernorResult<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> GovernorResult<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn usage(&self) -> GovernorResult<BTreeMap<StorageClass, UsageRow>> {
        let mut stmt = self
            .conn
            .prepare("SELECT class, bytes, objects, pinned_bytes FROM usage")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                UsageRow {
                    bytes: from_i64(row.get(1)?),
                    objects: from_i64(row.get(2)?),
                    pinned_bytes: from_i64(row.get(3)?),
                },
            ))
        })?;
        let mut out = BTreeMap::new();
        for row in rows {
            let (class, usage) = row?;
            if let Some(class) = StorageClass::from_label(&class) {
                out.insert(class, usage);
            }
        }
        Ok(out)
    }

    pub fn pending_by_class(&self) -> GovernorResult<BTreeMap<StorageClass, u64>> {
        let mut stmt = self.conn.prepare(
            "SELECT class, COALESCE(SUM(bytes), 0) FROM reservation
             WHERE lease_expires_ms >= ?1 GROUP BY class",
        )?;
        let rows = stmt.query_map(params![now_ms()], |row| {
            Ok((row.get::<_, String>(0)?, from_i64(row.get(1)?)))
        })?;
        let mut out = BTreeMap::new();
        for row in rows {
            let (class, bytes) = row?;
            if let Some(class) = StorageClass::from_label(&class) {
                out.insert(class, bytes);
            }
        }
        Ok(out)
    }

    pub fn pending_count(&self) -> GovernorResult<u64> {
        Ok(from_i64(self.conn.query_row(
            "SELECT COUNT(*) FROM reservation WHERE lease_expires_ms >= ?1",
            params![now_ms()],
            |row| row.get(0),
        )?))
    }

    pub fn insert_reservation(
        &mut self,
        id: &str,
        class: StorageClass,
        key: &str,
        bytes: u64,
        now_ms: i64,
        expires_ms: i64,
    ) -> GovernorResult<()> {
        self.conn.execute(
            "INSERT INTO reservation(id, class, key, bytes, created_ms, lease_expires_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, class.as_str(), key, to_i64(bytes), now_ms, expires_ms],
        )?;
        Ok(())
    }

    pub fn reservation(&self, id: &str) -> GovernorResult<Option<ReservationRow>> {
        let row: Option<(String, String, u64)> = self
            .conn
            .query_row(
                "SELECT class, key, bytes FROM reservation WHERE id = ?1",
                params![id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        from_i64(row.get(2)?),
                    ))
                },
            )
            .optional()?;
        Ok(row.and_then(|(class, key, bytes)| {
            StorageClass::from_label(&class).map(|class| ReservationRow {
                id: id.to_string(),
                class,
                key,
                bytes,
            })
        }))
    }

    pub fn delete_reservation(&mut self, id: &str) -> GovernorResult<()> {
        self.conn
            .execute("DELETE FROM reservation WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn expire_reservations(&mut self, now_ms: i64) -> GovernorResult<u64> {
        let removed = self.conn.execute(
            "DELETE FROM reservation WHERE lease_expires_ms < ?1",
            params![now_ms],
        )?;
        Ok(removed as u64)
    }

    /// Consume the reservation and index the object that was published.
    /// Returns the class/key/bytes actually committed.
    pub fn commit_reservation(
        &mut self,
        id: &str,
        actual_bytes: u64,
        digest: Option<&str>,
        last_used_ms: i64,
    ) -> GovernorResult<Option<ReservationRow>> {
        let Some(row) = self.reservation(id)? else {
            return Ok(None);
        };
        self.delete_reservation(id)?;
        let key = digest.unwrap_or(&row.key).to_string();
        self.upsert_object(row.class, &key, actual_bytes, digest, last_used_ms, false)?;
        Ok(Some(ReservationRow {
            bytes: actual_bytes,
            ..row
        }))
    }

    pub fn is_pinned(&self, class: StorageClass, key: &str) -> GovernorResult<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM pin WHERE class = ?1 AND key = ?2",
                params![class.as_str(), key],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn pin(&mut self, class: StorageClass, key: &str, reason: &str) -> GovernorResult<()> {
        let changed = self.conn.execute(
            "INSERT INTO pin(class, key, reason, created_ms) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(class, key) DO UPDATE SET reason = excluded.reason",
            params![class.as_str(), key, reason, now_ms()],
        )?;
        if changed > 0 {
            let bytes: Option<i64> = self
                .conn
                .query_row(
                    "SELECT bytes FROM object_index WHERE class = ?1 AND key = ?2",
                    params![class.as_str(), key],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;
            if let Some(bytes) = bytes.map(from_i64) {
                let already: bool = self
                    .conn
                    .query_row(
                        "SELECT required FROM object_index WHERE class = ?1 AND key = ?2",
                        params![class.as_str(), key],
                        |row| row.get::<_, i64>(0),
                    )
                    .map(|v| v != 0)
                    .unwrap_or(false);
                if !already {
                    self.conn.execute(
                        "UPDATE object_index SET required = 1 WHERE class = ?1 AND key = ?2",
                        params![class.as_str(), key],
                    )?;
                    self.add_pinned(class, bytes)?;
                }
            }
        }
        Ok(())
    }

    pub fn unpin(&mut self, class: StorageClass, key: &str) -> GovernorResult<()> {
        let changed = self.conn.execute(
            "DELETE FROM pin WHERE class = ?1 AND key = ?2",
            params![class.as_str(), key],
        )?;
        if changed > 0 {
            let bytes: Option<i64> = self
                .conn
                .query_row(
                    "SELECT bytes FROM object_index WHERE class = ?1 AND key = ?2 AND required = 1",
                    params![class.as_str(), key],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;
            if let Some(bytes) = bytes.map(from_i64) {
                self.conn.execute(
                    "UPDATE object_index SET required = 0 WHERE class = ?1 AND key = ?2",
                    params![class.as_str(), key],
                )?;
                self.sub_pinned(class, bytes)?;
            }
        }
        Ok(())
    }

    fn add_pinned(&mut self, class: StorageClass, bytes: u64) -> GovernorResult<()> {
        self.ensure_usage(class)?;
        self.conn.execute(
            "UPDATE usage SET pinned_bytes = pinned_bytes + ?2, updated_ms = ?3 WHERE class = ?1",
            params![class.as_str(), to_i64(bytes), now_ms()],
        )?;
        Ok(())
    }

    fn sub_pinned(&mut self, class: StorageClass, bytes: u64) -> GovernorResult<()> {
        self.conn.execute(
            "UPDATE usage SET pinned_bytes = MAX(pinned_bytes - ?2, 0), updated_ms = ?3 WHERE class = ?1",
            params![class.as_str(), to_i64(bytes), now_ms()],
        )?;
        Ok(())
    }

    fn ensure_usage(&mut self, class: StorageClass) -> GovernorResult<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO usage(class, bytes, objects, pinned_bytes, updated_ms)
             VALUES (?1, 0, 0, 0, ?2)",
            params![class.as_str(), now_ms()],
        )?;
        Ok(())
    }

    /// Insert or update one object. A digest already indexed under ANOTHER
    /// class is the same physical bytes: the existing row wins and the new
    /// class must not double-count it. Returns true when a row was inserted
    /// or changed.
    pub fn upsert_object(
        &mut self,
        class: StorageClass,
        key: &str,
        bytes: u64,
        digest: Option<&str>,
        last_used_ms: i64,
        required: bool,
    ) -> GovernorResult<bool> {
        let family = class.family();
        if let Some(digest) = digest {
            let owner: Option<(String, String)> = self
                .conn
                .query_row(
                    "SELECT class, key FROM object_index WHERE family = ?1 AND digest = ?2",
                    params![family, digest],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((owner_class, owner_key)) = owner {
                if !(owner_class == class.as_str() && owner_key == key) {
                    return Ok(false);
                }
            }
        }
        let required = required || self.is_pinned(class, key)?;
        let existing: Option<(u64, bool)> = self
            .conn
            .query_row(
                "SELECT bytes, required FROM object_index WHERE class = ?1 AND key = ?2",
                params![class.as_str(), key],
                |row| Ok((from_i64(row.get(0)?), row.get::<_, i64>(1)? != 0)),
            )
            .optional()?;
        self.ensure_usage(class)?;
        match existing {
            Some((old_bytes, old_required)) => {
                self.conn.execute(
                    "UPDATE object_index SET bytes = ?3, digest = COALESCE(?4, digest),
                        last_used_ms = ?5, required = ?6
                     WHERE class = ?1 AND key = ?2",
                    params![
                        class.as_str(),
                        key,
                        to_i64(bytes),
                        digest,
                        last_used_ms,
                        if required { 1i64 } else { 0i64 }
                    ],
                )?;
                let delta = to_i64(bytes).saturating_sub(to_i64(old_bytes));
                self.conn.execute(
                    "UPDATE usage SET bytes = bytes + ?2, updated_ms = ?3 WHERE class = ?1",
                    params![class.as_str(), delta, now_ms()],
                )?;
                if required != old_required {
                    if required {
                        self.add_pinned(class, bytes)?;
                    } else {
                        self.sub_pinned(class, old_bytes)?;
                    }
                }
            }
            None => {
                self.conn.execute(
                    "INSERT INTO object_index(class, key, bytes, digest, family, created_ms, last_used_ms, required)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7)",
                    params![
                        class.as_str(),
                        key,
                        to_i64(bytes),
                        digest,
                        class.family(),
                        last_used_ms,
                        if required { 1i64 } else { 0i64 }
                    ],
                )?;
                self.conn.execute(
                    "UPDATE usage SET bytes = bytes + ?2, objects = objects + 1, updated_ms = ?3
                     WHERE class = ?1",
                    params![class.as_str(), to_i64(bytes), now_ms()],
                )?;
                if required {
                    self.add_pinned(class, bytes)?;
                }
            }
        }
        Ok(true)
    }

    pub fn remove_object(
        &mut self,
        class: StorageClass,
        key: &str,
    ) -> GovernorResult<Option<ObjectRow>> {
        let row: Option<(u64, Option<String>, i64, bool)> = self
            .conn
            .query_row(
                "SELECT bytes, digest, last_used_ms, required FROM object_index
                 WHERE class = ?1 AND key = ?2",
                params![class.as_str(), key],
                |row| {
                    Ok((
                        from_i64(row.get(0)?),
                        row.get(1)?,
                        row.get(2)?,
                        row.get::<_, i64>(3)? != 0,
                    ))
                },
            )
            .optional()?;
        let Some((bytes, digest, last_used_ms, required)) = row else {
            return Ok(None);
        };
        self.conn.execute(
            "DELETE FROM object_index WHERE class = ?1 AND key = ?2",
            params![class.as_str(), key],
        )?;
        self.conn.execute(
            "UPDATE usage SET bytes = MAX(bytes - ?2, 0), objects = MAX(objects - 1, 0),
                pinned_bytes = MAX(pinned_bytes - ?3, 0), updated_ms = ?4
             WHERE class = ?1",
            params![
                class.as_str(),
                to_i64(bytes),
                if required { to_i64(bytes) } else { 0 },
                now_ms()
            ],
        )?;
        Ok(Some(ObjectRow {
            class,
            key: key.to_string(),
            bytes,
            digest,
            last_used_ms,
            required,
        }))
    }

    pub fn object_row(&self, class: StorageClass, key: &str) -> GovernorResult<Option<ObjectRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT bytes, digest, last_used_ms, required FROM object_index
                 WHERE class = ?1 AND key = ?2",
                params![class.as_str(), key],
                |row| {
                    Ok(ObjectRow {
                        class,
                        key: key.to_string(),
                        bytes: from_i64(row.get(0)?),
                        digest: row.get(1)?,
                        last_used_ms: row.get(2)?,
                        required: row.get::<_, i64>(3)? != 0,
                    })
                },
            )
            .optional()?)
    }

    pub fn object_class_of_digest(&self, digest: &str) -> GovernorResult<Option<StorageClass>> {
        self.object_class_of_digest_in_family(digest, "main")
    }

    pub fn object_class_of_digest_in_family(
        &self,
        digest: &str,
        family: &str,
    ) -> GovernorResult<Option<StorageClass>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT class FROM object_index WHERE family = ?1 AND digest = ?2",
                params![family, digest],
                |row| row.get(0),
            )
            .optional()?;
        Ok(raw.and_then(|raw| StorageClass::from_label(&raw)))
    }

    pub fn object_last_used(&self, class: StorageClass, key: &str) -> GovernorResult<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT last_used_ms FROM object_index WHERE class = ?1 AND key = ?2",
                params![class.as_str(), key],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn touch(&mut self, class: StorageClass, key: &str, now_ms: i64) -> GovernorResult<()> {
        self.conn.execute(
            "UPDATE object_index SET last_used_ms = ?3 WHERE class = ?1 AND key = ?2",
            params![class.as_str(), key, now_ms],
        )?;
        Ok(())
    }

    /// One bounded keyset page of eviction candidates (oldest first). The
    /// index makes each page O(log n + limit); no full sort per eviction.
    pub fn candidates_page(
        &self,
        class: StorageClass,
        after: (i64, &str),
        limit: usize,
    ) -> GovernorResult<Vec<ObjectRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT key, bytes, digest, last_used_ms, required FROM object_index
             WHERE class = ?1 AND required = 0
               AND (last_used_ms > ?2 OR (last_used_ms = ?2 AND key > ?3))
             ORDER BY last_used_ms, key LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            params![class.as_str(), after.0, after.1, limit as i64],
            |row| {
                Ok(ObjectRow {
                    class,
                    key: row.get(0)?,
                    bytes: from_i64(row.get(1)?),
                    digest: row.get(2)?,
                    last_used_ms: row.get(3)?,
                    required: row.get::<_, i64>(4)? != 0,
                })
            },
        )?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Global oldest-first page (used when the aggregate budget, not a class
    /// quota, is what is exhausted).
    pub fn global_candidates_page(
        &self,
        after: (i64, &str, &str),
        limit: usize,
    ) -> GovernorResult<Vec<ObjectRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT class, key, bytes, digest, last_used_ms, required FROM object_index
             WHERE required = 0
               AND (last_used_ms > ?1 OR (last_used_ms = ?1 AND (class > ?2 OR (class = ?2 AND key > ?3))))
             ORDER BY last_used_ms, class, key LIMIT ?4",
        )?;
        let rows = stmt.query_map(params![after.0, after.1, after.2, limit as i64], |row| {
            let class: String = row.get(0)?;
            Ok((
                class,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (class, key, bytes, digest, last_used_ms, required) = row?;
            if let Some(class) = StorageClass::from_label(&class) {
                out.push(ObjectRow {
                    class,
                    key,
                    bytes: from_i64(bytes),
                    digest,
                    last_used_ms,
                    required: required != 0,
                });
            }
        }
        Ok(out)
    }

    pub fn record_eviction(
        &mut self,
        class: StorageClass,
        key: &str,
        digest: Option<&str>,
        bytes: u64,
        verified: bool,
        reason: &str,
    ) -> GovernorResult<()> {
        self.conn.execute(
            "INSERT INTO eviction(class, key, digest, bytes, verified, reason, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                class.as_str(),
                key,
                digest,
                to_i64(bytes),
                if verified { 1i64 } else { 0i64 },
                reason,
                now_ms()
            ],
        )?;
        self.bump_counter("evictions")
    }

    pub fn record_loss_marker(
        &mut self,
        class: StorageClass,
        key: &str,
        digest: Option<&str>,
        bytes: u64,
        reason: &str,
    ) -> GovernorResult<()> {
        let exists: bool = self
            .conn
            .query_row(
                "SELECT 1 FROM loss_marker WHERE class = ?1 AND key = ?2 AND reason = ?3 LIMIT 1",
                params![class.as_str(), key, reason],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if exists {
            return Ok(());
        }
        self.conn.execute(
            "INSERT INTO loss_marker(class, key, digest, bytes, reason, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![class.as_str(), key, digest, to_i64(bytes), reason, now_ms()],
        )?;
        self.bump_counter("loss_markers")
    }

    pub fn record_refusal(
        &mut self,
        class: StorageClass,
        requested: u64,
        available: u64,
        reason: &str,
        detail: &str,
    ) -> GovernorResult<()> {
        self.conn.execute(
            "INSERT INTO refusal(class, requested, available, reason, detail, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                class.as_str(),
                to_i64(requested),
                to_i64(available),
                reason,
                detail,
                now_ms()
            ],
        )?;
        self.bump_counter("refusals")
    }

    pub fn counters(&self) -> GovernorResult<Counters> {
        Ok(Counters {
            evictions: self.counter("evictions")?,
            refusals: self.counter("refusals")?,
            loss_markers: self.counter("loss_markers")?,
        })
    }

    fn counter(&self, key: &str) -> GovernorResult<u64> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(raw.and_then(|raw| raw.parse::<u64>().ok()).unwrap_or(0))
    }

    fn bump_counter(&mut self, key: &str) -> GovernorResult<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES (?1, '1')
             ON CONFLICT(key) DO UPDATE SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)",
            params![key],
        )?;
        Ok(())
    }

    pub fn loss_markers(
        &self,
        class: Option<StorageClass>,
    ) -> GovernorResult<Vec<(String, u64, String)>> {
        let (sql, filter) = match class {
            Some(class) => (
                "SELECT key, bytes, reason FROM loss_marker WHERE class = ?1 ORDER BY id DESC",
                Some(class.as_str()),
            ),
            None => (
                "SELECT key, bytes, reason FROM loss_marker ORDER BY id DESC",
                None,
            ),
        };
        let mut stmt = self.conn.prepare(sql)?;
        let mut out = Vec::new();
        if let Some(filter) = filter {
            let rows = stmt.query_map(params![filter], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    from_i64(row.get(1)?),
                    row.get::<_, String>(2)?,
                ))
            })?;
            for row in rows {
                out.push(row?);
            }
        } else {
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    from_i64(row.get(1)?),
                    row.get::<_, String>(2)?,
                ))
            })?;
            for row in rows {
                out.push(row?);
            }
        }
        Ok(out)
    }

    /// The durable eviction index: every evicted object with its verified
    /// flag (CAS evictions verify the blob before deleting it).
    pub fn eviction_index(&self) -> GovernorResult<Vec<(String, String, u64, bool)>> {
        let mut stmt = self.conn.prepare(
            "SELECT class, key, bytes, verified FROM eviction ORDER BY id DESC LIMIT 65536",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                from_i64(row.get(2)?),
                row.get::<_, i64>(3)? != 0,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Replace the inventory of one class from a full scan. Missing archive
    /// objects get a durable loss marker; missing non-archive objects are
    /// removed from accounting without a marker.
    pub fn set_inventory(
        &mut self,
        class: StorageClass,
        objects: &[ObjectRow],
    ) -> GovernorResult<InventoryChange> {
        let tx = self.conn.transaction()?;
        let mut existing: BTreeMap<String, (u64, bool, Option<String>, String)> = BTreeMap::new();
        {
            let mut stmt = tx.prepare(
                "SELECT key, bytes, required, digest, family FROM object_index WHERE class = ?1",
            )?;
            let rows = stmt.query_map(params![class.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    from_i64(row.get(1)?),
                    row.get::<_, i64>(2)? != 0,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?;
            for row in rows {
                let (key, bytes, required, digest, family) = row?;
                existing.insert(key, (bytes, required, digest, family));
            }
        }
        let mut pins: BTreeSet<String> = BTreeSet::new();
        {
            let mut stmt = tx.prepare("SELECT key FROM pin WHERE class = ?1")?;
            let rows = stmt.query_map(params![class.as_str()], |row| row.get::<_, String>(0))?;
            for row in rows {
                pins.insert(row?);
            }
        }
        let mut seen = BTreeSet::new();
        let mut total_bytes = 0u64;
        let mut pinned_bytes = 0u64;
        let mut inserted = 0u64;
        let mut removed = 0u64;
        let mut missing_markers = Vec::new();
        let scanned_ms = now_ms();
        {
            let mut insert = tx.prepare_cached(
                "INSERT INTO object_index(class, key, bytes, digest, family, created_ms, last_used_ms, required)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(class, key) DO UPDATE SET bytes = excluded.bytes,
                    digest = COALESCE(excluded.digest, object_index.digest),
                    family = excluded.family,
                    last_used_ms = excluded.last_used_ms,
                    required = excluded.required",
            )?;
            for object in objects {
                seen.insert(object.key.clone());
                total_bytes = total_bytes.saturating_add(object.bytes);
                let required = object.required || pins.contains(&object.key);
                if required {
                    pinned_bytes = pinned_bytes.saturating_add(object.bytes);
                }
                let existing_row = existing.get(&object.key);
                match existing_row {
                    Some((bytes, was_required, digest, family))
                        if *bytes == object.bytes
                            && *was_required == required
                            && *digest == object.digest
                            && family == class.family() => {}
                    _ => {
                        insert.execute(params![
                            class.as_str(),
                            object.key,
                            to_i64(object.bytes),
                            object.digest,
                            class.family(),
                            scanned_ms,
                            object.last_used_ms,
                            if required { 1i64 } else { 0i64 }
                        ])?;
                        inserted += 1;
                    }
                }
            }
        }
        for (key, (bytes, _required, digest, _family)) in &existing {
            if seen.contains(key) {
                continue;
            }
            tx.execute(
                "DELETE FROM object_index WHERE class = ?1 AND key = ?2",
                params![class.as_str(), key],
            )?;
            removed += 1;
            if class.requires_loss_marker() {
                missing_markers.push((key.clone(), *bytes, digest.clone()));
            }
        }
        for (key, bytes, digest) in &missing_markers {
            let exists: bool = tx
                .query_row(
                    "SELECT 1 FROM loss_marker WHERE class = ?1 AND key = ?2 AND reason = 'missing_on_reconcile' LIMIT 1",
                    params![class.as_str(), key],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !exists {
                tx.execute(
                    "INSERT INTO loss_marker(class, key, digest, bytes, reason, created_ms)
                     VALUES (?1, ?2, ?3, ?4, 'missing_on_reconcile', ?5)",
                    params![class.as_str(), key, digest, to_i64(*bytes), now_ms()],
                )?;
                tx.execute(
                    "INSERT INTO meta(key, value) VALUES ('loss_markers', '1')
                     ON CONFLICT(key) DO UPDATE SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)",
                    [],
                )?;
            }
        }
        tx.execute(
            "INSERT INTO usage(class, bytes, objects, pinned_bytes, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(class) DO UPDATE SET bytes = excluded.bytes, objects = excluded.objects,
                pinned_bytes = excluded.pinned_bytes, updated_ms = excluded.updated_ms",
            params![
                class.as_str(),
                to_i64(total_bytes),
                objects.len() as i64,
                to_i64(pinned_bytes),
                now_ms()
            ],
        )?;
        tx.commit()?;
        Ok(InventoryChange {
            scanned: objects.len() as u64,
            inserted,
            removed,
            missing_markers: missing_markers.len() as u64,
        })
    }

    pub fn sum_usage(&self) -> GovernorResult<u64> {
        Ok(from_i64(self.conn.query_row(
            "SELECT COALESCE(SUM(bytes), 0) FROM usage",
            [],
            |row| row.get(0),
        )?))
    }

    pub fn sum_objects(&self) -> GovernorResult<u64> {
        Ok(from_i64(self.conn.query_row(
            "SELECT COALESCE(SUM(objects), 0) FROM usage",
            [],
            |row| row.get(0),
        )?))
    }

    pub fn sum_pinned(&self) -> GovernorResult<u64> {
        Ok(from_i64(self.conn.query_row(
            "SELECT COALESCE(SUM(pinned_bytes), 0) FROM usage",
            [],
            |row| row.get(0),
        )?))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InventoryChange {
    pub scanned: u64,
    pub inserted: u64,
    pub removed: u64,
    pub missing_markers: u64,
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
