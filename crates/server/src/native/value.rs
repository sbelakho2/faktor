//! Value-centric paid entitlement routes (additive native surface):
//!
//! - `GET /native/value-entitlements` — the caller organization's DERIVED
//!   value-entitlement picture: the plan-mapped value entitlements (advanced
//!   verification policy, organization contracts, shared/cross-repo
//!   intelligence, outcome analytics, signed proof/provenance, audit
//!   retention history, model-optimization learning, private runners,
//!   enterprise governance), the effective governance capability flags
//!   (SSO/SCIM as capability vocabulary only) and one typed decision per
//!   entitlement naming its enforcement hook, whether it is granted now and,
//!   when not, the exact cause.
//!
//! The route is inference-independent by construction: it never folds usage,
//! never reads credits and never counts tokens — a 100% local/Ollama
//! organization with zero provider attempts resolves a complete value
//! picture. Contract mirrors the billing routes: daemon password auth plus a
//! control-plane principal (`x-faktor-control-token`, viewer+ via
//! `BillingRead`); with no billing service wired (`[billing]` disabled, the
//! default) the route answers the typed 409 `billing_disabled` — the
//! offline default deny, while the local product is otherwise unchanged.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use super::billing::{billing_disabled, billing_err};
use super::control_plane::require_principal;
use super::{authed, wire_status};
use crate::api::AppState;

/// `GET /native/value-entitlements` — the caller organization's derived
/// value-entitlement report.
pub(crate) async fn native_value_entitlements(
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
    // The report is a BILLING READ (viewer and above), the same role gate as
    // `/native/entitlements`.
    if let Err(denied) = faktor_cloud::authorize(
        &principal,
        &principal.organization,
        faktor_cloud::Resource::Billing,
        faktor_cloud::Action::BillingRead,
    ) {
        return wire_status(billing_err(denied.into()));
    }
    match service.value_entitlements(&principal.organization) {
        Ok(set) => Json(serde_json::json!({
            "ok": true,
            "value_entitlements": {
                "organization_id": set.organization_id,
                "plan_id": set.plan_id,
                "plan_found": set.plan_found,
                "subscription_active": set.subscription_active,
                "plan_entitlements": set.plan_entitlements,
                "governance": set.governance,
                "decisions": set.decisions(),
                "now_ms": set.now_ms,
            }
        }))
        .into_response(),
        Err(e) => wire_status(billing_err(e)),
    }
}

#[cfg(test)]
#[path = "value_tests.rs"]
mod value_tests;
