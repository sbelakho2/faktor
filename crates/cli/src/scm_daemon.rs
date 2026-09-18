//! Daemon-side construction of the GitHub App SCM surface: the real
//! [`faktor_scm::GitHubApp`] adapter over the ONE checked transport, the
//! RS256 installation-token source, the durable webhook inbox and the
//! installation/repository sync — all built from strict config and the
//! operator-staged payload directory (private key PEM + webhook secret).
//!
//! Wiring contract:
//!
//! - disabled (`[cloud.github_app]` absent/disabled, the default): NOTHING
//!   is built, no payload is read and no payload directory is created —
//!   byte-identical to the pre-GitHub-App daemon;
//! - enabled: a missing/corrupt/too-permissive payload refuses construction
//!   with the typed payload error (the caller turns it into a startup
//!   failure), so no half-wired SCM surface ever boots;
//! - after readiness the caller runs [`ScmDaemon::run`]: one bounded initial
//!   installation/repository sync, then one idempotent re-sync per verified
//!   webhook delivery through a bounded queue. When the optional
//!   `[cloud.github_app.reconcile]` timer is enabled, the SAME loop also
//!   re-runs the sync on the configured jittered cadence (with bounded
//!   failure backoff) — every path shares one single-flight slot, so
//!   overlapping triggers coalesce instead of duplicating provider calls.
//!   All paths converge on the durable upserts, so a crash/retry/
//!   redelivery/timer pass can never duplicate rows. Disabled reconcile =
//!   no timer, byte-identical to the webhook-only surface.

use std::path::Path;
use std::sync::Arc;

use faktor_provider::egress::HttpTransport;
use faktor_scm::{
    Clock, GitHubApp, GitHubAppConfig, GitHubAppTokenConfig, GitHubAppTokenSource, IngestOutcome,
    ScmInstallationId, ScmReconcile, ScmStore, ScmSync, SystemClock, WebhookError, WebhookHeaders,
    WebhookInbox, WebhookVerifier,
};
use faktor_server::native::scm_webhook::WebhookSink;

use crate::config::CloudCfg;
use crate::payload::PayloadDir;

/// Bound on the pending webhook-driven re-syncs. A full queue refuses the
/// delivery with a retryable store error so the provider redelivers (and
/// the durable inbox claim keeps the eventual sync exactly-once).
pub const MAX_WEBHOOK_SYNC_QUEUE: usize = 64;

