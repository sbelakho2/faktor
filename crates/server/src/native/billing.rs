//! Wave 3 commercial metering routes (additive native control-plane
//! surface):
//!
//! - `GET /native/usage?org=&since=&limit=` — the organization's usage fold
//!   over the durable usage ledger plus one cursor page of its events. The
//!   steady-state projection drains the DURABLE SETTLEMENT OUTBOX
//!   incrementally (idempotent by reservation identity); a ONE-TIME bounded
//!   legacy backfill runs only until its durable marker is set. Admission
//!   never depends on this route. Without `org` the route keeps its frozen
//!   pre-billing shape (see `usage.rs`).
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

use axum::body::Body;
use axum::extract::State;
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use faktor_cloud::{
    csv_field, BillingProration, ControlPlaneError, OrganizationId, SubscriptionStatus, UsageFold,
};
use faktor_protocol::error::ApiError;

use super::control_plane::{require_idempotency_key, require_principal};
use super::{authed, malformed_body, money_json, wire_status};
use crate::api::AppState;

/// Bound on the usage page one response returns.
pub(crate) const MAX_BILLING_PAGE: usize = 200;
/// Bound on the sessions the ONE-TIME legacy backfill scans (bounded
/// everything: a flood past the bound is reported, never silently ignored).
pub(crate) const MAX_INGEST_SESSIONS: usize = 2_000;
/// Bound on the tasks per session the ONE-TIME legacy backfill scans (the
/// steady-state outbox projection has no per-task scan).
pub(crate) const MAX_INGEST_TASKS: usize = 500;
/// Bound on the rows one usage export returns (a download is bounded like
/// every other read; older rows stay reachable through `since`).
pub(crate) const MAX_EXPORT_EVENTS: usize = 5_000;
/// Bound on the CSV/JSON export body in bytes.
pub(crate) const MAX_EXPORT_BYTES: usize = 2 * 1024 * 1024;

/// The typed refusal shared by every cloud/billing-gated route when the
/// `[billing]`/`[cloud]` authority is absent: the offline default (value
/// entitlements included) denies typed instead of guessing.
pub(crate) fn billing_disabled() -> ApiError {
    ApiError {
        code: "billing_disabled",
        message: "commercial billing is disabled (enable the [billing] section to use it)".into(),
        http_status: 409,
        retryable: false,
    }
}

/// The typed refusal of the commercial legacy alias: once a billing service
/// is wired, the org-less `/native/usage` and `/native/outcomes` reads would
/// otherwise bypass the organisation scope (and every tenant/plan gate) and
/// serve whole-daemon data to a caller authenticated only by the local
/// password. The commercial read requires the explicit `?org=` scope.
pub(crate) fn org_required() -> ApiError {
    ApiError {
        code: "org_required",
        message: "this commercial read requires the caller organization as ?org=<id>".into(),
        http_status: 400,
        retryable: false,
    }
}

/// The typed refusal of a legacy backfill/outbox drain whose bounded window was
/// exceeded: refusing loudly is the only honest answer (silently folding a
/// prefix would under-report commercial spend).
pub(crate) fn usage_ingest_truncated(detail: &str) -> ApiError {
    ApiError {
        code: "usage_ingest_truncated",
        message: format!(
            "usage ingestion exceeded its bounded window ({detail}); refusing a partially \
             attributed commercial ledger"
        ),
        http_status: 409,
        retryable: false,
    }
}

