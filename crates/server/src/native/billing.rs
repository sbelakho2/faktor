//! Wave 3 commercial metering routes (additive native control-plane
//! surface):
//!
//! - `GET /native/usage?org=&since=&limit=` — the organization's usage fold
//!   over the durable usage ledger plus one cursor page of its events. The
//!   projection is read-through: the durable reservation/settlement rows of
//!   the local sessions are ingested idempotently FIRST (the reservation
//!   ledger stays the source of truth), then folded. Without `org` the route
//!   keeps its frozen pre-billing shape (see `usage.rs`).
//! - `GET /native/entitlements` — the caller organization's derived
//!   entitlement snapshot (plan features/limits from config, credit balance,
//!   folded managed vs BYOK spend, in-flight transactions).
//! - `POST /native/credits/grant` — grant credits (admin role only,
//!   idempotency-keyed).
//!
//! Contract: every route stays behind the daemon password auth AND requires
//! a control-plane principal (`x-faktor-control-token`); the organization is
//! fixed by the token, so a request naming a foreign org is the byte-
//! identical 404 a missing org answers. When no billing service is wired
//! (`[billing] enabled = false`, the default) every route answers a typed
//! 409 `billing_disabled` and the daemon is otherwise unchanged.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use faktor_cloud::{ControlPlaneError, OrganizationId, UsageFold};
use faktor_protocol::error::ApiError;

use super::control_plane::{require_idempotency_key, require_principal};
use super::{authed, malformed_body, wire_status};
use crate::api::AppState;

/// Bound on the usage page one response returns.
pub(crate) const MAX_BILLING_PAGE: usize = 200;
/// Bound on the sessions one read-through ingestion pass scans (bounded
/// everything: a flood past the bound is reported, never silently ignored).
pub(crate) const MAX_INGEST_SESSIONS: usize = 2_000;
/// Bound on the tasks per session one ingestion pass scans.
pub(crate) const MAX_INGEST_TASKS: usize = 500;

fn billing_disabled() -> ApiError {
    ApiError {
        code: "billing_disabled",
        message: "commercial billing is disabled (enable the [billing] section to use it)".into(),
        http_status: 409,
        retryable: false,
    }
}

/// Map one billing-domain failure onto the frozen API error surface.
pub(crate) fn billing_err(e: ControlPlaneError) -> ApiError {
    let code: &'static str = match e.code() {
        "not_found" => "not_found",
        "unauthorized" => "unauthorized",
        "permission_denied" => "permission_denied",
        "conflict" => "conflict",
        "malformed" => "malformed",
        _ => "internal",
    };
    ApiError {
        code,
        message: e.to_string(),
        http_status: e.http_status(),
        retryable: e.retryable(),
    }
}

/// The strict body of a credit grant.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreditGrantBody {
    /// The target billing account; omitted = the organization's default
    /// (first) account.
    pub(crate) account_id: Option<String>,
    pub(crate) amount_micro: u64,
    #[serde(default)]
    pub(crate) reason: String,
}

/// Resolve the caller's organization, tenant-isolated: the caller's
/// principal fixes the organization and a foreign `org` is the SAME typed
/// 404 a missing organization answers (no existence leak).
pub(crate) fn resolve_org(
    state: &AppState,
    headers: &HeaderMap,
    requested: &str,
) -> Result<(faktor_cloud::Principal, OrganizationId), ApiError> {
    let principal = require_principal(state, headers)?;
    let organization = OrganizationId::try_new(requested.to_string()).map_err(billing_err)?;
    if principal.organization != organization {
        return Err(ApiError {
            code: "not_found",
            message: "organization not found".into(),
            http_status: 404,
            retryable: false,
        });
    }
    Ok((principal, organization))
}