/// The PKCS#8 test key shared by the GitHub App wiring tests.
#[cfg(test)]
pub(crate) const TEST_PRIVATE_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQDOdaRv7SIbH6qK
dvYxg7L/7edtfeKJcjPxLCjzXYw71LBuPm4PVYeJexTW6pgAE0++LlY5TvK2VdR4
BpBpkSehRCfGKLiyw9hVpRm8Fz8Lfrl7EFltVxRAo2lkNeUEHwsnHE6CvOWw1qgO
tGLKF4W+Vi8WSPRgv0HHMYEOCnK++yGnrZjYD4vrqsP55iDtwyb6j1iv9TdnXzY2
XwhDQ4SMxOw+N3NXrz072KlDVFs3sY+WTvtfnaCaGdmYb1qDYZEUvBdc6m7MA0og
hXuWz4Kg+Yzx4rWXAAJPmJi39JWFPJR5hlf2j+1eOTbrSBSzygfltN3aVZul1v3X
/6/uzghHAgMBAAECggEAIAlmQk34OGBCDOlny4glqwwGGNnrYKudfsN8+UKfY5td
40WBu5Roiz9TnQPbIUvd2GOFUrA6/ms0JInUN+Vj0mTqjRe9jVPRinyrkSHEUSrR
alS/o7Va+arBzGCGkIympOOCFUxtkfLFMj7wg26B/OaPuPQKI8cZ1GiMn5qkcpjr
COOoirnJQ1N1/2TPFUEvX+ZG04LumJlTOuw1n+yQGLXt2Bkf9FeBT9f4b98WLrQk
PNd5DxXfJGaNLmX17n6qgQ2+qeYb5mPvI6OONgzQOSHtjsrniPYtoY/N+ZabSRWW
5BLP5x4uo4fYW+GXbhSpO4imlOX411dVDf5J2g7IaQKBgQDzh/VUJTbjTzRa+mY0
wFauM3WvXWRZ3FrevbUwrP2EZ75odZ2Z89DAc/OwXzvuzRviMaK9fLLDZTWAZmEh
9qm/JVIDX6Pn4raa7FAg4pdI/GinlpuAoeg7HbSIe7amLQoIWYe7riYI/k7zI21+
cA0fBGskIVDbAcC/LE5+MP8iDwKBgQDZB8ZuGVs7ntkunLl4JuwY4/UniMUlzWA0
Ekj8k2lbIG5Aq72U1NkSQE3JbkIvRpDrySWUYWPQ7YIYTMxygymNmML1puLr1/9A
Iy+06O6S4jL0+JZORcSaZ9BQZGdmCCl8T+G2TR5w8x3PVfiV8hLLC3TLSt8TGdFv
qY5vEUeOSQKBgQDQY7b6mh2txUj30O1ElpGV31MFDNWiT30yvQMe8+i8NEoq+Poz
kv8+r/oHIncWkU0a8X5gxyPxL9noVbMobPo0JqtXV6/Z7ZZ0W2L1wO/T9KlZPvcx
y1n9vB2P7M0Oxduf6XzMjOjfKT5FsDsxxpBzykQkVp3pykY1UKSaNzMa4QKBgAdn
mIGRI+e417gbaMiMq2l9/ZNHu1I625lrNkpHzURqqthSA7ncOTvCLeU9ecybH76r
sjiJyhoKwHGLzT3q87P9DknLU9qwF+lcSfhmKh2g0hRBlv88qiSKfjT/9/cnOCMh
ppXNs8guw0mbqUuUYsfCsE1vVIUWUGr64f0wHbzhAoGAZh9kgDMjZlJMF9jJCrzD
m+vTRpldotdtts9A4/rPY2kV7nphAoL0leSKqG7UUtKxXsaD7wHpxgczLUf8Y5Fi
y07Oh0VbL/IpRuNvse5Jn1cOYCDJs2x12WE3l8toG+krj0ZDwcnkhVnMhplT9pro
hCsczsx/P3pKSzRXIkRTm9A=
-----END PRIVATE KEY-----";

#[derive(Debug, Clone, PartialEq, Eq)]
enum SyncRequest {
    All,
    Installation(ScmInstallationId),
}

/// The wired GitHub App surface: the durable inbox the webhook route
/// dispatches into plus the bounded re-sync queue. `run` owns the consuming
/// half (initial sync, then one re-sync per delivery, plus the optional
/// periodic reconcile timer).
pub struct ScmDaemon {
    sync: Arc<ScmSync>,
    /// The single-flight reconcile runner; `None` while the timer is
    /// disabled (disabled parity: webhook-only, no extra provider call).
    reconcile: Option<Arc<ScmReconcile>>,
    inbox: Arc<WebhookInbox>,
    organization: String,
    queue: tokio::sync::mpsc::Sender<SyncRequest>,
    receiver: std::sync::Mutex<Option<tokio::sync::mpsc::Receiver<SyncRequest>>>,
}

impl std::fmt::Debug for ScmDaemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScmDaemon")
            .field("organization", &self.organization)
            .finish_non_exhaustive()
    }
}

