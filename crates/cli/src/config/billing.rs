//! `config::billing`: schema domain of the daemon config.

use super::*;

/// The additive `[billing]` section: the Wave 3 commercial metering service
/// (usage ledger + entitlements + credits).
///
/// Strict and additive:
///
/// - `enabled` (default `false`): when false — the default and the only
///   value for every pre-existing config — the daemon builds NO billing
///   service, creates no billing database file, and every
///   `/native/entitlements`, `/native/credits/grant` and
///   `/native/usage?org=...` request answers a typed 409 `billing_disabled`.
///   The local daemon is byte-identical to the pre-billing daemon;
/// - `database`: an optional simple FILE NAME (default `billing.db`) for
///   the durable usage/credit ledger. It reuses the control-plane store's
///   migration ladder (its own `user_version` v2 tables);
/// - `organization`: the organization the local daemon's sessions meter
///   into (required when enabled: a billing service without a tenant would
///   admit nothing and account for nothing);
/// - `account` / `account_name` / `managed`: the local billing account
///   provisioned idempotently at startup. `managed = true` (default)
///   permits Faktor-managed provider spend (credits are debited); `false`
///   restricts the account to BYOK usage (recorded, never debited);
/// - `default_plan` + `plans` + `managed_providers`: the plan table is the
///   ONLY source of features/limits and the managed-provider set the ONLY
///   source of the managed/BYOK decision. Unknown plan/feature/limit names
///   are refused loudly; an enabled section with no plans (or a default
///   plan outside the table) is a startup error — the daemon never boots
///   with a silently empty entitlement surface;
/// - unknown keys, duplicates, non-object shapes and wrong value types are
///   parse errors.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Default)]
pub struct BillingCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub database: Option<String>,
    #[serde(default)]
    pub organization: Option<String>,
    #[serde(default)]
    pub account: Option<String>,
    #[serde(default)]
    pub account_name: Option<String>,
    /// Whether the provisioned account permits Faktor-managed provider
    /// spend (default `true`; `false` = a BYOK-only account).
    #[serde(default)]
    pub managed: Option<bool>,
    #[serde(default)]
    pub default_plan: Option<String>,
    /// The plan table, keyed by plan id (each `PlanConfig.plan_id` must
    /// equal its key). Strict shapes come from `faktor_cloud::PlanConfig`.
    #[serde(default)]
    pub plans: std::collections::BTreeMap<String, faktor_cloud::PlanConfig>,
    /// Provider ids whose spend is Faktor-managed; every other provider is
    /// BYOK. Empty = all providers are BYOK.
    #[serde(default)]
    pub managed_providers: Vec<String>,
    /// The additive `[billing.report]` schedule: periodically reports the
    /// durable usage fold through the vendor adapter. Absent/disabled = no
    /// schedule rows are written and no vendor call is ever made.
    #[serde(default)]
    pub report: Option<BillingReportCfg>,
}

/// The `[billing.report]` section: the durable report schedule over the
/// report-only vendor adapter.
///
/// Strict and additive:
///
/// - `enabled` (default `false`): no schedule row is written, no vendor
///   call is made and the daemon is otherwise byte-identical;
/// - `base_url` (required when enabled): the vendor's absolute http(s) base
///   (a loopback mock in tests);
/// - `report_path` (default `/v1/usage-reports`), `user_agent`, `auth_env`:
///   the vendor request shape. `auth_env` defaults to
///   `FAKTOR_BILLING_VENDOR_TOKEN`; an EMPTY string explicitly selects the
///   unauthenticated local-mock shape (documented, never silent);
/// - `interval_ms` (default 60s, 1s..=24h): how often the maintenance loop
///   checks whether the current period is due;
/// - `period_ms` (default 24h, 1min..=366d): the reporting period bucket;
/// - `max_catch_up` (default 24, 0..=256): how many periods crossed during a
///   downtime gap are backfilled (oldest first) before the regular cadence
///   resumes; older periods beyond the cap are marked skipped-permanently
///   with a durable audit row, never silently dropped;
/// - `max_attempts` (default 5, cap 5) and `retry_base_ms` /
///   `max_backoff_ms`: the retry policy of a failed period (exponential
///   backoff with deterministic jitter).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct BillingReportCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub report_path: Option<String>,
    #[serde(default)]
    pub auth_env: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub interval_ms: Option<i64>,
    #[serde(default)]
    pub period_ms: Option<i64>,
    #[serde(default)]
    pub max_attempts: Option<u32>,
    #[serde(default)]
    pub retry_base_ms: Option<i64>,
    #[serde(default)]
    pub max_backoff_ms: Option<i64>,
    #[serde(default)]
    pub max_catch_up: Option<usize>,
}

/// The `[billing]` keys, in stable order (unknown-field errors list them).
pub const BILLING_FIELDS: &[&str] = &[
    "enabled",
    "database",
    "organization",
    "account",
    "account_name",
    "managed",
    "default_plan",
    "plans",
    "managed_providers",
    "report",
];

