//! The provider-neutral SCM domain: one [`ScmProvider`] seam every provider
//! adapter implements, expressed only in the typed identities of
//! [`crate::ids`]. No provider-specific spelling, no HTTP, and no GitHub
//! type exists here — provider quirks stay inside the adapter.
//!
//! Reconciliation contract (normative for every implementation):
//!
//! - [`ScmProvider::create_or_reconcile_branch`] MUST return the EXISTING
//!   branch when `spec.branch` already resolves to the requested head; a
//!   branch that resolves to a DIFFERENT head is a typed
//!   [`ScmError::ReconcileConflict`], never a force-move and never a
//!   duplicate;
//! - [`ScmProvider::create_or_reconcile_pull_request`] MUST look the PR up
//!   by the exact `(repository, head, base, marker)` identity and return the
//!   EXISTING object on a match. A create path that always creates would
//!   duplicate the PR after a crash between the remote create and the
//!   durable record;
//! - every mutating call carries the caller's durable
//!   [`ExternalOperationId`], which the adapter journals BEFORE the remote
//!   call (a crash therefore leaves a durable identity to reconcile from,
//!   never an untracked remote write).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::ScmError;
use crate::ids::{
    ExternalOperationId, IssueRef, PullRequestRef, RemoteRef, RepositoryRef, ScmInstallationId,
};

/// One installation as the provider reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScmInstallation {
    pub installation_id: ScmInstallationId,
    pub account_login: String,
    pub account_type: String,
    /// The provider's granted permission set (`(name, level)`).
    pub permissions: Vec<(String, String)>,
    pub suspended: bool,
}

/// One repository as the provider reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScmRepository {
    pub reference: RepositoryRef,
    pub full_name: String,
    pub default_branch: String,
    pub private: bool,
    pub archived: bool,
    pub url: String,
}

/// One branch as the provider reports it. `created` is `true` only when
/// THIS call created the ref (a reconciliation returns `false`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScmBranch {
    pub reference: RemoteRef,
    pub head_sha: String,
    pub url: String,
    pub created: bool,
}

/// One pull request as the provider reports it. `version` is the opaque
/// provider version token (head sha + updated-at); `created` is `true` only
/// when THIS call created the PR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScmPullRequest {
    pub reference: PullRequestRef,
    pub head: String,
    pub base: String,
    pub state: String,
    pub marker: String,
    pub version: String,
    pub url: String,
    pub created: bool,
}

/// One issue as the provider reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScmIssue {
    pub reference: IssueRef,
    pub title: String,
    pub state: String,
    pub body: String,
}

/// One remote ref lookup result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScmRemoteRef {
    pub reference: RemoteRef,
    pub head_sha: String,
    pub url: String,
}

/// One created comment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScmComment {
    pub id: String,
    pub url: String,
}

/// One review event of a pull request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScmReviewEvent {
    pub id: String,
    pub reviewer: String,
    pub state: String,
    pub submitted_ms: Option<i64>,
}

/// The target of one comment call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CommentTarget {
    Issue(IssueRef),
    PullRequest(PullRequestRef),
}

/// One idempotent branch reconciliation request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchSpec {
    pub repository: RepositoryRef,
    pub branch: String,
    pub head_sha: String,
    pub marker: String,
}

/// One idempotent pull-request reconciliation request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestSpec {
    pub repository: RepositoryRef,
    pub head: String,
    pub base: String,
    pub marker: String,
    pub title: String,
    pub body: String,
}

/// The provider-neutral SCM seam. Every method is async because a real
/// adapter performs network I/O; implementations MUST be `Send + Sync` (a
/// single adapter instance is shared across runtime tasks).
#[async_trait]
pub trait ScmProvider: Send + Sync {
    /// The stable provider name folded into durable operation identities.
    fn provider_name(&self) -> &'static str;

    /// Read one repository.
    async fn repository(&self, repository: &RepositoryRef) -> Result<ScmRepository, ScmError>;

    /// Read one issue.
    async fn issue(&self, issue: &IssueRef) -> Result<ScmIssue, ScmError>;

    /// Read one remote ref without creating anything.
    async fn remote_ref(&self, reference: &RemoteRef) -> Result<Option<ScmRemoteRef>, ScmError>;

    /// Create or reconcile `spec.branch` at `spec.head_sha` (idempotent).
    async fn create_or_reconcile_branch(
        &self,
        operation: &ExternalOperationId,
        spec: &BranchSpec,
    ) -> Result<ScmBranch, ScmError>;

    /// Create or reconcile the pull request matching the EXACT
    /// `(repository, head, base, marker)` identity (idempotent).
    async fn create_or_reconcile_pull_request(
        &self,
        operation: &ExternalOperationId,
        spec: &PullRequestSpec,
    ) -> Result<ScmPullRequest, ScmError>;

    /// Create one comment on an issue or pull request (idempotent under the
    /// caller's operation identity: a replayed call must not duplicate).
    async fn comment(
        &self,
        operation: &ExternalOperationId,
        target: &CommentTarget,
        body: &str,
    ) -> Result<ScmComment, ScmError>;

    /// The review events of one pull request (bounded page).
    async fn review_events(
        &self,
        pull_request: &PullRequestRef,
    ) -> Result<Vec<ScmReviewEvent>, ScmError>;

    /// Every installation visible to the app (installation/repository sync).
    async fn list_installations(&self) -> Result<Vec<ScmInstallation>, ScmError>;

    /// Every repository of one installation (installation/repository sync).
    async fn list_repositories(
        &self,
        installation: ScmInstallationId,
    ) -> Result<Vec<ScmRepository>, ScmError>;
}
