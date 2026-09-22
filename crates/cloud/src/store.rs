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

use crate::error::ControlPlaneError;
use crate::ids::{
    ApprovalId, AuthSessionId, InvitationId, OrganizationId, ServiceAccountId, TokenHash, UserId,
};
use crate::model::{
    ApprovalRequest, ApprovalStatus, AuthSession, ExternalIdentity, Invitation, Membership,
    Organization, ServiceAccount, User,
};

/// Hard bound on one recorded idempotent response body. The response is
/// persisted and replayed verbatim, so an unbounded body would turn one
/// mutation into an unbounded durable row (and an unbounded replay payload):
/// a response beyond this is a typed refusal that rolls the whole operation
/// back, exactly like a closure refusal.
pub const MAX_IDEMPOTENT_RESPONSE_BYTES: usize = 1024 * 1024;

/// Idempotency-journal retention: recorded operations older than this are
/// pruned on the open/backup tick. Documented trade-off: a retry that arrives
/// after the TTL is no longer deduplicated (it re-executes), which is the
/// price of a bounded journal; callers that need longer dedup windows must
/// keep their own key ledger.
pub const IDEMPOTENCY_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// Idempotency-journal retention: at most this many rows are kept (newest
/// first). Bounds a control-plane database that sees unbounded logins/links.
pub const MAX_IDEMPOTENCY_ROWS: usize = 4096;

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

/// What one [`ControlPlaneStore::execute_idempotent`] call did.
#[derive(Debug, Clone, PartialEq)]
pub enum IdempotentOutcome {
    /// The closure ran inside the transaction and its response is durable
    /// together with every domain write it performed.
    Executed(serde_json::Value),
    /// The key was already claimed: this is the WINNER's recorded response,
    /// replayed verbatim (the closure never ran).
    Replayed(serde_json::Value),
}

impl IdempotentOutcome {
    /// The response value, whether executed or replayed.
    pub fn into_response(self) -> serde_json::Value {
        match self {
            IdempotentOutcome::Executed(response) | IdempotentOutcome::Replayed(response) => {
                response
            }
        }
    }
}

/// The domain reads and writes available INSIDE one idempotent transaction.
///
/// Every method runs on the SAME transaction as the idempotency claim, so a
/// refusal, a crash or a lost race can never leave a domain write without
/// its recorded outcome (or the outcome without its writes). Implementations
/// are handed to the [`ControlPlaneStore::execute_idempotent`] closure and
/// must never be used outside it.
pub trait ControlPlaneTx {
    fn put_user(&mut self, user: &User) -> Result<(), CloudStoreError>;
    fn user(&mut self, id: &UserId) -> Result<Option<User>, CloudStoreError>;
    fn user_by_email(&mut self, email: &str) -> Result<Option<User>, CloudStoreError>;

    /// Link one external identity INSIDE the transaction: the SSO login
    /// resolves the subject and attaches it in the SAME transaction as the
    /// user resolution/creation, so no window exists where two concurrent
    /// logins could bind one subject to different accounts.
    fn put_external_identity(&mut self, identity: &ExternalIdentity)
        -> Result<(), CloudStoreError>;
    fn external_identity(
        &mut self,
        provider: &str,
        subject: &str,
    ) -> Result<Option<ExternalIdentity>, CloudStoreError>;

    fn put_organization(&mut self, organization: &Organization) -> Result<(), CloudStoreError>;
    fn organization(
        &mut self,
        id: &OrganizationId,
    ) -> Result<Option<Organization>, CloudStoreError>;

    fn put_membership(&mut self, membership: &Membership) -> Result<(), CloudStoreError>;
    fn membership(
        &mut self,
        organization: &OrganizationId,
        user: &UserId,
    ) -> Result<Option<Membership>, CloudStoreError>;

    fn put_invitation(&mut self, invitation: &Invitation) -> Result<(), CloudStoreError>;
    fn invitation_by_token_hash(
        &mut self,
        hash: &TokenHash,
    ) -> Result<Option<Invitation>, CloudStoreError>;

    fn put_auth_session(&mut self, session: &AuthSession) -> Result<(), CloudStoreError>;

    fn put_approval(&mut self, approval: &ApprovalRequest) -> Result<(), CloudStoreError>;
    fn approval(&mut self, id: &ApprovalId) -> Result<Option<ApprovalRequest>, CloudStoreError>;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CloudStoreError {
    #[error("control-plane store backend unavailable: {0}")]
    Backend(String),
    #[error("control-plane store refused a malformed row: {0}")]
    Malformed(String),
    /// A uniqueness invariant (email, (provider, subject), token hash,
    /// (organization, user)) refused the write. Both backends raise this same
    /// typed conflict; the service layer maps it to a 409.
    #[error("control-plane store refused a conflicting row: {0}")]
    Conflict(String),
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
    ///
    /// TEST-ONLY: seeding a record OUTSIDE [`Self::execute_idempotent`]
    /// bypasses the transaction machinery that binds the claim to its domain
    /// writes — a record written this way later makes a replay skip the
    /// closure, and an empty response would brick the key forever.
    /// Production callers use `execute_idempotent` exclusively.
    #[cfg(test)]
    fn claim_idempotent(&self, record: &IdempotencyRecord) -> Result<bool, CloudStoreError>;
    fn idempotent(&self, key: &str) -> Result<Option<IdempotencyRecord>, CloudStoreError>;

    /// The number of retained idempotency records (observability for the
    /// documented bounded retention).
    fn idempotency_count(&self) -> Result<usize, CloudStoreError>;

    /// Prune the idempotency journal to its documented retention: rows older
    /// than [`IDEMPOTENCY_TTL_MS`] are removed, then the oldest rows beyond
    /// the newest [`MAX_IDEMPOTENCY_ROWS`]. Returns how many were removed.
    /// Called on the open/backup tick; exposed for explicit maintenance.
    fn prune_idempotency(&self, now_ms: i64) -> Result<usize, CloudStoreError>;

    /// Execute `apply` exactly once per `(key, operation, request_digest)`.
    ///
    /// The idempotency claim (`INSERT OR IGNORE` with a response
    /// placeholder), every domain mutation `apply` performs through
    /// [`ControlPlaneTx`], and the recorded safe response commit in ONE
    /// transaction. A lost claim race NEVER runs the closure: it replays the
    /// winner's durable response instead. The same key under a different
    /// operation or request digest is a typed [`ControlPlaneError::Conflict`].
    /// A closure refusal (or any store failure) rolls the whole transaction
    /// back, so no partial domain write survives and the key stays free for
    /// a retry.
    fn execute_idempotent(
        &self,
        key: &str,
        operation: &str,
        request_digest: &str,
        now_ms: i64,
        apply: &mut dyn FnMut(
            &mut dyn ControlPlaneTx,
        ) -> Result<serde_json::Value, ControlPlaneError>,
    ) -> Result<IdempotentOutcome, ControlPlaneError>;
}

// ------------------------------------------------------------- in-memory

#[derive(Default)]
struct MemInner {
    users: BTreeMap<String, User>,
    organizations: BTreeMap<String, Organization>,
    external_identities: BTreeMap<(String, String), ExternalIdentity>,
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
    /// The additive enterprise slice (migration v3 domain): audit ledger,
    /// retention artifacts, deletion jobs, settings, tombstones, config
    /// layers and admission freezes. One lock, one in-memory authority.
    enterprise: Mutex<crate::enterprise_store::EnterpriseMem>,
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

