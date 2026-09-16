//! SSO login routes on the control plane (additive native surface):
//!
//! - `POST /native/sso/start` — resolve the organization's SSO configuration
//!   and mint one authorization URL (+ state) through the wired network OIDC
//!   adapter;
//! - `POST /native/sso/callback` — consume the single-use state, exchange the
//!   authorization code (PKCE), verify the ID token against the login's
//!   nonce, map the verified claims to a membership role and mint ONE
//!   control-plane session (the plaintext session token is returned exactly
//!   once).
//!
//! Both routes ride the daemon password (no control-plane principal exists
//! yet). With no SSO authority wired every route answers a typed 409
//! `sso_disabled`; an organization without an enabled `[enterprise]` SSO
//! configuration answers a typed 409 `sso_not_configured`.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use faktor_cloud::{OidcError, OrganizationId, SsoConfigRef};
use faktor_protocol::error::ApiError;

use super::{authed, malformed_body, wire_status};
use crate::api::AppState;

fn sso_disabled() -> ApiError {
    ApiError {
        code: "sso_disabled",
        message: "the SSO surface is disabled (wire the network OIDC adapter to enable it)".into(),
        http_status: 409,
        retryable: false,
    }
}

fn sso_not_configured() -> ApiError {
    ApiError {
        code: "sso_not_configured",
        message: "the organization has no enabled SSO configuration".into(),
        http_status: 409,
        retryable: false,
    }
}

/// Map the typed OIDC refusal surface onto the frozen API errors. Provider
/// unavailability is a retryable 502; every verification/membership refusal
/// is a non-retryable 401/403. Nothing leaks token material.
fn oidc_err(e: OidcError) -> ApiError {
    let (code, http_status, retryable) = match &e {
        OidcError::StateInvalid => ("sso_state_invalid", 401, false),
        OidcError::RedirectMismatch => ("sso_redirect_mismatch", 401, false),
        OidcError::NonceMismatch => ("sso_nonce_mismatch", 401, false),
        OidcError::BadSignature | OidcError::UnknownKey(_) => ("sso_token_untrusted", 401, false),
        OidcError::Expired | OidcError::NotYetValid => ("sso_token_expired", 401, false),
        OidcError::WrongIssuer { .. } | OidcError::WrongAudience { .. } => {
            ("sso_token_mismatch", 401, false)
        }
        OidcError::Malformed(_) => ("malformed", 400, false),
        OidcError::MembershipRefused(_) => ("sso_membership_refused", 403, false),
        OidcError::DiscoveryUnavailable(_) | OidcError::CodeExchangeRefused(_) => {
            ("oidc_unavailable", 502, true)
        }
    };
    ApiError {
        code,
        message: e.to_string(),
        http_status,
        retryable,
    }
}

/// Resolve the organization's enabled SSO configuration from the enterprise
/// settings (a login bootstrap has no principal; only the SSO reference is
/// read, and a missing organization is indistinguishable from a missing
/// configuration).
fn resolve_sso(state: &AppState, organization: &OrganizationId) -> Result<SsoConfigRef, ApiError> {
    let enterprise = state
        .deps
        .enterprise
        .as_ref()
        .ok_or_else(sso_not_configured)?;
    let settings = enterprise
        .store()
        .org_settings(organization)
        .map_err(|e| ApiError {
            code: "internal",
            message: format!("sso settings read: {e}"),
            http_status: 500,
            retryable: true,
        })?;
    settings
        .and_then(|settings| settings.sso)
        .filter(|sso| sso.enabled)
        .ok_or_else(sso_not_configured)
}

fn sso_authority(state: &AppState) -> Result<&std::sync::Arc<faktor_cloud::SsoLogin>, ApiError> {
    state.deps.sso.as_ref().ok_or_else(sso_disabled)
}

fn parse_organization(raw: &str) -> Result<OrganizationId, ApiError> {
    OrganizationId::try_new(raw.to_string())
        .map_err(|_| malformed_body("organization must be a bounded printable id"))
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SsoStartBody {
    pub(crate) organization: String,
    pub(crate) redirect_uri: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SsoCallbackBody {
    pub(crate) organization: String,
    pub(crate) redirect_uri: String,
    pub(crate) code: String,
    pub(crate) state: String,
}

/// `POST /native/sso/start` — mint one authorization URL for the
/// organization's IdP.
pub(crate) async fn native_sso_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<SsoStartBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid sso start body (strict DTO)")),
    };
    let authority = match sso_authority(&state) {
        Ok(authority) => authority,
        Err(e) => return wire_status(e),
    };
    let organization = match parse_organization(&body.organization) {
        Ok(organization) => organization,
        Err(e) => return wire_status(e),
    };
    let sso = match resolve_sso(&state, &organization) {
        Ok(sso) => sso,
        Err(e) => return wire_status(e),
    };
    match authority
        .start(&organization, &sso, &body.redirect_uri)
        .await
    {
        Ok(start) => Json(serde_json::json!({
            "ok": true,
            "authorizationUrl": start.authorization_url,
            "state": start.state,
        }))
        .into_response(),
        Err(e) => wire_status(oidc_err(e)),
    }
}

/// `POST /native/sso/callback` — complete one login and mint the
/// control-plane session (the token is present exactly once).
pub(crate) async fn native_sso_callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<SsoCallbackBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid sso callback body (strict DTO)")),
    };
    let authority = match sso_authority(&state) {
        Ok(authority) => authority,
        Err(e) => return wire_status(e),
    };
    let Some(control_plane) = state.deps.control_plane.as_ref() else {
        return wire_status(ApiError {
            code: "cloud_disabled",
            message: "the control plane is disabled (enable the [cloud] section)".into(),
            http_status: 409,
            retryable: false,
        });
    };
    let organization = match parse_organization(&body.organization) {
        Ok(organization) => organization,
        Err(e) => return wire_status(e),
    };
    let sso = match resolve_sso(&state, &organization) {
        Ok(sso) => sso,
        Err(e) => return wire_status(e),
    };
    match authority
        .callback(
            control_plane,
            &organization,
            &sso,
            &body.redirect_uri,
            &body.code,
            &body.state,
        )
        .await
    {
        Ok(outcome) => Json(serde_json::json!({
            "ok": true,
            "token": outcome.login.token.expose(),
            "session": outcome.login.session,
            "user": outcome.login.user,
            "role": outcome.login.role,
            "membership": outcome.membership,
        }))
        .into_response(),
        Err(e) => wire_status(oidc_err(e)),
    }
}

#[cfg(test)]
#[path = "sso_tests.rs"]
mod sso_tests;
