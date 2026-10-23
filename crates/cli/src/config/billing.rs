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
///   with a silently empty entitlement surface. Feature tags cover BOTH
///   halves of the commercial vocabulary: the inference-centric tags
///   (`managed_providers`, `byok`, `credits`) and the value-centric ones
///   (`advanced_verification_policy`, `organization_contracts`,
///   `shared_intelligence`, `cross_repo_intelligence`, `outcome_analytics`,
///   `signed_proof_provenance`, `audit_retention_history`,
///   `model_optimization_learning`, `private_runners`,
///   `enterprise_governance` plus the `governance_sso`/`governance_scim`
///   capability flags) — so a 100% local deployment with no inference
///   limits still maps paid value entitlements;
/// - `subscription` (optional): provision ONE durable organization
///   subscription at startup (idempotent `put_subscription`). Without a
///   subscription row the plan table still resolves (`plan_found`), but
///   every gated admission and value entitlement is refused
///   `subscription_inactive`; the local operator therefore needs an
///   explicit way to record the entitlement they configured. The subsection
///   names the plan (must exist in `plans`), the status (`active`,
///   `expired` or `canceled`; default `active`) and an optional derived
///   expiry (`expires_ms`). The row is provisioned only for the configured
///   billing organization and never invents an invoice or a vendor event;
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
    /// The additive `[billing.subscription]` subsection: provision the
    /// configured plan as an explicit organization subscription at startup.
    /// Absent = no subscription row is written (admissions stay
    /// `subscription_inactive`, never silently granted).
    #[serde(default)]
    pub subscription: Option<BillingSubscriptionCfg>,
    /// The additive `[billing.report]` schedule: periodically reports the
    /// durable usage fold through the vendor adapter. Absent/disabled = no
    /// schedule rows are written and no vendor call is ever made.
    #[serde(default)]
    pub report: Option<BillingReportCfg>,
    /// The additive `[billing.payments]` subsection: the real payment /
    /// invoice / seat / tax / currency plane. Absent/disabled = no
    /// `payment.db` is created and every payment route answers typed 409
    /// `payments_disabled`; the overview keeps the honest unavailable states.
    #[serde(default)]
    pub payments: Option<BillingPaymentsCfg>,
}

/// The `[billing.subscription]` subsection: the explicit local subscription
/// the operator records for the configured billing organization. It is a
/// CONFIG statement (no vendor call, no invoice): the durable row is the
/// same one the entitlement reads consume, so the value gate and the
/// admission gate agree.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BillingSubscriptionCfg {
    /// The plan id; it must name one of the `[billing.plans]` entries.
    pub plan_id: String,
    /// `active` (default), `trialing`, `expired` or `canceled`.
    #[serde(default)]
    pub status: Option<String>,
    /// Optional derived expiry (milliseconds since the Unix epoch). An
    /// `active` row past this instant is inactive, never silently renewed.
    #[serde(default)]
    pub expires_ms: Option<i64>,
}

impl BillingSubscriptionCfg {
    pub fn status(&self) -> Result<faktor_cloud::SubscriptionStatus, String> {
        match self.status.as_deref().unwrap_or("active") {
            "active" => Ok(faktor_cloud::SubscriptionStatus::Active),
            "trialing" => Ok(faktor_cloud::SubscriptionStatus::Trialing),
            "expired" => Ok(faktor_cloud::SubscriptionStatus::Expired),
            "canceled" => Ok(faktor_cloud::SubscriptionStatus::Canceled),
            other => Err(format!(
                "billing subscription: unknown status {other:?} (active|trialing|expired|canceled)"
            )),
        }
    }

