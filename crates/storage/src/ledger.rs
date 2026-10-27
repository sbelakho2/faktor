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
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

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
    owner TEXT NOT NULL DEFAULT '',
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
CREATE INDEX IF NOT EXISTS idx_object_global_lru_class
    ON object_index(required, last_used_ms, class, key);
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

/// One consistent budget view: durable usage plus in-flight reservations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BudgetSnapshot {
    pub class_bytes: u64,
    pub class_pinned: u64,
    pub class_pending: u64,
    pub total_bytes: u64,
    pub total_pinned: u64,
    pub total_pending: u64,
}

/// The result of one atomic admission attempt. A reservation id is present
/// only when the class quota AND the aggregate budget could hold the bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmitRow {
    pub reservation_id: Option<String>,
    pub snapshot: BudgetSnapshot,
    pub class_used: u64,
    pub total_used: u64,
    pub class_deficit: u64,
    pub total_deficit: u64,
}

pub struct Ledger {
    conn: Connection,
}

/// One id per process: reservations created by an earlier process are stale
/// immediately after a restart, while reservations shared by several governor
/// instances inside THIS process are not. (Doctor's read-only open never
/// sweeps.)
pub fn process_boot_id() -> &'static str {
    static BOOT: OnceLock<String> = OnceLock::new();
    BOOT.get_or_init(|| format!("{}-{}", std::process::id(), uuid::Uuid::new_v4().simple()))
}

