//! Additive native payment/invoice/seat/tax/currency routes.
//!
//! Contract: every authenticated route runs the daemon password check, a
//! control-plane principal, the RBAC decision and then the organization
//! resolution BEFORE it touches `state.deps.payments`. A foreign organization
//! is the byte-identical 404 a missing object answers. Mutations are owner-
//! only, idempotency-keyed and strict DTOs. The webhook ingress is the one
//! intentionally unauthenticated route: the configured adapter is the
//! credential boundary (signed body or Mollie id-only fetch + id/org binding),
//! and it resolves authoritative provider truth before durable state is
//! touched.
//!
//! With no `[billing.payments]` section every route answers 409
//! `payments_disabled`; the daemon is otherwise unchanged.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use faktor_cloud::OrganizationId;
use faktor_payments::{
    AddPaymentMethodRequest, CancelRequest, ChangePlanRequest, ChangeSeatsRequest,
    IssueInvoiceRequest, PayInvoiceRequest, PaymentError, PaymentService, RefundRequest,
    SubscribeRequest, UsageCharge, WebhookHeaders,
};
use faktor_protocol::error::ApiError;
use serde::Deserialize;

use super::control_plane::{require_idempotency_key, require_principal};
use super::{authed, malformed_body, wire_status};
use crate::api::AppState;

/// Per-request invoice page bound.
pub(crate) const MAX_PAYMENT_ROUTES_PAGE: usize = 500;

/// Typed refusal when the payment plane is disabled.
pub(crate) fn payments_disabled() -> ApiError {
    ApiError {
        code: "payments_disabled",
        message: "the payment/invoice plane is disabled (enable [billing.payments])".into(),
        http_status: 409,
        retryable: false,
    }
}

/// Map one payment failure onto the frozen API error surface.
pub(crate) fn payments_err(error: PaymentError) -> ApiError {
    ApiError {
        code: error.code(),
        message: error.to_string(),
        http_status: error.http_status(),
        retryable: error.retryable(),
    }
}

fn denied_err(denied: faktor_cloud::Denied) -> ApiError {
    match denied {
        faktor_cloud::Denied::NotFound { .. } => not_found(),
        faktor_cloud::Denied::Forbidden { message } => ApiError {
            code: "permission_denied",
            message,
            http_status: 403,
            retryable: false,
        },
        faktor_cloud::Denied::Malformed { message } => ApiError {
            code: "malformed",
            message,
            http_status: 400,
            retryable: false,
        },
    }
}

fn not_found() -> ApiError {
    ApiError {
        code: "not_found",
        message: "organization not found".into(),
        http_status: 404,
        retryable: false,
    }
}

fn internal() -> ApiError {
    ApiError {
        code: "internal",
        message: "failed to build the payment response".into(),
        http_status: 500,
        retryable: false,
    }
}

/// Authenticate, authorize and only then disclose the wired service. The
/// caller must already have passed [`authed`].
fn authorized(
    state: &AppState,
    headers: &HeaderMap,
    write: bool,
) -> Result<(faktor_cloud::Principal, Arc<PaymentService>), ApiError> {
    let principal = require_principal(state, headers)?;
    let action = if write {
        faktor_cloud::Action::BillingWrite
    } else {
        faktor_cloud::Action::BillingRead
    };
    faktor_cloud::authorize(
        &principal,
        &principal.organization,
        faktor_cloud::Resource::Billing,
        action,
    )
    .map_err(denied_err)?;
    let service = state.deps.payments.as_ref().ok_or_else(payments_disabled)?;
    Ok((principal, service.clone()))
}

