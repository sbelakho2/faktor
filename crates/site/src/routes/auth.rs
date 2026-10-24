use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::Form;
use serde::Deserialize;

use crate::auth;
use crate::error::{SiteError, SiteResult};
use crate::mail::MailMessage;
use crate::security::{
    clear_csrf_cookie, clear_session_cookie, safe_next, session_cookie, valid_secret_shape,
    validate_display_name,
};
use crate::web::{auth_pages, see_other};
use crate::AppState;

#[derive(Debug, Default, Deserialize)]
pub struct LinkQuery {
    #[serde(default)]
    pub secret: String,
    #[serde(default)]
    pub plan: String,
    #[serde(default)]
    pub next: String,
    #[serde(default)]
    pub verified: String,
}

#[derive(Debug, Deserialize)]
pub struct LoginForm {
    pub csrf: String,
    pub email: String,
    pub password: super::WireSecret,
    #[serde(default)]
    pub next: String,
}

#[derive(Debug, Deserialize)]
pub struct SignupForm {
    pub csrf: String,
    pub name: String,
    pub email: String,
    pub password: super::WireSecret,
    pub confirm: String,
    #[serde(default)]
    pub plan: String,
    #[serde(default)]
    pub accept_terms: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct EmailForm {
    pub csrf: String,
    pub email: String,
}

#[derive(Debug, Deserialize)]
pub struct ResetForm {
    pub csrf: String,
    pub secret: String,
    pub password: super::WireSecret,
    pub confirm: String,
}

#[derive(Debug, Deserialize)]
pub struct VerifyForm {
    pub csrf: String,
    pub secret: String,
}

pub async fn login_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<LinkQuery>,
) -> SiteResult<Response> {
    if state.auth_from(&headers)?.is_some() {
        return Ok(see_other("/account/", &[]));
    }
    let (csrf, cookies) = state.csrf_cookie_token(&headers, None);
    let next = safe_next(Some(&query.next), "/account/");
    let notice = (query.verified == "1").then_some("Your email is verified. Sign in to continue.");
    Ok(auth_pages::login_page(&csrf, &next, None, notice, &cookies))
}

pub async fn login_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<LoginForm>,
) -> SiteResult<Response> {
    state.verify_csrf(&headers, None, &form.csrf)?;
    state.limit(&state.ip_key(&headers, "login:ip"), 10, 15 * 60 * 1000)?;
    let email = crate::security::normalize_email(&form.email)
        .ok_or(SiteError::Unprocessable("Enter a valid email address."))?;
    state.limit(&state.email_key("login:email", &email), 5, 15 * 60 * 1000)?;
    let password_input = form.password.expose();
    if password_input.is_empty() || password_input.len() > 256 {
        return render_login_error(&state, &headers, "/account/");
    }
    let db = state.db.clone();
    let email_for_auth = email.clone();
    let password = form.password.expose().to_string();
    let user = tokio::task::spawn_blocking(move || {
        let conn = db.conn()?;
        auth::authenticate(&conn, &email_for_auth, &password)
    })
    .await
    .map_err(|_| SiteError::internal_message("login task failed"))??;
    let Some(user) = user else {
        return render_login_error(&state, &headers, &form.next);
    };
    let memberships = {
        let conn = state.db.conn()?;
        auth::memberships_for_user(&conn, &user.id)?
    };
    let Some(membership) = memberships.first() else {
        return Err(SiteError::Forbidden);
    };
    let client_tag = crate::security::client_tag(&headers, &state.config);
    let new_session = {
        let conn = state.db.conn()?;
        if let Some(old) = crate::security::cookie_value(&headers, crate::security::SESSION_COOKIE)
        {
            if let Ok(Some(old_session)) =
                auth::load_session(&conn, &old, state.now_ms(), state.config.idle_ttl_ms)
            {
                auth::revoke_session(&conn, &old_session.session_id, state.now_ms())?;
            }
        }
        auth::insert_session(
            &conn,
            &user.id,
            &membership.organization_id,
            state.now_ms(),
            state.config.session_ttl_ms,
            &client_tag,
        )?
    };
    let cookies = vec![
        session_cookie(
            &state.config,
            &new_session.secret,
            state.config.session_ttl_ms / 1000,
        ),
        clear_csrf_cookie(&state.config),
    ];
    let next = safe_next(Some(&form.next), "/account/");
    Ok(see_other(&next, &cookies))
}

