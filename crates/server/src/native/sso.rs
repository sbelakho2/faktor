//! SSO login routes on the control plane (additive native surface):
//!
//! - `POST /native/sso/start` — resolve the organization's SSO configuration
//!   and mint one authorization URL (+ state) through the wired network OIDC
//!   adapter;
//! - `POST /native/sso/callback` — consume the single-use state, exchange the
//!   authorization code (PKCE), verify the ID token against the login's
//!   nonce, map the verified claims to a membership role and mint ONE
//!   control-plane session (the plaintext session token is returned exactly
//!   once);
//! - `POST /native/sso/logout` — durably revoke the PRESENTED control-plane
//!   session: the session's own token rides `x-faktor-control-token` (the
//!   same header every control-plane principal uses) and the body names the
//!   `{organization, session_id}` it must own. Revocation is a durable row
//!   update, never a delete: every later presentation of the token is
//!   refused 401 by `ControlPlane::authenticate`. The route needs the
//!   control plane only (not the OIDC adapter): a session minted by
//!   bootstrap, invitation or SSO is revocable through it. A second logout
//!   of an already-revoked session is the typed idempotent replay; a
//!   foreign-organization session id is the same typed 404 a missing one
//!   answers.
//!
//! PRINCIPAL LAW (oracle closure): start and callback require a
//! control-plane PRINCIPAL (`x-faktor-control-token`), and the body's
//! `organization` must equal the PRINCIPAL's own organization. The daemon
//! password alone is not a principal: an unauthenticated daemon-password
//! caller can no longer probe arbitrary organization ids (plan/SSO-existence
//! oracle). A mismatch answers the SAME typed refusal as a nonexistent
//! organization and names only the CALLER's own organization — never the
//! requested one.
//!
//! All routes ride the daemon password. With no SSO authority wired,
//! start/callback answer a typed 409 `sso_disabled`; an organization without
//! an enabled `[enterprise]` SSO configuration answers a typed 409
//! `sso_not_configured`. Every route resolves the control-plane credential
//! FIRST: start/callback require a principal (401 when missing/unknown), and
//! logout requires the presented session token (401 when missing/unknown,
//! even with no control plane wired).

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use faktor_cloud::{
    AuthSessionId, ControlPlaneError, GovernanceCapability, OidcError, OrganizationId, SsoConfigRef,
};
use faktor_protocol::error::ApiError;

use super::billing::billing_disabled;
use super::control_plane::{
    control_plane_err, require_principal, CONTROL_TOKEN_HEADER, MAX_CONTROL_HEADER_BYTES,
};
use super::{authed, malformed_body, value_entitlement_refused, wire_status};
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
        OidcError::Expired | OidcError::NotYetValid | OidcError::TimestampOutOfRange { .. } => {
            ("sso_token_expired", 401, false)
        }
        OidcError::WrongIssuer { .. }
        | OidcError::WrongAudience { .. }
        | OidcError::WrongAzp { .. }
        | OidcError::AlgorithmRefused { .. } => ("sso_token_mismatch", 401, false),
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

/// Require the presented control-plane PRINCIPAL and BIND the request to its
/// organization: the named organization must be the principal's own. A
/// mismatch — whether the named organization exists or not — answers the
/// SAME typed refusal and names only the CALLER's own organization, so the
/// route can never be a plan/existence oracle for daemon-password callers.
fn require_own_organization(
    principal: &faktor_cloud::Principal,
    named: &OrganizationId,
) -> Result<(), ApiError> {
    if principal.organization != *named {
        return Err(ApiError {
            code: "not_found",
            message: format!(
                "organization {} is not in the caller's scope",
                principal.organization.as_str()
            ),
            http_status: 404,
            retryable: false,
        });
    }
    Ok(())
}

