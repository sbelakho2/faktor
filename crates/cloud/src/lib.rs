//! faktor-cloud — the control-plane identity, organization and RBAC model
//! of the commercial foundation.
//!
//! Layout:
//!
//! - [`ids`]: typed identities and one-shot secret tokens (stored only as
//!   SHA-256 hashes);
//! - [`model`]: User / Organization / Membership / Invitation / AuthSession
//!   / ExternalIdentity / ServiceAccount / ApprovalRequest — every
//!   organization-scoped row carries its [`ids::OrganizationId`];
//! - [`rbac`]: the central [`rbac::authorize(principal, organization,
//!   resource, action)`] with the role matrix (owner/admin/member/viewer)
//!   and the service-account role ∩ scope rule;
//! - [`store`]: the [`store::ControlPlaneStore`] durable seam plus its
//!   in-memory and SQLite implementations;
//! - [`service`]: the [`service::ControlPlane`] service — bootstrap,
//!   invitations, members, service accounts, sessions, approvals — with
//!   idempotency-keyed mutations and one-shot secret presentation.

pub mod error;
pub mod ids;
pub mod model;
pub mod rbac;
pub mod service;
pub mod store;

pub use error::ControlPlaneError;
pub use ids::{
    ApprovalId, AuthSessionId, ExternalIdentityId, InvitationId, MembershipId, OrganizationId,
    SecretToken, ServiceAccountId, TokenHash, UserId,
};
pub use model::{
    normalize_email, ApprovalRequest, ApprovalStatus, AuthSession, ExternalIdentity, Invitation,
    InvitationStatus, Membership, Organization, Page, ServiceAccount, User,
    DEFAULT_INVITATION_TTL_MS, DEFAULT_SESSION_TTL_MS,
};
pub use rbac::{
    authorize, Action, Denied, Principal, PrincipalSubject, Resource, Role, ALL_ACTIONS,
};
pub use service::{
    sha256_hex, BootstrapResult, Clock, ControlPlane, IdentityView, InvitationIssued, ManualClock,
    MemberView, ServiceAccountIssued, SystemClock, MAX_IDEMPOTENCY_KEY_BYTES, MAX_PAGE,
};
pub use store::{
    CloudStoreError, ControlPlaneStore, IdempotencyRecord, MemoryControlPlaneStore,
    SqliteControlPlaneStore,
};