fn render_login_error(state: &AppState, headers: &HeaderMap, next: &str) -> SiteResult<Response> {
    let (csrf, cookies) = state.csrf_cookie_token(headers, None);
    let next = safe_next(Some(next), "/account/");
    Ok(auth_pages::login_page(
        &csrf,
        &next,
        Some("That email and password combination did not sign in."),
        None,
        &cookies,
    ))
}

pub async fn signup_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<LinkQuery>,
) -> SiteResult<Response> {
    if state.auth_from(&headers)?.is_some() {
        return Ok(see_other("/account/", &[]));
    }
    let (csrf, cookies) = state.csrf_cookie_token(&headers, None);
    let plan = crate::billing::plan(&query.plan)
        .map(|plan| plan.id.to_string())
        .unwrap_or_default();
    Ok(auth_pages::signup_page(
        &csrf, "", "", &plan, None, &cookies,
    ))
}

pub async fn signup_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<SignupForm>,
) -> SiteResult<Response> {
    state.verify_csrf(&headers, None, &form.csrf)?;
    state.limit(&state.ip_key(&headers, "signup:ip"), 5, 60 * 60 * 1000)?;
    if form.accept_terms.as_deref() != Some("1") {
        return render_signup_error(
            &state,
            &headers,
            &form,
            "Accept the terms and privacy policy to continue.",
        );
    }
    let email = crate::security::normalize_email(&form.email)
        .ok_or(SiteError::Unprocessable("Enter a valid email address."))?;
    state.limit(&state.email_key("signup:email", &email), 3, 60 * 60 * 1000)?;
    let name = validate_display_name(&form.name)?;
    if form.password.expose() != form.confirm {
        return render_signup_error(
            &state,
            &headers,
            &form,
            "The password confirmation does not match.",
        );
    }
    crate::security::validate_password(form.password.expose())?;
    let plan = crate::billing::plan(&form.plan)
        .map(|plan| plan.id.to_string())
        .unwrap_or_default();
    let is_admin = state
        .config
        .admin_email
        .as_deref()
        .map(|admin| admin == email)
        .unwrap_or(false);
    let client_tag = crate::security::client_tag(&headers, &state.config);
    let now_ms = state.now_ms();
    let db = state.db.clone();
    let email_for_db = email.clone();
    let name_for_db = name.clone();
    let password = form.password.expose().to_string();
    let ttl = state.config.session_ttl_ms;
    let outcome = tokio::task::spawn_blocking(move || {
        let conn = db.conn()?;
        auth::create_user_and_org(
            &conn,
            auth::NewAccount {
                email: &email_for_db,
                display_name: &name_for_db,
                password: &password,
                now_ms,
                session_ttl_ms: ttl,
                is_admin,
                client_tag: &client_tag,
            },
        )
    })
    .await
    .map_err(|_| SiteError::internal_message("signup task failed"))??;
    match outcome {
        auth::SignupOutcome::Existing => {
            let _ = send_existing_account_notice(&state, &email);
            Ok(auth_pages::verify_sent_page(&[]))
        }
        auth::SignupOutcome::Created(signup) => {
            if let Err(error) = send_verification(&state, &signup) {
                tracing::error!(error = %error, "verification mail could not be delivered");
            }
            let _ = plan;
            let cookies = vec![
                session_cookie(
                    &state.config,
                    &signup.session.secret,
                    state.config.session_ttl_ms / 1000,
                ),
                clear_csrf_cookie(&state.config),
            ];
            Ok(see_other("/verify-email/sent/", &cookies))
        }
    }
}

fn render_signup_error(
    state: &AppState,
    headers: &HeaderMap,
    form: &SignupForm,
    message: &str,
) -> SiteResult<Response> {
    let (csrf, cookies) = state.csrf_cookie_token(headers, None);
    let plan = crate::billing::plan(&form.plan)
        .map(|plan| plan.id.to_string())
        .unwrap_or_default();
    Ok(auth_pages::signup_page(
        &csrf,
        &form.email,
        &form.name,
        &plan,
        Some(message),
        &cookies,
    ))
}

