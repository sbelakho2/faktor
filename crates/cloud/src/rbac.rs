//! The central authorization function and the role/permission matrix.
//!
//! One entry point — [`authorize`] — decides every control-plane action.
//! Rules, in order:
//!
//! 1. The principal's organization MUST equal the resource's organization.
//!    A mismatch is [`Denied::NotFound`] (deliberately indistinguishable
//!    from a nonexistent row: a foreign tenant learns nothing, not even
//!    whether the id exists);
//! 2. the action MUST belong to the named resource (a programming error is
//!    a typed refusal, never a silent allow);
//! 3. the principal's role MUST meet the action's minimum role
//!    ([`Action::minimum_role`], the role matrix below);
//! 4. a service-account principal must ALSO carry the action in its
//!    explicit scope (the effective permission is role ∩ scope).
//!
//! Default deny: an action with no matrix entry would be refused; the
//! matrix is total over [`Action`] and locked by a test that enumerates
//! every `(role, action)` pair.
//!
//! | action                | viewer | member | admin | owner |
//! |-----------------------|--------|--------|-------|-------|
//! | organization.read     |   x    |   x    |   x   |   x   |
//! | member.read           |   x    |   x    |   x   |   x   |
//! | repository.read       |   x    |   x    |   x   |   x   |
//! | provider_policy.read  |   x    |   x    |   x   |   x   |
//! | billing.read          |   x    |   x    |   x   |   x   |
//! | repository.write      |        |   x    |   x   |   x   |
//! | run.create            |        |   x    |   x   |   x   |
//! | provider_policy.write |        |        |   x   |   x   |
//! | secret.read           |        |   x    |   x   |   x   |
//! | secret.write          |        |        |   x   |   x   |
//! | approval.request      |        |   x    |   x   |   x   |
//! | approval.decide       |        |        |   x   |   x   |
//! | worker.manage         |        |        |   x   |   x   |
//! | member.write          |        |        |   x   |   x   |
//! | organization.update   |        |        |   x   |   x   |
//! | organization.delete   |        |        |       |   x   |
//! | billing.write         |        |        |       |   x   |

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::ids::{OrganizationId, ServiceAccountId, UserId};

/// One role in an organization, ordered: Owner > Admin > Member > Viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Owner,
    Admin,
    Member,
    Viewer,
}

impl Role {
    pub const fn as_str(self) -> &'static str {
        match self {
            Role::Owner => "owner",
            Role::Admin => "admin",
            Role::Member => "member",
            Role::Viewer => "viewer",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "owner" => Role::Owner,
            "admin" => Role::Admin,
            "member" => Role::Member,
            "viewer" => Role::Viewer,
            _ => return None,
        })
    }

    /// The ordering rank (Owner highest).
    pub const fn rank(self) -> u8 {
        match self {
            Role::Owner => 3,
            Role::Admin => 2,
            Role::Member => 1,
            Role::Viewer => 0,
        }
    }
}

/// One permission dimension of the control plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    OrganizationRead,
    OrganizationUpdate,
    OrganizationDelete,
    MemberRead,
    MemberWrite,
    RepositoryRead,
    RepositoryWrite,
    RunCreate,
    ProviderPolicyRead,
    ProviderPolicyWrite,
    SecretRead,
    SecretWrite,
    ApprovalRequest,
    ApprovalDecide,
    BillingRead,
    BillingWrite,
    WorkerManage,
}

/// The full action list, in stable order (used by the role-matrix test and
/// by the identity endpoint's advertised actions).
pub const ALL_ACTIONS: &[Action] = &[
    Action::OrganizationRead,
    Action::OrganizationUpdate,
    Action::OrganizationDelete,
    Action::MemberRead,
    Action::MemberWrite,
    Action::RepositoryRead,
    Action::RepositoryWrite,
    Action::RunCreate,
    Action::ProviderPolicyRead,
    Action::ProviderPolicyWrite,
    Action::SecretRead,
    Action::SecretWrite,
    Action::ApprovalRequest,
    Action::ApprovalDecide,
    Action::BillingRead,
    Action::BillingWrite,
    Action::WorkerManage,
];