fn budget_snapshot_on(
    conn: &Connection,
    class: StorageClass,
    now_ms: i64,
) -> rusqlite::Result<BudgetSnapshot> {
    conn.query_row(
        "SELECT
            COALESCE((SELECT bytes FROM usage WHERE class = ?1), 0),
            COALESCE((SELECT pinned_bytes FROM usage WHERE class = ?1), 0),
            COALESCE((SELECT SUM(bytes) FROM reservation
                      WHERE class = ?1 AND lease_expires_ms >= ?2), 0),
            COALESCE((SELECT SUM(bytes) FROM usage), 0),
            COALESCE((SELECT SUM(pinned_bytes) FROM usage), 0),
            COALESCE((SELECT SUM(bytes) FROM reservation
                      WHERE lease_expires_ms >= ?2), 0)",
        params![class.as_str(), now_ms],
        |row| {
            Ok(BudgetSnapshot {
                class_bytes: from_i64(row.get(0)?),
                class_pinned: from_i64(row.get(1)?),
                class_pending: from_i64(row.get(2)?),
                total_bytes: from_i64(row.get(3)?),
                total_pinned: from_i64(row.get(4)?),
                total_pending: from_i64(row.get(5)?),
            })
        },
    )
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
        let _ = conn.execute(
            "ALTER TABLE reservation ADD COLUMN owner TEXT NOT NULL DEFAULT ''",
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

    /// Persisted directory roots of one class. Roots are a property of the
    /// CLASS, not of one in-process governor instance: a browser download
    /// manager, the daemon anddoctor all open the same ledger but register
    /// different concrete paths, so every instance must see the union or a
    /// reconcile from one would erase inventory indexed by another.
    pub fn directory_roots(
        &self,
        class: StorageClass,
    ) -> GovernorResult<Option<DirectoryRootsMeta>> {
        let raw = self.meta(&format!("roots:{}", class.as_str()))?;
        match raw {
            Some(raw) if !raw.is_empty() => serde_json::from_str(&raw)
                .map(Some)
                .map_err(|e| GovernorError::Corrupt(format!("stored directory roots: {e}"))),
            _ => Ok(None),
        }
    }

    pub fn store_directory_roots(
        &self,
        class: StorageClass,
        roots: &DirectoryRootsMeta,
    ) -> GovernorResult<()> {
        let raw = serde_json::to_string(roots)
            .map_err(|e| GovernorError::Corrupt(format!("directory roots encode: {e}")))?;
        self.set_meta(&format!("roots:{}", class.as_str()), &raw)
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

    #[allow(clippy::too_many_arguments)]
    pub fn insert_reservation(
        &mut self,
        id: &str,
        class: StorageClass,
        key: &str,
        bytes: u64,
        owner: &str,
        now_ms: i64,
        expires_ms: i64,
    ) -> GovernorResult<()> {
        self.conn.execute(
            "INSERT INTO reservation(id, class, key, bytes, owner, created_ms, lease_expires_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id,
                class.as_str(),
                key,
                to_i64(bytes),
                owner,
                now_ms,
                expires_ms
            ],
        )?;
        Ok(())
    }

    pub fn budget_snapshot(
        &self,
        class: StorageClass,
        now_ms: i64,
    ) -> GovernorResult<BudgetSnapshot> {
        Ok(budget_snapshot_on(&self.conn, class, now_ms)?)
    }

    /// Atomic admission: expire stale reservations, read usage + in-flight
    /// reservations, compare against both limits and insert the reservation in
    /// ONE `BEGIN IMMEDIATE` transaction. The ledger mutex only serializes one
    /// process; this transaction is what makes the hard budget hold across
    /// concurrent governor instances, CAS roots and processes sharing one
    /// data directory.
    #[allow(clippy::too_many_arguments)]
    pub fn try_reserve_atomic(
        &mut self,
        class: StorageClass,
        key: &str,
        bytes: u64,
        now_ms: i64,
        expires_ms: i64,
        quota: u64,
        total: u64,
    ) -> GovernorResult<AdmitRow> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM reservation WHERE lease_expires_ms < ?1",
            params![now_ms],
        )?;
        let snapshot = budget_snapshot_on(&tx, class, now_ms)?;
        let class_used = snapshot.class_bytes.saturating_add(snapshot.class_pending);
        let total_used = snapshot.total_bytes.saturating_add(snapshot.total_pending);
        let class_deficit = class_used.saturating_add(bytes).saturating_sub(quota);
        let total_deficit = total_used.saturating_add(bytes).saturating_sub(total);
        let reservation_id = if class_deficit == 0 && total_deficit == 0 && bytes <= total {
            let id = uuid::Uuid::new_v4().simple().to_string();
            tx.execute(
                "INSERT INTO reservation(id, class, key, bytes, owner, created_ms, lease_expires_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    id,
                    class.as_str(),
                    key,
                    to_i64(bytes),
                    process_boot_id(),
                    now_ms,
                    expires_ms
                ],
            )?;
            Some(id)
        } else {
            None
        };
        tx.commit()?;
        Ok(AdmitRow {
            reservation_id,
            snapshot,
            class_used,
            total_used,
            class_deficit,
            total_deficit,
        })
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

    /// Delete reservations left by a DEAD process (a crash between reserve
    /// and publish). Same-process governor instances share the boot id; a
    /// different process whose pid is still alive keeps its reservations, so
    /// a second live daemon/run sharing the data dir is never robbed. Doctor
    /// opens existing state without calling this.
    pub fn expire_foreign_reservations(&mut self, owner: &str) -> GovernorResult<u64> {
        let owners: Vec<String> = {
            let mut stmt = self
                .conn
                .prepare("SELECT DISTINCT owner FROM reservation WHERE owner <> ?1")?;
            let rows = stmt.query_map(params![owner], |row| row.get::<_, String>(0))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            out
        };
        let mut removed = 0u64;
        for foreign in owners {
            if owner_is_alive(&foreign) {
                continue;
            }
            removed += self
                .conn
                .execute("DELETE FROM reservation WHERE owner = ?1", params![foreign])?
                as u64;
        }
        Ok(removed)
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
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String, String, u64)> = tx
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
        let Some((class_label, reserved_key, _reserved_bytes)) = row else {
            tx.commit()?;
            return Ok(None);
        };
        let Some(class) = StorageClass::from_label(&class_label) else {
            tx.commit()?;
            return Err(GovernorError::Corrupt(format!(
                "reservation {id} names unknown class {class_label:?}"
            )));
        };
        tx.execute("DELETE FROM reservation WHERE id = ?1", params![id])?;
        let key = digest.unwrap_or(&reserved_key).to_string();
        upsert_object_on(&tx, class, &key, actual_bytes, digest, last_used_ms, false)?;
        tx.commit()?;
        Ok(Some(ReservationRow {
            id: id.to_string(),
            class,
            key,
            bytes: actual_bytes,
        }))
    }

    pub fn is_pinned(&self, class: StorageClass, key: &str) -> GovernorResult<bool> {
        Ok(is_pinned_on(&self.conn, class, key)?)
    }

    pub fn pin(&mut self, class: StorageClass, key: &str, reason: &str) -> GovernorResult<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO pin(class, key, reason, created_ms) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(class, key) DO UPDATE SET reason = excluded.reason",
            params![class.as_str(), key, reason, now_ms()],
        )?;
        let bytes: Option<i64> = tx
            .query_row(
                "SELECT bytes FROM object_index WHERE class = ?1 AND key = ?2",
                params![class.as_str(), key],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        if let Some(bytes) = bytes.map(from_i64) {
            let already: bool = tx
                .query_row(
                    "SELECT required FROM object_index WHERE class = ?1 AND key = ?2",
                    params![class.as_str(), key],
                    |row| row.get::<_, i64>(0),
                )
                .map(|v| v != 0)
                .unwrap_or(false);
            if !already {
                tx.execute(
                    "UPDATE object_index SET required = 1 WHERE class = ?1 AND key = ?2",
                    params![class.as_str(), key],
                )?;
                add_pinned_on(&tx, class, bytes)?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn unpin(&mut self, class: StorageClass, key: &str) -> GovernorResult<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "DELETE FROM pin WHERE class = ?1 AND key = ?2",
            params![class.as_str(), key],
        )?;
        if changed > 0 {
            let bytes: Option<i64> = tx
                .query_row(
                    "SELECT bytes FROM object_index WHERE class = ?1 AND key = ?2 AND required = 1",
                    params![class.as_str(), key],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;
            if let Some(bytes) = bytes.map(from_i64) {
                tx.execute(
                    "UPDATE object_index SET required = 0 WHERE class = ?1 AND key = ?2",
                    params![class.as_str(), key],
                )?;
                sub_pinned_on(&tx, class, bytes)?;
            }
        }
        tx.commit()?;
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
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let inserted = upsert_object_on(&tx, class, key, bytes, digest, last_used_ms, required)?;
        tx.commit()?;
        Ok(inserted)
    }

    pub fn remove_object(
        &mut self,
        class: StorageClass,
        key: &str,
    ) -> GovernorResult<Option<ObjectRow>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row = remove_object_on(&tx, class, key)?;
        tx.commit()?;
        Ok(row)
    }

    /// Remove the object and record its eviction (and, for archive classes,
    /// its loss marker) in ONE transaction. A crash can therefore never leave
    /// "object gone, eviction evidence lost" or "object gone, archive loss
    /// marker lost".
    #[allow(clippy::too_many_arguments)]
    pub fn remove_object_and_record_eviction(
        &mut self,
        class: StorageClass,
        key: &str,
        digest: Option<&str>,
        freed_bytes: u64,
        verified: bool,
        reason: &str,
    ) -> GovernorResult<Option<ObjectRow>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(row) = remove_object_on(&tx, class, key)? else {
            tx.commit()?;
            return Ok(None);
        };
        tx.execute(
            "INSERT INTO eviction(class, key, digest, bytes, verified, reason, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                class.as_str(),
                key,
                digest.or(row.digest.as_deref()),
                to_i64(freed_bytes),
                if verified { 1i64 } else { 0i64 },
                reason,
                now_ms()
            ],
        )?;
        bump_counter_on(&tx, "evictions")?;
        if class.requires_loss_marker() {
            record_loss_marker_on(
                &tx,
                class,
                key,
                digest.or(row.digest.as_deref()),
                freed_bytes,
                "archive_evicted",
            )?;
        }
        tx.commit()?;
        Ok(Some(row))
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

    /// One bounded load of the whole digest index of a physical root family.
    /// CAS reconciliation classifies every stored blob in memory instead of
    /// issuing two point queries per blob (the old N+1 scan cost).
    pub fn family_index(
        &self,
        family: &str,
    ) -> GovernorResult<BTreeMap<String, (StorageClass, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT digest, class, last_used_ms FROM object_index
             WHERE family = ?1 AND digest IS NOT NULL",
        )?;
        let rows = stmt.query_map(params![family], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        let mut out = BTreeMap::new();
        for row in rows {
            let (digest, class, last_used_ms) = row?;
            if let Some(class) = StorageClass::from_label(&class) {
                out.insert(digest, (class, last_used_ms));
            }
        }
        Ok(out)
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
        let limit = i64::try_from(limit)
            .map_err(|_| GovernorError::Malformed("eviction page limit exceeds i64::MAX".into()))?;
        let rows = stmt.query_map(params![class.as_str(), after.0, after.1, limit], |row| {
            Ok(ObjectRow {
                class,
                key: row.get(0)?,
                bytes: from_i64(row.get(1)?),
                digest: row.get(2)?,
                last_used_ms: row.get(3)?,
                required: row.get::<_, i64>(4)? != 0,
            })
        })?;
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
        let limit = i64::try_from(limit)
            .map_err(|_| GovernorError::Malformed("eviction page limit exceeds i64::MAX".into()))?;
        let rows = stmt.query_map(params![after.0, after.1, after.2, limit], |row| {
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
        record_loss_marker_on(&self.conn, class, key, digest, bytes, reason)?;
        Ok(())
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
                "SELECT key, bytes, reason FROM loss_marker WHERE class = ?1 ORDER BY id DESC LIMIT 65536",
                Some(class.as_str()),
            ),
            None => (
                "SELECT key, bytes, reason FROM loss_marker ORDER BY id DESC LIMIT 65536",
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

    /// Replace the inventory of one class from a full scan that STARTED at
    /// `scan_started_ms`. Rows committed after that instant are NEWER than
    /// the snapshot and are preserved verbatim (with their bytes) instead of
    /// being deleted or overwritten by stale scan data. Missing archive
    /// objects get a durable loss marker; missing non-archive objects are
    /// removed from accounting without a marker.
    pub fn set_inventory_scanned_at(
        &mut self,
        class: StorageClass,
        objects: &[ObjectRow],
        scan_started_ms: i64,
    ) -> GovernorResult<InventoryChange> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut existing: BTreeMap<String, ExistingRow> = BTreeMap::new();
        {
            let mut stmt = tx.prepare(
                "SELECT key, bytes, required, digest, family, last_used_ms
                 FROM object_index WHERE class = ?1",
            )?;
            let rows = stmt.query_map(params![class.as_str()], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    ExistingRow {
                        bytes: from_i64(row.get(1)?),
                        required: row.get::<_, i64>(2)? != 0,
                        digest: row.get(3)?,
                        family: row.get(4)?,
                        last_used_ms: row.get(5)?,
                    },
                ))
            })?;
            for row in rows {
                let (key, row) = row?;
                existing.insert(key, row);
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
        let mut effective_objects = 0u64;
        let mut inserted = 0u64;
        let mut removed = 0u64;
        let mut missing_markers = Vec::new();
        let mut inserted_markers = 0u64;
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
            let mut mark_required = tx.prepare_cached(
                "UPDATE object_index SET required = ?3 WHERE class = ?1 AND key = ?2",
            )?;
            for object in objects {
                if !seen.insert(object.key.clone()) {
                    // A duplicated scan row (overlapping roots) must never
                    // double count the same physical object.
                    continue;
                }
                effective_objects += 1;
                let required = object.required || pins.contains(&object.key);
                match existing.get(&object.key) {
                    Some(row) if row.last_used_ms > scan_started_ms => {
                        // A writer touched/committed this object after the
                        // scan snapshot: the ledger row is newer truth.
                        let effective_required = row.required || pins.contains(&object.key);
                        total_bytes = total_bytes.saturating_add(row.bytes);
                        if effective_required {
                            pinned_bytes = pinned_bytes.saturating_add(row.bytes);
                        }
                        if effective_required != row.required {
                            mark_required.execute(params![
                                class.as_str(),
                                object.key,
                                if effective_required { 1i64 } else { 0i64 }
                            ])?;
                        }
                    }
                    Some(row)
                        if row.bytes == object.bytes
                            && row.required == required
                            && row.digest == object.digest
                            && row.family == class.family() =>
                    {
                        total_bytes = total_bytes.saturating_add(object.bytes);
                        if required {
                            pinned_bytes = pinned_bytes.saturating_add(object.bytes);
                        }
                    }
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
                        total_bytes = total_bytes.saturating_add(object.bytes);
                        if required {
                            pinned_bytes = pinned_bytes.saturating_add(object.bytes);
                        }
                    }
                }
            }
        }
        for (key, row) in &existing {
            if seen.contains(key) {
                continue;
            }
            if row.last_used_ms > scan_started_ms {
                // Published after this scan began: not evidence of a
                // vanished object. Keep it counted.
                let effective_required = row.required || pins.contains(key);
                total_bytes = total_bytes.saturating_add(row.bytes);
                if effective_required {
                    pinned_bytes = pinned_bytes.saturating_add(row.bytes);
                }
                if effective_required != row.required {
                    tx.execute(
                        "UPDATE object_index SET required = ?3 WHERE class = ?1 AND key = ?2",
                        params![
                            class.as_str(),
                            key,
                            if effective_required { 1i64 } else { 0i64 }
                        ],
                    )?;
                }
                effective_objects += 1;
                continue;
            }
            tx.execute(
                "DELETE FROM object_index WHERE class = ?1 AND key = ?2",
                params![class.as_str(), key],
            )?;
            removed += 1;
            if class.requires_loss_marker() {
                missing_markers.push((key.clone(), row.bytes, row.digest.clone()));
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
                inserted_markers += 1;
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
                effective_objects as i64,
                to_i64(pinned_bytes),
                now_ms()
            ],
        )?;
        tx.commit()?;
        Ok(InventoryChange {
            scanned: objects.len() as u64,
            inserted,
            removed,
            missing_markers: inserted_markers,
        })
    }

    /// Replace inventory from a scan that is current at call time (test and
    /// recovery seam). Production reconciliation passes its scan start.
    pub fn set_inventory(
        &mut self,
        class: StorageClass,
        objects: &[ObjectRow],
    ) -> GovernorResult<InventoryChange> {
        self.set_inventory_scanned_at(class, objects, now_ms())
    }

    /// Update the bytes of one indexed object in place (a WAL truncation is
    /// a shrink, not a deletion: the remaining frames stay accounted).
    pub fn resize_object(
        &mut self,
        class: StorageClass,
        key: &str,
        new_bytes: u64,
    ) -> GovernorResult<Option<ObjectRow>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(u64, Option<String>, i64, bool)> = tx
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
        let Some((old_bytes, digest, last_used_ms, required)) = row else {
            tx.commit()?;
            return Ok(None);
        };
        tx.execute(
            "UPDATE object_index SET bytes = ?3 WHERE class = ?1 AND key = ?2",
            params![class.as_str(), key, to_i64(new_bytes)],
        )?;
        let delta = to_i64(new_bytes).saturating_sub(to_i64(old_bytes));
        tx.execute(
            "UPDATE usage SET bytes = MAX(bytes + ?2, 0), updated_ms = ?3 WHERE class = ?1",
            params![class.as_str(), delta, now_ms()],
        )?;
        if required {
            adjust_pinned_on(&tx, class, old_bytes, new_bytes)?;
        }
        tx.commit()?;
        Ok(Some(ObjectRow {
            class,
            key: key.to_string(),
            bytes: new_bytes,
            digest,
            last_used_ms,
            required,
        }))
    }

    /// Shrink one indexed object and record the reclaimed bytes in the same
    /// transaction. Used by WAL checkpoint truncation: the file still exists,
    /// so the row (and its `remaining_bytes`) must survive.
    #[allow(clippy::too_many_arguments)]
    pub fn resize_object_and_record_eviction(
        &mut self,
        class: StorageClass,
        key: &str,
        digest: Option<&str>,
        reclaimed_bytes: u64,
        remaining_bytes: u64,
        verified: bool,
        reason: &str,
    ) -> GovernorResult<Option<ObjectRow>> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(u64, Option<String>, i64, bool)> = tx
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
        let Some((old_bytes, stored_digest, last_used_ms, required)) = row else {
            tx.commit()?;
            return Ok(None);
        };
        tx.execute(
            "UPDATE object_index SET bytes = ?3 WHERE class = ?1 AND key = ?2",
            params![class.as_str(), key, to_i64(remaining_bytes)],
        )?;
        let delta = to_i64(remaining_bytes).saturating_sub(to_i64(old_bytes));
        tx.execute(
            "UPDATE usage SET bytes = MAX(bytes + ?2, 0), updated_ms = ?3 WHERE class = ?1",
            params![class.as_str(), delta, now_ms()],
        )?;
        if required {
            adjust_pinned_on(&tx, class, old_bytes, remaining_bytes)?;
        }
        tx.execute(
            "INSERT INTO eviction(class, key, digest, bytes, verified, reason, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                class.as_str(),
                key,
                digest.or(stored_digest.as_deref()),
                to_i64(reclaimed_bytes),
                if verified { 1i64 } else { 0i64 },
                reason,
                now_ms()
            ],
        )?;
        bump_counter_on(&tx, "evictions")?;
        tx.commit()?;
        Ok(Some(ObjectRow {
            class,
            key: key.to_string(),
            bytes: remaining_bytes,
            digest: stored_digest,
            last_used_ms,
            required,
        }))
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