#[derive(Debug, Deserialize)]
pub struct LogoutForm {
    pub csrf: String,
}

pub async fn logout(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<LogoutForm>,
) -> SiteResult<Response> {
    let Some(auth) = state.auth_from(&headers)? else {
        return Ok(see_other(
            "/",
            &[
                clear_session_cookie(&state.config),
                clear_csrf_cookie(&state.config),
            ],
        ));
    };
    state.verify_csrf(&headers, Some(&auth), &form.csrf)?;
    {
        let conn = state.db.conn()?;
        auth::revoke_session(&conn, &auth.session_id, state.now_ms())?;
    }
    Ok(see_other(
        "/",
        &[
            clear_session_cookie(&state.config),
            clear_csrf_cookie(&state.config),
        ],
    ))
}

pub async fn forgot_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> SiteResult<Response> {
    let (csrf, cookies) = state.csrf_cookie_token(&headers, None);
    Ok(auth_pages::forgot_page(&csrf, false, None, &cookies))
}

pub async fn forgot_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<EmailForm>,
) -> SiteResult<Response> {
    state.verify_csrf(&headers, None, &form.csrf)?;
    state.limit(&state.ip_key(&headers, "forgot:ip"), 3, 60 * 60 * 1000)?;
    if let Some(email) = crate::security::normalize_email(&form.email) {
        state.limit(&state.email_key("forgot:email", &email), 3, 60 * 60 * 1000)?;
        if let Some(secret) = issue_reset(&state, &email)? {
            let body = format!(
                "A password reset was requested for your Faktor account.\n\nOpen this single-use link within one hour:\n\n{}/reset-password/?secret={}\n\nIf you did not request it, ignore this message. Existing sessions remain active until the password is changed.",
                state.config.base_url.trim_end_matches('/'),
                secret
            );
            let message = MailMessage {
                to: email,
                subject: "Faktor password reset".into(),
                body,
            };
            if let Err(error) = state.mailer.send(&message) {
                tracing::error!(error = %error, "reset mail could not be delivered");
            }
        }
    }
    let (csrf, cookies) = state.csrf_cookie_token(&headers, None);
    let _ = csrf;
    Ok(auth_pages::forgot_page("", true, None, &cookies))
}

fn issue_reset(state: &AppState, email: &str) -> SiteResult<Option<String>> {
    let conn = state.db.conn()?;
    use rusqlite::OptionalExtension;
    let user_id: Option<String> = conn
        .query_row(
            "SELECT id FROM users WHERE email=?1 AND disabled=0",
            rusqlite::params![email],
            |row| row.get(0),
        )
        .optional()?;
    let Some(user_id) = user_id else {
        return Ok(None);
    };
    auth::invalidate_email_events(&conn, &user_id, "reset_password", state.now_ms())?;
    let secret = auth::insert_email_event(
        &conn,
        &user_id,
        email,
        "reset_password",
        state.now_ms(),
        60 * 60 * 1000,
    )?;
    Ok(Some(secret))
}

pub async fn reset_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<LinkQuery>,
) -> SiteResult<Response> {
    let (csrf, cookies) = state.csrf_cookie_token(&headers, None);
    if !valid_secret_shape(&query.secret) {
        return Ok(auth_pages::forgot_page(&csrf, false, None, &cookies));
    }
    Ok(auth_pages::reset_page(&csrf, &query.secret, None, &cookies))
}

pub async fn reset_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<ResetForm>,
) -> SiteResult<Response> {
    state.verify_csrf(&headers, None, &form.csrf)?;
    state.limit(&state.ip_key(&headers, "reset:ip"), 5, 60 * 60 * 1000)?;
    if form.password.expose() != form.confirm {
        let (csrf, cookies) = state.csrf_cookie_token(&headers, None);
        return Ok(auth_pages::reset_page(
            &csrf,
            &form.secret,
            Some("The password confirmation does not match."),
            &cookies,
        ));
    }
    let db = state.db.clone();
    let secret = form.secret;
    let password = form.password.expose().to_string();
    let now = state.now_ms();
    let changed = tokio::task::spawn_blocking(move || {
        let conn = db.conn()?;
        auth::reset_password(&conn, &secret, &password, now)
    })
    .await
    .map_err(|_| SiteError::internal_message("reset task failed"))??;
    if !changed {
        let (csrf, cookies) = state.csrf_cookie_token(&headers, None);
        return Ok(auth_pages::reset_page(
            &csrf,
            "",
            Some("That reset link is invalid, expired or already used."),
            &cookies,
        ));
    }
    Ok(auth_pages::reset_done_page(&[
        clear_session_cookie(&state.config),
        clear_csrf_cookie(&state.config),
    ]))
}

