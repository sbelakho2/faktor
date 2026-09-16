//! The control-plane service: one [`ControlPlane`] over a durable
//! [`ControlPlaneStore`], with the central [`authorize`] check on every
//! operation, idempotency-keyed mutations, and one-shot secret presentation
//! (tokens are shown once at issuance and stored as hashes only).

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::error::ControlPlaneError;
use crate::ids::{
    ApprovalId, AuthSessionId, ExternalIdentityId, InvitationId, MembershipId, OrganizationId,
    SecretToken, ServiceAccountId, TokenHash, UserId, MAX_TOKEN_BYTES,
};
use crate::model::{
    normalize_email, ApprovalRequest, ApprovalStatus, AuthSession, ExternalIdentity, Invitation,
    InvitationStatus, Membership, Organization, Page, ServiceAccount, User,
    DEFAULT_INVITATION_TTL_MS, DEFAULT_SESSION_TTL_MS, MAX_DISPLAY_NAME_BYTES, MAX_NOTE_BYTES,
    MAX_ORG_NAME_BYTES,
};
use crate::rbac::{authorize, Action, Principal, Resource, Role};
use crate::store::{ControlPlaneStore, IdempotencyRecord};

/// Bound on one idempotency key.
pub const MAX_IDEMPOTENCY_KEY_BYTES: usize = 200;
/// Hard page cap for control-plane listings.
pub const MAX_PAGE: usize = 200;

/// SHA-256 as lowercase hex (the token-hash and request-hash primitive).
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// The clock seam (deterministic tests; production uses the wall clock).
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
}

/// The production clock.
#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}

/// A manually advanced clock for deterministic tests.
#[derive(Debug)]
pub struct ManualClock {
    now_ms: AtomicI64,
}

impl ManualClock {
    pub fn new(now_ms: i64) -> Self {
        Self {
            now_ms: AtomicI64::new(now_ms),
        }
    }

    pub fn advance(&self, delta_ms: i64) {
        self.now_ms.fetch_add(delta_ms, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> i64 {
        self.now_ms.load(Ordering::SeqCst)
    }
}

/// One org bootstrap result (the ONLY time the owner token is visible).
#[derive(Debug, Clone)]
pub struct BootstrapResult {
    pub organization: Organization,
    pub user: User,
    pub session: AuthSession,
    /// Present exactly once, at first issuance; `None` on an idempotent
    /// replay (the plaintext token is never stored anywhere).
    pub token: Option<SecretToken>,
}

/// The recorded (safe) bootstrap response: no plaintext token.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapRecord {
    pub organization: Organization,
    pub user: User,
    pub session: AuthSession,
}

/// One membership joined with its user.
#[derive(Debug, Clone, Serialize)]
pub struct MemberView {
    pub membership: Membership,
    pub user: User,
}

/// One issued invitation: the plaintext token is visible exactly once.
#[derive(Debug, Clone)]
pub struct InvitationIssued {
    pub invitation: Invitation,
    /// Present exactly once, at first issuance; `None` on a replay.
    pub token: Option<SecretToken>,
}

/// One issued service account: the plaintext token is visible exactly once.
#[derive(Debug, Clone)]
pub struct ServiceAccountIssued {
    pub account: ServiceAccount,
    /// Present exactly once, at first issuance.
    pub token: Option<SecretToken>,
}

/// The control-plane service.
pub struct ControlPlane {
    store: Arc<dyn ControlPlaneStore>,
    clock: Arc<dyn Clock>,
}

impl ControlPlane {
    pub fn new(store: Arc<dyn ControlPlaneStore>, clock: Arc<dyn Clock>) -> Self {
        Self { store, clock }
    }

    pub fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }

    fn new_id(prefix: &str) -> String {
        format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
    }

    fn new_token() -> Result<SecretToken, ControlPlaneError> {
        use rand::Rng;
        let mut rng = rand::rng();
        let bytes: [u8; 32] = rng.random();
        let mut token = String::with_capacity(64);
        for byte in bytes {
            token.push_str(&format!("{byte:02x}"));
        }
        SecretToken::try_new(token)
    }

