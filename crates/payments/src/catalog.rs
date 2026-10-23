//! The plan price catalogue, tax table, dunning policy and runtime provider
//! configuration. Every amount is an integer minor unit; every plan has ONE
//! currency. FX is opt-in: a missing explicit rate is a typed refusal, never
//! an implicit 1:1 conversion.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::PaymentError;
use crate::money::{default_exponent, valid_amount, valid_currency, MAX_CURRENCY_EXPONENT};

/// A billing interval. The period length is deterministic (30/365 days):
/// no calendar inference, matching the durable-period convention of the
/// metering plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingInterval {
    Monthly,
    Annual,
}

impl BillingInterval {
    pub const fn as_str(self) -> &'static str {
        match self {
            BillingInterval::Monthly => "monthly",
            BillingInterval::Annual => "annual",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "monthly" => BillingInterval::Monthly,
            "annual" => BillingInterval::Annual,
            _ => return None,
        })
    }

    pub const fn period_ms(self) -> i64 {
        match self {
            BillingInterval::Monthly => 30 * 24 * 60 * 60 * 1000,
            BillingInterval::Annual => 365 * 24 * 60 * 60 * 1000,
        }
    }
}

/// One plan's exact prices in exactly one currency.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanPrice {
    pub plan_id: String,
    pub currency: String,
    /// The full-period plan price in minor units.
    pub monthly_minor: i64,
    pub annual_minor: i64,
    /// The per-seat price in minor units, added to the plan price for every
    /// billed seat. Zero means seats are included in the plan.
    pub per_seat_monthly_minor: i64,
    pub per_seat_annual_minor: i64,
}

impl PlanPrice {
    pub fn price(&self, interval: BillingInterval) -> i64 {
        match interval {
            BillingInterval::Monthly => self.monthly_minor,
            BillingInterval::Annual => self.annual_minor,
        }
    }

    pub fn seat_price(&self, interval: BillingInterval) -> i64 {
        match interval {
            BillingInterval::Monthly => self.per_seat_monthly_minor,
            BillingInterval::Annual => self.per_seat_annual_minor,
        }
    }

    fn validate(&self, key: &str) -> Result<(), PaymentError> {
        if self.plan_id.is_empty() || self.plan_id.len() > 128 {
            return Err(PaymentError::Config(
                "plan price plan_id must be 1..=128 bytes".into(),
            ));
        }
        if key != self.plan_id {
            return Err(PaymentError::Config(format!(
                "price table key {key:?} does not match plan_id {:?}",
                self.plan_id
            )));
        }
        if !valid_currency(&self.currency) {
            return Err(PaymentError::Config(format!(
                "plan {} currency must be three uppercase letters",
                self.plan_id
            )));
        }
        for (name, amount) in [
            ("monthly_minor", self.monthly_minor),
            ("annual_minor", self.annual_minor),
            ("per_seat_monthly_minor", self.per_seat_monthly_minor),
            ("per_seat_annual_minor", self.per_seat_annual_minor),
        ] {
            if amount < 0 || !valid_amount(amount) {
                return Err(PaymentError::Config(format!(
                    "plan {} {name} must be a non-negative bounded minor amount",
                    self.plan_id
                )));
            }
        }
        if self.monthly_minor == 0 && self.annual_minor == 0 {
            return Err(PaymentError::Config(format!(
                "plan {} must carry a positive monthly or annual price",
                self.plan_id
            )));
        }
        Ok(())
    }
}

/// One currency's exact minor-unit exponent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CurrencySpec {
    pub exponent: u32,
}

impl CurrencySpec {
    fn validate(&self, code: &str) -> Result<(), PaymentError> {
        if !valid_currency(code) {
            return Err(PaymentError::Config(format!(
                "currency {code:?} must be three uppercase letters"
            )));
        }
        if self.exponent > MAX_CURRENCY_EXPONENT {
            return Err(PaymentError::Config(format!(
                "currency {code} exponent must be <= {MAX_CURRENCY_EXPONENT}"
            )));
        }
        Ok(())
    }
}

/// One explicit FX rate. `rate_bps = 10_000` is 1:1. The pair is directional.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FxRate {
    pub from: String,
    pub to: String,
    pub rate_bps: i64,
}

impl FxRate {
    pub fn key(&self) -> String {
        format!("{}/{}", self.from, self.to)
    }

    fn validate(&self) -> Result<(), PaymentError> {
        if !valid_currency(&self.from) || !valid_currency(&self.to) {
            return Err(PaymentError::Config(
                "FX rate currencies must be three uppercase letters".into(),
            ));
        }
        if self.from == self.to {
            return Err(PaymentError::Config(
                "an FX rate must name two distinct currencies".into(),
            ));
        }
        if self.rate_bps <= 0 {
            return Err(PaymentError::Config(
                "an FX rate must be a positive basis-point value".into(),
            ));
        }
        Ok(())
    }
}

