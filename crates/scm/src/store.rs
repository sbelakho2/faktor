//! Durable rows of the SCM domain: installations, repositories, webhook
//! deliveries (dedupe), external-operation identities and provider
//! rate-limit state.
//!
//! The seam is [`ScmStore`]: the in-memory implementation serves unit tests
//! and embedded hosts; [`SqliteScmStore`] is the durable implementation
//! (its own database file and migration cursor — SCM state is control-plane
//! state, not session state). Every write is an idempotent upsert or an
//! insert-if-absent claim, so replaying any ingest after a crash converges.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};

/// One installation row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallationRow {
    pub installation_id: i64,
    pub account_login: String,
    pub account_type: String,
    /// The provider's granted permission set, canonical JSON
    /// (`{"contents":"write",...}`).
    pub permissions_json: String,
    pub suspended: bool,
    pub updated_ms: i64,
}

/// One repository row. `organization_id` is the control-plane tenant the
/// repository is linked to — every SCM row carries its organization
/// identity, so a listing can never leak across tenants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryRow {
    pub id: i64,
    pub installation_id: i64,
    pub organization_id: String,
    pub owner: String,
    pub name: String,
    pub full_name: String,
    pub default_branch: String,
    pub private: bool,
    pub archived: bool,
    pub updated_ms: i64,
}

/// The outcome of claiming one webhook delivery id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryClaim {
    /// First sighting of this delivery id: the caller may apply it.
    Fresh,
    /// The delivery id was already claimed (a redelivery or a replay):
    /// `first_seen_ms` is when the durable row was first written.
    Duplicate { first_seen_ms: i64 },
}

/// One durable external-operation identity (the caller's key plus the
/// provider-side object it produced).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScmOperationRow {
    pub operation_id: String,
    pub provider: String,
    pub kind: String,
    pub repository: String,
    pub marker: String,
    /// The provider-side object id, once known.
    pub external_id: Option<String>,
    /// The provider's opaque version token, once known.
    pub version: Option<String>,
    /// `prepared` before the remote call, `completed` after it.
    pub state: String,
    pub updated_ms: i64,
}

/// One recorded provider rate-limit observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RateLimitRow {
    pub provider: String,
    pub until_ms: i64,
    pub observed_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScmStoreError {
    #[error("scm store backend unavailable: {0}")]
    Backend(String),
    #[error("scm store refused malformed row: {0}")]
    Malformed(String),
}

/// The durable SCM state seam. Object-safe: the server holds one
/// `Arc<dyn ScmStore>` shared by the adapter, the sync service and the
/// webhook inbox, so all three observe the same durable truth.
pub trait ScmStore: Send + Sync {
    fn upsert_installation(&self, row: &InstallationRow) -> Result<(), ScmStoreError>;
    fn installations(&self) -> Result<Vec<InstallationRow>, ScmStoreError>;
    fn upsert_repository(&self, row: &RepositoryRow) -> Result<i64, ScmStoreError>;
    /// The repositories linked to one organization, ordered by row id,
    /// strictly after `after_id` (cursor pagination).
    fn repositories_for_organization(
        &self,
        organization_id: &str,
        after_id: i64,
        limit: usize,
    ) -> Result<Vec<RepositoryRow>, ScmStoreError>;
    fn repositories_for_installation(
        &self,
        installation_id: i64,
    ) -> Result<Vec<RepositoryRow>, ScmStoreError>;
    /// Claim one delivery id exactly once (durable dedupe).
    fn claim_webhook_delivery(
        &self,
        delivery_id: &str,
        event: &str,
        received_ms: i64,
    ) -> Result<DeliveryClaim, ScmStoreError>;
    fn webhook_deliveries(&self, limit: usize)
        -> Result<Vec<(String, String, i64)>, ScmStoreError>;
    /// Upsert the durable operation identity (idempotent by `operation_id`).
    /// A `None` `external_id`/`version` NEVER erases a recorded one: a
    /// `prepared` rewrite of an already-completed operation preserves the
    /// observed remote identity (the same COALESCE semantics in both
    /// implementations).
    fn record_external_operation(&self, row: &ScmOperationRow) -> Result<(), ScmStoreError>;
    fn external_operation(
        &self,
        operation_id: &str,
    ) -> Result<Option<ScmOperationRow>, ScmStoreError>;
    fn record_rate_limit(&self, row: &RateLimitRow) -> Result<(), ScmStoreError>;
    fn rate_limit(&self, provider: &str) -> Result<Option<RateLimitRow>, ScmStoreError>;
}

