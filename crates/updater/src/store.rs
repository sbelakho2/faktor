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
//!
//! The same file also owns the durable per-channel ANTI-ROLLBACK high-water
//! mark ([`HighWaterMark`]). It is deliberately stored here — OUTSIDE the
//! install root — so replacing an installed version, its `current` pointer
//! or the content-addressed payload can never lower it. [`UpdaterStore::raise_high_water`]
//! is monotonic (`max(existing, generation)`, with the legacy-consumed flag
//! sticky); [`UpdaterStore::set_high_water`] is the single lowering path and
//! exists only for an explicitly authorized, audited downgrade.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

/// Bound on one listing page.
pub const MAX_LIST: usize = 200;
/// Bound on one high-water channel key.
pub const MAX_CHANNEL_BYTES: usize = 32;

/// Typed store failures.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UpdateStoreError {
    #[error("update store row is malformed: {0}")]
    Malformed(String),
    #[error("update store unavailable: {0}")]
    Backend(String),
    /// A database written by a NEWER binary's schema ladder must never be
    /// silently opened by an older one: the newer ladder may have changed
    /// semantics this binary cannot honour. Downgrade is refused typed; the
    /// recovery path is running the newer binary or restoring the
    /// pre-upgrade restore point.
    #[error(
        "update store schema v{found} is newer than this binary's ladder v{maximum_supported}: \
         downgrade refused (run the newer binary or restore the pre-upgrade restore point)"
    )]
    UnsupportedSchema { found: i64, maximum_supported: i64 },
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

/// The four lifecycle steps, plus the explicitly authorized downgrade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateOpKind {
    Check,
    Stage,
    Apply,
    Rollback,
    /// The separately authorized (admin) downgrade below the anti-rollback
    /// floor. Recorded as its own durable, audited row.
    Downgrade,
}

impl UpdateOpKind {
    pub const ALL: &'static [UpdateOpKind] = &[
        UpdateOpKind::Check,
        UpdateOpKind::Stage,
        UpdateOpKind::Apply,
        UpdateOpKind::Rollback,
        UpdateOpKind::Downgrade,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            UpdateOpKind::Check => "check",
            UpdateOpKind::Stage => "stage",
            UpdateOpKind::Apply => "apply",
            UpdateOpKind::Rollback => "rollback",
            UpdateOpKind::Downgrade => "downgrade",
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
    /// The signed release generation the manifest that produced this
    /// operation carried (`None` = legacy manifest / pre-anti-rollback row,
    /// treated as generation 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_generation: Option<u64>,
    /// The control-plane actor that explicitly authorized the operation
    /// (only the authorized downgrade carries one today); bounded text, never
    /// a secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
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
            release_generation: None,
            actor: None,
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

/// The durable per-channel anti-rollback high-water mark. Persisted in the
/// updater store (whose file lives outside the install root and is never part
/// of the installed payload): replacing a version, its pointer or its
/// artifacts cannot lower it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HighWaterMark {
    pub channel: String,
    pub generation: u64,
    /// Sticky once the one-time legacy-manifest allowance has been consumed.
    pub legacy_consumed: bool,
    pub updated_ms: i64,
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
    /// The durable anti-rollback high-water mark of one channel, if any.
    fn high_water(&self, channel: &str) -> Result<Option<HighWaterMark>, UpdateStoreError>;
    /// Monotonically RAISE the mark: `generation = max(existing, generation)`
    /// and `legacy_consumed |= legacy_consumed`. Never lowers; returns the
    /// durable row that resulted.
    fn raise_high_water(
        &self,
        channel: &str,
        generation: u64,
        legacy_consumed: bool,
        now_ms: i64,
    ) -> Result<HighWaterMark, UpdateStoreError>;
    /// Explicitly SET the mark to `generation` — the SINGLE lowering path,
    /// used only by an authorized, audited downgrade (and only after it
    /// succeeded). `legacy_consumed` stays sticky.
    fn set_high_water(
        &self,
        channel: &str,
        generation: u64,
        now_ms: i64,
    ) -> Result<HighWaterMark, UpdateStoreError>;
}

fn bounded(limit: usize) -> usize {
    limit.clamp(1, MAX_LIST)
}

fn validate_channel(channel: &str) -> Result<(), UpdateStoreError> {
    if channel.is_empty() || channel.len() > MAX_CHANNEL_BYTES || !channel.is_ascii() {
        return Err(UpdateStoreError::Malformed(format!(
            "high-water channel must be 1..={MAX_CHANNEL_BYTES} ASCII bytes"
        )));
    }
    Ok(())
}