    /// The enterprise slice of the in-memory authority (the additive
    /// enterprise store impl lives in `enterprise_store`).
    pub(crate) fn lock_enterprise(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, crate::enterprise_store::EnterpriseMem>, CloudStoreError>
    {
        self.enterprise.lock().map_err(|_| {
            CloudStoreError::Backend("in-memory enterprise store lock is poisoned".into())
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

// The in-memory authority is shared by the lock-holding store methods and by
// [`MemoryTx`]; the helpers keep ONE implementation of every invariant (the
// unique-email / unique-membership refusals included) for both paths.

fn mem_put_user(inner: &mut MemInner, user: &User) -> Result<(), CloudStoreError> {
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

fn mem_user(inner: &MemInner, id: &UserId) -> Option<User> {
    inner.users.get(id.as_str()).cloned()
}

fn mem_user_by_email(inner: &MemInner, email: &str) -> Option<User> {
    inner.users.values().find(|u| u.email == email).cloned()
}

fn mem_external_identity(
    inner: &MemInner,
    provider: &str,
    subject: &str,
) -> Option<ExternalIdentity> {
    inner
        .external_identities
        .get(&(provider.to_string(), subject.to_string()))
        .cloned()
}

/// Attach one external identity under the (provider, subject) uniqueness
/// invariant: a second id may never claim an already-bound subject. The key is
/// the TUPLE, never a delimiter-joined string: subjects/providers may contain
/// any byte (incl. `:`), and `("a:b", "c")` must not alias `("a", "b:c")` the
/// way a `format!("{provider}:{subject}")` key would (SQLite's
/// `UNIQUE(provider, subject)` draws the same line).
fn mem_put_external_identity(
    inner: &mut MemInner,
    identity: &ExternalIdentity,
) -> Result<(), CloudStoreError> {
    let key = (identity.provider.clone(), identity.subject.clone());
    if let Some(existing) = inner.external_identities.get(&key) {
        if existing.id != identity.id {
            return Err(CloudStoreError::Malformed(format!(
                "external identity ({}, {}) is already linked",
                identity.provider, identity.subject
            )));
        }
    }
    inner.external_identities.insert(key, identity.clone());
    Ok(())
}

fn mem_put_organization(inner: &mut MemInner, organization: &Organization) {
    inner
        .organizations
        .insert(organization.id.as_str().to_string(), organization.clone());
}

fn mem_organization(inner: &MemInner, id: &OrganizationId) -> Option<Organization> {
    inner.organizations.get(id.as_str()).cloned()
}

fn mem_put_membership(
    inner: &mut MemInner,
    membership: &Membership,
) -> Result<(), CloudStoreError> {
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

fn mem_membership(
    inner: &MemInner,
    organization: &OrganizationId,
    user: &UserId,
) -> Option<Membership> {
    inner
        .memberships
        .values()
        .find(|m| m.organization == *organization && m.user == *user)
        .cloned()
}

/// Invitations carry a one-shot token stored only as its hash; SQLite
/// enforces `UNIQUE(token_hash)`, so memory must refuse a second row claiming
/// the same hash (typed conflict) instead of silently aliasing the token.
fn mem_put_invitation(
    inner: &mut MemInner,
    invitation: &Invitation,
) -> Result<(), CloudStoreError> {
    if let Some(existing) = inner
        .invitations
        .values()
        .find(|i| i.token_hash == invitation.token_hash && i.id != invitation.id)
    {
        return Err(CloudStoreError::Conflict(format!(
            "invitation token hash is already claimed by {}",
            existing.id
        )));
    }
    inner
        .invitations
        .insert(invitation.id.as_str().to_string(), invitation.clone());
    Ok(())
}

fn mem_invitation_by_token_hash(inner: &MemInner, hash: &TokenHash) -> Option<Invitation> {
    inner
        .invitations
        .values()
        .find(|i| i.token_hash == *hash)
        .cloned()
}

/// Auth sessions are looked up by token hash; the hash is unique in SQLite
/// and must be unique here too.
fn mem_put_auth_session(
    inner: &mut MemInner,
    session: &AuthSession,
) -> Result<(), CloudStoreError> {
    if let Some(existing) = inner
        .auth_sessions
        .values()
        .find(|s| s.token_hash == session.token_hash && s.id != session.id)
    {
        return Err(CloudStoreError::Conflict(format!(
            "auth session token hash is already claimed by {}",
            existing.id
        )));
    }
    inner
        .auth_sessions
        .insert(session.id.as_str().to_string(), session.clone());
    Ok(())
}

/// Service-account bearer tokens are looked up by hash; the hash is unique in
/// SQLite and must be unique here too.
fn mem_put_service_account(
    inner: &mut MemInner,
    account: &ServiceAccount,
) -> Result<(), CloudStoreError> {
    if let Some(existing) = inner
        .service_accounts
        .values()
        .find(|s| s.token_hash == account.token_hash && s.id != account.id)
    {
        return Err(CloudStoreError::Conflict(format!(
            "service account token hash is already claimed by {}",
            existing.id
        )));
    }
    inner
        .service_accounts
        .insert(account.id.as_str().to_string(), account.clone());
    Ok(())
}

fn mem_put_approval(inner: &mut MemInner, approval: &ApprovalRequest) {
    inner
        .approvals
        .insert(approval.id.as_str().to_string(), approval.clone());
}

fn mem_approval(inner: &MemInner, id: &ApprovalId) -> Option<ApprovalRequest> {
    inner.approvals.get(id.as_str()).cloned()
}

/// One undo entry of a [`MemoryTx`]: it restores the touched key to the
/// state observed BEFORE the write (or removes it when there was none).
type MemUndo = Box<dyn FnOnce(&mut MemInner)>;

/// [`ControlPlaneTx`] over the locked in-memory authority. The store lock is
/// held for the whole closure, so every mutate/read is one critical section.
/// Write methods journal the previous value of every touched key; a refusal
/// (or any later failure) replays the journal in reverse, so the in-memory
/// authority has exactly the SQLite transaction's all-or-nothing semantics.
struct MemoryTx<'a> {
    inner: &'a mut MemInner,
    undo: Vec<MemUndo>,
}

impl ControlPlaneTx for MemoryTx<'_> {
    fn put_user(&mut self, user: &User) -> Result<(), CloudStoreError> {
        let key = user.id.as_str().to_string();
        let previous = mem_user(self.inner, &user.id);
        mem_put_user(self.inner, user)?;
        let undo = Box::new(move |inner: &mut MemInner| match previous {
            Some(value) => {
                inner.users.insert(key, value);
            }
            None => {
                inner.users.remove(&key);
            }
        });
        self.undo.push(undo);
        Ok(())
    }

    fn user(&mut self, id: &UserId) -> Result<Option<User>, CloudStoreError> {
        Ok(mem_user(self.inner, id))
    }

    fn user_by_email(&mut self, email: &str) -> Result<Option<User>, CloudStoreError> {
        Ok(mem_user_by_email(self.inner, email))
    }

    fn put_external_identity(
        &mut self,
        identity: &ExternalIdentity,
    ) -> Result<(), CloudStoreError> {
        // The map key IS the (provider, subject) uniqueness invariant, so a
        // re-link of an already-bound subject cannot smuggle in a second row.
        let key = (identity.provider.clone(), identity.subject.clone());
        let previous = self.inner.external_identities.get(&key).cloned();
        mem_put_external_identity(self.inner, identity)?;
        let undo = Box::new(move |inner: &mut MemInner| match previous {
            Some(value) => {
                inner.external_identities.insert(key, value);
            }
            None => {
                inner.external_identities.remove(&key);
            }
        });
        self.undo.push(undo);
        Ok(())
    }

    fn external_identity(
        &mut self,
        provider: &str,
        subject: &str,
    ) -> Result<Option<ExternalIdentity>, CloudStoreError> {
        Ok(mem_external_identity(self.inner, provider, subject))
    }

    fn put_organization(&mut self, organization: &Organization) -> Result<(), CloudStoreError> {
        let key = organization.id.as_str().to_string();
        let previous = mem_organization(self.inner, &organization.id);
        mem_put_organization(self.inner, organization);
        let undo = Box::new(move |inner: &mut MemInner| match previous {
            Some(value) => {
                inner.organizations.insert(key, value);
            }
            None => {
                inner.organizations.remove(&key);
            }
        });
        self.undo.push(undo);
        Ok(())
    }

    fn organization(
        &mut self,
        id: &OrganizationId,
    ) -> Result<Option<Organization>, CloudStoreError> {
        Ok(mem_organization(self.inner, id))
    }

    fn put_membership(&mut self, membership: &Membership) -> Result<(), CloudStoreError> {
        let key = membership.id.as_str().to_string();
        let previous = self.inner.memberships.get(&key).cloned();
        mem_put_membership(self.inner, membership)?;
        let undo = Box::new(move |inner: &mut MemInner| match previous {
            Some(value) => {
                inner.memberships.insert(key, value);
            }
            None => {
                inner.memberships.remove(&key);
            }
        });
        self.undo.push(undo);
        Ok(())
    }

    fn membership(
        &mut self,
        organization: &OrganizationId,
        user: &UserId,
    ) -> Result<Option<Membership>, CloudStoreError> {
        Ok(mem_membership(self.inner, organization, user))
    }

    fn put_invitation(&mut self, invitation: &Invitation) -> Result<(), CloudStoreError> {
        let key = invitation.id.as_str().to_string();
        let previous = self.inner.invitations.get(&key).cloned();
        mem_put_invitation(self.inner, invitation)?;
        let undo = Box::new(move |inner: &mut MemInner| match previous {
            Some(value) => {
                inner.invitations.insert(key, value);
            }
            None => {
                inner.invitations.remove(&key);
            }
        });
        self.undo.push(undo);
        Ok(())
    }

    fn invitation_by_token_hash(
        &mut self,
        hash: &TokenHash,
    ) -> Result<Option<Invitation>, CloudStoreError> {
        Ok(mem_invitation_by_token_hash(self.inner, hash))
    }

    fn put_auth_session(&mut self, session: &AuthSession) -> Result<(), CloudStoreError> {
        let key = session.id.as_str().to_string();
        let previous = self.inner.auth_sessions.get(&key).cloned();
        mem_put_auth_session(self.inner, session)?;
        let undo = Box::new(move |inner: &mut MemInner| match previous {
            Some(value) => {
                inner.auth_sessions.insert(key, value);
            }
            None => {
                inner.auth_sessions.remove(&key);
            }
        });
        self.undo.push(undo);
        Ok(())
    }

    fn put_approval(&mut self, approval: &ApprovalRequest) -> Result<(), CloudStoreError> {
        let key = approval.id.as_str().to_string();
        let previous = self.inner.approvals.get(&key).cloned();
        mem_put_approval(self.inner, approval);
        let undo = Box::new(move |inner: &mut MemInner| match previous {
            Some(value) => {
                inner.approvals.insert(key, value);
            }
            None => {
                inner.approvals.remove(&key);
            }
        });
        self.undo.push(undo);
        Ok(())
    }

    fn approval(&mut self, id: &ApprovalId) -> Result<Option<ApprovalRequest>, CloudStoreError> {
        Ok(mem_approval(self.inner, id))
    }
}

impl ControlPlaneStore for MemoryControlPlaneStore {
    fn put_user(&self, user: &User) -> Result<(), CloudStoreError> {
        mem_put_user(&mut *self.lock()?, user)
    }

    fn user(&self, id: &UserId) -> Result<Option<User>, CloudStoreError> {
        Ok(mem_user(&*self.lock()?, id))
    }

    fn user_by_email(&self, email: &str) -> Result<Option<User>, CloudStoreError> {
        Ok(mem_user_by_email(&*self.lock()?, email))
    }

    fn users(&self, after: Option<&str>, limit: usize) -> Result<Vec<User>, CloudStoreError> {
        Ok(page_by_id(&self.lock()?.users, after, limit, |u| {
            u.id.as_str()
        }))
    }

    fn put_organization(&self, organization: &Organization) -> Result<(), CloudStoreError> {
        mem_put_organization(&mut *self.lock()?, organization);
        Ok(())
    }

    fn organization(&self, id: &OrganizationId) -> Result<Option<Organization>, CloudStoreError> {
        Ok(mem_organization(&*self.lock()?, id))
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
        mem_put_external_identity(&mut *self.lock()?, identity)
    }

    fn external_identity(
        &self,
        provider: &str,
        subject: &str,
    ) -> Result<Option<ExternalIdentity>, CloudStoreError> {
        Ok(mem_external_identity(&*self.lock()?, provider, subject))
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
        mem_put_membership(&mut *self.lock()?, membership)
    }

    fn membership(
        &self,
        organization: &OrganizationId,
        user: &UserId,
    ) -> Result<Option<Membership>, CloudStoreError> {
        Ok(mem_membership(&*self.lock()?, organization, user))
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
        mem_put_invitation(&mut *self.lock()?, invitation)
    }

    fn invitation(&self, id: &InvitationId) -> Result<Option<Invitation>, CloudStoreError> {
        Ok(self.lock()?.invitations.get(id.as_str()).cloned())
    }

    fn invitation_by_token_hash(
        &self,
        hash: &TokenHash,
    ) -> Result<Option<Invitation>, CloudStoreError> {
        Ok(mem_invitation_by_token_hash(&*self.lock()?, hash))
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
        mem_put_auth_session(&mut *self.lock()?, session)
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
        mem_put_service_account(&mut *self.lock()?, account)
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
        mem_put_approval(&mut *self.lock()?, approval);
        Ok(())
    }

    fn approval(&self, id: &ApprovalId) -> Result<Option<ApprovalRequest>, CloudStoreError> {
        Ok(mem_approval(&*self.lock()?, id))
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

    #[cfg(test)]
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

    fn idempotency_count(&self) -> Result<usize, CloudStoreError> {
        Ok(self.lock()?.idempotency.len())
    }

    fn prune_idempotency(&self, now_ms: i64) -> Result<usize, CloudStoreError> {
        let mut inner = self.lock()?;
        let before = inner.idempotency.len();
        let cutoff = now_ms.saturating_sub(IDEMPOTENCY_TTL_MS);
        inner.idempotency.retain(|_, row| row.created_ms >= cutoff);
        if inner.idempotency.len() > MAX_IDEMPOTENCY_ROWS {
            let mut oldest: Vec<(i64, String)> = inner
                .idempotency
                .values()
                .map(|row| (row.created_ms, row.key.clone()))
                .collect();
            oldest.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            let excess = inner.idempotency.len() - MAX_IDEMPOTENCY_ROWS;
            for (_, key) in oldest.into_iter().take(excess) {
                inner.idempotency.remove(&key);
            }
        }
        Ok(before - inner.idempotency.len())
    }

    fn execute_idempotent(
        &self,
        key: &str,
        operation: &str,
        request_digest: &str,
        now_ms: i64,
        apply: &mut dyn FnMut(
            &mut dyn ControlPlaneTx,
        ) -> Result<serde_json::Value, ControlPlaneError>,
    ) -> Result<IdempotentOutcome, ControlPlaneError> {
        // The mutex is held for claim + closure + record: the whole logical
        // operation is one critical section, so a concurrent caller either
        // observes no record (and claims) or the committed record (and
        // replays); it can never interleave with a half-written operation.
        let mut inner = self.lock()?;
        if let Some(existing) = inner.idempotency.get(key).cloned() {
            return replay_record(key, operation, request_digest, &existing);
        }
        let (response, response_json) = {
            let mut tx = MemoryTx {
                inner: &mut inner,
                undo: Vec::new(),
            };
            match apply(&mut tx) {
                Ok(response) => match recorded_response_json(&response) {
                    Ok(json) => (response, json),
                    Err(e) => {
                        // A response that cannot be persisted leaves NO domain
                        // write behind: the journal replays exactly as for a
                        // closure refusal (the SQLite ROLLBACK twin).
                        replay_memory_undo(tx);
                        return Err(e);
                    }
                },
                Err(e) => {
                    // Replay the write journal in reverse: the in-memory
                    // authority is left exactly as if the operation had never
                    // started (the SQLite ROLLBACK twin).
                    replay_memory_undo(tx);
                    return Err(e);
                }
            }
        };
        inner.idempotency.insert(
            key.to_string(),
            IdempotencyRecord {
                key: key.to_string(),
                operation: operation.to_string(),
                request_hash: request_digest.to_string(),
                response_json,
                created_ms: now_ms,
            },
        );
        Ok(IdempotentOutcome::Executed(response))
    }
}

/// Replay a memory transaction's undo journal in reverse: the in-memory
/// authority is left exactly as if the operation had never started (the
/// SQLite ROLLBACK twin).
fn replay_memory_undo(tx: MemoryTx<'_>) {
    let MemoryTx { inner, undo } = tx;
    for undo in undo.into_iter().rev() {
        undo(inner);
    }
}

/// Encode one recorded idempotent response under the hard bound. An encode or
/// bound failure is a typed refusal that rolls the whole operation back (the
/// response is persisted and replayed verbatim; unbounded bodies would turn
/// one mutation into an unbounded durable row).
fn recorded_response_json(response: &serde_json::Value) -> Result<String, ControlPlaneError> {
    let encoded = serde_json::to_string(response)
        .map_err(|e| ControlPlaneError::Malformed(format!("recorded response encode: {e}")))?;
    if encoded.len() > MAX_IDEMPOTENT_RESPONSE_BYTES {
        return Err(ControlPlaneError::Malformed(format!(
            "recorded response is {} bytes, beyond the {MAX_IDEMPOTENT_RESPONSE_BYTES}-byte bound",
            encoded.len()
        )));
    }
    Ok(encoded)
}

/// Decode a recorded response for the same `(operation, request_digest)`;
/// any other use of the key is a typed conflict.
fn replay_record(
    key: &str,
    operation: &str,
    request_digest: &str,
    existing: &IdempotencyRecord,
) -> Result<IdempotentOutcome, ControlPlaneError> {
    if existing.operation != operation || existing.request_hash != request_digest {
        return Err(ControlPlaneError::Conflict(format!(
            "idempotency key {key:?} was already used for a different request"
        )));
    }
    let replayed = serde_json::from_str(&existing.response_json).map_err(|e| {
        ControlPlaneError::Backend(format!("recorded idempotent response is unreadable: {e}"))
    })?;
    Ok(IdempotentOutcome::Replayed(replayed))
}

// ---------------------------------------------------------------- sqlite

/// The durable [`ControlPlaneStore`] over its own SQLite database file.
///
/// Durability policy (P1 audit): the writer connection opens with
/// `synchronous = FULL` (see [`crate::durability`]), so every acknowledged
/// commit — credit grants, usage settles, invitations, approvals — fsyncs its
/// WAL frame before the caller sees `Ok`. File-backed opens additionally
/// record the policy marker, write a VERIFIED pre-migration restore point
/// before any schema migration, and run the interval-gated rotating backup.
/// This database is the LOCAL commercial deployment authority; hosted
/// deployments must move these authorities to a transactional service.
pub struct SqliteControlPlaneStore {
    conn: Mutex<Connection>,
    /// The database path (`None` for in-memory stores): the durability stack
    /// (backups, fingerprints, doctor) needs it.
    path: Option<std::path::PathBuf>,
    /// Test-only crash-injection seam: when below `usize::MAX`, the next
    /// `execute_idempotent` transaction fails after this many statements
    /// (each claim/domain/persist statement counts as one).
    #[cfg(test)]
    crash_after: std::sync::atomic::AtomicUsize,
}

const CP_MIGRATIONS: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS cp_user (
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
     );",
    // v2 — the Wave 3 commercial metering tables (usage ledger, credit
    // ledger, billing accounts/subscriptions, in-flight transactions). The
    // billing domain OWNS this next user_version of the shared ladder; the
    // SQL lives beside its store implementation and is append-only by
    // construction (no UPDATE/DELETE ever names usage_event/credit_entry).
    crate::billing_store::BILLING_SCHEMA_V2,
    // v3 — the enterprise plane tables (audit ledger, retention artifacts,
    // deletion jobs, admin settings, tombstones, config layers, admission
    // freezes). Like v2, the domain OWNS this next user_version of the
    // shared ladder and the SQL lives beside its store implementation; the
    // audit table is append-only by construction (no UPDATE/DELETE ever
    // names ent_audit_event).
    crate::enterprise_store::ENTERPRISE_SCHEMA_V3,
    // v4 — the durable billing-report schedule rows (period + cursor). Like
    // v2/v3, the billing domain owns this ladder slot; a REPORTED/FAILED
    // period row is terminal, so the report can never double-send a period.
    crate::billing_store::BILLING_REPORT_SCHEMA_V4,
    // v5 — the durability policy marker (P1 audit): the writer RECORDS the
    // acknowledged-durability policy (`synchronous = FULL`) so `doctor` can
    // observe it (SQLite pragmas are connection-scoped and invisible to a
    // separate probe connection). Owned by the durability stack.
    crate::durability::POLICY_SCHEMA_V5,
    // v6 — the usage ledger's task identity (P1 identity-integrity): rebuild
    // `usage_event.task_id` from the lossy signed INTEGER projection to the
    // reversible fixed-width 16-hex-digit TEXT encoding. The rebuild is
    // staged by SQL (the billing domain owns this ladder slot) and finalized
    // in Rust BEFORE the migration transaction commits, because SQLite JSON
    // cannot carry ids above i64::MAX exactly; a row that cannot be
    // recovered exactly refuses the whole migration rather than guessing.
    crate::billing_store::BILLING_TASK_ID_TEXT_SCHEMA_V6,
];

impl SqliteControlPlaneStore {
    /// Open (creating) the control-plane database at `path`.
    pub fn open(path: &Path) -> Result<Self, CloudStoreError> {
        let conn = Connection::open(path).map_err(backend)?;
        Self::prepare(conn, Some(path))
    }

    /// Open an in-memory database (tests, ephemeral hosts).
    pub fn open_in_memory() -> Result<Self, CloudStoreError> {
        let conn = Connection::open_in_memory().map_err(backend)?;
        Self::prepare(conn, None)
    }

    fn prepare(conn: Connection, path: Option<&Path>) -> Result<Self, CloudStoreError> {
        // The acknowledged-durability policy (WAL + synchronous = FULL; see
        // `crate::durability` for the documented choice) applies to EVERY
        // open, in-memory included, before any migration or query.
        crate::durability::apply_policy(&conn)?;
        let mut conn = conn;
        let started_version = migrate(&mut conn, path)?;
        if let Some(path) = path {
            let now = crate::durability::now_ms();
            // Record the writer's policy for `doctor` (best effort: a full
            // disk must not take the control plane down; doctor then reports
            // the absent/stale marker loudly).
            if let Err(e) = crate::durability::record_open_policy(&conn, now) {
                tracing::error!("control-plane durability marker not recorded: {e}");
            }
            // This database was opened with pending migrations, so it has (and
            // keeps requiring) a pre-migration restore point: doctor fails
            // loudly when one is missing.
            if started_version < CP_MIGRATIONS.len() as i64 {
                if let Err(e) = crate::durability::require_migration_restore_point(&conn) {
                    tracing::warn!("control-plane restore-point policy not recorded: {e}");
                }
            }
            // Bounded idempotency retention runs on the same open/backup tick
            // as the rotating backup (documented TTL + count).
            if let Err(e) = prune_idempotency_tick(&conn) {
                tracing::warn!("control-plane idempotency prune skipped: {e}");
            }
            // Interval-gated verified backup (main store convention). Best
            // effort, like the daemon's startup backup. The tick's own writes
            // count as a change (the safe direction: one extra snapshot, never
            // a hidden commit).
            match crate::durability::rotate_backup(&conn, path) {
                Ok(Some(dest)) => {
                    tracing::info!("commercial backup written to {}", dest.display());
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("commercial backup skipped: {e}"),
            }
        }
        Ok(Self {
            conn: Mutex::new(conn),
            path: path.map(Path::to_path_buf),
            #[cfg(test)]
            crash_after: std::sync::atomic::AtomicUsize::new(usize::MAX),
        })
    }

    /// The database path (`None` for in-memory stores).
    pub fn db_path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// The full `PRAGMA integrity_check` over the live database.
    pub fn integrity_check(&self) -> Result<Vec<String>, CloudStoreError> {
        let conn = self.lock()?;
        crate::durability::integrity_check(&conn, true)
    }

    /// The canonical schema/row-count fingerprint (restore verification).
    pub fn fingerprint(&self) -> Result<crate::durability::DbFingerprint, CloudStoreError> {
        let conn = self.lock()?;
        crate::durability::canonical_fingerprint(&conn)
    }

    /// Online backup into `dest` through the SQLite backup API (the main
    /// store's `Store::backup_to` convention). The caller owns verification:
    /// [`crate::durability::restore_verify`] against [`Self::fingerprint`].
    pub fn backup_to(&self, dest: &Path) -> Result<(), CloudStoreError> {
        let conn = self.lock()?;
        crate::durability::backup_to(&conn, dest)
    }

    /// The read-only durability report (`doctor`'s `cloud-db` section).
    pub fn durability_report(
        &self,
        deep: bool,
    ) -> Result<crate::durability::CommercialDbReport, CloudStoreError> {
        let path = self.path.as_deref().ok_or_else(|| {
            CloudStoreError::Backend("in-memory store has no durability file".into())
        })?;
        crate::durability::doctor_probe(path, deep)
    }

    /// Test-only: fail the NEXT `execute_idempotent` transaction once it has
    /// executed `statements` statements (the claim counts as the first), so
    /// every statement boundary can be crash-injected. `usize::MAX`
    /// disables the seam.
    #[cfg(test)]
    pub(crate) fn inject_crash_after(&self, statements: usize) {
        self.crash_after
            .store(statements, std::sync::atomic::Ordering::SeqCst);
    }

    /// The shared connection lock. `pub(crate)` so the additive billing
    /// store (the same SQLite file, its own migration v2 tables) can run its
    /// append-only transactions through the ONE writer connection.
    pub(crate) fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, CloudStoreError> {
        self.conn
            .lock()
            .map_err(|_| CloudStoreError::Backend("control-plane store lock is poisoned".into()))
    }
}

/// Test-only one-shot: make the NEXT migration of exactly `db_path` fail
/// AFTER the pre-migration restore point and BEFORE any migration SQL,
/// reproducing the crash-mid-migration durable state. Keyed by path so
/// concurrent tests never consume each other's injection.
#[cfg(test)]
static MIGRATION_CRASH: std::sync::Mutex<Option<std::path::PathBuf>> = std::sync::Mutex::new(None);

/// Arm [`MIGRATION_CRASH`] for `db_path` (one-shot; tests only).
#[cfg(test)]
pub(crate) fn inject_crash_before_migration(db_path: &Path) {
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

/// Test-only statement budget of one idempotent transaction. `tick` is
/// called BEFORE every statement: once more than `limit` statements would
/// run, the transaction fails (and therefore rolls back).
#[cfg(test)]
struct CrashBudget {
    limit: usize,
    executed: std::cell::Cell<usize>,
}

#[cfg(test)]
impl CrashBudget {
    fn tick(&self) -> Result<(), CloudStoreError> {
        let executed = self.executed.get() + 1;
        self.executed.set(executed);
        if executed > self.limit {
            return Err(CloudStoreError::Backend(format!(
                "injected crash after {} statements",
                self.limit
            )));
        }
        Ok(())
    }
}

/// [`ControlPlaneTx`] over one live SQLite transaction (the same connection
/// that holds the idempotency claim).
struct SqliteTx<'a> {
    tx: rusqlite::Transaction<'a>,
    #[cfg(test)]
    fault: Option<&'a CrashBudget>,
}

impl SqliteTx<'_> {
    fn tick(&self) -> Result<(), CloudStoreError> {
        #[cfg(test)]
        if let Some(fault) = self.fault {
            fault.tick()?;
        }
        Ok(())
    }
}

impl ControlPlaneTx for SqliteTx<'_> {
    fn put_user(&mut self, user: &User) -> Result<(), CloudStoreError> {
        self.tick()?;
        sql_put_user(&self.tx, user)
    }

    fn user(&mut self, id: &UserId) -> Result<Option<User>, CloudStoreError> {
        self.tick()?;
        sql_user(&self.tx, id)
    }

    fn user_by_email(&mut self, email: &str) -> Result<Option<User>, CloudStoreError> {
        self.tick()?;
        sql_user_by_email(&self.tx, email)
    }

    fn put_external_identity(
        &mut self,
        identity: &ExternalIdentity,
    ) -> Result<(), CloudStoreError> {
        self.tick()?;
        sql_put_external_identity(&self.tx, identity)
    }

    fn external_identity(
        &mut self,
        provider: &str,
        subject: &str,
    ) -> Result<Option<ExternalIdentity>, CloudStoreError> {
        self.tick()?;
        sql_external_identity(&self.tx, provider, subject)
    }

    fn put_organization(&mut self, organization: &Organization) -> Result<(), CloudStoreError> {
        self.tick()?;
        sql_put_organization(&self.tx, organization)
    }

    fn organization(
        &mut self,
        id: &OrganizationId,
    ) -> Result<Option<Organization>, CloudStoreError> {
        self.tick()?;
        sql_organization(&self.tx, id)
    }

    fn put_membership(&mut self, membership: &Membership) -> Result<(), CloudStoreError> {
        self.tick()?;
        sql_put_membership(&self.tx, membership)
    }

    fn membership(
        &mut self,
        organization: &OrganizationId,
        user: &UserId,
    ) -> Result<Option<Membership>, CloudStoreError> {
        self.tick()?;
        sql_membership(&self.tx, organization, user)
    }

    fn put_invitation(&mut self, invitation: &Invitation) -> Result<(), CloudStoreError> {
        self.tick()?;
        sql_put_invitation(&self.tx, invitation)
    }

    fn invitation_by_token_hash(
        &mut self,
        hash: &TokenHash,
    ) -> Result<Option<Invitation>, CloudStoreError> {
        self.tick()?;
        sql_invitation_by_token_hash(&self.tx, hash)
    }

    fn put_auth_session(&mut self, session: &AuthSession) -> Result<(), CloudStoreError> {
        self.tick()?;
        sql_put_auth_session(&self.tx, session)
    }

    fn put_approval(&mut self, approval: &ApprovalRequest) -> Result<(), CloudStoreError> {
        self.tick()?;
        sql_put_approval(&self.tx, approval)
    }

    fn approval(&mut self, id: &ApprovalId) -> Result<Option<ApprovalRequest>, CloudStoreError> {
        self.tick()?;
        sql_approval(&self.tx, id)
    }
}

fn backend(e: rusqlite::Error) -> CloudStoreError {
    CloudStoreError::Backend(e.to_string())
}

/// Bounded idempotency retention (TTL + count), shared by the store method
/// and the open/backup tick. Newest rows always survive.
fn prune_idempotency_sql(conn: &Connection, now_ms: i64) -> Result<usize, CloudStoreError> {
    let cutoff = now_ms.saturating_sub(IDEMPOTENCY_TTL_MS);
    let mut removed = conn
        .execute(
            "DELETE FROM cp_idempotency WHERE created_ms < ?1",
            params![cutoff],
        )
        .map_err(backend)?;
    removed += prune_idempotency_excess(conn)?;
    Ok(removed)
}

/// The count half of the retention: keep only the newest
/// [`MAX_IDEMPOTENCY_ROWS`] rows.
fn prune_idempotency_excess(conn: &Connection) -> Result<usize, CloudStoreError> {
    conn.execute(
        "DELETE FROM cp_idempotency WHERE key IN (
             SELECT key FROM cp_idempotency
             ORDER BY created_ms DESC, key DESC
             LIMIT -1 OFFSET ?1
         )",
        params![MAX_IDEMPOTENCY_ROWS as i64],
    )
    .map_err(backend)
}

/// Open/backup-tick prune: the TTL is measured against the journal's OWN
/// newest row, not the host wall clock, so a store whose event clock differs
/// from the tick's wall clock can never mass-delete live keys on open; the
/// count bound still applies. Returns how many rows were removed.
fn prune_idempotency_tick(conn: &Connection) -> Result<usize, CloudStoreError> {
    let mut removed = conn
        .execute(
            "DELETE FROM cp_idempotency
             WHERE created_ms <
                   (SELECT COALESCE(MAX(created_ms), 0) FROM cp_idempotency) - ?1",
            params![IDEMPOTENCY_TTL_MS],
        )
        .map_err(backend)?;
    removed += prune_idempotency_excess(conn)?;
    Ok(removed)
}

/// Apply the control-plane schema ladder. Returns the version the database
/// was at when the (serialized) migration began, so the caller can record
/// that migration restore points are required for it.
///
/// Serialization: the version read, the pre-migration restore point and every
/// migration statement run inside ONE `BEGIN IMMEDIATE` transaction. A second
/// concurrent opener blocks on the write lock, then re-reads the (already
/// advanced) version inside its own transaction and skips — it can never
/// snapshot post-migration content and label it `-pre-migration-vN-`.
///
/// A NEGATIVE `user_version` (impossible for a database this ladder created)
/// is refused typed as corruption, before any snapshot or migration.
fn migrate(conn: &mut Connection, db_path: Option<&Path>) -> Result<i64, CloudStoreError> {
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(backend)?;
    let started: i64 = tx
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(backend)?;
    let ladder = CP_MIGRATIONS.len() as i64;
    // A NEGATIVE `user_version` cannot have been produced by any legitimate
    // open of this ladder (SQLite stores the pragma as a SIGNED integer and
    // every writer here only ever moves it forward from 0). It is durable
    // corruption, and it is refused typed BEFORE the restore point and the
    // ladder: a snapshot labeled `-pre-migration-v-1-` would be a
    // trusted-looking way back to a state this binary never created, and
    // treating it as v0 would silently migrate corrupt state.
    if started < 0 {
        return Err(CloudStoreError::Malformed(format!(
            "control-plane database schema user_version {started} is corrupt (negative): \
             refusing to snapshot or migrate it"
        )));
    }
    // A database written by a NEWER binary must never be silently opened by
    // an older one: the newer ladder may have changed semantics this binary
    // cannot honour. Downgrade is refused typed and diagnosed by `doctor`;
    // the recovery path is running the newer binary or restoring the
    // pre-upgrade restore point.
    if started > ladder {
        return Err(CloudStoreError::Backend(format!(
            "control-plane database schema v{started} is newer than this binary's ladder v{ladder}: \
             downgrade refused (run the newer binary or restore the pre-upgrade restore point)"
        )));
    }
    if started == ladder {
        tx.commit().map_err(backend)?;
        return Ok(started);
    }
    let mut version = started;
    // A schema transition (or FIRST creation) on a file-backed database runs
    // only after a verified restore point of the pre-migration state exists;
    // if it cannot be written and self-verified, the migration is REFUSED.
    // The snapshot runs on a SEPARATE read-only connection: the backup API
    // cannot run on a connection that holds a write transaction, and the
    // BEGIN IMMEDIATE lock we hold makes every reader see exactly this
    // pre-migration state.
    if let Some(path) = db_path {
        let reader = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(backend)?;
        crate::durability::migration_backup(&reader, path, version).map_err(|e| {
            CloudStoreError::Backend(format!(
                "refusing migration without a verified pre-migration restore point: {e}"
            ))
        })?;
        drop(reader);
        #[cfg(test)]
        if take_injected_crash(path) {
            return Err(CloudStoreError::Backend(
                "injected crash after the pre-migration restore point".into(),
            ));
        }
    }
    for (i, sql) in CP_MIGRATIONS.iter().enumerate() {
        let target = (i + 1) as i64;
        if version >= target {
            continue;
        }
        tx.execute_batch(sql)
            .map_err(|e| CloudStoreError::Backend(format!("cp migration v{target}: {e}")))?;
        tx.execute_batch(&format!("PRAGMA user_version = {target}"))
            .map_err(|e| CloudStoreError::Backend(format!("cp migration v{target} cursor: {e}")))?;
        version = target;
    }
    // The billing domain's v6 rebuild stages every legacy task id and
    // resolves its exact reversible text encoding from the row payload in
    // Rust (SQLite JSON cannot carry ids above i64::MAX exactly). This runs
    // inside the SAME migration transaction and is a no-op once finalized; a
    // row that cannot be recovered exactly fails the open with the database
    // left at its pre-migration version. See
    // `billing_store::finalize_task_id_text_migration`.
    crate::billing_store::finalize_task_id_text_migration(&tx)
        .map_err(|e| CloudStoreError::Backend(format!("cp migration v6 task-id finalize: {e}")))?;
    tx.commit().map_err(backend)?;
    Ok(started)
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

// The SQL of every entity shared by the connection-holding store methods and
// by [`SqliteTx`]; `&Connection` also accepts a live `Transaction` (it
// derefs), so the SAME statements run inside and outside `execute_idempotent`.

fn sql_put_user(conn: &Connection, user: &User) -> Result<(), CloudStoreError> {
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

fn sql_user(conn: &Connection, id: &UserId) -> Result<Option<User>, CloudStoreError> {
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

fn sql_user_by_email(conn: &Connection, email: &str) -> Result<Option<User>, CloudStoreError> {
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

fn sql_put_external_identity(
    conn: &Connection,
    identity: &ExternalIdentity,
) -> Result<(), CloudStoreError> {
    // The (provider, subject) UNIQUE constraint is the subject-ownership
    // invariant: a second row for the same subject is refused here as a typed
    // malformed write rather than surfacing as a raw constraint error (and an
    // in-transaction caller that checked first can never rebind a subject).
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM cp_external_identity WHERE provider = ?1 AND subject = ?2",
            params![identity.provider, identity.subject],
            |r| r.get(0),
        )
        .optional()
        .map_err(backend)?;
    if let Some(existing) = existing {
        if existing != identity.id.as_str() {
            return Err(CloudStoreError::Malformed(format!(
                "external identity ({}, {}) is already linked",
                identity.provider, identity.subject
            )));
        }
    }
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

fn sql_external_identity(
    conn: &Connection,
    provider: &str,
    subject: &str,
) -> Result<Option<ExternalIdentity>, CloudStoreError> {
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

fn sql_put_organization(
    conn: &Connection,
    organization: &Organization,
) -> Result<(), CloudStoreError> {
    conn.execute(
        "INSERT INTO cp_organization (id, payload) VALUES (?1, ?2)
         ON CONFLICT(id) DO UPDATE SET payload = excluded.payload",
        params![organization.id.as_str(), encode(organization)?],
    )
    .map_err(backend)?;
    Ok(())
}

fn sql_organization(
    conn: &Connection,
    id: &OrganizationId,
) -> Result<Option<Organization>, CloudStoreError> {
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

fn sql_put_membership(conn: &Connection, membership: &Membership) -> Result<(), CloudStoreError> {
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

fn sql_membership(
    conn: &Connection,
    organization: &OrganizationId,
    user: &UserId,
) -> Result<Option<Membership>, CloudStoreError> {
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

/// Refuse a second row claiming an already-used token hash with the SAME
/// typed conflict the memory store raises. `table` is one of this module's
/// own literals (never caller input), so the format is injection-free.
fn sql_token_hash_conflict(
    conn: &Connection,
    table: &str,
    hash: &TokenHash,
    own_id: &str,
) -> Result<(), CloudStoreError> {
    let existing: Option<String> = conn
        .query_row(
            &format!("SELECT id FROM {table} WHERE token_hash = ?1"),
            params![hash.as_str()],
            |r| r.get(0),
        )
        .optional()
        .map_err(backend)?;
    if let Some(existing) = existing {
        if existing != own_id {
            return Err(CloudStoreError::Conflict(format!(
                "{table} token hash is already claimed by {existing}"
            )));
        }
    }
    Ok(())
}

fn sql_put_invitation(conn: &Connection, invitation: &Invitation) -> Result<(), CloudStoreError> {
    sql_token_hash_conflict(
        conn,
        "cp_invitation",
        &invitation.token_hash,
        invitation.id.as_str(),
    )?;
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

fn sql_invitation_by_token_hash(
    conn: &Connection,
    hash: &TokenHash,
) -> Result<Option<Invitation>, CloudStoreError> {
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

fn sql_put_auth_session(conn: &Connection, session: &AuthSession) -> Result<(), CloudStoreError> {
    sql_token_hash_conflict(
        conn,
        "cp_auth_session",
        &session.token_hash,
        session.id.as_str(),
    )?;
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

fn sql_put_service_account(
    conn: &Connection,
    account: &ServiceAccount,
) -> Result<(), CloudStoreError> {
    sql_token_hash_conflict(
        conn,
        "cp_service_account",
        &account.token_hash,
        account.id.as_str(),
    )?;
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

fn sql_put_approval(conn: &Connection, approval: &ApprovalRequest) -> Result<(), CloudStoreError> {
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

fn sql_approval(
    conn: &Connection,
    id: &ApprovalId,
) -> Result<Option<ApprovalRequest>, CloudStoreError> {
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

impl ControlPlaneStore for SqliteControlPlaneStore {
    fn put_user(&self, user: &User) -> Result<(), CloudStoreError> {
        sql_put_user(&*self.lock()?, user)
    }

    fn user(&self, id: &UserId) -> Result<Option<User>, CloudStoreError> {
        sql_user(&*self.lock()?, id)
    }

    fn user_by_email(&self, email: &str) -> Result<Option<User>, CloudStoreError> {
        sql_user_by_email(&*self.lock()?, email)
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
        sql_put_organization(&*self.lock()?, organization)
    }

    fn organization(&self, id: &OrganizationId) -> Result<Option<Organization>, CloudStoreError> {
        sql_organization(&*self.lock()?, id)
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
        sql_put_external_identity(&*self.lock()?, identity)
    }

    fn external_identity(
        &self,
        provider: &str,
        subject: &str,
    ) -> Result<Option<ExternalIdentity>, CloudStoreError> {
        sql_external_identity(&*self.lock()?, provider, subject)
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
        sql_put_membership(&*self.lock()?, membership)
    }

    fn membership(
        &self,
        organization: &OrganizationId,
        user: &UserId,
    ) -> Result<Option<Membership>, CloudStoreError> {
        sql_membership(&*self.lock()?, organization, user)
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
        sql_put_invitation(&*self.lock()?, invitation)
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
        sql_invitation_by_token_hash(&*self.lock()?, hash)
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
        sql_put_auth_session(&*self.lock()?, session)
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
        sql_put_service_account(&*self.lock()?, account)
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
        sql_put_approval(&*self.lock()?, approval)
    }

    fn approval(&self, id: &ApprovalId) -> Result<Option<ApprovalRequest>, CloudStoreError> {
        sql_approval(&*self.lock()?, id)
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

    #[cfg(test)]
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

    fn idempotency_count(&self) -> Result<usize, CloudStoreError> {
        let conn = self.lock()?;
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM cp_idempotency", [], |r| r.get(0))
            .map_err(backend)?;
        Ok(count.max(0) as usize)
    }

    fn prune_idempotency(&self, now_ms: i64) -> Result<usize, CloudStoreError> {
        let conn = self.lock()?;
        prune_idempotency_sql(&conn, now_ms)
    }

    fn execute_idempotent(
        &self,
        key: &str,
        operation: &str,
        request_digest: &str,
        now_ms: i64,
        apply: &mut dyn FnMut(
            &mut dyn ControlPlaneTx,
        ) -> Result<serde_json::Value, ControlPlaneError>,
    ) -> Result<IdempotentOutcome, ControlPlaneError> {
        // Test-only crash injection: a budget shared by the claim, every
        // closure statement and the response write.
        #[cfg(test)]
        let fault_limit = self.crash_after.load(std::sync::atomic::Ordering::SeqCst);
        #[cfg(test)]
        let budget = CrashBudget {
            limit: fault_limit,
            executed: std::cell::Cell::new(0),
        };

        let mut conn = self.lock()?;
        // IMMEDIATE: the write lock is taken up front, so concurrent claimers
        // serialize here and the loser observes the winner's committed row
        // (never an uncommitted placeholder).
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(backend)?;
        let mut wrapper = SqliteTx {
            tx,
            #[cfg(test)]
            fault: if fault_limit == usize::MAX {
                None
            } else {
                Some(&budget)
            },
        };

        wrapper.tick()?;
        let claimed = wrapper
            .tx
            .execute(
                "INSERT INTO cp_idempotency (key, operation, request_hash, response, created_ms)
                 VALUES (?1, ?2, ?3, '', ?4)
                 ON CONFLICT(key) DO NOTHING",
                params![key, operation, request_digest, now_ms],
            )
            .map_err(backend)?;

        if claimed == 1 {
            // First execution: the whole domain mutation runs against the
            // same transaction that holds the claim. Any refusal or failure
            // drops the transaction, so no domain write and no claim survive.
            let response = apply(&mut wrapper)?;
            let encoded = recorded_response_json(&response)?;
            wrapper.tick()?;
            wrapper
                .tx
                .execute(
                    "UPDATE cp_idempotency SET response = ?1 WHERE key = ?2",
                    params![encoded, key],
                )
                .map_err(backend)?;
            wrapper.tx.commit().map_err(backend)?;
            return Ok(IdempotentOutcome::Executed(response));
        }

        // Lost the claim race (or the key was already recorded): the winner's
        // response is replayed verbatim and the closure NEVER runs.
        wrapper.tick()?;
        let recorded: Option<(String, String, String)> = wrapper
            .tx
            .query_row(
                "SELECT operation, request_hash, response FROM cp_idempotency WHERE key = ?1",
                params![key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(backend)?;
        wrapper.tx.commit().map_err(backend)?;
        let Some((recorded_operation, recorded_digest, response_json)) = recorded else {
            return Err(ControlPlaneError::Backend(
                "idempotency claim disappeared inside its transaction".into(),
            ));
        };
        if recorded_operation != operation || recorded_digest != request_digest {
            return Err(ControlPlaneError::Conflict(format!(
                "idempotency key {key:?} was already used for a different request"
            )));
        }
        let replayed = serde_json::from_str(&response_json).map_err(|e| {
            ControlPlaneError::Backend(format!("recorded idempotent response is unreadable: {e}"))
        })?;
        Ok(IdempotentOutcome::Replayed(replayed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{ExternalIdentityId, MembershipId};
    use crate::rbac::{Action, Role};
    use std::sync::Arc;

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
            // A live timestamp: the open/backup tick prunes the journal's TTL
            // against its own newest row, and a fake ancient stamp would be
            // pruned on the restart below.
            created_ms: crate::durability::now_ms(),
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

    /// The store-level transactional idempotency contract, exercised on both
    /// backends: exactly one closure run per key, replays from the winner's
    /// recorded response, typed conflicts on a digest/operation mismatch, and
    /// a refused closure leaving neither domain rows nor a claimed key.
    fn idempotent_transaction_contract(store: &dyn ControlPlaneStore) {
        let digest = "request-digest-1";
        let mut runs = 0usize;
        let executed = store
            .execute_idempotent("tx-key-1", "op_a", digest, 7, &mut |tx| {
                runs += 1;
                tx.put_organization(&organization("org_tx"))?;
                Ok(serde_json::json!({"ok": true, "n": runs}))
            })
            .unwrap();
        assert_eq!(
            executed,
            IdempotentOutcome::Executed(serde_json::json!({"ok": true, "n": 1}))
        );
        assert_eq!(runs, 1);
        assert!(store
            .organization(&OrganizationId::try_new("org_tx").unwrap())
            .unwrap()
            .is_some());

        // A replay NEVER re-runs the closure, even when it would answer
        // differently.
        let replayed = store
            .execute_idempotent("tx-key-1", "op_a", digest, 7, &mut |_tx| {
                runs += 1;
                Ok(serde_json::json!({"ok": false}))
            })
            .unwrap();
        assert_eq!(
            replayed,
            IdempotentOutcome::Replayed(serde_json::json!({"ok": true, "n": 1}))
        );
        assert_eq!(runs, 1, "a replay never runs the closure");

        // The same key under another digest (or operation) is a typed
        // conflict, and the closure still never runs.
        for (operation, digest) in [("op_a", "other-digest"), ("op_b", digest)] {
            let conflict = store
                .execute_idempotent("tx-key-1", operation, digest, 7, &mut |_tx| {
                    runs += 1;
                    Ok(serde_json::json!({}))
                })
                .unwrap_err();
            assert!(matches!(conflict, ControlPlaneError::Conflict(_)));
        }
        assert_eq!(runs, 1);

        // A refusal rolls the whole transaction back: no domain row, no
        // recorded key, so a retry executes for real.
        let refused = store
            .execute_idempotent("tx-key-2", "op_a", digest, 7, &mut |tx| {
                tx.put_organization(&organization("org_partial"))?;
                Err(ControlPlaneError::Unauthorized("refused".into()))
            })
            .unwrap_err();
        assert!(matches!(refused, ControlPlaneError::Unauthorized(_)));
        assert!(store
            .organization(&OrganizationId::try_new("org_partial").unwrap())
            .unwrap()
            .is_none());
        assert!(store.idempotent("tx-key-2").unwrap().is_none());
        let retried = store
            .execute_idempotent("tx-key-2", "op_a", digest, 7, &mut |tx| {
                tx.put_organization(&organization("org_partial"))?;
                Ok(serde_json::json!({"ok": true}))
            })
            .unwrap();
        assert!(matches!(retried, IdempotentOutcome::Executed(_)));
        assert!(store
            .organization(&OrganizationId::try_new("org_partial").unwrap())
            .unwrap()
            .is_some());
    }

    #[test]
    fn memory_store_idempotent_transaction_contract() {
        idempotent_transaction_contract(&MemoryControlPlaneStore::new());
    }

    #[test]
    fn sqlite_store_idempotent_transaction_contract() {
        idempotent_transaction_contract(&SqliteControlPlaneStore::open_in_memory().unwrap());
    }

    /// F12: a recorded response beyond the hard bound cannot be persisted; the
    /// operation must roll back ENTIRELY on both backends (memory replays its
    /// undo journal, SQLite rolls its transaction back) — the domain write
    /// never survives a response that cannot be replayed.
    fn oversized_response_rolls_back_contract(store: &dyn ControlPlaneStore) {
        let huge = "x".repeat(MAX_IDEMPOTENT_RESPONSE_BYTES + 1);
        let err = store
            .execute_idempotent("oversized", "op", "d", 1, &mut |tx| {
                tx.put_organization(&organization("org_oversized"))?;
                Ok(serde_json::json!({ "blob": huge }))
            })
            .unwrap_err();
        assert!(matches!(err, ControlPlaneError::Malformed(_)), "{err:?}");
        assert!(
            store
                .organization(&OrganizationId::try_new("org_oversized").unwrap())
                .unwrap()
                .is_none(),
            "the domain write was rolled back with the unrecordable response"
        );
        assert!(store.idempotent("oversized").unwrap().is_none());
        // The key is free: a bounded retry executes for real.
        let retried = store
            .execute_idempotent("oversized", "op", "d", 1, &mut |tx| {
                tx.put_organization(&organization("org_oversized"))?;
                Ok(serde_json::json!({ "ok": true }))
            })
            .unwrap();
        assert!(matches!(retried, IdempotentOutcome::Executed(_)));
    }

    #[test]
    fn memory_store_oversized_response_rolls_back() {
        oversized_response_rolls_back_contract(&MemoryControlPlaneStore::new());
    }

    #[test]
    fn sqlite_store_oversized_response_rolls_back() {
        oversized_response_rolls_back_contract(&SqliteControlPlaneStore::open_in_memory().unwrap());
    }

    /// F13 negatives: a record seeded directly (the test-only `claim_idempotent`
    /// seam) can never make `execute_idempotent` skip the closure silently —
    /// a mismatched operation/digest is a typed conflict and an empty body is
    /// a typed backend failure, and the closure never runs.
    fn seeded_record_negative_contract(store: &dyn ControlPlaneStore) {
        let seeded = IdempotencyRecord {
            key: "seeded-key".into(),
            operation: "op".into(),
            request_hash: "digest".into(),
            response_json: String::new(),
            created_ms: 1,
        };
        assert!(store.claim_idempotent(&seeded).unwrap());
        assert!(!store.claim_idempotent(&seeded).unwrap());
        let mut runs = 0usize;
        for (operation, digest) in [("op", "other-digest"), ("other-op", "digest")] {
            let err = store
                .execute_idempotent("seeded-key", operation, digest, 2, &mut |_tx| {
                    runs += 1;
                    Ok(serde_json::json!({}))
                })
                .unwrap_err();
            assert!(matches!(err, ControlPlaneError::Conflict(_)), "{err:?}");
        }
        let err = store
            .execute_idempotent("seeded-key", "op", "digest", 2, &mut |_tx| {
                runs += 1;
                Ok(serde_json::json!({}))
            })
            .unwrap_err();
        assert!(matches!(err, ControlPlaneError::Backend(_)), "{err:?}");
        assert_eq!(runs, 0, "a claimed key never runs the closure");
    }

    #[test]
    fn memory_store_seeded_record_negatives() {
        seeded_record_negative_contract(&MemoryControlPlaneStore::new());
    }

    #[test]
    fn sqlite_store_seeded_record_negatives() {
        seeded_record_negative_contract(&SqliteControlPlaneStore::open_in_memory().unwrap());
    }

    /// F7: the idempotency journal has a documented TTL + count retention;
    /// both backends prune identically and the newest rows always survive.
    fn idempotency_retention_contract(store: &dyn ControlPlaneStore) {
        let record = |key: &str, created_ms: i64| IdempotencyRecord {
            key: key.into(),
            operation: "op".into(),
            request_hash: "d".into(),
            response_json: "{}".into(),
            created_ms,
        };
        let now = 1_000_000_000i64;
        store
            .claim_idempotent(&record("stale", now - IDEMPOTENCY_TTL_MS - 1))
            .unwrap();
        store.claim_idempotent(&record("fresh", now)).unwrap();
        assert_eq!(store.prune_idempotency(now).unwrap(), 1);
        assert!(store.idempotent("stale").unwrap().is_none());
        assert!(store.idempotent("fresh").unwrap().is_some());

        for i in 0..(MAX_IDEMPOTENCY_ROWS + 64) {
            store
                .claim_idempotent(&record(&format!("bulk-{i:05}"), now + 1 + i as i64))
                .unwrap();
        }
        assert_eq!(
            store.idempotency_count().unwrap(),
            MAX_IDEMPOTENCY_ROWS + 65
        );
        assert_eq!(store.prune_idempotency(now).unwrap(), 65);
        assert_eq!(
            store.idempotency_count().unwrap(),
            MAX_IDEMPOTENCY_ROWS,
            "the journal is bounded by count"
        );
        assert!(store
            .idempotent(&format!("bulk-{:05}", MAX_IDEMPOTENCY_ROWS + 63))
            .unwrap()
            .is_some());
        assert!(store.idempotent("bulk-00000").unwrap().is_none());
        assert!(store.idempotent("fresh").unwrap().is_none());
    }

    #[test]
    fn memory_store_idempotency_retention() {
        idempotency_retention_contract(&MemoryControlPlaneStore::new());
    }

    #[test]
    fn sqlite_store_idempotency_retention() {
        idempotency_retention_contract(&SqliteControlPlaneStore::open_in_memory().unwrap());
    }

    /// F4: a database written by a NEWER schema ladder is refused typed
    /// (documented downgrade refusal) and left untouched.
    #[test]
    fn opening_a_newer_schema_is_refused_typed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control-plane.db");
        let newer = CP_MIGRATIONS.len() as i64 + 1;
        {
            let conn = Connection::open(&path).unwrap();
            crate::durability::apply_policy(&conn).unwrap();
            conn.execute_batch(&format!("PRAGMA user_version = {newer}"))
                .unwrap();
        }
        let err = SqliteControlPlaneStore::open(&path)
            .err()
            .expect("a newer schema must be refused");
        let message = err.to_string();
        assert!(
            message.contains("newer than this binary's ladder") && message.contains("downgrade"),
            "{message}"
        );
        let conn = Connection::open(&path).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, newer, "the refused open changed nothing");
    }

    /// P1 corruption protection: a NEGATIVE `user_version` cannot be produced
    /// by any legitimate open (SQLite stores the cursor as a signed integer).
    /// It is refused typed BEFORE the restore point and the ladder, so no
    /// snapshot labeled `-pre-migration-v-1-` is ever written and the database
    /// is left unchanged; 0 and the ladder version still open as before.
    #[test]
    fn negative_schema_version_is_refused_typed_without_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control-plane.db");
        {
            let conn = Connection::open(&path).unwrap();
            crate::durability::apply_policy(&conn).unwrap();
            conn.execute_batch("PRAGMA user_version = -1").unwrap();
        }
        let before = {
            let conn = Connection::open(&path).unwrap();
            crate::durability::canonical_fingerprint(&conn).unwrap()
        };
        let err = SqliteControlPlaneStore::open(&path)
            .err()
            .expect("a negative schema version must be refused");
        match &err {
            CloudStoreError::Malformed(message) => {
                assert!(
                    message.contains("-1"),
                    "the refusal names the found value: {message}"
                );
                assert!(message.contains("corrupt"), "{message}");
            }
            other => panic!("a negative schema version must be refused typed, got {other:?}"),
        }
        // No migration, no restore point, not even the open/backup tick ran:
        // the refused open wrote nothing at all.
        assert!(
            crate::durability::list_backups(&path).is_empty(),
            "the refused open writes no snapshot"
        );
        {
            let conn = Connection::open(&path).unwrap();
            let version: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(version, -1, "the refused open changed nothing");
            assert_eq!(
                crate::durability::canonical_fingerprint(&conn).unwrap(),
                before,
                "the refused open left the database unchanged"
            );
        }
        // A clean 0 still migrates the ladder, and the ladder version still
        // reopens as a no-op.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA user_version = 0").unwrap();
        }
        drop(SqliteControlPlaneStore::open(&path).expect("0 still migrates"));
        let migration_points = || -> usize {
            crate::durability::list_backups(&path)
                .into_iter()
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("")
                        .contains(crate::durability::MIGRATION_MARKER)
                })
                .count()
        };
        assert_eq!(
            migration_points(),
            1,
            "0 -> ladder writes its v0 restore point"
        );
        drop(SqliteControlPlaneStore::open(&path).expect("the ladder still reopens"));
        assert_eq!(
            migration_points(),
            1,
            "an at-ladder reopen writes no new restore point"
        );
    }

    /// F5: two concurrent openers serialize on the migration write lock. The
    /// loser re-reads the ladder version inside its own transaction and SKIPS,
    /// so exactly ONE restore point is written and its name claim matches its
    /// content (no post-migration snapshot mislabeled `-pre-migration-v0-`).
    #[test]
    fn concurrent_openers_serialize_migration_and_label_restore_points_truthfully() {
        use std::sync::Barrier;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control-plane.db");
        {
            let conn = Connection::open(&path).unwrap();
            crate::durability::apply_policy(&conn).unwrap();
            conn.execute_batch(
                "CREATE TABLE legacy_row (id TEXT PRIMARY KEY, v INTEGER);
                 INSERT INTO legacy_row (id, v) VALUES ('a', 1);",
            )
            .unwrap();
        }
        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let path = path.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                SqliteControlPlaneStore::open(&path).map(|_| ())
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
            assert_eq!(version, CP_MIGRATIONS.len() as i64, "the ladder applied");
            let legacy: i64 = conn
                .query_row("SELECT v FROM legacy_row WHERE id = 'a'", [], |r| r.get(0))
                .unwrap();
            assert_eq!(legacy, 1, "the pre-existing row survived");
        }
        let points: Vec<std::path::PathBuf> = crate::durability::list_backups(&path)
            .into_iter()
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .contains(crate::durability::MIGRATION_MARKER)
            })
            .collect();
        assert_eq!(
            points.len(),
            1,
            "exactly one opener snapshotted the pre-migration state: {points:?}"
        );
        let name = points[0].file_name().unwrap().to_str().unwrap();
        assert!(name.contains("-pre-migration-v0-"), "{name}");
        let snapshot =
            Connection::open_with_flags(&points[0], rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        let snapshot_version: i64 = snapshot
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            snapshot_version, 0,
            "the restore point content matches its claimed version"
        );
        let snapshot_legacy: i64 = snapshot
            .query_row("SELECT v FROM legacy_row WHERE id = 'a'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            snapshot_legacy, 1,
            "the snapshot holds the pre-migration row"
        );
    }

    /// The in-transaction external-identity seam: an attach is visible in the
    /// same transaction, a refusal rolls the attach back, and a second row
    /// for a bound `(provider, subject)` is refused typed so a subject can
    /// never silently change owners.
    fn external_identity_transaction_contract(store: &dyn ControlPlaneStore) {
        let identity = ExternalIdentity {
            id: ExternalIdentityId::try_new("ext_tx_00000000000000000000000000000001").unwrap(),
            user: UserId::try_new("usr_tx_owner").unwrap(),
            provider: "idp.example".into(),
            subject: "sub-1".into(),
            created_ms: 5,
        };
        let executed = store
            .execute_idempotent("tx-ext-1", "link", "d", 5, &mut |tx| {
                assert!(tx.external_identity("idp.example", "sub-1")?.is_none());
                tx.put_external_identity(&identity)?;
                let seen = tx
                    .external_identity("idp.example", "sub-1")?
                    .expect("visible inside the same transaction");
                assert_eq!(seen, identity);
                Ok(serde_json::json!({"ok": true}))
            })
            .unwrap();
        assert!(matches!(executed, IdempotentOutcome::Executed(_)));
        assert_eq!(
            store
                .external_identity("idp.example", "sub-1")
                .unwrap()
                .unwrap(),
            identity
        );

        // A refusal rolls the attach back with the rest of the transaction.
        let refused = store
            .execute_idempotent("tx-ext-2", "link", "d", 5, &mut |tx| {
                tx.put_external_identity(&ExternalIdentity {
                    id: ExternalIdentityId::try_new("ext_tx_00000000000000000000000000000002")
                        .unwrap(),
                    user: UserId::try_new("usr_tx_owner").unwrap(),
                    provider: "idp.example".into(),
                    subject: "sub-2".into(),
                    created_ms: 5,
                })?;
                Err(ControlPlaneError::Unauthorized("refused".into()))
            })
            .unwrap_err();
        assert!(matches!(refused, ControlPlaneError::Unauthorized(_)));
        assert!(store
            .external_identity("idp.example", "sub-2")
            .unwrap()
            .is_none());

        // A different id may not claim an already-bound subject.
        let stolen = ExternalIdentity {
            id: ExternalIdentityId::try_new("ext_tx_00000000000000000000000000000003").unwrap(),
            user: UserId::try_new("usr_tx_thief").unwrap(),
            provider: "idp.example".into(),
            subject: "sub-1".into(),
            created_ms: 6,
        };
        let error = store.put_external_identity(&stolen).unwrap_err();
        assert!(matches!(error, CloudStoreError::Malformed(_)));
        assert_eq!(
            store
                .external_identity("idp.example", "sub-1")
                .unwrap()
                .unwrap()
                .user
                .as_str(),
            "usr_tx_owner"
        );

        // Delimiter-bearing pairs must not alias: `format!("{provider}:{subject}")`
        // would render both ("a:b", "c") and ("a", "b:c") as "a:b:c". The
        // tuple key and SQLite's UNIQUE(provider, subject) resolve them apart.
        store
            .put_external_identity(&ExternalIdentity {
                id: ExternalIdentityId::try_new("ext_tx_00000000000000000000000000000004").unwrap(),
                user: UserId::try_new("usr_tx_colon_left").unwrap(),
                provider: "a:b".into(),
                subject: "c".into(),
                created_ms: 7,
            })
            .unwrap();
        store
            .put_external_identity(&ExternalIdentity {
                id: ExternalIdentityId::try_new("ext_tx_00000000000000000000000000000005").unwrap(),
                user: UserId::try_new("usr_tx_colon_right").unwrap(),
                provider: "a".into(),
                subject: "b:c".into(),
                created_ms: 7,
            })
            .unwrap();
        assert_eq!(
            store
                .external_identity("a:b", "c")
                .unwrap()
                .unwrap()
                .user
                .as_str(),
            "usr_tx_colon_left"
        );
        assert_eq!(
            store
                .external_identity("a", "b:c")
                .unwrap()
                .unwrap()
                .user
                .as_str(),
            "usr_tx_colon_right"
        );
    }

    /// Explicit cross-backend parity for delimiter-bearing identity pairs: the
    /// memory store and SQLite must resolve the same (provider, subject)
    /// requests to the same users.
    #[test]
    fn external_identity_backends_agree_on_delimiter_bearing_pairs() {
        let memory = MemoryControlPlaneStore::new();
        let sqlite = SqliteControlPlaneStore::open_in_memory().unwrap();
        let mut resolved = Vec::new();
        for store in [
            &memory as &dyn ControlPlaneStore,
            &sqlite as &dyn ControlPlaneStore,
        ] {
            for (id, provider, subject, user) in [
                (
                    "ext_parity_00000000000000000000000000000001",
                    "a:b",
                    "c",
                    "usr_parity_left",
                ),
                (
                    "ext_parity_00000000000000000000000000000002",
                    "a",
                    "b:c",
                    "usr_parity_right",
                ),
            ] {
                store
                    .put_external_identity(&ExternalIdentity {
                        id: ExternalIdentityId::try_new(id).unwrap(),
                        user: UserId::try_new(user).unwrap(),
                        provider: provider.into(),
                        subject: subject.into(),
                        created_ms: 1,
                    })
                    .unwrap();
            }
            resolved.push((
                store
                    .external_identity("a:b", "c")
                    .unwrap()
                    .unwrap()
                    .user
                    .as_str()
                    .to_string(),
                store
                    .external_identity("a", "b:c")
                    .unwrap()
                    .unwrap()
                    .user
                    .as_str()
                    .to_string(),
            ));
        }
        assert_eq!(
            resolved[0], resolved[1],
            "memory and SQLite resolve delimiter-bearing pairs identically"
        );
        assert_eq!(
            resolved[0],
            (
                "usr_parity_left".to_string(),
                "usr_parity_right".to_string()
            )
        );
    }

    #[test]
    fn memory_store_external_identity_transaction_contract() {
        external_identity_transaction_contract(&MemoryControlPlaneStore::new());
    }

    #[test]
    fn sqlite_store_external_identity_transaction_contract() {
        external_identity_transaction_contract(&SqliteControlPlaneStore::open_in_memory().unwrap());
    }

    /// SQLite enforces `UNIQUE(token_hash)` on invitations, auth sessions and
    /// service accounts. The memory store must refuse the exact same
    /// duplicates with the same typed conflict, never silently alias a token.
    fn token_hash_uniqueness_contract(store: &dyn ControlPlaneStore) {
        let org = OrganizationId::try_new("org_a").unwrap();

        let hash = TokenHash::try_new("c".repeat(64)).unwrap();
        let invitation = Invitation {
            id: InvitationId::try_new("inv_tok_1").unwrap(),
            organization: org.clone(),
            email: "invitee@example.test".into(),
            role: Role::Member,
            status: crate::model::InvitationStatus::Pending,
            invited_by: UserId::try_new("usr_1").unwrap(),
            token_hash: hash.clone(),
            created_ms: 1,
            expires_ms: 100,
            decided_ms: None,
        };
        store.put_invitation(&invitation).unwrap();
        let mut second = invitation.clone();
        second.id = InvitationId::try_new("inv_tok_2").unwrap();
        assert!(matches!(
            store.put_invitation(&second).unwrap_err(),
            CloudStoreError::Conflict(_)
        ));
        store
            .put_invitation(&invitation)
            .expect("the same id may still be updated in place");
        assert_eq!(
            store
                .invitation_by_token_hash(&hash)
                .unwrap()
                .unwrap()
                .id
                .as_str(),
            "inv_tok_1",
            "the token still resolves to the surviving row"
        );

        let session_hash = TokenHash::try_new("d".repeat(64)).unwrap();
        let session = AuthSession {
            id: AuthSessionId::try_new("ses_tok_1").unwrap(),
            organization: org.clone(),
            user: UserId::try_new("usr_1").unwrap(),
            token_hash: session_hash.clone(),
            created_ms: 1,
            expires_ms: 100,
            revoked_ms: None,
        };
        store.put_auth_session(&session).unwrap();
        let mut second_session = session.clone();
        second_session.id = AuthSessionId::try_new("ses_tok_2").unwrap();
        assert!(matches!(
            store.put_auth_session(&second_session).unwrap_err(),
            CloudStoreError::Conflict(_)
        ));
        assert_eq!(
            store
                .auth_session_by_token_hash(&session_hash)
                .unwrap()
                .unwrap()
                .id,
            session.id
        );

        let account_hash = TokenHash::try_new("e".repeat(64)).unwrap();
        let account = ServiceAccount {
            id: ServiceAccountId::try_new("sa_tok_1").unwrap(),
            organization: org.clone(),
            name: "ci".into(),
            role: Role::Member,
            scopes: vec![Action::RepositoryRead],
            token_hash: account_hash.clone(),
            created_ms: 1,
            disabled: false,
        };
        store.put_service_account(&account).unwrap();
        let mut second_account = account.clone();
        second_account.id = ServiceAccountId::try_new("sa_tok_2").unwrap();
        assert!(matches!(
            store.put_service_account(&second_account).unwrap_err(),
            CloudStoreError::Conflict(_)
        ));
        assert_eq!(
            store
                .service_account_by_token_hash(&account_hash)
                .unwrap()
                .unwrap()
                .id,
            account.id
        );

        // The same invariant holds INSIDE an idempotent transaction: the
        // duplicate is refused typed and the key is never claimed.
        let mut third = invitation.clone();
        third.id = InvitationId::try_new("inv_tok_3").unwrap();
        let refused = store
            .execute_idempotent("tok-hash-tx", "invite", "d", 1, &mut |tx| {
                tx.put_invitation(&third)?;
                Ok(serde_json::json!({"ok": true}))
            })
            .unwrap_err();
        assert!(matches!(refused, ControlPlaneError::Conflict(_)));
        assert!(store.idempotent("tok-hash-tx").unwrap().is_none());
        assert!(store.invitation(&third.id).unwrap().is_none());
    }

    #[test]
    fn memory_store_token_hash_uniqueness_contract() {
        token_hash_uniqueness_contract(&MemoryControlPlaneStore::new());
    }

    #[test]
    fn sqlite_store_token_hash_uniqueness_contract() {
        token_hash_uniqueness_contract(&SqliteControlPlaneStore::open_in_memory().unwrap());
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

    // ------------------------------------------------ commercial durability

    #[test]
    fn file_backed_open_is_full_synchronous_and_records_the_policy_marker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control-plane.db");
        let store = SqliteControlPlaneStore::open(&path).unwrap();
        {
            let conn = store.lock().unwrap();
            let sync: i64 = conn
                .query_row("PRAGMA synchronous", [], |r| r.get(0))
                .unwrap();
            assert_eq!(sync, 2, "the writer connection is synchronous=FULL");
        }
        let report = store.durability_report(false).unwrap();
        assert_eq!(report.journal_mode, "wal");
        let policy = report.policy.expect("the policy marker is recorded");
        assert_eq!(
            policy
                .iter()
                .find(|(k, _)| k == "synchronous")
                .map(|(_, v)| v.as_str()),
            Some("FULL")
        );
        assert_eq!(
            policy
                .iter()
                .find(|(k, _)| k == "backup_policy")
                .map(|(_, v)| v.as_str()),
            Some("rotating")
        );
        assert!(report.integrity.is_empty(), "fresh database is intact");
        assert!(
            report.last_backup.is_some(),
            "the first open writes a verified rotating backup"
        );
    }

    /// Crash-mid-migration: the injected failure fires AFTER the pre-migration
    /// restore point and BEFORE any migration SQL. The database must stay at
    /// the old version, the restore point must exist and restore-verify, and
    /// `doctor` must report it.
    #[test]
    fn migration_crash_leaves_a_verified_restore_point_doctor_reports() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control-plane.db");
        drop(SqliteControlPlaneStore::open(&path).unwrap());
        // Roll the cursor back one version so the next open has a pending
        // migration (the tables are already there; the transition is the
        // crash point under test).
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("PRAGMA user_version = 4").unwrap();
        }
        inject_crash_before_migration(&path);
        let err = SqliteControlPlaneStore::open(&path)
            .err()
            .expect("the injected crash must fail the open");
        assert!(
            err.to_string().contains("injected crash"),
            "the failure is the injected crash: {err}"
        );
        // The database is untouched at v4.
        {
            let conn = Connection::open(&path).unwrap();
            let version: i64 = conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(version, 4, "no migration ran without its restore point");
        }
        let report = crate::durability::doctor_probe(&path, false).unwrap();
        let (point, _) = report
            .migration_restore_point
            .expect("doctor reports the pre-migration restore point");
        assert!(
            point
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .contains("-pre-migration-v4-"),
            "the restore point names the version it protects: {}",
            point.display()
        );
        // The restore point is a real v4 database, not a partial copy.
        let backup =
            Connection::open_with_flags(&point, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .unwrap();
        let version: i64 = backup
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 4);
        assert!(crate::durability::integrity_check(&backup, true)
            .unwrap()
            .is_empty());
        // A clean re-open completes the migration and records the policy.
        let store = SqliteControlPlaneStore::open(&path).unwrap();
        let report = store.durability_report(false).unwrap();
        assert!(report.policy.is_some());
    }

    /// The kill proof: the child opens the commercial database, records an
    /// acknowledged credit grant, prints the ACK + writer pragma, and hangs;
    /// the parent SIGKILLs it and reopens the database. The acknowledged
    /// grant must be there. (Kill -9 proves no application-level buffering;
    /// the `synchronous = FULL` assertion on the WRITER connection is what
    /// extends that to a power-loss boundary — `NORMAL` can roll back the WAL
    /// tail of an acknowledged commit.)
    #[test]
    fn acknowledged_credit_commit_survives_sigkill_and_reopen() {
        const CHILD_ENV: &str = "FAKTOR_CP_DURABILITY_CHILD_DB";
        if let Ok(db_path) = std::env::var(CHILD_ENV) {
            child_acknowledged_credit_commit(&db_path);
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("billing.db");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("store::tests::acknowledged_credit_commit_survives_sigkill_and_reopen")
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
        // SIGKILL the child while it holds the acknowledged commit.
        child.kill().unwrap();
        let _ = child.wait();
        let ack = ack.expect("the child acknowledges its credit commit");
        assert!(
            ack.contains("SYNC=2"),
            "the writer connection was FULL at acknowledgement: {ack}"
        );
        let store = SqliteControlPlaneStore::open(&path).unwrap();
        let service = crate::entitlements::EntitlementService::with_system_clock(
            Arc::new(store),
            crate::billing::BillingConfig::default(),
        )
        .unwrap();
        let balance = service
            .credit_balance(&OrganizationId::try_new("org_durability").unwrap())
            .unwrap();
        assert_eq!(
            balance.granted_micro, 500,
            "the acknowledged credit grant survived the kill"
        );
    }

    /// The child body of the kill test. Never returns: the parent SIGKILLs it
    /// after the ACK (the bounded loop is only the fail-safe).
    fn child_acknowledged_credit_commit(db_path: &str) -> ! {
        let store = SqliteControlPlaneStore::open(std::path::Path::new(db_path)).unwrap();
        let sync: i64 = {
            let conn = store.lock().unwrap();
            conn.query_row("PRAGMA synchronous", [], |r| r.get(0))
                .unwrap()
        };
        let service = crate::entitlements::EntitlementService::with_system_clock(
            Arc::new(store),
            crate::billing::BillingConfig::default(),
        )
        .unwrap();
        let organization = OrganizationId::try_new("org_durability").unwrap();
        let account = crate::ids::BillingAccountId::try_new("acct_durability").unwrap();
        service
            .ensure_account(&organization, &account, "local", true)
            .unwrap();
        service
            .grant_credits(
                &organization,
                &account,
                500,
                "durability test",
                Some("key-1"),
            )
            .unwrap();
        println!("ACK SYNC={sync}");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        for _ in 0..600 {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        std::process::exit(2);
    }
}
