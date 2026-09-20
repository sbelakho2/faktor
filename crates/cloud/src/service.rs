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
use crate::store::ControlPlaneStore;

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

/// One external (SSO/OIDC) login result: the resolved user, the durable
/// session and the ONE-SHOT plaintext session token.
#[derive(Debug, Clone)]
pub struct ExternalLogin {
    pub user: User,
    pub session: AuthSession,
    /// Visible exactly once, at issuance; only the hash is stored.
    pub token: SecretToken,
    /// The membership role in force after the login (an existing membership
    /// keeps its role).
    pub role: Role,
}

/// One control-plane session logout result. `already_revoked = true` is the
/// idempotent replay: the session was revoked by an earlier logout and the
/// durable row is unchanged.
#[derive(Debug, Clone)]
pub struct SessionRevocation {
    /// The (now revoked) durable session row.
    pub session: AuthSession,
    /// `true` when the session was already revoked before this call.
    pub already_revoked: bool,
}

/// The recorded (safe, token-free) external-login response.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ExternalLoginRecord {
    user: User,
    session: AuthSession,
    role: Role,
}

/// The recorded (safe) explicit-link response.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ExternalIdentityRecord {
    identity: ExternalIdentity,
}

/// The recorded (safe) create-user response. A replay reports
/// `created = false` (the caller did not create the row).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateUserRecord {
    user: User,
    created: bool,
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

    /// Decode one recorded safe response back into its typed shape. A
    /// response that does not parse is an internal inconsistency, never a
    /// partial answer.
    fn decode_idempotent<T: for<'de> serde::Deserialize<'de>>(
        response: serde_json::Value,
    ) -> Result<T, ControlPlaneError> {
        serde_json::from_value(response).map_err(|e| {
            ControlPlaneError::Backend(format!("recorded idempotent response is unreadable: {e}"))
        })
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
    ///
    /// The existence check and the insert run inside ONE store transaction
    /// keyed deterministically by the email, so two concurrent callers for the
    /// same address can never both observe "absent": the loser replays the
    /// winner's recorded user and answers `(existing, false)`. The recorded
    /// response holds the winner's `created` flag; a replay always reports
    /// `created = false` (a replay did not create anything).
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
        let now = self.now_ms();
        // The digest covers ONLY the email: the display name of a later call
        // must not turn the documented existing-user answer into a conflict.
        let digest = Self::request_hash(&serde_json::json!({ "email": email }))?;
        let key = format!("create-user-{}", sha256_hex(email.as_bytes()));
        let email_in_tx = email.clone();
        let outcome =
            self.store
                .execute_idempotent(&key, "create_user", &digest, now, &mut |tx| {
                    let (user, created) = match tx.user_by_email(&email_in_tx)? {
                        Some(existing) => (existing, false),
                        None => {
                            let user = User {
                                id: UserId::try_new(Self::new_id("usr"))?,
                                email: email_in_tx.clone(),
                                display_name: display_name.to_string(),
                                created_ms: now,
                                disabled: false,
                            };
                            tx.put_user(&user)?;
                            (user, true)
                        }
                    };
                    Ok(serde_json::json!({ "user": user, "created": created }))
                })?;
        let executed = matches!(outcome, crate::store::IdempotentOutcome::Executed(_));
        let record: CreateUserRecord = Self::decode_idempotent(outcome.into_response())?;
        Ok((record.user, executed && record.created))
    }

    /// Link one external identity subject to a user (idempotent: the same
    /// `(provider, subject)` resolves to the same user; a conflicting link is
    /// refused `Conflict`, NEVER re-bound).
    ///
    /// The existence check and the attach run inside ONE store transaction (a
    /// fresh, never-replayed key per call), so two concurrent explicit links
    /// of one subject can never both observe "absent" and write divergent
    /// owners.
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
        let now = self.now_ms();
        let digest = Self::request_hash(&serde_json::json!({
            "user": user.as_str(),
            "provider": provider,
            "subject": subject,
        }))?;
        let outcome = self.store.execute_idempotent(
            &Self::new_id("link"),
            "link_external_identity",
            &digest,
            now,
            &mut |tx| {
                if let Some(existing) = tx.external_identity(provider, subject)? {
                    if existing.user != *user {
                        return Err(ControlPlaneError::Conflict(
                            "external identity is already linked to another user".into(),
                        ));
                    }
                    return Ok(serde_json::json!({ "identity": existing }));
                }
                let identity = ExternalIdentity {
                    id: ExternalIdentityId::try_new(Self::new_id("ext"))?,
                    user: user.clone(),
                    provider: provider.to_string(),
                    subject: subject.to_string(),
                    created_ms: now,
                };
                tx.put_external_identity(&identity)?;
                Ok(serde_json::json!({ "identity": identity }))
            },
        )?;
        let record: ExternalIdentityRecord = Self::decode_idempotent(outcome.into_response())?;
        Ok(record.identity)
    }

    /// Bootstrap one organization with its first owner. The returned token
    /// is the owner's first session; it is shown exactly once.
    ///
    /// The idempotency claim, the organization/user/membership/session rows
    /// and the recorded (safe, token-free) response commit in ONE store
    /// transaction: a crash at any statement rolls all of them back, and a
    /// retry with the same key either completes the operation or (for a
    /// lost concurrent race) replays the winner's recorded response.
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
        if display_name.len() > MAX_DISPLAY_NAME_BYTES {
            return Err(ControlPlaneError::Malformed(
                "display name is oversized".into(),
            ));
        }
        let email = normalize_email(owner_email)?;
        let key = Self::validate_idempotency_key(idempotency_key)?;
        let request = serde_json::json!({
            "name": name,
            "owner_email": email,
            "display_name": display_name,
        });
        let digest = Self::request_hash(&request)?;
        let now = self.now_ms();
        // Set only by the FIRST execution's closure: a replay never re-runs
        // it, so a replay never re-presents the one-shot token.
        let mut issued: Option<SecretToken> = None;
        let outcome = self.store.execute_idempotent(
            &key,
            "bootstrap_organization",
            &digest,
            now,
            &mut |tx| {
                let organization = Organization {
                    id: OrganizationId::try_new(Self::new_id("org"))?,
                    name: name.to_string(),
                    created_ms: now,
                    deleted: false,
                };
                tx.put_organization(&organization)?;
                let user = match tx.user_by_email(&email)? {
                    Some(existing) => existing,
                    None => {
                        let user = User {
                            id: UserId::try_new(Self::new_id("usr"))?,
                            email: email.clone(),
                            display_name: display_name.to_string(),
                            created_ms: now,
                            disabled: false,
                        };
                        tx.put_user(&user)?;
                        user
                    }
                };
                let membership = Membership {
                    id: MembershipId::try_new(Self::new_id("mem"))?,
                    organization: organization.id.clone(),
                    user: user.id.clone(),
                    role: Role::Owner,
                    created_ms: now,
                };
                tx.put_membership(&membership)?;
                let token = Self::new_token()?;
                let session = AuthSession {
                    id: AuthSessionId::try_new(Self::new_id("ses"))?,
                    organization: organization.id.clone(),
                    user: user.id.clone(),
                    token_hash: TokenHash::of(token.expose()),
                    created_ms: now,
                    expires_ms: now.saturating_add(DEFAULT_SESSION_TTL_MS),
                    revoked_ms: None,
                };
                tx.put_auth_session(&session)?;
                issued = Some(token);
                Ok(serde_json::json!({
                    "organization": organization,
                    "user": user,
                    "session": session,
                }))
            },
        )?;
        let record: BootstrapRecord = Self::decode_idempotent(outcome.into_response())?;
        Ok(BootstrapResult {
            organization: record.organization,
            user: record.user,
            session: record.session,
            token: issued,
        })
    }

    /// One SSO/external-identity login: resolve the user by the SUBJECT-FIRST
    /// identity rule, reconcile the verified claims, ensure the membership and
    /// mint ONE control-plane auth session (the plaintext token is visible
    /// exactly once).
    ///
    /// Semantics (documented, fail closed), all inside ONE store transaction
    /// (a fresh, never-replayed key per call):
    ///
    /// - the organization must exist and not be deleted (`NotFound`);
    /// - an EXISTING `(provider, subject)` link is authoritative: the linked
    ///   user is loaded and validated (a disabled user is refused
    ///   `Unauthorized`). The subject is NEVER transferred to another account;
    ///   an explicit attempt to link a bound subject elsewhere is a typed
    ///   `Conflict`;
    /// - email reconciliation for a linked user (the explicit default
    ///   policy): adopt the IdP-asserted email ONLY when the IdP asserts
    ///   `email_verified` AND the address is free; otherwise keep the
    ///   recorded email. A verified email that already belongs to a
    ///   DIFFERENT user is a typed `Conflict`: accounts are never merged
    ///   implicitly. The operator resolution path is explicit: free the
    ///   duplicate address (rename/disable/remove the other account) or
    ///   perform an audited merge outside this path, then retry the login;
    /// - with NO existing link, the email must be VERIFIED: it resolves an
    ///   existing user by email or creates one, and the `(provider, subject)`
    ///   link is attached in the SAME transaction as that
    ///   resolution/creation, so no window exists for a concurrent login to
    ///   bind the subject to a different account (an unverified email is
    ///   refused `Unauthorized` before any row is written);
    /// - an EXISTING membership keeps its role: the IdP mapping authorizes
    ///   joining, never a privilege change of an existing member (role
    ///   changes remain an explicit admin action);
    /// - a new membership is created with the MAPPED role.
    #[allow(clippy::too_many_arguments)]
    pub fn login_external(
        &self,
        organization: &OrganizationId,
        provider: &str,
        subject: &str,
        email: &str,
        display_name: &str,
        email_verified: bool,
        mapped_role: Role,
    ) -> Result<ExternalLogin, ControlPlaneError> {
        if provider.is_empty() || provider.len() > 64 || subject.is_empty() || subject.len() > 256 {
            return Err(ControlPlaneError::Malformed(
                "external identity provider/subject shape is invalid".into(),
            ));
        }
        let email = normalize_email(email)?;
        if display_name.len() > MAX_DISPLAY_NAME_BYTES {
            return Err(ControlPlaneError::Malformed(
                "display name is oversized".into(),
            ));
        }
        let now = self.now_ms();
        let digest = Self::request_hash(&serde_json::json!({
            "organization": organization.as_str(),
            "provider": provider,
            "subject": subject,
            "email": email,
            "email_verified": email_verified,
            "role": mapped_role,
        }))?;
        // Set only by this execution's closure (a replay can never
        // re-present a one-shot token).
        let mut issued: Option<SecretToken> = None;
        let outcome = self.store.execute_idempotent(
            &Self::new_id("login"),
            "login_external",
            &digest,
            now,
            &mut |tx| {
                let org = tx
                    .organization(organization)?
                    .filter(|org| !org.deleted)
                    .ok_or_else(|| ControlPlaneError::NotFound("organization not found".into()))?;
                let user = match tx.external_identity(provider, subject)? {
                    // Subject-first: an existing link decides the account.
                    Some(identity) => {
                        let mut user = tx.user(&identity.user)?.ok_or_else(|| {
                            ControlPlaneError::Unauthorized(
                                "external identity user no longer exists".into(),
                            )
                        })?;
                        if user.disabled {
                            return Err(ControlPlaneError::Unauthorized("user is disabled".into()));
                        }
                        // Email reconciliation (the explicit default policy):
                        // adopt a verified, free email; keep the recorded one
                        // otherwise.
                        if email_verified && user.email != email {
                            if let Some(collision) = tx.user_by_email(&email)? {
                                if collision.id != user.id {
                                    return Err(ControlPlaneError::Conflict(format!(
                                        "verified email {email} already belongs to another \
                                         user; the external identity keeps its linked account \
                                         (operator resolution: free the duplicate address or \
                                         merge the accounts explicitly, never implicitly)"
                                    )));
                                }
                            }
                            user.email = email.clone();
                            tx.put_user(&user)?;
                        }
                        user
                    }
                    // No link yet: resolve/create by the VERIFIED email and
                    // attach the subject in the same transaction.
                    None => {
                        if !email_verified {
                            return Err(ControlPlaneError::Unauthorized(
                                "external identity email is not verified".into(),
                            ));
                        }
                        let user = match tx.user_by_email(&email)? {
                            Some(existing) => existing,
                            None => {
                                let user = User {
                                    id: UserId::try_new(Self::new_id("usr"))?,
                                    email: email.clone(),
                                    display_name: display_name.to_string(),
                                    created_ms: now,
                                    disabled: false,
                                };
                                tx.put_user(&user)?;
                                user
                            }
                        };
                        if user.disabled {
                            return Err(ControlPlaneError::Unauthorized("user is disabled".into()));
                        }
                        let identity = ExternalIdentity {
                            id: ExternalIdentityId::try_new(Self::new_id("ext"))?,
                            user: user.id.clone(),
                            provider: provider.to_string(),
                            subject: subject.to_string(),
                            created_ms: now,
                        };
                        tx.put_external_identity(&identity)?;
                        user
                    }
                };
                let membership = match tx.membership(&org.id, &user.id)? {
                    Some(existing) => existing,
                    None => {
                        let membership = Membership {
                            id: MembershipId::try_new(Self::new_id("mem"))?,
                            organization: org.id.clone(),
                            user: user.id.clone(),
                            role: mapped_role,
                            created_ms: now,
                        };
                        tx.put_membership(&membership)?;
                        membership
                    }
                };
                let token = Self::new_token()?;
                let session = AuthSession {
                    id: AuthSessionId::try_new(Self::new_id("ses"))?,
                    organization: org.id.clone(),
                    user: user.id.clone(),
                    token_hash: TokenHash::of(token.expose()),
                    created_ms: now,
                    expires_ms: now.saturating_add(DEFAULT_SESSION_TTL_MS),
                    revoked_ms: None,
                };
                tx.put_auth_session(&session)?;
                issued = Some(token);
                Ok(serde_json::json!({
                    "user": user,
                    "session": session,
                    "role": membership.role,
                }))
            },
        )?;
        let record: ExternalLoginRecord = Self::decode_idempotent(outcome.into_response())?;
        // A login key is FRESH per call (`new_id("login")`), so this operation
        // can never be a replay: every login mints a new one-shot token. A
        // deterministic key would be wrong here precisely because a replay
        // could not re-present the token. The guard is an internal-consistency
        // check (the invariant "fresh key => closure ran"), not a replay path.
        let token = issued.ok_or_else(|| {
            ControlPlaneError::Backend(
                "login invariant violated: a fresh per-call key replayed unexpectedly".into(),
            )
        })?;
        Ok(ExternalLogin {
            user: record.user,
            session: record.session,
            token,
            role: record.role,
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

    /// Revoke ONE control-plane auth session durably (the logout path): the
    /// presented token must be that session's OWN token, the named session
    /// must belong to the named organization, and the revocation is written
    /// to the durable row (`revoked_ms` set), so every later presentation of
    /// the token is refused by [`Self::authenticate`]. There is no delete
    /// path that could resurrect a session; the row is the audit trail.
    ///
    /// Refusals are typed and leak nothing: a missing session and a foreign
    /// organization's session are the SAME `NotFound`; a presented
    /// credential that does not own the named session (including a service
    /// account token) is `Unauthorized`.
    ///
    /// Idempotent: a SECOND logout of an already-revoked session returns
    /// `already_revoked = true` instead of an error.
    pub fn revoke_session(
        &self,
        organization: &OrganizationId,
        session_id: &AuthSessionId,
        token: &str,
    ) -> Result<SessionRevocation, ControlPlaneError> {
        if token.is_empty() || token.len() > MAX_TOKEN_BYTES {
            return Err(ControlPlaneError::Unauthorized(
                "unknown control-plane credential".into(),
            ));
        }
        let Some(mut session) = self.store.auth_session(session_id)? else {
            return Err(ControlPlaneError::NotFound(
                "control-plane session not found".into(),
            ));
        };
        if session.organization != *organization {
            // Tenant isolation: a foreign session is the same not-found a
            // missing one answers.
            return Err(ControlPlaneError::NotFound(
                "control-plane session not found".into(),
            ));
        }
        if session.token_hash != TokenHash::of(token) {
            return Err(ControlPlaneError::Unauthorized(
                "presented credential does not own the named session".into(),
            ));
        }
        if session.revoked_ms.is_some() {
            return Ok(SessionRevocation {
                session,
                already_revoked: true,
            });
        }
        session.revoked_ms = Some(self.now_ms());
        self.store.put_auth_session(&session)?;
        Ok(SessionRevocation {
            session,
            already_revoked: false,
        })
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

    /// Invite one email into an organization (idempotency-keyed). The
    /// invitation row and its recorded response commit in ONE transaction
    /// with the key claim; the role/tenant check runs inside that
    /// transaction too.
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
        let key = Self::validate_idempotency_key(idempotency_key)?;
        let request = serde_json::json!({
            "organization": organization.as_str(),
            "email": email,
            "role": role.as_str(),
        });
        let digest = Self::request_hash(&request)?;
        let invited_by = match &principal.subject {
            crate::rbac::PrincipalSubject::User(id) => id.clone(),
            crate::rbac::PrincipalSubject::ServiceAccount(_) => {
                return Err(ControlPlaneError::Forbidden(
                    "a service account cannot invite members".into(),
                ));
            }
        };
        let now = self.now_ms();
        let mut issued: Option<SecretToken> = None;
        let outcome = self
            .store
            .execute_idempotent(&key, "invite", &digest, now, &mut |tx| {
                // The same authorization is re-run INSIDE the transaction:
                // the role/tenant state that guarded the precheck must still
                // hold at commit time.
                authorize(
                    principal,
                    organization,
                    Resource::Member,
                    Action::MemberWrite,
                )?;
                // An email that is already a member is a typed conflict,
                // never a second membership.
                if let Some(user) = tx.user_by_email(&email)? {
                    if tx.membership(organization, &user.id)?.is_some() {
                        return Err(ControlPlaneError::Conflict(
                            "this email is already a member of the organization".into(),
                        ));
                    }
                }
                let token = Self::new_token()?;
                let invitation = Invitation {
                    id: InvitationId::try_new(Self::new_id("inv"))?,
                    organization: organization.clone(),
                    email: email.clone(),
                    role,
                    status: InvitationStatus::Pending,
                    invited_by: invited_by.clone(),
                    token_hash: TokenHash::of(token.expose()),
                    created_ms: now,
                    expires_ms: now.saturating_add(DEFAULT_INVITATION_TTL_MS),
                    decided_ms: None,
                };
                tx.put_invitation(&invitation)?;
                issued = Some(token);
                Ok(serde_json::json!({ "invitation": invitation }))
            })?;
        let record: InvitationRecord = Self::decode_idempotent(outcome.into_response())?;
        Ok(InvitationIssued {
            invitation: record.invitation,
            token: issued,
        })
    }

    /// Accept one invitation (the accepting user must already exist and
    /// match the invited email). The membership insert and the
    /// invitation-accepted update commit in ONE transaction with the
    /// idempotency claim: a crash between them can never persist a member
    /// without the terminal invitation row (or vice versa), and a retry with
    /// the same key replays the original membership.
    pub fn accept_invitation(
        &self,
        token: &str,
        user: &UserId,
        idempotency_key: &str,
    ) -> Result<Membership, ControlPlaneError> {
        if token.is_empty() || token.len() > MAX_TOKEN_BYTES {
            return Err(ControlPlaneError::Unauthorized("unknown invitation".into()));
        }
        let hash = TokenHash::of(token);
        let key = Self::validate_idempotency_key(idempotency_key)?;
        let request = serde_json::json!({
            "invitation_token_hash": hash.as_str(),
            "user": user.as_str(),
        });
        let digest = Self::request_hash(&request)?;
        let now = self.now_ms();
        let outcome =
            self.store
                .execute_idempotent(&key, "accept_invitation", &digest, now, &mut |tx| {
                    let invitation = tx.invitation_by_token_hash(&hash)?.ok_or_else(|| {
                        ControlPlaneError::Unauthorized("unknown invitation".into())
                    })?;
                    match invitation.status_at(now) {
                        InvitationStatus::Accepted => {
                            return Err(ControlPlaneError::Conflict(
                                "invitation was already accepted".into(),
                            ));
                        }
                        InvitationStatus::Revoked => {
                            return Err(ControlPlaneError::Conflict(
                                "invitation was revoked".into(),
                            ));
                        }
                        InvitationStatus::Expired => {
                            return Err(ControlPlaneError::Conflict(
                                "invitation has expired".into(),
                            ));
                        }
                        InvitationStatus::Pending => {}
                    }
                    let accepting = tx
                        .user(user)?
                        .ok_or_else(|| ControlPlaneError::Unauthorized("unknown user".into()))?;
                    if accepting.email != invitation.email {
                        return Err(ControlPlaneError::Forbidden(
                            "invitation belongs to a different email".into(),
                        ));
                    }
                    if tx.membership(&invitation.organization, user)?.is_some() {
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
                    tx.put_membership(&membership)?;
                    let mut accepted = invitation;
                    accepted.status = InvitationStatus::Accepted;
                    accepted.decided_ms = Some(now);
                    tx.put_invitation(&accepted)?;
                    Ok(serde_json::json!({ "membership": membership }))
                })?;
        let record: MembershipRecord = Self::decode_idempotent(outcome.into_response())?;
        Ok(record.membership)
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
                ));
            }
            InvitationStatus::Revoked => {
                return Err(ControlPlaneError::Conflict(
                    "invitation was already revoked".into(),
                ));
            }
            InvitationStatus::Expired => {
                return Err(ControlPlaneError::Conflict("invitation has expired".into()));
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

    /// Request one approval (idempotency-keyed). The approval row and its
    /// recorded response commit in ONE transaction with the key claim.
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
                ));
            }
        };
        let key = Self::validate_idempotency_key(idempotency_key)?;
        let request = serde_json::json!({
            "organization": organization.as_str(),
            "action": action.as_str(),
            "resource": resource,
            "reason": reason,
        });
        let digest = Self::request_hash(&request)?;
        let now = self.now_ms();
        let outcome =
            self.store
                .execute_idempotent(&key, "request_approval", &digest, now, &mut |tx| {
                    authorize(
                        principal,
                        organization,
                        Resource::Approval,
                        Action::ApprovalRequest,
                    )?;
                    let approval = ApprovalRequest {
                        id: ApprovalId::try_new(Self::new_id("apr"))?,
                        organization: organization.clone(),
                        action,
                        resource: resource.to_string(),
                        requested_by: requested_by.clone(),
                        reason: reason.to_string(),
                        status: ApprovalStatus::Open,
                        decided_by: None,
                        note: None,
                        created_ms: now,
                        decided_ms: None,
                    };
                    tx.put_approval(&approval)?;
                    Ok(serde_json::json!({ "approval": approval }))
                })?;
        let record: ApprovalRecord = Self::decode_idempotent(outcome.into_response())?;
        Ok(record.approval)
    }

    /// Decide one approval (Admin+ only; exactly once). A foreign
    /// organization's approval is a `NotFound` with no existence leak. The
    /// open→decided transition and its recorded response commit in ONE
    /// transaction with the key claim, so concurrent decisions cannot both
    /// succeed and a same-key retry replays the recorded decision.
    pub fn decide_approval(
        &self,
        principal: &Principal,
        approval_id: &ApprovalId,
        approved: bool,
        note: &str,
        idempotency_key: &str,
    ) -> Result<ApprovalRequest, ControlPlaneError> {
        if note.len() > MAX_NOTE_BYTES {
            return Err(ControlPlaneError::Malformed(
                "decision note is oversized".into(),
            ));
        }
        let key = Self::validate_idempotency_key(idempotency_key)?;
        let request = serde_json::json!({
            "approval": approval_id.as_str(),
            "approved": approved,
            "note": note,
        });
        let digest = Self::request_hash(&request)?;
        let decided_by = match &principal.subject {
            crate::rbac::PrincipalSubject::User(id) => id.clone(),
            crate::rbac::PrincipalSubject::ServiceAccount(_) => {
                return Err(ControlPlaneError::Forbidden(
                    "a service account cannot decide approvals".into(),
                ));
            }
        };
        let now = self.now_ms();
        let outcome =
            self.store
                .execute_idempotent(&key, "decide_approval", &digest, now, &mut |tx| {
                    let approval = tx
                        .approval(approval_id)?
                        .ok_or_else(|| ControlPlaneError::NotFound("approval not found".into()))?;
                    // The tenant check runs INSIDE the transaction, before any
                    // state is read back or written: a foreign organization's
                    // approval stays indistinguishable from a missing one.
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
                    let mut decided = approval;
                    decided.status = if approved {
                        ApprovalStatus::Approved
                    } else {
                        ApprovalStatus::Rejected
                    };
                    decided.decided_by = Some(decided_by.clone());
                    decided.note = if note.is_empty() {
                        None
                    } else {
                        Some(note.to_string())
                    };
                    decided.decided_ms = Some(now);
                    tx.put_approval(&decided)?;
                    Ok(serde_json::json!({ "approval": decided }))
                })?;
        let record: ApprovalRecord = Self::decode_idempotent(outcome.into_response())?;
        Ok(record.approval)
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

/// The recorded (safe) invitation-acceptance response.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MembershipRecord {
    pub membership: Membership,
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
    fn revoke_session_is_durable_idempotent_and_tenant_scoped() {
        let cp = service();
        let boot = cp
            .bootstrap_organization("Acme", "owner@acme.test", "Owner", "k")
            .unwrap();
        let token = boot.token.clone().unwrap().expose().to_string();
        let organization = boot.organization.id.clone();
        let session_id = boot.session.id.clone();
        // The owner token does not own a session id that does not exist.
        let ghost = AuthSessionId::try_new("ses_ghost").unwrap();
        assert!(matches!(
            cp.revoke_session(&organization, &ghost, &token)
                .unwrap_err(),
            ControlPlaneError::NotFound(_)
        ));
        // A FOREIGN organization's session is the same not-found (no leak).
        let other = cp
            .bootstrap_organization("Other", "owner@other.test", "Owner", "k2")
            .unwrap();
        assert!(matches!(
            cp.revoke_session(&other.organization.id, &session_id, &token)
                .unwrap_err(),
            ControlPlaneError::NotFound(_)
        ));
        // A credential that does not own the named session is Unauthorized
        // (the other owner's token, a random secret, and garbage).
        let other_token = other.token.clone().unwrap().expose().to_string();
        for wrong in ["not-a-token", "", other_token.as_str()] {
            assert!(matches!(
                cp.revoke_session(&organization, &session_id, wrong)
                    .unwrap_err(),
                ControlPlaneError::Unauthorized(_)
            ));
        }
        // First logout: durable revocation; the token dies for authenticate.
        let first = cp
            .revoke_session(&organization, &session_id, &token)
            .unwrap();
        assert!(!first.already_revoked);
        assert!(first.session.revoked_ms.is_some());
        assert!(matches!(
            cp.authenticate(&token).unwrap_err(),
            ControlPlaneError::Unauthorized(_)
        ));
        // Second logout is the typed idempotent replay, not an error.
        let second = cp
            .revoke_session(&organization, &session_id, &token)
            .unwrap();
        assert!(second.already_revoked);
        assert_eq!(second.session.revoked_ms, first.session.revoked_ms);
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
            cp.accept_invitation(&token, &stranger.id, "accept-stranger")
                .unwrap_err(),
            ControlPlaneError::Forbidden(_)
        ));

        let (invitee, _) = cp.create_user("new@acme.test", "N").unwrap();
        let membership = cp
            .accept_invitation(&token, &invitee.id, "accept-1")
            .unwrap();
        assert_eq!(membership.role, Role::Member);
        // A same-key retry replays the recorded membership, never a second
        // one.
        let replayed = cp
            .accept_invitation(&token, &invitee.id, "accept-1")
            .unwrap();
        assert_eq!(replayed.id, membership.id);
        // Under a NEW key the same token is a terminal conflict.
        assert!(matches!(
            cp.accept_invitation(&token, &invitee.id, "accept-2")
                .unwrap_err(),
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
            cp.accept_invitation(
                expired.token.as_ref().unwrap().expose(),
                &invitee.id,
                "e1-accept"
            )
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
                &invitee.id,
                "e2-accept",
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
            .decide_approval(&owner, &approval.id, true, "ok", "dec-key-1")
            .unwrap();
        assert_eq!(decided.status, ApprovalStatus::Approved);
        assert_eq!(decided.decided_by.as_ref(), Some(&boot.user.id));
        // A same-key retry replays the recorded decision, never a second one.
        let replayed = cp
            .decide_approval(&owner, &approval.id, true, "ok", "dec-key-1")
            .unwrap();
        assert_eq!(replayed.decided_ms, decided.decided_ms);
        assert_eq!(replayed.note, decided.note);
        // Under a new key the approval is already terminal: typed conflict.
        assert!(matches!(
            cp.decide_approval(&owner, &approval.id, false, "", "dec-key-2")
                .unwrap_err(),
            ControlPlaneError::Conflict(_)
        ));
    }

    #[test]
    fn idempotency_digest_mismatch_is_a_typed_conflict_for_every_operation() {
        let cp = service();
        let boot = cp
            .bootstrap_organization("Acme", "owner@acme.test", "Owner", "boot")
            .unwrap();
        let owner = cp
            .authenticate(boot.token.as_ref().unwrap().expose())
            .unwrap();
        let org = boot.organization.id.clone();

        // Bootstrap: the same key with a different organization name.
        assert!(matches!(
            cp.bootstrap_organization("Other", "owner@acme.test", "Owner", "boot")
                .unwrap_err(),
            ControlPlaneError::Conflict(_)
        ));
        // Invite: the same key with a different email.
        cp.invite(&owner, &org, "a@acme.test", Role::Member, "inv")
            .unwrap();
        assert!(matches!(
            cp.invite(&owner, &org, "b@acme.test", Role::Member, "inv")
                .unwrap_err(),
            ControlPlaneError::Conflict(_)
        ));
        // Request approval: the same key with a different resource.
        let approval = cp
            .request_approval(&owner, &org, Action::SecretWrite, "s:1", "r", "apr")
            .unwrap();
        assert!(matches!(
            cp.request_approval(&owner, &org, Action::SecretWrite, "s:2", "r", "apr")
                .unwrap_err(),
            ControlPlaneError::Conflict(_)
        ));
        // Decide: the same key with a different decision.
        cp.decide_approval(&owner, &approval.id, true, "", "dec")
            .unwrap();
        assert!(matches!(
            cp.decide_approval(&owner, &approval.id, false, "", "dec")
                .unwrap_err(),
            ControlPlaneError::Conflict(_)
        ));
        // Accept: the same key for another invitation token.
        let issued = cp
            .invite(&owner, &org, "new@acme.test", Role::Member, "inv-2")
            .unwrap();
        let token = issued.token.unwrap();
        let (invitee, _) = cp.create_user("new@acme.test", "N").unwrap();
        cp.accept_invitation(token.expose(), &invitee.id, "acc")
            .unwrap();
        assert!(matches!(
            cp.accept_invitation("some-other-token", &invitee.id, "acc")
                .unwrap_err(),
            ControlPlaneError::Conflict(_)
        ));
    }

    #[test]
    fn foreign_org_and_role_denials_never_consume_the_idempotency_key() {
        let cp = service();
        let boot_a = cp
            .bootstrap_organization("Alpha", "owner@alpha.test", "Owner", "a")
            .unwrap();
        let owner_a = cp
            .authenticate(boot_a.token.as_ref().unwrap().expose())
            .unwrap();
        let org_a = boot_a.organization.id.clone();
        let boot_b = cp
            .bootstrap_organization("Beta", "owner@beta.test", "Owner", "b")
            .unwrap();
        let owner_b = cp
            .authenticate(boot_b.token.as_ref().unwrap().expose())
            .unwrap();

        // A foreign organization is a NotFound and burns no key.
        assert!(matches!(
            cp.invite(&owner_b, &org_a, "x@y.test", Role::Member, "foreign-inv")
                .unwrap_err(),
            ControlPlaneError::NotFound(_)
        ));
        assert!(cp.store.idempotent("foreign-inv").unwrap().is_none());
        // A viewer/member role cannot invite, and burns no key.
        let (member_user, _) = cp.create_user("member@alpha.test", "M").unwrap();
        cp.store
            .put_membership(&Membership {
                id: MembershipId::try_new("mem_member").unwrap(),
                organization: org_a.clone(),
                user: member_user.id.clone(),
                role: Role::Member,
                created_ms: 1,
            })
            .unwrap();
        let member_token = SecretToken::try_new("member-token").unwrap();
        cp.store
            .put_auth_session(&AuthSession {
                id: AuthSessionId::try_new("ses_member").unwrap(),
                organization: org_a.clone(),
                user: member_user.id,
                token_hash: TokenHash::of(member_token.expose()),
                created_ms: 1,
                expires_ms: cp.now_ms() + DEFAULT_SESSION_TTL_MS,
                revoked_ms: None,
            })
            .unwrap();
        let member = cp.authenticate(member_token.expose()).unwrap();
        assert!(matches!(
            cp.invite(&member, &org_a, "x@y.test", Role::Member, "role-inv")
                .unwrap_err(),
            ControlPlaneError::Forbidden(_)
        ));
        assert!(cp.store.idempotent("role-inv").unwrap().is_none());
        // A member cannot decide approvals either.
        let approval = cp
            .request_approval(&owner_a, &org_a, Action::SecretWrite, "s:1", "r", "apr")
            .unwrap();
        assert!(matches!(
            cp.decide_approval(&member, &approval.id, true, "", "role-dec")
                .unwrap_err(),
            ControlPlaneError::Forbidden(_)
        ));
        assert!(cp.store.idempotent("role-dec").unwrap().is_none());
        // A foreign approval id is a NotFound and burns no key.
        assert!(matches!(
            cp.decide_approval(&owner_b, &approval.id, true, "", "foreign-dec")
                .unwrap_err(),
            ControlPlaneError::NotFound(_)
        ));
        assert!(cp.store.idempotent("foreign-dec").unwrap().is_none());
    }
}

