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
            installation_id: installation.installation_id.to_sqlite_i64(),
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
            installation_id: repository.reference.installation().to_sqlite_i64(),
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
    use crate::store::{MemoryScmStore, RepositoryRow, SqliteScmStore};
    use async_trait::async_trait;

    #[derive(Default)]
    struct FakeProvider {
        installations: Vec<ScmInstallation>,
        repositories: Vec<ScmRepository>,
        calls: std::sync::atomic::AtomicUsize,
    }

    fn installation(id: u64) -> ScmInstallation {
        installation_of(ScmInstallationId::try_from_raw(id).unwrap())
    }

    fn installation_of(installation_id: ScmInstallationId) -> ScmInstallation {
        ScmInstallation {
            installation_id,
            account_login: "acme".into(),
            account_type: "Organization".into(),
            permissions: vec![("contents".into(), "write".into())],
            suspended: false,
        }
    }

    fn repository(installation: u64, name: &str) -> ScmRepository {
        repository_of(ScmInstallationId::try_from_raw(installation).unwrap(), name)
    }

    fn repository_of(installation: ScmInstallationId, name: &str) -> ScmRepository {
        ScmRepository {
            reference: RepositoryRef::try_new(installation, "acme", name).unwrap(),
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
            .sync_installation("org:alpha", ScmInstallationId::try_from_raw(2).unwrap())
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
            .sync_installation(
                "x".repeat(257).as_str(),
                ScmInstallationId::try_from_raw(1).unwrap()
            )
            .await
            .is_err());
    }

    /// P1 persistence-domain bound: the maximum admitted installation id
    /// (`i64::MAX`) is persisted POSITIVE, round-trips exactly through
    /// SQLite, and the signed column still orders and range-scans correctly
    /// at the high end of the domain.
    #[tokio::test]
    async fn high_but_valid_installation_ids_round_trip_and_order_in_sqlite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scm.db");
        let store: Arc<dyn ScmStore> = Arc::new(SqliteScmStore::open(&path).unwrap());
        let low = ScmInstallationId::try_from_raw(1).unwrap();
        let near = ScmInstallationId::try_from_raw(i64::MAX as u64 - 1).unwrap();
        let max = ScmInstallationId::try_from_raw(i64::MAX as u64).unwrap();
        let provider = Arc::new(FakeProvider {
            installations: vec![
                installation_of(near),
                installation_of(max),
                installation_of(low),
            ],
            repositories: vec![
                repository_of(near, "gadgets"),
                repository_of(max, "sprockets"),
                repository_of(low, "widgets"),
            ],
            calls: Default::default(),
        });
        let sync = ScmSync::new(provider, store.clone(), Arc::new(ManualClock::new(1_000)));
        let report = sync.sync_all("org:alpha").await.unwrap();
        assert_eq!(report.installations, 3);
        assert_eq!(report.repositories, 3);

        let ids: Vec<i64> = store
            .installations()
            .unwrap()
            .iter()
            .map(|row| row.installation_id)
            .collect();
        assert_eq!(
            ids,
            vec![1, i64::MAX - 1, i64::MAX],
            "ORDER BY installation_id is exact and positive at the high end"
        );
        assert!(ids.iter().all(|id| *id > 0), "no wrapped row");

        // Point queries at the high end resolve exactly their rows.
        let max_rows = store.repositories_for_installation(i64::MAX).unwrap();
        assert_eq!(max_rows.len(), 1);
        assert_eq!(max_rows[0].name, "sprockets");
        assert_eq!(max_rows[0].installation_id, i64::MAX);
        assert_eq!(
            store.repositories_for_installation(i64::MAX - 1).unwrap()[0].name,
            "gadgets"
        );

        // Raw SQL from an independent connection: the durable column is
        // positive, `MIN`/`MAX` see the true domain bounds, and a range scan
        // over the top of the domain sees exactly the high rows.
        let probe = rusqlite::Connection::open(&path).unwrap();
        let (minimum, maximum, negatives): (i64, i64, i64) = probe
            .query_row(
                "SELECT MIN(installation_id), MAX(installation_id),
                        COUNT(*) FILTER (WHERE installation_id < 0)
                 FROM scm_installation",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((minimum, maximum, negatives), (1, i64::MAX, 0));
        let high: Vec<i64> = probe
            .prepare(
                "SELECT installation_id FROM scm_repository
                 WHERE installation_id >= ?1 ORDER BY installation_id",
            )
            .unwrap()
            .query_map([i64::MAX - 1], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(high, vec![i64::MAX - 1, i64::MAX]);
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
