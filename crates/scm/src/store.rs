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
    /// A database written by a NEWER binary's schema ladder must never be
    /// silently opened by an older one: the newer ladder may have changed
    /// semantics this binary cannot honour. Downgrade is refused typed; the
    /// recovery path is running the newer binary or restoring the
    /// pre-upgrade restore point.
    #[error(
        "scm store schema v{found} is newer than this binary's ladder v{maximum_supported}: \
         downgrade refused (run the newer binary or restore the pre-upgrade restore point)"
    )]
    UnsupportedSchema { found: i64, maximum_supported: i64 },
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
    // v2 — the durability policy marker (P1 audit): the writer RECORDS the
    // acknowledged-durability policy (`synchronous = FULL`) so `doctor` can
    // observe it (SQLite pragmas are connection-scoped and invisible to a
    // separate probe connection). Owned by the durability stack
    // (`faktor_cloud::durability`), the same marker cloud records.
    faktor_cloud::durability::POLICY_SCHEMA_V5,
];

impl SqliteScmStore {
    /// Open (creating) the SCM database at `path` and apply migrations.
    ///
    /// Durability policy (P1 audit; the [`faktor_cloud::durability`] stack):
    /// the writer connection opens `synchronous = FULL`, file-backed opens
    /// record the policy marker, write a VERIFIED pre-migration restore point
    /// before any schema transition away from an existing version, and run
    /// the interval-gated rotating backup.
    pub fn open(path: &Path) -> Result<Self, ScmStoreError> {
        let conn = Connection::open(path).map_err(backend)?;
        Self::prepare(conn, Some(path))
    }

    /// Open an in-memory database (tests, ephemeral hosts).
    pub fn open_in_memory() -> Result<Self, ScmStoreError> {
        let conn = Connection::open_in_memory().map_err(backend)?;
        Self::prepare(conn, None)
    }

