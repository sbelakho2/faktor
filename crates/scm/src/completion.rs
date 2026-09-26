//! The completion-step SCM adapter: the ONE thin bridge between the
//! orchestrator's native PR step and the canonical [`crate::provider::ScmProvider`]
//! domain. The orchestrator owns the durable external-operation protocol
//! (write-before-call, reconcile-from-recorded-identity); this adapter owns
//! the provider-side mapping only:
//!
//! - resolve the synced installation of one `organization/repository` pair
//!   from the durable [`ScmStore`] rows the GitHub App sync maintains (the
//!   canonical [`RepositoryRef`] requires an installation id; the git remote
//!   URL does not carry one). No synced row is an explicit
//!   [`ScmError::Config`] blocker — never a guess and never a silent skip;
//! - reconcile the branch at the exact verified head FIRST (idempotent,
//!   conflict on a moved branch), then create-or-reconcile the pull request
//!   under the caller's durable [`ExternalOperationId`]. Both calls go
//!   through the canonical seam, so the real [`crate::github::GitHubApp`]
//!   adapter (installation tokens, REST, rate limits, durable journal)
//!   executes them in production; there is no second SCM domain.
//!
//! The adapter performs NO durable bookkeeping of its own: the orchestrator
//! records the external-operation identity and decides retry/reconcile, and
//! the canonical provider journals its own operation rows. A crash at any
//! boundary reconciles from the recorded identity instead of duplicating a
//! remote object.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::ScmError;
use crate::ids::{ExternalOperationId, RepositoryRef, ScmInstallationId};
use crate::provider::{BranchSpec, PullRequestSpec, ScmBranch, ScmProvider, ScmPullRequest};
use crate::store::ScmStore;

/// Bound on one page of the synced-repository walk that resolves the
/// installation of a repository.
pub const COMPLETION_REPOSITORY_PAGE: usize = 200;
/// Bound on the configured tenant organization label.
pub const MAX_COMPLETION_ORGANIZATION_BYTES: usize = 128;

/// One native-PR reconciliation request: the orchestrator's exact durable
/// input identity plus the operator-configured PR shape. All strings are
/// already bounded/validated by the caller; the canonical provider
/// re-validates every ref and identity it touches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionPrRequest {
    /// The caller's durable operation identity, folded into every canonical
    /// provider journal row (the orchestrator's operation key).
    pub operation_id: ExternalOperationId,
    /// The git remote's parsed GitHub owner (organization or user).
    pub organization: String,
    /// The git remote's parsed GitHub repository name.
    pub repository: String,
    /// The PR head branch.
    pub branch: String,
    /// The PR base branch.
    pub base: String,
    /// The exact verified head commit the remote branch must carry.
    pub head_sha: String,
    /// The stable Faktor task marker the reconciliation lookup is keyed by.
    pub marker: String,
    /// The PR title.
    pub title: String,
    /// The PR body (carries the marker for provider-side reconciliation).
    pub body: String,
}

/// The remote objects one native-PR reconciliation certified: the resolved
/// repository identity, the reconciled branch and the reconciled PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionPrResult {
    pub repository: RepositoryRef,
    pub branch: ScmBranch,
    pub pull_request: ScmPullRequest,
}

/// The completion-step SCM seam. Object-safe (the runner holds one
/// `Arc<dyn CompletionScm>`); the production implementation is
/// [`GitHubCompletionScm`] over the real GitHub App adapter.
#[async_trait]
pub trait CompletionScm: Send + Sync {
    /// The stable provider name folded into the orchestrator's durable
    /// operation key.
    fn provider_name(&self) -> &'static str;

    /// Create-or-reconcile the branch at the requested head, then the pull
    /// request matching the exact `(repository, head, base, marker)`
    /// identity. Idempotent: a repeated call after a lost response returns
    /// the EXISTING objects and never duplicates a remote object. A branch
    /// that resolves to a different head is an
    /// [`ScmError::ReconcileConflict`], never a force-move.
    async fn reconcile_completion_pr(
        &self,
        request: &CompletionPrRequest,
    ) -> Result<CompletionPrResult, ScmError>;
}

/// The production adapter: the canonical provider (the real [`crate::github::GitHubApp`]
/// in the daemon) plus the synced [`ScmStore`] rows an installation lookup
/// needs, scoped to one tenant organization.
pub struct GitHubCompletionScm {
    provider: Arc<dyn ScmProvider>,
    store: Arc<dyn ScmStore>,
    organization: String,
}

impl std::fmt::Debug for GitHubCompletionScm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubCompletionScm")
            .field("provider", &self.provider.provider_name())
            .field("organization", &self.organization)
            .finish_non_exhaustive()
    }
}