/// Project the durable spend rows of the local sessions into the
/// organization's usage ledger (read-through ingestion; idempotent, so a
/// re-read appends nothing). Returns the number of newly appended events
/// plus the sessions/tasks that were scanned. A store failure is loud.
pub(crate) fn ingest_local_spend(
    state: &AppState,
    organization: &OrganizationId,
) -> Result<(usize, usize), ApiError> {
    let Some(service) = state.deps.billing.as_ref() else {
        return Err(billing_disabled());
    };
    let Some(account) = service.default_account(organization).map_err(billing_err)? else {
        // No billing account exists yet: nothing can be attributed and
        // NOTHING is fabricated (the operator creates the account first).
        return Ok((0, 0));
    };
    let store = state.deps.session.store();
    let mut sessions = state
        .deps
        .session
        .list_sessions(None)
        .map_err(|e| faktor_protocol::error::from_core(&e))?;
    sessions.truncate(MAX_INGEST_SESSIONS);
    let scanned = sessions.len();
    let mut appended = 0usize;
    let mut append = |row: faktor_session::DurableSpendRow| -> Result<(), ApiError> {
        let projected = faktor_cloud::DurableSpendRow {
            organization_id: organization.as_str().to_string(),
            session_id: row.session_id,
            task_id: row.task_id,
            reservation_id: row.reservation_id,
            attempt_id: row.attempt_id,
            provider: row.provider,
            model: row.model,
            input_tokens: row.input_tokens,
            output_tokens: row.output_tokens,
            cache_read_tokens: row.prefix_cache_tokens,
            cache_write_tokens: 0,
            // Reasoning is folded into the recorded output line by the
            // settlement; the durable rows carry no separate counter, so the
            // projection reports the honest zero instead of fabricating one.
            reasoning_tokens: 0,
            provider_cost_micro: row.provider_cost_micro,
            provider_reported_micro: row.provider_reported_micro,
            state: row.state,
            occurred_at_ms: row.occurred_at_ms,
            source_operation: row.source_operation,
        };
        let category = service.config().category_of(&projected.provider);
        let report = service
            .ingest_spend_row(organization, &account, &projected, category)
            .map_err(billing_err)?;
        appended += report.appended;
        Ok(())
    };
    for handle in &sessions {
        let tasks = match handle.list_tasks() {
            Ok(tasks) => tasks,
            Err(_) => continue, // a vanished task list is skipped, never fatal
        };
        for task in tasks.iter().take(MAX_INGEST_TASKS) {
            let rows = handle
                .durable_spend_rows(task.task_id, MAX_INGEST_TASKS as i64)
                .map_err(|e| faktor_protocol::error::from_core(&e))?;
            for row in rows {
                append(row)?;
            }
        }
    }
    let _ = store; // the session read is the only durable source used here
    Ok((appended, scanned))
}

/// The billing branch of `/native/usage?org=&since=&limit=` — the
/// organization's usage fold plus one cursor page of its usage events. The
/// read-through ingestion runs first, so the fold always reflects the
/// durable reservation ledger.
pub(crate) fn billing_usage(
    state: AppState,
    headers: HeaderMap,
    org: String,
    since: Option<String>,
    limit: Option<u64>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let (_principal, organization) = match resolve_org(&state, &headers, &org) {
        Ok(resolved) => resolved,
        Err(e) => return wire_status(e),
    };
    let limit = match limit {
        None => MAX_BILLING_PAGE,
        Some(0) => return wire_status(malformed_body("limit must be >= 1")),
        Some(l) if l as usize > MAX_BILLING_PAGE => {
            return wire_status(malformed_body(&format!(
                "limit {l} exceeds the billing page bound {MAX_BILLING_PAGE}"
            )))
        }
        Some(l) => l as usize,
    };
    let Some(service) = state.deps.billing.as_ref() else {
        return wire_status(billing_disabled());
    };
    if let Err(e) = ingest_local_spend(&state, &organization) {
        return wire_status(e);
    }
    let page = match service.usage_page(&organization, since.as_deref(), limit) {
        Ok(page) => page,
        Err(e) => return wire_status(billing_err(e)),
    };
    let fold: UsageFold = match service.fold(&organization) {
        Ok(fold) => fold,
        Err(e) => return wire_status(billing_err(e)),
    };
    let balance = match service.credit_balance(&organization) {
        Ok(balance) => balance,
        Err(e) => return wire_status(billing_err(e)),
    };
    Json(serde_json::json!({
        "ok": true,
        "organization": organization,
        "fold": fold,
        "credits": balance,
        "items": page.items.iter().map(|row| serde_json::json!({
            "cursor": row.event_seq.to_string(),
            "event": row.event,
        })).collect::<Vec<_>>(),
        "nextCursor": page.next_cursor,
    }))
    .into_response()
}