struct ExistingRow {
    bytes: u64,
    required: bool,
    digest: Option<String>,
    family: String,
    last_used_ms: i64,
}

/// Durable directory-root registry for one governed class.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryRootsMeta {
    pub roots: Vec<PathBuf>,
    pub min_age_ms: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InventoryChange {
    pub scanned: u64,
    pub inserted: u64,
    pub removed: u64,
    pub missing_markers: u64,
}

/// A reservation owner is `"<pid>-<boot-uuid>"`. Liveness is checked only
/// (signal 0), so a stale row from a crashed process is reclaimed while a
/// live second process's in-flight reservations survive. Unknown/legacy
/// owners are treated as dead. On non-Unix, pid liveness is not probed and the
/// lease remains the bound.
fn owner_is_alive(owner: &str) -> bool {
    if owner.is_empty() {
        return false;
    }
    let Some(pid) = owner
        .split('-')
        .next()
        .and_then(|raw| raw.parse::<u32>().ok())
    else {
        return false;
    };
    #[cfg(unix)]
    {
        if pid == 0 || pid > i32::MAX as u32 {
            return false;
        }
        // Signal 0 performs only an existence/permission check. rustix is a
        // safe wrapper over `kill(pid, 0)`, so this crate keeps its no-unsafe
        // lint and never constructs a child process just to probe liveness.
        let Some(pid) = rustix::process::Pid::from_raw(pid as i32) else {
            return false;
        };
        rustix::process::test_kill_process(pid).is_ok()
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

fn is_pinned_on(conn: &Connection, class: StorageClass, key: &str) -> rusqlite::Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM pin WHERE class = ?1 AND key = ?2",
            params![class.as_str(), key],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn ensure_usage_on(conn: &Connection, class: StorageClass) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO usage(class, bytes, objects, pinned_bytes, updated_ms)
         VALUES (?1, 0, 0, 0, ?2)",
        params![class.as_str(), now_ms()],
    )?;
    Ok(())
}