// ------------------------------------------------------------- in-memory

#[derive(Default)]
struct MemInner {
    installations: BTreeMap<i64, InstallationRow>,
    repositories: Vec<RepositoryRow>,
    next_repository_id: i64,
    deliveries: BTreeMap<String, (String, i64)>,
    operations: BTreeMap<String, ScmOperationRow>,
    rate_limits: BTreeMap<String, RateLimitRow>,
}

/// In-memory [`ScmStore`]: unit tests and embedded hosts. Durable across
/// calls, not across process restarts — the restart tests use
/// [`SqliteScmStore`].
#[derive(Default)]
pub struct MemoryScmStore {
    inner: Mutex<MemInner>,
}

impl MemoryScmStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, MemInner>, ScmStoreError> {
        self.inner
            .lock()
            .map_err(|_| ScmStoreError::Backend("in-memory scm store lock is poisoned".into()))
    }
}

impl ScmStore for MemoryScmStore {
    fn upsert_installation(&self, row: &InstallationRow) -> Result<(), ScmStoreError> {
        self.lock()?
            .installations
            .insert(row.installation_id, row.clone());
        Ok(())
    }

    fn installations(&self) -> Result<Vec<InstallationRow>, ScmStoreError> {
        Ok(self.lock()?.installations.values().cloned().collect())
    }

    fn upsert_repository(&self, row: &RepositoryRow) -> Result<i64, ScmStoreError> {
        let mut inner = self.lock()?;
        let position = inner.repositories.iter().position(|existing| {
            existing.installation_id == row.installation_id
                && existing.owner.eq_ignore_ascii_case(&row.owner)
                && existing.name.eq_ignore_ascii_case(&row.name)
        });
        match position {
            Some(index) => {
                let id = inner.repositories[index].id;
                let mut stored = row.clone();
                stored.id = id;
                inner.repositories[index] = stored;
                Ok(id)
            }
            None => {
                inner.next_repository_id += 1;
                let id = inner.next_repository_id;
                let mut stored = row.clone();
                stored.id = id;
                inner.repositories.push(stored);
                Ok(id)
            }
        }
    }

    fn repositories_for_organization(
        &self,
        organization_id: &str,
        after_id: i64,
        limit: usize,
    ) -> Result<Vec<RepositoryRow>, ScmStoreError> {
        Ok(self
            .lock()?
            .repositories
            .iter()
            .filter(|row| row.organization_id == organization_id && row.id > after_id)
            .take(limit)
            .cloned()
            .collect())
    }

    fn repositories_for_installation(
        &self,
        installation_id: i64,
    ) -> Result<Vec<RepositoryRow>, ScmStoreError> {
        Ok(self
            .lock()?
            .repositories
            .iter()
            .filter(|row| row.installation_id == installation_id)
            .cloned()
            .collect())
    }

    fn claim_webhook_delivery(
        &self,
        delivery_id: &str,
        event: &str,
        received_ms: i64,
    ) -> Result<DeliveryClaim, ScmStoreError> {
        let mut inner = self.lock()?;
        match inner.deliveries.get(delivery_id) {
            Some((_, first_seen_ms)) => Ok(DeliveryClaim::Duplicate {
                first_seen_ms: *first_seen_ms,
            }),
            None => {
                inner
                    .deliveries
                    .insert(delivery_id.to_string(), (event.to_string(), received_ms));
                Ok(DeliveryClaim::Fresh)
            }
        }
    }

    fn webhook_deliveries(
        &self,
        limit: usize,
    ) -> Result<Vec<(String, String, i64)>, ScmStoreError> {
        Ok(self
            .lock()?
            .deliveries
            .iter()
            .take(limit)
            .map(|(id, (event, ms))| (id.clone(), event.clone(), *ms))
            .collect())
    }