/// Build the wired GitHub App surface from strict config. `Ok(None)` = the
/// GitHub App is not enabled (disabled parity: nothing is read or built).
pub fn build_scm_daemon(
    cfg: &CloudCfg,
    data_dir: &Path,
    store: Arc<dyn ScmStore>,
    transport: Arc<dyn HttpTransport>,
) -> Result<Option<Arc<ScmDaemon>>, String> {
    let Some(app_cfg) = cfg.github_app.as_ref().filter(|app| app.enabled) else {
        return Ok(None);
    };
    if !cfg.enabled {
        return Err("cloud github_app: requires [cloud] enabled".into());
    }
    app_cfg.validate()?;
    let payload_root = cfg.payload_root(data_dir)?;
    let payloads = PayloadDir::new(payload_root);
    let private_key_name = app_cfg
        .private_key
        .as_deref()
        .ok_or("cloud github_app: an enabled section requires `private_key`")?;
    let webhook_secret_name = app_cfg
        .webhook_secret
        .as_deref()
        .ok_or("cloud github_app: an enabled section requires `webhook_secret`")?;
    let private_key_pem = payloads
        .load_private_key_pem(private_key_name)
        .map_err(|e| format!("cloud github_app: {e}"))?;
    let webhook_secret = payloads
        .load_secret(webhook_secret_name)
        .map_err(|e| format!("cloud github_app: {e}"))?;

    let app_config = app_cfg.app_config()?;
    let organization = app_cfg.organization()?;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let mut token_config = GitHubAppTokenConfig {
        app_id: app_cfg.app_id.unwrap_or(0),
        private_key_pkcs8_pem: private_key_pem,
        api_base: app_config.api_base.clone(),
        user_agent: app_config.user_agent.clone(),
        ..Default::default()
    };
    token_config.max_attempts = token_config.max_attempts.max(1);
    let tokens = Arc::new(
        GitHubAppTokenSource::new(token_config, transport.clone(), clock.clone())
            .map_err(|e| format!("cloud github_app: {e}"))?,
    );
    let app = GitHubApp::new(
        GitHubAppConfig {
            api_base: app_config.api_base.clone(),
            user_agent: app_config.user_agent.clone(),
            page_size: app_config.page_size,
            max_pages: app_config.max_pages,
        },
        transport,
        tokens,
        store.clone(),
        clock.clone(),
    )
    .map_err(|e| format!("cloud github_app: {e}"))?;
    let sync = Arc::new(ScmSync::new(Arc::new(app), store.clone(), clock.clone()));
    let reconcile = match app_cfg.reconcile_policy()? {
        Some(policy) => Some(Arc::new(
            ScmReconcile::new(sync.clone(), organization.clone(), policy, clock.clone())
                .map_err(|e| format!("cloud github_app reconcile: {e}"))?,
        )),
        None => None,
    };
    let verifier = WebhookVerifier::new(webhook_secret.into_bytes())
        .map_err(|e| format!("cloud github_app: webhook secret: {e}"))?;
    let (queue, receiver) = tokio::sync::mpsc::channel(MAX_WEBHOOK_SYNC_QUEUE);
    Ok(Some(Arc::new(ScmDaemon {
        sync,
        reconcile,
        inbox: Arc::new(WebhookInbox::new(verifier, store)),
        organization,
        queue,
        receiver: std::sync::Mutex::new(Some(receiver)),
    })))
}

impl ScmDaemon {
    /// One bounded installation/repository sync of the whole app. When the
    /// reconcile timer is enabled, this path shares the runner's
    /// single-flight slot with webhook passes and the timer.
    pub async fn sync_all(&self) -> Result<faktor_scm::SyncReport, faktor_scm::ScmError> {
        match &self.reconcile {
            Some(reconcile) => reconcile.sync_all().await,
            None => self.sync.sync_all(&self.organization).await,
        }
    }

    /// One bounded re-sync of a single installation (webhook path), sharing
    /// the same single-flight slot as [`Self::sync_all`].
    pub async fn sync_installation(
        &self,
        installation: ScmInstallationId,
    ) -> Result<faktor_scm::SyncReport, faktor_scm::ScmError> {
        match &self.reconcile {
            Some(reconcile) => reconcile.sync_installation(installation).await,
            None => {
                self.sync
                    .sync_installation(&self.organization, installation)
                    .await
            }
        }
    }

    /// The bounded, typed reconcile journal (empty while the timer is
    /// disabled). Test-surface observation of the single-flight/backoff
    /// state.
    #[cfg(test)]
    pub fn reconcile_journal(&self) -> Vec<faktor_scm::ReconcileEvent> {
        self.reconcile
            .as_ref()
            .map(|reconcile| reconcile.journal())
            .unwrap_or_default()
    }

