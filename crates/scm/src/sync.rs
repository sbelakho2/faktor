//! Installation/repository sync: fetch what the provider reports and upsert
//! it into durable rows. Every write is an idempotent upsert keyed by the
//! provider identity `(installation, owner, name)`, so re-running a sync —
//! after a crash, a webhook, or a process restart — converges instead of
//! duplicating. Rows carry the ORGANIZATION identity of the tenant that
//! owns the installation mapping, so a later listing can never widen across
//! tenants.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::error::ScmError;
use crate::github::Clock;
use crate::ids::ScmInstallationId;
use crate::provider::{ScmInstallation, ScmProvider, ScmRepository};
use crate::store::{InstallationRow, RepositoryRow, ScmStore};

/// The outcome of one sync pass.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SyncReport {
    pub installations: usize,
    pub repositories: usize,
}

/// The installation/repository sync service.
pub struct ScmSync {
    provider: Arc<dyn ScmProvider>,
    store: Arc<dyn ScmStore>,
    clock: Arc<dyn Clock>,
}

impl ScmSync {
    pub fn new(
        provider: Arc<dyn ScmProvider>,
        store: Arc<dyn ScmStore>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            provider,
            store,
            clock,
        }
    }

    /// Sync every installation the app can see plus each installation's
    /// repositories, linked to `organization`.
    pub async fn sync_all(&self, organization: &str) -> Result<SyncReport, ScmError> {
        if organization.is_empty() || organization.len() > 256 {
            return Err(ScmError::InvalidInput(
                "organization id must be 1..=256 bytes".into(),
            ));
        }
        let installations = self.provider.list_installations().await?;
        let mut report = SyncReport::default();
        for installation in &installations {
            self.upsert_installation(installation)?;
            report.installations += 1;
            let repositories = self
                .provider
                .list_repositories(installation.installation_id)
                .await?;
            for repository in &repositories {
                self.upsert_repository(organization, repository)?;
                report.repositories += 1;
            }
        }
        Ok(report)
    }

    /// Sync one installation (webhook-driven path: a single installation
    /// was created/updated).
    pub async fn sync_installation(
        &self,
        organization: &str,
        installation: ScmInstallationId,
    ) -> Result<SyncReport, ScmError> {
        if organization.is_empty() || organization.len() > 256 {
            return Err(ScmError::InvalidInput(
                "organization id must be 1..=256 bytes".into(),
            ));
        }
        let repositories = self.provider.list_repositories(installation).await?;
        let mut report = SyncReport::default();
        for repository in &repositories {
            self.upsert_repository(organization, repository)?;
            report.repositories += 1;
        }
        Ok(report)
    }

    /// The ordered, canonical permission JSON stored for one installation.
    pub fn permissions_json(installation: &ScmInstallation) -> String {
        let map: BTreeMap<&str, &str> = installation
            .permissions
            .iter()
            .map(|(name, level)| (name.as_str(), level.as_str()))
            .collect();
        serde_json::to_string(&map).unwrap_or_else(|_| "{}".into())
    }

    fn upsert_installation(&self, installation: &ScmInstallation) -> Result<(), ScmError> {
        self.store.upsert_installation(&InstallationRow {
            installation_id: installation.installation_id.raw() as i64,
            account_login: installation.account_login.clone(),
            account_type: installation.account_type.clone(),
            permissions_json: Self::permissions_json(installation),
            suspended: installation.suspended,
            updated_ms: self.clock.now_ms(),
        })?;
        Ok(())
    }

    fn upsert_repository(
        &self,
        organization: &str,
        repository: &ScmRepository,
    ) -> Result<(), ScmError> {
        self.store.upsert_repository(&RepositoryRow {
            id: 0,
            installation_id: repository.reference.installation().raw() as i64,
            organization_id: organization.to_string(),
            owner: repository.reference.owner().to_string(),
            name: repository.reference.name().to_string(),
            full_name: repository.full_name.clone(),
            default_branch: repository.default_branch.clone(),
            private: repository.private,
            archived: repository.archived,
            updated_ms: self.clock.now_ms(),
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ScmError;
    use crate::github::ManualClock;
    use crate::ids::{ExternalOperationId, IssueRef, PullRequestRef, RemoteRef, RepositoryRef};
    use crate::provider::{
        BranchSpec, CommentTarget, PullRequestSpec, ScmBranch, ScmComment, ScmIssue,
        ScmPullRequest, ScmRemoteRef, ScmReviewEvent,
    };
    use crate::store::{MemoryScmStore, RepositoryRow};
    use async_trait::async_trait;

    #[derive(Default)]
    struct FakeProvider {
        installations: Vec<ScmInstallation>,
        repositories: Vec<ScmRepository>,
        calls: std::sync::atomic::AtomicUsize,
    }

    fn installation(id: u64) -> ScmInstallation {
        ScmInstallation {
            installation_id: ScmInstallationId::new(id),
            account_login: "acme".into(),
            account_type: "Organization".into(),
            permissions: vec![("contents".into(), "write".into())],
            suspended: false,
        }
    }

    fn repository(installation: u64, name: &str) -> ScmRepository {
        ScmRepository {
            reference: RepositoryRef::try_new(ScmInstallationId::new(installation), "acme", name)
                .unwrap(),
            full_name: format!("acme/{name}"),
            default_branch: "main".into(),
            private: true,
            archived: false,
            url: format!("https://example.test/acme/{name}"),
        }
    }

    #[async_trait]
    impl ScmProvider for FakeProvider {
        fn provider_name(&self) -> &'static str {
            "fake"
        }
        async fn repository(&self, _: &RepositoryRef) -> Result<ScmRepository, ScmError> {
            Err(ScmError::NotFound("fake".into()))
        }
        async fn issue(&self, _: &IssueRef) -> Result<ScmIssue, ScmError> {
            Err(ScmError::NotFound("fake".into()))
        }
        async fn remote_ref(&self, _: &RemoteRef) -> Result<Option<ScmRemoteRef>, ScmError> {
            Ok(None)
        }
        async fn create_or_reconcile_branch(
            &self,
            _: &ExternalOperationId,
            _: &BranchSpec,
        ) -> Result<ScmBranch, ScmError> {
            Err(ScmError::Forbidden("fake".into()))
        }
        async fn create_or_reconcile_pull_request(
            &self,
            _: &ExternalOperationId,
            _: &PullRequestSpec,
        ) -> Result<ScmPullRequest, ScmError> {
            Err(ScmError::Forbidden("fake".into()))
        }
        async fn comment(
            &self,
            _: &ExternalOperationId,
            _: &CommentTarget,
            _: &str,
        ) -> Result<ScmComment, ScmError> {
            Err(ScmError::Forbidden("fake".into()))
        }
        async fn review_events(&self, _: &PullRequestRef) -> Result<Vec<ScmReviewEvent>, ScmError> {
            Ok(Vec::new())
        }
        async fn list_installations(&self) -> Result<Vec<ScmInstallation>, ScmError> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self.installations.clone())
        }
        async fn list_repositories(
            &self,
            installation: ScmInstallationId,
        ) -> Result<Vec<ScmRepository>, ScmError> {
            Ok(self
                .repositories
                .iter()
                .filter(|r| r.reference.installation() == installation)
                .cloned()
                .collect())
        }
    }

    fn provider() -> Arc<FakeProvider> {
        Arc::new(FakeProvider {
            installations: vec![installation(1), installation(2)],
            repositories: vec![
                repository(1, "widgets"),
                repository(1, "gadgets"),
                repository(2, "sprockets"),
            ],
            calls: Default::default(),
        })
    }

    #[tokio::test]
    async fn sync_is_idempotent_and_scoped_to_the_owning_organization() {
        let store: Arc<dyn ScmStore> = Arc::new(MemoryScmStore::new());
        let sync = ScmSync::new(provider(), store.clone(), Arc::new(ManualClock::new(1_000)));
        let first = sync.sync_all("org:alpha").await.unwrap();
        assert_eq!(
            first,
            SyncReport {
                installations: 2,
                repositories: 3
            }
        );
        let second = sync.sync_all("org:alpha").await.unwrap();
        assert_eq!(second, first, "a second sync must converge, not duplicate");
        let rows: Vec<RepositoryRow> = store
            .repositories_for_organization("org:alpha", 0, 10)
            .unwrap();
        assert_eq!(rows.len(), 3);
        assert!(
            store
                .repositories_for_organization("org:beta", 0, 10)
                .unwrap()
                .is_empty(),
            "another tenant sees nothing"
        );
        // Re-linking moves the rows to the new tenant (latest owner wins).
        sync.sync_all("org:beta").await.unwrap();
        assert_eq!(
            store
                .repositories_for_organization("org:beta", 0, 10)
                .unwrap()
                .len(),
            3
        );
        assert!(store
            .repositories_for_organization("org:alpha", 0, 10)
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn installation_only_sync_is_bounded_and_validated() {
        let store: Arc<dyn ScmStore> = Arc::new(MemoryScmStore::new());
        let sync = ScmSync::new(provider(), store.clone(), Arc::new(ManualClock::new(1_000)));
        let report = sync
            .sync_installation("org:alpha", ScmInstallationId::new(2))
            .await
            .unwrap();
        assert_eq!(
            report,
            SyncReport {
                installations: 0,
                repositories: 1
            }
        );
        assert_eq!(
            store
                .repositories_for_organization("org:alpha", 0, 10)
                .unwrap()[0]
                .name,
            "sprockets"
        );
        assert!(sync.sync_all("").await.is_err());
        assert!(sync
            .sync_installation("x".repeat(257).as_str(), ScmInstallationId::new(1))
            .await
            .is_err());
    }

    #[test]
    fn permissions_json_is_canonical() {
        let mut installation = installation(1);
        installation.permissions = vec![
            ("pull_requests".into(), "write".into()),
            ("contents".into(), "write".into()),
        ];
        assert_eq!(
            ScmSync::permissions_json(&installation),
            r#"{"contents":"write","pull_requests":"write"}"#
        );
    }
}