pub async fn verify_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<LinkQuery>,
) -> SiteResult<Response> {
    let (csrf, cookies) = state.csrf_cookie_token(&headers, None);
    if !valid_secret_shape(&query.secret) {
        return Ok(auth_pages::forgot_page(&csrf, false, None, &cookies));
    }
    Ok(auth_pages::verify_page(
        &csrf,
        &query.secret,
        None,
        &cookies,
    ))
}

pub async fn verify_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<VerifyForm>,
) -> SiteResult<Response> {
    state.verify_csrf(&headers, None, &form.csrf)?;
    state.limit(&state.ip_key(&headers, "verify:ip"), 10, 60 * 60 * 1000)?;
    let db = state.db.clone();
    let secret = form.secret;
    let now = state.now_ms();
    let ok = tokio::task::spawn_blocking(move || {
        let conn = db.conn()?;
        auth::verify_email(&conn, &secret, now)
    })
    .await
    .map_err(|_| SiteError::internal_message("verification task failed"))??;
    if !ok {
        let (csrf, cookies) = state.csrf_cookie_token(&headers, None);
        return Ok(auth_pages::verify_page(
            &csrf,
            "",
            Some("That verification link is invalid, expired or already used."),
            &cookies,
        ));
    }
    Ok(see_other(
        "/login/?verified=1",
        &[clear_csrf_cookie(&state.config)],
    ))
}

pub async fn verify_sent() -> Response {
    auth_pages::verify_sent_page(&[])
}

#[derive(Debug, Deserialize)]
pub struct ResendForm {
    pub csrf: String,
}

pub async fn resend_verification(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<ResendForm>,
) -> SiteResult<Response> {
    let auth = state.require_auth(&headers)?;
    state.verify_csrf(&headers, Some(&auth), &form.csrf)?;
    state.limit(
        &state.email_key("verify:resend", &auth.email),
        3,
        60 * 60 * 1000,
    )?;
    if !auth.email_verified {
        let db = state.db.clone();
        let user_id = auth.user_id.clone();
        let email = auth.email.clone();
        let now = state.now_ms();
        let secret = tokio::task::spawn_blocking(move || {
            let conn = db.conn()?;
            auth::invalidate_email_events(&conn, &user_id, "verify_email", now)?;
            auth::insert_email_event(
                &conn,
                &user_id,
                &email,
                "verify_email",
                now,
                24 * 60 * 60 * 1000,
            )
        })
        .await
        .map_err(|_| SiteError::internal_message("verification task failed"))??;
        let body = verification_body(&state, &secret);
        let message = MailMessage {
            to: auth.email.clone(),
            subject: "Verify your Faktor email".into(),
            body,
        };
        state.mailer.send(&message)?;
    }
    Ok(see_other("/account/?resent=1", &[]))
}

fn send_verification(state: &AppState, signup: &auth::Signup) -> SiteResult<()> {
    let body = verification_body(state, &signup.verification_secret);
    state.mailer.send(&MailMessage {
        to: signup.user.email.clone(),
        subject: "Verify your Faktor email".into(),
        body,
    })
}

fn verification_body(state: &AppState, secret: &str) -> String {
    format!(
        "Welcome to Faktor.\n\nVerify your email by opening this single-use link within 24 hours:\n\n{}/verify-email/?secret={}\n\nBilling actions require a verified email address.",
        state.config.base_url.trim_end_matches('/'),
        secret
    )
}

fn send_existing_account_notice(state: &AppState, email: &str) -> SiteResult<()> {
    state.mailer.send(&MailMessage {
        to: email.to_string(),
        subject: "A Faktor account already exists".into(),
        body: "Someone tried to create a Faktor account with this address. If that was you, sign in or use the password reset flow. No duplicate account was created.".into(),
    })
}