    fn prepare(conn: Connection, path: Option<&Path>) -> Result<Self, ScmStoreError> {
        // The acknowledged-durability policy (WAL + synchronous = FULL; see
        // `faktor_cloud::durability` for the documented choice) applies to
        // EVERY open, in-memory included, before any migration or query.
        faktor_cloud::durability::apply_policy(&conn).map_err(durability)?;
        let mut conn = conn;
        migrate(&mut conn, path)?;
        if let Some(path) = path {
            let now = faktor_cloud::durability::now_ms();
            // Record the writer's policy for `doctor` (best effort: a full
            // disk must not take SCM down; doctor then reports the
            // absent/stale marker loudly).
            if let Err(e) = faktor_cloud::durability::record_open_policy(&conn, now) {
                tracing::error!("scm durability marker not recorded: {e}");
            }
            // Interval-gated verified backup. Best effort, like the daemon's
            // startup backup.
            match faktor_cloud::durability::rotate_backup(&conn, path) {
                Ok(Some(dest)) => {
                    tracing::info!("scm backup written to {}", dest.display());
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("scm backup skipped: {e}"),
            }
        }
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

fn durability(e: faktor_cloud::CloudStoreError) -> ScmStoreError {
    ScmStoreError::Backend(e.to_string())
}

/// Apply the SCM schema ladder. The version read, the pre-migration restore
/// point and every migration statement run inside ONE `BEGIN IMMEDIATE`
/// transaction: a second concurrent opener blocks on the write lock, then
/// re-reads the (already advanced) version inside its own transaction and
/// skips — it can never snapshot post-migration content and label it
/// `-pre-migration-vN-`.
///
/// A database written by a NEWER binary (`user_version` above this binary's
/// ladder) is refused typed, before any snapshot or write; a NEGATIVE
/// `user_version` (impossible for a database this ladder created) is refused
/// typed as corruption, before any snapshot or write.
fn migrate(conn: &mut Connection, db_path: Option<&Path>) -> Result<(), ScmStoreError> {
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(backend)?;
    let started: i64 = tx
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(backend)?;
    let ladder = SCM_MIGRATIONS.len() as i64;
    // A NEGATIVE `user_version` cannot have been produced by any legitimate
    // open of this ladder (SQLite stores the pragma as a SIGNED integer and
    // every writer here only ever moves it forward from 0). It is durable
    // corruption, and it is refused typed BEFORE the restore point and the
    // ladder: a snapshot labeled `-pre-migration-v-1-` would be a
    // trusted-looking way back to a state this binary never created, and
    // treating it as v0 would silently migrate corrupt state.
    if started < 0 {
        return Err(ScmStoreError::Malformed(format!(
            "scm store schema user_version {started} is corrupt (negative): \
             refusing to snapshot or migrate it"
        )));
    }
    if started > ladder {
        return Err(ScmStoreError::UnsupportedSchema {
            found: started,
            maximum_supported: ladder,
        });
    }
    if started == ladder {
        tx.commit().map_err(backend)?;
        return Ok(());
    }
    let mut version = started;
    // A schema transition (or FIRST creation) on a file-backed database runs
    // only after a verified restore point of the ACTUAL predecessor state
    // exists; if it cannot be written and self-verified, the migration is
    // REFUSED. The snapshot runs on a SEPARATE read-only connection: the
    // backup API cannot run on a connection that holds a write transaction,
    // and the BEGIN IMMEDIATE lock we hold makes every reader see exactly
    // this pre-migration state.
    if let Some(path) = db_path {
        let reader = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(backend)?;
        faktor_cloud::durability::migration_backup(&reader, path, version).map_err(|e| {
            ScmStoreError::Backend(format!(
                "refusing migration without a verified pre-migration restore point: {e}"
            ))
        })?;
        drop(reader);
        #[cfg(test)]
        if take_injected_crash(path) {
            return Err(ScmStoreError::Backend(
                "injected crash after the pre-migration restore point".into(),
            ));
        }
    }
    for (i, sql) in SCM_MIGRATIONS.iter().enumerate() {
        let target = (i + 1) as i64;
        if version >= target {
            continue;
        }
        tx.execute_batch(sql)
            .map_err(|e| ScmStoreError::Backend(format!("scm migration v{target}: {e}")))?;
        tx.execute_batch(&format!("PRAGMA user_version = {target}"))
            .map_err(|e| ScmStoreError::Backend(format!("scm migration v{target} cursor: {e}")))?;
        version = target;
    }
    tx.commit().map_err(backend)?;
    Ok(())
}

/// Test-only one-shot: make the NEXT migration of exactly `db_path` fail
/// AFTER the pre-migration restore point and BEFORE any migration SQL,
/// reproducing the crash-mid-migration durable state. Keyed by path so
/// concurrent tests never consume each other's injection.
#[cfg(test)]
static MIGRATION_CRASH: std::sync::Mutex<Option<std::path::PathBuf>> = std::sync::Mutex::new(None);

/// Arm [`MIGRATION_CRASH`] for `db_path` (one-shot; tests only).
#[cfg(test)]
fn inject_crash_before_migration(db_path: &Path) {
    *MIGRATION_CRASH.lock().unwrap() = Some(db_path.to_path_buf());
}

/// Consume the one-shot injection when it targets `db_path`.
#[cfg(test)]
fn take_injected_crash(db_path: &Path) -> bool {
    let mut armed = MIGRATION_CRASH.lock().unwrap();
    if armed.as_deref() == Some(db_path) {
        *armed = None;
        true
    } else {
        false
    }
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

    /// P1 durability: the commercial SCM writer connection acknowledges a
    /// commit only after an fsync (`synchronous = FULL`), so a crash/power
    /// loss cannot roll back an acknowledged repository/webhook mutation.
    #[test]
    fn commercial_writer_connection_is_synchronous_full() {
        let store = SqliteScmStore::open_in_memory().unwrap();
        let conn = store.lock().unwrap();
        let sync: i64 = conn
            .query_row("PRAGMA synchronous", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sync, 2, "synchronous = FULL on the writer connection");
    }

    /// A file-backed open records the writer policy marker `doctor` reads and
    /// writes a first restore-verified rotating backup.
    #[test]
    fn file_backed_open_records_the_policy_marker_and_a_verified_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scm.db");
        let store = SqliteScmStore::open(&path).unwrap();
        drop(store);
        let report = faktor_cloud::durability::doctor_probe(&path, false).unwrap();
        assert_eq!(report.journal_mode, "wal");
        assert!(report.integrity.is_empty(), "{:?}", report.integrity);
        let policy = report.policy.expect("the policy marker is recorded");
        for (key, value) in [
            ("synchronous", "FULL"),
            ("backup_policy", "rotating"),
            ("policy_version", "1"),
        ] {
            assert_eq!(
                policy
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| v.as_str()),
                Some(value),
                "marker key {key}"
            );
        }
        let (backup, _) = report
            .last_backup
            .expect("the first open writes a verified rotating backup");
        let expected = {
            let conn = Connection::open(&path).unwrap();
            faktor_cloud::durability::canonical_fingerprint(&conn).unwrap()
        };
        faktor_cloud::durability::restore_verify(&backup, &expected).unwrap();
    }

    /// Adversarial: more verified snapshots than the retention bound. The
    /// rotation must bound the kept set, and the newest must still verify.
    #[test]
    fn verified_backups_rotate_within_both_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scm.db");
        let store = SqliteScmStore::open(&path).unwrap();
        let conn = store.lock().unwrap();
        for _ in 0..(faktor_cloud::durability::MAX_BACKUP_FILES + 4) {
            let dest = faktor_cloud::durability::force_backup(&conn, &path).unwrap();
            let fp = faktor_cloud::durability::canonical_fingerprint(&conn).unwrap();
            faktor_cloud::durability::restore_verify(&dest, &fp).unwrap();
        }
        let kept = faktor_cloud::durability::list_rotating_backups(&path);
        assert_eq!(
            kept.len(),
            faktor_cloud::durability::MAX_BACKUP_FILES,
            "the rotating set is bounded"
        );
        let newest = kept.first().expect("a rotating backup is kept");
        let fp = faktor_cloud::durability::canonical_fingerprint(&conn).unwrap();
        faktor_cloud::durability::restore_verify(newest, &fp).unwrap();
        // Migration restore points are a separate class: never rotated as
        // rotating backups. The claim must match the live schema version so
        // the point self-verifies (see `faktor_cloud::durability`).
        faktor_cloud::durability::migration_backup(&conn, &path, 2).unwrap();
        assert_eq!(
            faktor_cloud::durability::list_rotating_backups(&path).len(),
            faktor_cloud::durability::MAX_BACKUP_FILES
        );
        assert!(faktor_cloud::durability::latest_migration_backup(&path).is_some());
    }