/// The IDENTITY-FEDERATION ENTRY gate (`enterprise_governance`): the
/// organization's plan must map the base entitlement AND carry the
/// capability flag (`sso`; `scim` is the same capability vocabulary for a
/// future SCIM entry). The gate runs BEFORE any login state exists: a
/// denial is the typed 403 `value_entitlement_refused` naming
/// `enterprise_governance` (and the capability), and no billing authority
/// wired answers the shared typed 409 `billing_disabled` — the flag gates
/// the entry, never an implementation.
fn authorize_identity_federation(
    state: &AppState,
    organization: &OrganizationId,
    capability: GovernanceCapability,
) -> Result<(), ApiError> {
    let Some(service) = state.deps.billing.as_ref() else {
        return Err(billing_disabled());
    };
    service
        .check_governance_capability(organization, capability)
        .map_err(value_entitlement_refused)
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
    // Principal law: the control-plane principal authenticates BEFORE the
    // body is parsed, so an invalid/foreign credential never reaches an
    // organization id, a plan or the SSO feature state.
    let principal = match require_principal(&state, &headers) {
        Ok(principal) => principal,
        Err(e) => return wire_status(e),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid sso start body (strict DTO)")),
    };
    let organization = match parse_organization(&body.organization) {
        Ok(organization) => organization,
        Err(e) => return wire_status(e),
    };
    // Principal law: the daemon password alone cannot probe organizations —
    // the presented control-plane principal must own the named organization
    // (the scope refusal names only the caller's own data).
    if let Err(e) = require_own_organization(&principal, &organization) {
        return wire_status(e);
    }
    let authority = match sso_authority(&state) {
        Ok(authority) => authority,
        Err(e) => return wire_status(e),
    };
    if let Err(e) = authorize_identity_federation(&state, &organization, GovernanceCapability::Sso)
    {
        return wire_status(e);
    }
    let sso = match resolve_sso(&state, &organization) {
        Ok(sso) => sso,
        Err(e) => return wire_status(e),
    };
    match authority
        .start(&organization, &sso, &body.redirect_uri)
        .await
    {
        Ok(start) => {
            // The wire projection is the ONLY way the plaintext state leaves
            // the authority (P2-B); the domain type itself is redacted.
            let wire = start.into_wire_response();
            Json(serde_json::json!({
                "ok": true,
                "authorizationUrl": wire.authorization_url(),
                "state": wire.state(),
            }))
            .into_response()
        }
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
    // Principal law: identical to `start` — the principal authenticates
    // before any login state, plan or settings row is read.
    let principal = match require_principal(&state, &headers) {
        Ok(principal) => principal,
        Err(e) => return wire_status(e),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid sso callback body (strict DTO)")),
    };
    let organization = match parse_organization(&body.organization) {
        Ok(organization) => organization,
        Err(e) => return wire_status(e),
    };
    if let Err(e) = require_own_organization(&principal, &organization) {
        return wire_status(e);
    }
    let Some(control_plane) = state.deps.control_plane.as_ref() else {
        return wire_status(ApiError {
            code: "cloud_disabled",
            message: "the control plane is disabled (enable the [cloud] section)".into(),
            http_status: 409,
            retryable: false,
        });
    };
    let authority = match sso_authority(&state) {
        Ok(authority) => authority,
        Err(e) => return wire_status(e),
    };
    if let Err(e) = authorize_identity_federation(&state, &organization, GovernanceCapability::Sso)
    {
        return wire_status(e);
    }
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

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SsoLogoutBody {
    pub(crate) organization: String,
    pub(crate) session_id: String,
}

/// The presented control-plane session token (bounded, never logged).
fn control_token(headers: &HeaderMap) -> Result<&str, ApiError> {
    headers
        .get(CONTROL_TOKEN_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty() && value.len() <= MAX_CONTROL_HEADER_BYTES)
        .ok_or_else(|| ApiError {
            code: "unauthorized",
            message: format!("missing {CONTROL_TOKEN_HEADER} control-plane credential"),
            http_status: 401,
            retryable: false,
        })
}

/// `POST /native/sso/logout` — durably revoke the presented control-plane
/// session. The named `{organization, session_id}` must be owned by the
/// presented token; the durable `revoked_ms` row update kills the token for
/// every later request (401). Idempotent: a repeat reports
/// `alreadyRevoked: true`.
pub(crate) async fn native_sso_logout(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<SsoLogoutBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    // ORDERING: the presented control-plane credential is resolved BEFORE the
    // body or the control-plane feature state. A missing/unknown/foreign
    // credential is 401/404 even when `[cloud]` is disabled. A REVOKED
    // session token is deliberately allowed through to `revoke_session` so a
    // second logout remains the documented idempotent replay.
    let token = match control_token(&headers) {
        Ok(token) => token,
        Err(e) => return wire_status(e),
    };
    let Some(control_plane) = state.deps.control_plane.as_ref() else {
        return wire_status(ApiError {
            code: "unauthorized",
            message: "invalid control-plane credential: no control plane is wired".into(),
            http_status: 401,
            retryable: false,
        });
    };
    let principal = match control_plane.authenticate(token) {
        Ok(principal) => Some(principal),
        Err(ControlPlaneError::Unauthorized(message))
            if message == "control-plane session is expired or revoked" =>
        {
            None
        }
        Err(e) => return wire_status(control_plane_err(e)),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid sso logout body (strict DTO)")),
    };
    let organization = match parse_organization(&body.organization) {
        Ok(organization) => organization,
        Err(e) => return wire_status(e),
    };
    // Tenant isolation before any session lookup: the named organization must
    // be the authenticated principal's own (a foreign org is the same typed
    // 404 a missing one answers).
    if let Some(principal) = &principal {
        if principal.organization != organization {
            return wire_status(ApiError {
                code: "not_found",
                message: "control-plane session not found".into(),
                http_status: 404,
                retryable: false,
            });
        }
    }
    let session_id = match AuthSessionId::try_new(body.session_id.clone()) {
        Ok(session_id) => session_id,
        Err(e) => return wire_status(control_plane_err(e)),
    };
    match control_plane.revoke_session(&organization, &session_id, token) {
        Ok(outcome) => Json(serde_json::json!({
            "ok": true,
            "session": outcome.session.id,
            "organization": outcome.session.organization,
            "user": outcome.session.user,
            "revoked": !outcome.already_revoked,
            "alreadyRevoked": outcome.already_revoked,
            "revokedMs": outcome.session.revoked_ms,
        }))
        .into_response(),
        Err(e) => wire_status(control_plane_err(e)),
    }
}

#[cfg(test)]
#[path = "sso_tests.rs"]
mod sso_tests;