    fn record_external_operation(&self, row: &ScmOperationRow) -> Result<(), ScmStoreError> {
        let mut inner = self.lock()?;
        let mut stored = row.clone();
        if let Some(existing) = inner.operations.get(&row.operation_id) {
            if stored.external_id.is_none() {
                stored.external_id = existing.external_id.clone();
            }
            if stored.version.is_none() {
                stored.version = existing.version.clone();
            }
        }
        inner.operations.insert(row.operation_id.clone(), stored);
        Ok(())
    }

    fn external_operation(
        &self,
        operation_id: &str,
    ) -> Result<Option<ScmOperationRow>, ScmStoreError> {
        Ok(self.lock()?.operations.get(operation_id).cloned())
    }

    fn record_rate_limit(&self, row: &RateLimitRow) -> Result<(), ScmStoreError> {
        self.lock()?
            .rate_limits
            .insert(row.provider.clone(), row.clone());
        Ok(())
    }

    fn rate_limit(&self, provider: &str) -> Result<Option<RateLimitRow>, ScmStoreError> {
        Ok(self.lock()?.rate_limits.get(provider).cloned())
    }
}

// ---------------------------------------------------------------- sqlite

/// The durable [`ScmStore`] over its own SQLite database file. WAL +
/// busy-timeout + an explicit migration cursor (`PRAGMA user_version`), the
/// same storage discipline as the daemon store: every value read back is
/// parsed fallibly and every upsert is transactional.
pub struct SqliteScmStore {
    conn: Mutex<Connection>,
}

const SCM_MIGRATIONS: &[&str] = &[
    // v1 — the whole SCM control-plane schema.
    "CREATE TABLE IF NOT EXISTS scm_installation (
        installation_id INTEGER PRIMARY KEY,
        account_login TEXT NOT NULL,
        account_type TEXT NOT NULL,
        permissions TEXT NOT NULL,
        suspended INTEGER NOT NULL DEFAULT 0,
        updated_ms INTEGER NOT NULL
     );
     CREATE TABLE IF NOT EXISTS scm_repository (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        installation_id INTEGER NOT NULL,
        organization_id TEXT NOT NULL,
        owner TEXT NOT NULL,
        name TEXT NOT NULL,
        full_name TEXT NOT NULL,
        default_branch TEXT NOT NULL,
        private INTEGER NOT NULL,
        archived INTEGER NOT NULL,
        updated_ms INTEGER NOT NULL,
        UNIQUE (installation_id, owner, name)
     );
     CREATE INDEX IF NOT EXISTS idx_scm_repository_org ON scm_repository(organization_id, id);
     CREATE TABLE IF NOT EXISTS scm_webhook_delivery (
        delivery_id TEXT PRIMARY KEY,
        event TEXT NOT NULL,
        received_ms INTEGER NOT NULL
     );
     CREATE TABLE IF NOT EXISTS scm_external_operation (
        operation_id TEXT PRIMARY KEY,
        provider TEXT NOT NULL,
        kind TEXT NOT NULL,
        repository TEXT NOT NULL,
        marker TEXT NOT NULL,
        external_id TEXT,
        version TEXT,
        state TEXT NOT NULL,
        updated_ms INTEGER NOT NULL
     );
     CREATE TABLE IF NOT EXISTS scm_rate_limit (
        provider TEXT PRIMARY KEY,
        until_ms INTEGER NOT NULL,
        observed_ms INTEGER NOT NULL
     );",
];

impl SqliteScmStore {
    /// Open (creating) the SCIM database at `path` and apply migrations.
    pub fn open(path: &Path) -> Result<Self, ScmStoreError> {
        let conn = Connection::open(path).map_err(backend)?;
        Self::prepare(conn)
    }

    /// Open an in-memory database (tests, ephemeral hosts).
    pub fn open_in_memory() -> Result<Self, ScmStoreError> {
        let conn = Connection::open_in_memory().map_err(backend)?;
        Self::prepare(conn)
    }

    fn prepare(conn: Connection) -> Result<Self, ScmStoreError> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;
             PRAGMA foreign_keys = ON;",
        )
        .map_err(backend)?;
        let mut conn = conn;
        migrate(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, ScmStoreError> {
        self.conn
            .lock()
            .map_err(|_| ScmStoreError::Backend("scm store lock is poisoned".into()))
    }
}

fn backend(e: rusqlite::Error) -> ScmStoreError {
    ScmStoreError::Backend(e.to_string())
}

