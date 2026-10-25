use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::Form;
use faktor_cloud::OrganizationId;
use faktor_payments::{
    AddPaymentMethodRequest, BillingInterval, CancelRequest, Invoice, InvoiceStatus,
    ProviderCheckout, WebhookDisposition, WebhookHeaders,
};
use serde::Deserialize;

use crate::auth;
use crate::billing::{self, org_id, CheckoutDraft, CheckoutRecord};
use crate::error::{SiteError, SiteResult};
use crate::security::{looks_like_pan, random_secret, valid_secret_shape};
use crate::web::{billing_pages, see_other};
use crate::AppState;

#[derive(Debug, Default, Deserialize)]
pub struct BillingQuery {
    #[serde(default)]
    pub updated: String,
    #[serde(default)]
    pub invoice: String,
}

#[derive(Debug, Deserialize)]
pub struct CheckoutForm {
    pub csrf: String,
    pub plan_id: String,
    pub interval: String,
    pub request_id: String,
}

#[derive(Debug, Deserialize)]
pub struct PayForm {
    pub csrf: String,
}

#[derive(Debug, Deserialize)]
pub struct ChangeForm {
    pub csrf: String,
    pub plan_id: String,
    pub interval: String,
    #[serde(default)]
    pub confirm: String,
}

#[derive(Debug, Deserialize)]
pub struct CancelForm {
    pub csrf: String,
    pub at_period_end: String,
    #[serde(default)]
    pub confirm: String,
}

#[derive(Debug, Deserialize)]
pub struct RemoveMethodForm {
    pub csrf: String,
    #[serde(default)]
    pub confirm: String,
}

#[derive(Debug, Deserialize)]
pub struct MethodForm {
    pub csrf: String,
    pub provider_token: super::WireSecret,
    pub brand: String,
    pub last4: String,
    pub exp_month: String,
    pub exp_year: String,
}

#[derive(Debug, Deserialize)]
pub struct CsrfForm {
    pub csrf: String,
}

pub async fn billing_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<BillingQuery>,
) -> SiteResult<Response> {
    let auth = state.require_auth(&headers)?;
    let org = org_id(&auth)?;
    let _ = state.payments.tick(&org);
    let overview = state.payments.overview(&org)?;
    let invoices = state.payments.invoices(&org, None, 20)?.items;
    let methods = state.payments.payment_methods(&org)?;
    let notice = match query.updated.as_str() {
        "plan" => Some((
            "ok",
            "The plan change was recorded. Any proration is attached to the next invoice.",
        )),
        "cancel" => Some(("warn", "The cancellation request was recorded.")),
        "method" => Some(("ok", "The provider token was stored and masked.")),
        "method-removed" => Some(("ok", "The payment method was removed.")),
        "retry" => Some((
            "warn",
            "The checkout was opened again. Only a signed webhook settles the invoice.",
        )),
        _ => None,
    };
    let request_id = random_secret();
    Ok(billing_pages::billing(
        &auth,
        &auth.csrf,
        &billing_pages::BillingData {
            payments: &state.payments,
            overview: &overview,
            invoices: &invoices,
            methods: &methods,
            request_id: &request_id,
            notice,
        },
        &[],
    ))
}

pub async fn checkout_start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<CheckoutForm>,
) -> SiteResult<Response> {
    let auth = state.require_auth(&headers)?;
    state.verify_csrf(&headers, Some(&auth), &form.csrf)?;
    auth.require_role(auth::Role::Owner)?;
    auth.require_verified()?;
    state.limit(
        &state.email_key("billing:checkout", &auth.email),
        20,
        60 * 60 * 1000,
    )?;
    if !valid_secret_shape(&form.request_id) {
        return Err(SiteError::BadRequest(
            "The checkout request identifier is invalid.",
        ));
    }
    if crate::config::parse_interval(&form.interval).is_none() {
        return Err(SiteError::Unprocessable(
            "Choose monthly or annual billing.",
        ));
    }
    if billing::plan(&form.plan_id).is_none() {
        return Err(SiteError::Unprocessable("Choose a current plan."));
    }
    let org = org_id(&auth)?;
    let invoice = begin_subscription(&state, &auth, &org, &form).await?;
    let checkout =
        create_checkout_for_invoice(&state, &auth, &org, &invoice, &form.request_id).await?;
    redirect_checkout(&state, &auth, &checkout)
}

