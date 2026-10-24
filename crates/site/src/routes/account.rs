use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::Form;
use serde::Deserialize;

use crate::auth;
use crate::billing::org_id;
use crate::error::{SiteError, SiteResult};
use crate::security::{
    hash_password, normalize_email, validate_display_name, validate_org_name, validate_password,
    verify_password,
};
use crate::web::{self, account_pages, see_other};
use crate::AppState;

#[derive(Debug, Default, Deserialize)]
pub struct AccountQuery {
    #[serde(default)]
    pub verified: String,
    #[serde(default)]
    pub resent: String,
    #[serde(default)]
    pub updated: String,
    #[serde(default)]
    pub format: String,
}

#[derive(Debug, Deserialize)]
pub struct CsrfForm {
    pub csrf: String,
}

#[derive(Debug, Deserialize)]
pub struct ProfileForm {
    pub csrf: String,
    pub display_name: String,
}

#[derive(Debug, Deserialize)]
pub struct SwitchForm {
    pub csrf: String,
    pub org_id: String,
}

#[derive(Debug, Deserialize)]
pub struct NameForm {
    pub csrf: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct MemberForm {
    pub csrf: String,
    pub email: String,
    pub role: String,
}

#[derive(Debug, Deserialize)]
pub struct MemberUpdateForm {
    pub csrf: String,
    pub user_id: String,
    pub role: String,
}

#[derive(Debug, Deserialize)]
pub struct MemberRemoveForm {
    pub csrf: String,
    pub user_id: String,
}

#[derive(Debug, Deserialize)]
pub struct PasswordForm {
    pub csrf: String,
    pub current_password: String,
    pub new_password: String,
    pub confirm_password: String,
}

pub async fn overview_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<AccountQuery>,
) -> SiteResult<Response> {
    let auth = state.require_auth(&headers)?;
    let org = org_id(&auth)?;
    let _ = state.payments.tick(&org);
    let payments_overview = state.payments.overview(&org)?;
    let invoices = state.payments.invoices(&org, None, 10)?.items;
    let memberships = {
        let conn = state.db.conn()?;
        auth::memberships_for_user(&conn, &auth.user_id)?
    };
    let notice = if query.verified == "1" {
        Some("Email verified. Billing actions are now available.")
    } else if !query.resent.is_empty() {
        Some("A fresh verification email was sent.")
    } else {
        None
    };
    Ok(account_pages::overview(
        &auth,
        &auth.csrf,
        &auth.display_name,
        &memberships,
        &payments_overview,
        &invoices,
        notice,
        &[],
    ))
}

pub async fn profile_post(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<ProfileForm>,
) -> SiteResult<Response> {
    let auth = state.require_auth(&headers)?;
    state.verify_csrf(&headers, Some(&auth), &form.csrf)?;
    let name = validate_display_name(&form.display_name)?;
    {
        let conn = state.db.conn()?;
        conn.execute(
            "UPDATE users SET display_name=?1,updated_ms=?2 WHERE id=?3",
            rusqlite::params![name, state.now_ms(), auth.user_id],
        )?;
        auth::audit(
            &conn,
            &auth.user_id,
            Some(&auth.organization_id),
            "profile.update",
            &auth.user_id,
            "display name updated",
            state.now_ms(),
        )?;
    }
    Ok(see_other("/account/?updated=profile", &[]))
}

pub async fn organization_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> SiteResult<Response> {
    let auth = state.require_auth(&headers)?;
    let members = {
        let conn = state.db.conn()?;
        auth::list_members(&conn, &auth.organization_id)?
    };
    let memberships = {
        let conn = state.db.conn()?;
        auth::memberships_for_user(&conn, &auth.user_id)?
    };
    Ok(account_pages::organization(
        &auth,
        &auth.csrf,
        &account_pages::OrganizationData {
            members: &members,
            memberships: &memberships,
        },
        &[],
    ))
}

pub async fn switch_org(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<SwitchForm>,
) -> SiteResult<Response> {
    let auth = state.require_auth(&headers)?;
    state.verify_csrf(&headers, Some(&auth), &form.csrf)?;
    state.limit(
        &state.email_key("org:switch", &auth.email),
        30,
        60 * 60 * 1000,
    )?;
    {
        let conn = state.db.conn()?;
        auth::switch_organization(&conn, &auth.user_id, &auth.session_id, &form.org_id)?;
    }
    Ok(see_other("/account/", &[]))
}