fn internal_billing_response() -> ApiError {
    ApiError {
        code: "internal",
        message: "failed to build the billing response".into(),
        http_status: 500,
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
        // An authoritative ledger fold refused (typed overflow naming the
        // field): surfaced as its own non-retryable code, never flattened
        // into `internal` and never a saturated total.
        "ledger_refused" => "ledger_refused",
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
    /// Money input: decimal string or legacy JSON integer (see
    /// `faktor_cloud::money`).
    #[serde(with = "faktor_cloud::money")]
    pub(crate) amount_micro: u64,
    #[serde(default)]
    pub(crate) reason: String,
}

/// The native-protocol projection of one usage-fold bucket: identical field
/// names and values except the monetary fields, which are decimal strings
/// (see `faktor_cloud::money`). The shared [`faktor_cloud::UsageTotals`]
/// keeps its own numeric serde shape for the external vendor report payload.
#[derive(serde::Serialize)]
struct NativeUsageTotals {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_write_tokens: u64,
    reasoning_tokens: u64,
    #[serde(with = "faktor_cloud::money")]
    provider_cost_micro: u64,
    #[serde(with = "faktor_cloud::money")]
    managed_cost_micro: u64,
    #[serde(with = "faktor_cloud::money")]
    byok_cost_micro: u64,
    events: u64,
    corrected_events: u64,
}

impl From<&faktor_cloud::UsageTotals> for NativeUsageTotals {
    fn from(totals: &faktor_cloud::UsageTotals) -> Self {
        Self {
            input_tokens: totals.input_tokens,
            output_tokens: totals.output_tokens,
            cache_read_tokens: totals.cache_read_tokens,
            cache_write_tokens: totals.cache_write_tokens,
            reasoning_tokens: totals.reasoning_tokens,
            provider_cost_micro: totals.provider_cost_micro,
            managed_cost_micro: totals.managed_cost_micro,
            byok_cost_micro: totals.byok_cost_micro,
            events: totals.events,
            corrected_events: totals.corrected_events,
        }
    }
}

#[derive(serde::Serialize)]
struct NativeTaskUsage {
    task_id: u64,
    run_id: String,
    totals: NativeUsageTotals,
}

/// The native-protocol projection of [`faktor_cloud::UsageFold`].
#[derive(serde::Serialize)]
struct NativeUsageFold {
    organization_id: faktor_cloud::OrganizationId,
    totals: NativeUsageTotals,
    per_task: Vec<NativeTaskUsage>,
    next_cursor: Option<String>,
}

impl From<&faktor_cloud::UsageFold> for NativeUsageFold {
    fn from(fold: &faktor_cloud::UsageFold) -> Self {
        Self {
            organization_id: fold.organization_id.clone(),
            totals: NativeUsageTotals::from(&fold.totals),
            per_task: fold
                .per_task
                .iter()
                .map(|task| NativeTaskUsage {
                    task_id: task.task_id,
                    run_id: task.run_id.clone(),
                    totals: NativeUsageTotals::from(&task.totals),
                })
                .collect(),
            next_cursor: fold.next_cursor.clone(),
        }
    }
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

/// Drain the durable settlement outbox into the organization's usage
/// ledger (bounded). This is the ONLY steady-state projection: it reads
/// settled outbox rows incrementally by watermark, never every session/task/
/// reservation. A backlog past the bound is a typed refusal.
pub(crate) fn project_settlement_outbox(
    state: &AppState,
    organization: &OrganizationId,
    max_rows: usize,
) -> Result<crate::billing_projection::ProjectionReport, ApiError> {
    let Some(service) = state.deps.billing.as_ref() else {
        return Err(billing_disabled());
    };
    let projector = crate::billing_projection::BillingSpendProjector::new(
        service.clone(),
        organization.clone(),
        state.deps.session.clone(),
    );
    projector.drain(max_rows).map_err(projection_err)
}

fn projection_err(error: crate::billing_projection::ProjectionError) -> ApiError {
    use crate::billing_projection::ProjectionError;
    match error {
        ProjectionError::Lag { projected } => usage_ingest_truncated(&format!(
            "{projected} outbox rows projected and more remain"
        )),
        ProjectionError::Outbox(detail) => ApiError {
            code: "usage_projection_failed",
            message: format!("settlement outbox projection failed: {detail}"),
            http_status: 500,
            retryable: true,
        },
        ProjectionError::Billing(error) => billing_err(error),
    }
}

/// One-time LEGACY backfill (UI-triggered, bounded, never on the admission
/// path): scan the pre-outbox settled reservation rows into the durable
/// outbox so the ordinary projector attributes them. Idempotent by the
/// reservation identity. A typed `NotFound` (concurrent session deletion) is
/// the ONLY skippable `list_tasks` failure — any store/corruption/I/O error
/// refuses the whole pass instead of under-reporting spend.
pub(crate) fn backfill_local_spend_with_bounds(
    state: &AppState,
    organization: &OrganizationId,
    max_sessions: usize,
    max_tasks: usize,
) -> Result<(usize, usize), ApiError> {
    let Some(service) = state.deps.billing.as_ref() else {
        return Err(billing_disabled());
    };
    // The local sessions belong to the configured billing organization.
    // Without a configured organization (embedded/test service) the backfill
    // is allowed so the provisioning contract stays usable.
    let owns_local_sessions = service
        .billing_organization()
        .map(|configured| configured == organization)
        .unwrap_or(true);
    if !owns_local_sessions {
        return Ok((0, 0));
    }
    let Some(_account) = service.default_account(organization).map_err(billing_err)? else {
        // No billing account exists yet: nothing can be attributed and
        // NOTHING is fabricated (the operator creates the account first).
        return Ok((0, 0));
    };
    let store = state.deps.session.store();
    if store.billing_outbox_backfill_done().map_err(|e| {
        faktor_protocol::error::from_core(&faktor_core::Error::new(
            faktor_core::ErrorKind::Internal,
            e.to_string(),
        ))
    })? {
        return Ok((0, 0));
    }
    let sessions = state
        .deps
        .session
        .list_sessions(None)
        .map_err(|e| faktor_protocol::error::from_core(&e))?;
    if sessions.len() > max_sessions {
        return Err(usage_ingest_truncated(&format!(
            "{} sessions exceed the {max_sessions}-session bound",
            sessions.len()
        )));
    }
    // Preflight the per-session task windows BEFORE writing anything, so the
    // refusal leaves the outbox byte-identical. A typed NotFound (the
    // session was deleted concurrently) is skippable; every other error is
    // fatal.
    for handle in &sessions {
        match handle.list_tasks() {
            Ok(tasks) if tasks.len() > max_tasks => {
                return Err(usage_ingest_truncated(&format!(
                    "a session has {} tasks, exceeding the {max_tasks}-task bound",
                    tasks.len()
                )));
            }
            Ok(_) => {}
            Err(e) if e.kind == faktor_core::ErrorKind::NotFound => {}
            Err(e) => return Err(faktor_protocol::error::from_core(&e)),
        }
    }
    let scanned = sessions.len();
    let mut inserted = 0usize;
    for handle in &sessions {
        let tasks = match handle.list_tasks() {
            Ok(tasks) => tasks,
            Err(e) if e.kind == faktor_core::ErrorKind::NotFound => continue,
            Err(e) => return Err(faktor_protocol::error::from_core(&e)),
        };
        if tasks.len() > max_tasks {
            return Err(usage_ingest_truncated(&format!(
                "a session has {} tasks, exceeding the {max_tasks}-task bound",
                tasks.len()
            )));
        }
        for task in tasks.iter() {
            let rows = handle
                .durable_spend_rows(task.task_id, faktor_session::MAX_DURABLE_SPEND_ROWS)
                .map_err(|e| faktor_protocol::error::from_core(&e))?;
            for row in rows {
                let fact = faktor_store::BillingOutboxFact {
                    reservation_id: row.reservation_id,
                    session_id: handle.id(),
                    task_id: task.task_id,
                    attempt_id: row.attempt_id,
                    provider: row.provider,
                    model: row.model,
                    // Legacy durable rows fold cache/reasoning into the
                    // input/output totals before persistence; the exact
                    // split is unavailable, so it is honestly reported as
                    // uncached input rather than fabricated cache lines.
                    uncached_input_tokens: row.input_tokens,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                    output_tokens: row.output_tokens,
                    reasoning_tokens: 0,
                    provider_cost_micro: row.provider_cost_micro,
                    provider_reported_micro: (row.provider_reported_micro > 0)
                        .then_some(row.provider_reported_micro),
                    occurred_at_ms: row.occurred_at_ms,
                    source_operation: format!("legacy_backfill:{}", row.source_operation),
                };
                if store.billing_outbox_record(fact).map_err(|e| {
                    faktor_protocol::error::from_core(&faktor_core::Error::new(
                        faktor_core::ErrorKind::Internal,
                        e.to_string(),
                    ))
                })? {
                    inserted += 1;
                }
            }
        }
    }
    store.mark_billing_outbox_backfill_done().map_err(|e| {
        faktor_protocol::error::from_core(&faktor_core::Error::new(
            faktor_core::ErrorKind::Internal,
            e.to_string(),
        ))
    })?;
    Ok((inserted, scanned))
}

/// The UI/route entry point: run the one-time legacy backfill exactly once,
/// then drain a bounded outbox window. Admission uses the projector directly
/// and never calls this.
pub(crate) fn catch_up_local_spend(
    state: &AppState,
    organization: &OrganizationId,
) -> Result<(usize, usize), ApiError> {
    let (inserted, scanned) = backfill_local_spend_with_bounds(
        state,
        organization,
        MAX_INGEST_SESSIONS,
        MAX_INGEST_TASKS,
    )?;
    let report = project_settlement_outbox(
        state,
        organization,
        crate::billing_projection::MAX_CATCHUP_OUTBOX_ROWS,
    )?;
    Ok((inserted + report.appended, scanned))
}

/// The billing branch of `/native/usage?org=&since=&limit=` — the
/// organization's usage fold plus one cursor page of its usage events. It
/// first runs the one-time legacy backfill (UI catch-up only) and then a
/// bounded settlement-outbox projection, so the fold reflects every settled
/// reservation without re-listing session history.
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
    let (principal, organization) = match resolve_org(&state, &headers, &org) {
        Ok(resolved) => resolved,
        Err(e) => return wire_status(e),
    };
    // Authorization FIRST: an underprivileged principal must not be able to
    // probe request-shape validity (or the billing feature state) before the
    // role gate.
    if let Err(denied) = faktor_cloud::authorize(
        &principal,
        &principal.organization,
        faktor_cloud::Resource::Billing,
        faktor_cloud::Action::BillingRead,
    ) {
        return wire_status(billing_err(denied.into()));
    }
    let limit = match limit {
        None => MAX_BILLING_PAGE,
        Some(0) => return wire_status(malformed_body("limit must be >= 1")),
        Some(l) if l as usize > MAX_BILLING_PAGE => {
            return wire_status(malformed_body(&format!(
                "limit {l} exceeds the billing page bound {MAX_BILLING_PAGE}"
            )));
        }
        Some(l) => l as usize,
    };
    let Some(service) = state.deps.billing.as_ref() else {
        return wire_status(billing_disabled());
    };
    if let Err(e) = catch_up_local_spend(&state, &organization) {
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
    money_json(serde_json::json!({
        "ok": true,
        "organization": organization,
        "fold": NativeUsageFold::from(&fold),
        "credits": balance,
        "items": page.items.iter().map(|row| serde_json::json!({
            "cursor": row.event_seq.to_string(),
            "event": row.event,
        })).collect::<Vec<_>>(),
        "nextCursor": page.next_cursor,
    }))
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
    // Entitlements are a BILLING READ (viewer and above): authorize before
    // the billing feature state is disclosed.
    if let Err(denied) = faktor_cloud::authorize(
        &principal,
        &principal.organization,
        faktor_cloud::Resource::Billing,
        faktor_cloud::Action::BillingRead,
    ) {
        return wire_status(billing_err(denied.into()));
    }
    let Some(service) = state.deps.billing.as_ref() else {
        return wire_status(billing_disabled());
    };
    match service.entitlement_snapshot(&principal.organization) {
        Ok(snapshot) => money_json(serde_json::json!({
            "ok": true,
            "entitlements": snapshot,
        })),
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
    let principal = match require_principal(&state, &headers) {
        Ok(p) => p,
        Err(e) => return wire_status(e),
    };
    // Admin role only (the role matrix's `CreditsGrant` row), decided BEFORE
    // the body, the idempotency header or the billing feature state: an
    // underprivileged principal learns nothing about request validity.
    if let Err(denied) = faktor_cloud::authorize(
        &principal,
        &principal.organization,
        faktor_cloud::Resource::Billing,
        faktor_cloud::Action::CreditsGrant,
    ) {
        return wire_status(billing_err(denied.into()));
    }
    let Some(service) = state.deps.billing.as_ref() else {
        return wire_status(billing_disabled());
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid credit grant body (strict DTO)")),
    };
    let idempotency_key = match require_idempotency_key(&headers) {
        Ok(k) => k,
        Err(e) => return wire_status(e),
    };
    let account = match &body.account_id {
        Some(raw) => match faktor_cloud::BillingAccountId::try_new(raw.clone()) {
            Ok(id) => {
                // Resolve the NAMED account of this organization, not just
                // the default one: an organization may own several accounts,
                // and a grant must target the requested ledger identity (or
                // refuse) instead of silently redirecting to the first.
                match service
                    .store()
                    .billing_account(&principal.organization, &id)
                {
                    Ok(Some(existing)) => existing.id,
                    Ok(None) => {
                        return wire_status(ApiError {
                            code: "not_found",
                            message: "billing account not found".into(),
                            http_status: 404,
                            retryable: false,
                        });
                    }
                    Err(e) => return wire_status(billing_err(e.into())),
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
                });
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
                Ok(balance) => money_json(serde_json::json!({
                    "ok": true,
                    "duplicate": outcome == faktor_cloud::CreditAppend::Duplicate,
                    "credits": balance,
                })),
                Err(e) => wire_status(e),
            }
        }
        Err(e) => wire_status(billing_err(e)),
    }
}

/// The strict body of a subscription change/cancel (`POST
/// /native/billing/subscription`). `change` applies an upgrade or downgrade;
/// `cancel` ends the subscription (and requires `confirm: true`).
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SubscriptionChangeBody {
    pub(crate) action: String,
    #[serde(default)]
    pub(crate) plan_id: Option<String>,
    #[serde(default)]
    pub(crate) status: Option<String>,
    #[serde(default)]
    pub(crate) expires_ms: Option<i64>,
    #[serde(default)]
    pub(crate) confirm: Option<bool>,
}

/// The validated subscription operation. Validation happens BEFORE the
/// idempotency key is claimed, so a malformed request never burns a key.
enum SubscriptionAction {
    Change {
        plan_id: String,
        status: SubscriptionStatus,
        expires_ms: Option<i64>,
    },
    Cancel,
}

impl SubscriptionAction {
    fn canonical(&self) -> String {
        match self {
            SubscriptionAction::Change {
                plan_id,
                status,
                expires_ms,
            } => format!(
                "change;plan={plan_id};status={};expires={}",
                status.as_str(),
                expires_ms
                    .map(|value| value.to_string())
                    .unwrap_or_default()
            ),
            SubscriptionAction::Cancel => "cancel".to_string(),
        }
    }
}

fn parse_subscription_action(
    body: &SubscriptionChangeBody,
) -> Result<SubscriptionAction, faktor_protocol::error::ApiError> {
    match body.action.as_str() {
        "change" => {
            let Some(plan_id) = body.plan_id.as_deref().filter(|id| !id.trim().is_empty()) else {
                return Err(malformed_body("change requires a non-empty plan_id"));
            };
            let status = match body.status.as_deref().unwrap_or("active") {
                "active" => SubscriptionStatus::Active,
                "trialing" => SubscriptionStatus::Trialing,
                "expired" => SubscriptionStatus::Expired,
                "canceled" => {
                    return Err(malformed_body(
                        "use the cancel action to cancel a subscription",
                    ))
                }
                other => {
                    return Err(malformed_body(&format!(
                        "unknown subscription status {other:?} (active|trialing|expired)"
                    )))
                }
            };
            Ok(SubscriptionAction::Change {
                plan_id: plan_id.to_string(),
                status,
                expires_ms: body.expires_ms,
            })
        }
        "cancel" => {
            if body.plan_id.is_some() || body.status.is_some() || body.expires_ms.is_some() {
                return Err(malformed_body(
                    "cancel carries no plan_id, status or expires_ms",
                ));
            }
            if body.confirm != Some(true) {
                return Err(malformed_body(
                    "cancel requires {\"confirm\": true}: cancellation denies new admissions",
                ));
            }
            Ok(SubscriptionAction::Cancel)
        }
        other => Err(malformed_body(&format!(
            "unknown subscription action {other:?} (change|cancel)"
        ))),
    }
}

/// `GET /native/billing/overview` — the caller organization's durable
/// commercial account: account + subscription + plan resolution, the derived
/// trial view, and the HONEST availability of the capabilities this build
/// does not implement (payment, invoices, seats, tax, currency). Usage export
/// is reported available.
pub(crate) async fn native_billing_overview(
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
    if let Err(denied) = faktor_cloud::authorize(
        &principal,
        &principal.organization,
        faktor_cloud::Resource::Billing,
        faktor_cloud::Action::BillingRead,
    ) {
        return wire_status(billing_err(denied.into()));
    }
    let Some(service) = state.deps.billing.as_ref() else {
        return wire_status(billing_disabled());
    };
    match service.overview(&principal.organization) {
        Ok(overview) => money_json(serde_json::json!({
            "ok": true,
            "billing": overview,
        })),
        Err(e) => wire_status(billing_err(e)),
    }
}

/// `POST /native/billing/subscription` — change (upgrade/downgrade) or cancel
/// the organization's subscription. OWNER only (`BillingWrite`), idempotency-
/// keyed, strict DTO. The effect is durable and immediate at the next
/// admission boundary. Proration is reported unavailable because the plan
/// table carries no price; no money delta is invented.
pub(crate) async fn native_billing_subscription(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<SubscriptionChangeBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(p) => p,
        Err(e) => return wire_status(e),
    };
    // OWNER-only (`BillingWrite`) BEFORE the body/idempotency parsing and
    // BEFORE the billing feature state: an underprivileged principal learns
    // neither request validity nor whether `[billing]` is wired.
    if let Err(denied) = faktor_cloud::authorize(
        &principal,
        &principal.organization,
        faktor_cloud::Resource::Billing,
        faktor_cloud::Action::BillingWrite,
    ) {
        return wire_status(billing_err(denied.into()));
    }
    let idempotency_key = match require_idempotency_key(&headers) {
        Ok(k) => k,
        Err(e) => return wire_status(e),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid subscription body (strict DTO)")),
    };
    let Some(service) = state.deps.billing.as_ref() else {
        return wire_status(billing_disabled());
    };
    // Validate the request shape BEFORE claiming the key: a malformed body
    // never burns an idempotency key.
    let action = match parse_subscription_action(&body) {
        Ok(action) => action,
        Err(e) => return wire_status(e),
    };
    // Semantic validation also happens before the key is claimed, so an
    // undefined plan or an unbounded trial never burns an idempotency key.
    if let SubscriptionAction::Change {
        plan_id,
        status,
        expires_ms,
    } = &action
    {
        if service.config().plan(plan_id).is_none() {
            return wire_status(malformed_body(&format!(
                "plan {plan_id:?} is not defined in the billing plan table"
            )));
        }
        if *status == SubscriptionStatus::Trialing && expires_ms.is_none() {
            return wire_status(malformed_body(
                "a trialing subscription requires expires_ms",
            ));
        }
        if expires_ms.is_some_and(|value| value < 0) {
            return wire_status(malformed_body("expires_ms must be >= 0"));
        }
    }
    // A missing account is a precondition, not a mutation: refuse it
    // BEFORE claiming the idempotency key so a 404 never burns a key (a
    // later, different request under the same key must not be a conflict
    // for an effect that never happened).
    if let Err(e) = service
        .default_account(&principal.organization)
        .map_err(billing_err)
        .and_then(|account| {
            account.map(|_| ()).ok_or_else(|| ApiError {
                code: "not_found",
                message: "the organization has no billing account".into(),
                http_status: 404,
                retryable: false,
            })
        })
    {
        return wire_status(e);
    }
    // The idempotency key is DURABLE and request-bound: the same key with a
    // byte-identical canonical request replays the recorded response, while a
    // different request under the same key is a typed conflict with NO
    // mutation (an ignored key would let a retried/altered request apply).
    let canonical_request = action.canonical();
    let mutation_claim = service.store().claim_billing_mutation(
        &principal.organization,
        &idempotency_key,
        &canonical_request,
        service.now_ms(),
    );
    match mutation_claim {
        Ok(faktor_cloud::BillingMutationClaim::Replay {
            response: Some(recorded),
        }) => {
            let mut value: serde_json::Value = match serde_json::from_str(&recorded) {
                Ok(value) => value,
                Err(_) => return wire_status(internal_billing_response()),
            };
            // The replay is flagged without changing the recorded effect.
            value["replayed"] = serde_json::Value::Bool(true);
            return Json(value).into_response();
        }
        Ok(faktor_cloud::BillingMutationClaim::Replay { response: None })
        | Ok(faktor_cloud::BillingMutationClaim::Fresh) => {}
        Ok(faktor_cloud::BillingMutationClaim::Conflict { .. }) => {
            return wire_status(ApiError {
                code: "idempotency_conflict",
                message: "the idempotency key was already used for a different subscription \
                          change (nothing was written)"
                    .into(),
                http_status: 409,
                retryable: false,
            });
        }
        Err(e) => return wire_status(billing_err(e.into())),
    }
    let outcome = match action {
        SubscriptionAction::Change {
            plan_id,
            status,
            expires_ms,
        } => service.change_subscription(&principal.organization, &plan_id, status, expires_ms),
        SubscriptionAction::Cancel => service.cancel_subscription(&principal.organization),
    };
    let (subscription, replayed) = match outcome {
        Ok(value) => value,
        Err(e) => return wire_status(billing_err(e)),
    };
    let overview = match service.overview(&principal.organization) {
        Ok(overview) => overview,
        Err(e) => return wire_status(billing_err(e)),
    };
    let mut value = serde_json::json!({
        "ok": true,
        "replayed": replayed,
        "subscription": subscription,
        "proration": BillingProration::unavailable_no_prices(),
        "billing": overview,
    });
    faktor_cloud::money::stringify_money_fields(&mut value);
    let encoded = value.to_string();
    if let Err(e) =
        service
            .store()
            .finish_billing_mutation(&principal.organization, &idempotency_key, &encoded)
    {
        return wire_status(billing_err(e.into()));
    }
    Json(value).into_response()
}

/// The strict query of `GET /native/usage/export`.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UsageExportQuery {
    pub(crate) org: String,
    #[serde(default)]
    pub(crate) since: Option<String>,
    #[serde(default)]
    pub(crate) limit: Option<u64>,
    #[serde(default)]
    pub(crate) format: Option<String>,
}

