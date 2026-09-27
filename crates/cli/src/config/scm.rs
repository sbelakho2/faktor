//! `config::scm`: schema domain of the daemon config.

#![allow(unused_imports)]

use super::*;

/// The `[cloud.github_app]` section: the real GitHub App wiring. Disabled by
/// default; while disabled no adapter/sync/webhook sink is built and the
/// daemon keeps its pre-existing SCM surface byte-identical.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CloudGithubAppCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub app_id: Option<u64>,
    /// The payload NAME of the app's PKCS#8 private key PEM. The Rust field
    /// carries a NAME (not the key itself), hence the different name; the
    /// config wire key is `private_key`.
    #[serde(default, rename = "private_key")]
    pub key_payload: Option<String>,
    /// The payload NAME of the app's webhook HMAC secret.
    #[serde(default)]
    pub webhook_secret: Option<String>,
    /// The REST API base (default `https://api.github.com`; a loopback mock
    /// in tests).
    #[serde(default)]
    pub api_base: Option<String>,
    /// The tenant organization the synced rows belong to (required).
    #[serde(default)]
    pub organization: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub page_size: Option<usize>,
    #[serde(default)]
    pub max_pages: Option<usize>,
    /// The optional periodic reconcile timer. Absent (or explicitly
    /// disabled, normalized away byte-for-byte) = webhook-only parity: no
    /// timer is built and no extra sync ever runs.
    #[serde(default, deserialize_with = "deserialize_reconcile_section")]
    pub reconcile: Option<CloudGithubAppReconcileCfg>,
}

/// The `[cloud.github_app.reconcile]` section: the bounded periodic
/// installation/repository re-sync (post-readiness, in addition to the
/// webhook-driven syncs). Disabled by default; a disabled section resolves
/// byte-identically to the absent one.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct CloudGithubAppReconcileCfg {
    #[serde(default)]
    pub enabled: bool,
    /// Base cadence between passes (`1s..=24h`; default 5min).
    #[serde(default)]
    pub interval_ms: Option<i64>,
    /// Cadence jitter bound (`0..=60s`, also capped by the interval; default
    /// 30s). The next pass is `interval + deterministic jitter`.
    #[serde(default)]
    pub jitter_ms: Option<i64>,
    /// Failure-backoff ceiling (`interval..=24h`; default 1h). A failed pass
    /// retries after a bounded exponential backoff.
    #[serde(default)]
    pub max_backoff_ms: Option<i64>,
}

/// Disabled parity: `{enabled: false}` (or a missing section) resolves to
/// `None`, exactly like the absent key.
pub(crate) fn deserialize_reconcile_section<'de, D>(
    de: D,
) -> Result<Option<CloudGithubAppReconcileCfg>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = <Option<CloudGithubAppReconcileCfg> as serde::Deserialize>::deserialize(de)?;
    Ok(raw.filter(|section| section.enabled))
}

/// The default SCM database file name.
pub const DEFAULT_SCM_DATABASE: &str = "scm.db";

impl CloudGithubAppCfg {
    /// Validate the section. A disabled section validates nothing beyond its
    /// shape; an enabled one requires an app id, the two staged payload
    /// NAMES and the tenant organization, and its api base must validate.
    pub fn validate(&self) -> Result<(), String> {
        for (field, name) in [
            ("private_key", self.key_payload.as_deref()),
            ("webhook_secret", self.webhook_secret.as_deref()),
        ] {
            if let Some(name) = name {
                crate::payload::PayloadDir::validate_name(name)
                    .map_err(|e| format!("cloud github_app: {field} {e}"))?;
            }
        }
        if let Some(reconcile) = &self.reconcile {
            if !self.enabled {
                return Err(
                    "cloud github_app: reconcile requires an enabled github_app section".into(),
                );
            }
            reconcile.validate()?;
        }
        if !self.enabled {
            return Ok(());
        }
        if self.app_id.unwrap_or(0) == 0 {
            return Err("cloud github_app: an enabled section requires a non-zero `app_id`".into());
        }
        if self.key_payload.is_none() {
            return Err(
                "cloud github_app: an enabled section requires `private_key` (a payload name)"
                    .into(),
            );
        }
        if self.webhook_secret.is_none() {
            return Err(
                "cloud github_app: an enabled section requires `webhook_secret` (a payload name)"
                    .into(),
            );
        }
        let _ = self.organization()?;
        let config = self.app_config()?;
        config
            .validate()
            .map_err(|e| format!("cloud github_app: {e}"))?;
        Ok(())
    }

    /// The tenant organization the synced rows are linked to.
    pub fn organization(&self) -> Result<String, String> {
        let raw = self.organization.clone().ok_or_else(|| {
            "cloud github_app: an enabled section requires `organization`".to_string()
        })?;
        faktor_cloud::OrganizationId::try_new(raw.clone())
            .map_err(|e| format!("cloud github_app: {e}"))?;
        Ok(raw)
    }

    /// The REST adapter configuration.
    pub fn app_config(&self) -> Result<faktor_scm::GitHubAppConfig, String> {
        let mut config = faktor_scm::GitHubAppConfig {
            api_base: self
                .api_base
                .clone()
                .unwrap_or_else(|| "https://api.github.com".to_string()),
            ..Default::default()
        };
        if let Some(user_agent) = &self.user_agent {
            config.user_agent = user_agent.clone();
        }
        if let Some(page_size) = self.page_size {
            config.page_size = page_size;
        }
        if let Some(max_pages) = self.max_pages {
            config.max_pages = max_pages;
        }
        config
            .validate()
            .map_err(|e| format!("cloud github_app: {e}"))?;
        Ok(config)
    }

    /// The optional periodic-reconcile policy (`None` = no timer; webhook
    /// parity) with every bound already validated.
    pub fn reconcile_policy(&self) -> Result<Option<faktor_scm::ReconcilePolicy>, String> {
        self.reconcile
            .as_ref()
            .map(CloudGithubAppReconcileCfg::policy)
            .transpose()
    }
}

impl CloudGithubAppReconcileCfg {
    /// The strict policy handed to the reconcile runner: configured values
    /// or documented defaults, validated against the crate's hard bounds.
    pub fn policy(&self) -> Result<faktor_scm::ReconcilePolicy, String> {
        let policy = faktor_scm::ReconcilePolicy {
            interval_ms: self
                .interval_ms
                .unwrap_or(faktor_scm::DEFAULT_RECONCILE_INTERVAL_MS),
            jitter_ms: self
                .jitter_ms
                .unwrap_or(faktor_scm::DEFAULT_RECONCILE_JITTER_MS),
            max_backoff_ms: self
                .max_backoff_ms
                .unwrap_or(faktor_scm::DEFAULT_RECONCILE_MAX_BACKOFF_MS),
        };
        policy
            .validate()
            .map_err(|e| format!("cloud github_app reconcile: {e}"))?;
        Ok(policy)
    }

    /// Validate the section (bounds only; disabled sections are normalized
    /// away before this is reached).
    pub fn validate(&self) -> Result<(), String> {
        let _ = self.policy()?;
        Ok(())
    }
}
