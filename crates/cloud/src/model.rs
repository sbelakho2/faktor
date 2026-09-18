//! The control-plane domain model: users, organizations, memberships,
//! invitations, auth sessions, external identities, service accounts and
//! approvals.
//!
//! Every organization-scoped resource carries its [`OrganizationId`]
//! explicitly — tenant identity is structural, never inferred from a
//! request, so a store query can always be scoped and a listing can never
//! widen across tenants.

use serde::{Deserialize, Serialize};

use crate::error::ControlPlaneError;
use crate::ids::{
    ApprovalId, AuthSessionId, ExternalIdentityId, InvitationId, MembershipId, OrganizationId,
    ServiceAccountId, TokenHash, UserId,
};
use crate::rbac::{Action, Role};

/// Bound on one email address.
pub const MAX_EMAIL_BYTES: usize = 320;
/// Bound on one organization name.
pub const MAX_ORG_NAME_BYTES: usize = 128;
/// Bound on one display name.
pub const MAX_DISPLAY_NAME_BYTES: usize = 128;
/// Bound on one reason/note text.
pub const MAX_NOTE_BYTES: usize = 1024;
/// Default invitation lifetime (7 days).
pub const DEFAULT_INVITATION_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;
/// Default auth-session lifetime (30 days).
pub const DEFAULT_SESSION_TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// Normalize + validate one email (lowercase, bounded, exactly one `@` with
/// non-empty local/domain parts, no whitespace).
pub fn normalize_email(email: &str) -> Result<String, ControlPlaneError> {
    let email = email.trim().to_ascii_lowercase();
    if email.is_empty() || email.len() > MAX_EMAIL_BYTES {
        return Err(ControlPlaneError::Malformed(format!(
            "email must be 1..={MAX_EMAIL_BYTES} bytes"
        )));
    }
    let mut parts = email.split('@');
    let (local, domain) = match (parts.next(), parts.next(), parts.next()) {
        (Some(local), Some(domain), None) => (local, domain),
        _ => {
            return Err(ControlPlaneError::Malformed(
                "email must contain exactly one '@'".into(),
            ));
        }
    };
    if local.is_empty() || domain.is_empty() || !domain.contains('.') {
        return Err(ControlPlaneError::Malformed(
            "email must have a local part and a dotted domain".into(),
        ));
    }
    if email.contains(char::is_whitespace) {
        return Err(ControlPlaneError::Malformed(
            "email must not contain whitespace".into(),
        ));
    }
    Ok(email)
}

/// One human identity. Users are global (an identity can be a member of
/// several organizations); membership carries the tenant link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct User {
    pub id: UserId,
    pub email: String,
    pub display_name: String,
    pub created_ms: i64,
    pub disabled: bool,
}

/// One organization (tenant).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Organization {
    pub id: OrganizationId,
    pub name: String,
    pub created_ms: i64,
    pub deleted: bool,
}

/// One membership: the (organization, user) link carrying the role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Membership {
    pub id: MembershipId,
    pub organization: OrganizationId,
    pub user: UserId,
    pub role: Role,
    pub created_ms: i64,
}

/// The durable lifecycle state of an invitation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvitationStatus {
    Pending,
    Accepted,
    Revoked,
    Expired,
}

impl InvitationStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            InvitationStatus::Pending => "pending",
            InvitationStatus::Accepted => "accepted",
            InvitationStatus::Revoked => "revoked",
            InvitationStatus::Expired => "expired",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "pending" => InvitationStatus::Pending,
            "accepted" => InvitationStatus::Accepted,
            "revoked" => InvitationStatus::Revoked,
            "expired" => InvitationStatus::Expired,
            _ => return None,
        })
    }
}

/// One invitation. The plaintext token exists only in the issuance
/// response; the row stores its hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Invitation {
    pub id: InvitationId,
    pub organization: OrganizationId,
    pub email: String,
    pub role: Role,
    pub status: InvitationStatus,
    pub invited_by: UserId,
    pub token_hash: TokenHash,
    pub created_ms: i64,
    pub expires_ms: i64,
    pub decided_ms: Option<i64>,
}

impl Invitation {
    /// The status observed at `now_ms` (an un-decided invitation older than
    /// its expiry is Expired, never silently Pending).
    pub fn status_at(&self, now_ms: i64) -> InvitationStatus {
        match self.status {
            InvitationStatus::Pending if now_ms > self.expires_ms => InvitationStatus::Expired,
            other => other,
        }
    }
}

/// One auth session: a user acting inside exactly ONE organization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthSession {
    pub id: AuthSessionId,
    pub organization: OrganizationId,
    pub user: UserId,
    pub token_hash: TokenHash,
    pub created_ms: i64,
    pub expires_ms: i64,
    pub revoked_ms: Option<i64>,
}

impl AuthSession {
    pub fn is_valid_at(&self, now_ms: i64) -> bool {
        self.revoked_ms.is_none() && now_ms <= self.expires_ms
    }
}

/// One linked external identity (SSO subject → user).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalIdentity {
    pub id: ExternalIdentityId,
    pub user: UserId,
    pub provider: String,
    pub subject: String,
    pub created_ms: i64,
}