impl Action {
    pub const fn as_str(self) -> &'static str {
        match self {
            Action::OrganizationRead => "organization_read",
            Action::OrganizationUpdate => "organization_update",
            Action::OrganizationDelete => "organization_delete",
            Action::MemberRead => "member_read",
            Action::MemberWrite => "member_write",
            Action::RepositoryRead => "repository_read",
            Action::RepositoryWrite => "repository_write",
            Action::RunCreate => "run_create",
            Action::ProviderPolicyRead => "provider_policy_read",
            Action::ProviderPolicyWrite => "provider_policy_write",
            Action::SecretRead => "secret_read",
            Action::SecretWrite => "secret_write",
            Action::ApprovalRequest => "approval_request",
            Action::ApprovalDecide => "approval_decide",
            Action::BillingRead => "billing_read",
            Action::BillingWrite => "billing_write",
            Action::WorkerManage => "worker_manage",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        ALL_ACTIONS.iter().copied().find(|a| a.as_str() == raw)
    }

    /// The resource this action belongs to.
    pub const fn resource(self) -> Resource {
        match self {
            Action::OrganizationRead | Action::OrganizationUpdate | Action::OrganizationDelete => {
                Resource::Organization
            }
            Action::MemberRead | Action::MemberWrite => Resource::Member,
            Action::RepositoryRead | Action::RepositoryWrite => Resource::Repository,
            Action::RunCreate => Resource::Run,
            Action::ProviderPolicyRead | Action::ProviderPolicyWrite => Resource::ProviderPolicy,
            Action::SecretRead | Action::SecretWrite => Resource::Secret,
            Action::ApprovalRequest | Action::ApprovalDecide => Resource::Approval,
            Action::BillingRead | Action::BillingWrite => Resource::Billing,
            Action::WorkerManage => Resource::Worker,
        }
    }

    /// The minimum role required — THE role matrix (total over `Action`).
    pub const fn minimum_role(self) -> Role {
        match self {
            Action::OrganizationRead
            | Action::MemberRead
            | Action::RepositoryRead
            | Action::ProviderPolicyRead
            | Action::BillingRead => Role::Viewer,
            Action::RepositoryWrite
            | Action::RunCreate
            | Action::SecretRead
            | Action::ApprovalRequest => Role::Member,
            Action::OrganizationUpdate
            | Action::MemberWrite
            | Action::ProviderPolicyWrite
            | Action::SecretWrite
            | Action::ApprovalDecide
            | Action::WorkerManage => Role::Admin,
            Action::OrganizationDelete | Action::BillingWrite => Role::Owner,
        }
    }
}

/// One control-plane resource kind (every action names exactly one).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resource {
    Organization,
    Member,
    Repository,
    Run,
    ProviderPolicy,
    Secret,
    Approval,
    Billing,
    Worker,
}

impl Resource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Resource::Organization => "organization",
            Resource::Member => "member",
            Resource::Repository => "repository",
            Resource::Run => "run",
            Resource::ProviderPolicy => "provider_policy",
            Resource::Secret => "secret",
            Resource::Approval => "approval",
            Resource::Billing => "billing",
            Resource::Worker => "worker",
        }
    }
}

/// The authenticated subject behind a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrincipalSubject {
    User(UserId),
    ServiceAccount(ServiceAccountId),
}

impl PrincipalSubject {
    pub const fn as_str(&self) -> &'static str {
        match self {
            PrincipalSubject::User(_) => "user",
            PrincipalSubject::ServiceAccount(_) => "service_account",
        }
    }

    pub fn id(&self) -> &str {
        match self {
            PrincipalSubject::User(id) => id.as_str(),
            PrincipalSubject::ServiceAccount(id) => id.as_str(),
        }
    }
}

