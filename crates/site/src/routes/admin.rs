use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::Form;
use serde::Deserialize;

use crate::auth;
use crate::error::{SiteError, SiteResult};
use crate::web::{account_pages, see_other};
use crate::AppState;

#[derive(Debug, Default, Deserialize)]
pub struct AdminQuery {
    #[serde(default)]
    pub updated: String,
}

#[derive(Debug, Deserialize)]
pub struct DisableForm {
    pub csrf: String,
    pub user_id: String,
    pub disabled: String,
}

#[derive(Debug, Deserialize)]
pub struct UsageForm {
    pub csrf: String,
    pub organization_id: String,
    pub description: String,
    pub quantity: i64,
    pub unit_amount_minor: i64,
}

pub async fn admin_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<AdminQuery>,
) -> SiteResult<Response> {
    let auth = state.require_auth(&headers)?;
    auth.require_platform_admin()?;
    let (users, orgs) = {
        let conn = state.db.conn()?;
        (auth::admin_users(&conn)?, auth::admin_orgs(&conn)?)
    };
    let message = match query.updated.as_str() {
        "disabled" => Some("User disabled and sessions revoked."),
        "enabled" => Some("User enabled."),
        "usage" => Some("Usage invoice recorded."),
        _ => None,
    };
    Ok(account_pages::admin(
        &auth,
        &auth.csrf,
        &account_pages::AdminData {
            users: &users,
            orgs: &orgs,
        },
        message,
        &[],
    ))
}

pub async fn disable_user(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<DisableForm>,
) -> SiteResult<Response> {
    let auth = state.require_auth(&headers)?;
    state.verify_csrf(&headers, Some(&auth), &form.csrf)?;
    auth.require_platform_admin()?;
    state.limit(
        &state.email_key("admin:disable", &auth.email),
        20,
        60 * 60 * 1000,
    )?;
    if form.user_id == auth.user_id {
        return Err(SiteError::Conflict("You cannot disable your own account."));
    }
    let disabled = match form.disabled.as_str() {
        "1" => true,
        "0" => false,
        _ => return Err(SiteError::Unprocessable("Choose a valid user state.")),
    };
    {
        let conn = state.db.conn()?;
        auth::set_user_disabled(&conn, &form.user_id, disabled, state.now_ms())?;
        auth::audit(
            &conn,
            &auth.user_id,
            None,
            if disabled {
                "admin.user.disable"
            } else {
                "admin.user.enable"
            },
            &form.user_id,
            "platform administration",
            state.now_ms(),
        )?;
    }
    Ok(see_other(
        if disabled {
            "/admin/?updated=disabled"
        } else {
            "/admin/?updated=enabled"
        },
        &[],
    ))
}

pub async fn record_usage(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<UsageForm>,
) -> SiteResult<Response> {
    let auth = state.require_auth(&headers)?;
    state.verify_csrf(&headers, Some(&auth), &form.csrf)?;
    auth.require_platform_admin()?;
    state.limit(
        &state.email_key("admin:usage", &auth.email),
        60,
        60 * 60 * 1000,
    )?;
    if form.description.trim().is_empty() || form.description.len() > 200 {
        return Err(SiteError::Unprocessable(
            "Use a usage description of 1–200 bytes.",
        ));
    }
    if form.quantity <= 0 || form.quantity > 1_000_000 {
        return Err(SiteError::Unprocessable(
            "Quantity must be between 1 and 1,000,000.",
        ));
    }
    if form.unit_amount_minor < 0 || form.unit_amount_minor > 100_000_000 {
        return Err(SiteError::Unprocessable(
            "Unit amount is outside the accepted bound.",
        ));
    }
    let target = faktor_cloud::OrganizationId::try_new(form.organization_id.clone())
        .map_err(|_| SiteError::Unprocessable("Choose a valid organization."))?;
    let overview = state.payments.overview(&target)?;
    let subscription = overview
        .subscription
        .as_ref()
        .ok_or(SiteError::Unprocessable(
            "The organization has no subscription to invoice.",
        ))?;
    let request = faktor_payments::IssueInvoiceRequest {
        subscription_id: subscription.id.clone(),
        usage: vec![faktor_payments::UsageCharge {
            description: form.description.trim().to_string(),
            quantity: form.quantity,
            unit_amount_minor: form.unit_amount_minor,
        }],
        credits_minor: 0,
        discount_minor: 0,
        tax_jurisdiction: Some("NL".to_string()),
        due_in_days: Some(7),
        idempotency_key: format!("admin-usage-{}", uuid::Uuid::new_v4()),
    };
    let outcome = state.payments.issue_invoice(&target, request).await?;
    {
        let conn = state.db.conn()?;
        auth::audit(
            &conn,
            &auth.user_id,
            Some(target.as_str()),
            "admin.usage.invoice",
            &outcome.invoice.id,
            &format!(
                "{} units / {} minor each",
                form.quantity, form.unit_amount_minor
            ),
            state.now_ms(),
        )?;
    }
    Ok(see_other("/admin/?updated=usage", &[]))
}