/// The default billing database file name (relative to the daemon data
/// dir).
pub const DEFAULT_BILLING_DATABASE: &str = "billing.db";

impl<'de> serde::Deserialize<'de> for BillingCfg {
    fn deserialize<D>(de: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{Error as _, MapAccess, Visitor};

        struct SectionVisitor;

        impl<'de> Visitor<'de> for SectionVisitor {
            type Value = BillingCfg;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("the [billing] section as a JSON object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<BillingCfg, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut out = BillingCfg::default();
                let mut seen: u16 = 0;
                while let Some(key) = map.next_key::<String>()? {
                    let (bit, name) = match key.as_str() {
                        "enabled" => (1u16, "enabled"),
                        "database" => (2, "database"),
                        "organization" => (4, "organization"),
                        "account" => (8, "account"),
                        "account_name" => (16, "account_name"),
                        "managed" => (32, "managed"),
                        "default_plan" => (64, "default_plan"),
                        "plans" => (128, "plans"),
                        "managed_providers" => (256, "managed_providers"),
                        "report" => (512, "report"),
                        other => return Err(A::Error::unknown_field(other, BILLING_FIELDS)),
                    };
                    if seen & bit != 0 {
                        return Err(A::Error::duplicate_field(name));
                    }
                    seen |= bit;
                    match bit {
                        1 => out.enabled = map.next_value::<bool>()?,
                        2 => out.database = map.next_value::<Option<String>>()?,
                        4 => out.organization = map.next_value::<Option<String>>()?,
                        8 => out.account = map.next_value::<Option<String>>()?,
                        16 => out.account_name = map.next_value::<Option<String>>()?,
                        32 => out.managed = map.next_value::<Option<bool>>()?,
                        64 => out.default_plan = map.next_value::<Option<String>>()?,
                        128 => {
                            out.plans = map.next_value::<std::collections::BTreeMap<
                                String,
                                faktor_cloud::PlanConfig,
                            >>()?
                        }
                        256 => out.managed_providers = map.next_value::<Vec<String>>()?,
                        _ => out.report = map.next_value::<Option<BillingReportCfg>>()?,
                    }
                }
                // Disabled parity: an explicitly disabled report section
                // resolves exactly like the absent one.
                if out.report.as_ref().is_some_and(|report| !report.enabled) {
                    out.report = None;
                }
                Ok(out)
            }
        }

        de.deserialize_map(SectionVisitor)
    }
}

impl BillingCfg {
    /// The strict billing configuration handed to the service (`None` while
    /// disabled). Validated eagerly: a plan table the config cannot honor is
    /// refused at load, never at first admission.
    pub fn service_config(&self) -> Result<Option<faktor_cloud::BillingConfig>, String> {
        if !self.enabled {
            return Ok(None);
        }
        self.validate()?;
        Ok(Some(self.build_service_config()?))
    }

    /// Build (and validate) the cloud-side configuration without recursing
    /// back into [`Self::validate`].
    pub(crate) fn build_service_config(&self) -> Result<faktor_cloud::BillingConfig, String> {
        let config = faktor_cloud::BillingConfig {
            default_plan: self.default_plan.clone(),
            plans: self.plans.clone(),
            managed_providers: self.managed_providers.iter().cloned().collect(),
        };
        config.validate().map_err(|e| format!("billing: {e}"))?;
        Ok(config)
    }

    /// The resolved billing database path under `data_dir` (`None` when the
    /// section is disabled — the daemon then never creates it).
    pub fn billing_path(&self, data_dir: &Path) -> Result<Option<std::path::PathBuf>, String> {
        if !self.enabled {
            return Ok(None);
        }
        self.validate()?;
        let name = self
            .database
            .clone()
            .unwrap_or_else(|| DEFAULT_BILLING_DATABASE.to_string());
        Ok(Some(data_dir.join(name)))
    }

    /// The organization the daemon meters into (required when enabled).
    pub fn organization(&self) -> Result<faktor_cloud::OrganizationId, String> {
        let raw = self.organization.clone().ok_or_else(|| {
            "billing: an enabled [billing] section requires `organization`".to_string()
        })?;
        faktor_cloud::OrganizationId::try_new(raw).map_err(|e| format!("billing: {e}"))
    }

    /// The provisioned billing account id (default `local`).
    pub fn account_id(&self) -> Result<faktor_cloud::BillingAccountId, String> {
        let raw = self.account.clone().unwrap_or_else(|| "local".to_string());
        faktor_cloud::BillingAccountId::try_new(raw).map_err(|e| format!("billing: {e}"))
    }

    /// Validate the section (called by [`Config::validate`] on both load
    /// paths). A disabled section validates nothing beyond its shape.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(database) = &self.database {
            CloudCfg::validate_database_name("billing database", database)?;
        }
        if let Some(report) = &self.report {
            report.validate()?;
        }
        if self.enabled {
            let organization = self.organization()?;
            let _ = organization;
            let account = self.account_id()?;
            let _ = account;
            if let Some(name) = &self.account_name {
                if name.is_empty() || name.len() > 128 {
                    return Err("billing: account_name must be 1..=128 bytes".into());
                }
            }
            if self.plans.is_empty() {
                return Err(
                    "billing: an enabled section requires a non-empty `plans` table (the ONLY source of features/limits)"
                        .into(),
                );
            }
            self.build_service_config()?;
        }
        Ok(())
    }
}