/// One authenticated principal, bound to exactly one organization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub subject: PrincipalSubject,
    pub organization: OrganizationId,
    pub role: Role,
    /// `Some` for service accounts: the explicit action scope (the
    /// effective permission is role ∩ scope). `None` for users.
    pub scopes: Option<BTreeSet<Action>>,
}

impl Principal {
    pub fn user(id: UserId, organization: OrganizationId, role: Role) -> Self {
        Self {
            subject: PrincipalSubject::User(id),
            organization,
            role,
            scopes: None,
        }
    }

    pub fn service_account(
        id: ServiceAccountId,
        organization: OrganizationId,
        role: Role,
        scopes: impl IntoIterator<Item = Action>,
    ) -> Self {
        Self {
            subject: PrincipalSubject::ServiceAccount(id),
            organization,
            role,
            scopes: Some(scopes.into_iter().collect()),
        }
    }

    pub fn is_service_account(&self) -> bool {
        matches!(self.subject, PrincipalSubject::ServiceAccount(_))
    }

    /// Every action the principal may perform (role ∩ scope).
    pub fn effective_actions(&self) -> Vec<Action> {
        ALL_ACTIONS
            .iter()
            .copied()
            .filter(|action| self.may(*action).is_ok())
            .collect()
    }

    fn may(&self, action: Action) -> Result<(), Denied> {
        if self.role.rank() < action.minimum_role().rank() {
            return Err(Denied::Forbidden {
                message: format!(
                    "role {} cannot perform {}",
                    self.role.as_str(),
                    action.as_str()
                ),
            });
        }
        if let Some(scopes) = &self.scopes {
            if !scopes.contains(&action) {
                return Err(Denied::Forbidden {
                    message: format!("service account scope does not include {}", action.as_str()),
                });
            }
        }
        Ok(())
    }
}

/// A typed authorization refusal. `NotFound` is the tenant-isolation answer
/// and carries no existence information.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Denied {
    #[error("not found: {message}")]
    NotFound { message: String },
    #[error("permission denied: {message}")]
    Forbidden { message: String },
    #[error("authorization request is malformed: {message}")]
    Malformed { message: String },
}

