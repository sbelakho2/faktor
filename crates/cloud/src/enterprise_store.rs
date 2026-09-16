//! The durable enterprise seam: the [`EnterpriseStore`] trait plus its
//! in-memory implementation and the additive SQLite implementation over the
//! SAME [`crate::store::SqliteControlPlaneStore`] database and migration
//! ladder (its migration v3 — the crate-owned next `user_version`).
//!
//! Append-only enforcement lives here: the audit ledger is INSERT-only (it
//! has no UPDATE/DELETE surface), and an append is idempotent per derived
//! event key (`UNIQUE (organization_id, event_key)`), so a retried mutation
//! can never fork the ledger. Artifact rows are the ONE mutable enterprise
//! table — they carry the deletion state machine.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::enterprise::{
    ArtifactRecord, AuditEvent, DeletionJob, NewAuditEvent, OrgSettings, Tombstone,
};
use crate::ids::{ArtifactId, DeletionJobId, OrganizationId};
use crate::layered_config::ConfigLayer;
use crate::store::{CloudStoreError, MemoryControlPlaneStore, SqliteControlPlaneStore};

/// The SQL schema of the enterprise domain (migration v3 of the control-plane
/// ladder). The audit table is append-only by construction: no UPDATE/DELETE
/// statement names `ent_audit_event` anywhere in this crate.
pub const ENTERPRISE_SCHEMA_V3: &str = "
     CREATE TABLE IF NOT EXISTS ent_audit_event (
        seq INTEGER PRIMARY KEY AUTOINCREMENT,
        organization_id TEXT NOT NULL,
        event_key TEXT NOT NULL,
        action TEXT NOT NULL,
        object TEXT NOT NULL,
        occurred_at_ms INTEGER NOT NULL,
        payload TEXT NOT NULL,
        UNIQUE (organization_id, event_key)
     );
     CREATE INDEX IF NOT EXISTS idx_ent_audit_org_seq
        ON ent_audit_event(organization_id, seq);
     CREATE TABLE IF NOT EXISTS ent_artifact (
        id TEXT PRIMARY KEY,
        organization_id TEXT NOT NULL,
        digest TEXT NOT NULL,
        deletion_state TEXT NOT NULL,
        expires_at_ms INTEGER,
        payload TEXT NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_ent_artifact_org
        ON ent_artifact(organization_id, id);
     CREATE INDEX IF NOT EXISTS idx_ent_artifact_org_digest
        ON ent_artifact(organization_id, digest);
     CREATE TABLE IF NOT EXISTS ent_deletion_job (
        id TEXT PRIMARY KEY,
        organization_id TEXT NOT NULL,
        state TEXT NOT NULL,
        payload TEXT NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_ent_deletion_job_org
        ON ent_deletion_job(organization_id, id);
     CREATE TABLE IF NOT EXISTS ent_org_settings (
        organization_id TEXT PRIMARY KEY,
        revision INTEGER NOT NULL,
        payload TEXT NOT NULL
     );
     CREATE TABLE IF NOT EXISTS ent_tombstone (
        scope_key TEXT PRIMARY KEY,
        organization_id TEXT NOT NULL,
        payload TEXT NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_ent_tombstone_org
        ON ent_tombstone(organization_id);
     CREATE TABLE IF NOT EXISTS ent_config_layer (
        id TEXT PRIMARY KEY,
        organization_id TEXT NOT NULL,
        payload TEXT NOT NULL,
        UNIQUE (organization_id, id)
     );
     CREATE INDEX IF NOT EXISTS idx_ent_config_layer_org
        ON ent_config_layer(organization_id, id);
     CREATE TABLE IF NOT EXISTS ent_admission_freeze (
        organization_id TEXT PRIMARY KEY,
        frozen_ms INTEGER NOT NULL
     );
";

/// One durable configuration layer row (organization-scoped).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredConfigLayer {
    pub organization: OrganizationId,
    /// The canonical row id: `<scope>:<scope_ref>`.
    pub id: String,
    pub layer: ConfigLayer,
}

impl StoredConfigLayer {
    pub fn row_id(layer: &ConfigLayer) -> String {
        format!(
            "{}:{}",
            layer.scope.as_str(),
            layer.scope_ref.as_deref().unwrap_or("root")
        )
    }
}

/// The durable enterprise seam. Object-safe: the service holds one
/// `Arc<dyn EnterpriseStore>`.
pub trait EnterpriseStore: Send + Sync {
    /// Append one audit row. Idempotent per [`NewAuditEvent::event_key`]:
    /// an identical logical mutation returns the ORIGINAL row (seq intact).
    fn append_audit_event(&self, event: &NewAuditEvent) -> Result<AuditEvent, CloudStoreError>;
    fn audit_events(
        &self,
        organization: &OrganizationId,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<AuditEvent>, CloudStoreError>;
    fn audit_head_seq(&self, organization: &OrganizationId) -> Result<i64, CloudStoreError>;

    fn put_artifact(&self, artifact: &ArtifactRecord) -> Result<(), CloudStoreError>;
    fn artifact(&self, id: &ArtifactId) -> Result<Option<ArtifactRecord>, CloudStoreError>;
    fn artifacts(
        &self,
        organization: &OrganizationId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ArtifactRecord>, CloudStoreError>;

    fn put_deletion_job(&self, job: &DeletionJob) -> Result<(), CloudStoreError>;
    fn deletion_job(&self, id: &DeletionJobId) -> Result<Option<DeletionJob>, CloudStoreError>;
    fn deletion_jobs(
        &self,
        organization: &OrganizationId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<DeletionJob>, CloudStoreError>;

    fn put_org_settings(&self, settings: &OrgSettings) -> Result<(), CloudStoreError>;
    fn org_settings(
        &self,
        organization: &OrganizationId,
    ) -> Result<Option<OrgSettings>, CloudStoreError>;

    fn put_tombstone(&self, tombstone: &Tombstone) -> Result<(), CloudStoreError>;
    fn tombstone(&self, scope_key: &str) -> Result<Option<Tombstone>, CloudStoreError>;

    fn put_config_layer(&self, layer: &StoredConfigLayer) -> Result<(), CloudStoreError>;
    fn config_layers(
        &self,
        organization: &OrganizationId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<StoredConfigLayer>, CloudStoreError>;
    fn delete_config_layer(
        &self,
        organization: &OrganizationId,
        id: &str,
    ) -> Result<bool, CloudStoreError>;

    /// Freeze/unfreeze new admissions for one organization (durable: a
    /// restart keeps a running deletion job's freeze).
    fn set_frozen(
        &self,
        organization: &OrganizationId,
        frozen: bool,
    ) -> Result<(), CloudStoreError>;
    fn frozen(&self, organization: &OrganizationId) -> Result<bool, CloudStoreError>;
}

// ------------------------------------------------------------- in-memory

/// The enterprise slice of the in-memory control-plane state.
#[derive(Default)]
pub(crate) struct EnterpriseMem {
    audit: BTreeMap<String, BTreeMap<i64, AuditEvent>>,
    audit_keys: BTreeMap<String, i64>,
    next_audit_seq: i64,
    artifacts: BTreeMap<String, ArtifactRecord>,
    deletion_jobs: BTreeMap<String, DeletionJob>,
    settings: BTreeMap<String, OrgSettings>,
    tombstones: BTreeMap<String, Tombstone>,
    config_layers: BTreeMap<String, StoredConfigLayer>,
    frozen: BTreeSet<String>,
}

fn mem_enterprise(
    store: &MemoryControlPlaneStore,
) -> Result<std::sync::MutexGuard<'_, EnterpriseMem>, CloudStoreError> {
    store.enterprise_lock()
}

impl EnterpriseStore for MemoryControlPlaneStore {
    fn append_audit_event(&self, event: &NewAuditEvent) -> Result<AuditEvent, CloudStoreError> {
        let mut mem = mem_enterprise(self)?;
        let organization = event.organization.as_str().to_string();
        let key = format!("{organization}\u{0}{}", event.event_key());
        if let Some(seq) = mem.audit_keys.get(&key) {
            let row = mem
                .audit
                .get(&organization)
                .and_then(|rows| rows.get(seq))
                .cloned()
                .ok_or_else(|| {
                    CloudStoreError::Malformed(format!(
                        "in-memory audit index names seq {seq} but the row is absent"
                    ))
                })?;
            return Ok(row);
        }
        mem.next_audit_seq += 1;
        let row = AuditEvent {
            seq: mem.next_audit_seq,
            event_key: event.event_key(),
            event: event.clone(),
        };
        mem.audit
            .entry(organization)
            .or_default()
            .insert(row.seq, row.clone());
        mem.audit_keys.insert(key, row.seq);
        Ok(row)
    }

    fn audit_events(
        &self,
        organization: &OrganizationId,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<AuditEvent>, CloudStoreError> {
        let mem = mem_enterprise(self)?;
        Ok(mem
            .audit
            .get(organization.as_str())
            .map(|rows| {
                rows.range((after_seq + 1)..)
                    .take(limit)
                    .map(|(_, row)| row.clone())
                    .collect()
            })
            .unwrap_or_default())
    }

    fn audit_head_seq(&self, organization: &OrganizationId) -> Result<i64, CloudStoreError> {
        let mem = mem_enterprise(self)?;
        Ok(mem
            .audit
            .get(organization.as_str())
            .and_then(|rows| rows.keys().next_back().copied())
            .unwrap_or(0))
    }

    fn put_artifact(&self, artifact: &ArtifactRecord) -> Result<(), CloudStoreError> {
        let mut mem = mem_enterprise(self)?;
        mem.artifacts
            .insert(artifact.id.as_str().to_string(), artifact.clone());
        Ok(())
    }

    fn artifact(&self, id: &ArtifactId) -> Result<Option<ArtifactRecord>, CloudStoreError> {
        Ok(mem_enterprise(self)?.artifacts.get(id.as_str()).cloned())
    }

    fn artifacts(
        &self,
        organization: &OrganizationId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ArtifactRecord>, CloudStoreError> {
        let mem = mem_enterprise(self)?;
        Ok(mem
            .artifacts
            .values()
            .filter(|row| row.organization == *organization)
            .filter(|row| after.map(|cursor| row.id.as_str() > cursor).unwrap_or(true))
            .take(limit)
            .cloned()
            .collect())
    }

    fn put_deletion_job(&self, job: &DeletionJob) -> Result<(), CloudStoreError> {
        let mut mem = mem_enterprise(self)?;
        mem.deletion_jobs
            .insert(job.id.as_str().to_string(), job.clone());
        Ok(())
    }

    fn deletion_job(&self, id: &DeletionJobId) -> Result<Option<DeletionJob>, CloudStoreError> {
        Ok(mem_enterprise(self)?
            .deletion_jobs
            .get(id.as_str())
            .cloned())
    }

    fn deletion_jobs(
        &self,
        organization: &OrganizationId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<DeletionJob>, CloudStoreError> {
        let mem = mem_enterprise(self)?;
        Ok(mem
            .deletion_jobs
            .values()
            .filter(|row| row.organization == *organization)
            .filter(|row| after.map(|cursor| row.id.as_str() > cursor).unwrap_or(true))
            .take(limit)
            .cloned()
            .collect())
    }

    fn put_org_settings(&self, settings: &OrgSettings) -> Result<(), CloudStoreError> {
        let mut mem = mem_enterprise(self)?;
        mem.settings
            .insert(settings.organization.as_str().to_string(), settings.clone());
        Ok(())
    }

    fn org_settings(
        &self,
        organization: &OrganizationId,
    ) -> Result<Option<OrgSettings>, CloudStoreError> {
        Ok(mem_enterprise(self)?
            .settings
            .get(organization.as_str())
            .cloned())
    }

    fn put_tombstone(&self, tombstone: &Tombstone) -> Result<(), CloudStoreError> {
        let mut mem = mem_enterprise(self)?;
        mem.tombstones
            .insert(tombstone.scope_key.clone(), tombstone.clone());
        Ok(())
    }

    fn tombstone(&self, scope_key: &str) -> Result<Option<Tombstone>, CloudStoreError> {
        Ok(mem_enterprise(self)?.tombstones.get(scope_key).cloned())
    }

    fn put_config_layer(&self, layer: &StoredConfigLayer) -> Result<(), CloudStoreError> {
        let mut mem = mem_enterprise(self)?;
        mem.config_layers.insert(
            format!("{}\u{0}{}", layer.organization, layer.id),
            layer.clone(),
        );
        Ok(())
    }

    fn config_layers(
        &self,
        organization: &OrganizationId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<StoredConfigLayer>, CloudStoreError> {
        let mem = mem_enterprise(self)?;
        let mut rows: Vec<StoredConfigLayer> = mem
            .config_layers
            .values()
            .filter(|row| row.organization == *organization)
            .cloned()
            .collect();
        rows.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(rows
            .into_iter()
            .filter(|row| after.map(|cursor| row.id.as_str() > cursor).unwrap_or(true))
            .take(limit)
            .collect())
    }

    fn delete_config_layer(
        &self,
        organization: &OrganizationId,
        id: &str,
    ) -> Result<bool, CloudStoreError> {
        Ok(mem_enterprise(self)?
            .config_layers
            .remove(&format!("{organization}\u{0}{id}"))
            .is_some())
    }

    fn set_frozen(
        &self,
        organization: &OrganizationId,
        frozen: bool,
    ) -> Result<(), CloudStoreError> {
        let mut mem = mem_enterprise(self)?;
        if frozen {
            mem.frozen.insert(organization.as_str().to_string());
        } else {
            mem.frozen.remove(organization.as_str());
        }
        Ok(())
    }

    fn frozen(&self, organization: &OrganizationId) -> Result<bool, CloudStoreError> {
        Ok(mem_enterprise(self)?.frozen.contains(organization.as_str()))
    }
}

// ---------------------------------------------------------------- sqlite

fn backend(e: rusqlite::Error) -> CloudStoreError {
    CloudStoreError::Backend(e.to_string())
}

fn encode<T: Serialize>(value: &T) -> Result<String, CloudStoreError> {
    serde_json::to_string(value).map_err(|e| CloudStoreError::Malformed(e.to_string()))
}

fn parse<T: for<'de> Deserialize<'de>>(payload: &str) -> Result<T, CloudStoreError> {
    serde_json::from_str(payload).map_err(|e| CloudStoreError::Malformed(e.to_string()))
}

fn scoped_rows<T: for<'de> Deserialize<'de>>(
    conn: &Connection,
    sql: &str,
    args: &[&dyn rusqlite::ToSql],
) -> Result<Vec<T>, CloudStoreError> {
    let mut stmt = conn.prepare(sql).map_err(backend)?;
    let payloads = stmt
        .query_map(args, |row| row.get::<_, String>(0))
        .map_err(backend)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(backend)?;
    payloads.iter().map(|payload| parse(payload)).collect()
}

impl EnterpriseStore for SqliteControlPlaneStore {
    fn append_audit_event(&self, event: &NewAuditEvent) -> Result<AuditEvent, CloudStoreError> {
        let conn = self.lock()?;
        let event_key = event.event_key();
        let payload = encode(event)?;
        conn.execute(
            "INSERT INTO ent_audit_event
                 (organization_id, event_key, action, object, occurred_at_ms, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (organization_id, event_key) DO NOTHING",
            params![
                event.organization.as_str(),
                event_key,
                event.action.as_str(),
                event.object,
                event.timestamp_ms,
                payload,
            ],
        )
        .map_err(backend)?;
        let row: (i64, String) = conn
            .query_row(
                "SELECT seq, payload FROM ent_audit_event
                 WHERE organization_id = ?1 AND event_key = ?2",
                params![event.organization.as_str(), event_key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(backend)?;
        let mut stored: NewAuditEvent = parse(&row.1)?;
        // The stored timestamp is the ORIGINAL row's (an idempotent replay
        // never rewrites history); the key stays authoritative.
        stored.organization = event.organization.clone();
        Ok(AuditEvent {
            seq: row.0,
            event_key,
            event: stored,
        })
    }

    fn audit_events(
        &self,
        organization: &OrganizationId,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<AuditEvent>, CloudStoreError> {
        let conn = self.lock()?;
        let mut stmt = conn
            .prepare(
                "SELECT seq, event_key, payload FROM ent_audit_event
                 WHERE organization_id = ?1 AND seq > ?2
                 ORDER BY seq ASC LIMIT ?3",
            )
            .map_err(backend)?;
        let rows = stmt
            .query_map(
                params![organization.as_str(), after_seq, limit as i64],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .map_err(backend)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend)?;
        rows.into_iter()
            .map(|(seq, event_key, payload)| {
                Ok(AuditEvent {
                    seq,
                    event_key,
                    event: parse(&payload)?,
                })
            })
            .collect()
    }

    fn audit_head_seq(&self, organization: &OrganizationId) -> Result<i64, CloudStoreError> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM ent_audit_event WHERE organization_id = ?1",
            params![organization.as_str()],
            |row| row.get(0),
        )
        .map_err(backend)
    }

    fn put_artifact(&self, artifact: &ArtifactRecord) -> Result<(), CloudStoreError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO ent_artifact
                 (id, organization_id, digest, deletion_state, expires_at_ms, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
                 organization_id = excluded.organization_id,
                 digest = excluded.digest,
                 deletion_state = excluded.deletion_state,
                 expires_at_ms = excluded.expires_at_ms,
                 payload = excluded.payload",
            params![
                artifact.id.as_str(),
                artifact.organization.as_str(),
                artifact.digest,
                artifact.deletion_state.as_str(),
                artifact.expires_at_ms,
                encode(artifact)?,
            ],
        )
        .map_err(backend)?;
        Ok(())
    }

    fn artifact(&self, id: &ArtifactId) -> Result<Option<ArtifactRecord>, CloudStoreError> {
        let conn = self.lock()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM ent_artifact WHERE id = ?1",
                params![id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(backend)?;
        payload.map(|payload| parse(&payload)).transpose()
    }

    fn artifacts(
        &self,
        organization: &OrganizationId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ArtifactRecord>, CloudStoreError> {
        let conn = self.lock()?;
        scoped_rows(
            &conn,
            "SELECT payload FROM ent_artifact
             WHERE organization_id = ?1 AND id > ?2
             ORDER BY id ASC LIMIT ?3",
            &[
                &organization.as_str(),
                &after.unwrap_or(""),
                &(limit as i64),
            ],
        )
    }

    fn put_deletion_job(&self, job: &DeletionJob) -> Result<(), CloudStoreError> {
        let conn = self.lock()?;
        let state = match job.state {
            crate::enterprise::DeletionJobState::Running => "running",
            crate::enterprise::DeletionJobState::Completed => "completed",
        };
        conn.execute(
            "INSERT INTO ent_deletion_job (id, organization_id, state, payload)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET
                 organization_id = excluded.organization_id,
                 state = excluded.state,
                 payload = excluded.payload",
            params![
                job.id.as_str(),
                job.organization.as_str(),
                state,
                encode(job)?
            ],
        )
        .map_err(backend)?;
        Ok(())
    }

    fn deletion_job(&self, id: &DeletionJobId) -> Result<Option<DeletionJob>, CloudStoreError> {
        let conn = self.lock()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM ent_deletion_job WHERE id = ?1",
                params![id.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(backend)?;
        payload.map(|payload| parse(&payload)).transpose()
    }

    fn deletion_jobs(
        &self,
        organization: &OrganizationId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<DeletionJob>, CloudStoreError> {
        let conn = self.lock()?;
        scoped_rows(
            &conn,
            "SELECT payload FROM ent_deletion_job
             WHERE organization_id = ?1 AND id > ?2
             ORDER BY id ASC LIMIT ?3",
            &[
                &organization.as_str(),
                &after.unwrap_or(""),
                &(limit as i64),
            ],
        )
    }

    fn put_org_settings(&self, settings: &OrgSettings) -> Result<(), CloudStoreError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO ent_org_settings (organization_id, revision, payload)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(organization_id) DO UPDATE SET
                 revision = excluded.revision,
                 payload = excluded.payload",
            params![
                settings.organization.as_str(),
                settings.revision as i64,
                encode(settings)?
            ],
        )
        .map_err(backend)?;
        Ok(())
    }

    fn org_settings(
        &self,
        organization: &OrganizationId,
    ) -> Result<Option<OrgSettings>, CloudStoreError> {
        let conn = self.lock()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM ent_org_settings WHERE organization_id = ?1",
                params![organization.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(backend)?;
        payload.map(|payload| parse(&payload)).transpose()
    }

    fn put_tombstone(&self, tombstone: &Tombstone) -> Result<(), CloudStoreError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO ent_tombstone (scope_key, organization_id, payload)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(scope_key) DO UPDATE SET
                 organization_id = excluded.organization_id,
                 payload = excluded.payload",
            params![
                tombstone.scope_key,
                tombstone.organization.as_str(),
                encode(tombstone)?
            ],
        )
        .map_err(backend)?;
        Ok(())
    }

    fn tombstone(&self, scope_key: &str) -> Result<Option<Tombstone>, CloudStoreError> {
        let conn = self.lock()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM ent_tombstone WHERE scope_key = ?1",
                params![scope_key],
                |row| row.get(0),
            )
            .optional()
            .map_err(backend)?;
        payload.map(|payload| parse(&payload)).transpose()
    }

    fn put_config_layer(&self, layer: &StoredConfigLayer) -> Result<(), CloudStoreError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO ent_config_layer (id, organization_id, payload)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET
                 organization_id = excluded.organization_id,
                 payload = excluded.payload",
            params![layer.id, layer.organization.as_str(), encode(layer)?],
        )
        .map_err(backend)?;
        Ok(())
    }

    fn config_layers(
        &self,
        organization: &OrganizationId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<StoredConfigLayer>, CloudStoreError> {
        let conn = self.lock()?;
        scoped_rows(
            &conn,
            "SELECT payload FROM ent_config_layer
             WHERE organization_id = ?1 AND id > ?2
             ORDER BY id ASC LIMIT ?3",
            &[
                &organization.as_str(),
                &after.unwrap_or(""),
                &(limit as i64),
            ],
        )
    }

    fn delete_config_layer(
        &self,
        organization: &OrganizationId,
        id: &str,
    ) -> Result<bool, CloudStoreError> {
        let conn = self.lock()?;
        let deleted = conn
            .execute(
                "DELETE FROM ent_config_layer WHERE organization_id = ?1 AND id = ?2",
                params![organization.as_str(), id],
            )
            .map_err(backend)?;
        Ok(deleted > 0)
    }

    fn set_frozen(
        &self,
        organization: &OrganizationId,
        frozen: bool,
    ) -> Result<(), CloudStoreError> {
        let conn = self.lock()?;
        if frozen {
            let frozen_ms = crate::service::Clock::now_ms(&crate::service::SystemClock);
            conn.execute(
                "INSERT INTO ent_admission_freeze (organization_id, frozen_ms)
                 VALUES (?1, ?2)
                 ON CONFLICT(organization_id) DO UPDATE SET frozen_ms = excluded.frozen_ms",
                params![organization.as_str(), frozen_ms],
            )
            .map_err(backend)?;
        } else {
            conn.execute(
                "DELETE FROM ent_admission_freeze WHERE organization_id = ?1",
                params![organization.as_str()],
            )
            .map_err(backend)?;
        }
        Ok(())
    }

    fn frozen(&self, organization: &OrganizationId) -> Result<bool, CloudStoreError> {
        let conn = self.lock()?;
        let found: Option<i64> = conn
            .query_row(
                "SELECT frozen_ms FROM ent_admission_freeze WHERE organization_id = ?1",
                params![organization.as_str()],
                |row| row.get(0),
            )
            .optional()
            .map_err(backend)?;
        Ok(found.is_some())
    }
}

impl MemoryControlPlaneStore {
    /// The enterprise slice lock (the additive enterprise store shares the
    /// ONE in-memory authority with the control-plane store).
    pub(crate) fn enterprise_lock(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, EnterpriseMem>, CloudStoreError> {
        self.lock_enterprise()
    }
}