/// Resolve the caller's organization; an optional `org` must equal it.
fn scoped_org(
    principal: &faktor_cloud::Principal,
    requested: Option<&str>,
) -> Result<OrganizationId, ApiError> {
    if let Some(requested) = requested {
        let requested = OrganizationId::try_new(requested.to_string()).map_err(|_| not_found())?;
        if requested != principal.organization {
            return Err(not_found());
        }
    }
    Ok(principal.organization.clone())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InvoiceListQuery {
    #[serde(default)]
    pub(crate) org: Option<String>,
    #[serde(default)]
    pub(crate) cursor: Option<String>,
    #[serde(default)]
    pub(crate) limit: Option<usize>,
    #[serde(default)]
    pub(crate) format: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InvoiceQuery {
    #[serde(default)]
    pub(crate) org: Option<String>,
    #[serde(default)]
    pub(crate) format: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReconcileQuery {
    #[serde(default)]
    pub(crate) org: Option<String>,
    #[serde(default)]
    pub(crate) limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PaymentListQuery {
    #[serde(default)]
    pub(crate) org: Option<String>,
    #[serde(default)]
    pub(crate) limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MethodListQuery {
    #[serde(default)]
    pub(crate) org: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubscribeBody {
    pub(crate) plan_id: String,
    pub(crate) interval: faktor_payments::BillingInterval,
    pub(crate) seats: u32,
    #[serde(default)]
    pub(crate) trial_days: Option<u32>,
    pub(crate) customer_email: String,
    pub(crate) customer_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SeatsBody {
    pub(crate) subscription_id: String,
    pub(crate) quantity: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanBody {
    pub(crate) subscription_id: String,
    pub(crate) plan_id: String,
    #[serde(default)]
    pub(crate) interval: Option<faktor_payments::BillingInterval>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CancelBody {
    pub(crate) subscription_id: String,
    pub(crate) at_period_end: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IssueInvoiceBody {
    pub(crate) subscription_id: String,
    #[serde(default)]
    pub(crate) usage: Vec<UsageCharge>,
    #[serde(default)]
    pub(crate) credits_minor: i64,
    #[serde(default)]
    pub(crate) discount_minor: i64,
    #[serde(default)]
    pub(crate) tax_jurisdiction: Option<String>,
    #[serde(default)]
    pub(crate) due_in_days: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PayInvoiceBody {
    pub(crate) payment_method_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RefundBody {
    pub(crate) payment_id: String,
    pub(crate) amount_minor: i64,
    pub(crate) reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RotatePaymentMethodBody {
    pub(crate) token: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AddPaymentMethodBody {
    pub(crate) token: String,
    pub(crate) brand: String,
    pub(crate) last4: String,
    pub(crate) exp_month: u8,
    pub(crate) exp_year: u16,
    #[serde(default)]
    pub(crate) set_default: bool,
}

fn page_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(200).clamp(1, MAX_PAYMENT_ROUTES_PAGE)
}

fn json<T: serde::Serialize>(value: T) -> Response {
    match serde_json::to_value(value) {
        Ok(value) => Json(value).into_response(),
        Err(_) => wire_status(internal()),
    }
}

pub(crate) async fn native_billing_invoices(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<InvoiceListQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(principal) => principal,
        Err(e) => return wire_status(e),
    };
    if let Err(denied) = faktor_cloud::authorize(
        &principal,
        &principal.organization,
        faktor_cloud::Resource::Billing,
        faktor_cloud::Action::BillingRead,
    ) {
        return wire_status(denied_err(denied));
    }
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return wire_status(malformed_body("invalid invoice query (strict DTO)")),
    };
    let organization = match scoped_org(&principal, query.org.as_deref()) {
        Ok(organization) => organization,
        Err(e) => return wire_status(e),
    };
    let Some(service) = state.deps.payments.as_ref() else {
        return wire_status(payments_disabled());
    };
    let format = query.format.as_deref().unwrap_or("json");
    if format == "csv" {
        return match service.export_invoices(&organization, "csv", page_limit(query.limit)) {
            Ok(text) => (
                [
                    (CONTENT_TYPE, "text/csv; charset=utf-8"),
                    (CONTENT_DISPOSITION, "attachment; filename=\"invoices.csv\""),
                ],
                text,
            )
                .into_response(),
            Err(e) => wire_status(payments_err(e)),
        };
    }
    if format != "json" {
        return wire_status(malformed_body("format must be json or csv"));
    }
    match service.invoices(
        &organization,
        query.cursor.as_deref(),
        page_limit(query.limit),
    ) {
        Ok(page) => {
            let mut value = match serde_json::to_value(&page) {
                Ok(value) => value,
                Err(_) => return wire_status(internal()),
            };
            if let Some(object) = value.as_object_mut() {
                object.insert("ok".into(), serde_json::Value::Bool(true));
            }
            Json(value).into_response()
        }
        Err(e) => wire_status(payments_err(e)),
    }
}

pub(crate) async fn native_billing_invoice(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    query: Result<Query<InvoiceQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(principal) => principal,
        Err(e) => return wire_status(e),
    };
    if let Err(denied) = faktor_cloud::authorize(
        &principal,
        &principal.organization,
        faktor_cloud::Resource::Billing,
        faktor_cloud::Action::BillingRead,
    ) {
        return wire_status(denied_err(denied));
    }
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return wire_status(malformed_body("invalid invoice query (strict DTO)")),
    };
    let organization = match scoped_org(&principal, query.org.as_deref()) {
        Ok(organization) => organization,
        Err(e) => return wire_status(e),
    };
    let Some(service) = state.deps.payments.as_ref() else {
        return wire_status(payments_disabled());
    };
    let invoice = match service.invoice(&organization, &id) {
        Ok(invoice) => invoice,
        Err(e) => return wire_status(payments_err(e)),
    };
    match query.format.as_deref().unwrap_or("json") {
        "csv" => (
            [
                (CONTENT_TYPE, "text/csv; charset=utf-8".to_string()),
                (
                    CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{}.csv\"", invoice.number),
                ),
            ],
            service.export_invoice_csv(&invoice),
        )
            .into_response(),
        "json" => {
            let provider_observations =
                match service.provider_invoice_observations(&organization, &invoice.id) {
                    Ok(observations) => observations,
                    Err(e) => return wire_status(payments_err(e)),
                };
            let provider_payments =
                match service.provider_payment_observations(&organization, &invoice.id) {
                    Ok(observations) => observations,
                    Err(e) => return wire_status(payments_err(e)),
                };
            let provider_refunds =
                match service.provider_refund_observations(&organization, &invoice.id) {
                    Ok(observations) => observations,
                    Err(e) => return wire_status(payments_err(e)),
                };
            json(serde_json::json!({
                "ok": true,
                "invoice": invoice,
                "providerObservations": provider_observations,
                "providerPayments": provider_payments,
                "providerRefunds": provider_refunds,
            }))
        }
        _ => wire_status(malformed_body("format must be json or csv")),
    }
}

/// `POST /native/billing/reconcile` — one bounded, read-only provider fetch
/// pass. It records linked observations and typed reconciliation states; it
/// never rewrites a Faktor invoice total or status.
pub(crate) async fn native_billing_reconcile(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<ReconcileQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(principal) => principal,
        Err(e) => return wire_status(e),
    };
    if let Err(denied) = faktor_cloud::authorize(
        &principal,
        &principal.organization,
        faktor_cloud::Resource::Billing,
        faktor_cloud::Action::BillingWrite,
    ) {
        return wire_status(denied_err(denied));
    }
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return wire_status(malformed_body("invalid reconcile query (strict DTO)")),
    };
    let organization = match scoped_org(&principal, query.org.as_deref()) {
        Ok(organization) => organization,
        Err(e) => return wire_status(e),
    };
    let Some(service) = state.deps.payments.as_ref() else {
        return wire_status(payments_disabled());
    };
    match service
        .reconcile(&organization, page_limit(query.limit))
        .await
    {
        Ok(report) => json(serde_json::json!({"ok": true, "reconciliation": report})),
        Err(e) => wire_status(payments_err(e)),
    }
}

pub(crate) async fn native_billing_payments(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<PaymentListQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(principal) => principal,
        Err(e) => return wire_status(e),
    };
    if let Err(denied) = faktor_cloud::authorize(
        &principal,
        &principal.organization,
        faktor_cloud::Resource::Billing,
        faktor_cloud::Action::BillingRead,
    ) {
        return wire_status(denied_err(denied));
    }
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return wire_status(malformed_body("invalid payment query (strict DTO)")),
    };
    let organization = match scoped_org(&principal, query.org.as_deref()) {
        Ok(organization) => organization,
        Err(e) => return wire_status(e),
    };
    let Some(service) = state.deps.payments.as_ref() else {
        return wire_status(payments_disabled());
    };
    match service.payments(&organization, page_limit(query.limit)) {
        Ok(payments) => json(serde_json::json!({"ok": true, "payments": payments})),
        Err(e) => wire_status(payments_err(e)),
    }
}

pub(crate) async fn native_billing_payment_methods(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<MethodListQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(principal) => principal,
        Err(e) => return wire_status(e),
    };
    if let Err(denied) = faktor_cloud::authorize(
        &principal,
        &principal.organization,
        faktor_cloud::Resource::Billing,
        faktor_cloud::Action::BillingRead,
    ) {
        return wire_status(denied_err(denied));
    }
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return wire_status(malformed_body("invalid payment-method query (strict DTO)")),
    };
    let organization = match scoped_org(&principal, query.org.as_deref()) {
        Ok(organization) => organization,
        Err(e) => return wire_status(e),
    };
    let Some(service) = state.deps.payments.as_ref() else {
        return wire_status(payments_disabled());
    };
    match service.payment_methods(&organization) {
        Ok(methods) => json(serde_json::json!({"ok": true, "paymentMethods": methods})),
        Err(e) => wire_status(payments_err(e)),
    }
}

pub(crate) async fn native_billing_tax(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<MethodListQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(principal) => principal,
        Err(e) => return wire_status(e),
    };
    if let Err(denied) = faktor_cloud::authorize(
        &principal,
        &principal.organization,
        faktor_cloud::Resource::Billing,
        faktor_cloud::Action::BillingRead,
    ) {
        return wire_status(denied_err(denied));
    }
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return wire_status(malformed_body("invalid tax query (strict DTO)")),
    };
    let organization = match scoped_org(&principal, query.org.as_deref()) {
        Ok(organization) => organization,
        Err(e) => return wire_status(e),
    };
    let Some(service) = state.deps.payments.as_ref() else {
        return wire_status(payments_disabled());
    };
    let _ = organization;
    let tax = service.tax_surface();
    let mut prices = serde_json::Map::new();
    for (plan_id, price) in &service.config().catalogue.plans {
        prices.insert(
            plan_id.clone(),
            serde_json::json!({
                "currency": price.currency,
                "monthlyMinor": price.monthly_minor,
                "annualMinor": price.annual_minor,
                "perSeatMonthlyMinor": price.per_seat_monthly_minor,
                "perSeatAnnualMinor": price.per_seat_annual_minor,
                "formattedMonthly": service.format_money(price.monthly_minor, &price.currency),
                "formattedAnnual": service.format_money(price.annual_minor, &price.currency),
            }),
        );
    }
    json(serde_json::json!({"ok": true, "tax": tax, "prices": prices}))
}