    fn validate(
        &self,
        plans: &std::collections::BTreeMap<String, faktor_cloud::PlanConfig>,
    ) -> Result<(), String> {
        if self.plan_id.trim().is_empty() {
            return Err("billing subscription: plan_id must not be empty".into());
        }
        if !plans.contains_key(&self.plan_id) {
            return Err(format!(
                "billing subscription: plan {:?} is not in the plans table",
                self.plan_id
            ));
        }
        let status = self.status()?;
        if let Some(expires_ms) = self.expires_ms {
            if expires_ms < 0 {
                return Err("billing subscription: expires_ms must be >= 0".into());
            }
        }
        if status == faktor_cloud::SubscriptionStatus::Trialing && self.expires_ms.is_none() {
            return Err("billing subscription: a trialing status requires expires_ms".into());
        }
        Ok(())
    }
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
    "subscription",
    "report",
    "payments",
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
                        "subscription" => (512, "subscription"),
                        "report" => (1024, "report"),
                        "payments" => (2048, "payments"),
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
                        512 => {
                            out.subscription =
                                map.next_value::<Option<BillingSubscriptionCfg>>()?
                        }
                        1024 => out.report = map.next_value::<Option<BillingReportCfg>>()?,
                        _ => {
                            out.payments = map.next_value::<Option<BillingPaymentsCfg>>()?
                        }
                    }
                }
                // Disabled parity: an explicitly disabled report section
                // resolves exactly like the absent one.
                if out.report.as_ref().is_some_and(|report| !report.enabled) {
                    out.report = None;
                }
                if out
                    .payments
                    .as_ref()
                    .is_some_and(|payments| !payments.enabled)
                {
                    out.payments = None;
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

    /// The resolved payment database path under `data_dir` (`None` when the
    /// payments subsection is absent/disabled: no `payment.db` is created).
    pub fn payments_path(&self, data_dir: &Path) -> Result<Option<std::path::PathBuf>, String> {
        let Some(payments) = self.payments.as_ref().filter(|payments| payments.enabled) else {
            return Ok(None);
        };
        payments.database_path(data_dir)
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
        if let Some(payments) = &self.payments {
            if !self.enabled {
                return Err(
                    "billing payments: an enabled [billing.payments] section requires [billing] enabled"
                        .into(),
                );
            }
            payments.validate(&self.plans)?;
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
            if let Some(subscription) = &self.subscription {
                subscription.validate(&self.plans)?;
            }
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

/// The additive `[billing.payments]` subsection: the real commercial
/// payment/invoice plane. It is separate from the inference plan table (which
/// intentionally carries no price) and names its provider credentials by
/// environment-variable NAME only.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct BillingPaymentsCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub database: Option<String>,
    /// `local` (offline SQLite provider) or `stripe` (checked-egress adapter).
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub webhook_secret_env: Option<String>,
    /// The plan price catalogue, keyed by plan id (the key must equal the
    /// entry's `plan_id`).
    #[serde(default)]
    pub prices: std::collections::BTreeMap<String, BillingPlanPriceCfg>,
    /// Optional currency exponent overrides (USD/EUR default to 2, JPY to 0).
    #[serde(default)]
    pub currencies: std::collections::BTreeMap<String, u32>,
    /// Explicit FX rates; no conversion is ever inferred.
    #[serde(default)]
    pub fx: Vec<BillingFxCfg>,
    #[serde(default)]
    pub tax: Option<BillingTaxCfg>,
    #[serde(default)]
    pub dunning: Option<BillingDunningCfg>,
    #[serde(default)]
    pub stripe: Option<BillingStripeCfg>,
}

/// One exact plan price in one currency (integer minor units).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BillingPlanPriceCfg {
    pub plan_id: String,
    pub currency: String,
    pub monthly_minor: i64,
    pub annual_minor: i64,
    #[serde(default)]
    pub per_seat_monthly_minor: i64,
    #[serde(default)]
    pub per_seat_annual_minor: i64,
}

/// One explicit directional FX rate (`rate_bps = 10_000` is 1:1).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BillingFxCfg {
    pub from: String,
    pub to: String,
    pub rate_bps: i64,
}

/// One tax jurisdiction.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BillingTaxRateCfg {
    pub rate_bps: i64,
    pub inclusive: bool,
}

/// The configured tax table and default jurisdiction.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct BillingTaxCfg {
    #[serde(default)]
    pub default_jurisdiction: Option<String>,
    #[serde(default)]
    pub rates: std::collections::BTreeMap<String, BillingTaxRateCfg>,
}

/// The dunning policy.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BillingDunningCfg {
    pub max_attempts: u32,
    pub grace_ms: i64,
}

/// The Stripe adapter's environment-variable references.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BillingStripeCfg {
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub webhook_secret_env: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
}

/// The default payment database file name.
pub const DEFAULT_PAYMENTS_DATABASE: &str = "payments.db";
/// The default local webhook secret environment variable.
pub const DEFAULT_PAYMENTS_WEBHOOK_ENV: &str = "FAKTOR_BILLING_WEBHOOK_SECRET";

impl BillingPaymentsCfg {
    /// The resolved database path (`None` when disabled).
    pub fn database_path(&self, data_dir: &Path) -> Result<Option<std::path::PathBuf>, String> {
        if !self.enabled {
            return Ok(None);
        }
        let name = self
            .database
            .clone()
            .unwrap_or_else(|| DEFAULT_PAYMENTS_DATABASE.to_string());
        CloudCfg::validate_database_name("payments database", &name)?;
        Ok(Some(data_dir.join(name)))
    }