impl GitHubCompletionScm {
    /// Build the adapter over one canonical provider, the synced store and
    /// the tenant organization label the rows are scoped to. The label is
    /// validated (non-empty, bounded, no control bytes) so a degenerate
    /// config can never silently select the wrong tenant.
    pub fn new(
        provider: Arc<dyn ScmProvider>,
        store: Arc<dyn ScmStore>,
        organization: impl Into<String>,
    ) -> Result<Self, ScmError> {
        let organization = organization.into();
        if organization.is_empty() || organization.len() > MAX_COMPLETION_ORGANIZATION_BYTES {
            return Err(ScmError::Config(format!(
                "completion scm organization must be 1..={MAX_COMPLETION_ORGANIZATION_BYTES} bytes"
            )));
        }
        if organization.chars().any(char::is_control) {
            return Err(ScmError::Config(
                "completion scm organization contains control characters".into(),
            ));
        }
        Ok(Self {
            provider,
            store,
            organization,
        })
    }

    /// The synced installation of one owner/repository pair: a bounded walk
    /// of the tenant's durable repository rows (case-insensitive, exactly
    /// like GitHub's repository identity). No row is an explicit
    /// configuration blocker naming the sync prerequisite.
    fn installation_for(&self, owner: &str, name: &str) -> Result<ScmInstallationId, ScmError> {
        let mut after_id = 0i64;
        loop {
            let rows = self.store.repositories_for_organization(
                &self.organization,
                after_id,
                COMPLETION_REPOSITORY_PAGE,
            )?;
            if rows.is_empty() {
                break;
            }
            for row in &rows {
                if row.owner.eq_ignore_ascii_case(owner) && row.name.eq_ignore_ascii_case(name) {
                    let raw = u64::try_from(row.installation_id).map_err(|_| {
                        ScmError::Store(format!(
                            "synced repository {owner}/{name} carries a non-positive installation id"
                        ))
                    })?;
                    return ScmInstallationId::try_from_raw(raw);
                }
            }
            let last = rows.last().map(|row| row.id).unwrap_or(after_id);
            if last <= after_id || rows.len() < COMPLETION_REPOSITORY_PAGE {
                break;
            }
            after_id = last;
        }
        Err(ScmError::Config(format!(
            "no synced GitHub App repository {owner}/{name} for organization {:?}; run the \
             GitHub App installation/repository sync before requesting a pull request",
            self.organization
        )))
    }
}

#[async_trait]
impl CompletionScm for GitHubCompletionScm {
    fn provider_name(&self) -> &'static str {
        self.provider.provider_name()
    }

    async fn reconcile_completion_pr(
        &self,
        request: &CompletionPrRequest,
    ) -> Result<CompletionPrResult, ScmError> {
        let installation = self.installation_for(&request.organization, &request.repository)?;
        let repository = RepositoryRef::try_new(
            installation,
            request.organization.clone(),
            request.repository.clone(),
        )?;
        let branch = self
            .provider
            .create_or_reconcile_branch(
                &request.operation_id,
                &BranchSpec {
                    repository: repository.clone(),
                    branch: request.branch.clone(),
                    head_sha: request.head_sha.clone(),
                    marker: request.marker.clone(),
                },
            )
            .await?;
        if branch.head_sha != request.head_sha {
            return Err(ScmError::ReconcileConflict {
                detail: format!(
                    "branch {} is at {} but {} was requested",
                    request.branch, branch.head_sha, request.head_sha
                ),
            });
        }
        let pull_request = self
            .provider
            .create_or_reconcile_pull_request(
                &request.operation_id,
                &PullRequestSpec {
                    repository: repository.clone(),
                    head: request.branch.clone(),
                    base: request.base.clone(),
                    marker: request.marker.clone(),
                    title: request.title.clone(),
                    body: request.body.clone(),
                },
            )
            .await?;
        Ok(CompletionPrResult {
            repository,
            branch,
            pull_request,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The adapter refuses a degenerate tenant label instead of silently
    /// querying the wrong organization scope.
    #[test]
    fn adapter_configuration_is_strict() {
        let provider: Arc<dyn ScmProvider> = Arc::new(
            crate::github::GitHubApp::new(
                crate::github::GitHubAppConfig::default(),
                Arc::new(faktor_provider::egress::PolicyCheckedHttpTransport::permissive()),
                Arc::new(crate::github::StaticTokenSource::minimal(1).unwrap()),
                Arc::new(crate::store::MemoryScmStore::new()),
                Arc::new(crate::github::SystemClock),
            )
            .unwrap(),
        );
        for bad in [
            "",
            "x".repeat(MAX_COMPLETION_ORGANIZATION_BYTES + 1).as_str(),
        ] {
            assert!(
                GitHubCompletionScm::new(
                    provider.clone(),
                    Arc::new(crate::store::MemoryScmStore::new()),
                    bad
                )
                .is_err(),
                "{bad:?} must be refused"
            );
        }
        assert!(GitHubCompletionScm::new(
            provider,
            Arc::new(crate::store::MemoryScmStore::new()),
            "tenant"
        )
        .is_ok());
    }
}