async fn begin_subscription(
    state: &AppState,
    auth: &auth::AuthSession,
    org: &OrganizationId,
    form: &CheckoutForm,
) -> SiteResult<Invoice> {
    let _ = state.payments.tick(org);
    let overview = state.payments.overview(org)?;
    let interval = crate::config::parse_interval(&form.interval).ok_or(
        SiteError::Unprocessable("Choose monthly or annual billing."),
    )?;
    if let Some(subscription) = overview.subscription.as_ref() {
        if subscription.plan_id != form.plan_id || subscription.interval != interval {
            return Err(SiteError::Conflict(
                "A live subscription exists. Use the plan-change form instead of starting a second subscription.",
            ));
        }
        return billing::open_invoice_for_subscription(&state.payments, org, &subscription.id)?
            .ok_or(SiteError::Conflict(
                "The subscription is already active and has no open invoice to pay.",
            ));
    }
    let request = billing::subscription_request(
        &form.plan_id,
        interval,
        &auth.email,
        &auth.display_name,
        format!("site-sub-{}", form.request_id),
    )?;
    let _ = state.payments.subscribe(org, request).await?;
    let after = state.payments.overview(org)?;
    let subscription = after
        .subscription
        .as_ref()
        .ok_or_else(|| SiteError::internal_message("subscription vanished after creation"))?;
    let issue = faktor_payments::IssueInvoiceRequest {
        subscription_id: subscription.id.clone(),
        usage: Vec::new(),
        credits_minor: 0,
        discount_minor: 0,
        tax_jurisdiction: Some("NL".to_string()),
        due_in_days: Some(7),
        idempotency_key: format!("site-invoice-{}", form.request_id),
    };
    let outcome = state.payments.issue_invoice(org, issue).await?;
    Ok(outcome.invoice)
}

async fn create_checkout_for_invoice(
    state: &AppState,
    auth: &auth::AuthSession,
    org: &OrganizationId,
    invoice: &Invoice,
    request_id: &str,
) -> SiteResult<CheckoutRecord> {
    if !billing::invoice_is_open(invoice) {
        return Err(SiteError::Conflict("That invoice is not open for payment."));
    }
    if let Some(existing) = {
        let conn = state.db.conn()?;
        billing::checkout_by_idempotency(&conn, org.as_str(), request_id)?
    } {
        return Ok(existing);
    }
    let overview = state.payments.overview(org)?;
    let customer = overview
        .customer
        .as_ref()
        .ok_or_else(|| SiteError::internal_message("billing customer is missing"))?;
    let request = billing::checkout_request(
        org,
        &customer.provider_customer_id,
        invoice,
        &invoice_plan(invoice, &overview),
        &state.config.base_url,
        format!("site-provider-{request_id}"),
    )?;
    let provider_checkout = state.payments.create_checkout(org, request).await?;
    store_checkout(state, auth, org, invoice, request_id, &provider_checkout)
}

fn invoice_plan(invoice: &Invoice, overview: &faktor_payments::PaymentsOverview) -> String {
    if let Some(subscription) = overview.subscription.as_ref() {
        if invoice.subscription_id.as_deref() == Some(subscription.id.as_str()) {
            return subscription.plan_id.clone();
        }
    }
    invoice
        .lines
        .iter()
        .find(|line| line.kind == faktor_payments::InvoiceLineKind::Plan)
        .and_then(|line| line.description.split_whitespace().next())
        .unwrap_or("faktor")
        .to_string()
}

