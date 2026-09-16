//! faktor-scm — the provider-neutral SCM domain plus the GitHub App
//! adapter.
//!
//! Layout:
//!
//! - [`ids`]: typed, validated identities ([`ids::RepositoryRef`],
//!   [`ids::IssueRef`], [`ids::PullRequestRef`], [`ids::RemoteRef`],
//!   [`ids::ExternalOperationId`]);
//! - [`provider`]: the [`provider::ScmProvider`] seam — repository, issue,
//!   remote ref, branch/PR reconciliation, comment, review events — with no
//!   provider-specific type anywhere;
//! - [`github`]: the real GitHub App adapter (installation-token auth
//!   through the injected checked transport, minimal repository
//!   permissions, durable rate-limit backoff, ETag revalidation);
//! - [`webhook`]: HMAC-SHA256 signature verification (constant time),
//!   delivery-id dedupe and the replay window, over the durable
//!   [`webhook::WebhookInbox`];
//! - [`sync`]: installation/repository sync into durable rows;
//! - [`store`]: the [`store::ScmStore`] durable seam plus its in-memory and
//!   SQLite implementations.
//!
//! Provider quirks stay inside adapters: no GitHub type is referenced by
//! the domain, and no HTTP client is constructed here (all I/O executes
//! through the daemon's ONE policy-checked transport).

pub mod error;
pub mod github;
pub mod ids;
pub mod provider;
pub mod store;
pub mod sync;
pub mod token;
pub mod webhook;

pub use error::ScmError;
pub use github::{
    Clock, GitHubApp, GitHubAppConfig, InstallationToken, InstallationTokenSource, ManualClock,
    StaticTokenSource, SystemClock, REQUIRED_REPOSITORY_PERMISSIONS,
};
pub use ids::{
    ExternalOperationId, IssueRef, PullRequestRef, RemoteRef, RepositoryRef, ScmInstallationId,
};
pub use provider::{
    BranchSpec, CommentTarget, PullRequestSpec, ScmBranch, ScmComment, ScmInstallation, ScmIssue,
    ScmProvider, ScmPullRequest, ScmRemoteRef, ScmRepository, ScmReviewEvent,
};
pub use store::{
    DeliveryClaim, InstallationRow, MemoryScmStore, RateLimitRow, RepositoryRow, ScmOperationRow,
    ScmStore, ScmStoreError, SqliteScmStore,
};
pub use sync::{ScmSync, SyncReport};
pub use token::{
    GitHubAppTokenConfig, GitHubAppTokenSource, DEFAULT_JWT_SKEW_SECS, DEFAULT_JWT_TTL_SECS,
    MAX_APP_KEY_PEM_BYTES, MAX_APP_TOKEN_ATTEMPTS,
};
pub use webhook::{
    hmac_sha256_hex, IngestOutcome, VerifiedWebhook, WebhookError, WebhookHeaders, WebhookInbox,
    WebhookVerifier, DEFAULT_REPLAY_WINDOW_MS, MAX_WEBHOOK_BODY_BYTES,
};

/// Convenience constructor for one external-operation identity.
pub fn try_operation_id(raw: impl Into<String>) -> Result<ExternalOperationId, ScmError> {
    ExternalOperationId::try_new(raw)
}