fn generation_to_i64(generation: u64) -> Result<i64, UpdateStoreError> {
    i64::try_from(generation).map_err(|_| {
        UpdateStoreError::Malformed(format!(
            "high-water generation {generation} exceeds the durable integer range"
        ))
    })
}

fn row_generation(channel: &str, generation: i64) -> Result<u64, UpdateStoreError> {
    u64::try_from(generation).map_err(|_| {
        UpdateStoreError::Malformed(format!(
            "high-water row for channel {channel:?} carries a negative generation; refusing to guess"
        ))
    })
}

/// In-memory store (tests + embedded hosts without a data dir).
#[derive(Debug, Default)]
pub struct MemoryUpdaterStore {
    rows: Mutex<Vec<UpdateOperation>>,
    high_water: Mutex<BTreeMap<String, HighWaterMark>>,
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

    fn lock_high_water(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, HighWaterMark>>, UpdateStoreError> {
        self.high_water
            .lock()
            .map_err(|_| UpdateStoreError::Backend("high-water lock is poisoned".into()))
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

    fn high_water(&self, channel: &str) -> Result<Option<HighWaterMark>, UpdateStoreError> {
        validate_channel(channel)?;
        Ok(self.lock_high_water()?.get(channel).cloned())
    }

    fn raise_high_water(
        &self,
        channel: &str,
        generation: u64,
        legacy_consumed: bool,
        now_ms: i64,
    ) -> Result<HighWaterMark, UpdateStoreError> {
        validate_channel(channel)?;
        let mut marks = self.lock_high_water()?;
        let mark = marks.entry(channel.to_string()).or_insert(HighWaterMark {
            channel: channel.to_string(),
            generation: 0,
            legacy_consumed: false,
            updated_ms: now_ms,
        });
        mark.generation = mark.generation.max(generation);
        mark.legacy_consumed |= legacy_consumed;
        mark.updated_ms = now_ms;
        Ok(mark.clone())
    }

    fn set_high_water(
        &self,
        channel: &str,
        generation: u64,
        now_ms: i64,
    ) -> Result<HighWaterMark, UpdateStoreError> {
        validate_channel(channel)?;
        let mut marks = self.lock_high_water()?;
        let mark = marks.entry(channel.to_string()).or_insert(HighWaterMark {
            channel: channel.to_string(),
            generation: 0,
            legacy_consumed: false,
            updated_ms: now_ms,
        });
        mark.generation = generation;
        mark.updated_ms = now_ms;
        Ok(mark.clone())
    }
}

/// SQLite-backed durable store (the daemon's `update.db`).
pub struct SqliteUpdaterStore {
    conn: Mutex<rusqlite::Connection>,
}

const UPDATER_MIGRATIONS: &[&str] = &[
    "
CREATE TABLE IF NOT EXISTS update_operation (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    status TEXT NOT NULL,
    created_ms INTEGER NOT NULL,
    payload TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS update_operation_status ON update_operation (status);
CREATE INDEX IF NOT EXISTS update_operation_kind_status ON update_operation (kind, status);
",
    // v2 — the anti-rollback high-water marks (per channel, monotonic).
    // The updater database lives OUTSIDE the install root, so this durable
    // floor is never replaced together with an installed version.
    "
CREATE TABLE IF NOT EXISTS update_high_water (
    channel TEXT PRIMARY KEY,
    generation INTEGER NOT NULL,
    legacy_consumed INTEGER NOT NULL DEFAULT 0,
    updated_ms INTEGER NOT NULL
);
",
    // v3 — the durability policy marker (P1 audit): the writer RECORDS the
    // acknowledged-durability policy (`synchronous = FULL`) so `doctor` can
    // observe it (SQLite pragmas are connection-scoped and invisible to a
    // separate probe connection). Owned by the durability stack
    // (`faktor_cloud::durability`), the same marker cloud records.
    faktor_cloud::durability::POLICY_SCHEMA_V5,
];

impl SqliteUpdaterStore {
    /// Open (creating) the updater database at `path`.
    ///
    /// Durability policy (P1 audit; the [`faktor_cloud::durability`] stack):
    /// the writer connection opens `synchronous = FULL`, file-backed opens
    /// record the policy marker, write a VERIFIED pre-migration restore point
    /// before any schema transition away from an existing version, and run
    /// the interval-gated rotating backup.
    pub fn open(path: &Path) -> Result<Self, UpdateStoreError> {
        let conn = rusqlite::Connection::open(path).map_err(backend)?;
        Self::prepare(conn, Some(path))
    }

    /// Open an in-memory database (tests, ephemeral hosts).
    pub fn open_in_memory() -> Result<Self, UpdateStoreError> {
        let conn = rusqlite::Connection::open_in_memory().map_err(backend)?;
        Self::prepare(conn, None)
    }

    fn prepare(conn: rusqlite::Connection, path: Option<&Path>) -> Result<Self, UpdateStoreError> {
        // The acknowledged-durability policy (WAL + synchronous = FULL; see
        // `faktor_cloud::durability` for the documented choice) applies to
        // EVERY open, in-memory included, before any migration or query. The
        // updater database holds the signed-release high-water floor
        // (anti-rollback), so the writer connection acknowledges commits only
        // after an fsync.
        faktor_cloud::durability::apply_policy(&conn).map_err(durability)?;
        let mut conn = conn;
        migrate(&mut conn, path)?;
        if let Some(path) = path {
            let now = faktor_cloud::durability::now_ms();
            // Record the writer's policy for `doctor` (best effort: a full
            // disk must not take the updater down; doctor then reports the
            // absent/stale marker loudly).
            if let Err(e) = faktor_cloud::durability::record_open_policy(&conn, now) {
                tracing::error!("updater durability marker not recorded: {e}");
            }
            // Interval-gated verified backup. Best effort, like the daemon's
            // startup backup.
            match faktor_cloud::durability::rotate_backup(&conn, path) {
                Ok(Some(dest)) => {
                    tracing::info!("updater backup written to {}", dest.display());
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("updater backup skipped: {e}"),
            }
        }
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

fn durability(e: faktor_cloud::CloudStoreError) -> UpdateStoreError {
    UpdateStoreError::Backend(e.to_string())
}

/// Apply the updater schema ladder. The version read, the pre-migration
/// restore point and every migration statement run inside ONE `BEGIN
/// IMMEDIATE` transaction: a second concurrent opener blocks on the write
/// lock, then re-reads the (already advanced) version inside its own
/// transaction and skips — it can never snapshot post-migration content and
/// label it `-pre-migration-vN-`.
///
/// A database written by a NEWER binary (`user_version` above this binary's
/// ladder) is refused typed, before any snapshot or write; a NEGATIVE
/// `user_version` (impossible for a database this ladder created) is refused
/// typed as corruption, before any snapshot or write.
fn migrate(
    conn: &mut rusqlite::Connection,
    db_path: Option<&Path>,
) -> Result<(), UpdateStoreError> {
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(backend)?;
    let started: i64 = tx
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(backend)?;
    let ladder = UPDATER_MIGRATIONS.len() as i64;
    // A NEGATIVE `user_version` cannot have been produced by any legitimate
    // open of this ladder (SQLite stores the pragma as a SIGNED integer and
    // every writer here only ever moves it forward from 0). It is durable
    // corruption, and it is refused typed BEFORE the restore point and the
    // ladder: a snapshot labeled `-pre-migration-v-1-` would be a
    // trusted-looking way back to a state this binary never created, and
    // treating it as v0 would silently migrate corrupt state.
    if started < 0 {
        return Err(UpdateStoreError::Malformed(format!(
            "update store schema user_version {started} is corrupt (negative): \
             refusing to snapshot or migrate it"
        )));
    }
    if started > ladder {
        return Err(UpdateStoreError::UnsupportedSchema {
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
        let reader =
            rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(backend)?;
        faktor_cloud::durability::migration_backup(&reader, path, version).map_err(|e| {
            UpdateStoreError::Backend(format!(
                "refusing migration without a verified pre-migration restore point: {e}"
            ))
        })?;
        drop(reader);
        #[cfg(test)]
        if take_injected_crash(path) {
            return Err(UpdateStoreError::Backend(
                "injected crash after the pre-migration restore point".into(),
            ));
        }
    }
    for (i, sql) in UPDATER_MIGRATIONS.iter().enumerate() {
        let target = (i + 1) as i64;
        if version >= target {
            continue;
        }
        tx.execute_batch(sql)
            .map_err(|e| UpdateStoreError::Backend(format!("updater migration v{target}: {e}")))?;
        tx.execute_batch(&format!("PRAGMA user_version = {target}"))
            .map_err(|e| UpdateStoreError::Backend(format!("updater migration v{target}: {e}")))?;
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

    fn high_water(&self, channel: &str) -> Result<Option<HighWaterMark>, UpdateStoreError> {
        validate_channel(channel)?;
        let conn = self.lock()?;
        let row: Option<(i64, i64, i64)> = conn
            .query_row(
                "SELECT generation, legacy_consumed, updated_ms
                 FROM update_high_water WHERE channel = ?1",
                rusqlite::params![channel],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(backend)?;
        row.map(|(generation, legacy_consumed, updated_ms)| {
            Ok(HighWaterMark {
                channel: channel.to_string(),
                generation: row_generation(channel, generation)?,
                legacy_consumed: legacy_consumed != 0,
                updated_ms,
            })
        })
        .transpose()
    }

    fn raise_high_water(
        &self,
        channel: &str,
        generation: u64,
        legacy_consumed: bool,
        now_ms: i64,
    ) -> Result<HighWaterMark, UpdateStoreError> {
        validate_channel(channel)?;
        let generation = generation_to_i64(generation)?;
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO update_high_water (channel, generation, legacy_consumed, updated_ms)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(channel) DO UPDATE SET
                 generation = MAX(update_high_water.generation, excluded.generation),
                 legacy_consumed = MAX(update_high_water.legacy_consumed, excluded.legacy_consumed),
                 updated_ms = excluded.updated_ms",
            rusqlite::params![channel, generation, legacy_consumed as i64, now_ms],
        )
        .map_err(backend)?;
        drop(conn);
        self.high_water(channel)?.ok_or_else(|| {
            UpdateStoreError::Backend("high-water row vanished after the raise".into())
        })
    }

    fn set_high_water(
        &self,
        channel: &str,
        generation: u64,
        now_ms: i64,
    ) -> Result<HighWaterMark, UpdateStoreError> {
        validate_channel(channel)?;
        let generation = generation_to_i64(generation)?;
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO update_high_water (channel, generation, legacy_consumed, updated_ms)
             VALUES (?1, ?2, 0, ?3)
             ON CONFLICT(channel) DO UPDATE SET
                 generation = excluded.generation,
                 updated_ms = excluded.updated_ms",
            rusqlite::params![channel, generation, now_ms],
        )
        .map_err(backend)?;
        drop(conn);
        self.high_water(channel)?.ok_or_else(|| {
            UpdateStoreError::Backend("high-water row vanished after the set".into())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P1 durability: the updater database carries the anti-rollback
    /// high-water floor, so its writer connection acknowledges a commit only
    /// after an fsync (`synchronous = FULL`): a crash/power loss must not roll
    /// back a raised release generation.
    #[test]
    fn commercial_writer_connection_is_synchronous_full() {
        let store = SqliteUpdaterStore::open_in_memory().unwrap();
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
        let path = dir.path().join("update.db");
        drop(SqliteUpdaterStore::open(&path).unwrap());
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
            let conn = rusqlite::Connection::open(&path).unwrap();
            faktor_cloud::durability::canonical_fingerprint(&conn).unwrap()
        };
        faktor_cloud::durability::restore_verify(&backup, &expected).unwrap();
    }

    /// Adversarial: more verified snapshots than the retention bound. The
    /// rotation must bound the kept set, and the newest must still verify.
    #[test]
    fn verified_backups_rotate_within_both_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update.db");
        let store = SqliteUpdaterStore::open(&path).unwrap();
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
        faktor_cloud::durability::migration_backup(&conn, &path, 3).unwrap();
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
        let path = dir.path().join("update.db");
        drop(SqliteUpdaterStore::open(&path).unwrap());
        // Roll the cursor back one version so the next open has a pending
        // migration (the tables are already there; the transition is the
        // crash point under test).
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA user_version = 2").unwrap();
        }
        inject_crash_before_migration(&path);
        let err = SqliteUpdaterStore::open(&path)
            .err()
            .expect("the injected crash must fail the open");
        assert!(
            err.to_string().contains("injected crash"),
            "the failure is the injected crash: {err}"
        );
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            let version: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(version, 2, "no migration ran without its restore point");
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
                .contains("-pre-migration-v2-"),
            "the restore point names the version it protects: {}",
            point.display()
        );
        // The restore point is a real v2 database, not a partial copy.
        let backup = rusqlite::Connection::open_with_flags(
            &point,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let version: i64 = backup
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 2);
        assert!(faktor_cloud::durability::integrity_check(&backup, true)
            .unwrap()
            .is_empty());
        // A clean re-open completes the migration and records the policy.
        drop(SqliteUpdaterStore::open(&path).unwrap());
        let report = faktor_cloud::durability::doctor_probe(&path, false).unwrap();
        assert!(report.policy.is_some());
    }

    /// Adversarial: when the pre-migration restore point CANNOT be written,
    /// the migration is refused and the database stays at the old version —
    /// never a schema change without a way back.
    #[test]
    fn migration_is_refused_when_the_restore_point_cannot_be_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update.db");
        drop(SqliteUpdaterStore::open(&path).unwrap());
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA user_version = 2").unwrap();
        }
        // Block the backup directory: a regular file where the directory must
        // be makes every restore-point write impossible.
        let blocked = faktor_cloud::durability::backup_dir(&path);
        let _ = std::fs::remove_dir_all(&blocked);
        std::fs::write(&blocked, b"not a directory").unwrap();
        let err = SqliteUpdaterStore::open(&path)
            .err()
            .expect("a migration without its restore point must be refused");
        assert!(
            err.to_string()
                .contains("refusing migration without a verified pre-migration restore point"),
            "the refusal is typed and names the gate: {err}"
        );
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            let version: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(version, 2, "the refused migration never ran");
        }
        // Unblock: the same open now migrates and records the policy.
        std::fs::remove_file(&blocked).unwrap();
        drop(SqliteUpdaterStore::open(&path).unwrap());
        let report = faktor_cloud::durability::doctor_probe(&path, false).unwrap();
        assert!(report.policy.is_some());
    }

    /// The kill proof: the child opens the updater database, records an
    /// acknowledged high-water raise (the anti-rollback floor), prints the
    /// ACK + writer pragma, and hangs; the parent SIGKILLs it and reopens.
    /// The acknowledged floor must be there.
    #[test]
    fn acknowledged_high_water_raise_survives_sigkill_and_reopen() {
        const CHILD_ENV: &str = "FAKTOR_UPDATER_DURABILITY_CHILD_DB";
        if let Ok(db_path) = std::env::var(CHILD_ENV) {
            child_acknowledged_high_water_raise(&db_path);
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update.db");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("store::tests::acknowledged_high_water_raise_survives_sigkill_and_reopen")
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
        let ack = ack.expect("the child acknowledges its high-water raise");
        assert!(
            ack.contains("SYNC=2"),
            "the writer connection was FULL at acknowledgement: {ack}"
        );
        let store = SqliteUpdaterStore::open(&path).unwrap();
        let mark = store
            .high_water("stable")
            .unwrap()
            .expect("the acknowledged high-water floor survived the kill");
        assert_eq!(mark.generation, 7);
    }

    /// The child body of the kill test. Never returns: the parent SIGKILLs it
    /// after the ACK (the bounded loop is only the fail-safe).
    fn child_acknowledged_high_water_raise(db_path: &str) -> ! {
        let store = SqliteUpdaterStore::open(Path::new(db_path)).unwrap();
        let sync: i64 = {
            let conn = store.lock().unwrap();
            conn.query_row("PRAGMA synchronous", [], |r| r.get(0))
                .unwrap()
        };
        let mark = store.raise_high_water("stable", 7, false, 100).unwrap();
        assert_eq!(mark.generation, 7);
        println!("ACK SYNC={sync}");
        loop {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

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

        // Anti-rollback high-water marks: absent until admitted, monotonic on
        // raise, sticky on the legacy flag, and the explicit set is the only
        // lowering path.
        assert_eq!(store.high_water("stable").unwrap(), None);
        let raised = store.raise_high_water("stable", 5, false, 100).unwrap();
        assert_eq!(raised.generation, 5);
        assert!(!raised.legacy_consumed);
        let held = store.raise_high_water("stable", 3, true, 101).unwrap();
        assert_eq!(held.generation, 5, "raise must never lower the mark");
        assert!(held.legacy_consumed, "the legacy flag is sticky");
        let lowered = store.set_high_water("stable", 2, 102).unwrap();
        assert_eq!(
            lowered.generation, 2,
            "the explicit set is the lowering path"
        );
        assert!(
            lowered.legacy_consumed,
            "an explicit downgrade never revives the legacy allowance"
        );
        assert_eq!(store.high_water("beta").unwrap(), None);
        assert!(store.high_water("").is_err());
        assert!(
            store
                .high_water(&"x".repeat(MAX_CHANNEL_BYTES + 1))
                .is_err(),
            "an oversized channel key is a typed refusal"
        );
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
        // The durable floor survives the reopen: `raise` left it at 5 even
        // though a later `set` lowered it to 2, and the legacy flag is
        // sticky across connections.
        let mark = reopened.high_water("stable").unwrap().unwrap();
        assert_eq!(mark.generation, 2);
        assert!(mark.legacy_consumed);
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
                let conn = rusqlite::Connection::open_with_flags(
                    &p,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                )
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
        let path = dir.path().join("update.db");
        let newer = UPDATER_MIGRATIONS.len() as i64 + 1;
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(&format!("PRAGMA user_version = {newer}"))
                .unwrap();
        }
        let err = SqliteUpdaterStore::open(&path)
            .err()
            .expect("a newer schema must be refused");
        match &err {
            UpdateStoreError::UnsupportedSchema {
                found,
                maximum_supported,
            } => {
                assert_eq!(*found, newer);
                assert_eq!(*maximum_supported, UPDATER_MIGRATIONS.len() as i64);
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
            message.contains(&format!("v{}", UPDATER_MIGRATIONS.len())),
            "{message}"
        );
        let conn = rusqlite::Connection::open(&path).unwrap();
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
        let path = dir.path().join("update.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA user_version = -1").unwrap();
        }
        let before = {
            let conn = rusqlite::Connection::open(&path).unwrap();
            faktor_cloud::durability::canonical_fingerprint(&conn).unwrap()
        };
        let err = SqliteUpdaterStore::open(&path)
            .err()
            .expect("a negative schema version must be refused");
        match &err {
            UpdateStoreError::Malformed(message) => {
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
            let conn = rusqlite::Connection::open(&path).unwrap();
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
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA user_version = 0").unwrap();
        }
        drop(SqliteUpdaterStore::open(&path).expect("0 still migrates"));
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            let version: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(version, UPDATER_MIGRATIONS.len() as i64);
        }
        let points = migration_points(&path);
        assert_eq!(points.len(), 1, "0 -> ladder writes its v0 restore point");
        drop(SqliteUpdaterStore::open(&path).expect("the ladder still reopens"));
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
        let path = dir.path().join("update.db");
        drop(SqliteUpdaterStore::open(&path).unwrap());
        let before = migration_points(&path);
        let store = SqliteUpdaterStore::open(&path).unwrap();
        let version: i64 = store
            .lock()
            .unwrap()
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, UPDATER_MIGRATIONS.len() as i64);
        assert_eq!(
            migration_points(&path),
            before,
            "an at-ladder reopen writes no new restore point"
        );
    }

    /// Adversarial concurrency: two openers race the migration. One applies
    /// the ladder and snapshots the TRUE predecessor (v2); the loser blocks
    /// on the write lock, re-reads the advanced version inside its own
    /// transaction and skips — so exactly one point is labeled v2 and every
    /// point's name claim matches its content.
    #[test]
    fn concurrent_openers_serialize_the_migration_and_label_one_restore_point() {
        use std::sync::Barrier;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update.db");
        drop(SqliteUpdaterStore::open(&path).unwrap());
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA user_version = 2").unwrap();
        }
        let barrier = std::sync::Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let path = path.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                SqliteUpdaterStore::open(&path).map(|_| ())
            }));
        }
        for handle in handles {
            handle
                .join()
                .unwrap()
                .expect("both concurrent openers must succeed");
        }
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            let version: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(
                version,
                UPDATER_MIGRATIONS.len() as i64,
                "the ladder applied"
            );
        }
        let points = migration_points(&path);
        for (name, claimed, content) in &points {
            assert_eq!(
                claimed, content,
                "{name} claims v{claimed} but holds v{content}"
            );
        }
        let v2: Vec<&(String, i64, i64)> = points
            .iter()
            .filter(|(n, _, _)| n.contains("-pre-migration-v2-"))
            .collect();
        assert_eq!(
            v2.len(),
            1,
            "exactly one opener snapshotted the v2 predecessor: {points:?}"
        );
        assert_eq!(v2[0].2, 2, "the v2 point holds the true predecessor state");
    }
}