pub async fn rename_org(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<NameForm>,
) -> SiteResult<Response> {
    let auth = state.require_auth(&headers)?;
    state.verify_csrf(&headers, Some(&auth), &form.csrf)?;
    auth.require_role(auth::Role::Owner)?;
    let name = validate_org_name(&form.name)?;
    {
        let conn = state.db.conn()?;
        auth::rename_organization(&conn, &auth.organization_id, &name)?;
        auth::audit(
            &conn,
            &auth.user_id,
            Some(&auth.organization_id),
            "organization.rename",
            &auth.organization_id,
            "organization renamed",
            state.now_ms(),
        )?;
    }
    Ok(see_other("/account/organization/?updated=name", &[]))
}

pub async fn add_member(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<MemberForm>,
) -> SiteResult<Response> {
    let auth = state.require_auth(&headers)?;
    state.verify_csrf(&headers, Some(&auth), &form.csrf)?;
    auth.require_role(auth::Role::Owner)?;
    state.limit(
        &state.email_key("org:member", &auth.email),
        20,
        60 * 60 * 1000,
    )?;
    let email = normalize_email(&form.email)
        .ok_or(SiteError::Unprocessable("Enter a valid email address."))?;
    let role = auth::Role::parse(&form.role)
        .filter(|role| *role != auth::Role::Owner)
        .ok_or(SiteError::Unprocessable("Choose member or admin."))?;
    {
        let conn = state.db.conn()?;
        auth::add_member(&conn, &auth.organization_id, &email, role, state.now_ms())?;
        auth::audit(
            &conn,
            &auth.user_id,
            Some(&auth.organization_id),
            "organization.member.add",
            &email,
            role.as_str(),
            state.now_ms(),
        )?;
    }
    Ok(see_other("/account/organization/?updated=member", &[]))
}

pub async fn update_member(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<MemberUpdateForm>,
) -> SiteResult<Response> {
    let auth = state.require_auth(&headers)?;
    state.verify_csrf(&headers, Some(&auth), &form.csrf)?;
    auth.require_role(auth::Role::Owner)?;
    let role =
        auth::Role::parse(&form.role).ok_or(SiteError::Unprocessable("Choose a valid role."))?;
    {
        let conn = state.db.conn()?;
        auth::update_member_role(
            &conn,
            &auth.organization_id,
            &form.user_id,
            role,
            state.now_ms(),
        )?;
        auth::audit(
            &conn,
            &auth.user_id,
            Some(&auth.organization_id),
            "organization.member.role",
            &form.user_id,
            role.as_str(),
            state.now_ms(),
        )?;
    }
    Ok(see_other("/account/organization/?updated=role", &[]))
}

pub async fn remove_member(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<MemberRemoveForm>,
) -> SiteResult<Response> {
    let auth = state.require_auth(&headers)?;
    state.verify_csrf(&headers, Some(&auth), &form.csrf)?;
    auth.require_role(auth::Role::Owner)?;
    {
        let conn = state.db.conn()?;
        auth::remove_member(&conn, &auth.organization_id, &form.user_id, state.now_ms())?;
        auth::audit(
            &conn,
            &auth.user_id,
            Some(&auth.organization_id),
            "organization.member.remove",
            &form.user_id,
            "membership removed",
            state.now_ms(),
        )?;
    }
    Ok(see_other("/account/organization/?updated=removed", &[]))
}

pub async fn security_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> SiteResult<Response> {
    let auth = state.require_auth(&headers)?;
    let sessions = {
        let conn = state.db.conn()?;
        auth::sessions_for_user(&conn, &auth.user_id, state.now_ms())?
    };
    Ok(account_pages::security(
        &auth,
        &auth.csrf,
        &account_pages::SecurityData {
            sessions: &sessions,
        },
        &[],
    ))
}

pub async fn change_password(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<PasswordForm>,
) -> SiteResult<Response> {
    let auth = state.require_auth(&headers)?;
    state.verify_csrf(&headers, Some(&auth), &form.csrf)?;
    state.limit(
        &state.email_key("password:change", &auth.email),
        5,
        60 * 60 * 1000,
    )?;
    if form.new_password != form.confirm_password {
        return Err(SiteError::Unprocessable(
            "The new password confirmation does not match.",
        ));
    }
    validate_password(&form.new_password)?;
    let db = state.db.clone();
    let user_id = auth.user_id.clone();
    let session_id = auth.session_id.clone();
    let current = form.current_password;
    let new_password = form.new_password;
    let now = state.now_ms();
    let changed = tokio::task::spawn_blocking(move || {
        let conn = db.conn()?;
        let stored: String = conn.query_row(
            "SELECT password_hash FROM users WHERE id=?1",
            rusqlite::params![user_id],
            |row| row.get(0),
        )?;
        if !verify_password(&stored, &current) {
            return Ok::<bool, SiteError>(false);
        }
        let new_hash = hash_password(&new_password)?;
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE users SET password_hash=?1,updated_ms=?2 WHERE id=?3",
            rusqlite::params![new_hash, now, user_id],
        )?;
        tx.execute(
            "UPDATE sessions SET revoked_ms=?1 WHERE user_id=?2 AND id<>?3 AND revoked_ms IS NULL",
            rusqlite::params![now, user_id, session_id],
        )?;
        tx.commit()?;
        Ok(true)
    })
    .await
    .map_err(|_| SiteError::internal_message("password change task failed"))??;
    if !changed {
        return Err(SiteError::Forbidden);
    }
    Ok(see_other("/account/security/?updated=password", &[]))
}

