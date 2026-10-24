use std::path::{Path, PathBuf};
use std::sync::Arc;

use faktor_payments::{
    BillingInterval, CurrencySpec, MollieConfig, PaymentsConfig, PlanPrice, PriceCatalogue,
    ProviderKind, SecretString, StaticSecrets, TaxConfig, TaxRate,
};
use thiserror::Error;

const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8080";
const DEFAULT_BIND: &str = "127.0.0.1:8080";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid configuration: {0}")]
    Invalid(String),
    #[error("configuration I/O failed")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    pub static_dir: PathBuf,
    pub payment_db: PathBuf,
    pub base_url: String,
    pub bind: String,
    pub cookie_secure: bool,
    pub trust_proxy: bool,
    pub admin_email: Option<String>,
    pub session_ttl_ms: i64,
    pub idle_ttl_ms: i64,
    pub billing_provider: ProviderKind,
    pub payments: PaymentsConfig,
    pub local_webhook_secret: Option<SecretString>,
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let data_dir = std::env::var("FAKTOR_SITE_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("data/site"));
        let static_dir = std::env::var("FAKTOR_SITE_STATIC_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("site/public"));
        let base_url =
            std::env::var("FAKTOR_SITE_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
        let bind = std::env::var("FAKTOR_SITE_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_string());
        let billing_provider = match std::env::var("FAKTOR_SITE_BILLING_PROVIDER")
            .unwrap_or_else(|_| "local".into())
            .as_str()
        {
            "local" => ProviderKind::Local,
            "mollie" => ProviderKind::Mollie,
            other => {
                return Err(ConfigError::Invalid(format!(
                    "unknown billing provider {other:?}; expected local or mollie"
                )))
            }
        };
        let cookie_secure = std::env::var("FAKTOR_SITE_COOKIE_SECURE")
            .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
            .unwrap_or_else(|_| base_url.starts_with("https://"));
        let trust_proxy = std::env::var("FAKTOR_SITE_TRUST_PROXY")
            .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let admin_email = std::env::var("FAKTOR_SITE_ADMIN_EMAIL")
            .ok()
            .map(|email| normalize_email(&email))
            .filter(|email| !email.is_empty());
        let local_secret = match std::env::var("FAKTOR_SITE_WEBHOOK_SECRET") {
            Ok(secret) if !secret.is_empty() => Some(SecretString::new(secret)),
            _ if billing_provider == ProviderKind::Local => {
                let generated = random_secret();
                Some(SecretString::new(generated))
            }
            _ => None,
        };
        let payments = build_payments_config(billing_provider)?;
        let config = Self {
            data_dir: data_dir.clone(),
            static_dir,
            payment_db: data_dir.join("payments.db"),
            base_url: validate_base_url(&base_url)?,
            bind,
            cookie_secure,
            trust_proxy,
            admin_email,
            session_ttl_ms: 30 * 24 * 60 * 60 * 1000,
            idle_ttl_ms: 7 * 24 * 60 * 60 * 1000,
            billing_provider,
            payments,
            local_webhook_secret: local_secret,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        validate_base_url(&self.base_url)?;
        if self.data_dir.as_os_str().is_empty() {
            return Err(ConfigError::Invalid("data directory is empty".into()));
        }
        if self.static_dir.as_os_str().is_empty() {
            return Err(ConfigError::Invalid("static directory is empty".into()));
        }
        if self.session_ttl_ms <= 0
            || self.idle_ttl_ms <= 0
            || self.idle_ttl_ms > self.session_ttl_ms
        {
            return Err(ConfigError::Invalid(
                "session lifetime bounds are invalid".into(),
            ));
        }
        if self.billing_provider == ProviderKind::Mollie
            && std::env::var(&self.payments.mollie.api_key_env).is_err()
        {
            tracing::warn!(
                "Mollie is selected but its API-key environment variable is unset; paid operations will refuse typed"
            );
        }
        if self.billing_provider == ProviderKind::Local {
            let local_origin = self.base_url.starts_with("http://127.0.0.1")
                || self.base_url.starts_with("http://localhost")
                || self.base_url.starts_with("http://[::1]");
            if !local_origin {
                tracing::warn!(
                    base_url = %self.base_url,
                    "the local billing provider is enabled for a non-loopback origin; use FAKTOR_SITE_BILLING_PROVIDER=mollie in production"
                );
            }
        }
        Ok(())
    }

    pub fn payment_secrets(&self) -> Arc<StaticSecrets> {
        let mut values = Vec::new();
        if let Some(secret) = &self.local_webhook_secret {
            values.push((
                self.payments.webhook_secret_env.clone(),
                secret.expose().to_string(),
            ));
        }
        if let Ok(value) = std::env::var(&self.payments.mollie.api_key_env) {
            values.push((self.payments.mollie.api_key_env.clone(), value));
        }
        if let Ok(value) = std::env::var(&self.payments.vault_key_env) {
            values.push((self.payments.vault_key_env.clone(), value));
        }
        Arc::new(StaticSecrets::new(values))
    }

    pub fn site_db_path(&self) -> PathBuf {
        self.data_dir.join("site.db")
    }

    pub fn mail_dir(&self) -> PathBuf {
        self.data_dir.join("outbox")
    }

    pub fn base_origin(&self) -> String {
        let without_path = self.base_url.trim_end_matches('/');
        match without_path.find("://") {
            Some(index) => match without_path[index + 3..].find('/') {
                Some(path) => without_path[..index + 3 + path].to_string(),
                None => without_path.to_string(),
            },
            None => without_path.to_string(),
        }
    }

    pub fn base_path(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.base_url.trim_end_matches('/'),
            path.trim_start_matches('/')
        )
    }

    pub fn ensure_directories(&self) -> Result<(), ConfigError> {
        std::fs::create_dir_all(&self.data_dir)?;
        std::fs::create_dir_all(self.mail_dir())?;
        restrict_directory(&self.data_dir)?;
        Ok(())
    }
}

pub fn build_payments_config(provider: ProviderKind) -> Result<PaymentsConfig, ConfigError> {
    let mut plans = std::collections::BTreeMap::new();
    for (id, monthly, annual) in [
        ("solo", 1_900_i64, 19_000_i64),
        ("team", 4_900_i64, 49_000_i64),
        ("enterprise", 19_900_i64, 199_000_i64),
    ] {
        plans.insert(
            id.to_string(),
            PlanPrice {
                plan_id: id.to_string(),
                currency: "EUR".to_string(),
                monthly_minor: monthly,
                annual_minor: annual,
                per_seat_monthly_minor: 0,
                per_seat_annual_minor: 0,
            },
        );
    }
    let mut currencies = std::collections::BTreeMap::new();
    currencies.insert("EUR".to_string(), CurrencySpec { exponent: 2 });
    let mut rates = std::collections::BTreeMap::new();
    rates.insert(
        "NL".to_string(),
        TaxRate {
            rate_bps: 2_100,
            inclusive: false,
        },
    );
    let public_base =
        std::env::var("FAKTOR_SITE_BASE_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".into());
    let mollie_base = std::env::var("FAKTOR_MOLLIE_BASE_URL")
        .unwrap_or_else(|_| "https://api.mollie.com/v2".to_string());
    let config = PaymentsConfig {
        provider,
        catalogue: PriceCatalogue {
            plans,
            currencies,
            fx: std::collections::BTreeMap::new(),
        },
        tax: TaxConfig {
            default_jurisdiction: Some("NL".to_string()),
            rates,
        },
        dunning: Default::default(),
        webhook_secret_env: "FAKTOR_SITE_WEBHOOK_SECRET".to_string(),
        vault_key_env: "FAKTOR_SITE_PAYMENT_VAULT_KEY".to_string(),
        stripe: Default::default(),
        mollie: MollieConfig {
            base_url: mollie_base,
            api_key_env: "FAKTOR_MOLLIE_API_KEY".to_string(),
            webhook_url: Some(format!(
                "{}/webhooks/payments",
                public_base.trim_end_matches('/')
            )),
            user_agent: "faktor-site/0.1".to_string(),
        },
    };
    config
        .validate()
        .map_err(|error| ConfigError::Invalid(format!("billing configuration: {error}")))?;
    Ok(config)
}

pub fn interval_name(interval: BillingInterval) -> &'static str {
    match interval {
        BillingInterval::Monthly => "monthly",
        BillingInterval::Annual => "annual",
    }
}

pub fn parse_interval(raw: &str) -> Option<BillingInterval> {
    match raw {
        "monthly" => Some(BillingInterval::Monthly),
        "annual" => Some(BillingInterval::Annual),
        _ => None,
    }
}

pub fn normalize_email(raw: &str) -> String {
    raw.trim().to_ascii_lowercase()
}

fn validate_base_url(raw: &str) -> Result<String, ConfigError> {
    let parsed = url::Url::parse(raw)
        .map_err(|_| ConfigError::Invalid("base URL must be an absolute http(s) URL".into()))?;
    if parsed.scheme() != "https" && parsed.scheme() != "http" {
        return Err(ConfigError::Invalid(
            "base URL must use http or https".into(),
        ));
    }
    if parsed.host_str().is_none() || parsed.username() != "" || parsed.password().is_some() {
        return Err(ConfigError::Invalid(
            "base URL must carry a host and no userinfo".into(),
        ));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(ConfigError::Invalid(
            "base URL must not contain a query or fragment".into(),
        ));
    }
    let mut normalized = parsed.to_string();
    while normalized.ends_with('/') {
        normalized.pop();
    }
    Ok(normalized)
}

fn random_secret() -> String {
    use base64::Engine;
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn restrict_directory(path: &Path) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