fn add_pinned_on(conn: &Connection, class: StorageClass, bytes: u64) -> rusqlite::Result<()> {
    ensure_usage_on(conn, class)?;
    conn.execute(
        "UPDATE usage SET pinned_bytes = pinned_bytes + ?2, updated_ms = ?3 WHERE class = ?1",
        params![class.as_str(), to_i64(bytes), now_ms()],
    )?;
    Ok(())
}

fn sub_pinned_on(conn: &Connection, class: StorageClass, bytes: u64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE usage SET pinned_bytes = MAX(pinned_bytes - ?2, 0), updated_ms = ?3 WHERE class = ?1",
        params![class.as_str(), to_i64(bytes), now_ms()],
    )?;
    Ok(())
}

fn adjust_pinned_on(
    conn: &Connection,
    class: StorageClass,
    old_bytes: u64,
    new_bytes: u64,
) -> rusqlite::Result<()> {
    let delta = to_i64(new_bytes).saturating_sub(to_i64(old_bytes));
    conn.execute(
        "UPDATE usage SET pinned_bytes = MAX(pinned_bytes + ?2, 0), updated_ms = ?3
         WHERE class = ?1",
        params![class.as_str(), delta, now_ms()],
    )?;
    Ok(())
}

fn upsert_object_on(
    conn: &Connection,
    class: StorageClass,
    key: &str,
    bytes: u64,
    digest: Option<&str>,
    last_used_ms: i64,
    required: bool,
) -> rusqlite::Result<bool> {
    let family = class.family();
    if let Some(digest) = digest {
        let owner: Option<(String, String)> = conn
            .query_row(
                "SELECT class, key FROM object_index WHERE family = ?1 AND digest = ?2",
                params![family, digest],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((owner_class, owner_key)) = owner {
            if !(owner_class == class.as_str() && owner_key == key) {
                let owner = StorageClass::from_label(&owner_class);
                // Same physical blob, stronger logical owner: promote the row
                // so archive/checkpoint/capture retention (and archive loss
                // markers) survive a dedup hit against general CAS content.
                if key == digest
                    && owner.is_some_and(|owner| class.retention_rank() > owner.retention_rank())
                {
                    promote_object_on(conn, family, digest, class, last_used_ms, required)?;
                    return Ok(true);
                }
                return Ok(false);
            }
        }
    }
    let required = required || is_pinned_on(conn, class, key)?;
    let existing: Option<(u64, bool)> = conn
        .query_row(
            "SELECT bytes, required FROM object_index WHERE class = ?1 AND key = ?2",
            params![class.as_str(), key],
            |row| Ok((from_i64(row.get(0)?), row.get::<_, i64>(1)? != 0)),
        )
        .optional()?;
    ensure_usage_on(conn, class)?;
    match existing {
        Some((old_bytes, old_required)) => {
            conn.execute(
                "UPDATE object_index SET bytes = ?3, digest = COALESCE(?4, digest),
                    last_used_ms = ?5, required = ?6, family = ?7
                 WHERE class = ?1 AND key = ?2",
                params![
                    class.as_str(),
                    key,
                    to_i64(bytes),
                    digest,
                    last_used_ms,
                    if required { 1i64 } else { 0i64 },
                    family
                ],
            )?;
            let delta = to_i64(bytes).saturating_sub(to_i64(old_bytes));
            conn.execute(
                "UPDATE usage SET bytes = bytes + ?2, updated_ms = ?3 WHERE class = ?1",
                params![class.as_str(), delta, now_ms()],
            )?;
            if required != old_required {
                if required {
                    add_pinned_on(conn, class, bytes)?;
                } else {
                    sub_pinned_on(conn, class, old_bytes)?;
                }
            } else if required {
                // Required bytes changing size must move the pinned counter
                // with them, otherwise pinned accounting drifts and eviction
                // decisions become untruthful.
                adjust_pinned_on(conn, class, old_bytes, bytes)?;
            }
        }
        None => {
            conn.execute(
                "INSERT INTO object_index(class, key, bytes, digest, family, created_ms, last_used_ms, required)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7)",
                params![
                    class.as_str(),
                    key,
                    to_i64(bytes),
                    digest,
                    family,
                    last_used_ms,
                    if required { 1i64 } else { 0i64 }
                ],
            )?;
            conn.execute(
                "UPDATE usage SET bytes = bytes + ?2, objects = objects + 1, updated_ms = ?3
                 WHERE class = ?1",
                params![class.as_str(), to_i64(bytes), now_ms()],
            )?;
            if required {
                add_pinned_on(conn, class, bytes)?;
            }
        }
    }
    Ok(true)
}

fn promote_object_on(
    conn: &Connection,
    family: &str,
    digest: &str,
    to_class: StorageClass,
    last_used_ms: i64,
    required: bool,
) -> rusqlite::Result<()> {
    let row: Option<(String, String, u64, bool, i64)> = conn
        .query_row(
            "SELECT class, key, bytes, required, created_ms FROM object_index
             WHERE family = ?1 AND digest = ?2",
            params![family, digest],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    from_i64(row.get(2)?),
                    row.get::<_, i64>(3)? != 0,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((from_label, from_key, bytes, was_required, created_ms)) = row else {
        return Ok(());
    };
    let Some(from_class) = StorageClass::from_label(&from_label) else {
        return Ok(());
    };
    // A pin is a property of the OBJECT, not of the class that happened to
    // index it first. Moving the row without moving its pin would leave the
    // stronger owner permanently required (a stale `required=1` with the pin
    // under a class that no longer owns the key), i.e. pinned starvation.
    let old_pin: Option<(String, i64)> = conn
        .query_row(
            "SELECT reason, created_ms FROM pin WHERE class = ?1 AND key = ?2",
            params![from_label, from_key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let required =
        required || was_required || old_pin.is_some() || is_pinned_on(conn, to_class, digest)?;
    conn.execute(
        "DELETE FROM object_index WHERE class = ?1 AND key = ?2",
        params![from_label, from_key],
    )?;
    conn.execute(
        "UPDATE usage SET bytes = MAX(bytes - ?2, 0), objects = MAX(objects - 1, 0),
            pinned_bytes = MAX(pinned_bytes - ?3, 0), updated_ms = ?4
         WHERE class = ?1",
        params![
            from_label,
            to_i64(bytes),
            if was_required { to_i64(bytes) } else { 0 },
            now_ms()
        ],
    )?;
    ensure_usage_on(conn, to_class)?;
    conn.execute(
        "INSERT INTO object_index(class, key, bytes, digest, family, created_ms, last_used_ms, required)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            to_class.as_str(),
            digest,
            to_i64(bytes),
            digest,
            family,
            created_ms,
            last_used_ms.max(created_ms),
            if required { 1i64 } else { 0i64 }
        ],
    )?;
    if let Some((reason, pin_created_ms)) = old_pin {
        conn.execute(
            "INSERT INTO pin(class, key, reason, created_ms) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(class, key) DO UPDATE SET reason = excluded.reason",
            params![to_class.as_str(), digest, reason, pin_created_ms],
        )?;
        conn.execute(
            "DELETE FROM pin WHERE class = ?1 AND key = ?2",
            params![from_label, from_key],
        )?;
    }
    conn.execute(
        "UPDATE usage SET bytes = bytes + ?2, objects = objects + 1, updated_ms = ?3
         WHERE class = ?1",
        params![to_class.as_str(), to_i64(bytes), now_ms()],
    )?;
    if required {
        add_pinned_on(conn, to_class, bytes)?;
    }
    let _ = from_class;
    Ok(())
}

fn remove_object_on(
    conn: &Connection,
    class: StorageClass,
    key: &str,
) -> rusqlite::Result<Option<ObjectRow>> {
    let row: Option<(u64, Option<String>, i64, bool)> = conn
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
    conn.execute(
        "DELETE FROM object_index WHERE class = ?1 AND key = ?2",
        params![class.as_str(), key],
    )?;
    // A removed object cannot still carry a pin: leaving it would mark an
    // unrelated future object at the same address as required forever.
    conn.execute(
        "DELETE FROM pin WHERE class = ?1 AND key = ?2",
        params![class.as_str(), key],
    )?;
    conn.execute(
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

fn bump_counter_on(conn: &Connection, key: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES (?1, '1')
         ON CONFLICT(key) DO UPDATE SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT)",
        params![key],
    )?;
    Ok(())
}

fn record_loss_marker_on(
    conn: &Connection,
    class: StorageClass,
    key: &str,
    digest: Option<&str>,
    bytes: u64,
    reason: &str,
) -> rusqlite::Result<()> {
    let exists: bool = conn
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
    conn.execute(
        "INSERT INTO loss_marker(class, key, digest, bytes, reason, created_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![class.as_str(), key, digest, to_i64(bytes), reason, now_ms()],
    )?;
    bump_counter_on(conn, "loss_markers")?;
    Ok(())
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