    fn take_receiver(&self) -> Option<tokio::sync::mpsc::Receiver<SyncRequest>> {
        self.receiver
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    /// Handle one queued webhook-driven sync request (idempotent upserts).
    async fn handle_request(&self, request: SyncRequest) {
        let outcome = match request {
            SyncRequest::All => self.sync_all().await.map(|report| (report, None)),
            SyncRequest::Installation(installation) => self
                .sync_installation(installation)
                .await
                .map(|report| (report, Some(installation))),
        };
        match outcome {
            Ok((report, Some(installation))) => tracing::info!(
                installation = installation.raw(),
                repositories = report.repositories,
                "scm webhook sync complete"
            ),
            Ok((report, None)) => tracing::info!(
                installations = report.installations,
                repositories = report.repositories,
                "scm webhook sync complete"
            ),
            Err(e) => tracing::warn!("scm webhook sync failed (redelivery converges): {e}"),
        }
    }

    /// Run the durable sync half: ONE initial bounded sync, then one
    /// idempotent re-sync per queued webhook delivery — plus, when the
    /// periodic reconcile timer is enabled, one single-flight pass on the
    /// configured cadence. Runs until the queue (and every sender) is
    /// dropped or the task is aborted at shutdown. Failures are logged,
    /// never fatal: the next delivery, timer pass or restart converges.
    pub async fn run(self: Arc<Self>) {
        let Some(mut receiver) = self.take_receiver() else {
            tracing::warn!("scm daemon run called twice; the second run has no queue");
            return;
        };
        match self.sync_all().await {
            Ok(report) => tracing::info!(
                installations = report.installations,
                repositories = report.repositories,
                "scm initial sync complete"
            ),
            Err(e) => tracing::warn!("scm initial sync failed (webhook syncs still apply): {e}"),
        }
        let Some(reconcile) = self.reconcile.clone() else {
            // Disabled parity: exactly the pre-reconcile webhook-only loop.
            while let Some(request) = receiver.recv().await {
                self.handle_request(request).await;
            }
            return;
        };
        loop {
            let delay = reconcile.scheduled_delay_ms().max(1) as u64;
            tokio::select! {
                request = receiver.recv() => match request {
                    Some(request) => self.handle_request(request).await,
                    None => break,
                },
                _ = tokio::time::sleep(std::time::Duration::from_millis(delay)) => {
                    match self.sync_all().await {
                        Ok(report) => tracing::info!(
                            installations = report.installations,
                            repositories = report.repositories,
                            "scm reconcile sync complete"
                        ),
                        Err(e) => tracing::warn!(
                            "scm reconcile sync failed (bounded backoff applies): {e}"
                        ),
                    }
                }
            }
        }
    }
}

impl WebhookSink for ScmDaemon {
    fn deliver(
        &self,
        headers: &WebhookHeaders,
        body: &[u8],
    ) -> Result<IngestOutcome, WebhookError> {
        let now_ms = SystemClock.now_ms();
        // Authenticate FIRST (signature/replay/bounds), then validate the
        // payload, then claim: a forged delivery is never influenced by the
        // body, and a signed-but-malformed payload is refused before any
        // durable write or queue enqueue.
        self.inbox.verify(headers, body, now_ms)?;
        let request = match faktor_scm::installation_of(body)? {
            Some(id) => SyncRequest::Installation(id),
            None => SyncRequest::All,
        };
        let outcome = self.inbox.ingest(headers, body, now_ms)?;
        // A duplicate delivery still enqueues the (idempotent) sync: a
        // redelivery after a saturated queue must not lose its sync, and the
        // upserts converge instead of duplicating.
        self.queue.try_send(request).map_err(|_| {
            WebhookError::Store("the webhook sync queue is saturated; redelivery converges".into())
        })?;
        Ok(outcome)
    }
}

#[cfg(test)]
#[path = "scm_daemon_tests.rs"]
mod scm_daemon_tests;
