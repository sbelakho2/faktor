//! Durable update operations: every lifecycle transition is a row.
//!
//! The updater's crash safety rests on two facts: the install pointer is
//! swapped atomically (`current` names the old or the new artifact, never a
//! partial one), and every transition is recorded here BEFORE its side
//! effect. Recovery reads the `running` rows and the pointer and either
//! resumes the post-swap verification or rolls the pointer back — it never
//! re-runs a download blindly.
//!
//! Two implementations mirror the control-plane store seam: an in-memory
//! store (tests, ephemeral hosts) and a SQLite store (`update.db`, WAL,
//! `user_version` migration ladder).

use std::path::Path;
use std::sync::Mutex;

use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

/// Bound on one listing page.
pub const MAX_LIST: usize = 200;

/// Typed store failures.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UpdateStoreError {
    #[error("update store row is malformed: {0}")]
    Malformed(String),
    #[error("update store unavailable: {0}")]
    Backend(String),
}

/// One update operation id.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct UpdateOpId(String);

impl UpdateOpId {
    /// A fresh operation id for one lifecycle step.
    pub fn generate(kind: UpdateOpKind) -> Self {
        UpdateOpId(format!(
            "upd_{}_{}",
            kind.as_str(),
            uuid::Uuid::new_v4().simple()
        ))
    }

    pub fn try_new(raw: impl Into<String>) -> Result<Self, UpdateStoreError> {
        let raw = raw.into();
        if raw.is_empty() || raw.len() > 128 || !raw.is_ascii() {
            return Err(UpdateStoreError::Malformed(
                "update operation id must be 1..=128 ASCII bytes".into(),
            ));
        }
        Ok(UpdateOpId(raw))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for UpdateOpId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The four lifecycle steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateOpKind {
    Check,
    Stage,
    Apply,
    Rollback,
}

impl UpdateOpKind {
    pub const ALL: &'static [UpdateOpKind] = &[
        UpdateOpKind::Check,
        UpdateOpKind::Stage,
        UpdateOpKind::Apply,
        UpdateOpKind::Rollback,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            UpdateOpKind::Check => "check",
            UpdateOpKind::Stage => "stage",
            UpdateOpKind::Apply => "apply",
            UpdateOpKind::Rollback => "rollback",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|k| k.as_str() == raw)
    }
}

/// The durable state of one operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateOpStatus {
    /// In flight; a crash leaves this state and recovery must resolve it.
    Running,
    /// A check completed.
    Succeeded,
    /// An artifact is fully verified and published, awaiting `apply`.
    Staged,
    /// The pointer names this operation's target artifact and the probe
    /// passed.
    Applied,
    /// The pointer was restored to the previous artifact.
    RolledBack,
    /// The operation failed and left the install untouched.
    Failed,
    /// A crash interrupted an apply and the post-swap state could not be
    /// proven; verification is forced (never a silent success).
    Unverified,
}

impl UpdateOpStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            UpdateOpStatus::Running => "running",
            UpdateOpStatus::Succeeded => "succeeded",
            UpdateOpStatus::Staged => "staged",
            UpdateOpStatus::Applied => "applied",
            UpdateOpStatus::RolledBack => "rolled_back",
            UpdateOpStatus::Failed => "failed",
            UpdateOpStatus::Unverified => "unverified",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        [
            UpdateOpStatus::Running,
            UpdateOpStatus::Succeeded,
            UpdateOpStatus::Staged,
            UpdateOpStatus::Applied,
            UpdateOpStatus::RolledBack,
            UpdateOpStatus::Failed,
            UpdateOpStatus::Unverified,
        ]
        .into_iter()
        .find(|s| s.as_str() == raw)
    }

    pub const fn is_terminal(self) -> bool {
        !matches!(self, UpdateOpStatus::Running)
    }
}