/// The immutable price catalogue. `plans` is keyed by plan id; `currencies`
/// supplies exponents; `fx` holds the only permitted conversions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PriceCatalogue {
    #[serde(default)]
    pub plans: BTreeMap<String, PlanPrice>,
    #[serde(default)]
    pub currencies: BTreeMap<String, CurrencySpec>,
    #[serde(default)]
    pub fx: BTreeMap<String, i64>,
}

impl PriceCatalogue {
    pub fn validate(&self) -> Result<(), PaymentError> {
        if self.plans.len() > 256 {
            return Err(PaymentError::Config(
                "price catalogue carries more than 256 plans".into(),
            ));
        }
        for (key, plan) in &self.plans {
            plan.validate(key)?;
            if let Some(spec) = self.currencies.get(&plan.currency) {
                spec.validate(&plan.currency)?;
            }
        }
        for (code, spec) in &self.currencies {
            spec.validate(code)?;
        }
        for (key, rate_bps) in &self.fx {
            let Some((from, to)) = key.split_once('/') else {
                return Err(PaymentError::Config(format!(
                    "FX key {key:?} must be FROM/TO"
                )));
            };
            let rate = FxRate {
                from: from.to_string(),
                to: to.to_string(),
                rate_bps: *rate_bps,
            };
            rate.validate()?;
            if rate.key() != *key {
                return Err(PaymentError::Config(format!(
                    "FX key {key:?} is not in canonical FROM/TO form"
                )));
            }
        }
        Ok(())
    }

    pub fn plan(&self, plan_id: &str) -> Option<&PlanPrice> {
        self.plans.get(plan_id)
    }

    pub fn price_for(&self, plan_id: &str, interval: BillingInterval) -> Result<i64, PaymentError> {
        self.plan(plan_id)
            .map(|plan| plan.price(interval))
            .ok_or_else(|| PaymentError::PlanPriceUnconfigured(plan_id.to_string()))
    }

    pub fn seat_price_for(
        &self,
        plan_id: &str,
        interval: BillingInterval,
    ) -> Result<i64, PaymentError> {
        self.plan(plan_id)
            .map(|plan| plan.seat_price(interval))
            .ok_or_else(|| PaymentError::PlanPriceUnconfigured(plan_id.to_string()))
    }

    pub fn currency_of(&self, plan_id: &str) -> Result<&str, PaymentError> {
        self.plan(plan_id)
            .map(|plan| plan.currency.as_str())
            .ok_or_else(|| PaymentError::PlanPriceUnconfigured(plan_id.to_string()))
    }

    pub fn exponent(&self, currency: &str) -> u32 {
        self.currencies
            .get(currency)
            .map(|spec| spec.exponent)
            .unwrap_or_else(|| default_exponent(currency))
    }

    /// Convert only through an explicitly configured rate.
    pub fn convert(&self, amount_minor: i64, from: &str, to: &str) -> Result<i64, PaymentError> {
        if from == to {
            return Ok(amount_minor);
        }
        let key = format!("{from}/{to}");
        let Some(rate_bps) = self.fx.get(&key) else {
            return Err(PaymentError::FxUnconfigured(
                from.to_string(),
                to.to_string(),
            ));
        };
        crate::money::convert(amount_minor, *rate_bps)
    }

    /// The exponent the plan's currency uses.
    pub fn plan_exponent(&self, plan_id: &str) -> Result<u32, PaymentError> {
        let currency = self.currency_of(plan_id)?;
        Ok(self.exponent(currency))
    }
}

/// One tax rate. `inclusive` means the configured price already contains the
/// tax; `exclusive` adds it on top.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaxRate {
    pub rate_bps: i64,
    pub inclusive: bool,
}

/// The configured tax table plus the default jurisdiction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct TaxConfig {
    #[serde(default)]
    pub default_jurisdiction: Option<String>,
    #[serde(default)]
    pub rates: BTreeMap<String, TaxRate>,
}

impl TaxConfig {
    pub fn validate(&self) -> Result<(), PaymentError> {
        if let Some(default) = &self.default_jurisdiction {
            if default.is_empty() || default.len() > 64 {
                return Err(PaymentError::Config(
                    "tax default_jurisdiction must be 1..=64 bytes".into(),
                ));
            }
            if !self.rates.contains_key(default) {
                return Err(PaymentError::Config(format!(
                    "tax default jurisdiction {default:?} is not in the rate table"
                )));
            }
        }
        if self.rates.len() > 256 {
            return Err(PaymentError::Config(
                "tax table carries more than 256 jurisdictions".into(),
            ));
        }
        for (jurisdiction, rate) in &self.rates {
            if jurisdiction.is_empty() || jurisdiction.len() > 64 {
                return Err(PaymentError::Config(
                    "tax jurisdiction must be 1..=64 bytes".into(),
                ));
            }
            if !(0..=10_000).contains(&rate.rate_bps) {
                return Err(PaymentError::Config(format!(
                    "tax jurisdiction {jurisdiction} rate must be 0..=10000 basis points"
                )));
            }
        }
        Ok(())
    }