pub(crate) async fn native_billing_subscribe(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<SubscribeBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(principal) => principal,
        Err(e) => return wire_status(e),
    };
    if let Err(denied) = faktor_cloud::authorize(
        &principal,
        &principal.organization,
        faktor_cloud::Resource::Billing,
        faktor_cloud::Action::BillingWrite,
    ) {
        return wire_status(denied_err(denied));
    }
    let idempotency_key = match require_idempotency_key(&headers) {
        Ok(key) => key,
        Err(e) => return wire_status(e),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid subscribe body (strict DTO)")),
    };
    let Some(service) = state.deps.payments.as_ref() else {
        return wire_status(payments_disabled());
    };
    let request = SubscribeRequest {
        plan_id: body.plan_id,
        interval: body.interval,
        seats: body.seats,
        trial_days: body.trial_days,
        customer_email: body.customer_email,
        customer_name: body.customer_name,
        idempotency_key,
    };
    match service.subscribe(&principal.organization, request).await {
        Ok(outcome) => json(
            serde_json::json!({"ok": true, "subscription": outcome.subscription, "replayed": outcome.replayed}),
        ),
        Err(e) => wire_status(payments_err(e)),
    }
}

pub(crate) async fn native_billing_seats(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<SeatsBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(principal) => principal,
        Err(e) => return wire_status(e),
    };
    if let Err(denied) = faktor_cloud::authorize(
        &principal,
        &principal.organization,
        faktor_cloud::Resource::Billing,
        faktor_cloud::Action::BillingWrite,
    ) {
        return wire_status(denied_err(denied));
    }
    let idempotency_key = match require_idempotency_key(&headers) {
        Ok(key) => key,
        Err(e) => return wire_status(e),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid seats body (strict DTO)")),
    };
    let Some(service) = state.deps.payments.as_ref() else {
        return wire_status(payments_disabled());
    };
    let request = ChangeSeatsRequest {
        subscription_id: body.subscription_id,
        quantity: body.quantity,
        idempotency_key,
    };
    match service.change_seats(&principal.organization, request).await {
        Ok(outcome) => json(serde_json::json!({
            "ok": true,
            "subscription": outcome.subscription,
            "proration": outcome.proration,
            "replayed": outcome.replayed,
        })),
        Err(e) => wire_status(payments_err(e)),
    }
}