impl BillingReportCfg {
    /// Validate the section. A disabled section validates nothing beyond its
    /// shape; an enabled one requires a vendor base URL and bounded cadence
    /// bounds.
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        let _ = self.vendor_config()?;
        let _ = self.policy()?;
        Ok(())
    }

    /// The strict vendor-adapter configuration.
    pub fn vendor_config(&self) -> Result<faktor_cloud::BillingVendorConfig, String> {
        let mut config = faktor_cloud::BillingVendorConfig {
            base_url: self
                .base_url
                .clone()
                .ok_or_else(|| {
                    "billing report: an enabled section requires `base_url`".to_string()
                })?
                .trim_end_matches('/')
                .to_string(),
            ..Default::default()
        };
        if let Some(report_path) = &self.report_path {
            config.report_path = report_path.clone();
        }
        // An explicitly EMPTY auth_env is the documented unauthenticated
        // local-mock opt-out: the vendor config keeps its default (valid)
        // env name and the runner passes no credential.
        if let Some(auth_env) = self.auth_env.as_deref().filter(|env| !env.is_empty()) {
            config.auth_env = auth_env.to_string();
        }
        if let Some(user_agent) = &self.user_agent {
            config.user_agent = user_agent.clone();
        }
        if let Some(max_attempts) = self.max_attempts {
            config.max_attempts = max_attempts;
        }
        if let Some(retry_base_ms) = self.retry_base_ms {
            config.retry_base_ms = retry_base_ms;
        }
        config
            .validate()
            .map_err(|e| format!("billing report: {e}"))?;
        Ok(config)
    }

    /// Whether the report calls carry a bearer credential resolved from
    /// `auth_env` (`false` only for the explicitly empty auth-env opt-out).
    pub fn unauthenticated(&self) -> bool {
        matches!(self.auth_env.as_deref(), Some(""))
    }

    pub fn interval_ms_resolved(&self) -> i64 {
        self.interval_ms.unwrap_or(60_000)
    }

    pub fn period_ms_resolved(&self) -> i64 {
        self.period_ms.unwrap_or(86_400_000)
    }

    pub fn max_attempts_resolved(&self) -> u32 {
        self.max_attempts
            .unwrap_or(faktor_cloud::MAX_REPORT_ATTEMPTS)
    }

    pub fn retry_base_ms_resolved(&self) -> i64 {
        self.retry_base_ms.unwrap_or(250)
    }

    pub fn max_backoff_ms_resolved(&self) -> i64 {
        self.max_backoff_ms.unwrap_or(3_600_000)
    }

    /// The schedule policy handed to the maintenance runner.
    pub fn policy(&self) -> Result<crate::billing_report::ReportPolicy, String> {
        let interval = self.interval_ms_resolved();
        if !(crate::billing_report::MIN_REPORT_INTERVAL_MS
            ..=crate::billing_report::MAX_REPORT_INTERVAL_MS)
            .contains(&interval)
        {
            return Err(format!(
                "billing report: interval_ms must be {}..={}",
                crate::billing_report::MIN_REPORT_INTERVAL_MS,
                crate::billing_report::MAX_REPORT_INTERVAL_MS
            ));
        }
        let period = self.period_ms_resolved();
        if !(crate::billing_report::MIN_REPORT_PERIOD_MS
            ..=crate::billing_report::MAX_REPORT_PERIOD_MS)
            .contains(&period)
        {
            return Err(format!(
                "billing report: period_ms must be {}..={}",
                crate::billing_report::MIN_REPORT_PERIOD_MS,
                crate::billing_report::MAX_REPORT_PERIOD_MS
            ));
        }
        let max_backoff = self.max_backoff_ms_resolved();
        if !(0..=crate::billing_report::MAX_REPORT_MAX_BACKOFF_MS).contains(&max_backoff) {
            return Err(format!(
                "billing report: max_backoff_ms must be 0..={}",
                crate::billing_report::MAX_REPORT_MAX_BACKOFF_MS
            ));
        }
        let max_catch_up = self
            .max_catch_up
            .unwrap_or(faktor_cloud::DEFAULT_REPORT_CATCH_UP);
        if max_catch_up > faktor_cloud::MAX_REPORT_CATCH_UP_PERIODS {
            return Err(format!(
                "billing report: max_catch_up must be <= {}",
                faktor_cloud::MAX_REPORT_CATCH_UP_PERIODS
            ));
        }
        Ok(crate::billing_report::ReportPolicy {
            interval_ms: interval,
            period_ms: period,
            max_attempts: self.max_attempts_resolved(),
            retry_base_ms: self.retry_base_ms_resolved(),
            max_backoff_ms: max_backoff,
            max_catch_up,
        })
    }
}