fn migrate(conn: &mut Connection) -> Result<(), ScmStoreError> {
    let mut version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(backend)?;
    for (i, sql) in SCM_MIGRATIONS.iter().enumerate() {
        let target = (i + 1) as i64;
        if version >= target {
            continue;
        }
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(backend)?;
        tx.execute_batch(sql)
            .map_err(|e| ScmStoreError::Backend(format!("scm migration v{target}: {e}")))?;
        tx.execute_batch(&format!("PRAGMA user_version = {target}"))
            .map_err(|e| ScmStoreError::Backend(format!("scm migration v{target} cursor: {e}")))?;
        tx.commit().map_err(backend)?;
        version = target;
    }
    Ok(())
}

fn installation_from_row(r: &rusqlite::Row<'_>) -> Result<InstallationRow, rusqlite::Error> {
    Ok(InstallationRow {
        installation_id: r.get(0)?,
        account_login: r.get(1)?,
        account_type: r.get(2)?,
        permissions_json: r.get(3)?,
        suspended: r.get::<_, i64>(4)? != 0,
        updated_ms: r.get(5)?,
    })
}

fn repository_from_row(r: &rusqlite::Row<'_>) -> Result<RepositoryRow, rusqlite::Error> {
    Ok(RepositoryRow {
        id: r.get(0)?,
        installation_id: r.get(1)?,
        organization_id: r.get(2)?,
        owner: r.get(3)?,
        name: r.get(4)?,
        full_name: r.get(5)?,
        default_branch: r.get(6)?,
        private: r.get::<_, i64>(7)? != 0,
        archived: r.get::<_, i64>(8)? != 0,
        updated_ms: r.get(9)?,
    })
}

fn operation_from_row(r: &rusqlite::Row<'_>) -> Result<ScmOperationRow, rusqlite::Error> {
    Ok(ScmOperationRow {
        operation_id: r.get(0)?,
        provider: r.get(1)?,
        kind: r.get(2)?,
        repository: r.get(3)?,
        marker: r.get(4)?,
        external_id: r.get(5)?,
        version: r.get(6)?,
        state: r.get(7)?,
        updated_ms: r.get(8)?,
    })
}