pub(crate) async fn native_billing_plan(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<PlanBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(principal) => principal,
        Err(e) => return wire_status(e),
    };
    if let Err(denied) = faktor_cloud::authorize(
        &principal,
        &principal.organization,
        faktor_cloud::Resource::Billing,
        faktor_cloud::Action::BillingWrite,
    ) {
        return wire_status(denied_err(denied));
    }
    let idempotency_key = match require_idempotency_key(&headers) {
        Ok(key) => key,
        Err(e) => return wire_status(e),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid plan body (strict DTO)")),
    };
    let Some(service) = state.deps.payments.as_ref() else {
        return wire_status(payments_disabled());
    };
    let request = ChangePlanRequest {
        subscription_id: body.subscription_id,
        plan_id: body.plan_id,
        interval: body.interval,
        idempotency_key,
    };
    match service.change_plan(&principal.organization, request).await {
        Ok(outcome) => json(serde_json::json!({
            "ok": true,
            "subscription": outcome.subscription,
            "proration": outcome.proration,
            "replayed": outcome.replayed,
        })),
        Err(e) => wire_status(payments_err(e)),
    }
}

pub(crate) async fn native_billing_cancel(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<CancelBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(principal) => principal,
        Err(e) => return wire_status(e),
    };
    if let Err(denied) = faktor_cloud::authorize(
        &principal,
        &principal.organization,
        faktor_cloud::Resource::Billing,
        faktor_cloud::Action::BillingWrite,
    ) {
        return wire_status(denied_err(denied));
    }
    let idempotency_key = match require_idempotency_key(&headers) {
        Ok(key) => key,
        Err(e) => return wire_status(e),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid cancel body (strict DTO)")),
    };
    let Some(service) = state.deps.payments.as_ref() else {
        return wire_status(payments_disabled());
    };
    let request = CancelRequest {
        subscription_id: body.subscription_id,
        at_period_end: body.at_period_end,
        idempotency_key,
    };
    match service.cancel(&principal.organization, request).await {
        Ok(outcome) => json(serde_json::json!({
            "ok": true,
            "subscription": outcome.subscription,
            "replayed": outcome.replayed,
        })),
        Err(e) => wire_status(payments_err(e)),
    }
}