    /// Crash-mid-migration: the injected failure fires AFTER the
    /// pre-migration restore point and BEFORE any migration SQL. The database
    /// must stay at the old version, the restore point must exist and
    /// restore-verify, and `doctor` must report it.
    #[test]
    fn migration_crash_leaves_a_verified_restore_point_doctor_reports() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scm.db");
        drop(SqliteScmStore::open(&path).unwrap());
        // Roll the cursor back one version so the next open has a pending
        // migration (the tables are already there; the transition is the
        // crash point under test).
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA user_version = 1").unwrap();
        }
        inject_crash_before_migration(&path);
        let err = SqliteScmStore::open(&path)
            .err()
            .expect("the injected crash must fail the open");
        assert!(
            err.to_string().contains("injected crash"),
            "the failure is the injected crash: {err}"
        );
        {
            let conn = Connection::open(&path).unwrap();
            let version: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(version, 1, "no migration ran without its restore point");
        }
        let report = faktor_cloud::durability::doctor_probe(&path, false).unwrap();
        let (point, _) = report
            .migration_restore_point
            .expect("doctor reports the pre-migration restore point");
        assert!(
            point
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .contains("-pre-migration-v1-"),
            "the restore point names the version it protects: {}",
            point.display()
        );
        // The restore point is a real v1 database, not a partial copy.
        let backup =
            Connection::open_with_flags(&point, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        let version: i64 = backup
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 1);
        assert!(faktor_cloud::durability::integrity_check(&backup, true)
            .unwrap()
            .is_empty());
        // A clean re-open completes the migration and records the policy.
        drop(SqliteScmStore::open(&path).unwrap());
        let report = faktor_cloud::durability::doctor_probe(&path, false).unwrap();
        assert!(report.policy.is_some());
    }

    /// Adversarial: when the pre-migration restore point CANNOT be written,
    /// the migration is refused and the database stays at the old version —
    /// never a schema change without a way back.
    #[test]
    fn migration_is_refused_when_the_restore_point_cannot_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scm.db");
        drop(SqliteScmStore::open(&path).unwrap());
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA user_version = 1").unwrap();
        }
        // Block the backup directory: a regular file where the directory must
        // be makes every restore-point write impossible.
        let blocked = faktor_cloud::durability::backup_dir(&path);
        let _ = std::fs::remove_dir_all(&blocked);
        std::fs::write(&blocked, b"not a directory").unwrap();
        let err = SqliteScmStore::open(&path)
            .err()
            .expect("a migration without its restore point must be refused");
        assert!(
            err.to_string()
                .contains("refusing migration without a verified pre-migration restore point"),
            "the refusal is typed and names the gate: {err}"
        );
        {
            let conn = Connection::open(&path).unwrap();
            let version: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(version, 1, "the refused migration never ran");
        }
        // Unblock: the same open now migrates and records the policy.
        std::fs::remove_file(&blocked).unwrap();
        drop(SqliteScmStore::open(&path).unwrap());
        let report = faktor_cloud::durability::doctor_probe(&path, false).unwrap();
        assert!(report.policy.is_some());
    }

    /// The kill proof: the child opens the SCM database, records an
    /// acknowledged webhook claim, prints the ACK + writer pragma, and hangs;
    /// the parent SIGKILLs it and reopens. The acknowledged claim must be
    /// there (a redelivery is a durable Duplicate, not a Fresh claim).
    #[test]
    fn acknowledged_webhook_claim_survives_sigkill_and_reopen() {
        const CHILD_ENV: &str = "FAKTOR_SCM_DURABILITY_CHILD_DB";
        if let Ok(db_path) = std::env::var(CHILD_ENV) {
            child_acknowledged_webhook_claim(&db_path);
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scm.db");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("store::tests::acknowledged_webhook_claim_survives_sigkill_and_reopen")
            .arg("--nocapture")
            .env(CHILD_ENV, &path)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut lines = std::io::BufRead::lines(std::io::BufReader::new(stdout));
        let mut ack = None;
        while let Some(Ok(line)) = lines.next() {
            if let Some(rest) = line.strip_prefix("ACK ") {
                ack = Some(rest.to_string());
                break;
            }
        }
        child.kill().unwrap();
        let _ = child.wait();
        let ack = ack.expect("the child acknowledges its webhook claim");
        assert!(
            ack.contains("SYNC=2"),
            "the writer connection was FULL at acknowledgement: {ack}"
        );
        let store = SqliteScmStore::open(&path).unwrap();
        assert_eq!(
            store
                .claim_webhook_delivery("kill-delivery", "push", 7)
                .unwrap(),
            DeliveryClaim::Duplicate { first_seen_ms: 7 },
            "the acknowledged claim survived the kill"
        );
    }

    /// The child body of the kill test. Never returns: the parent SIGKILLs it
    /// after the ACK (the bounded loop is only the fail-safe).
    fn child_acknowledged_webhook_claim(db_path: &str) -> ! {
        let store = SqliteScmStore::open(Path::new(db_path)).unwrap();
        let sync: i64 = {
            let conn = store.lock().unwrap();
            conn.query_row("PRAGMA synchronous", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(
            store
                .claim_webhook_delivery("kill-delivery", "push", 7)
                .unwrap(),
            DeliveryClaim::Fresh
        );
        println!("ACK SYNC={sync}");
        loop {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

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

    /// Every migration restore point this database owns, with the version
    /// its NAME claims and the version its CONTENT holds (they must agree).
    fn migration_points(db_path: &Path) -> Vec<(String, i64, i64)> {
        faktor_cloud::durability::list_backups(db_path)
            .into_iter()
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .contains(faktor_cloud::durability::MIGRATION_MARKER)
            })
            .map(|p| {
                let name = p.file_name().unwrap().to_str().unwrap().to_string();
                let claimed: i64 = name
                    .rsplit_once(faktor_cloud::durability::MIGRATION_MARKER)
                    .and_then(|(_, rest)| {
                        rest.chars()
                            .take_while(|c| c.is_ascii_digit())
                            .collect::<String>()
                            .parse()
                            .ok()
                    })
                    .expect("a migration point carries a parseable version");
                let conn =
                    Connection::open_with_flags(&p, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                        .unwrap();
                let content: i64 = conn
                    .query_row("PRAGMA user_version", [], |r| r.get(0))
                    .unwrap();
                (name, claimed, content)
            })
            .collect()
    }

    /// P1 downgrade protection: a database written by a NEWER schema ladder
    /// is refused typed (naming both versions) and left untouched — no writes
    /// and no restore-point snapshot of a state this binary cannot honour.
    #[test]
    fn opening_a_newer_schema_is_refused_typed_without_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scm.db");
        let newer = SCM_MIGRATIONS.len() as i64 + 1;
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(&format!("PRAGMA user_version = {newer}"))
                .unwrap();
        }
        let err = SqliteScmStore::open(&path)
            .err()
            .expect("a newer schema must be refused");
        match &err {
            ScmStoreError::UnsupportedSchema {
                found,
                maximum_supported,
            } => {
                assert_eq!(*found, newer);
                assert_eq!(*maximum_supported, SCM_MIGRATIONS.len() as i64);
            }
            other => panic!("downgrade must be refused typed, got {other:?}"),
        }
        let message = err.to_string();
        assert!(
            message.contains("newer than this binary's ladder"),
            "{message}"
        );
        assert!(message.contains("downgrade refused"), "{message}");
        assert!(message.contains(&format!("v{newer}")), "{message}");
        assert!(
            message.contains(&format!("v{}", SCM_MIGRATIONS.len())),
            "{message}"
        );
        let conn = Connection::open(&path).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, newer, "the refused open changed nothing");
        drop(conn);
        assert!(
            migration_points(&path).is_empty(),
            "a newer schema is never snapshotted as a trusted way back"
        );
    }

    /// P1 corruption protection: a NEGATIVE `user_version` cannot be produced
    /// by any legitimate open (SQLite stores the cursor as a signed integer).
    /// It is refused typed BEFORE the restore point and the ladder, so no
    /// snapshot labeled `-pre-migration-v-1-` is ever written and the database
    /// is left unchanged; 0 and the ladder version still open as before.
    #[test]
    fn negative_schema_version_is_refused_typed_without_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scm.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA user_version = -1").unwrap();
        }
        let before = {
            let conn = Connection::open(&path).unwrap();
            faktor_cloud::durability::canonical_fingerprint(&conn).unwrap()
        };
        let err = SqliteScmStore::open(&path)
            .err()
            .expect("a negative schema version must be refused");
        match &err {
            ScmStoreError::Malformed(message) => {
                assert!(
                    message.contains("-1"),
                    "the refusal names the found value: {message}"
                );
                assert!(message.contains("corrupt"), "{message}");
            }
            other => panic!("a negative schema version must be refused typed, got {other:?}"),
        }
        assert!(
            migration_points(&path).is_empty(),
            "the refused open writes no restore point"
        );
        assert!(
            faktor_cloud::durability::list_backups(&path).is_empty(),
            "the refused open writes no backup at all"
        );
        {
            let conn = Connection::open(&path).unwrap();
            let version: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(version, -1, "the refused open changed nothing");
            assert_eq!(
                faktor_cloud::durability::canonical_fingerprint(&conn).unwrap(),
                before,
                "the refused open left the database unchanged"
            );
        }
        // A clean cursor still opens: 0 migrates the whole ladder and the
        // ladder version reopens as a no-op (no new restore point).
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA user_version = 0").unwrap();
        }
        drop(SqliteScmStore::open(&path).expect("0 still migrates"));
        {
            let conn = Connection::open(&path).unwrap();
            let version: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(version, SCM_MIGRATIONS.len() as i64);
        }
        let points = migration_points(&path);
        assert_eq!(points.len(), 1, "0 -> ladder writes its v0 restore point");
        drop(SqliteScmStore::open(&path).expect("the ladder still reopens"));
        assert_eq!(
            migration_points(&path),
            points,
            "an at-ladder reopen writes no new restore point"
        );
    }

    /// A database already AT this binary's ladder opens; re-opening neither
    /// migrates nor writes another restore point (the ladder is a no-op).
    #[test]
    fn a_database_at_the_ladder_reopens_without_a_new_restore_point() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scm.db");
        drop(SqliteScmStore::open(&path).unwrap());
        let before = migration_points(&path);
        let store = SqliteScmStore::open(&path).unwrap();
        let version: i64 = store
            .lock()
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCM_MIGRATIONS.len() as i64);
        assert_eq!(
            migration_points(&path),
            before,
            "an at-ladder reopen writes no new restore point"
        );
    }

    /// Adversarial concurrency: two openers race the migration. One applies
    /// the ladder and snapshots the TRUE predecessor (v1); the loser blocks
    /// on the write lock, re-reads the advanced version inside its own
    /// transaction and skips — so exactly one point is labeled v1 and every
    /// point's name claim matches its content.
    #[test]
    fn concurrent_openers_serialize_the_migration_and_label_one_restore_point() {
        use std::sync::Barrier;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scm.db");
        drop(SqliteScmStore::open(&path).unwrap());
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA user_version = 1").unwrap();
        }
        let barrier = std::sync::Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let path = path.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                SqliteScmStore::open(&path).map(|_| ())
            }));
        }
        for handle in handles {
            handle
                .join()
                .unwrap()
                .expect("both concurrent openers must succeed");
        }
        {
            let conn = Connection::open(&path).unwrap();
            let version: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(version, SCM_MIGRATIONS.len() as i64, "the ladder applied");
        }
        let points = migration_points(&path);
        for (name, claimed, content) in &points {
            assert_eq!(
                claimed, content,
                "{name} claims v{claimed} but holds v{content}"
            );
        }
        let v1: Vec<&(String, i64, i64)> = points
            .iter()
            .filter(|(n, _, _)| n.contains("-pre-migration-v1-"))
            .collect();
        assert_eq!(
            v1.len(),
            1,
            "exactly one opener snapshotted the v1 predecessor: {points:?}"
        );
        assert_eq!(v1[0].2, 1, "the v1 point holds the true predecessor state");
    }
}
