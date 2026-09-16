//! Durable control-plane state: the [`ControlPlaneStore`] seam plus its
//! in-memory and SQLite implementations.
//!
//! Both implementations store each entity behind its typed id and keep the
//! ORGANIZATION column (or the `organization_id` index) alongside, so every
//! organization-scoped query is a scoped scan by construction. Listings are
//! ordered by id with an exclusive cursor, so pagination is stable and
//! bounded. The SQLite implementation keeps the full entity JSON as its
//! payload column: reads are fallible parses (a corrupt row is a typed
//! [`CloudStoreError::Malformed`], never a partial guess).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::ids::{
    ApprovalId, AuthSessionId, InvitationId, OrganizationId, ServiceAccountId, TokenHash, UserId,
};
use crate::model::{
    ApprovalRequest, ApprovalStatus, AuthSession, ExternalIdentity, Invitation, Membership,
    Organization, ServiceAccount, User,
};

/// One recorded idempotent operation: the exact response of the first
/// successful execution of `(key, operation, request_hash)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdempotencyRecord {
    pub key: String,
    pub operation: String,
    pub request_hash: String,
    /// The recorded response body (the caller replays it verbatim).
    pub response_json: String,
    pub created_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CloudStoreError {
    #[error("control-plane store backend unavailable: {0}")]
    Backend(String),
    #[error("control-plane store refused a malformed row: {0}")]
    Malformed(String),
}

/// The durable control-plane seam. Object-safe: the service holds one
/// `Arc<dyn ControlPlaneStore>`.
pub trait ControlPlaneStore: Send + Sync {
    fn put_user(&self, user: &User) -> Result<(), CloudStoreError>;
    fn user(&self, id: &UserId) -> Result<Option<User>, CloudStoreError>;
    fn user_by_email(&self, email: &str) -> Result<Option<User>, CloudStoreError>;
    fn users(&self, after: Option<&str>, limit: usize) -> Result<Vec<User>, CloudStoreError>;