fn store_checkout(
    state: &AppState,
    _auth: &auth::AuthSession,
    org: &OrganizationId,
    invoice: &Invoice,
    request_id: &str,
    provider_checkout: &ProviderCheckout,
) -> SiteResult<CheckoutRecord> {
    let url = billing::provider_checkout_url(provider_checkout);
    let provider = state.payments.provider_kind().as_str();
    let overview = state.payments.overview(org)?;
    let plan_id = invoice_plan(invoice, &overview);
    let interval = crate::config::interval_name(invoice_interval(invoice, &overview)).to_string();
    let draft = CheckoutDraft {
        organization_id: org.as_str(),
        invoice_id: &invoice.id,
        plan_id: &plan_id,
        interval: &interval,
        amount_minor: invoice.balance_minor(),
        currency: &invoice.currency,
        provider,
        idempotency_key: request_id,
        now_ms: state.now_ms(),
    };
    let conn = state.db.conn()?;
    match billing::insert_checkout(
        &conn,
        &draft,
        Some(provider_checkout.id.as_str()),
        url.as_deref(),
    ) {
        Ok(checkout) => Ok(checkout),
        Err(SiteError::Internal(_)) => {
            billing::checkout_by_idempotency(&conn, org.as_str(), request_id)?
                .ok_or_else(|| SiteError::internal_message("checkout could not be recorded"))
        }
        Err(error) => Err(error),
    }
}

fn invoice_interval(
    invoice: &Invoice,
    overview: &faktor_payments::PaymentsOverview,
) -> BillingInterval {
    overview
        .subscription
        .as_ref()
        .filter(|subscription| invoice.subscription_id.as_deref() == Some(subscription.id.as_str()))
        .map(|subscription| subscription.interval)
        .unwrap_or(BillingInterval::Monthly)
}

fn redirect_checkout(
    state: &AppState,
    _auth: &auth::AuthSession,
    checkout: &CheckoutRecord,
) -> SiteResult<Response> {
    if let Some(url) = checkout.checkout_url.as_deref() {
        if url.starts_with("https://") || url.starts_with("http://") {
            return Ok(see_other(url, &[]));
        }
    }
    if state.payments.provider_kind() != faktor_payments::ProviderKind::Local {
        return Err(SiteError::Unavailable(
            "The payment provider did not return a hosted checkout URL.",
        ));
    }
    Ok(see_other(
        &format!("/account/billing/local/{}/", checkout.id),
        &[],
    ))
}

pub async fn pay_invoice(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(invoice_id): Path<String>,
    Form(form): Form<PayForm>,
) -> SiteResult<Response> {
    let auth = state.require_auth(&headers)?;
    state.verify_csrf(&headers, Some(&auth), &form.csrf)?;
    auth.require_role(auth::Role::Owner)?;
    auth.require_verified()?;
    state.limit(
        &state.email_key("billing:pay", &auth.email),
        20,
        60 * 60 * 1000,
    )?;
    let org = org_id(&auth)?;
    let invoice = state.payments.invoice(&org, &invoice_id)?;
    let request_id = random_secret();
    let checkout = create_checkout_for_invoice(&state, &auth, &org, &invoice, &request_id).await?;
    redirect_checkout(&state, &auth, &checkout)
}

pub async fn change_plan(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<ChangeForm>,
) -> SiteResult<Response> {
    let auth = state.require_auth(&headers)?;
    state.verify_csrf(&headers, Some(&auth), &form.csrf)?;
    auth.require_role(auth::Role::Owner)?;
    auth.require_verified()?;
    if form.confirm != "yes" {
        return Err(SiteError::Unprocessable(
            "Confirm the plan change before submitting it.",
        ));
    }
    let org = org_id(&auth)?;
    let overview = state.payments.overview(&org)?;
    let subscription = overview.subscription.as_ref().ok_or(SiteError::Conflict(
        "There is no live subscription to change.",
    ))?;
    let interval = crate::config::parse_interval(&form.interval).ok_or(
        SiteError::Unprocessable("Choose monthly or annual billing."),
    )?;
    let request = billing::change_request(
        &subscription.id,
        &form.plan_id,
        Some(interval),
        format!("site-change-{}", random_secret()),
    )?;
    let _ = state.payments.change_plan(&org, request).await?;
    Ok(see_other("/account/billing/?updated=plan", &[]))
}

