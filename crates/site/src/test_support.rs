#![allow(dead_code)]
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::Router;
use faktor_cloud::{Clock, ManualClock};
use faktor_payments::{
    PaymentProvider, PaymentService, SecretString, SqlitePaymentStore, StaticSecrets,
};
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;

use crate::config::{build_payments_config, Config};
use crate::db::Db;
use crate::mail::{MailMessage, MemoryMailer};
use crate::{app, AppState};

pub const TEST_NOW_MS: i64 = 1_792_800_000_000;
pub const TEST_SECRET: &str = "faktor-test-webhook-secret";

pub fn test_config() -> Config {
    let payments = build_payments_config(faktor_payments::ProviderKind::Local).unwrap();
    Config {
        data_dir: std::env::temp_dir().join("faktor-site-config-test"),
        static_dir: std::path::PathBuf::from("site/public"),
        payment_db: std::env::temp_dir().join("faktor-site-config-test/payments.db"),
        base_url: "http://127.0.0.1:8080".into(),
        bind: "127.0.0.1:0".into(),
        cookie_secure: false,
        trusted_proxy_cidrs: vec![
            "127.0.0.1/32".parse().unwrap(),
            "10.0.0.0/8".parse().unwrap(),
            "::1/128".parse().unwrap(),
        ],
        admin_email: Some("admin@example.com".into()),
        session_ttl_ms: 30 * 24 * 60 * 60 * 1000,
        idle_ttl_ms: 7 * 24 * 60 * 60 * 1000,
        billing_provider: faktor_payments::ProviderKind::Local,
        payments,
        local_webhook_secret: Some(SecretString::new(TEST_SECRET)),
    }
}

type CustomPaymentProviderFactory = Box<
    dyn FnOnce(
        Arc<SqlitePaymentStore>,
        Arc<ManualClock>,
        Arc<StaticSecrets>,
    ) -> Arc<dyn PaymentProvider>,
>;

pub struct TestApp {
    #[allow(dead_code)]
    pub dir: TempDir,
    pub state: Arc<AppState>,
    pub mailer: Arc<MemoryMailer>,
    #[allow(dead_code)]
    pub clock: Arc<ManualClock>,
    pub router: Router,
}

impl TestApp {
    pub fn build() -> Self {
        Self::build_with_provider(faktor_payments::ProviderKind::Local, None)
    }

    pub fn build_with_provider(
        provider: faktor_payments::ProviderKind,
        transport: Option<Arc<dyn faktor_provider::egress::HttpTransport>>,
    ) -> Self {
        Self::build_inner(provider, transport, None)
    }

    pub fn build_with_mollie(
        transport: Arc<dyn faktor_provider::egress::HttpTransport>,
        base_url: String,
    ) -> Self {
        Self::build_inner(
            faktor_payments::ProviderKind::Mollie,
            Some(transport),
            Some(base_url),
        )
    }

    pub fn build_with_custom_provider<F>(factory: F) -> Self
    where
        F: FnOnce(
                Arc<SqlitePaymentStore>,
                Arc<ManualClock>,
                Arc<StaticSecrets>,
            ) -> Arc<dyn PaymentProvider>
            + 'static,
    {
        Self::build_inner_with(
            faktor_payments::ProviderKind::Local,
            None,
            None,
            Some(Box::new(factory)),
        )
    }

    fn build_inner(
        provider: faktor_payments::ProviderKind,
        transport: Option<Arc<dyn faktor_provider::egress::HttpTransport>>,
        mollie_base_url: Option<String>,
    ) -> Self {
        Self::build_inner_with(provider, transport, mollie_base_url, None)
    }