pub(crate) async fn native_billing_issue_invoice(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<IssueInvoiceBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(principal) => principal,
        Err(e) => return wire_status(e),
    };
    if let Err(denied) = faktor_cloud::authorize(
        &principal,
        &principal.organization,
        faktor_cloud::Resource::Billing,
        faktor_cloud::Action::BillingWrite,
    ) {
        return wire_status(denied_err(denied));
    }
    let idempotency_key = match require_idempotency_key(&headers) {
        Ok(key) => key,
        Err(e) => return wire_status(e),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid invoice body (strict DTO)")),
    };
    let Some(service) = state.deps.payments.as_ref() else {
        return wire_status(payments_disabled());
    };
    let request = IssueInvoiceRequest {
        subscription_id: body.subscription_id,
        usage: body.usage,
        credits_minor: body.credits_minor,
        discount_minor: body.discount_minor,
        tax_jurisdiction: body.tax_jurisdiction,
        due_in_days: body.due_in_days,
        idempotency_key,
    };
    match service
        .issue_invoice(&principal.organization, request)
        .await
    {
        Ok(outcome) => json(serde_json::json!({
            "ok": true,
            "invoice": outcome.invoice,
            "replayed": outcome.replayed,
        })),
        Err(e) => wire_status(payments_err(e)),
    }
}

