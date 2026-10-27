mod account;
mod admin;
mod auth;
mod billing;
mod health;
mod webhook;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{ConnectInfo, DefaultBodyLimit, Request, State};
use axum::http::{header, header::HeaderName, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use tower_http::services::ServeDir;

use crate::AppState;

/// A wire-captured credential that never renders itself. The HTTP form
/// shape is still `String` on the wire, but the field cannot be logged or
/// accidentally compared/printed; callers must opt in with [`Self::expose`].
#[derive(Clone, serde::Deserialize)]
#[serde(transparent)]
pub(crate) struct WireSecret(String);

impl WireSecret {
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for WireSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WireSecret([redacted])")
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    let static_dir = state.config.static_dir.clone();
    let routes = Router::new()
        .route("/healthz", get(health::health))
        .route("/login/", get(auth::login_get).post(auth::login_post))
        .route("/signup/", get(auth::signup_get).post(auth::signup_post))
        .route("/logout/", post(auth::logout))
        .route(
            "/forgot-password/",
            get(auth::forgot_get).post(auth::forgot_post),
        )
        .route(
            "/reset-password/",
            get(auth::reset_get).post(auth::reset_post),
        )
        .route(
            "/verify-email/",
            get(auth::verify_get).post(auth::verify_post),
        )
        .route("/verify-email/sent/", get(auth::verify_sent))
        .route(
            "/account/",
            get(account::overview_get).post(account::profile_post),
        )
        .route("/account/profile/", post(account::profile_post))
        .route(
            "/account/verify-email/resend/",
            post(auth::resend_verification),
        )
        .route("/account/organization/", get(account::organization_get))
        .route("/account/organization/switch/", post(account::switch_org))
        .route("/account/organization/name/", post(account::rename_org))
        .route(
            "/account/organization/members/add/",
            post(account::add_member),
        )
        .route(
            "/account/organization/members/update/",
            post(account::update_member),
        )
        .route(
            "/account/organization/members/remove/",
            post(account::remove_member),
        )
        .route("/account/security/", get(account::security_get))
        .route(
            "/account/security/password/",
            post(account::change_password),
        )
        .route(
            "/account/security/sessions/revoke/",
            post(account::revoke_other_sessions),
        )
        .route("/account/billing/", get(billing::billing_get))
        .route("/account/billing/checkout/", post(billing::checkout_start))
        .route(
            "/account/billing/checkout/return/",
            get(billing::checkout_return),
        )
        .route(
            "/account/billing/invoices/{id}/pay/",
            post(billing::pay_invoice),
        )
        .route("/account/billing/change-plan/", post(billing::change_plan))
        .route("/account/billing/cancel/", post(billing::cancel))
        .route(
            "/account/billing/payment-methods/add/",
            post(billing::add_payment_method),
        )
        .route(
            "/account/billing/payment-methods/{id}/remove/",
            post(billing::remove_payment_method),
        )
        .route(
            "/account/billing/local/{id}/",
            get(billing::local_checkout_get),
        )
        .route(
            "/account/billing/local/{id}/pay/",
            post(billing::local_checkout_pay),
        )
        .route("/account/invoices/", get(account::invoices_get))
        .route("/account/invoices/export/", get(account::invoices_export))
        .route("/account/invoices/{id}/", get(account::invoice_get))
        .route(
            "/account/invoices/{id}/download/",
            get(account::invoice_download),
        )
        .route("/account/invoices/{id}/receipt/", get(account::receipt_get))
        .route("/account/usage/", get(account::usage_get))
        .route("/admin/", get(admin::admin_get))
        .route("/admin/users/disable/", post(admin::disable_user))
        .route("/admin/usage/", post(admin::record_usage))
        .fallback_service(
            ServeDir::new(static_dir)
                .append_index_html_on_directories(true)
                .precompressed_gzip(),
        )
        .merge(
            Router::new()
                .route("/webhooks/payments", post(webhook::payments))
                .layer(DefaultBodyLimit::max(256 * 1024)),
        )
        .with_state(state.clone());
    routes
        .layer(middleware::from_fn(peer_ip))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            security_headers,
        ))
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(30),
        ))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .with_state(state)
}

async fn peer_ip(mut request: Request, next: Next) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(address)| address.ip().to_string());
    request.headers_mut().remove("x-faktor-peer-ip");
    if let Some(peer) = peer {
        if let Ok(value) = HeaderValue::from_str(&peer) {
            request.headers_mut().insert("x-faktor-peer-ip", value);
        }
    }
    next.run(request).await
}

async fn security_headers(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path().to_string();
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    let csp = "default-src 'self'; script-src 'none'; style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'none'; media-src 'self'; object-src 'none'; frame-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(csp),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static("camera=(), microphone=(), geolocation=(), payment=()"),
    );
    headers.insert(
        HeaderName::from_static("cross-origin-opener-policy"),
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        HeaderName::from_static("cross-origin-resource-policy"),
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(
        HeaderName::from_static("x-permitted-cross-domain-policies"),
        HeaderValue::from_static("none"),
    );
    if state.config.cookie_secure {
        headers.insert(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=31536000; includeSubDomains"),
        );
    }
    if path.starts_with("/account/")
        || path.starts_with("/admin/")
        || path.starts_with("/login/")
        || path.starts_with("/signup/")
        || path.starts_with("/forgot-password/")
        || path.starts_with("/reset-password/")
        || path.starts_with("/verify-email/")
    {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    } else if path.starts_with("/fonts/") || path.starts_with("/css/") {
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=86400"),
        );
    }
    response
}