    fn build_inner_with(
        provider: faktor_payments::ProviderKind,
        transport: Option<Arc<dyn faktor_provider::egress::HttpTransport>>,
        mollie_base_url: Option<String>,
        factory: Option<CustomPaymentProviderFactory>,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut config = test_config();
        config.data_dir = dir.path().to_path_buf();
        config.payment_db = dir.path().join("payments.db");
        config.billing_provider = provider;
        config.payments = build_payments_config(provider).unwrap();
        if let Some(base_url) = mollie_base_url {
            config.payments.mollie.base_url = base_url;
        }
        let clock = Arc::new(ManualClock::new(TEST_NOW_MS));
        let store = Arc::new(SqlitePaymentStore::open(&config.payment_db).unwrap());
        let secrets = Arc::new(StaticSecrets::new([
            ("FAKTOR_SITE_WEBHOOK_SECRET".into(), TEST_SECRET.into()),
            ("FAKTOR_MOLLIE_API_KEY".into(), "test_mollie_key".into()),
            ("FAKTOR_SITE_PAYMENT_VAULT_KEY".into(), "11".repeat(32)),
        ]));
        let service_clock: Arc<dyn Clock> = clock.clone();
        let payments = match factory {
            Some(factory) => {
                let instance = factory(store.clone(), clock.clone(), secrets.clone());
                PaymentService::with_provider(
                    store,
                    instance,
                    config.payments.clone(),
                    service_clock,
                    secrets,
                )
                .unwrap()
            }
            None => match provider {
                faktor_payments::ProviderKind::Local => PaymentService::open_local(
                    store,
                    config.payments.clone(),
                    service_clock,
                    secrets,
                )
                .unwrap(),
                faktor_payments::ProviderKind::Mollie => {
                    let transport = transport.expect("Mollie tests inject a mock transport");
                    PaymentService::open_mollie(
                        store,
                        config.payments.clone(),
                        service_clock,
                        secrets,
                        Some(transport),
                    )
                    .unwrap()
                }
                faktor_payments::ProviderKind::Stripe => panic!("site tests do not use Stripe"),
            },
        };
        let db = Arc::new(Db::open(&dir.path().join("site.db")).unwrap());
        let mailer = Arc::new(MemoryMailer::new());
        let state = AppState::new(
            Arc::new(config),
            db,
            Arc::new(payments),
            mailer.clone(),
            clock.clone(),
        );
        let router = app(state.clone());
        Self {
            dir,
            state,
            mailer,
            clock,
            router,
        }
    }

    pub fn client(&self) -> Client {
        Client {
            router: self.router.clone(),
            jar: HashMap::new(),
            peer: None,
            forwarded_for: None,
        }
    }

    #[allow(dead_code)]
    pub fn payments(&self) -> &Arc<PaymentService> {
        &self.state.payments
    }

    pub fn org_for_email(&self, email: &str) -> String {
        let conn = self.state.db.conn().unwrap();
        conn.query_row(
            "SELECT m.organization_id FROM memberships m JOIN users u ON u.id=m.user_id WHERE u.email=?1 LIMIT 1",
            rusqlite::params![email],
            |row| row.get(0),
        )
        .unwrap()
    }

    pub fn user_id_for_email(&self, email: &str) -> String {
        let conn = self.state.db.conn().unwrap();
        conn.query_row(
            "SELECT id FROM users WHERE email=?1",
            rusqlite::params![email],
            |row| row.get(0),
        )
        .unwrap()
    }

    pub fn all_mails(&self) -> Vec<MailMessage> {
        self.mailer.messages()
    }
}

pub struct TestClientResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: String,
}

#[derive(Clone)]
pub struct Client {
    router: Router,
    jar: HashMap<String, String>,
    peer: Option<SocketAddr>,
    forwarded_for: Option<String>,
}

impl Client {
    #[allow(dead_code)]
    pub fn with_peer(mut self, peer: SocketAddr) -> Self {
        self.peer = Some(peer);
        self
    }

    #[allow(dead_code)]
    pub fn with_forwarded_for(mut self, value: &str) -> Self {
        self.forwarded_for = Some(value.to_string());
        self
    }

    fn forwarded_for(&self) -> &str {
        self.forwarded_for.as_deref().unwrap_or("203.0.113.9")
    }

    pub async fn get(&mut self, path: &str) -> TestClientResponse {
        self.request(Method::GET, path, None).await
    }

    pub async fn post(&mut self, path: &str, form: &[(&str, &str)]) -> TestClientResponse {
        let encoded = form
            .iter()
            .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
            .collect::<Vec<_>>()
            .join("&");
        self.request(Method::POST, path, Some(encoded)).await
    }