impl ScmStore for SqliteScmStore {
    fn upsert_installation(&self, row: &InstallationRow) -> Result<(), ScmStoreError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO scm_installation
                (installation_id, account_login, account_type, permissions, suspended, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(installation_id) DO UPDATE SET
                account_login = excluded.account_login,
                account_type = excluded.account_type,
                permissions = excluded.permissions,
                suspended = excluded.suspended,
                updated_ms = excluded.updated_ms",
            params![
                row.installation_id,
                row.account_login,
                row.account_type,
                row.permissions_json,
                row.suspended as i64,
                row.updated_ms,
            ],
        )
        .map_err(backend)?;
        Ok(())
    }

    fn installations(&self) -> Result<Vec<InstallationRow>, ScmStoreError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT installation_id, account_login, account_type, permissions, suspended, updated_ms
                 FROM scm_installation ORDER BY installation_id",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map([], installation_from_row)
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        Ok(rows)
    }

    fn upsert_repository(&self, row: &RepositoryRow) -> Result<i64, ScmStoreError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO scm_repository
                (installation_id, organization_id, owner, name, full_name, default_branch,
                 private, archived, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(installation_id, owner, name) DO UPDATE SET
                organization_id = excluded.organization_id,
                full_name = excluded.full_name,
                default_branch = excluded.default_branch,
                private = excluded.private,
                archived = excluded.archived,
                updated_ms = excluded.updated_ms",
            params![
                row.installation_id,
                row.organization_id,
                row.owner,
                row.name,
                row.full_name,
                row.default_branch,
                row.private as i64,
                row.archived as i64,
                row.updated_ms,
            ],
        )
        .map_err(backend)?;
        let id: i64 = conn
            .query_row(
                "SELECT id FROM scm_repository WHERE installation_id = ?1 AND owner = ?2 AND name = ?3",
                params![row.installation_id, row.owner, row.name],
                |r| r.get(0),
            )
            .map_err(backend)?;
        Ok(id)
    }

    fn repositories_for_organization(
        &self,
        organization_id: &str,
        after_id: i64,
        limit: usize,
    ) -> Result<Vec<RepositoryRow>, ScmStoreError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, installation_id, organization_id, owner, name, full_name,
                        default_branch, private, archived, updated_ms
                 FROM scm_repository
                 WHERE organization_id = ?1 AND id > ?2
                 ORDER BY id LIMIT ?3",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map(
                params![organization_id, after_id, limit as i64],
                repository_from_row,
            )
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        Ok(rows)
    }

    fn repositories_for_installation(
        &self,
        installation_id: i64,
    ) -> Result<Vec<RepositoryRow>, ScmStoreError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT id, installation_id, organization_id, owner, name, full_name,
                        default_branch, private, archived, updated_ms
                 FROM scm_repository WHERE installation_id = ?1 ORDER BY id",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map(params![installation_id], repository_from_row)
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        Ok(rows)
    }

    fn claim_webhook_delivery(
        &self,
        delivery_id: &str,
        event: &str,
        received_ms: i64,
    ) -> Result<DeliveryClaim, ScmStoreError> {
        let conn = self.lock()?;
        let inserted = conn
            .execute(
                "INSERT OR IGNORE INTO scm_webhook_delivery (delivery_id, event, received_ms)
                 VALUES (?1, ?2, ?3)",
                params![delivery_id, event, received_ms],
            )
            .map_err(backend)?;
        if inserted == 1 {
            return Ok(DeliveryClaim::Fresh);
        }
        let first_seen_ms: i64 = conn
            .query_row(
                "SELECT received_ms FROM scm_webhook_delivery WHERE delivery_id = ?1",
                params![delivery_id],
                |r| r.get(0),
            )
            .map_err(backend)?;
        Ok(DeliveryClaim::Duplicate { first_seen_ms })
    }

    fn webhook_deliveries(
        &self,
        limit: usize,
    ) -> Result<Vec<(String, String, i64)>, ScmStoreError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare("SELECT delivery_id, event, received_ms FROM scm_webhook_delivery ORDER BY received_ms LIMIT ?1")
            .map_err(backend)?;
        let rows = stmt
            .query_map(params![limit as i64], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        Ok(rows)
    }

    fn record_external_operation(&self, row: &ScmOperationRow) -> Result<(), ScmStoreError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO scm_external_operation
                (operation_id, provider, kind, repository, marker, external_id, version, state, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(operation_id) DO UPDATE SET
                provider = excluded.provider,
                kind = excluded.kind,
                repository = excluded.repository,
                marker = excluded.marker,
                external_id = COALESCE(excluded.external_id, scm_external_operation.external_id),
                version = COALESCE(excluded.version, scm_external_operation.version),
                state = excluded.state,
                updated_ms = excluded.updated_ms",
            params![
                row.operation_id,
                row.provider,
                row.kind,
                row.repository,
                row.marker,
                row.external_id,
                row.version,
                row.state,
                row.updated_ms,
            ],
        )
        .map_err(backend)?;
        Ok(())
    }

    fn external_operation(
        &self,
        operation_id: &str,
    ) -> Result<Option<ScmOperationRow>, ScmStoreError> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT operation_id, provider, kind, repository, marker, external_id, version, state, updated_ms
             FROM scm_external_operation WHERE operation_id = ?1",
            params![operation_id],
            operation_from_row,
        )
        .optional()
        .map_err(backend)
    }

    fn record_rate_limit(&self, row: &RateLimitRow) -> Result<(), ScmStoreError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO scm_rate_limit (provider, until_ms, observed_ms)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(provider) DO UPDATE SET
                until_ms = excluded.until_ms,
                observed_ms = excluded.observed_ms",
            params![row.provider, row.until_ms, row.observed_ms],
        )
        .map_err(backend)?;
        Ok(())
    }

    fn rate_limit(&self, provider: &str) -> Result<Option<RateLimitRow>, ScmStoreError> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT provider, until_ms, observed_ms FROM scm_rate_limit WHERE provider = ?1",
            params![provider],
            |r| {
                Ok(RateLimitRow {
                    provider: r.get(0)?,
                    until_ms: r.get(1)?,
                    observed_ms: r.get(2)?,
                })
            },
        )
        .optional()
        .map_err(backend)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installation() -> InstallationRow {
        InstallationRow {
            installation_id: 11,
            account_login: "acme".into(),
            account_type: "Organization".into(),
            permissions_json: "{\"contents\":\"write\"}".into(),
            suspended: false,
            updated_ms: 1,
        }
    }

    fn repository(org: &str, owner: &str, name: &str) -> RepositoryRow {
        RepositoryRow {
            id: 0,
            installation_id: 11,
            organization_id: org.into(),
            owner: owner.into(),
            name: name.into(),
            full_name: format!("{owner}/{name}"),
            default_branch: "main".into(),
            private: true,
            archived: false,
            updated_ms: 1,
        }
    }

    fn roundtrip(store: &dyn ScmStore, tag: &str) {
        store.upsert_installation(&installation()).unwrap();
        store.upsert_installation(&installation()).unwrap();
        assert_eq!(store.installations().unwrap().len(), 1);

        let id1 = store
            .upsert_repository(&repository("org:a", "acme", "widgets"))
            .unwrap();
        let id2 = store
            .upsert_repository(&repository("org:a", "acme", "widgets"))
            .unwrap();
        assert_eq!(id1, id2, "repeated upserts must not duplicate a repository");
        store
            .upsert_repository(&repository("org:b", "acme", "gadgets"))
            .unwrap();
        let org_a = store.repositories_for_organization("org:a", 0, 10).unwrap();
        assert_eq!(org_a.len(), 1);
        assert_eq!(org_a[0].full_name, "acme/widgets");
        assert_eq!(
            store
                .repositories_for_organization("org:b", 0, 10)
                .unwrap()
                .len(),
            1
        );
        assert!(store
            .repositories_for_organization("org:a", org_a[0].id, 10)
            .unwrap()
            .is_empty());

        let delivery = format!("d1-{tag}");
        assert_eq!(
            store.claim_webhook_delivery(&delivery, "push", 5).unwrap(),
            DeliveryClaim::Fresh
        );
        assert_eq!(
            store.claim_webhook_delivery(&delivery, "push", 9).unwrap(),
            DeliveryClaim::Duplicate { first_seen_ms: 5 }
        );
        assert!(store
            .webhook_deliveries(10)
            .unwrap()
            .iter()
            .any(|(id, event, ms)| id == &delivery && event == "push" && *ms == 5));

        let op = ScmOperationRow {
            operation_id: "task:1".into(),
            provider: "github".into(),
            kind: "pull_request".into(),
            repository: "acme/widgets".into(),
            marker: "faktor:task:1".into(),
            external_id: None,
            version: None,
            state: "prepared".into(),
            updated_ms: 2,
        };
        store.record_external_operation(&op).unwrap();
        let completed = ScmOperationRow {
            external_id: Some("42".into()),
            version: Some("abc".into()),
            state: "completed".into(),
            updated_ms: 3,
            ..op.clone()
        };
        store.record_external_operation(&completed).unwrap();
        let read = store.external_operation("task:1").unwrap().unwrap();
        assert_eq!(read.state, "completed");
        assert_eq!(read.external_id.as_deref(), Some("42"));
        assert_eq!(read.version.as_deref(), Some("abc"));

        store
            .record_rate_limit(&RateLimitRow {
                provider: "github".into(),
                until_ms: 100,
                observed_ms: 5,
            })
            .unwrap();
        assert_eq!(store.rate_limit("github").unwrap().unwrap().until_ms, 100);
        assert!(store.rate_limit("gitlab").unwrap().is_none());
    }

    #[test]
    fn memory_store_roundtrip() {
        roundtrip(&MemoryScmStore::new(), "mem");
    }

    #[test]
    fn sqlite_store_roundtrip_and_restart_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scm.db");
        {
            let store = SqliteScmStore::open(&path).unwrap();
            roundtrip(&store, "first");
        }
        // Reopen: every row survives, and re-running the migration is a
        // no-op.
        let store = SqliteScmStore::open(&path).unwrap();
        roundtrip(&store, "second");
    }

    #[test]
    fn poisoned_sqlite_lock_refuses_typed() {
        let store = SqliteScmStore::open_in_memory().unwrap();
        let poisoner = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = store.conn.lock().unwrap();
            panic!("poison the scm store");
        }));
        assert!(poisoner.is_err());
        let err = store.installations().unwrap_err();
        assert!(matches!(err, ScmStoreError::Backend(_)));
    }
}
