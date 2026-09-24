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
//! - [`reconcile`]: the periodic, single-flight reconcile timer over that
//!   sync (bounded cadence jitter, bounded failure backoff, typed bounded
//!   journal);
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
pub mod reconcile;
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
pub use reconcile::{
    failure_backoff_ms, jittered_delay_ms, ReconcileEvent, ReconcileKind, ReconcilePolicy,
    ScmReconcile, DEFAULT_RECONCILE_INTERVAL_MS, DEFAULT_RECONCILE_JITTER_MS,
    DEFAULT_RECONCILE_MAX_BACKOFF_MS, MAX_RECONCILE_BACKOFF_MS, MAX_RECONCILE_ERROR_BYTES,
    MAX_RECONCILE_INTERVAL_MS, MAX_RECONCILE_JITTER_MS, MAX_RECONCILE_JOURNAL,
    MIN_RECONCILE_INTERVAL_MS,
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
    hmac_sha256_hex, installation_of, IngestOutcome, VerifiedWebhook, WebhookError, WebhookHeaders,
    WebhookInbox, WebhookVerifier, DEFAULT_REPLAY_WINDOW_MS, MAX_WEBHOOK_BODY_BYTES,
};

/// Convenience constructor for one external-operation identity.
pub fn try_operation_id(raw: impl Into<String>) -> Result<ExternalOperationId, ScmError> {
    ExternalOperationId::try_new(raw)
}

/// Documented wall-clock bound for ONE SCM HTTP attempt (send plus full
/// bounded body materialization). The shared egress client only bounds
/// connect, so a provider that accepts and then stalls would otherwise pin
/// the caller — and every retry loop — forever. Retry loops are bounded by
/// their attempt counts, so total time is bounded by
/// `attempts × (SCM_HTTP_TIMEOUT_MS + backoff)`.
pub const SCM_HTTP_TIMEOUT_MS: u64 = 30_000;

/// Execute one raw request under [`SCM_HTTP_TIMEOUT_MS`]; a breach is a
/// typed [`ScmError::Transport`] naming the call. Policy denials map to
/// [`ScmError::Forbidden`] exactly like the direct egress mapping.
pub(crate) async fn execute_raw_bounded(
    transport: &dyn faktor_provider::egress::HttpTransport,
    request: faktor_provider::egress::RawRequest,
    what: &str,
) -> Result<faktor_provider::egress::RawResponse, ScmError> {
    execute_raw_bounded_with(
        transport,
        request,
        std::time::Duration::from_millis(SCM_HTTP_TIMEOUT_MS),
        what,
    )
    .await
}

/// The response budget every SCM read passes to the materializing egress
/// seam: head/idle/total all equal the caller's wall bound, and the body is
/// capped by the seam's own [`faktor_provider::egress::MAX_RAW_RESPONSE_BYTES`]
/// materialization bound.
pub(crate) fn scm_response_budget(
    bound: std::time::Duration,
) -> faktor_provider::egress::ResponseBudget {
    faktor_provider::egress::ResponseBudget::for_timeout(
        bound,
        faktor_provider::egress::MAX_RAW_RESPONSE_BYTES as u64,
    )
}

/// Testable variant of [`execute_raw_bounded`] with an explicit bound.
pub(crate) async fn execute_raw_bounded_with(
    transport: &dyn faktor_provider::egress::HttpTransport,
    request: faktor_provider::egress::RawRequest,
    bound: std::time::Duration,
    what: &str,
) -> Result<faktor_provider::egress::RawResponse, ScmError> {
    let budget = scm_response_budget(bound);
    match tokio::time::timeout(
        bound,
        faktor_provider::egress::execute_raw(transport, request, &budget),
    )
    .await
    {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(faktor_provider::egress::EgressError::Denied { url, .. })) => Err(
            ScmError::Forbidden(format!("egress to {url} denied by policy")),
        ),
        Ok(Err(other)) => Err(ScmError::Transport(other.to_string())),
        Err(_) => Err(ScmError::Transport(format!(
            "{what} exceeded its {} ms HTTP bound",
            bound.as_millis()
        ))),
    }
}
