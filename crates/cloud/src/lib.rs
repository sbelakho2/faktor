//! faktor-cloud — the control-plane identity, organization and RBAC model
//! of the commercial foundation, plus the Wave 3 commercial metering domain.
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
//!   idempotency-keyed mutations and one-shot secret presentation;
//! - [`billing`]: the Wave 3 append-only usage ledger, the
//!   BillingAccount/Subscription/Plan/EntitlementSnapshot/Quota/Credit
//!   model, the strict config-provided plan table and the pure folds;
//! - [`billing_store`]: the [`billing_store::BillingStore`] seam over the
//!   SAME SQLite database (its own migration v2) and the in-memory twin;
//! - [`entitlements`]: the [`entitlements::EntitlementService`] — the ONE
//!   snapshot/derivation authority and the ONE admission gate.

pub mod billing;
pub mod billing_store;
pub mod durability;
pub mod enterprise;
pub mod enterprise_store;
pub mod entitlements;
pub mod error;
pub mod ids;
pub mod layered_config;
pub mod model;
pub mod oidc;
pub mod oidc_net;
pub mod rbac;
pub mod report_schedule;
pub mod service;
pub mod sso;
pub mod store;
pub mod vendor;

pub use billing::{
    fold_credits, fold_usage, Admission, AdmissionBoundary, AdmissionRequest, BillingAccount,
    BillingConfig, CreditBalance, CreditEntry, CreditKind, EntitlementExceeded,
    EntitlementSnapshot, InFlightKind, InFlightTxn, ObservedUsage, PlanConfig, ReconciliationState,
    SpendCategory, Subscription, SubscriptionStatus, TaskUsage, UsageEvent, UsageFold, UsageTotals,
    UsageUnit, CAUSE_CREDITS, CAUSE_FEATURE_MANAGED, CAUSE_PLAN, CAUSE_SUBSCRIPTION_ACTIVE,
    FEATURE_BYOK, FEATURE_CREDITS, FEATURE_MANAGED_PROVIDERS, LIMIT_MAX_ACTIVE_TASKS,
    LIMIT_MAX_CHILDREN_PER_TASK, LIMIT_MAX_MANAGED_SPEND_MICRO_PER_PERIOD,
    LIMIT_MAX_PROVIDER_ATTEMPTS_PER_TASK, LIMIT_MAX_TOKENS_PER_PERIOD,
    LIMIT_MIN_CREDIT_BALANCE_MICRO,
};
pub use billing_store::{
    BillingStore, BillingStoreError, CreditAppend, CreditAppendRefusal, MemoryBillingStore,
    ReportPeriodRow, ReportPeriodStatus, StoredCreditEntry, StoredUsageEvent, UsageAppend,
    BILLING_REPORT_SCHEMA_V4, BILLING_SCHEMA_V2, MAX_REPORT_PERIODS_PER_ORG,
};
pub use enterprise::config_layer_digest;
pub use enterprise::{
    ArtifactKind, ArtifactRecord, ArtifactReference, AuditAction, AuditEvent, AuditExport,
    AuditPrincipal, AuditPrincipalKind, BlobDeletion, BlobStoreError, ClassPolicyView, DeletionJob,
    DeletionJobState, DeletionManifest, DeletionScope, DeletionState, DeletionStep,
    EnterpriseService, EnterpriseStatus, GcOutcome, GcReport, ManifestEntry, NewArtifact,
    NewAuditEvent, NoReferences, OrgSettings, ReferenceKind, ReferenceScanError,
    RetentionBlobStore, RetentionClass, RetentionPolicy, RetentionReferenceOracle, SsoConfigRef,
    Tombstone, DIGEST_HEX_BYTES, MAX_ARTIFACT_PAGE, MAX_AUDIT_PAGE, MAX_DELETION_MANIFEST,
    MAX_ENTERPRISE_TEXT_BYTES, MAX_GC_PASS,
};
pub use enterprise_store::{EnterpriseStore, StoredConfigLayer, ENTERPRISE_SCHEMA_V3};
pub use entitlements::{
    CreditAppendOutcome, DurableSpendRow, EntitlementService, IngestReport, UsagePage,
};
pub use error::ControlPlaneError;
pub use ids::{
    ApprovalId, ArtifactId, AuthSessionId, BillingAccountId, CreditEntryId, DeletionJobId,
    ExternalIdentityId, InFlightTxnId, InvitationId, MembershipId, OrganizationId, SecretToken,
    ServiceAccountId, SubscriptionId, TokenHash, UsageEventId, UserId,
};
pub use layered_config::{
    resolve_layers, ConfigAttestation, ConfigKey, ConfigLayer, ConfigScope, EffectiveConfig,
    LayerSemantics, LayerValue, LayeredConfigError, PreferenceResolution,
};
pub use model::{
    normalize_email, ApprovalRequest, ApprovalStatus, AuthSession, ExternalIdentity, Invitation,
    InvitationStatus, Membership, Organization, Page, ServiceAccount, User,
    DEFAULT_INVITATION_TTL_MS, DEFAULT_SESSION_TTL_MS,
};
pub use oidc::{
    ClaimMapping, CodeExchangeRequest, FakeOidcAdapter, IdTokenExpectations, JwkView, OidcAdapter,
    OidcClaims, OidcDiscovery, OidcError, OidcMembership, OidcTokenSet,
};
pub use oidc_net::{
    AsyncOidcAdapter, CachedJwksView, ClientAuthMethod, ClientSecret, NetworkOidcAdapter,
    NetworkOidcConfig, DEFAULT_ALLOWED_ALGORITHMS, DEFAULT_DISCOVERY_MAX_AGE_MS,
    DEFAULT_JWKS_MAX_AGE_MS, MAX_ALLOWED_ALGORITHMS, MAX_CACHE_MAX_AGE_MS, MAX_CLIENT_SECRET_BYTES,
    MAX_CODE_EXCHANGE_INPUT_BYTES, MAX_JWKS_REFETCHES, SUPPORTED_ALGORITHMS,
};
pub use rbac::{
    authorize, Action, Denied, Principal, PrincipalSubject, Resource, Role, ALL_ACTIONS,
};
pub use report_schedule::{
    backoff_ms, catch_up_plan, period_from_index, period_index, period_key, BillingReportRunner,
    CatchUpPlan, ReportPolicy, TickOutcome, DEFAULT_REPORT_CATCH_UP, MAX_REPORT_CATCH_UP_PERIODS,
    MAX_REPORT_INTERVAL_MS, MAX_REPORT_MAX_BACKOFF_MS, MAX_REPORT_PERIOD_MS,
    MIN_REPORT_INTERVAL_MS, MIN_REPORT_PERIOD_MS,
};
pub use service::{
    sha256_hex, BootstrapResult, Clock, ControlPlane, ExternalLogin, IdentityView,
    InvitationIssued, ManualClock, MemberView, ServiceAccountIssued, SessionRevocation,
    SystemClock, MAX_IDEMPOTENCY_KEY_BYTES, MAX_PAGE,
};
pub use sso::{SsoLogin, SsoLoginOutcome, SsoStart, MAX_PENDING_SSO_LOGINS, SSO_STATE_TTL_MS};
pub use store::{
    CloudStoreError, ControlPlaneStore, IdempotencyRecord, MemoryControlPlaneStore,
    SqliteControlPlaneStore,
};
pub use vendor::{
    BillingVendorAdapter, BillingVendorConfig, ReportOutcome, DEFAULT_REPORT_PATH,
    MAX_REPORT_ATTEMPTS, MAX_REPORT_PAGES, MAX_REPORT_TASKS,
};