    pub fn is_configured(&self) -> bool {
        !self.rates.is_empty()
    }

    pub fn rate_for(&self, jurisdiction: Option<&str>) -> Result<Option<&TaxRate>, PaymentError> {
        let jurisdiction = jurisdiction.or(self.default_jurisdiction.as_deref());
        match jurisdiction {
            None => Ok(None),
            Some(jurisdiction) => self
                .rates
                .get(jurisdiction)
                .map(Some)
                .ok_or_else(|| PaymentError::TaxUnconfigured(jurisdiction.to_string())),
        }
    }
}

/// Dunning policy: how many failed attempts are tolerated and how long the
/// grace window lasts before a past-due subscription is canceled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DunningConfig {
    pub max_attempts: u32,
    pub grace_ms: i64,
}

impl Default for DunningConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            grace_ms: 7 * 24 * 60 * 60 * 1000,
        }
    }
}

impl DunningConfig {
    pub fn validate(&self) -> Result<(), PaymentError> {
        if !(1..=10).contains(&self.max_attempts) {
            return Err(PaymentError::Config(
                "dunning max_attempts must be 1..=10".into(),
            ));
        }
        if !(0..=90 * 24 * 60 * 60 * 1000).contains(&self.grace_ms) {
            return Err(PaymentError::Config(
                "dunning grace_ms must be 0..=90 days".into(),
            ));
        }
        Ok(())
    }
}

/// The selected payment provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Local,
    Stripe,
}

impl ProviderKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            ProviderKind::Local => "local",
            ProviderKind::Stripe => "stripe",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw {
            "local" => ProviderKind::Local,
            "stripe" => ProviderKind::Stripe,
            _ => return None,
        })
    }
}

/// The Stripe-compatible adapter configuration. Only environment-variable
/// NAMES live here; values are resolved through [`crate::secrets`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StripeConfig {
    pub base_url: String,
    pub api_key_env: String,
    pub webhook_secret_env: String,
    #[serde(default = "default_stripe_user_agent")]
    pub user_agent: String,
}

fn default_stripe_user_agent() -> String {
    "faktor-payments/0.1".to_string()
}

impl Default for StripeConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.stripe.com".to_string(),
            api_key_env: "FAKTOR_STRIPE_SECRET_KEY".to_string(),
            webhook_secret_env: "FAKTOR_STRIPE_WEBHOOK_SECRET".to_string(),
            user_agent: default_stripe_user_agent(),
        }
    }
}

impl StripeConfig {
    pub fn validate(&self) -> Result<(), PaymentError> {
        let base = self.base_url.trim_end_matches('/');
        if !(base.starts_with("https://") || base.starts_with("http://")) {
            return Err(PaymentError::Config(
                "stripe base_url must be an absolute http(s) URL".into(),
            ));
        }
        if base.contains(char::is_whitespace) || base.contains('@') {
            return Err(PaymentError::Config(
                "stripe base_url must not contain whitespace or userinfo".into(),
            ));
        }
        for (name, env) in [
            ("api_key_env", &self.api_key_env),
            ("webhook_secret_env", &self.webhook_secret_env),
        ] {
            if env.is_empty() || env.len() > 128 {
                return Err(PaymentError::Config(format!(
                    "stripe {name} must name a bounded environment variable"
                )));
            }
        }
        if self.user_agent.is_empty() || self.user_agent.len() > 256 {
            return Err(PaymentError::Config(
                "stripe user_agent must be 1..=256 bytes".into(),
            ));
        }
        Ok(())
    }
}

/// The complete runtime payment configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PaymentsConfig {
    pub provider: ProviderKind,
    pub catalogue: PriceCatalogue,
    #[serde(default)]
    pub tax: TaxConfig,
    #[serde(default)]
    pub dunning: DunningConfig,
    pub webhook_secret_env: String,
    #[serde(default)]
    pub stripe: StripeConfig,
}

impl PaymentsConfig {
    pub fn validate(&self) -> Result<(), PaymentError> {
        self.catalogue.validate()?;
        self.tax.validate()?;
        self.dunning.validate()?;
        if self.webhook_secret_env.is_empty() || self.webhook_secret_env.len() > 128 {
            return Err(PaymentError::Config(
                "webhook_secret_env must name a bounded environment variable".into(),
            ));
        }
        if self.provider == ProviderKind::Stripe {
            self.stripe.validate()?;
        }
        Ok(())
    }