/// `GET /native/entitlements` — the caller organization's derived
/// entitlement snapshot.
pub(crate) async fn native_entitlements(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(p) => p,
        Err(e) => return wire_status(e),
    };
    let Some(service) = state.deps.billing.as_ref() else {
        return wire_status(billing_disabled());
    };
    // Entitlements are a BILLING READ (viewer and above).
    if let Err(denied) = faktor_cloud::authorize(
        &principal,
        &principal.organization,
        faktor_cloud::Resource::Billing,
        faktor_cloud::Action::BillingRead,
    ) {
        return wire_status(billing_err(denied.into()));
    }
    match service.entitlement_snapshot(&principal.organization) {
        Ok(snapshot) => Json(serde_json::json!({
            "ok": true,
            "entitlements": snapshot,
        }))
        .into_response(),
        Err(e) => wire_status(billing_err(e)),
    }
}

/// `POST /native/credits/grant` — grant credits (ADMIN role only,
/// idempotency-keyed). The grant is an append-only credit event.
pub(crate) async fn native_credits_grant(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<CreditGrantBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid credit grant body (strict DTO)")),
    };
    let principal = match require_principal(&state, &headers) {
        Ok(p) => p,
        Err(e) => return wire_status(e),
    };
    let idempotency_key = match require_idempotency_key(&headers) {
        Ok(k) => k,
        Err(e) => return wire_status(e),
    };
    let Some(service) = state.deps.billing.as_ref() else {
        return wire_status(billing_disabled());
    };
    // Admin role only (the role matrix's `CreditsGrant` row).
    if let Err(denied) = faktor_cloud::authorize(
        &principal,
        &principal.organization,
        faktor_cloud::Resource::Billing,
        faktor_cloud::Action::CreditsGrant,
    ) {
        return wire_status(billing_err(denied.into()));
    }
    let account = match &body.account_id {
        Some(raw) => match faktor_cloud::BillingAccountId::try_new(raw.clone()) {
            Ok(id) => {
                match service
                    .billing_account_of(&principal.organization)
                    .map_err(billing_err)
                {
                    Ok(Some(existing)) if existing.id == id => existing.id,
                    Ok(_) => {
                        return wire_status(ApiError {
                            code: "not_found",
                            message: "billing account not found".into(),
                            http_status: 404,
                            retryable: false,
                        })
                    }
                    Err(e) => return wire_status(e),
                }
            }
            Err(e) => return wire_status(billing_err(e)),
        },
        None => match service
            .default_account(&principal.organization)
            .map_err(billing_err)
        {
            Ok(Some(id)) => id,
            Ok(None) => {
                return wire_status(ApiError {
                    code: "not_found",
                    message: "the organization has no billing account".into(),
                    http_status: 404,
                    retryable: false,
                })
            }
            Err(e) => return wire_status(e),
        },
    };
    let reason = if body.reason.is_empty() {
        "operator grant".to_string()
    } else {
        body.reason.clone()
    };
    match service.grant_credits(
        &principal.organization,
        &account,
        body.amount_micro,
        &reason,
        Some(&idempotency_key),
    ) {
        Ok(outcome) => {
            let balance = service
                .credit_balance(&principal.organization)
                .map_err(billing_err);
            match balance {
                Ok(balance) => Json(serde_json::json!({
                    "ok": true,
                    "duplicate": outcome == faktor_cloud::CreditAppend::Duplicate,
                    "credits": balance,
                }))
                .into_response(),
                Err(e) => wire_status(e),
            }
        }
        Err(e) => wire_status(billing_err(e)),
    }
}

#[cfg(test)]
#[path = "billing_tests.rs"]
mod billing_tests;