pub async fn revoke_other_sessions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> SiteResult<Response> {
    let auth = state.require_auth(&headers)?;
    state.verify_csrf(&headers, Some(&auth), &form.csrf)?;
    {
        let conn = state.db.conn()?;
        auth::revoke_user_sessions(&conn, &auth.user_id, state.now_ms(), Some(&auth.session_id))?;
    }
    Ok(see_other("/account/security/?updated=revoked", &[]))
}

pub async fn invoices_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> SiteResult<Response> {
    let auth = state.require_auth(&headers)?;
    let org = org_id(&auth)?;
    let _ = state.payments.tick(&org);
    let invoices = state.payments.invoices(&org, None, 100)?.items;
    Ok(account_pages::invoices_page(
        &auth,
        &auth.csrf,
        &invoices,
        &[],
    ))
}

pub async fn invoice_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<AccountQuery>,
) -> SiteResult<Response> {
    let auth = state.require_auth(&headers)?;
    let org = org_id(&auth)?;
    let invoice = state.payments.invoice(&org, &id)?;
    let notice = query.updated.as_str();
    let message = match notice {
        "paid" => Some("The invoice is paid in the local billing record."),
        "pending" => Some("Payment is still pending provider confirmation."),
        _ => None,
    };
    Ok(account_pages::invoice_detail(
        &auth,
        &auth.csrf,
        &invoice,
        message,
        &[],
    ))
}

pub async fn receipt_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> SiteResult<Response> {
    let auth = state.require_auth(&headers)?;
    let org = org_id(&auth)?;
    let invoice = state.payments.invoice(&org, &id)?;
    if invoice.status != faktor_payments::InvoiceStatus::Paid {
        return Err(SiteError::Conflict(
            "A receipt is only available for a paid invoice.",
        ));
    }
    Ok(account_pages::receipt(&auth, &invoice, &[]))
}

pub async fn invoices_export(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<AccountQuery>,
) -> SiteResult<Response> {
    let auth = state.require_auth(&headers)?;
    let org = org_id(&auth)?;
    let format = if query.format == "json" {
        "json"
    } else {
        "csv"
    };
    let text = state.payments.export_invoices(&org, format, 1_000)?;
    let content_type = if format == "json" {
        "application/json; charset=utf-8"
    } else {
        "text/csv; charset=utf-8"
    };
    let mut response = web::text_response(axum::http::StatusCode::OK, content_type, text);
    let disposition = if format == "json" {
        "attachment; filename=faktor-invoices.json"
    } else {
        "attachment; filename=faktor-invoices.csv"
    };
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        header::HeaderValue::from_static(disposition),
    );
    Ok(response)
}

pub async fn usage_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> SiteResult<Response> {
    let auth = state.require_auth(&headers)?;
    let org = org_id(&auth)?;
    let _ = state.payments.tick(&org);
    let overview = state.payments.overview(&org)?;
    let invoices = state.payments.invoices(&org, None, 200)?.items;
    let mut aggregated: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    let mut invoice_count = 0;
    for invoice in &invoices {
        let mut counted = false;
        for line in &invoice.lines {
            if line.kind == faktor_payments::InvoiceLineKind::Usage {
                let entry = aggregated.entry(line.description.clone()).or_insert((0, 0));
                entry.0 = entry.0.saturating_add(line.quantity);
                entry.1 = entry.1.saturating_add(line.amount_minor);
                counted = true;
            }
        }
        if counted {
            invoice_count += 1;
        }
    }
    let descriptions = aggregated
        .into_iter()
        .map(|(description, (quantity, amount))| (description, quantity, amount))
        .collect();
    let seats = overview.seats.unwrap_or(0);
    Ok(account_pages::usage_page(
        &auth,
        &account_pages::UsageSummary {
            descriptions,
            invoices: invoice_count,
            seats,
        },
        &[],
    ))
}