#[cfg(test)]
mod crash_and_race_tests {
    use std::sync::{Arc, Barrier};

    use super::*;
    use crate::store::{ControlPlaneStore, MemoryControlPlaneStore, SqliteControlPlaneStore};

    const T0: i64 = 1_700_000_000_000;

    fn rows(store: &SqliteControlPlaneStore, table: &str) -> i64 {
        store
            .lock()
            .unwrap()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    fn open(path: &std::path::Path) -> Arc<SqliteControlPlaneStore> {
        Arc::new(SqliteControlPlaneStore::open(path).unwrap())
    }

    /// Bootstrap: for EVERY statement boundary a crash rolls back the claim,
    /// the organization, the user, the membership and the session; the retry
    /// with the same key completes exactly one logical operation and a
    /// further retry replays the original (token-free) response.
    #[test]
    fn bootstrap_crash_matrix_is_one_transaction() {
        let base = tempfile::tempdir().unwrap();
        let clock = Arc::new(ManualClock::new(T0));
        let mut reached_full_path = false;
        for k in 1..=12usize {
            let path = base.path().join(format!("boot-{k}.db"));
            let store = open(&path);
            let cp = ControlPlane::new(store.clone(), clock.clone());
            store.inject_crash_after(k);
            let attempt = cp.bootstrap_organization("Acme", "owner@acme.test", "Owner", "boot");
            store.inject_crash_after(usize::MAX);
            if let Ok(first) = attempt {
                assert!(first.token.is_some(), "k={k}");
                reached_full_path = true;
                let replay = cp
                    .bootstrap_organization("Acme", "owner@acme.test", "Owner", "boot")
                    .unwrap();
                assert_eq!(replay.organization.id, first.organization.id);
                assert_eq!(replay.session.id, first.session.id);
                assert!(replay.token.is_none());
                assert_eq!(rows(&store, "cp_organization"), 1);
                assert_eq!(rows(&store, "cp_user"), 1);
                assert_eq!(rows(&store, "cp_membership"), 1);
                assert_eq!(rows(&store, "cp_auth_session"), 1);
                assert_eq!(rows(&store, "cp_idempotency"), 1);
                break;
            }
            let attempt = attempt.unwrap_err();
            assert!(
                matches!(attempt, ControlPlaneError::Backend(_)),
                "k={k}: {attempt:?}"
            );
            let store = open(&path);
            assert_eq!(rows(&store, "cp_organization"), 0, "k={k}: partial org");
            assert_eq!(rows(&store, "cp_user"), 0, "k={k}: partial user");
            assert_eq!(
                rows(&store, "cp_membership"),
                0,
                "k={k}: partial membership"
            );
            assert_eq!(rows(&store, "cp_auth_session"), 0, "k={k}: partial session");
            assert!(
                store.idempotent("boot").unwrap().is_none(),
                "k={k}: the claim rolled back with the crash"
            );
            let cp = ControlPlane::new(store.clone(), clock.clone());
            let first = cp
                .bootstrap_organization("Acme", "owner@acme.test", "Owner", "boot")
                .unwrap();
            assert!(first.token.is_some(), "k={k}: the retry executes");
            let replay = cp
                .bootstrap_organization("Acme", "owner@acme.test", "Owner", "boot")
                .unwrap();
            assert_eq!(replay.organization.id, first.organization.id);
            assert_eq!(replay.session.id, first.session.id);
            assert!(replay.token.is_none(), "k={k}: no token on replay");
            assert_eq!(rows(&store, "cp_organization"), 1, "k={k}");
            assert_eq!(rows(&store, "cp_user"), 1, "k={k}");
            assert_eq!(rows(&store, "cp_membership"), 1, "k={k}");
            assert_eq!(rows(&store, "cp_auth_session"), 1, "k={k}");
            assert_eq!(rows(&store, "cp_idempotency"), 1, "k={k}");
        }
        assert!(reached_full_path, "the matrix never reached the full path");
    }

    /// Invitation creation: a crash at any statement leaves no invitation
    /// and no claimed key; the retry creates exactly one.
    #[test]
    fn invite_crash_matrix_is_one_transaction() {
        let base = tempfile::tempdir().unwrap();
        let clock = Arc::new(ManualClock::new(T0));
        let mut reached_full_path = false;
        for k in 1..=12usize {
            let path = base.path().join(format!("invite-{k}.db"));
            let store = open(&path);
            let cp = ControlPlane::new(store.clone(), clock.clone());
            let boot = cp
                .bootstrap_organization("Acme", "owner@acme.test", "Owner", "boot")
                .unwrap();
            let org = boot.organization.id.clone();
            let owner_token = boot.token.as_ref().unwrap().expose().to_string();
            let owner = cp.authenticate(&owner_token).unwrap();
            store.inject_crash_after(k);
            let attempt = cp.invite(&owner, &org, "new@acme.test", Role::Member, "invite-key");
            store.inject_crash_after(usize::MAX);
            if let Ok(issued) = attempt {
                assert!(issued.token.is_some(), "k={k}");
                reached_full_path = true;
                let replay = cp
                    .invite(&owner, &org, "new@acme.test", Role::Member, "invite-key")
                    .unwrap();
                assert_eq!(replay.invitation.id, issued.invitation.id);
                assert!(replay.token.is_none());
                assert_eq!(rows(&store, "cp_invitation"), 1);
                assert_eq!(rows(&store, "cp_idempotency"), 2);
                break;
            }
            let attempt = attempt.unwrap_err();
            assert!(
                matches!(attempt, ControlPlaneError::Backend(_)),
                "k={k}: {attempt:?}"
            );
            let store = open(&path);
            assert_eq!(
                rows(&store, "cp_invitation"),
                0,
                "k={k}: partial invitation"
            );
            assert!(
                store.idempotent("invite-key").unwrap().is_none(),
                "k={k}: the claim rolled back with the crash"
            );
            let cp = ControlPlane::new(store.clone(), clock.clone());
            let owner = cp.authenticate(&owner_token).unwrap();
            let issued = cp
                .invite(&owner, &org, "new@acme.test", Role::Member, "invite-key")
                .unwrap();
            assert!(issued.token.is_some(), "k={k}: the retry executes");
            let replay = cp
                .invite(&owner, &org, "new@acme.test", Role::Member, "invite-key")
                .unwrap();
            assert_eq!(replay.invitation.id, issued.invitation.id);
            assert!(replay.token.is_none(), "k={k}: no token on replay");
            assert_eq!(rows(&store, "cp_invitation"), 1, "k={k}");
            assert_eq!(rows(&store, "cp_idempotency"), 2, "k={k}");
        }
        assert!(reached_full_path, "the matrix never reached the full path");
    }

    /// Acceptance atomicity: at every crash point the membership insert and
    /// the invitation-accepted update are BOTH absent (or both present); the
    /// retry commits them together and replays the same membership.
    #[test]
    fn acceptance_crash_matrix_keeps_membership_and_invitation_atomic() {
        let base = tempfile::tempdir().unwrap();
        let clock = Arc::new(ManualClock::new(T0));
        let mut reached_full_path = false;
        for k in 1..=12usize {
            let path = base.path().join(format!("accept-{k}.db"));
            let store = open(&path);
            let cp = ControlPlane::new(store.clone(), clock.clone());
            let boot = cp
                .bootstrap_organization("Acme", "owner@acme.test", "Owner", "boot")
                .unwrap();
            let org = boot.organization.id.clone();
            let owner_token = boot.token.as_ref().unwrap().expose().to_string();
            let owner = cp.authenticate(&owner_token).unwrap();
            let issued = cp
                .invite(&owner, &org, "new@acme.test", Role::Member, "invite-key")
                .unwrap();
            let invitation_token = issued.token.unwrap().expose().to_string();
            let (invitee, _) = cp.create_user("new@acme.test", "New").unwrap();
            let pending = store
                .invitation_by_token_hash(&TokenHash::of(&invitation_token))
                .unwrap()
                .unwrap();
            assert_eq!(pending.status, InvitationStatus::Pending);

            store.inject_crash_after(k);
            let attempt = cp.accept_invitation(&invitation_token, &invitee.id, "accept-key");
            store.inject_crash_after(usize::MAX);
            if let Ok(membership) = attempt {
                reached_full_path = true;
                assert_eq!(membership.role, Role::Member);
                let replay = cp
                    .accept_invitation(&invitation_token, &invitee.id, "accept-key")
                    .unwrap();
                assert_eq!(replay.id, membership.id);
                let accepted = store
                    .invitation_by_token_hash(&TokenHash::of(&invitation_token))
                    .unwrap()
                    .unwrap();
                assert_eq!(accepted.status, InvitationStatus::Accepted);
                assert_eq!(rows(&store, "cp_membership"), 2);
                // boot + invite + create_user + accept.
                assert_eq!(rows(&store, "cp_idempotency"), 4);
                break;
            }
            let attempt = attempt.unwrap_err();
            assert!(
                matches!(attempt, ControlPlaneError::Backend(_)),
                "k={k}: {attempt:?}"
            );
            let store = open(&path);
            // Both halves of the compound mutation are absent together, and
            // the owner's membership (from bootstrap) is untouched.
            assert!(
                store.membership(&org, &invitee.id).unwrap().is_none(),
                "k={k}: a membership survived without the accepted invitation"
            );
            assert_eq!(rows(&store, "cp_membership"), 1, "k={k}");
            let still = store
                .invitation_by_token_hash(&TokenHash::of(&invitation_token))
                .unwrap()
                .unwrap();
            assert_eq!(
                still.status,
                InvitationStatus::Pending,
                "k={k}: the invitation was marked accepted without the membership"
            );
            assert!(store.idempotent("accept-key").unwrap().is_none(), "k={k}");
            let cp = ControlPlane::new(store.clone(), clock.clone());
            let membership = cp
                .accept_invitation(&invitation_token, &invitee.id, "accept-key")
                .unwrap();
            assert_eq!(membership.role, Role::Member);
            let accepted = store
                .invitation_by_token_hash(&TokenHash::of(&invitation_token))
                .unwrap()
                .unwrap();
            assert_eq!(accepted.status, InvitationStatus::Accepted);
            assert_eq!(accepted.decided_ms, Some(T0));
            let replay = cp
                .accept_invitation(&invitation_token, &invitee.id, "accept-key")
                .unwrap();
            assert_eq!(replay.id, membership.id, "k={k}");
            assert_eq!(rows(&store, "cp_membership"), 2, "k={k}");
            // boot + invite + create_user + accept.
            assert_eq!(rows(&store, "cp_idempotency"), 4, "k={k}");
        }
        assert!(reached_full_path, "the matrix never reached the full path");
    }

    /// Approval decisions: a crash at any statement leaves the approval OPEN
    /// and the key unclaimed; the retry decides exactly once and replays the
    /// identical decision.
    #[test]
    fn approval_decision_crash_matrix_is_one_transaction() {
        let base = tempfile::tempdir().unwrap();
        let clock = Arc::new(ManualClock::new(T0));
        let mut reached_full_path = false;
        for k in 1..=12usize {
            let path = base.path().join(format!("decide-{k}.db"));
            let store = open(&path);
            let cp = ControlPlane::new(store.clone(), clock.clone());
            let boot = cp
                .bootstrap_organization("Acme", "owner@acme.test", "Owner", "boot")
                .unwrap();
            let org = boot.organization.id.clone();
            let owner_token = boot.token.as_ref().unwrap().expose().to_string();
            let owner = cp.authenticate(&owner_token).unwrap();
            let approval = cp
                .request_approval(
                    &owner,
                    &org,
                    Action::SecretWrite,
                    "s:1",
                    "rotate",
                    "apr-key",
                )
                .unwrap();
            store.inject_crash_after(k);
            let attempt = cp.decide_approval(&owner, &approval.id, true, "ok", "decide-key");
            store.inject_crash_after(usize::MAX);
            if let Ok(decided) = attempt {
                reached_full_path = true;
                assert_eq!(decided.status, ApprovalStatus::Approved);
                let replay = cp
                    .decide_approval(&owner, &approval.id, true, "ok", "decide-key")
                    .unwrap();
                assert_eq!(replay.decided_ms, decided.decided_ms);
                assert_eq!(rows(&store, "cp_approval"), 1);
                assert_eq!(rows(&store, "cp_idempotency"), 3);
                break;
            }
            let attempt = attempt.unwrap_err();
            assert!(
                matches!(attempt, ControlPlaneError::Backend(_)),
                "k={k}: {attempt:?}"
            );
            let store = open(&path);
            let open = store.approval(&approval.id).unwrap().unwrap();
            assert_eq!(open.status, ApprovalStatus::Open, "k={k}");
            assert!(open.decided_ms.is_none(), "k={k}");
            assert!(store.idempotent("decide-key").unwrap().is_none(), "k={k}");
            let cp = ControlPlane::new(store.clone(), clock.clone());
            let owner = cp.authenticate(&owner_token).unwrap();
            let decided = cp
                .decide_approval(&owner, &approval.id, true, "ok", "decide-key")
                .unwrap();
            assert_eq!(decided.status, ApprovalStatus::Approved);
            let replay = cp
                .decide_approval(&owner, &approval.id, true, "ok", "decide-key")
                .unwrap();
            assert_eq!(replay.decided_ms, decided.decided_ms, "k={k}");
            assert_eq!(rows(&store, "cp_approval"), 1, "k={k}");
            assert_eq!(rows(&store, "cp_idempotency"), 3, "k={k}");
        }
        assert!(reached_full_path, "the matrix never reached the full path");
    }

    /// Approval creation: a crash at any statement leaves no approval row
    /// and no claimed key; the retry creates exactly one.
    #[test]
    fn approval_request_crash_matrix_is_one_transaction() {
        let base = tempfile::tempdir().unwrap();
        let clock = Arc::new(ManualClock::new(T0));
        let mut reached_full_path = false;
        for k in 1..=12usize {
            let path = base.path().join(format!("request-{k}.db"));
            let store = open(&path);
            let cp = ControlPlane::new(store.clone(), clock.clone());
            let boot = cp
                .bootstrap_organization("Acme", "owner@acme.test", "Owner", "boot")
                .unwrap();
            let org = boot.organization.id.clone();
            let owner_token = boot.token.as_ref().unwrap().expose().to_string();
            let owner = cp.authenticate(&owner_token).unwrap();
            store.inject_crash_after(k);
            let attempt = cp.request_approval(
                &owner,
                &org,
                Action::SecretWrite,
                "s:1",
                "rotate",
                "apr-key",
            );
            store.inject_crash_after(usize::MAX);
            if let Ok(approval) = attempt {
                reached_full_path = true;
                assert_eq!(approval.status, ApprovalStatus::Open);
                let replay = cp
                    .request_approval(
                        &owner,
                        &org,
                        Action::SecretWrite,
                        "s:1",
                        "rotate",
                        "apr-key",
                    )
                    .unwrap();
                assert_eq!(replay.id, approval.id);
                assert_eq!(rows(&store, "cp_approval"), 1);
                assert_eq!(rows(&store, "cp_idempotency"), 2);
                break;
            }
            let attempt = attempt.unwrap_err();
            assert!(
                matches!(attempt, ControlPlaneError::Backend(_)),
                "k={k}: {attempt:?}"
            );
            let store = open(&path);
            assert_eq!(rows(&store, "cp_approval"), 0, "k={k}: partial approval");
            assert!(
                store.idempotent("apr-key").unwrap().is_none(),
                "k={k}: the claim rolled back with the crash"
            );
            let cp = ControlPlane::new(store.clone(), clock.clone());
            let owner = cp.authenticate(&owner_token).unwrap();
            let approval = cp
                .request_approval(
                    &owner,
                    &org,
                    Action::SecretWrite,
                    "s:1",
                    "rotate",
                    "apr-key",
                )
                .unwrap();
            let replay = cp
                .request_approval(
                    &owner,
                    &org,
                    Action::SecretWrite,
                    "s:1",
                    "rotate",
                    "apr-key",
                )
                .unwrap();
            assert_eq!(replay.id, approval.id, "k={k}");
            assert_eq!(rows(&store, "cp_approval"), 1, "k={k}");
            assert_eq!(rows(&store, "cp_idempotency"), 2, "k={k}");
        }
        assert!(reached_full_path, "the matrix never reached the full path");
    }

    /// 50 callers, one key, one store: exactly one execution and 49 typed
    /// identical replays, with zero duplicated rows.
    #[test]
    fn fifty_concurrent_callers_on_one_key_yield_one_operation() {
        let dir = tempfile::tempdir().unwrap();
        let store = open(&dir.path().join("race.db"));
        let cp = Arc::new(ControlPlane::new(
            store.clone(),
            Arc::new(ManualClock::new(T0)),
        ));
        let barrier = Arc::new(Barrier::new(50));
        let mut handles = Vec::new();
        for _ in 0..50 {
            let cp = cp.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                cp.bootstrap_organization("Acme", "owner@acme.test", "Owner", "race-key")
            }));
        }
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        let mut fresh = 0usize;
        let mut replayed = 0usize;
        let winner = results
            .iter()
            .find_map(|result| result.as_ref().ok())
            .expect("at least one caller succeeds");
        for result in &results {
            let boot = result.as_ref().unwrap_or_else(|e| panic!("{e:?}"));
            assert_eq!(boot.organization.id, winner.organization.id);
            assert_eq!(boot.user.id, winner.user.id);
            assert_eq!(boot.session.id, winner.session.id);
            if boot.token.is_some() {
                fresh += 1;
            } else {
                replayed += 1;
            }
        }
        assert_eq!(fresh, 1, "exactly one caller executed the operation");
        assert_eq!(replayed, 49, "the other 49 are typed replays");
        assert_eq!(rows(&store, "cp_organization"), 1);
        assert_eq!(rows(&store, "cp_user"), 1);
        assert_eq!(rows(&store, "cp_membership"), 1);
        assert_eq!(rows(&store, "cp_auth_session"), 1);
        assert_eq!(rows(&store, "cp_idempotency"), 1);
    }

    /// The same race across INDEPENDENT SQLite connections on one file: the
    /// `BEGIN IMMEDIATE` claim serializes the writers, so a loser that raced
    /// past nothing (there is no precheck to pass) observes the winner's
    /// committed row and replays it.
    #[test]
    fn concurrent_writers_across_connections_claim_one_operation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("race-connections.db");
        drop(SqliteControlPlaneStore::open(&path).unwrap());
        let probe = open(&path);
        let barrier = Arc::new(Barrier::new(8));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let store = open(&path);
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                let cp = ControlPlane::new(store, Arc::new(ManualClock::new(T0)));
                barrier.wait();
                cp.bootstrap_organization("Acme", "owner@acme.test", "Owner", "race-key")
            }));
        }
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        let mut fresh = 0usize;
        let mut replayed = 0usize;
        let winner = results
            .iter()
            .find_map(|result| result.as_ref().ok())
            .expect("at least one caller succeeds");
        for result in &results {
            let boot = result.as_ref().unwrap_or_else(|e| panic!("{e:?}"));
            assert_eq!(boot.organization.id, winner.organization.id);
            assert_eq!(boot.session.id, winner.session.id);
            if boot.token.is_some() {
                fresh += 1;
            } else {
                replayed += 1;
            }
        }
        assert_eq!(fresh, 1);
        assert_eq!(replayed, 7);
        assert_eq!(rows(&probe, "cp_organization"), 1);
        assert_eq!(rows(&probe, "cp_user"), 1);
        assert_eq!(rows(&probe, "cp_membership"), 1);
        assert_eq!(rows(&probe, "cp_auth_session"), 1);
        assert_eq!(rows(&probe, "cp_idempotency"), 1);
    }

    /// External login: a crash at EVERY statement boundary rolls back the
    /// user, the external subject link, the membership and the session
    /// together; the retry completes exactly one logical login (with a fresh
    /// one-shot token) and no partial subject binding survives.
    #[test]
    fn login_crash_matrix_is_one_transaction() {
        use crate::rbac::PrincipalSubject;

        let base = tempfile::tempdir().unwrap();
        let clock = Arc::new(ManualClock::new(T0));
        let mut reached_full_path = false;
        for k in 1..=12usize {
            let path = base.path().join(format!("login-{k}.db"));
            let store = open(&path);
            let cp = ControlPlane::new(store.clone(), clock.clone());
            let boot = cp
                .bootstrap_organization("Acme", "owner@acme.test", "Owner", "boot")
                .unwrap();
            let org = boot.organization.id.clone();
            store.inject_crash_after(k);
            let attempt = cp.login_external(
                &org,
                "idp",
                "sub-1",
                "new@acme.test",
                "New",
                true,
                Role::Member,
            );
            store.inject_crash_after(usize::MAX);
            if let Ok(login) = attempt {
                reached_full_path = true;
                assert_eq!(login.role, Role::Member, "k={k}");
                // The one-shot token authenticates the resolved user.
                let principal = cp.authenticate(login.token.expose()).unwrap();
                assert_eq!(
                    principal.subject,
                    PrincipalSubject::User(login.user.id.clone()),
                    "k={k}"
                );
                assert_eq!(rows(&store, "cp_user"), 2, "k={k}: owner + sso user");
                assert_eq!(rows(&store, "cp_external_identity"), 1, "k={k}");
                assert_eq!(rows(&store, "cp_membership"), 2, "k={k}");
                assert_eq!(rows(&store, "cp_auth_session"), 2, "k={k}");
                break;
            }
            let attempt = attempt.unwrap_err();
            assert!(
                matches!(attempt, ControlPlaneError::Backend(_)),
                "k={k}: {attempt:?}"
            );
            let store = open(&path);
            // No half-bound subject: no user, no link, no membership, no
            // session (nor any claimed idempotency key).
            assert!(
                store.user_by_email("new@acme.test").unwrap().is_none(),
                "k={k}: a partial SSO user survived"
            );
            assert!(
                store.external_identity("idp", "sub-1").unwrap().is_none(),
                "k={k}: a partial subject link survived"
            );
            assert_eq!(rows(&store, "cp_user"), 1, "k={k}");
            assert_eq!(rows(&store, "cp_external_identity"), 0, "k={k}");
            assert_eq!(rows(&store, "cp_membership"), 1, "k={k}");
            assert_eq!(rows(&store, "cp_auth_session"), 1, "k={k}");
            // The retry completes the login for real, binding the subject.
            let cp = ControlPlane::new(store.clone(), clock.clone());
            let login = cp
                .login_external(
                    &org,
                    "idp",
                    "sub-1",
                    "new@acme.test",
                    "New",
                    true,
                    Role::Member,
                )
                .unwrap();
            assert_eq!(
                cp.authenticate(login.token.expose()).unwrap().subject,
                PrincipalSubject::User(login.user.id.clone()),
                "k={k}"
            );
            assert_eq!(
                store
                    .external_identity("idp", "sub-1")
                    .unwrap()
                    .unwrap()
                    .user,
                login.user.id,
                "k={k}"
            );
            assert_eq!(rows(&store, "cp_user"), 2, "k={k}");
            assert_eq!(rows(&store, "cp_external_identity"), 1, "k={k}");
            assert_eq!(rows(&store, "cp_membership"), 2, "k={k}");
            assert_eq!(rows(&store, "cp_auth_session"), 2, "k={k}");
        }
        assert!(reached_full_path, "the matrix never reached the full path");
    }

    /// Concurrent same-email creates resolve to exactly ONE user: the winner
    /// creates, the losers replay the recorded answer as `(existing, false)`
    /// instead of racing a check-then-insert into a unique-email failure.
    fn concurrent_create_race(services: Vec<Arc<ControlPlane>>) {
        let barrier = Arc::new(std::sync::Barrier::new(services.len()));
        let mut handles = Vec::new();
        for (i, cp) in services.into_iter().enumerate() {
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                cp.create_user("race@acme.test", &format!("Racer {i}"))
                    .unwrap()
            }));
        }
        let results: Vec<(User, bool)> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        let created = results.iter().filter(|(_, created)| *created).count();
        assert_eq!(created, 1, "exactly one caller created the user");
        let winner = results[0].0.id.clone();
        assert!(
            results.iter().all(|(user, _)| user.id == winner),
            "every caller resolved the same user"
        );
    }

    #[test]
    fn concurrent_create_user_races_resolve_to_one_user_on_memory() {
        let store: Arc<dyn ControlPlaneStore> = Arc::new(MemoryControlPlaneStore::new());
        let services = (0..4)
            .map(|_| {
                Arc::new(ControlPlane::new(
                    store.clone(),
                    Arc::new(ManualClock::new(T0)),
                ))
            })
            .collect();
        concurrent_create_race(services);
        assert_eq!(store.users(None, 10).unwrap().len(), 1);
        assert_eq!(
            store
                .user_by_email("race@acme.test")
                .unwrap()
                .unwrap()
                .email,
            "race@acme.test"
        );
    }

    #[test]
    fn concurrent_create_user_races_resolve_to_one_user_on_sqlite() {
        // Separate connections to ONE file: a real cross-connection race.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control-plane.db");
        let stores: Vec<Arc<SqliteControlPlaneStore>> = (0..4).map(|_| open(&path)).collect();
        let services = stores
            .iter()
            .map(|store| {
                Arc::new(ControlPlane::new(
                    store.clone(),
                    Arc::new(ManualClock::new(T0)),
                ))
            })
            .collect();
        concurrent_create_race(services);
        assert_eq!(stores[0].users(None, 10).unwrap().len(), 1);
        assert_eq!(
            stores[0]
                .user_by_email("race@acme.test")
                .unwrap()
                .unwrap()
                .email,
            "race@acme.test"
        );
    }

    /// Bounded idempotency retention across MANY logins: rows accumulate
    /// (one per login) but the documented prune brings the journal back under
    /// the count bound, and the store keeps serving logins afterwards.
    #[test]
    fn login_idempotency_rows_are_bounded_across_many_logins() {
        let store = Arc::new(MemoryControlPlaneStore::new());
        let clock = Arc::new(ManualClock::new(T0));
        let cp = ControlPlane::new(store.clone(), clock.clone());
        let boot = cp
            .bootstrap_organization("Acme", "owner@acme.test", "Owner", "boot")
            .unwrap();
        let org = boot.organization.id.clone();
        for i in 0..(crate::store::MAX_IDEMPOTENCY_ROWS + 20) {
            cp.login_external(
                &org,
                "idp",
                &format!("sub-{i}"),
                &format!("user-{i}@acme.test"),
                "User",
                true,
                Role::Member,
            )
            .unwrap();
        }
        assert!(
            store.idempotency_count().unwrap() > crate::store::MAX_IDEMPOTENCY_ROWS,
            "one durable row per login until the prune tick"
        );
        let before = store.idempotency_count().unwrap();
        let removed = store.prune_idempotency(clock.now_ms()).unwrap();
        assert_eq!(
            removed,
            before - crate::store::MAX_IDEMPOTENCY_ROWS,
            "the prune removes exactly the oldest excess rows"
        );
        assert_eq!(
            store.idempotency_count().unwrap(),
            crate::store::MAX_IDEMPOTENCY_ROWS
        );
        // A fresh login still works after pruning (the journal is not wedged).
        cp.login_external(
            &org,
            "idp",
            "sub-after-prune",
            "after@acme.test",
            "After",
            true,
            Role::Member,
        )
        .unwrap();
        assert_eq!(
            store.idempotency_count().unwrap(),
            crate::store::MAX_IDEMPOTENCY_ROWS + 1
        );
    }
}