/// One service account: a machine principal inside ONE organization with a
/// role (never Owner) AND an explicit action scope (the intersection is
/// enforced by `authorize`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceAccount {
    pub id: ServiceAccountId,
    pub organization: OrganizationId,
    pub name: String,
    pub role: Role,
    pub scopes: Vec<Action>,
    pub token_hash: TokenHash,
    pub created_ms: i64,
    pub disabled: bool,
}

impl ServiceAccount {
    /// Validate the account's shape: a bounded name, a non-owner role and a
    /// non-empty, deduplicated, sorted scope set.
    pub fn validate(&self) -> Result<(), ControlPlaneError> {
        if self.name.is_empty() || self.name.len() > MAX_DISPLAY_NAME_BYTES {
            return Err(ControlPlaneError::Malformed(format!(
                "service account name must be 1..={MAX_DISPLAY_NAME_BYTES} bytes"
            )));
        }
        if self.role == Role::Owner {
            return Err(ControlPlaneError::Malformed(
                "a service account cannot hold the owner role".into(),
            ));
        }
        if self.scopes.is_empty() {
            return Err(ControlPlaneError::Malformed(
                "a service account must carry at least one explicit scope".into(),
            ));
        }
        let mut sorted = self.scopes.clone();
        sorted.sort();
        sorted.dedup();
        if sorted != self.scopes {
            return Err(ControlPlaneError::Malformed(
                "service account scopes must be sorted and unique".into(),
            ));
        }
        Ok(())
    }
}

/// The lifecycle state of one approval request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Open,
    Approved,
    Rejected,
}

impl ApprovalStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            ApprovalStatus::Open => "open",
            ApprovalStatus::Approved => "approved",
            ApprovalStatus::Rejected => "rejected",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "open" => ApprovalStatus::Open,
            "approved" => ApprovalStatus::Approved,
            "rejected" => ApprovalStatus::Rejected,
            _ => return None,
        })
    }
}

/// One approval request: a durable ask to perform `action` on a resource,
/// decided by a principal with [`Action::ApprovalDecide`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRequest {
    pub id: ApprovalId,
    pub organization: OrganizationId,
    pub action: Action,
    pub resource: String,
    pub requested_by: UserId,
    pub reason: String,
    pub status: ApprovalStatus,
    pub decided_by: Option<UserId>,
    pub note: Option<String>,
    pub created_ms: i64,
    pub decided_ms: Option<i64>,
}

/// One page of durable rows with an opaque cursor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emails_are_normalized_and_strictly_validated() {
        assert_eq!(
            normalize_email("  Admin@Example.TEST ").unwrap(),
            "admin@example.test"
        );
        for bad in [
            "",
            "no-at",
            "a@b",
            "@b.c",
            "a@",
            "a b@c.d",
            "a@@b.c",
            "x".repeat(400).as_str(),
        ] {
            assert!(normalize_email(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn invitation_expiry_is_derived_not_stored_stale() {
        let invitation = Invitation {
            id: InvitationId::try_new("inv_1").unwrap(),
            organization: OrganizationId::try_new("org_1").unwrap(),
            email: "a@b.c".into(),
            role: Role::Member,
            status: InvitationStatus::Pending,
            invited_by: UserId::try_new("usr_1").unwrap(),
            token_hash: TokenHash::try_new("a".repeat(64)).unwrap(),
            created_ms: 0,
            expires_ms: 100,
            decided_ms: None,
        };
        assert_eq!(invitation.status_at(50), InvitationStatus::Pending);
        assert_eq!(invitation.status_at(101), InvitationStatus::Expired);
        let mut accepted = invitation.clone();
        accepted.status = InvitationStatus::Accepted;
        assert_eq!(accepted.status_at(1_000), InvitationStatus::Accepted);
    }

    #[test]
    fn service_accounts_refuse_owner_and_unscoped_shapes() {
        let base = ServiceAccount {
            id: ServiceAccountId::try_new("sa_1").unwrap(),
            organization: OrganizationId::try_new("org_1").unwrap(),
            name: "ci".into(),
            role: Role::Member,
            scopes: vec![Action::RepositoryRead],
            token_hash: TokenHash::try_new("b".repeat(64)).unwrap(),
            created_ms: 0,
            disabled: false,
        };
        assert!(base.validate().is_ok());
        let mut owner = base.clone();
        owner.role = Role::Owner;
        assert!(owner.validate().is_err());
        let mut empty = base.clone();
        empty.scopes = vec![];
        assert!(empty.validate().is_err());
        let mut unsorted = base.clone();
        unsorted.scopes = vec![Action::RepositoryWrite, Action::RepositoryRead];
        assert!(unsorted.validate().is_err());
        let mut dupes = base;
        dupes.scopes = vec![Action::RepositoryRead, Action::RepositoryRead];
        assert!(dupes.validate().is_err());
    }

    #[test]
    fn statuses_roundtrip_through_their_stable_names() {
        for status in [
            InvitationStatus::Pending,
            InvitationStatus::Accepted,
            InvitationStatus::Revoked,
            InvitationStatus::Expired,
        ] {
            assert_eq!(InvitationStatus::parse(status.as_str()), Some(status));
        }
        for status in [
            ApprovalStatus::Open,
            ApprovalStatus::Approved,
            ApprovalStatus::Rejected,
        ] {
            assert_eq!(ApprovalStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(InvitationStatus::parse("bogus"), None);
    }
}