pub async fn cancel(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<CancelForm>,
) -> SiteResult<Response> {
    let auth = state.require_auth(&headers)?;
    state.verify_csrf(&headers, Some(&auth), &form.csrf)?;
    auth.require_role(auth::Role::Owner)?;
    auth.require_verified()?;
    if form.confirm != "yes" {
        return Err(SiteError::Unprocessable(
            "Confirm the cancellation boundary before submitting it.",
        ));
    }
    let org = org_id(&auth)?;
    let overview = state.payments.overview(&org)?;
    let subscription = overview.subscription.as_ref().ok_or(SiteError::Conflict(
        "There is no live subscription to cancel.",
    ))?;
    let at_period_end = match form.at_period_end.as_str() {
        "1" => true,
        "0" => false,
        _ => return Err(SiteError::Unprocessable("Choose a cancellation boundary.")),
    };
    let _ = state
        .payments
        .cancel(
            &org,
            CancelRequest {
                subscription_id: subscription.id.clone(),
                at_period_end,
                idempotency_key: format!("site-cancel-{}", random_secret()),
            },
        )
        .await?;
    Ok(see_other("/account/billing/?updated=cancel", &[]))
}

pub async fn add_payment_method(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<MethodForm>,
) -> SiteResult<Response> {
    let auth = state.require_auth(&headers)?;
    state.verify_csrf(&headers, Some(&auth), &form.csrf)?;
    auth.require_role(auth::Role::Owner)?;
    auth.require_verified()?;
    state.limit(
        &state.email_key("billing:method", &auth.email),
        10,
        60 * 60 * 1000,
    )?;
    if looks_like_pan(form.provider_token.expose()) {
        return Err(SiteError::Unprocessable(
            "Card numbers are refused. Use an opaque provider token only.",
        ));
    }
    let provider_token = form.provider_token.expose();
    if provider_token.len() > 512 || provider_token.trim().is_empty() {
        return Err(SiteError::Unprocessable(
            "Use a provider token between 1 and 512 characters.",
        ));
    }
    if form.brand.trim().is_empty() || form.brand.len() > 40 {
        return Err(SiteError::Unprocessable(
            "Use a provider brand of 1–40 characters.",
        ));
    }
    if form.last4.len() != 4 || !form.last4.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(SiteError::Unprocessable(
            "Last four must be exactly four digits.",
        ));
    }
    let exp_month: u8 = form
        .exp_month
        .parse()
        .map_err(|_| SiteError::Unprocessable("Expiry month is invalid."))?;
    let exp_year: u16 = form
        .exp_year
        .parse()
        .map_err(|_| SiteError::Unprocessable("Expiry year is invalid."))?;
    if !(1..=12).contains(&exp_month) || !(2026..=2099).contains(&exp_year) {
        return Err(SiteError::Unprocessable(
            "Expiry is outside the accepted range.",
        ));
    }
    let org = org_id(&auth)?;
    let _ = state.payments.add_payment_method(
        &org,
        AddPaymentMethodRequest {
            token: provider_token.to_string(),
            brand: form.brand.trim().to_string(),
            last4: form.last4,
            exp_month,
            exp_year,
            set_default: true,
            idempotency_key: format!("site-method-{}", random_secret()),
        },
    )?;
    Ok(see_other("/account/billing/?updated=method", &[]))
}

pub async fn remove_payment_method(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(method_id): Path<String>,
    Form(form): Form<RemoveMethodForm>,
) -> SiteResult<Response> {
    let auth = state.require_auth(&headers)?;
    state.verify_csrf(&headers, Some(&auth), &form.csrf)?;
    auth.require_role(auth::Role::Owner)?;
    auth.require_verified()?;
    if form.confirm != "yes" {
        return Err(SiteError::Unprocessable(
            "Confirm removing the payment method before submitting it.",
        ));
    }
    let org = org_id(&auth)?;
    let _ = state.payments.remove_payment_method(
        &org,
        &method_id,
        &format!("site-remove-{}", random_secret()),
    )?;
    Ok(see_other("/account/billing/?updated=method-removed", &[]))
}

pub async fn checkout_return(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<BillingQuery>,
) -> SiteResult<Response> {
    let auth = state.require_auth(&headers)?;
    let org = org_id(&auth)?;
    let invoice = if query.invoice.is_empty() {
        None
    } else {
        state.payments.invoice(&org, &query.invoice).ok()
    };
    Ok(billing_pages::checkout_return(
        &auth,
        &auth.csrf,
        invoice.as_ref(),
        &[],
    ))
}

