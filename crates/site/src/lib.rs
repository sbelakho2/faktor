#![deny(unsafe_code)]

pub mod auth;
pub mod billing;
pub mod config;
pub mod db;
pub mod error;
pub mod mail;
pub mod routes;
pub mod security;
pub mod web;

#[cfg(test)]
mod checkout_fence_tests;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use axum::http::HeaderMap;
use faktor_cloud::{Clock, SystemClock};
use faktor_payments::{PaymentService, PaymentsConfig, ProviderKind, SqlitePaymentStore};
use faktor_provider::egress::PolicyCheckedHttpTransport;
use faktor_security::destination::DestinationPolicy;

use crate::auth::AuthSession;
use crate::config::Config;
use crate::db::Db;
use crate::error::{SiteError, SiteResult};
use crate::mail::{FileMailer, Mailer};
use crate::security::{
    client_ip, cookie_value, hash_secret, random_secret, CSRF_COOKIE, SESSION_COOKIE,
};

pub struct AppState {
    pub config: Arc<Config>,
    pub db: Arc<Db>,
    pub clock: Arc<dyn Clock>,
    pub mailer: Arc<dyn Mailer>,
    pub limiter: security::RateLimiter,
    pub payments: Arc<PaymentService>,
    /// Checkout fences owned by THIS process. A durable pending fence absent
    /// from this set survived a crash and is reconciled immediately.
    pub checkout_fences: Mutex<HashSet<String>>,
}

impl AppState {
    pub fn new(
        config: Arc<Config>,
        db: Arc<Db>,
        payments: Arc<PaymentService>,
        mailer: Arc<dyn Mailer>,
        clock: Arc<dyn Clock>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            db,
            clock,
            mailer,
            limiter: security::RateLimiter::new(),
            payments,
            checkout_fences: Mutex::new(HashSet::new()),
        })
    }

    pub fn build(config: Config) -> SiteResult<Arc<Self>> {
        config.ensure_directories()?;
        let db = Arc::new(Db::open(&config.site_db_path())?);
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let payments = Arc::new(build_payment_service(&config, clock.clone())?);
        let mailer: Arc<dyn Mailer> = Arc::new(FileMailer::new(config.mail_dir())?);
        let config = Arc::new(config);
        Ok(Self::new(config, db, payments, mailer, clock))
    }

    pub fn checkout_fence_is_live(&self, fence_id: &str) -> bool {
        self.checkout_fences
            .lock()
            .map(|owners| owners.contains(fence_id))
            .unwrap_or(false)
    }

    pub fn own_checkout_fence(&self, fence_id: &str) {
        if let Ok(mut owners) = self.checkout_fences.lock() {
            owners.insert(fence_id.to_string());
        }
    }

    pub fn release_checkout_fence(&self, fence_id: &str) {
        if let Ok(mut owners) = self.checkout_fences.lock() {
            owners.remove(fence_id);
        }
    }

    pub fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }

    pub fn auth_from(&self, headers: &HeaderMap) -> SiteResult<Option<AuthSession>> {
        let Some(raw) = cookie_value(headers, SESSION_COOKIE) else {
            return Ok(None);
        };
        let conn = self.db.conn()?;
        auth::load_session(&conn, &raw, self.now_ms(), self.config.idle_ttl_ms)
    }

    pub fn require_auth(&self, headers: &HeaderMap) -> SiteResult<AuthSession> {
        self.auth_from(headers)?.ok_or(SiteError::Unauthorized)
    }

    pub fn csrf_cookie_token(
        &self,
        headers: &HeaderMap,
        auth: Option<&AuthSession>,
    ) -> (String, Vec<String>) {
        if let Some(auth) = auth {
            return (auth.csrf.clone(), Vec::new());
        }
        if let Some(value) = cookie_value(headers, CSRF_COOKIE) {
            if security::valid_secret_shape(&value) {
                return (value, Vec::new());
            }
        }
        let value = random_secret();
        let cookie = security::csrf_cookie(&self.config, &value);
        (value, vec![cookie])
    }

    pub fn verify_csrf(
        &self,
        headers: &HeaderMap,
        auth: Option<&AuthSession>,
        submitted: &str,
    ) -> SiteResult<()> {
        if !security::same_origin(headers, &self.config) {
            return Err(SiteError::Forbidden);
        }
        if !security::valid_secret_shape(submitted) {
            return Err(SiteError::Forbidden);
        }
        let expected = match auth {
            Some(auth) => auth.csrf.clone(),
            None => cookie_value(headers, CSRF_COOKIE).ok_or(SiteError::Forbidden)?,
        };
        if !security::constant_time_eq(submitted.as_bytes(), expected.as_bytes()) {
            return Err(SiteError::Forbidden);
        }
        Ok(())
    }

    pub fn limit(&self, key: &str, limit: u32, window_ms: i64) -> SiteResult<()> {
        self.limiter.check(key, limit, window_ms, self.now_ms())
    }

    pub fn ip_key(&self, headers: &HeaderMap, scope: &str) -> String {
        format!("{scope}:{}", client_ip(headers, &self.config))
    }

    pub fn email_key(&self, scope: &str, email: &str) -> String {
        format!("{scope}:{}", &hash_secret(email)[..24])
    }
}

fn build_payment_service(config: &Config, clock: Arc<dyn Clock>) -> SiteResult<PaymentService> {
    let store = Arc::new(SqlitePaymentStore::open(&config.payment_db)?);
    let secrets = config.payment_secrets();
    let service = match config.billing_provider {
        ProviderKind::Local => {
            PaymentService::open_local(store, config.payments.clone(), clock, secrets)?
        }
        ProviderKind::Mollie => {
            let transport = mollie_transport(&config.payments)?;
            PaymentService::open_mollie(
                store,
                config.payments.clone(),
                clock,
                secrets,
                Some(transport),
            )?
        }
        ProviderKind::Stripe => {
            return Err(SiteError::internal_message(
                "the website supports local and Mollie billing providers",
            ))
        }
    };
    Ok(service)
}

fn mollie_transport(
    config: &PaymentsConfig,
) -> SiteResult<Arc<dyn faktor_provider::egress::HttpTransport>> {
    let url = url::Url::parse(&config.mollie.base_url)
        .map_err(|_| SiteError::internal_message("Mollie base URL is invalid"))?;
    let host = url
        .host_str()
        .ok_or_else(|| SiteError::internal_message("Mollie base URL has no host"))?;
    let rule = match url.scheme() {
        "https" => format!("https://{host}"),
        "http" => format!("http://{host}"),
        _ => {
            return Err(SiteError::internal_message(
                "Mollie base URL must use http or https",
            ))
        }
    };
    let policy = DestinationPolicy::parse_lines([rule])
        .map_err(|error| SiteError::internal_message(format!("Mollie egress policy: {error}")))?;
    let transport = PolicyCheckedHttpTransport::try_with_policy(policy).map_err(|error| {
        SiteError::internal_message(format!("Mollie egress transport: {error}"))
    })?;
    Ok(Arc::new(transport))
}

pub fn app(state: Arc<AppState>) -> axum::Router {
    routes::router(state)
}