/// `GET /native/usage/export?org=&since=&limit=&format=csv|json` — a BOUNDED
/// download of the organization's usage ledger. It runs the same bounded
/// backfill/outbox catch-up as `/native/usage`, then pages at most
/// [`MAX_EXPORT_EVENTS`] rows. The response is an attachment; no unbounded
/// ledger is ever buffered.
pub(crate) async fn native_usage_export(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<axum::extract::Query<UsageExportQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let axum::extract::Query(query) = match query {
        Ok(query) => query,
        Err(_) => return wire_status(malformed_body("invalid usage export query")),
    };
    let (principal, organization) = match resolve_org(&state, &headers, &query.org) {
        Ok(resolved) => resolved,
        Err(e) => return wire_status(e),
    };
    if let Err(denied) = faktor_cloud::authorize(
        &principal,
        &principal.organization,
        faktor_cloud::Resource::Billing,
        faktor_cloud::Action::BillingRead,
    ) {
        return wire_status(billing_err(denied.into()));
    }
    let Some(service) = state.deps.billing.as_ref() else {
        return wire_status(billing_disabled());
    };
    let format = query.format.as_deref().unwrap_or("json");
    if format != "csv" && format != "json" {
        return wire_status(malformed_body("format must be csv or json"));
    }
    let limit = match query.limit {
        None => MAX_EXPORT_EVENTS,
        Some(0) => return wire_status(malformed_body("limit must be >= 1")),
        Some(l) if l as usize > MAX_EXPORT_EVENTS => {
            return wire_status(malformed_body(&format!(
                "limit {l} exceeds the export bound {MAX_EXPORT_EVENTS}"
            )));
        }
        Some(l) => l as usize,
    };
    if let Err(e) = catch_up_local_spend(&state, &organization) {
        return wire_status(e);
    }
    let mut rows = Vec::with_capacity(limit.min(MAX_BILLING_PAGE));
    let mut cursor = query.since;
    let mut truncated = false;
    while rows.len() < limit {
        let page = match service.usage_page(
            &organization,
            cursor.as_deref(),
            MAX_BILLING_PAGE.min(limit - rows.len()),
        ) {
            Ok(page) => page,
            Err(e) => return wire_status(billing_err(e)),
        };
        if page.items.is_empty() {
            cursor = None;
            break;
        }
        for item in page.items {
            if rows.len() == limit {
                truncated = true;
                break;
            }
            rows.push(item);
        }
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    if cursor.is_some() && rows.len() == limit {
        truncated = true;
    }
    let (body, content_type) = if format == "csv" {
        let mut text = String::new();
        text.push_str(
            "cursor,event_id,task_id,run_id,attempt_id,provider,model,unit,quantity,provider_cost_micro,category,reconciliation_state,occurred_at_ms,source_operation,correction_of\n",
        );
        for row in &rows {
            let event = &row.event;
            let fields = [
                row.event_seq.to_string(),
                event.id.as_str().to_string(),
                event.task_id.to_string(),
                event.run_id.clone(),
                event.attempt_id.clone(),
                event.provider.clone(),
                event.model.clone(),
                event.unit.as_str().to_string(),
                event.quantity.to_string(),
                event.provider_cost_micro.to_string(),
                event.category.as_str().to_string(),
                event.reconciliation_state.as_str().to_string(),
                event.occurred_at_ms.to_string(),
                event.source_operation.clone(),
                event
                    .correction_of
                    .as_ref()
                    .map(|id| id.as_str().to_string())
                    .unwrap_or_default(),
            ];
            text.push_str(&fields.map(|field| csv_field(&field)).join(","));
            text.push('\n');
            if text.len() > MAX_EXPORT_BYTES {
                return wire_status(ApiError {
                    code: "export_too_large",
                    message: format!(
                        "the bounded export exceeded {MAX_EXPORT_BYTES} bytes; narrow the window with `since` or `limit`"
                    ),
                    http_status: 413,
                    retryable: false,
                });
            }
        }
        (text, "text/csv; charset=utf-8")
    } else {
        let items: Vec<serde_json::Value> = rows
            .iter()
            .map(|row| {
                serde_json::json!({
                    "cursor": row.event_seq.to_string(),
                    "event": &row.event,
                })
            })
            .collect();
        let value = serde_json::json!({
            "ok": true,
            "organization": organization,
            "items": items,
            "next_cursor": cursor,
            "truncated": truncated,
        });
        let text = value.to_string();
        if text.len() > MAX_EXPORT_BYTES {
            return wire_status(ApiError {
                code: "export_too_large",
                message: format!(
                    "the bounded export exceeded {MAX_EXPORT_BYTES} bytes; narrow the window with `since` or `limit`"
                ),
                http_status: 413,
                retryable: false,
            });
        }
        (text, "application/json")
    };
    let filename: String = organization
        .as_str()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, content_type)
        .header(
            CONTENT_DISPOSITION,
            format!("attachment; filename=\"faktor-usage-{filename}.{format}\""),
        )
        .body(Body::from(body))
        .unwrap_or_else(|_| {
            wire_status(ApiError {
                code: "internal",
                message: "failed to build the usage export response".into(),
                http_status: 500,
                retryable: false,
            })
        })
}

#[cfg(test)]
#[path = "billing_tests.rs"]
mod billing_tests;