pub async fn local_checkout_get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(checkout_id): Path<String>,
) -> SiteResult<Response> {
    let auth = state.require_auth(&headers)?;
    auth.require_role(auth::Role::Owner)?;
    auth.require_verified()?;
    let org = org_id(&auth)?;
    let checkout = {
        let conn = state.db.conn()?;
        billing::checkout_by_id(&conn, org.as_str(), &checkout_id)?.ok_or(SiteError::NotFound)?
    };
    let invoice = state.payments.invoice(&org, &checkout.invoice_id)?;
    if invoice.status != InvoiceStatus::Open && invoice.status != InvoiceStatus::Paid {
        return Err(SiteError::Conflict("The checkout invoice is not open."));
    }
    if invoice.status == InvoiceStatus::Paid {
        return Ok(see_other(
            &format!("/account/billing/checkout/return/?invoice={}", invoice.id),
            &[],
        ));
    }
    Ok(billing_pages::local_checkout(
        &auth,
        &auth.csrf,
        &checkout_id,
        &invoice,
        &[],
    ))
}

pub async fn local_checkout_pay(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(checkout_id): Path<String>,
    Form(form): Form<CsrfForm>,
) -> SiteResult<Response> {
    let auth = state.require_auth(&headers)?;
    state.verify_csrf(&headers, Some(&auth), &form.csrf)?;
    auth.require_role(auth::Role::Owner)?;
    auth.require_verified()?;
    state.limit(
        &state.email_key("billing:local-pay", &auth.email),
        10,
        60 * 60 * 1000,
    )?;
    let org = org_id(&auth)?;
    let (checkout, invoice) = {
        let conn = state.db.conn()?;
        let checkout = billing::checkout_by_id(&conn, org.as_str(), &checkout_id)?
            .ok_or(SiteError::NotFound)?;
        let invoice = state.payments.invoice(&org, &checkout.invoice_id)?;
        (checkout, invoice)
    };
    if invoice.status == InvoiceStatus::Paid {
        return Ok(see_other(
            &format!("/account/billing/checkout/return/?invoice={}", invoice.id),
            &[],
        ));
    }
    if !billing::invoice_is_open(&invoice) {
        return Err(SiteError::Conflict("The invoice is not open for payment."));
    }
    deliver_local_payment(&state, &checkout, &invoice).await?;
    Ok(see_other(
        &format!("/account/billing/checkout/return/?invoice={}", invoice.id),
        &[],
    ))
}

async fn deliver_local_payment(
    state: &AppState,
    checkout: &CheckoutRecord,
    invoice: &Invoice,
) -> SiteResult<()> {
    let secret = state
        .config
        .local_webhook_secret
        .as_ref()
        .ok_or(SiteError::Unavailable(
            "Local webhook signing is not configured.",
        ))?;
    if state.payments.provider_kind() != faktor_payments::ProviderKind::Local {
        return Err(SiteError::Unavailable("Local checkout is not enabled."));
    }
    let now = state.now_ms();
    let event_id = format!("evt_local_{}", checkout.id);
    let body = serde_json::json!({
        "id": event_id,
        "type": "payment.succeeded",
        "created": now,
        "data": {"object": {
            "id": checkout.provider_checkout_id.clone().unwrap_or_else(|| checkout.id.clone()),
            "status": "succeeded",
            "amount_minor": invoice.balance_minor(),
            "currency": invoice.currency,
            "metadata": {
                "organization_id": checkout.organization_id,
                "local_invoice_id": invoice.id,
            }
        }}
    });
    let body = serde_json::to_vec(&body).map_err(SiteError::internal)?;
    let signature = faktor_payments::sign_local_webhook(secret.expose(), now / 1000, &body);
    let headers = WebhookHeaders::from_pairs([("x-faktor-signature", signature.as_str())]);
    let outcome = state.payments.handle_webhook(&headers, &body, now).await?;
    match outcome.disposition {
        WebhookDisposition::Applied | WebhookDisposition::Duplicate => {
            if let Ok(conn) = state.db.conn() {
                if let Err(error) =
                    billing::mark_checkout_paid(&conn, &checkout.organization_id, &invoice.id, now)
                {
                    tracing::error!(error = %error, "checkout status mirror failed after settled invoice");
                }
            }
            Ok(())
        }
        other => {
            tracing::warn!(?other, "local webhook was not applied");
            Err(SiteError::Conflict(
                "The local provider event was not applied.",
            ))
        }
    }
}