/// One durable update operation row. `before_*` is what the install looked
/// like when the operation started; `after_*` is its target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateOperation {
    pub id: UpdateOpId,
    pub kind: UpdateOpKind,
    pub status: UpdateOpStatus,
    pub channel: Option<String>,
    pub before_version: Option<String>,
    pub after_version: Option<String>,
    pub before_digest: Option<String>,
    pub after_digest: Option<String>,
    /// The artifact the install pointed at before this operation (needed to
    /// restore the exact previous state on rollback).
    pub before_artifact: Option<String>,
    pub artifact: Option<String>,
    /// The verified signing identity, when a manifest was involved.
    pub identity: Option<String>,
    /// The certification level the manifest carried, when any.
    pub certification_level: Option<String>,
    /// Idempotency key for a replayable `stage`.
    pub idempotency_key: Option<String>,
    pub detail: Option<String>,
    pub created_ms: i64,
    pub updated_ms: i64,
}

impl UpdateOperation {
    pub fn new(kind: UpdateOpKind, now_ms: i64, detail: Option<String>) -> Self {
        UpdateOperation {
            id: UpdateOpId::generate(kind),
            kind,
            status: UpdateOpStatus::Running,
            channel: None,
            before_version: None,
            after_version: None,
            before_digest: None,
            after_digest: None,
            before_artifact: None,
            artifact: None,
            identity: None,
            certification_level: None,
            idempotency_key: None,
            detail,
            created_ms: now_ms,
            updated_ms: now_ms,
        }
    }

    /// True when the row is an unfinished operation a crash left behind.
    pub fn is_running(&self) -> bool {
        self.status == UpdateOpStatus::Running
    }
}

/// The durable seam.
pub trait UpdaterStore: Send + Sync {
    fn insert(&self, operation: &UpdateOperation) -> Result<(), UpdateStoreError>;
    fn update(&self, operation: &UpdateOperation) -> Result<(), UpdateStoreError>;
    fn get(&self, id: &str) -> Result<Option<UpdateOperation>, UpdateStoreError>;
    /// Newest-first listing, bounded by `limit` (hard cap [`MAX_LIST`]).
    fn list(&self, limit: usize) -> Result<Vec<UpdateOperation>, UpdateStoreError>;
    /// Every operation still in `running` state (crash residue), oldest
    /// first so recovery replays in the order the steps were issued.
    fn running(&self) -> Result<Vec<UpdateOperation>, UpdateStoreError>;
    /// The newest operation of one kind/status pair, if any.
    fn latest(
        &self,
        kind: UpdateOpKind,
        status: UpdateOpStatus,
    ) -> Result<Option<UpdateOperation>, UpdateStoreError>;
    /// A staged operation recorded under one idempotency key.
    fn by_key(&self, key: &str) -> Result<Option<UpdateOperation>, UpdateStoreError>;
}

fn bounded(limit: usize) -> usize {
    limit.clamp(1, MAX_LIST)
}

/// In-memory store (tests + embedded hosts without a data dir).
#[derive(Debug, Default)]
pub struct MemoryUpdaterStore {
    rows: Mutex<Vec<UpdateOperation>>,
}

impl MemoryUpdaterStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Vec<UpdateOperation>>, UpdateStoreError> {
        self.rows
            .lock()
            .map_err(|_| UpdateStoreError::Backend("update store lock is poisoned".into()))
    }
}

impl UpdaterStore for MemoryUpdaterStore {
    fn insert(&self, operation: &UpdateOperation) -> Result<(), UpdateStoreError> {
        let mut rows = self.lock()?;
        if rows.iter().any(|row| row.id == operation.id) {
            return Err(UpdateStoreError::Malformed(format!(
                "update operation {} already exists",
                operation.id
            )));
        }
        rows.push(operation.clone());
        Ok(())
    }

    fn update(&self, operation: &UpdateOperation) -> Result<(), UpdateStoreError> {
        let mut rows = self.lock()?;
        let Some(slot) = rows.iter_mut().find(|row| row.id == operation.id) else {
            return Err(UpdateStoreError::Malformed(format!(
                "update operation {} does not exist",
                operation.id
            )));
        };
        *slot = operation.clone();
        Ok(())
    }

    fn get(&self, id: &str) -> Result<Option<UpdateOperation>, UpdateStoreError> {
        Ok(self
            .lock()?
            .iter()
            .find(|row| row.id.as_str() == id)
            .cloned())
    }