pub(crate) async fn native_billing_pay_invoice(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<PayInvoiceBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(principal) => principal,
        Err(e) => return wire_status(e),
    };
    if let Err(denied) = faktor_cloud::authorize(
        &principal,
        &principal.organization,
        faktor_cloud::Resource::Billing,
        faktor_cloud::Action::BillingWrite,
    ) {
        return wire_status(denied_err(denied));
    }
    let idempotency_key = match require_idempotency_key(&headers) {
        Ok(key) => key,
        Err(e) => return wire_status(e),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid pay body (strict DTO)")),
    };
    let Some(service) = state.deps.payments.as_ref() else {
        return wire_status(payments_disabled());
    };
    let request = PayInvoiceRequest {
        invoice_id: id,
        payment_method_id: body.payment_method_id,
        idempotency_key,
    };
    match service.pay_invoice(&principal.organization, request).await {
        Ok(outcome) => json(serde_json::json!({
            "ok": true,
            "payment": outcome.payment,
            "invoice": outcome.invoice,
            "subscription": outcome.subscription,
            "replayed": outcome.replayed,
        })),
        Err(e) => wire_status(payments_err(e)),
    }
}

pub(crate) async fn native_billing_refund(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<RefundBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(principal) => principal,
        Err(e) => return wire_status(e),
    };
    if let Err(denied) = faktor_cloud::authorize(
        &principal,
        &principal.organization,
        faktor_cloud::Resource::Billing,
        faktor_cloud::Action::BillingWrite,
    ) {
        return wire_status(denied_err(denied));
    }
    let idempotency_key = match require_idempotency_key(&headers) {
        Ok(key) => key,
        Err(e) => return wire_status(e),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid refund body (strict DTO)")),
    };
    let Some(service) = state.deps.payments.as_ref() else {
        return wire_status(payments_disabled());
    };
    let request = RefundRequest {
        payment_id: body.payment_id,
        amount_minor: body.amount_minor,
        reason: body.reason,
        idempotency_key,
    };
    match service.refund(&principal.organization, request).await {
        Ok(outcome) => json(serde_json::json!({
            "ok": true,
            "refund": outcome.refund,
            "creditNote": outcome.credit_note,
            "payment": outcome.payment,
            "invoice": outcome.invoice,
            "replayed": outcome.replayed,
        })),
        Err(e) => wire_status(payments_err(e)),
    }
}

pub(crate) async fn native_billing_add_payment_method(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<AddPaymentMethodBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(principal) => principal,
        Err(e) => return wire_status(e),
    };
    if let Err(denied) = faktor_cloud::authorize(
        &principal,
        &principal.organization,
        faktor_cloud::Resource::Billing,
        faktor_cloud::Action::BillingWrite,
    ) {
        return wire_status(denied_err(denied));
    }
    let idempotency_key = match require_idempotency_key(&headers) {
        Ok(key) => key,
        Err(e) => return wire_status(e),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid payment-method body (strict DTO)")),
    };
    let Some(service) = state.deps.payments.as_ref() else {
        return wire_status(payments_disabled());
    };
    let request = AddPaymentMethodRequest {
        token: body.token,
        brand: body.brand,
        last4: body.last4,
        exp_month: body.exp_month,
        exp_year: body.exp_year,
        set_default: body.set_default,
        idempotency_key,
    };
    match service.add_payment_method(&principal.organization, request) {
        Ok(outcome) => json(serde_json::json!({
            "ok": true,
            "paymentMethod": outcome.payment_method,
            "replayed": outcome.replayed,
        })),
        Err(e) => wire_status(payments_err(e)),
    }
}

pub(crate) async fn native_billing_rotate_payment_method(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<RotatePaymentMethodBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let (principal, service) = match authorized(&state, &headers, true) {
        Ok(value) => value,
        Err(e) => return wire_status(e),
    };
    let idempotency_key = match require_idempotency_key(&headers) {
        Ok(key) => key,
        Err(e) => return wire_status(e),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => {
            return wire_status(malformed_body(
                "invalid payment-method rotate body (strict DTO)",
            ))
        }
    };
    match service.rotate_payment_method_token(
        &principal.organization,
        &id,
        &body.token,
        &idempotency_key,
    ) {
        Ok(outcome) => json(serde_json::json!({
            "ok": true,
            "paymentMethod": outcome.payment_method,
            "replayed": outcome.replayed,
        })),
        Err(e) => wire_status(payments_err(e)),
    }
}