    pub fn local() -> Self {
        Self {
            provider: ProviderKind::Local,
            catalogue: PriceCatalogue::default(),
            tax: TaxConfig::default(),
            dunning: DunningConfig::default(),
            webhook_secret_env: "FAKTOR_BILLING_WEBHOOK_SECRET".to_string(),
            stripe: StripeConfig::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::money::{convert, format_minor};

    fn price(plan: &str, currency: &str) -> PlanPrice {
        PlanPrice {
            plan_id: plan.into(),
            currency: currency.into(),
            monthly_minor: 1_000,
            annual_minor: 10_000,
            per_seat_monthly_minor: 250,
            per_seat_annual_minor: 2_500,
        }
    }

    fn catalogue() -> PriceCatalogue {
        let mut currencies = BTreeMap::new();
        currencies.insert("USD".to_string(), CurrencySpec { exponent: 2 });
        currencies.insert("JPY".to_string(), CurrencySpec { exponent: 0 });
        let mut plans = BTreeMap::new();
        plans.insert("pro".to_string(), price("pro", "USD"));
        plans.insert("local".to_string(), price("local", "JPY"));
        PriceCatalogue {
            plans,
            currencies,
            fx: [("USD/JPY".to_string(), 15_000)].into_iter().collect(),
        }
    }

    #[test]
    fn catalogue_is_exact_and_currency_is_per_plan() {
        let c = catalogue();
        c.validate().unwrap();
        assert_eq!(c.price_for("pro", BillingInterval::Monthly).unwrap(), 1_000);
        assert_eq!(c.price_for("pro", BillingInterval::Annual).unwrap(), 10_000);
        assert_eq!(
            c.seat_price_for("pro", BillingInterval::Monthly).unwrap(),
            250
        );
        assert_eq!(c.currency_of("pro").unwrap(), "USD");
        assert_eq!(c.currency_of("local").unwrap(), "JPY");
        assert_eq!(format_minor(1_000, "USD", c.exponent("USD")), "10.00 USD");
        assert_eq!(format_minor(1_000, "JPY", c.exponent("JPY")), "1000 JPY");
        assert_eq!(c.convert(1_000, "USD", "JPY").unwrap(), 1_500);
        assert_eq!(c.convert(1_000, "USD", "USD").unwrap(), 1_000);
        assert_eq!(
            c.convert(1_000, "JPY", "USD"),
            Err(PaymentError::FxUnconfigured("JPY".into(), "USD".into()))
        );
    }

    #[test]
    fn hostile_prices_and_fx_are_refused() {
        let mut c = catalogue();
        c.plans.get_mut("pro").unwrap().currency = "usd".into();
        assert!(c.validate().is_err());
        let mut c = catalogue();
        c.plans.get_mut("pro").unwrap().monthly_minor = -1;
        assert!(c.validate().is_err());
        let mut c = catalogue();
        c.fx.insert("USD/JPY".into(), 0);
        assert!(c.validate().is_err());
        let mut c = catalogue();
        c.plans.insert("ghost".into(), price("other", "USD"));
        assert!(c.validate().is_err());
        let mut c = catalogue();
        c.plans.get_mut("pro").unwrap().monthly_minor = crate::money::MAX_MONEY_MINOR + 1;
        assert!(c.validate().is_err());
    }

    #[test]
    fn tax_config_is_per_jurisdiction_and_inclusive_aware() {
        let tax = TaxConfig {
            default_jurisdiction: Some("DE".into()),
            rates: [
                (
                    "DE".into(),
                    TaxRate {
                        rate_bps: 1_900,
                        inclusive: true,
                    },
                ),
                (
                    "US-CA".into(),
                    TaxRate {
                        rate_bps: 825,
                        inclusive: false,
                    },
                ),
            ]
            .into_iter()
            .collect(),
        };
        tax.validate().unwrap();
        assert!(tax.rate_for(None).unwrap().unwrap().inclusive);
        assert!(!tax.rate_for(Some("US-CA")).unwrap().unwrap().inclusive);
        assert_eq!(
            tax.rate_for(Some("FR")),
            Err(PaymentError::TaxUnconfigured("FR".into()))
        );
        assert_eq!(crate::money::exclusive_tax(10_000, 825), Ok(825));
        assert_eq!(crate::money::inclusive_tax(11_900, 1_900), Ok(1_900));
        let mut bad = tax;
        bad.rates.get_mut("DE").unwrap().rate_bps = 10_001;
        assert!(bad.validate().is_err());
    }

    #[test]
    fn explicit_conversion_is_checked() {
        assert_eq!(convert(333, 15_000).unwrap(), 500);
        assert!(convert(1, 0).is_err());
    }
}