    fn list(&self, limit: usize) -> Result<Vec<UpdateOperation>, UpdateStoreError> {
        let rows = self.lock()?;
        let mut items: Vec<UpdateOperation> = rows.clone();
        items.sort_by(|a, b| {
            b.created_ms
                .cmp(&a.created_ms)
                .then_with(|| b.id.cmp(&a.id))
        });
        items.truncate(bounded(limit));
        Ok(items)
    }

    fn running(&self) -> Result<Vec<UpdateOperation>, UpdateStoreError> {
        let rows = self.lock()?;
        let mut items: Vec<UpdateOperation> = rows
            .iter()
            .filter(|row| row.is_running())
            .cloned()
            .collect();
        items.sort_by(|a, b| {
            a.created_ms
                .cmp(&b.created_ms)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(items)
    }

    fn latest(
        &self,
        kind: UpdateOpKind,
        status: UpdateOpStatus,
    ) -> Result<Option<UpdateOperation>, UpdateStoreError> {
        let rows = self.lock()?;
        Ok(rows
            .iter()
            .filter(|row| row.kind == kind && row.status == status)
            .max_by(|a, b| {
                a.created_ms
                    .cmp(&b.created_ms)
                    .then_with(|| a.id.cmp(&b.id))
            })
            .cloned())
    }

    fn by_key(&self, key: &str) -> Result<Option<UpdateOperation>, UpdateStoreError> {
        let rows = self.lock()?;
        Ok(rows
            .iter()
            .filter(|row| row.idempotency_key.as_deref() == Some(key))
            .max_by(|a, b| {
                a.created_ms
                    .cmp(&b.created_ms)
                    .then_with(|| a.id.cmp(&b.id))
            })
            .cloned())
    }
}

/// SQLite-backed durable store (the daemon's `update.db`).
pub struct SqliteUpdaterStore {
    conn: Mutex<rusqlite::Connection>,
}

const UPDATER_MIGRATIONS: &[&str] = &["
CREATE TABLE IF NOT EXISTS update_operation (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    status TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    payload TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS update_operation_status ON update_operation (status);
CREATE INDEX IF NOT EXISTS update_operation_kind_status ON update_operation (kind, status);
"];

impl SqliteUpdaterStore {
    /// Open (creating) the updater database at `path`.
    pub fn open(path: &Path) -> Result<Self, UpdateStoreError> {
        let conn = rusqlite::Connection::open(path).map_err(backend)?;
        Self::prepare(conn)
    }

    /// Open an in-memory database (tests, ephemeral hosts).
    pub fn open_in_memory() -> Result<Self, UpdateStoreError> {
        let conn = rusqlite::Connection::open_in_memory().map_err(backend)?;
        Self::prepare(conn)
    }

    fn prepare(conn: rusqlite::Connection) -> Result<Self, UpdateStoreError> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;",
        )
        .map_err(backend)?;
        let mut conn = conn;
        migrate(&mut conn)?;
        Ok(SqliteUpdaterStore {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, rusqlite::Connection>, UpdateStoreError> {
        self.conn
            .lock()
            .map_err(|_| UpdateStoreError::Backend("update store lock is poisoned".into()))
    }
}

fn backend(e: rusqlite::Error) -> UpdateStoreError {
    UpdateStoreError::Backend(e.to_string())
}

fn migrate(conn: &mut rusqlite::Connection) -> Result<(), UpdateStoreError> {
    let mut version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(backend)?;
    for (i, sql) in UPDATER_MIGRATIONS.iter().enumerate() {
        let target = (i + 1) as i64;
        if version >= target {
            continue;
        }
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(backend)?;
        tx.execute_batch(sql)
            .map_err(|e| UpdateStoreError::Backend(format!("updater migration v{target}: {e}")))?;
        tx.execute_batch(&format!("PRAGMA user_version = {target}"))
            .map_err(|e| UpdateStoreError::Backend(format!("updater migration v{target}: {e}")))?;
        tx.commit().map_err(backend)?;
        version = target;
    }
    Ok(())
}

fn encode(operation: &UpdateOperation) -> Result<String, UpdateStoreError> {
    serde_json::to_string(operation)
        .map_err(|e| UpdateStoreError::Malformed(format!("update row encode: {e}")))
}

fn decode(payload: &str) -> Result<UpdateOperation, UpdateStoreError> {
    serde_json::from_str(payload)
        .map_err(|e| UpdateStoreError::Malformed(format!("update row payload: {e}")))
}

impl UpdaterStore for SqliteUpdaterStore {
    fn insert(&self, operation: &UpdateOperation) -> Result<(), UpdateStoreError> {
        let conn = self.lock()?;
        // Strict insert: an existing row is a programming error, never a
        // silent overwrite of durable audit state.
        conn.execute(
            "INSERT INTO update_operation (id, kind, status, created_ms, payload)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![
                operation.id.as_str(),
                operation.kind.as_str(),
                operation.status.as_str(),
                operation.created_ms,
                encode(operation)?
            ],
        )
        .map_err(|e| {
            if matches!(
                e,
                rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error {
                        code: rusqlite::ErrorCode::ConstraintViolation,
                        ..
                    },
                    _
                )
            ) {
                UpdateStoreError::Malformed(format!(
                    "update operation {} already exists",
                    operation.id
                ))
            } else {
                backend(e)
            }
        })?;
        Ok(())
    }

    fn update(&self, operation: &UpdateOperation) -> Result<(), UpdateStoreError> {
        let conn = self.lock()?;
        let changed = conn
            .execute(
                "UPDATE update_operation
                 SET status = ?2, created_ms = ?3, payload = ?4
                 WHERE id = ?1",
                rusqlite::params![
                    operation.id.as_str(),
                    operation.status.as_str(),
                    operation.created_ms,
                    encode(operation)?
                ],
            )
            .map_err(backend)?;
        if changed == 0 {
            return Err(UpdateStoreError::Malformed(format!(
                "update operation {} does not exist",
                operation.id
            )));
        }
        Ok(())
    }

    fn get(&self, id: &str) -> Result<Option<UpdateOperation>, UpdateStoreError> {
        let conn = self.lock()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM update_operation WHERE id = ?1",
                rusqlite::params![id],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        payload.map(|p| decode(&p)).transpose()
    }

    fn list(&self, limit: usize) -> Result<Vec<UpdateOperation>, UpdateStoreError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT payload FROM update_operation
                 ORDER BY created_ms DESC, id DESC LIMIT ?1",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map(rusqlite::params![bounded(limit) as i64], |r| {
                r.get::<_, String>(0)
            })
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        rows.iter().map(|payload| decode(payload)).collect()
    }