    /// Validate the subsection. A disabled subsection validates only its
    /// strict shape.
    pub fn validate(
        &self,
        plans: &std::collections::BTreeMap<String, faktor_cloud::PlanConfig>,
    ) -> Result<(), String> {
        if let Some(database) = &self.database {
            CloudCfg::validate_database_name("payments database", database)?;
        }
        if !self.enabled {
            return Ok(());
        }
        let _ = self.runtime_config(plans)?;
        Ok(())
    }

    /// The runtime configuration handed to `faktor-payments`. Strict: an
    /// unknown plan, malformed currency, negative price, absent plan price for
    /// a configured plan, duplicate FX pair or bad tax/dunning value is a
    /// configuration error, never silently dropped.
    pub fn runtime_config(
        &self,
        plans: &std::collections::BTreeMap<String, faktor_cloud::PlanConfig>,
    ) -> Result<faktor_payments::PaymentsConfig, String> {
        let provider = match self.provider.as_deref().unwrap_or("local") {
            "local" => faktor_payments::ProviderKind::Local,
            "stripe" => faktor_payments::ProviderKind::Stripe,
            other => {
                return Err(format!(
                    "billing payments: unknown provider {other:?} (local|stripe)"
                ))
            }
        };
        let mut catalogue = faktor_payments::PriceCatalogue::default();
        for (code, exponent) in &self.currencies {
            catalogue.currencies.insert(
                code.clone(),
                faktor_payments::CurrencySpec {
                    exponent: *exponent,
                },
            );
        }
        for (key, price) in &self.prices {
            if !plans.contains_key(key) {
                return Err(format!(
                    "billing payments: price {key:?} names a plan absent from [billing.plans]"
                ));
            }
            catalogue.plans.insert(
                key.clone(),
                faktor_payments::PlanPrice {
                    plan_id: price.plan_id.clone(),
                    currency: price.currency.clone(),
                    monthly_minor: price.monthly_minor,
                    annual_minor: price.annual_minor,
                    per_seat_monthly_minor: price.per_seat_monthly_minor,
                    per_seat_annual_minor: price.per_seat_annual_minor,
                },
            );
        }
        for rate in &self.fx {
            let key = format!("{}/{}", rate.from, rate.to);
            if catalogue.fx.insert(key.clone(), rate.rate_bps).is_some() {
                return Err(format!("billing payments: duplicate FX rate {key:?}"));
            }
        }
        let tax = match &self.tax {
            None => faktor_payments::TaxConfig::default(),
            Some(tax) => faktor_payments::TaxConfig {
                default_jurisdiction: tax.default_jurisdiction.clone(),
                rates: tax
                    .rates
                    .iter()
                    .map(|(jurisdiction, rate)| {
                        (
                            jurisdiction.clone(),
                            faktor_payments::TaxRate {
                                rate_bps: rate.rate_bps,
                                inclusive: rate.inclusive,
                            },
                        )
                    })
                    .collect(),
            },
        };
        let dunning = match &self.dunning {
            None => faktor_payments::DunningConfig::default(),
            Some(dunning) => faktor_payments::DunningConfig {
                max_attempts: dunning.max_attempts,
                grace_ms: dunning.grace_ms,
            },
        };
        let mut stripe = faktor_payments::StripeConfig::default();
        if let Some(cfg) = &self.stripe {
            if let Some(base_url) = &cfg.base_url {
                stripe.base_url = base_url.clone();
            }
            if let Some(env) = &cfg.api_key_env {
                stripe.api_key_env = env.clone();
            }
            if let Some(env) = &cfg.webhook_secret_env {
                stripe.webhook_secret_env = env.clone();
            }
            if let Some(agent) = &cfg.user_agent {
                stripe.user_agent = agent.clone();
            }
        }
        let webhook_secret_env = self
            .webhook_secret_env
            .clone()
            .unwrap_or_else(|| DEFAULT_PAYMENTS_WEBHOOK_ENV.to_string());
        let config = faktor_payments::PaymentsConfig {
            provider,
            catalogue,
            tax,
            dunning,
            webhook_secret_env,
            stripe,
        };
        config
            .validate()
            .map_err(|e| format!("billing payments: {e}"))?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled(body: &str) -> BillingCfg {
        serde_json::from_str::<BillingCfg>(body).expect("billing section parses")
    }

    const PLAN: &str = r#""plans":{"p":{"plan_id":"p","features":["credits"],"limits":{}}}"#;

    #[test]
    fn subscription_is_optional_and_disabled_parity_is_unchanged() {
        let absent = enabled(r#"{"enabled":false}"#);
        let explicitly_disabled = enabled(r#"{"enabled":false,"subscription":null}"#);
        assert_eq!(absent, explicitly_disabled);
        assert!(absent.subscription.is_none());
        absent.validate().unwrap();
    }

    #[test]
    fn subscription_defaults_to_active_and_validates_its_plan() {
        let cfg = enabled(&format!(
            r#"{{"enabled":true,"organization":"org-live","default_plan":"p",{PLAN},"subscription":{{"plan_id":"p"}}}}"#
        ));
        let subscription = cfg.subscription.as_ref().expect("subscription");
        assert_eq!(
            subscription.status().unwrap(),
            faktor_cloud::SubscriptionStatus::Active
        );
        cfg.validate().unwrap();

        let unknown_plan = enabled(&format!(
            r#"{{"enabled":true,"organization":"org-live","default_plan":"p",{PLAN},"subscription":{{"plan_id":"missing"}}}}"#
        ));
        assert!(unknown_plan
            .validate()
            .unwrap_err()
            .contains("is not in the plans table"));

        let bad_status = enabled(&format!(
            r#"{{"enabled":true,"organization":"org-live","default_plan":"p",{PLAN},"subscription":{{"plan_id":"p","status":"paused"}}}}"#
        ));
        assert!(bad_status
            .validate()
            .unwrap_err()
            .contains("unknown status"));

        // A trialing row is bounded: an open-ended trial is refused.
        let trial_without_expiry = enabled(&format!(
            r#"{{"enabled":true,"organization":"org-live","default_plan":"p",{PLAN},"subscription":{{"plan_id":"p","status":"trialing"}}}}"#
        ));
        assert!(trial_without_expiry
            .validate()
            .unwrap_err()
            .contains("requires expires_ms"));
        let bounded_trial = enabled(&format!(
            r#"{{"enabled":true,"organization":"org-live","default_plan":"p",{PLAN},"subscription":{{"plan_id":"p","status":"trialing","expires_ms":1000}}}}"#
        ));
        bounded_trial.validate().unwrap();
        assert_eq!(
            bounded_trial.subscription.unwrap().status().unwrap(),
            faktor_cloud::SubscriptionStatus::Trialing
        );

        let negative_expiry = enabled(&format!(
            r#"{{"enabled":true,"organization":"org-live","default_plan":"p",{PLAN},"subscription":{{"plan_id":"p","expires_ms":-1}}}}"#
        ));
        assert!(negative_expiry
            .validate()
            .unwrap_err()
            .contains("expires_ms must be >= 0"));
    }

    #[test]
    fn hostile_subscription_shapes_are_strict_parse_errors() {
        for body in [
            r#"{"subscription":{"plan_id":"p","bogus":true}}"#,
            r#"{"subscription":{"plan_id":"p"},"subscription":{"plan_id":"p"}}"#,
            r#"{"subscription":"p"}"#,
        ] {
            assert!(
                serde_json::from_str::<BillingCfg>(body).is_err(),
                "must refuse {body}"
            );
        }
    }

    #[test]
    fn expired_subscription_parses_and_is_not_renewed() {
        let cfg = enabled(&format!(
            r#"{{"enabled":true,"organization":"org-live","default_plan":"p",{PLAN},"subscription":{{"plan_id":"p","status":"expired","expires_ms":1}}}}"#
        ));
        cfg.validate().unwrap();
        let subscription = cfg.subscription.as_ref().unwrap();
        assert_eq!(
            subscription.status().unwrap(),
            faktor_cloud::SubscriptionStatus::Expired
        );
    }

    #[test]
    fn payments_subsection_is_strict_and_builds_exact_runtime_config() {
        let body = r#"{"enabled":true,"organization":"org-live","default_plan":"p",
            "plans":{"p":{"plan_id":"p","features":[],"limits":{}}},
            "payments":{"enabled":true,"database":"payment.db","provider":"local",
              "webhook_secret_env":"FAKTOR_TEST_HOOK",
              "prices":{"p":{"plan_id":"p","currency":"USD","monthly_minor":1000,
                "annual_minor":10000,"per_seat_monthly_minor":250,
                "per_seat_annual_minor":2500}},
              "currencies":{"USD":2},
              "tax":{"default_jurisdiction":"DE","rates":{"DE":{"rate_bps":1900,"inclusive":true}}},
              "dunning":{"max_attempts":2,"grace_ms":1000}}}"#;
        let cfg = enabled(body);
        cfg.validate().unwrap();
        let runtime = cfg
            .payments
            .as_ref()
            .unwrap()
            .runtime_config(&cfg.plans)
            .unwrap();
        assert_eq!(runtime.provider, faktor_payments::ProviderKind::Local);
        assert_eq!(
            runtime
                .catalogue
                .price_for("p", faktor_payments::BillingInterval::Monthly)
                .unwrap(),
            1000
        );
        assert_eq!(runtime.tax.default_jurisdiction.as_deref(), Some("DE"));
        assert_eq!(
            cfg.payments_path(std::path::Path::new("/tmp")).unwrap(),
            Some(std::path::PathBuf::from("/tmp/payment.db"))
        );

        // Disabled parity: an explicitly disabled subsection is absent.
        let disabled = enabled(r#"{"enabled":false,"payments":{"enabled":false}}"#);
        assert!(disabled.payments.is_none());
        // Strict: unknown keys, duplicate FX pairs, prices naming absent plans,
        // traversal database names and negative prices are refused.
        for body in [
            r#"{"enabled":true,"organization":"o","plans":{"p":{"plan_id":"p","features":[],"limits":{}}},"payments":{"enabled":true,"bogus":1}}"#,
            r#"{"enabled":true,"organization":"o","plans":{"p":{"plan_id":"p","features":[],"limits":{}}},"payments":{"enabled":true,"database":"../x.db"}}"#,
            r#"{"enabled":true,"organization":"o","plans":{"p":{"plan_id":"p","features":[],"limits":{}}},"payments":{"enabled":true,"prices":{"p":{"plan_id":"p","currency":"USD","monthly_minor":-1,"annual_minor":1}}}}"#,
            r#"{"enabled":true,"organization":"o","plans":{"p":{"plan_id":"p","features":[],"limits":{}}},"payments":{"enabled":true,"prices":{"absent":{"plan_id":"absent","currency":"USD","monthly_minor":1,"annual_minor":1}}}}"#,
            r#"{"enabled":true,"organization":"o","plans":{"p":{"plan_id":"p","features":[],"limits":{}}},"payments":{"enabled":true,"fx":[{"from":"USD","to":"EUR","rate_bps":9000},{"from":"USD","to":"EUR","rate_bps":8000}]}}"#,
            r#"{"enabled":false,"payments":{"enabled":true}}"#,
        ] {
            let rejected = match serde_json::from_str::<BillingCfg>(body) {
                Ok(parsed) => parsed.validate().is_err(),
                Err(_) => true,
            };
            assert!(rejected, "must refuse {body}");
        }
    }

    #[test]
    fn payments_require_billing_and_stripe_uses_env_names_only() {
        let cfg = enabled(
            r#"{"enabled":true,"organization":"o","plans":{"p":{"plan_id":"p","features":[],"limits":{}}},"payments":{"enabled":true,"provider":"stripe","stripe":{"api_key_env":"MY_STRIPE_KEY","webhook_secret_env":"MY_HOOK"}}}"#,
        );
        cfg.validate().unwrap();
        let runtime = cfg.payments.unwrap().runtime_config(&cfg.plans).unwrap();
        assert_eq!(runtime.provider, faktor_payments::ProviderKind::Stripe);
        assert_eq!(runtime.stripe.api_key_env, "MY_STRIPE_KEY");
        assert_eq!(runtime.stripe.webhook_secret_env, "MY_HOOK");
        assert!(!format!("{runtime:?}").contains("sk_live"));
    }

    #[test]
    fn disabled_section_refuses_malformed_subscription_shapes_but_skips_plan_lookup() {
        // Fails if `enabled:false` short-circuits parsing (accepting an
        // unknown field or a duplicate subsection) before shape validation.
        for body in [
            r#"{"enabled":false,"subscription":{"plan_id":"absent","bogus":true}}"#,
            r#"{"enabled":false,"subscription":"absent"}"#,
            r#"{"enabled":false,"subscription":{"plan_id":1}}"#,
        ] {
            assert!(
                serde_json::from_str::<BillingCfg>(body).is_err(),
                "disabled billing must still refuse malformed shape: {body}"
            );
        }
        // Shape-only means exactly that: a disabled section naming a plan
        // absent from the table parses and validates, but never starts a
        // service (the daemon's disabled parity is tested at wiring level).
        let cfg = enabled(r#"{"enabled":false,"subscription":{"plan_id":"absent"}}"#);
        assert!(!cfg.enabled);
        cfg.validate().unwrap();
    }
}