    pub async fn post_raw(
        &mut self,
        path: &str,
        body: &str,
        content_type: &str,
    ) -> TestClientResponse {
        let mut request = Request::builder()
            .method(Method::POST)
            .uri(path)
            .header(header::CONTENT_TYPE, content_type)
            .header("x-forwarded-for", self.forwarded_for())
            .header("origin", "http://127.0.0.1:8080")
            .header("sec-fetch-site", "same-origin");
        if !self.jar.is_empty() {
            request = request.header(
                header::COOKIE,
                self.jar
                    .iter()
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect::<Vec<_>>()
                    .join("; "),
            );
        }
        let request = request.body(Body::from(body.to_string())).unwrap();
        self.send(request).await
    }

    pub async fn post_raw_with(
        &mut self,
        path: &str,
        body: &str,
        content_type: &str,
        extra_headers: &[(&str, &str)],
    ) -> TestClientResponse {
        let mut request = Request::builder()
            .method(Method::POST)
            .uri(path)
            .header(header::CONTENT_TYPE, content_type)
            .header("x-forwarded-for", self.forwarded_for())
            .header("origin", "http://127.0.0.1:8080")
            .header("sec-fetch-site", "same-origin");
        for (name, value) in extra_headers {
            request = request.header(*name, *value);
        }
        if !self.jar.is_empty() {
            request = request.header(
                header::COOKIE,
                self.jar
                    .iter()
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect::<Vec<_>>()
                    .join("; "),
            );
        }
        let request = request.body(Body::from(body.to_string())).unwrap();
        self.send(request).await
    }

    async fn request(
        &mut self,
        method: Method,
        path: &str,
        body: Option<String>,
    ) -> TestClientResponse {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
            .header("x-forwarded-for", self.forwarded_for())
            .header(header::USER_AGENT, "faktor-site-test")
            .header("origin", "http://127.0.0.1:8080")
            .header("sec-fetch-site", "same-origin");
        if !self.jar.is_empty() {
            builder = builder.header(
                header::COOKIE,
                self.jar
                    .iter()
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect::<Vec<_>>()
                    .join("; "),
            );
        }
        let request = if let Some(body) = body {
            builder
                .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                .body(Body::from(body))
                .unwrap()
        } else {
            builder.body(Body::empty()).unwrap()
        };
        self.send(request).await
    }

    async fn send(&mut self, mut request: Request<Body>) -> TestClientResponse {
        if let Some(peer) = self.peer {
            request.extensions_mut().insert(ConnectInfo(peer));
        }
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        for value in headers.get_all(header::SET_COOKIE) {
            if let Ok(cookie) = value.to_str() {
                let pair = cookie.split(';').next().unwrap_or("");
                if let Some((name, value)) = pair.split_once('=') {
                    if cookie.to_ascii_lowercase().contains("max-age=0") {
                        self.jar.remove(name);
                    } else {
                        self.jar.insert(name.into(), value.into());
                    }
                }
            }
        }
        let body = response.into_body().collect().await.unwrap().to_bytes();
        TestClientResponse {
            status,
            headers,
            body: String::from_utf8_lossy(&body).to_string(),
        }
    }

    #[allow(dead_code)]
    pub fn csrf_cookie(&self) -> String {
        self.jar
            .get("faktor_site_csrf")
            .cloned()
            .unwrap_or_default()
    }

    pub fn form_csrf(&self, html: &str) -> String {
        field_value(html, "csrf").unwrap_or_default()
    }

    pub fn cookie(&self, name: &str) -> Option<&str> {
        self.jar.get(name).map(String::as_str)
    }
}

pub fn field_value(html: &str, name: &str) -> Option<String> {
    let needle = format!("name=\"{name}\"");
    let index = html.find(&needle)?;
    let rest = &html[index + needle.len()..];
    let value_start = rest.find("value=\"")?;
    let rest = &rest[value_start + 7..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

pub fn extract_secret(body: &str) -> String {
    let marker = "secret=";
    let start = body.find(marker).expect("mail carries a secret link") + marker.len();
    body[start..]
        .chars()
        .take_while(|ch| !ch.is_whitespace() && *ch != '\n')
        .collect()
}

fn percent_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}