    fn running(&self) -> Result<Vec<UpdateOperation>, UpdateStoreError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT payload FROM update_operation
                 WHERE status = 'running' ORDER BY created_ms ASC, id ASC",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        rows.iter().map(|payload| decode(payload)).collect()
    }

    fn latest(
        &self,
        kind: UpdateOpKind,
        status: UpdateOpStatus,
    ) -> Result<Option<UpdateOperation>, UpdateStoreError> {
        let conn = self.lock()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM update_operation
                 WHERE kind = ?1 AND status = ?2
                 ORDER BY created_ms DESC, id DESC LIMIT 1",
                rusqlite::params![kind.as_str(), status.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        payload.map(|p| decode(&p)).transpose()
    }

    fn by_key(&self, key: &str) -> Result<Option<UpdateOperation>, UpdateStoreError> {
        let conn = self.lock()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM update_operation
                 WHERE json_extract(payload, '$.idempotency_key') = ?1
                 ORDER BY created_ms DESC, id DESC LIMIT 1",
                rusqlite::params![key],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        payload.map(|p| decode(&p)).transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: UpdateOpKind, status: UpdateOpStatus, created_ms: i64) -> UpdateOperation {
        let mut operation = UpdateOperation::new(kind, created_ms, None);
        operation.status = status;
        operation.updated_ms = created_ms;
        operation
    }

    fn exercise(store: &dyn UpdaterStore) {
        let staged = row(UpdateOpKind::Stage, UpdateOpStatus::Staged, 10);
        let check = row(UpdateOpKind::Check, UpdateOpStatus::Succeeded, 20);
        let running = row(UpdateOpKind::Apply, UpdateOpStatus::Running, 30);
        store.insert(&staged).unwrap();
        store.insert(&check).unwrap();
        store.insert(&running).unwrap();

        assert_eq!(store.get(staged.id.as_str()).unwrap(), Some(staged.clone()));
        assert_eq!(store.list(2).unwrap().len(), 2);
        assert_eq!(store.list(2).unwrap()[0], running);
        assert_eq!(store.running().unwrap(), vec![running.clone()]);
        assert_eq!(
            store
                .latest(UpdateOpKind::Stage, UpdateOpStatus::Staged)
                .unwrap(),
            Some(staged.clone())
        );
        assert_eq!(
            store
                .latest(UpdateOpKind::Apply, UpdateOpStatus::Staged)
                .unwrap(),
            None
        );

        // Transition: only the recorded row's status may move.
        let mut applied = running.clone();
        applied.status = UpdateOpStatus::Applied;
        applied.updated_ms = 40;
        store.update(&applied).unwrap();
        assert_eq!(store.running().unwrap(), vec![]);
        assert_eq!(
            store
                .latest(UpdateOpKind::Apply, UpdateOpStatus::Applied)
                .unwrap(),
            Some(applied)
        );

        // Updating a row that does not exist is a typed error, never a
        // silent no-op.
        let ghost = row(UpdateOpKind::Apply, UpdateOpStatus::Failed, 50);
        assert!(store.update(&ghost).is_err());
        // Duplicate insert is refused.
        assert!(store.insert(&staged).is_err());

        // Idempotency-key lookup.
        let mut keyed = row(UpdateOpKind::Stage, UpdateOpStatus::Staged, 60);
        keyed.idempotency_key = Some("k-1".into());
        store.insert(&keyed).unwrap();
        assert_eq!(store.by_key("k-1").unwrap(), Some(keyed));
        assert_eq!(store.by_key("missing").unwrap(), None);
    }

    #[test]
    fn memory_store_records_every_transition() {
        exercise(&MemoryUpdaterStore::new());
    }

    #[test]
    fn sqlite_store_records_every_transition_and_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update.db");
        {
            let store = SqliteUpdaterStore::open(&path).unwrap();
            exercise(&store);
            // A crash leaves a running row; a NEW connection must see it.
            let running = row(UpdateOpKind::Rollback, UpdateOpStatus::Running, 70);
            store.insert(&running).unwrap();
        }
        let reopened = SqliteUpdaterStore::open(&path).unwrap();
        let running = reopened.running().unwrap();
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].kind, UpdateOpKind::Rollback);
    }

    #[test]
    fn kind_and_status_names_round_trip() {
        for kind in UpdateOpKind::ALL {
            assert_eq!(UpdateOpKind::parse(kind.as_str()), Some(*kind));
        }
        for status in [
            UpdateOpStatus::Running,
            UpdateOpStatus::Succeeded,
            UpdateOpStatus::Staged,
            UpdateOpStatus::Applied,
            UpdateOpStatus::RolledBack,
            UpdateOpStatus::Failed,
            UpdateOpStatus::Unverified,
        ] {
            assert_eq!(UpdateOpStatus::parse(status.as_str()), Some(status));
            assert_eq!(status.is_terminal(), status != UpdateOpStatus::Running);
        }
        assert_eq!(UpdateOpKind::parse("delete"), None);
        assert_eq!(UpdateOpStatus::parse("half"), None);
    }

    #[test]
    fn operation_ids_are_typed_and_unique() {
        let a = UpdateOpId::generate(UpdateOpKind::Stage);
        let b = UpdateOpId::generate(UpdateOpKind::Stage);
        assert_ne!(a, b);
        assert!(a.as_str().starts_with("upd_stage_"));
        assert!(UpdateOpId::try_new("").is_err());
        assert!(UpdateOpId::try_new("x".repeat(200)).is_err());
        assert!(UpdateOpId::try_new("upd_ok").is_ok());
    }
}