    fn put_organization(&self, organization: &Organization) -> Result<(), CloudStoreError>;
    fn organization(&self, id: &OrganizationId) -> Result<Option<Organization>, CloudStoreError>;
    fn organizations(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Organization>, CloudStoreError>;

    fn put_external_identity(&self, identity: &ExternalIdentity) -> Result<(), CloudStoreError>;
    fn external_identity(
        &self,
        provider: &str,
        subject: &str,
    ) -> Result<Option<ExternalIdentity>, CloudStoreError>;
    fn external_identities_for_user(
        &self,
        user: &UserId,
    ) -> Result<Vec<ExternalIdentity>, CloudStoreError>;

    fn put_membership(&self, membership: &Membership) -> Result<(), CloudStoreError>;
    fn membership(
        &self,
        organization: &OrganizationId,
        user: &UserId,
    ) -> Result<Option<Membership>, CloudStoreError>;
    fn memberships(
        &self,
        organization: &OrganizationId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Membership>, CloudStoreError>;
    fn delete_membership(
        &self,
        organization: &OrganizationId,
        user: &UserId,
    ) -> Result<bool, CloudStoreError>;

    fn put_invitation(&self, invitation: &Invitation) -> Result<(), CloudStoreError>;
    fn invitation(&self, id: &InvitationId) -> Result<Option<Invitation>, CloudStoreError>;
    fn invitation_by_token_hash(
        &self,
        hash: &TokenHash,
    ) -> Result<Option<Invitation>, CloudStoreError>;
    fn invitations(
        &self,
        organization: &OrganizationId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Invitation>, CloudStoreError>;

    fn put_auth_session(&self, session: &AuthSession) -> Result<(), CloudStoreError>;
    fn auth_session(&self, id: &AuthSessionId) -> Result<Option<AuthSession>, CloudStoreError>;
    fn auth_session_by_token_hash(
        &self,
        hash: &TokenHash,
    ) -> Result<Option<AuthSession>, CloudStoreError>;

    fn put_service_account(&self, account: &ServiceAccount) -> Result<(), CloudStoreError>;
    fn service_account(
        &self,
        id: &ServiceAccountId,
    ) -> Result<Option<ServiceAccount>, CloudStoreError>;
    fn service_account_by_token_hash(
        &self,
        hash: &TokenHash,
    ) -> Result<Option<ServiceAccount>, CloudStoreError>;
    fn service_accounts(
        &self,
        organization: &OrganizationId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ServiceAccount>, CloudStoreError>;

    fn put_approval(&self, approval: &ApprovalRequest) -> Result<(), CloudStoreError>;
    fn approval(&self, id: &ApprovalId) -> Result<Option<ApprovalRequest>, CloudStoreError>;
    fn approvals(
        &self,
        organization: &OrganizationId,
        status: Option<ApprovalStatus>,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ApprovalRequest>, CloudStoreError>;

    /// Claim one idempotency key: `true` only for the FIRST writer. A
    /// later writer observes `false` and replays the recorded response.
    fn claim_idempotent(&self, record: &IdempotencyRecord) -> Result<bool, CloudStoreError>;
    fn idempotent(&self, key: &str) -> Result<Option<IdempotencyRecord>, CloudStoreError>;
}

// ------------------------------------------------------------- in-memory

#[derive(Default)]
struct MemInner {
    users: BTreeMap<String, User>,
    organizations: BTreeMap<String, Organization>,
    external_identities: BTreeMap<String, ExternalIdentity>,
    memberships: BTreeMap<String, Membership>,
    invitations: BTreeMap<String, Invitation>,
    auth_sessions: BTreeMap<String, AuthSession>,
    service_accounts: BTreeMap<String, ServiceAccount>,
    approvals: BTreeMap<String, ApprovalRequest>,
    idempotency: BTreeMap<String, IdempotencyRecord>,
}

/// In-memory [`ControlPlaneStore`] for unit tests and embedded hosts.
#[derive(Default)]
pub struct MemoryControlPlaneStore {
    inner: Mutex<MemInner>,
}

impl MemoryControlPlaneStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, MemInner>, CloudStoreError> {
        self.inner.lock().map_err(|_| {
            CloudStoreError::Backend("in-memory control-plane store lock is poisoned".into())
        })
    }
}

fn page_by_id<T, F>(map: &BTreeMap<String, T>, after: Option<&str>, limit: usize, key: F) -> Vec<T>
where
    F: Fn(&T) -> &str,
    T: Clone,
{
    map.iter()
        .filter(|(_, value)| after.map(|cursor| key(value) > cursor).unwrap_or(true))
        .take(limit)
        .map(|(_, value)| value.clone())
        .collect()
}

impl ControlPlaneStore for MemoryControlPlaneStore {
    fn put_user(&self, user: &User) -> Result<(), CloudStoreError> {
        let mut inner = self.lock()?;
        if let Some(existing) = inner.users.values().find(|u| u.email == user.email) {
            if existing.id != user.id {
                return Err(CloudStoreError::Malformed(format!(
                    "email {} is already taken by {}",
                    user.email, existing.id
                )));
            }
        }
        inner
            .users
            .insert(user.id.as_str().to_string(), user.clone());
        Ok(())
    }

    fn user(&self, id: &UserId) -> Result<Option<User>, CloudStoreError> {
        Ok(self.lock()?.users.get(id.as_str()).cloned())
    }

    fn user_by_email(&self, email: &str) -> Result<Option<User>, CloudStoreError> {
        Ok(self
            .lock()?
            .users
            .values()
            .find(|u| u.email == email)
            .cloned())
    }

    fn users(&self, after: Option<&str>, limit: usize) -> Result<Vec<User>, CloudStoreError> {
        Ok(page_by_id(&self.lock()?.users, after, limit, |u| {
            u.id.as_str()
        }))
    }

    fn put_organization(&self, organization: &Organization) -> Result<(), CloudStoreError> {
        self.lock()?
            .organizations
            .insert(organization.id.as_str().to_string(), organization.clone());
        Ok(())
    }

    fn organization(&self, id: &OrganizationId) -> Result<Option<Organization>, CloudStoreError> {
        Ok(self.lock()?.organizations.get(id.as_str()).cloned())
    }

    fn organizations(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Organization>, CloudStoreError> {
        Ok(page_by_id(&self.lock()?.organizations, after, limit, |o| {
            o.id.as_str()
        }))
    }

    fn put_external_identity(&self, identity: &ExternalIdentity) -> Result<(), CloudStoreError> {
        self.lock()?.external_identities.insert(
            format!("{}:{}", identity.provider, identity.subject),
            identity.clone(),
        );
        Ok(())
    }

    fn external_identity(
        &self,
        provider: &str,
        subject: &str,
    ) -> Result<Option<ExternalIdentity>, CloudStoreError> {
        Ok(self
            .lock()?
            .external_identities
            .get(&format!("{provider}:{subject}"))
            .cloned())
    }

    fn external_identities_for_user(
        &self,
        user: &UserId,
    ) -> Result<Vec<ExternalIdentity>, CloudStoreError> {
        Ok(self
            .lock()?
            .external_identities
            .values()
            .filter(|i| i.user == *user)
            .cloned()
            .collect())
    }

    fn put_membership(&self, membership: &Membership) -> Result<(), CloudStoreError> {
        let mut inner = self.lock()?;
        if let Some(existing) = inner
            .memberships
            .values()
            .find(|m| m.organization == membership.organization && m.user == membership.user)
        {
            if existing.id != membership.id {
                return Err(CloudStoreError::Malformed(
                    "membership already exists for this (organization, user)".into(),
                ));
            }
        }
        inner
            .memberships
            .insert(membership.id.as_str().to_string(), membership.clone());
        Ok(())
    }

    fn membership(
        &self,
        organization: &OrganizationId,
        user: &UserId,
    ) -> Result<Option<Membership>, CloudStoreError> {
        Ok(self
            .lock()?
            .memberships
            .values()
            .find(|m| m.organization == *organization && m.user == *user)
            .cloned())
    }

    fn memberships(
        &self,
        organization: &OrganizationId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Membership>, CloudStoreError> {
        let inner = self.lock()?;
        Ok(inner
            .memberships
            .values()
            .filter(|m| m.organization == *organization)
            .filter(|m| after.map(|c| m.id.as_str() > c).unwrap_or(true))
            .take(limit)
            .cloned()
            .collect())
    }

    fn delete_membership(
        &self,
        organization: &OrganizationId,
        user: &UserId,
    ) -> Result<bool, CloudStoreError> {
        let mut inner = self.lock()?;
        let key = inner
            .memberships
            .iter()
            .find(|(_, m)| m.organization == *organization && m.user == *user)
            .map(|(id, _)| id.clone());
        match key {
            Some(id) => {
                inner.memberships.remove(&id);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn put_invitation(&self, invitation: &Invitation) -> Result<(), CloudStoreError> {
        self.lock()?
            .invitations
            .insert(invitation.id.as_str().to_string(), invitation.clone());
        Ok(())
    }

    fn invitation(&self, id: &InvitationId) -> Result<Option<Invitation>, CloudStoreError> {
        Ok(self.lock()?.invitations.get(id.as_str()).cloned())
    }

    fn invitation_by_token_hash(
        &self,
        hash: &TokenHash,
    ) -> Result<Option<Invitation>, CloudStoreError> {
        Ok(self
            .lock()?
            .invitations
            .values()
            .find(|i| i.token_hash == *hash)
            .cloned())
    }

    fn invitations(
        &self,
        organization: &OrganizationId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Invitation>, CloudStoreError> {
        let inner = self.lock()?;
        Ok(inner
            .invitations
            .values()
            .filter(|i| i.organization == *organization)
            .filter(|i| after.map(|c| i.id.as_str() > c).unwrap_or(true))
            .take(limit)
            .cloned()
            .collect())
    }

    fn put_auth_session(&self, session: &AuthSession) -> Result<(), CloudStoreError> {
        self.lock()?
            .auth_sessions
            .insert(session.id.as_str().to_string(), session.clone());
        Ok(())
    }

    fn auth_session(&self, id: &AuthSessionId) -> Result<Option<AuthSession>, CloudStoreError> {
        Ok(self.lock()?.auth_sessions.get(id.as_str()).cloned())
    }

    fn auth_session_by_token_hash(
        &self,
        hash: &TokenHash,
    ) -> Result<Option<AuthSession>, CloudStoreError> {
        Ok(self
            .lock()?
            .auth_sessions
            .values()
            .find(|s| s.token_hash == *hash)
            .cloned())
    }

    fn put_service_account(&self, account: &ServiceAccount) -> Result<(), CloudStoreError> {
        self.lock()?
            .service_accounts
            .insert(account.id.as_str().to_string(), account.clone());
        Ok(())
    }

    fn service_account(
        &self,
        id: &ServiceAccountId,
    ) -> Result<Option<ServiceAccount>, CloudStoreError> {
        Ok(self.lock()?.service_accounts.get(id.as_str()).cloned())
    }

    fn service_account_by_token_hash(
        &self,
        hash: &TokenHash,
    ) -> Result<Option<ServiceAccount>, CloudStoreError> {
        Ok(self
            .lock()?
            .service_accounts
            .values()
            .find(|s| s.token_hash == *hash)
            .cloned())
    }

    fn service_accounts(
        &self,
        organization: &OrganizationId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ServiceAccount>, CloudStoreError> {
        let inner = self.lock()?;
        Ok(inner
            .service_accounts
            .values()
            .filter(|s| s.organization == *organization)
            .filter(|s| after.map(|c| s.id.as_str() > c).unwrap_or(true))
            .take(limit)
            .cloned()
            .collect())
    }

    fn put_approval(&self, approval: &ApprovalRequest) -> Result<(), CloudStoreError> {
        self.lock()?
            .approvals
            .insert(approval.id.as_str().to_string(), approval.clone());
        Ok(())
    }

    fn approval(&self, id: &ApprovalId) -> Result<Option<ApprovalRequest>, CloudStoreError> {
        Ok(self.lock()?.approvals.get(id.as_str()).cloned())
    }

    fn approvals(
        &self,
        organization: &OrganizationId,
        status: Option<ApprovalStatus>,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ApprovalRequest>, CloudStoreError> {
        let inner = self.lock()?;
        Ok(inner
            .approvals
            .values()
            .filter(|a| a.organization == *organization)
            .filter(|a| status.map(|s| a.status == s).unwrap_or(true))
            .filter(|a| after.map(|c| a.id.as_str() > c).unwrap_or(true))
            .take(limit)
            .cloned()
            .collect())
    }

    fn claim_idempotent(&self, record: &IdempotencyRecord) -> Result<bool, CloudStoreError> {
        let mut inner = self.lock()?;
        if inner.idempotency.contains_key(&record.key) {
            return Ok(false);
        }
        inner.idempotency.insert(record.key.clone(), record.clone());
        Ok(true)
    }

    fn idempotent(&self, key: &str) -> Result<Option<IdempotencyRecord>, CloudStoreError> {
        Ok(self.lock()?.idempotency.get(key).cloned())
    }
}

// ---------------------------------------------------------------- sqlite

/// The durable [`ControlPlaneStore`] over its own SQLite database file.
pub struct SqliteControlPlaneStore {
    conn: Mutex<Connection>,
}

const CP_MIGRATIONS: &[&str] = &["CREATE TABLE IF NOT EXISTS cp_user (
        id TEXT PRIMARY KEY,
        email TEXT NOT NULL UNIQUE,
        payload TEXT NOT NULL
     );
     CREATE TABLE IF NOT EXISTS cp_organization (
        id TEXT PRIMARY KEY,
        payload TEXT NOT NULL
     );
     CREATE TABLE IF NOT EXISTS cp_external_identity (
        id TEXT PRIMARY KEY,
        provider TEXT NOT NULL,
        subject TEXT NOT NULL,
        user_id TEXT NOT NULL,
        payload TEXT NOT NULL,
        UNIQUE (provider, subject)
     );
     CREATE INDEX IF NOT EXISTS idx_cp_external_identity_user ON cp_external_identity(user_id);
     CREATE TABLE IF NOT EXISTS cp_membership (
        id TEXT PRIMARY KEY,
        organization_id TEXT NOT NULL,
        user_id TEXT NOT NULL,
        payload TEXT NOT NULL,
        UNIQUE (organization_id, user_id)
     );
     CREATE INDEX IF NOT EXISTS idx_cp_membership_org ON cp_membership(organization_id, id);
     CREATE TABLE IF NOT EXISTS cp_invitation (
        id TEXT PRIMARY KEY,
        organization_id TEXT NOT NULL,
        token_hash TEXT NOT NULL UNIQUE,
        payload TEXT NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_cp_invitation_org ON cp_invitation(organization_id, id);
     CREATE TABLE IF NOT EXISTS cp_auth_session (
        id TEXT PRIMARY KEY,
        token_hash TEXT NOT NULL UNIQUE,
        payload TEXT NOT NULL
     );
     CREATE TABLE IF NOT EXISTS cp_service_account (
        id TEXT PRIMARY KEY,
        organization_id TEXT NOT NULL,
        token_hash TEXT NOT NULL UNIQUE,
        payload TEXT NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_cp_service_account_org
        ON cp_service_account(organization_id, id);
     CREATE TABLE IF NOT EXISTS cp_approval (
        id TEXT PRIMARY KEY,
        organization_id TEXT NOT NULL,
        status TEXT NOT NULL,
        payload TEXT NOT NULL
     );
     CREATE INDEX IF NOT EXISTS idx_cp_approval_org ON cp_approval(organization_id, status, id);
     CREATE TABLE IF NOT EXISTS cp_idempotency (
        key TEXT PRIMARY KEY,
        operation TEXT NOT NULL,
        request_hash TEXT NOT NULL,
        response TEXT NOT NULL,
        created_ms INTEGER NOT NULL
     );"];

impl SqliteControlPlaneStore {
    /// Open (creating) the control-plane database at `path`.
    pub fn open(path: &Path) -> Result<Self, CloudStoreError> {
        let conn = Connection::open(path).map_err(backend)?;
        Self::prepare(conn)
    }

    /// Open an in-memory database (tests, ephemeral hosts).
    pub fn open_in_memory() -> Result<Self, CloudStoreError> {
        let conn = Connection::open_in_memory().map_err(backend)?;
        Self::prepare(conn)
    }

    fn prepare(conn: Connection) -> Result<Self, CloudStoreError> {
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

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, CloudStoreError> {
        self.conn
            .lock()
            .map_err(|_| CloudStoreError::Backend("control-plane store lock is poisoned".into()))
    }
}

fn backend(e: rusqlite::Error) -> CloudStoreError {
    CloudStoreError::Backend(e.to_string())
}

fn migrate(conn: &mut Connection) -> Result<(), CloudStoreError> {
    let mut version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(backend)?;
    for (i, sql) in CP_MIGRATIONS.iter().enumerate() {
        let target = (i + 1) as i64;
        if version >= target {
            continue;
        }
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(backend)?;
        tx.execute_batch(sql)
            .map_err(|e| CloudStoreError::Backend(format!("cp migration v{target}: {e}")))?;
        tx.execute_batch(&format!("PRAGMA user_version = {target}"))
            .map_err(|e| CloudStoreError::Backend(format!("cp migration v{target} cursor: {e}")))?;
        tx.commit().map_err(backend)?;
        version = target;
    }
    Ok(())
}

fn parse<T: for<'de> Deserialize<'de>>(payload: &str) -> Result<T, CloudStoreError> {
    serde_json::from_str(payload)
        .map_err(|e| CloudStoreError::Malformed(format!("control-plane row payload: {e}")))
}

fn encode<T: Serialize>(value: &T) -> Result<String, CloudStoreError> {
    serde_json::to_string(value)
        .map_err(|e| CloudStoreError::Malformed(format!("control-plane row encode: {e}")))
}

fn scoped_page<T: for<'de> Deserialize<'de>>(
    conn: &Connection,
    sql: &str,
    args: &[&dyn rusqlite::ToSql],
) -> Result<Vec<T>, CloudStoreError> {
    let mut stmt = conn.prepare(sql).map_err(backend)?;
    let rows = stmt
        .query_map(args, |r| r.get::<_, String>(0))
        .map_err(backend)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(backend)?;
    rows.iter().map(|payload| parse(payload)).collect()
}

impl ControlPlaneStore for SqliteControlPlaneStore {
    fn put_user(&self, user: &User) -> Result<(), CloudStoreError> {
        let conn = self.lock()?;
        let existing: Option<String> = conn
            .query_row(
                "SELECT id FROM cp_user WHERE email = ?1",
                params![user.email],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        if let Some(existing) = existing {
            if existing != user.id.as_str() {
                return Err(CloudStoreError::Malformed(format!(
                    "email {} is already taken",
                    user.email
                )));
            }
        }
        conn.execute(
            "INSERT INTO cp_user (id, email, payload) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET email = excluded.email, payload = excluded.payload",
            params![user.id.as_str(), user.email, encode(user)?],
        )
        .map_err(backend)?;
        Ok(())
    }

    fn user(&self, id: &UserId) -> Result<Option<User>, CloudStoreError> {
        let conn = self.lock()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM cp_user WHERE id = ?1",
                params![id.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        payload.map(|p| parse(&p)).transpose()
    }

    fn user_by_email(&self, email: &str) -> Result<Option<User>, CloudStoreError> {
        let conn = self.lock()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM cp_user WHERE email = ?1",
                params![email],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        payload.map(|p| parse(&p)).transpose()
    }

    fn users(&self, after: Option<&str>, limit: usize) -> Result<Vec<User>, CloudStoreError> {
        let conn = self.lock()?;
        scoped_page(
            &conn,
            "SELECT payload FROM cp_user WHERE (?1 IS NULL OR id > ?1) ORDER BY id LIMIT ?2",
            &[&after, &(limit as i64)],
        )
    }

    fn put_organization(&self, organization: &Organization) -> Result<(), CloudStoreError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO cp_organization (id, payload) VALUES (?1, ?2)
             ON CONFLICT(id) DO UPDATE SET payload = excluded.payload",
            params![organization.id.as_str(), encode(organization)?],
        )
        .map_err(backend)?;
        Ok(())
    }

    fn organization(&self, id: &OrganizationId) -> Result<Option<Organization>, CloudStoreError> {
        let conn = self.lock()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM cp_organization WHERE id = ?1",
                params![id.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        payload.map(|p| parse(&p)).transpose()
    }

    fn organizations(
        &self,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Organization>, CloudStoreError> {
        let conn = self.lock()?;
        scoped_page(
            &conn,
            "SELECT payload FROM cp_organization WHERE (?1 IS NULL OR id > ?1) ORDER BY id LIMIT ?2",
            &[&after, &(limit as i64)],
        )
    }

    fn put_external_identity(&self, identity: &ExternalIdentity) -> Result<(), CloudStoreError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO cp_external_identity (id, provider, subject, user_id, payload)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET
                provider = excluded.provider,
                subject = excluded.subject,
                user_id = excluded.user_id,
                payload = excluded.payload",
            params![
                identity.id.as_str(),
                identity.provider,
                identity.subject,
                identity.user.as_str(),
                encode(identity)?,
            ],
        )
        .map_err(backend)?;
        Ok(())
    }

    fn external_identity(
        &self,
        provider: &str,
        subject: &str,
    ) -> Result<Option<ExternalIdentity>, CloudStoreError> {
        let conn = self.lock()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM cp_external_identity WHERE provider = ?1 AND subject = ?2",
                params![provider, subject],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        payload.map(|p| parse(&p)).transpose()
    }

    fn external_identities_for_user(
        &self,
        user: &UserId,
    ) -> Result<Vec<ExternalIdentity>, CloudStoreError> {
        let conn = self.lock()?;
        scoped_page(
            &conn,
            "SELECT payload FROM cp_external_identity WHERE user_id = ?1 ORDER BY id LIMIT 500",
            &[&user.as_str()],
        )
    }

    fn put_membership(&self, membership: &Membership) -> Result<(), CloudStoreError> {
        let conn = self.lock()?;
        let existing: Option<String> = conn
            .query_row(
                "SELECT id FROM cp_membership WHERE organization_id = ?1 AND user_id = ?2",
                params![membership.organization.as_str(), membership.user.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        if let Some(existing) = existing {
            if existing != membership.id.as_str() {
                return Err(CloudStoreError::Malformed(
                    "membership already exists for this (organization, user)".into(),
                ));
            }
        }
        conn.execute(
            "INSERT INTO cp_membership (id, organization_id, user_id, payload)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET payload = excluded.payload",
            params![
                membership.id.as_str(),
                membership.organization.as_str(),
                membership.user.as_str(),
                encode(membership)?,
            ],
        )
        .map_err(backend)?;
        Ok(())
    }

    fn membership(
        &self,
        organization: &OrganizationId,
        user: &UserId,
    ) -> Result<Option<Membership>, CloudStoreError> {
        let conn = self.lock()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM cp_membership WHERE organization_id = ?1 AND user_id = ?2",
                params![organization.as_str(), user.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        payload.map(|p| parse(&p)).transpose()
    }

    fn memberships(
        &self,
        organization: &OrganizationId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Membership>, CloudStoreError> {
        let conn = self.lock()?;
        scoped_page(
            &conn,
            "SELECT payload FROM cp_membership
             WHERE organization_id = ?1 AND (?2 IS NULL OR id > ?2)
             ORDER BY id LIMIT ?3",
            &[&organization.as_str(), &after, &(limit as i64)],
        )
    }

    fn delete_membership(
        &self,
        organization: &OrganizationId,
        user: &UserId,
    ) -> Result<bool, CloudStoreError> {
        let conn = self.lock()?;
        let removed = conn
            .execute(
                "DELETE FROM cp_membership WHERE organization_id = ?1 AND user_id = ?2",
                params![organization.as_str(), user.as_str()],
            )
            .map_err(backend)?;
        Ok(removed == 1)
    }

    fn put_invitation(&self, invitation: &Invitation) -> Result<(), CloudStoreError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO cp_invitation (id, organization_id, token_hash, payload)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET
                organization_id = excluded.organization_id,
                token_hash = excluded.token_hash,
                payload = excluded.payload",
            params![
                invitation.id.as_str(),
                invitation.organization.as_str(),
                invitation.token_hash.as_str(),
                encode(invitation)?,
            ],
        )
        .map_err(backend)?;
        Ok(())
    }

    fn invitation(&self, id: &InvitationId) -> Result<Option<Invitation>, CloudStoreError> {
        let conn = self.lock()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM cp_invitation WHERE id = ?1",
                params![id.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        payload.map(|p| parse(&p)).transpose()
    }

    fn invitation_by_token_hash(
        &self,
        hash: &TokenHash,
    ) -> Result<Option<Invitation>, CloudStoreError> {
        let conn = self.lock()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM cp_invitation WHERE token_hash = ?1",
                params![hash.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        payload.map(|p| parse(&p)).transpose()
    }

    fn invitations(
        &self,
        organization: &OrganizationId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Invitation>, CloudStoreError> {
        let conn = self.lock()?;
        scoped_page(
            &conn,
            "SELECT payload FROM cp_invitation
             WHERE organization_id = ?1 AND (?2 IS NULL OR id > ?2)
             ORDER BY id LIMIT ?3",
            &[&organization.as_str(), &after, &(limit as i64)],
        )
    }

    fn put_auth_session(&self, session: &AuthSession) -> Result<(), CloudStoreError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO cp_auth_session (id, token_hash, payload) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET
                token_hash = excluded.token_hash,
                payload = excluded.payload",
            params![
                session.id.as_str(),
                session.token_hash.as_str(),
                encode(session)?,
            ],
        )
        .map_err(backend)?;
        Ok(())
    }

    fn auth_session(&self, id: &AuthSessionId) -> Result<Option<AuthSession>, CloudStoreError> {
        let conn = self.lock()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM cp_auth_session WHERE id = ?1",
                params![id.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        payload.map(|p| parse(&p)).transpose()
    }

    fn auth_session_by_token_hash(
        &self,
        hash: &TokenHash,
    ) -> Result<Option<AuthSession>, CloudStoreError> {
        let conn = self.lock()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM cp_auth_session WHERE token_hash = ?1",
                params![hash.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        payload.map(|p| parse(&p)).transpose()
    }

    fn put_service_account(&self, account: &ServiceAccount) -> Result<(), CloudStoreError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO cp_service_account (id, organization_id, token_hash, payload)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET
                organization_id = excluded.organization_id,
                token_hash = excluded.token_hash,
                payload = excluded.payload",
            params![
                account.id.as_str(),
                account.organization.as_str(),
                account.token_hash.as_str(),
                encode(account)?,
            ],
        )
        .map_err(backend)?;
        Ok(())
    }

    fn service_account(
        &self,
        id: &ServiceAccountId,
    ) -> Result<Option<ServiceAccount>, CloudStoreError> {
        let conn = self.lock()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM cp_service_account WHERE id = ?1",
                params![id.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        payload.map(|p| parse(&p)).transpose()
    }

    fn service_account_by_token_hash(
        &self,
        hash: &TokenHash,
    ) -> Result<Option<ServiceAccount>, CloudStoreError> {
        let conn = self.lock()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM cp_service_account WHERE token_hash = ?1",
                params![hash.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        payload.map(|p| parse(&p)).transpose()
    }

    fn service_accounts(
        &self,
        organization: &OrganizationId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ServiceAccount>, CloudStoreError> {
        let conn = self.lock()?;
        scoped_page(
            &conn,
            "SELECT payload FROM cp_service_account
             WHERE organization_id = ?1 AND (?2 IS NULL OR id > ?2)
             ORDER BY id LIMIT ?3",
            &[&organization.as_str(), &after, &(limit as i64)],
        )
    }

    fn put_approval(&self, approval: &ApprovalRequest) -> Result<(), CloudStoreError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO cp_approval (id, organization_id, status, payload)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET
                organization_id = excluded.organization_id,
                status = excluded.status,
                payload = excluded.payload",
            params![
                approval.id.as_str(),
                approval.organization.as_str(),
                approval.status.as_str(),
                encode(approval)?,
            ],
        )
        .map_err(backend)?;
        Ok(())
    }

    fn approval(&self, id: &ApprovalId) -> Result<Option<ApprovalRequest>, CloudStoreError> {
        let conn = self.lock()?;
        let payload: Option<String> = conn
            .query_row(
                "SELECT payload FROM cp_approval WHERE id = ?1",
                params![id.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(backend)?;
        payload.map(|p| parse(&p)).transpose()
    }

    fn approvals(
        &self,
        organization: &OrganizationId,
        status: Option<ApprovalStatus>,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ApprovalRequest>, CloudStoreError> {
        let conn = self.lock()?;
        scoped_page(
            &conn,
            "SELECT payload FROM cp_approval
             WHERE organization_id = ?1
               AND (?2 IS NULL OR status = ?2)
               AND (?3 IS NULL OR id > ?3)
             ORDER BY id LIMIT ?4",
            &[
                &organization.as_str(),
                &status.map(|s| s.as_str()),
                &after,
                &(limit as i64),
            ],
        )
    }

    fn claim_idempotent(&self, record: &IdempotencyRecord) -> Result<bool, CloudStoreError> {
        let conn = self.lock()?;
        let inserted = conn
            .execute(
                "INSERT INTO cp_idempotency (key, operation, request_hash, response, created_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(key) DO NOTHING",
                params![
                    record.key,
                    record.operation,
                    record.request_hash,
                    record.response_json,
                    record.created_ms,
                ],
            )
            .map_err(backend)?;
        Ok(inserted == 1)
    }

    fn idempotent(&self, key: &str) -> Result<Option<IdempotencyRecord>, CloudStoreError> {
        let conn = self.lock()?;
        let row = conn
            .query_row(
                "SELECT key, operation, request_hash, response, created_ms
                 FROM cp_idempotency WHERE key = ?1",
                params![key],
                |r| {
                    Ok(IdempotencyRecord {
                        key: r.get(0)?,
                        operation: r.get(1)?,
                        request_hash: r.get(2)?,
                        response_json: r.get(3)?,
                        created_ms: r.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(backend)?;
        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{ExternalIdentityId, MembershipId};
    use crate::rbac::{Action, Role};

    fn user(id: &str, email: &str) -> User {
        User {
            id: UserId::try_new(id).unwrap(),
            email: email.into(),
            display_name: id.into(),
            created_ms: 1,
            disabled: false,
        }
    }

    fn organization(id: &str) -> Organization {
        Organization {
            id: OrganizationId::try_new(id).unwrap(),
            name: id.into(),
            created_ms: 1,
            deleted: false,
        }
    }

    fn membership(id: &str, org: &str, user_id: &str) -> Membership {
        Membership {
            id: MembershipId::try_new(id).unwrap(),
            organization: OrganizationId::try_new(org).unwrap(),
            user: UserId::try_new(user_id).unwrap(),
            role: Role::Member,
            created_ms: 1,
        }
    }

    fn approval(id: &str, org: &str) -> ApprovalRequest {
        ApprovalRequest {
            id: ApprovalId::try_new(id).unwrap(),
            organization: OrganizationId::try_new(org).unwrap(),
            action: Action::RepositoryWrite,
            resource: "repo:1".into(),
            requested_by: UserId::try_new("usr_1").unwrap(),
            reason: "please".into(),
            status: ApprovalStatus::Open,
            decided_by: None,
            note: None,
            created_ms: 1,
            decided_ms: None,
        }
    }

    fn service_account(id: &str, org: &str) -> ServiceAccount {
        ServiceAccount {
            id: ServiceAccountId::try_new(id).unwrap(),
            organization: OrganizationId::try_new(org).unwrap(),
            name: "ci".into(),
            role: Role::Member,
            scopes: vec![Action::RepositoryRead],
            token_hash: TokenHash::try_new(format!("{:0>64}", id.len())).unwrap(),
            created_ms: 1,
            disabled: false,
        }
    }

    fn roundtrip(store: &dyn ControlPlaneStore) {
        store.put_user(&user("usr_1", "a@b.c")).unwrap();
        store.put_user(&user("usr_2", "d@e.f")).unwrap();
        assert_eq!(
            store.user_by_email("a@b.c").unwrap().unwrap().id.as_str(),
            "usr_1"
        );
        assert!(store.user_by_email("missing@x.y").unwrap().is_none());
        // Duplicate email under a different id is refused.
        assert!(store.put_user(&user("usr_3", "a@b.c")).is_err());
        assert_eq!(store.users(None, 10).unwrap().len(), 2);
        assert_eq!(
            store.users(Some("usr_1"), 10).unwrap()[0].id.as_str(),
            "usr_2",
            "the cursor is exclusive"
        );

        store.put_organization(&organization("org_a")).unwrap();
        store.put_organization(&organization("org_b")).unwrap();
        assert_eq!(store.organizations(None, 10).unwrap().len(), 2);
        assert_eq!(store.organizations(Some("org_a"), 10).unwrap().len(), 1);

        store
            .put_external_identity(&ExternalIdentity {
                id: ExternalIdentityId::try_new("ext_1").unwrap(),
                user: UserId::try_new("usr_1").unwrap(),
                provider: "oidc".into(),
                subject: "sub-1".into(),
                created_ms: 1,
            })
            .unwrap();
        assert_eq!(
            store
                .external_identity("oidc", "sub-1")
                .unwrap()
                .unwrap()
                .user
                .as_str(),
            "usr_1"
        );
        assert_eq!(
            store
                .external_identities_for_user(&UserId::try_new("usr_1").unwrap())
                .unwrap()
                .len(),
            1
        );

        store
            .put_membership(&membership("mem_1", "org_a", "usr_1"))
            .unwrap();
        store
            .put_membership(&membership("mem_2", "org_a", "usr_2"))
            .unwrap();
        store
            .put_membership(&membership("mem_3", "org_b", "usr_1"))
            .unwrap();
        let org_a = OrganizationId::try_new("org_a").unwrap();
        assert_eq!(store.memberships(&org_a, None, 10).unwrap().len(), 2);
        assert_eq!(
            store
                .membership(&org_a, &UserId::try_new("usr_1").unwrap())
                .unwrap()
                .unwrap()
                .id
                .as_str(),
            "mem_1"
        );
        assert!(store
            .delete_membership(&org_a, &UserId::try_new("usr_2").unwrap())
            .unwrap());
        assert!(!store
            .delete_membership(&org_a, &UserId::try_new("usr_2").unwrap())
            .unwrap());

        let invitation = Invitation {
            id: InvitationId::try_new("inv_1").unwrap(),
            organization: org_a.clone(),
            email: "d@e.f".into(),
            role: Role::Member,
            status: crate::model::InvitationStatus::Pending,
            invited_by: UserId::try_new("usr_1").unwrap(),
            token_hash: TokenHash::try_new("a".repeat(64)).unwrap(),
            created_ms: 1,
            expires_ms: 100,
            decided_ms: None,
        };
        store.put_invitation(&invitation).unwrap();
        assert_eq!(store.invitations(&org_a, None, 10).unwrap().len(), 1);
        assert_eq!(
            store
                .invitation_by_token_hash(&invitation.token_hash)
                .unwrap()
                .unwrap()
                .id
                .as_str(),
            "inv_1"
        );

        let session = AuthSession {
            id: AuthSessionId::try_new("ses_1").unwrap(),
            organization: org_a.clone(),
            user: UserId::try_new("usr_1").unwrap(),
            token_hash: TokenHash::try_new("b".repeat(64)).unwrap(),
            created_ms: 1,
            expires_ms: 100,
            revoked_ms: None,
        };
        store.put_auth_session(&session).unwrap();
        assert!(store.auth_session(&session.id).unwrap().is_some());
        assert!(store
            .auth_session_by_token_hash(&session.token_hash)
            .unwrap()
            .is_some());

        store
            .put_service_account(&service_account("sa_1", "org_a"))
            .unwrap();
        assert_eq!(
            store.service_accounts(&org_a, None, 10).unwrap().len(),
            1,
            "service accounts are scoped to their organization"
        );
        assert!(store
            .service_account(&service_account("sa_1", "org_a").id)
            .unwrap()
            .is_some());

        store.put_approval(&approval("apr_1", "org_a")).unwrap();
        store.put_approval(&approval("apr_2", "org_b")).unwrap();
        assert_eq!(store.approvals(&org_a, None, None, 10).unwrap().len(), 1);
        assert_eq!(
            store
                .approvals(&org_a, Some(ApprovalStatus::Open), None, 10)
                .unwrap()
                .len(),
            1
        );
        assert!(store
            .approvals(&org_a, Some(ApprovalStatus::Approved), None, 10)
            .unwrap()
            .is_empty());

        let record = IdempotencyRecord {
            key: "key-1".into(),
            operation: "org_create".into(),
            request_hash: "h".into(),
            response_json: "{}".into(),
            created_ms: 1,
        };
        assert!(store.claim_idempotent(&record).unwrap());
        assert!(
            !store.claim_idempotent(&record).unwrap(),
            "the second claim loses the race"
        );
        assert_eq!(
            store.idempotent("key-1").unwrap().unwrap().response_json,
            "{}"
        );
        assert!(store.idempotent("missing").unwrap().is_none());
    }

    #[test]
    fn memory_store_roundtrip() {
        roundtrip(&MemoryControlPlaneStore::new());
    }

    #[test]
    fn sqlite_store_roundtrip_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control-plane.db");
        {
            roundtrip(&SqliteControlPlaneStore::open(&path).unwrap());
        }
        let store = SqliteControlPlaneStore::open(&path).unwrap();
        assert_eq!(store.users(None, 10).unwrap().len(), 2);
        assert_eq!(
            store
                .invitations(&OrganizationId::try_new("org_a").unwrap(), None, 10)
                .unwrap()
                .len(),
            1
        );
        assert!(store.idempotent("key-1").unwrap().is_some());
    }

    #[test]
    fn poisoned_sqlite_lock_refuses_typed() {
        let store = SqliteControlPlaneStore::open_in_memory().unwrap();
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = store.conn.lock().unwrap();
            panic!("poison");
        }));
        assert!(poisoned.is_err());
        assert!(matches!(
            store.users(None, 1).unwrap_err(),
            CloudStoreError::Backend(_)
        ));
    }

    #[test]
    fn corrupt_payloads_are_typed_malformed_never_a_guess() {
        let store = SqliteControlPlaneStore::open_in_memory().unwrap();
        store.put_user(&user("usr_1", "a@b.c")).unwrap();
        {
            let conn = store.lock().unwrap();
            conn.execute(
                "UPDATE cp_user SET payload = '{not json' WHERE id = 'usr_1'",
                [],
            )
            .unwrap();
        }
        assert!(matches!(
            store.user(&UserId::try_new("usr_1").unwrap()).unwrap_err(),
            CloudStoreError::Malformed(_)
        ));
    }
}