/// THE central authorization function. See the module docs for the rule
/// order; default deny.
pub fn authorize(
    principal: &Principal,
    organization: &OrganizationId,
    resource: Resource,
    action: Action,
) -> Result<(), Denied> {
    if principal.organization != *organization {
        // Tenant isolation: the caller learns nothing about the foreign
        // organization, not even whether it exists.
        return Err(Denied::NotFound {
            message: "organization not found".into(),
        });
    }
    if action.resource() != resource {
        return Err(Denied::Malformed {
            message: format!(
                "action {} does not belong to resource {}",
                action.as_str(),
                resource.as_str()
            ),
        });
    }
    principal.may(action)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn principal(role: Role) -> Principal {
        Principal::user(
            UserId::try_new("usr_1").unwrap(),
            OrganizationId::try_new("org_1").unwrap(),
            role,
        )
    }

    fn org() -> OrganizationId {
        OrganizationId::try_new("org_1").unwrap()
    }

    #[test]
    fn role_matrix_rows_are_exact() {
        // (action, minimum role) — the locked matrix, one row per action.
        let rows: &[(Action, Role)] = &[
            (Action::OrganizationRead, Role::Viewer),
            (Action::OrganizationUpdate, Role::Admin),
            (Action::OrganizationDelete, Role::Owner),
            (Action::MemberRead, Role::Viewer),
            (Action::MemberWrite, Role::Admin),
            (Action::RepositoryRead, Role::Viewer),
            (Action::RepositoryWrite, Role::Member),
            (Action::RunCreate, Role::Member),
            (Action::ProviderPolicyRead, Role::Viewer),
            (Action::ProviderPolicyWrite, Role::Admin),
            (Action::SecretRead, Role::Member),
            (Action::SecretWrite, Role::Admin),
            (Action::ApprovalRequest, Role::Member),
            (Action::ApprovalDecide, Role::Admin),
            (Action::BillingRead, Role::Viewer),
            (Action::BillingWrite, Role::Owner),
            (Action::WorkerManage, Role::Admin),
        ];
        assert_eq!(
            rows.len(),
            ALL_ACTIONS.len(),
            "every action has a matrix row"
        );
        for (action, minimum) in rows {
            assert_eq!(action.minimum_role(), *minimum, "{}", action.as_str());
            // Every role at or above the minimum is allowed; every role
            // below is refused.
            for role in [Role::Owner, Role::Admin, Role::Member, Role::Viewer] {
                let result = authorize(&principal(role), &org(), action.resource(), *action);
                if role.rank() >= minimum.rank() {
                    assert!(
                        result.is_ok(),
                        "{role:?} must be allowed {}",
                        action.as_str()
                    );
                } else {
                    assert!(
                        result.is_err(),
                        "{role:?} must not be allowed {}",
                        action.as_str()
                    );
                }
            }
        }
    }

    #[test]
    fn foreign_organization_is_indistinguishable_from_a_missing_one() {
        let foreign = OrganizationId::try_new("org_other").unwrap();
        let denied = authorize(
            &principal(Role::Owner),
            &foreign,
            Resource::Repository,
            Action::RepositoryRead,
        )
        .unwrap_err();
        assert_eq!(
            denied,
            Denied::NotFound {
                message: "organization not found".into()
            }
        );
    }

    #[test]
    fn action_resource_mismatch_is_a_typed_refusal() {
        let denied = authorize(
            &principal(Role::Owner),
            &org(),
            Resource::Billing,
            Action::RunCreate,
        )
        .unwrap_err();
        assert!(matches!(denied, Denied::Malformed { .. }));
    }

    #[test]
    fn service_account_permission_is_role_intersected_with_scope() {
        let account = Principal::service_account(
            ServiceAccountId::try_new("sa_1").unwrap(),
            org(),
            Role::Admin,
            [Action::RepositoryRead, Action::RunCreate],
        );
        // In role AND scope: allowed.
        assert!(authorize(
            &account,
            &org(),
            Resource::Repository,
            Action::RepositoryRead
        )
        .is_ok());
        assert!(authorize(&account, &org(), Resource::Run, Action::RunCreate).is_ok());
        // In role but NOT in scope: refused.
        assert!(authorize(&account, &org(), Resource::Member, Action::MemberRead).is_err());
        assert!(authorize(
            &account,
            &org(),
            Resource::Repository,
            Action::RepositoryWrite
        )
        .is_err());
        // In scope but NOT in role (viewer-only action on an admin?): the
        // scope cannot widen the role above its rank either way.
        let viewer_account = Principal::service_account(
            ServiceAccountId::try_new("sa_2").unwrap(),
            org(),
            Role::Viewer,
            [Action::OrganizationDelete],
        );
        assert!(
            authorize(
                &viewer_account,
                &org(),
                Resource::Organization,
                Action::OrganizationDelete
            )
            .is_err(),
            "a scope entry never outranks the role"
        );
        // Effective actions are the intersection.
        assert_eq!(
            account.effective_actions(),
            vec![Action::RepositoryRead, Action::RunCreate]
        );
    }

    #[test]
    fn role_and_action_names_roundtrip() {
        for role in [Role::Owner, Role::Admin, Role::Member, Role::Viewer] {
            assert_eq!(Role::parse(role.as_str()), Some(role));
        }
        for action in ALL_ACTIONS {
            assert_eq!(Action::parse(action.as_str()), Some(*action));
        }
        assert_eq!(Role::parse("superuser"), None);
        assert_eq!(Action::parse("burn_it_all"), None);
    }
}