    /// Validate + normalize one idempotency key (required on mutating
    /// control-plane requests).
    pub fn validate_idempotency_key(key: &str) -> Result<String, ControlPlaneError> {
        if key.is_empty() || key.len() > MAX_IDEMPOTENCY_KEY_BYTES {
            return Err(ControlPlaneError::Malformed(format!(
                "idempotency key must be 1..={MAX_IDEMPOTENCY_KEY_BYTES} bytes"
            )));
        }
        if !key.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(ControlPlaneError::Malformed(
                "idempotency key must be printable ASCII without whitespace".into(),
            ));
        }
        Ok(key.to_string())
    }

    /// Idempotency precheck: when `(key, operation, request)` was already
    /// recorded, parse and return the recorded SAFE response (never a
    /// plaintext token — the recorded shape omits it); the same key with a
    /// different request is a typed conflict. `None` = first execution.
    fn idempotency_precheck<T: for<'de> serde::Deserialize<'de>>(
        &self,
        key: &str,
        operation: &str,
        request: &serde_json::Value,
    ) -> Result<Option<T>, ControlPlaneError> {
        let key = Self::validate_idempotency_key(key)?;
        let request_hash = Self::request_hash(request)?;
        let Some(record) = self.store.idempotent(&key)? else {
            return Ok(None);
        };
        if record.operation != operation || record.request_hash != request_hash {
            return Err(ControlPlaneError::Conflict(format!(
                "idempotency key {key:?} was already used for a different request"
            )));
        }
        let replayed: T = serde_json::from_str(&record.response_json).map_err(|e| {
            ControlPlaneError::Backend(format!("recorded idempotent response is unreadable: {e}"))
        })?;
        Ok(Some(replayed))
    }

    /// Record the SAFE response of one fresh execution. A lost race (the key
    /// was claimed concurrently) replays the winner's response instead.
    fn record_idempotent(
        &self,
        key: &str,
        operation: &str,
        request: &serde_json::Value,
        recorded: &serde_json::Value,
    ) -> Result<(), ControlPlaneError> {
        let key = Self::validate_idempotency_key(key)?;
        let claimed = self.store.claim_idempotent(&IdempotencyRecord {
            key,
            operation: operation.to_string(),
            request_hash: Self::request_hash(request)?,
            response_json: serde_json::to_string(recorded).map_err(|e| {
                ControlPlaneError::Malformed(format!("recorded response encode: {e}"))
            })?,
            created_ms: self.now_ms(),
        })?;
        let _ = claimed;
        Ok(())
    }

    fn request_hash(request: &serde_json::Value) -> Result<String, ControlPlaneError> {
        Ok(sha256_hex(
            serde_json::to_string(request)
                .map_err(|e| ControlPlaneError::Malformed(format!("request encode: {e}")))?
                .as_bytes(),
        ))
    }

    // ------------------------------------------------------------ identity

    /// Create one user (idempotent by email: an existing user is returned).
    pub fn create_user(
        &self,
        email: &str,
        display_name: &str,
    ) -> Result<(User, bool), ControlPlaneError> {
        let email = normalize_email(email)?;
        if display_name.len() > MAX_DISPLAY_NAME_BYTES {
            return Err(ControlPlaneError::Malformed(
                "display name is oversized".into(),
            ));
        }
        if let Some(existing) = self.store.user_by_email(&email)? {
            return Ok((existing, false));
        }
        let user = User {
            id: UserId::try_new(Self::new_id("usr"))?,
            email,
            display_name: display_name.to_string(),
            created_ms: self.now_ms(),
            disabled: false,
        };
        self.store.put_user(&user)?;
        Ok((user, true))
    }

    /// Link one external identity subject to a user (idempotent: the same
    /// (provider, subject) resolves to the same user; a conflicting link is
    /// refused).
    pub fn link_external_identity(
        &self,
        user: &UserId,
        provider: &str,
        subject: &str,
    ) -> Result<ExternalIdentity, ControlPlaneError> {
        if provider.is_empty() || provider.len() > 64 || subject.is_empty() || subject.len() > 256 {
            return Err(ControlPlaneError::Malformed(
                "external identity provider/subject shape is invalid".into(),
            ));
        }
        if let Some(existing) = self.store.external_identity(provider, subject)? {
            if existing.user != *user {
                return Err(ControlPlaneError::Conflict(
                    "external identity is already linked to another user".into(),
                ));
            }
            return Ok(existing);
        }
        let identity = ExternalIdentity {
            id: ExternalIdentityId::try_new(Self::new_id("ext"))?,
            user: user.clone(),
            provider: provider.to_string(),
            subject: subject.to_string(),
            created_ms: self.now_ms(),
        };
        self.store.put_external_identity(&identity)?;
        Ok(identity)
    }

    /// Bootstrap one organization with its first owner. The returned token
    /// is the owner's first session; it is shown exactly once.
    pub fn bootstrap_organization(
        &self,
        name: &str,
        owner_email: &str,
        display_name: &str,
        idempotency_key: &str,
    ) -> Result<BootstrapResult, ControlPlaneError> {
        if name.is_empty() || name.len() > MAX_ORG_NAME_BYTES {
            return Err(ControlPlaneError::Malformed(format!(
                "organization name must be 1..={MAX_ORG_NAME_BYTES} bytes"
            )));
        }
        let email = normalize_email(owner_email)?;
        let request = serde_json::json!({"name": name, "owner_email": email});
        // A replay returns the recorded organization/user/session WITHOUT
        // re-presenting the one-shot token (the plaintext is never stored).
        if let Some(record) = self.idempotency_precheck::<BootstrapRecord>(
            idempotency_key,
            "bootstrap_organization",
            &request,
        )? {
            return Ok(BootstrapResult {
                organization: record.organization,
                user: record.user,
                session: record.session,
                token: None,
            });
        }
        let organization = Organization {
            id: OrganizationId::try_new(Self::new_id("org"))?,
            name: name.to_string(),
            created_ms: self.now_ms(),
            deleted: false,
        };
        self.store.put_organization(&organization)?;
        let (user, _) = self.create_user(&email, display_name)?;
        let membership = Membership {
            id: MembershipId::try_new(Self::new_id("mem"))?,
            organization: organization.id.clone(),
            user: user.id.clone(),
            role: Role::Owner,
            created_ms: self.now_ms(),
        };
        self.store.put_membership(&membership)?;
        let token = Self::new_token()?;
        let session = AuthSession {
            id: AuthSessionId::try_new(Self::new_id("ses"))?,
            organization: organization.id.clone(),
            user: user.id.clone(),
            token_hash: TokenHash::of(token.expose()),
            created_ms: self.now_ms(),
            expires_ms: self.now_ms().saturating_add(DEFAULT_SESSION_TTL_MS),
            revoked_ms: None,
        };
        self.store.put_auth_session(&session)?;
        self.record_idempotent(
            idempotency_key,
            "bootstrap_organization",
            &request,
            &serde_json::json!({
                "organization": organization,
                "user": user,
                "session": session,
            }),
        )?;
        Ok(BootstrapResult {
            organization,
            user,
            session,
            token: Some(token),
        })
    }

    /// Resolve one presented bearer token (auth session or service-account
    /// token) into a [`Principal`]. Unknown, revoked, expired and disabled
    /// credentials are all typed `Unauthorized` (no partial answers).
    pub fn authenticate(&self, token: &str) -> Result<Principal, ControlPlaneError> {
        if token.is_empty() || token.len() > MAX_TOKEN_BYTES {
            return Err(ControlPlaneError::Unauthorized(
                "unknown control-plane credential".into(),
            ));
        }
        let hash = TokenHash::of(token);
        let now = self.now_ms();
        if let Some(session) = self.store.auth_session_by_token_hash(&hash)? {
            if !session.is_valid_at(now) {
                return Err(ControlPlaneError::Unauthorized(
                    "control-plane session is expired or revoked".into(),
                ));
            }
            let user = self
                .store
                .user(&session.user)?
                .ok_or_else(|| ControlPlaneError::Unauthorized("user no longer exists".into()))?;
            if user.disabled {
                return Err(ControlPlaneError::Unauthorized("user is disabled".into()));
            }
            let organization =
                self.store
                    .organization(&session.organization)?
                    .ok_or_else(|| {
                        ControlPlaneError::Unauthorized("organization no longer exists".into())
                    })?;
            if organization.deleted {
                return Err(ControlPlaneError::Unauthorized(
                    "organization is deleted".into(),
                ));
            }
            let membership = self
                .store
                .membership(&session.organization, &session.user)?
                .ok_or_else(|| {
                    ControlPlaneError::Unauthorized("membership no longer exists".into())
                })?;
            return Ok(Principal::user(
                session.user,
                session.organization,
                membership.role,
            ));
        }
        if let Some(account) = self.store.service_account_by_token_hash(&hash)? {
            if account.disabled {
                return Err(ControlPlaneError::Unauthorized(
                    "service account is disabled".into(),
                ));
            }
            account.validate()?;
            return Ok(Principal::service_account(
                account.id,
                account.organization,
                account.role,
                account.scopes,
            ));
        }
        Err(ControlPlaneError::Unauthorized(
            "unknown control-plane credential".into(),
        ))
    }

    /// The caller's identity view (used by `GET /native/identity`).
    pub fn identity(&self, principal: &Principal) -> Result<IdentityView, ControlPlaneError> {
        let organization = self
            .store
            .organization(&principal.organization)?
            .ok_or_else(|| ControlPlaneError::NotFound("organization not found".into()))?;
        let (subject_id, display_name, email) = match &principal.subject {
            crate::rbac::PrincipalSubject::User(id) => {
                let user = self
                    .store
                    .user(id)?
                    .ok_or_else(|| ControlPlaneError::NotFound("user not found".into()))?;
                (
                    user.id.as_str().to_string(),
                    user.display_name,
                    Some(user.email),
                )
            }
            crate::rbac::PrincipalSubject::ServiceAccount(id) => {
                let account = self.store.service_account(id)?.ok_or_else(|| {
                    ControlPlaneError::NotFound("service account not found".into())
                })?;
                (account.id.as_str().to_string(), account.name, None)
            }
        };
        Ok(IdentityView {
            subject_kind: principal.subject.as_str().to_string(),
            subject_id,
            display_name,
            email,
            organization: organization.id.clone(),
            organization_name: organization.name,
            role: principal.role,
            scopes: principal.scopes.clone().map(|s| s.into_iter().collect()),
            effective_actions: principal.effective_actions(),
        })
    }

    // --------------------------------------------------------- membership

    /// Invite one email into an organization (idempotency-keyed).
    pub fn invite(
        &self,
        principal: &Principal,
        organization: &OrganizationId,
        email: &str,
        role: Role,
        idempotency_key: &str,
    ) -> Result<InvitationIssued, ControlPlaneError> {
        authorize(
            principal,
            organization,
            Resource::Member,
            Action::MemberWrite,
        )?;
        let email = normalize_email(email)?;
        let request = serde_json::json!({
            "organization": organization.as_str(),
            "email": email,
            "role": role.as_str(),
        });
        if let Some(record) =
            self.idempotency_precheck::<InvitationRecord>(idempotency_key, "invite", &request)?
        {
            return Ok(InvitationIssued {
                invitation: record.invitation,
                token: None,
            });
        }
        // An email that is already a member is a typed conflict, never a
        // second membership.
        if let Some(user) = self.store.user_by_email(&email)? {
            if self.store.membership(organization, &user.id)?.is_some() {
                return Err(ControlPlaneError::Conflict(
                    "this email is already a member of the organization".into(),
                ));
            }
        }
        let invited_by = match &principal.subject {
            crate::rbac::PrincipalSubject::User(id) => id.clone(),
            crate::rbac::PrincipalSubject::ServiceAccount(_) => {
                return Err(ControlPlaneError::Forbidden(
                    "a service account cannot invite members".into(),
                ))
            }
        };
        let token = Self::new_token()?;
        let invitation = Invitation {
            id: InvitationId::try_new(Self::new_id("inv"))?,
            organization: organization.clone(),
            email,
            role,
            status: InvitationStatus::Pending,
            invited_by,
            token_hash: TokenHash::of(token.expose()),
            created_ms: self.now_ms(),
            expires_ms: self.now_ms().saturating_add(DEFAULT_INVITATION_TTL_MS),
            decided_ms: None,
        };
        self.store.put_invitation(&invitation)?;
        self.record_idempotent(
            idempotency_key,
            "invite",
            &request,
            &serde_json::json!({ "invitation": invitation }),
        )?;
        Ok(InvitationIssued {
            invitation,
            token: Some(token),
        })
    }

    /// Accept one invitation (the accepting user must already exist and
    /// match the invited email).
    pub fn accept_invitation(
        &self,
        token: &str,
        user: &UserId,
    ) -> Result<Membership, ControlPlaneError> {
        if token.is_empty() || token.len() > MAX_TOKEN_BYTES {
            return Err(ControlPlaneError::Unauthorized("unknown invitation".into()));
        }
        let hash = TokenHash::of(token);
        let invitation = self
            .store
            .invitation_by_token_hash(&hash)?
            .ok_or_else(|| ControlPlaneError::Unauthorized("unknown invitation".into()))?;
        let now = self.now_ms();
        match invitation.status_at(now) {
            InvitationStatus::Accepted => {
                return Err(ControlPlaneError::Conflict(
                    "invitation was already accepted".into(),
                ))
            }
            InvitationStatus::Revoked => {
                return Err(ControlPlaneError::Conflict("invitation was revoked".into()))
            }
            InvitationStatus::Expired => {
                return Err(ControlPlaneError::Conflict("invitation has expired".into()))
            }
            InvitationStatus::Pending => {}
        }
        let accepting = self
            .store
            .user(user)?
            .ok_or_else(|| ControlPlaneError::Unauthorized("unknown user".into()))?;
        if accepting.email != invitation.email {
            return Err(ControlPlaneError::Forbidden(
                "invitation belongs to a different email".into(),
            ));
        }
        if self
            .store
            .membership(&invitation.organization, user)?
            .is_some()
        {
            return Err(ControlPlaneError::Conflict(
                "this user is already a member of the organization".into(),
            ));
        }
        let membership = Membership {
            id: MembershipId::try_new(Self::new_id("mem"))?,
            organization: invitation.organization.clone(),
            user: user.clone(),
            role: invitation.role,
            created_ms: now,
        };
        self.store.put_membership(&membership)?;
        let mut accepted = invitation;
        accepted.status = InvitationStatus::Accepted;
        accepted.decided_ms = Some(now);
        self.store.put_invitation(&accepted)?;
        Ok(membership)
    }

    /// Revoke one pending invitation.
    pub fn revoke_invitation(
        &self,
        principal: &Principal,
        invitation_id: &InvitationId,
    ) -> Result<Invitation, ControlPlaneError> {
        let invitation = self
            .store
            .invitation(invitation_id)?
            .ok_or_else(|| ControlPlaneError::NotFound("invitation not found".into()))?;
        // The tenant check runs BEFORE anything is revealed: a foreign
        // organization's invitation is indistinguishable from a missing one
        // (same 404, same message).
        entity_authorize(
            authorize(
                principal,
                &invitation.organization,
                Resource::Member,
                Action::MemberWrite,
            ),
            "invitation",
        )?;
        match invitation.status_at(self.now_ms()) {
            InvitationStatus::Pending => {}
            InvitationStatus::Accepted => {
                return Err(ControlPlaneError::Conflict(
                    "an accepted invitation cannot be revoked".into(),
                ))
            }
            InvitationStatus::Revoked => {
                return Err(ControlPlaneError::Conflict(
                    "invitation was already revoked".into(),
                ))
            }
            InvitationStatus::Expired => {
                return Err(ControlPlaneError::Conflict("invitation has expired".into()))
            }
        }
        let mut revoked = invitation;
        revoked.status = InvitationStatus::Revoked;
        revoked.decided_ms = Some(self.now_ms());
        self.store.put_invitation(&revoked)?;
        Ok(revoked)
    }

    /// One page of the organization's invitations.
    pub fn invitations(
        &self,
        principal: &Principal,
        organization: &OrganizationId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Page<Invitation>, ControlPlaneError> {
        authorize(
            principal,
            organization,
            Resource::Member,
            Action::MemberRead,
        )?;
        let limit = checked_limit(limit)?;
        let mut items = self
            .store
            .invitations(organization, after, limit.saturating_add(1))?;
        let has_more = items.len() > limit;
        items.truncate(limit);
        let next_cursor = if has_more {
            items.last().map(|i| i.id.as_str().to_string())
        } else {
            None
        };
        Ok(Page { items, next_cursor })
    }

    /// One page of the organization's members (with their users).
    pub fn members(
        &self,
        principal: &Principal,
        organization: &OrganizationId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Page<MemberView>, ControlPlaneError> {
        authorize(
            principal,
            organization,
            Resource::Member,
            Action::MemberRead,
        )?;
        let limit = checked_limit(limit)?;
        let mut rows = self
            .store
            .memberships(organization, after, limit.saturating_add(1))?;
        let has_more = rows.len() > limit;
        rows.truncate(limit);
        let next_cursor = if has_more {
            rows.last().map(|m| m.id.as_str().to_string())
        } else {
            None
        };
        let mut items = Vec::with_capacity(rows.len());
        for membership in rows {
            let user = self
                .store
                .user(&membership.user)?
                .ok_or_else(|| ControlPlaneError::Backend("membership user is missing".into()))?;
            items.push(MemberView { membership, user });
        }
        Ok(Page { items, next_cursor })
    }

    // ---------------------------------------------------- service accounts

    /// Create one service account (Admin+ only; the owner role is refused
    /// for machine principals, and the scope set must be explicit).
    pub fn create_service_account(
        &self,
        principal: &Principal,
        organization: &OrganizationId,
        name: &str,
        role: Role,
        scopes: Vec<Action>,
    ) -> Result<ServiceAccountIssued, ControlPlaneError> {
        authorize(
            principal,
            organization,
            Resource::Member,
            Action::MemberWrite,
        )?;
        if role == Role::Owner {
            return Err(ControlPlaneError::Malformed(
                "a service account cannot hold the owner role".into(),
            ));
        }
        let token = Self::new_token()?;
        let account = ServiceAccount {
            id: ServiceAccountId::try_new(Self::new_id("sa"))?,
            organization: organization.clone(),
            name: name.to_string(),
            role,
            scopes,
            token_hash: TokenHash::of(token.expose()),
            created_ms: self.now_ms(),
            disabled: false,
        };
        account.validate()?;
        self.store.put_service_account(&account)?;
        Ok(ServiceAccountIssued {
            account,
            token: Some(token),
        })
    }

    /// One page of the organization's service accounts (metadata only —
    /// token hashes never leave the store as a readable credential).
    pub fn service_accounts(
        &self,
        principal: &Principal,
        organization: &OrganizationId,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Page<ServiceAccount>, ControlPlaneError> {
        authorize(
            principal,
            organization,
            Resource::Member,
            Action::MemberRead,
        )?;
        let limit = checked_limit(limit)?;
        let mut items =
            self.store
                .service_accounts(organization, after, limit.saturating_add(1))?;
        let has_more = items.len() > limit;
        items.truncate(limit);
        let next_cursor = if has_more {
            items.last().map(|a| a.id.as_str().to_string())
        } else {
            None
        };
        Ok(Page { items, next_cursor })
    }

    // ------------------------------------------------------------ approvals

    /// Request one approval (idempotency-keyed).
    pub fn request_approval(
        &self,
        principal: &Principal,
        organization: &OrganizationId,
        action: Action,
        resource: &str,
        reason: &str,
        idempotency_key: &str,
    ) -> Result<ApprovalRequest, ControlPlaneError> {
        authorize(
            principal,
            organization,
            Resource::Approval,
            Action::ApprovalRequest,
        )?;
        if resource.is_empty() || resource.len() > 256 {
            return Err(ControlPlaneError::Malformed(
                "approval resource must be 1..=256 bytes".into(),
            ));
        }
        if reason.len() > MAX_NOTE_BYTES {
            return Err(ControlPlaneError::Malformed(
                "approval reason is oversized".into(),
            ));
        }
        let requested_by = match &principal.subject {
            crate::rbac::PrincipalSubject::User(id) => id.clone(),
            crate::rbac::PrincipalSubject::ServiceAccount(_) => {
                return Err(ControlPlaneError::Forbidden(
                    "a service account cannot request approvals on behalf of a user".into(),
                ))
            }
        };
        let request = serde_json::json!({
            "organization": organization.as_str(),
            "action": action.as_str(),
            "resource": resource,
            "reason": reason,
        });
        if let Some(approval) = self.idempotency_precheck::<ApprovalRecord>(
            idempotency_key,
            "request_approval",
            &request,
        )? {
            return Ok(approval.approval);
        }
        let approval = ApprovalRequest {
            id: ApprovalId::try_new(Self::new_id("apr"))?,
            organization: organization.clone(),
            action,
            resource: resource.to_string(),
            requested_by,
            reason: reason.to_string(),
            status: ApprovalStatus::Open,
            decided_by: None,
            note: None,
            created_ms: self.now_ms(),
            decided_ms: None,
        };
        self.store.put_approval(&approval)?;
        self.record_idempotent(
            idempotency_key,
            "request_approval",
            &request,
            &serde_json::json!({ "approval": approval }),
        )?;
        Ok(approval)
    }

    /// Decide one approval (Admin+ only; exactly once). A foreign
    /// organization's approval is a `NotFound` with no existence leak.
    pub fn decide_approval(
        &self,
        principal: &Principal,
        approval_id: &ApprovalId,
        approved: bool,
        note: &str,
    ) -> Result<ApprovalRequest, ControlPlaneError> {
        if note.len() > MAX_NOTE_BYTES {
            return Err(ControlPlaneError::Malformed(
                "decision note is oversized".into(),
            ));
        }
        let approval = self
            .store
            .approval(approval_id)?
            .ok_or_else(|| ControlPlaneError::NotFound("approval not found".into()))?;
        entity_authorize(
            authorize(
                principal,
                &approval.organization,
                Resource::Approval,
                Action::ApprovalDecide,
            ),
            "approval",
        )?;
        if approval.status != ApprovalStatus::Open {
            return Err(ControlPlaneError::Conflict(format!(
                "approval is already {}",
                approval.status.as_str()
            )));
        }
        let decided_by = match &principal.subject {
            crate::rbac::PrincipalSubject::User(id) => id.clone(),
            crate::rbac::PrincipalSubject::ServiceAccount(_) => {
                return Err(ControlPlaneError::Forbidden(
                    "a service account cannot decide approvals".into(),
                ))
            }
        };
        let mut decided = approval;
        decided.status = if approved {
            ApprovalStatus::Approved
        } else {
            ApprovalStatus::Rejected
        };
        decided.decided_by = Some(decided_by);
        decided.note = if note.is_empty() {
            None
        } else {
            Some(note.to_string())
        };
        decided.decided_ms = Some(self.now_ms());
        self.store.put_approval(&decided)?;
        Ok(decided)
    }

    /// One page of the organization's approvals, optionally filtered by
    /// status.
    pub fn approvals(
        &self,
        principal: &Principal,
        organization: &OrganizationId,
        status: Option<ApprovalStatus>,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Page<ApprovalRequest>, ControlPlaneError> {
        authorize(
            principal,
            organization,
            Resource::Approval,
            Action::ApprovalRequest,
        )?;
        let limit = checked_limit(limit)?;
        let mut items =
            self.store
                .approvals(organization, status, after, limit.saturating_add(1))?;
        let has_more = items.len() > limit;
        items.truncate(limit);
        let next_cursor = if has_more {
            items.last().map(|a| a.id.as_str().to_string())
        } else {
            None
        };
        Ok(Page { items, next_cursor })
    }
}

/// The recorded (safe) invitation response: no plaintext token.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvitationRecord {
    pub invitation: Invitation,
}

/// The recorded (safe) approval response.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRecord {
    pub approval: ApprovalRequest,
}

/// The identity view returned by `GET /native/identity` (no secrets).
#[derive(Debug, Clone, Serialize)]
pub struct IdentityView {
    pub subject_kind: String,
    pub subject_id: String,
    pub display_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub organization: OrganizationId,
    pub organization_name: String,
    pub role: Role,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scopes: Option<Vec<Action>>,
    pub effective_actions: Vec<Action>,
}

/// Map a foreign-organization refusal on a fetched-by-id entity to the SAME
/// `NotFound` a missing entity produces: the caller learns nothing about
/// whether the id exists in another tenant.
fn entity_authorize(
    decision: Result<(), crate::rbac::Denied>,
    entity: &str,
) -> Result<(), ControlPlaneError> {
    match decision {
        Ok(()) => Ok(()),
        Err(crate::rbac::Denied::NotFound { .. }) => {
            Err(ControlPlaneError::NotFound(format!("{entity} not found")))
        }
        Err(other) => Err(other.into()),
    }
}

fn checked_limit(limit: usize) -> Result<usize, ControlPlaneError> {
    if limit == 0 || limit > MAX_PAGE {
        return Err(ControlPlaneError::Malformed(format!(
            "limit must be 1..={MAX_PAGE}"
        )));
    }
    Ok(limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryControlPlaneStore;

    fn service() -> ControlPlane {
        ControlPlane::new(
            Arc::new(MemoryControlPlaneStore::new()),
            Arc::new(ManualClock::new(1_000_000)),
        )
    }

    #[test]
    fn sha256_is_the_standard_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn idempotency_keys_are_strictly_shaped() {
        assert!(ControlPlane::validate_idempotency_key("").is_err());
        assert!(ControlPlane::validate_idempotency_key("has space").is_err());
        assert!(ControlPlane::validate_idempotency_key(&"k".repeat(201)).is_err());
        assert!(ControlPlane::validate_idempotency_key("org-create-1").is_ok());
    }

    #[test]
    fn bootstrap_is_idempotent_and_replays_without_a_token() {
        let cp = service();
        let first = cp
            .bootstrap_organization("Acme", "owner@acme.test", "Owner", "key-1")
            .unwrap();
        assert!(first.token.is_some());
        let second = cp
            .bootstrap_organization("Acme", "owner@acme.test", "Owner", "key-1")
            .unwrap();
        assert_eq!(second.organization.id, first.organization.id);
        assert!(
            second.token.is_none(),
            "a replayed response never re-presents the plaintext token"
        );
        // The same key with a different request conflicts.
        let conflict = cp
            .bootstrap_organization("Other", "owner@acme.test", "Owner", "key-1")
            .unwrap_err();
        assert_eq!(conflict.code(), "conflict");
    }

    #[test]
    fn authentication_rejects_unknown_and_revoked_credentials() {
        let cp = service();
        let boot = cp
            .bootstrap_organization("Acme", "owner@acme.test", "Owner", "k")
            .unwrap();
        let token = boot.token.clone().unwrap();
        let principal = cp.authenticate(token.expose()).unwrap();
        assert_eq!(principal.role, Role::Owner);
        assert!(cp.authenticate("not-a-token").is_err());
        assert!(cp.authenticate("").is_err());
        let mut revoked = boot.session.clone();
        revoked.revoked_ms = Some(1);
        cp.store.put_auth_session(&revoked).unwrap();
        assert!(matches!(
            cp.authenticate(token.expose()).unwrap_err(),
            ControlPlaneError::Unauthorized(_)
        ));
    }

    #[test]
    fn expired_sessions_and_disabled_accounts_are_unauthorized() {
        let store: Arc<dyn ControlPlaneStore> = Arc::new(MemoryControlPlaneStore::new());
        let clock = Arc::new(ManualClock::new(1_000));
        let cp = ControlPlane::new(store, clock.clone());
        let boot = cp
            .bootstrap_organization("Acme", "owner@acme.test", "Owner", "k")
            .unwrap();
        let token = boot.token.clone().unwrap().expose().to_string();
        clock.advance(DEFAULT_SESSION_TTL_MS + 1);
        assert!(matches!(
            cp.authenticate(&token).unwrap_err(),
            ControlPlaneError::Unauthorized(_)
        ));
        // A disabled user is refused even with a valid session.
        let cp = service();
        let boot = cp
            .bootstrap_organization("Acme", "owner@acme.test", "Owner", "k")
            .unwrap();
        let mut disabled = boot.user.clone();
        disabled.disabled = true;
        cp.store.put_user(&disabled).unwrap();
        assert!(matches!(
            cp.authenticate(boot.token.unwrap().expose()).unwrap_err(),
            ControlPlaneError::Unauthorized(_)
        ));
    }

    #[test]
    fn invitation_lifecycle_is_terminal_and_tenant_scoped() {
        let cp = service();
        let boot = cp
            .bootstrap_organization("Acme", "owner@acme.test", "Owner", "k")
            .unwrap();
        let owner = cp
            .authenticate(boot.token.as_ref().unwrap().expose())
            .unwrap();
        let org = boot.organization.id.clone();

        let issued = cp
            .invite(&owner, &org, "new@acme.test", Role::Member, "inv-key-1")
            .unwrap();
        assert_eq!(
            issued.invitation.status_at(cp.now_ms()),
            InvitationStatus::Pending
        );
        let token = issued.token.as_ref().unwrap().expose().to_string();
        // An idempotent replay returns the SAME invitation without a token.
        let replay = cp
            .invite(&owner, &org, "new@acme.test", Role::Member, "inv-key-1")
            .unwrap();
        assert_eq!(replay.invitation.id, issued.invitation.id);
        assert!(replay.token.is_none());

        let (stranger, _) = cp.create_user("stranger@x.test", "S").unwrap();
        assert!(matches!(
            cp.accept_invitation(&token, &stranger.id).unwrap_err(),
            ControlPlaneError::Forbidden(_)
        ));

        let (invitee, _) = cp.create_user("new@acme.test", "N").unwrap();
        let membership = cp.accept_invitation(&token, &invitee.id).unwrap();
        assert_eq!(membership.role, Role::Member);
        // A second accept is a terminal conflict.
        assert!(matches!(
            cp.accept_invitation(&token, &invitee.id).unwrap_err(),
            ControlPlaneError::Conflict(_)
        ));
        // The new member can authenticate into the same organization.
        let page = cp.members(&owner, &org, None, 10).unwrap();
        assert_eq!(page.items.len(), 2);
    }

    #[test]
    fn expired_and_revoked_invitations_are_refused() {
        let store: Arc<dyn ControlPlaneStore> = Arc::new(MemoryControlPlaneStore::new());
        let clock = Arc::new(ManualClock::new(1_000));
        let cp = ControlPlane::new(store, clock.clone());
        let boot = cp
            .bootstrap_organization("Acme", "owner@acme.test", "Owner", "k")
            .unwrap();
        let owner = cp
            .authenticate(boot.token.as_ref().unwrap().expose())
            .unwrap();
        let org = boot.organization.id.clone();
        let (invitee, _) = cp.create_user("new@acme.test", "N").unwrap();

        let expired = cp
            .invite(&owner, &org, "new@acme.test", Role::Member, "e1")
            .unwrap();
        clock.advance(DEFAULT_INVITATION_TTL_MS + 1);
        assert!(matches!(
            cp.accept_invitation(expired.token.as_ref().unwrap().expose(), &invitee.id)
                .unwrap_err(),
            ControlPlaneError::Conflict(_)
        ));

        let revoked = cp
            .invite(&owner, &org, "new@acme.test", Role::Member, "e2")
            .unwrap();
        let revoked = cp
            .revoke_invitation(&owner, &revoked.invitation.id)
            .unwrap();
        assert_eq!(revoked.status, InvitationStatus::Revoked);
        assert!(matches!(
            cp.accept_invitation(
                // The plaintext token is gone with the issuance response; the
                // row's hash is the only record, so acceptance by a new token
                // can never match.
                "wrong-token",
                &invitee.id
            )
            .unwrap_err(),
            ControlPlaneError::Unauthorized(_)
        ));
    }

    #[test]
    fn service_account_scoping_is_role_intersected_with_scope() {
        let cp = service();
        let boot = cp
            .bootstrap_organization("Acme", "owner@acme.test", "Owner", "k")
            .unwrap();
        let owner = cp
            .authenticate(boot.token.as_ref().unwrap().expose())
            .unwrap();
        let org = boot.organization.id.clone();
        let issued = cp
            .create_service_account(
                &owner,
                &org,
                "ci",
                Role::Member,
                vec![Action::RepositoryRead, Action::RunCreate],
            )
            .unwrap();
        let principal = cp
            .authenticate(issued.token.as_ref().unwrap().expose())
            .unwrap();
        assert!(principal.is_service_account());
        assert!(authorize(
            &principal,
            &org,
            Resource::Repository,
            Action::RepositoryRead
        )
        .is_ok());
        assert!(authorize(
            &principal,
            &org,
            Resource::Repository,
            Action::RepositoryWrite
        )
        .is_err());
        assert!(authorize(&principal, &org, Resource::Member, Action::MemberRead).is_err());
        // Owner-role service accounts are refused outright.
        assert!(cp
            .create_service_account(
                &owner,
                &org,
                "root",
                Role::Owner,
                vec![Action::RepositoryRead]
            )
            .is_err());
        // Unscoped accounts are refused too.
        assert!(cp
            .create_service_account(&owner, &org, "noscope", Role::Member, vec![])
            .is_err());
        // A VIEWER session cannot mint service accounts at all.
        let (viewer_user, _) = cp.create_user("v@acme.test", "V").unwrap();
        cp.store
            .put_membership(&Membership {
                id: MembershipId::try_new("mem_viewer").unwrap(),
                organization: org.clone(),
                user: viewer_user.id.clone(),
                role: Role::Viewer,
                created_ms: 1,
            })
            .unwrap();
        let session = cp
            .store
            .auth_session_by_token_hash(&TokenHash::of("viewer-token"))
            .unwrap();
        assert!(session.is_none());
        let viewer_token = SecretToken::try_new("viewer-token").unwrap();
        cp.store
            .put_auth_session(&AuthSession {
                id: AuthSessionId::try_new("ses_viewer").unwrap(),
                organization: org.clone(),
                user: viewer_user.id,
                token_hash: TokenHash::of(viewer_token.expose()),
                created_ms: 1,
                expires_ms: cp.now_ms() + DEFAULT_SESSION_TTL_MS,
                revoked_ms: None,
            })
            .unwrap();
        let viewer = cp.authenticate(viewer_token.expose()).unwrap();
        assert!(cp
            .create_service_account(
                &viewer,
                &org,
                "nope",
                Role::Member,
                vec![Action::RepositoryRead]
            )
            .is_err());
    }

    #[test]
    fn approvals_are_requested_and_decided_exactly_once() {
        let cp = service();
        let boot = cp
            .bootstrap_organization("Acme", "owner@acme.test", "Owner", "k")
            .unwrap();
        let owner = cp
            .authenticate(boot.token.as_ref().unwrap().expose())
            .unwrap();
        let org = boot.organization.id.clone();
        let approval = cp
            .request_approval(
                &owner,
                &org,
                Action::SecretWrite,
                "secret:prod",
                "rotate",
                "apr-key-1",
            )
            .unwrap();
        assert_eq!(approval.status, ApprovalStatus::Open);
        let replay = cp
            .request_approval(
                &owner,
                &org,
                Action::SecretWrite,
                "secret:prod",
                "rotate",
                "apr-key-1",
            )
            .unwrap();
        assert_eq!(replay.id, approval.id);
        let decided = cp
            .decide_approval(&owner, &approval.id, true, "ok")
            .unwrap();
        assert_eq!(decided.status, ApprovalStatus::Approved);
        assert_eq!(decided.decided_by.as_ref(), Some(&boot.user.id));
        assert!(matches!(
            cp.decide_approval(&owner, &approval.id, false, "")
                .unwrap_err(),
            ControlPlaneError::Conflict(_)
        ));
    }
}