pub(crate) async fn native_billing_remove_payment_method(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let (principal, service) = match authorized(&state, &headers, true) {
        Ok(value) => value,
        Err(e) => return wire_status(e),
    };
    let idempotency_key = match require_idempotency_key(&headers) {
        Ok(key) => key,
        Err(e) => return wire_status(e),
    };
    match service.remove_payment_method(&principal.organization, &id, &idempotency_key) {
        Ok(removed) => json(serde_json::json!({"ok": true, "removed": removed})),
        Err(e) => wire_status(payments_err(e)),
    }
}

/// `POST /native/billing/webhook` — the provider ingress. There is no daemon
/// password: the configured adapter authenticates the delivery (a signed
/// body, or Mollie's id-only payload fetched and id/org-bound through checked
/// egress) before the event id/order/store is consulted.
pub(crate) async fn native_billing_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(service) = state.deps.payments.as_ref() else {
        return wire_status(payments_disabled());
    };
    let pairs: Vec<(&str, &str)> = headers
        .iter()
        .filter_map(|(name, value)| value.to_str().ok().map(|value| (name.as_str(), value)))
        .collect();
    let webhook_headers = WebhookHeaders::from_pairs(pairs);
    match service
        .handle_webhook(&webhook_headers, &body, service.now_ms())
        .await
    {
        Ok(outcome) => Json(serde_json::json!({
            "ok": true,
            "status": match outcome.disposition {
                faktor_payments::WebhookDisposition::Applied => "applied",
                faktor_payments::WebhookDisposition::Duplicate => "duplicate",
                faktor_payments::WebhookDisposition::StaleIgnored => "stale_ignored",
                faktor_payments::WebhookDisposition::Ignored => "ignored",
                faktor_payments::WebhookDisposition::ReconciliationConflict => {
                    "reconciliation_conflict"
                }
            },
            "eventId": outcome.event_id,
            "eventType": outcome.event_type,
        }))
        .into_response(),
        Err(e) => wire_status(payments_err(e)),
    }
}

/// `GET /native/billing/plan-preview` — the exact money of a prospective plan
/// change, using the configured real prices.
pub(crate) async fn native_billing_plan_preview(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<PlanPreviewQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let (principal, service) = match authorized(&state, &headers, false) {
        Ok(value) => value,
        Err(e) => return wire_status(e),
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return wire_status(malformed_body("invalid plan-preview query (strict DTO)")),
    };
    let organization = match scoped_org(&principal, query.org.as_deref()) {
        Ok(organization) => organization,
        Err(e) => return wire_status(e),
    };
    match service.preview_plan_change(
        &organization,
        faktor_payments::PreviewPlanRequest {
            subscription_id: query.subscription_id,
            plan_id: query.plan_id,
            interval: query.interval,
        },
    ) {
        Ok(preview) => json(serde_json::json!({
            "ok": true,
            "proration": preview,
            "text": service.preview_text(&preview),
        })),
        Err(e) => wire_status(payments_err(e)),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlanPreviewQuery {
    #[serde(default)]
    pub(crate) org: Option<String>,
    pub(crate) subscription_id: String,
    pub(crate) plan_id: String,
    #[serde(default)]
    pub(crate) interval: Option<faktor_payments::BillingInterval>,
}

/// `GET /native/billing/seat-preview` — the exact seat-change proration.
pub(crate) async fn native_billing_seat_preview(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<SeatPreviewQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let (principal, service) = match authorized(&state, &headers, false) {
        Ok(value) => value,
        Err(e) => return wire_status(e),
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return wire_status(malformed_body("invalid seat-preview query (strict DTO)")),
    };
    let organization = match scoped_org(&principal, query.org.as_deref()) {
        Ok(organization) => organization,
        Err(e) => return wire_status(e),
    };
    match service.preview_seat_change(
        &organization,
        faktor_payments::PreviewSeatsRequest {
            subscription_id: query.subscription_id,
            quantity: query.quantity,
        },
    ) {
        Ok(preview) => json(serde_json::json!({
            "ok": true,
            "proration": preview,
            "text": service.preview_text(&preview),
        })),
        Err(e) => wire_status(payments_err(e)),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SeatPreviewQuery {
    #[serde(default)]
    pub(crate) org: Option<String>,
    pub(crate) subscription_id: String,
    pub(crate) quantity: u32,
}

#[cfg(test)]
#[path = "billing_payments_tests.rs"]
mod billing_payments_tests;
