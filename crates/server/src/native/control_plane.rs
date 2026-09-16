//! The native control-plane surface: identity, organizations, members,
//! repositories and approvals.
//!
//! Contract:
//!
//! - every route stays behind the daemon's password auth (`authed`); the
//!   CONTROL-PLANE principal additionally rides the `x-faktor-control-token`
//!   header (an auth-session or service-account token; the daemon password
//!   is never a control-plane principal);
//! - when no control plane is wired (`[cloud] enabled = false`, the
//!   default), every route answers a typed 409 `cloud_disabled` and NOTHING
//!   else in the daemon changes;
//! - tenant isolation: the principal's organization is fixed by its token;
//!   a path `{id}` naming a foreign organization is a 404 indistinguishable
//!   from a nonexistent one (no existence leak);
//! - every mutating route requires an `Idempotency-Key` header (bounded,
//!   printable ASCII); the same key + same request replays the recorded
//!   response, the same key + a different request is a 409;
//! - every list route is cursor-paginated (`cursor` + `limit`, hard cap
//!   200): rows are ordered by id and the page carries `nextCursor` when
//!   more rows exist.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use faktor_cloud::rbac::{Action, Resource, Role};
use faktor_cloud::{ApprovalStatus, ControlPlaneError, OrganizationId, Page};
use faktor_protocol::error::ApiError;

use super::{authed, malformed_body, wire_status};
use crate::api::AppState;

/// The control-plane credential header. Separate from the daemon password
/// so a control-plane token can never be confused with (or substituted
/// for) the local daemon credential.
pub const CONTROL_TOKEN_HEADER: &str = "x-faktor-control-token";
/// The idempotency-key header required by mutating routes.
pub const IDEMPOTENCY_KEY_HEADER: &str = "idempotency-key";
/// Default page size of a control-plane listing.
pub const DEFAULT_PAGE_LIMIT: u64 = 50;
/// Hard page cap (mirrors the cloud crate's `MAX_PAGE`).
pub const MAX_PAGE_LIMIT: u64 = 200;
/// Bound on one control-plane header value.
pub const MAX_CONTROL_HEADER_BYTES: usize = 4096;

fn cloud_disabled() -> ApiError {
    ApiError {
        code: "cloud_disabled",
        message: "the control plane is disabled (enable the [cloud] section to use it)".into(),
        http_status: 409,
        retryable: false,
    }
}

fn scm_disabled() -> ApiError {
    ApiError {
        code: "scm_disabled",
        message: "no SCM store is wired into this daemon".into(),
        http_status: 409,
        retryable: false,
    }
}

/// Map one control-plane failure onto the frozen API error surface.
pub(crate) fn control_plane_err(e: ControlPlaneError) -> ApiError {
    let code: &'static str = match e.code() {
        "not_found" => "not_found",
        "unauthorized" => "unauthorized",
        "permission_denied" => "permission_denied",
        "conflict" => "conflict",
        "malformed" => "malformed",
        "control_plane_config" => "internal",
        _ => "internal",
    };
    ApiError {
        code,
        message: e.to_string(),
        http_status: e.http_status(),
        retryable: e.retryable(),
    }
}

fn header_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.len() <= MAX_CONTROL_HEADER_BYTES)
}

/// Resolve the control-plane principal from the request. `Ok(None)` = no
/// token presented (only the bootstrap path accepts that).
fn resolve_principal(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<faktor_cloud::Principal>, ApiError> {
    let Some(control_plane) = state.deps.control_plane.as_ref() else {
        return Err(cloud_disabled());
    };
    let Some(token) = header_value(headers, CONTROL_TOKEN_HEADER) else {
        return Ok(None);
    };
    control_plane
        .authenticate(token)
        .map(Some)
        .map_err(control_plane_err)
}

/// Resolve a REQUIRED principal (every route except the bootstrap).
pub(crate) fn require_principal(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<faktor_cloud::Principal, ApiError> {
    match resolve_principal(state, headers)? {
        Some(principal) => Ok(principal),
        None => Err(ApiError {
            code: "unauthorized",
            message: format!("missing {CONTROL_TOKEN_HEADER} control-plane credential"),
            http_status: 401,
            retryable: false,
        }),
    }
}

pub(crate) fn require_idempotency_key(headers: &HeaderMap) -> Result<String, ApiError> {
    let Some(key) = header_value(headers, IDEMPOTENCY_KEY_HEADER) else {
        return Err(malformed_body(&format!(
            "missing {IDEMPOTENCY_KEY_HEADER} (required on mutating control-plane requests)"
        )));
    };
    faktor_cloud::ControlPlane::validate_idempotency_key(key).map_err(control_plane_err)
}

fn parse_org_id(id: &str) -> Result<OrganizationId, ApiError> {
    OrganizationId::try_new(id.to_string()).map_err(control_plane_err)
}

fn page_limit(limit: Option<u64>) -> Result<usize, ApiError> {
    match limit {
        None => Ok(DEFAULT_PAGE_LIMIT as usize),
        Some(0) => Err(malformed_body("limit must be >= 1")),
        Some(l) if l > MAX_PAGE_LIMIT => Err(malformed_body(&format!(
            "limit {l} exceeds the control-plane page bound {MAX_PAGE_LIMIT}"
        ))),
        Some(l) => Ok(l as usize),
    }
}

// ------------------------------------------------------------------ DTOs

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PageQuery {
    cursor: Option<String>,
    limit: Option<u64>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ApprovalsQuery {
    cursor: Option<String>,
    limit: Option<u64>,
    status: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BootstrapOrganizationBody {
    name: String,
    owner_email: String,
    display_name: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct InviteMemberBody {
    email: String,
    role: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CreateApprovalBody {
    action: String,
    resource: String,
    #[serde(default)]
    reason: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DecideApprovalBody {
    approved: bool,
    #[serde(default)]
    note: String,
}

fn parse_role(raw: &str) -> Result<Role, ApiError> {
    Role::parse(raw).ok_or_else(|| malformed_body("role must be one of owner|admin|member|viewer"))
}

fn parse_action(raw: &str) -> Result<Action, ApiError> {
    Action::parse(raw).ok_or_else(|| malformed_body(&format!("unknown action {raw:?}")))
}

// -------------------------------------------------------------- handlers

/// `GET /native/identity` — the caller's control-plane identity.
pub(crate) async fn native_identity(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(p) => p,
        Err(e) => return wire_status(e),
    };
    let Some(control_plane) = state.deps.control_plane.as_ref() else {
        return wire_status(cloud_disabled());
    };
    match control_plane.identity(&principal) {
        Ok(view) => Json(serde_json::json!({
            "ok": true,
            "identity": view,
        }))
        .into_response(),
        Err(e) => wire_status(control_plane_err(e)),
    }
}

/// `GET /native/orgs` — the caller's organization (exactly one: a principal
/// is bound to one tenant).
pub(crate) async fn native_orgs_list(
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
        Resource::Organization,
        Action::OrganizationRead,
    ) {
        return wire_status(control_plane_err(denied.into()));
    }
    let Some(control_plane) = state.deps.control_plane.as_ref() else {
        return wire_status(cloud_disabled());
    };
    // The identity view carries the organization's durable row too; a
    // listing stays a one-element page so a future multi-org principal can
    // widen without changing the shape.
    match control_plane.identity(&principal) {
        Ok(view) => Json(serde_json::json!({
            "ok": true,
            "items": [{
                "id": view.organization,
                "name": view.organization_name,
                "role": view.role,
            }],
            "nextCursor": serde_json::Value::Null,
        }))
        .into_response(),
        Err(e) => wire_status(control_plane_err(e)),
    }
}

/// `POST /native/orgs` — bootstrap a new organization with its first owner.
/// Only the daemon owner (password auth WITHOUT a control-plane token) may
/// bootstrap; the response carries the owner session token exactly once.
pub(crate) async fn native_orgs_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<BootstrapOrganizationBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => {
            return wire_status(malformed_body(
                "invalid organization bootstrap body (strict DTO)",
            ))
        }
    };
    let idempotency_key = match require_idempotency_key(&headers) {
        Ok(k) => k,
        Err(e) => return wire_status(e),
    };
    match resolve_principal(&state, &headers) {
        Ok(None) => {}
        Ok(Some(_)) => {
            return wire_status(ApiError {
                code: "permission_denied",
                message: "organization creation is a daemon bootstrap operation; a control-plane principal cannot mint tenants".into(),
                http_status: 403,
                retryable: false,
            })
        }
        Err(e) => return wire_status(e),
    }
    let Some(control_plane) = state.deps.control_plane.as_ref() else {
        return wire_status(cloud_disabled());
    };
    match control_plane.bootstrap_organization(
        &body.name,
        &body.owner_email,
        &body.display_name,
        &idempotency_key,
    ) {
        Ok(result) => Json(serde_json::json!({
            "ok": true,
            "idempotent": result.token.is_none(),
            "organization": result.organization,
            "user": result.user,
            "sessionId": result.session.id,
            "role": Role::Owner,
            "token": result.token.map(|t| t.expose().to_string()),
        }))
        .into_response(),
        Err(e) => wire_status(control_plane_err(e)),
    }
}

/// `GET /native/orgs/{id}/members` — one cursor page of the organization's
/// members.
pub(crate) async fn native_org_members_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(query): Query<PageQuery>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(p) => p,
        Err(e) => return wire_status(e),
    };
    let organization = match parse_org_id(&id) {
        Ok(o) => o,
        Err(e) => return wire_status(e),
    };
    let limit = match page_limit(query.limit) {
        Ok(l) => l,
        Err(e) => return wire_status(e),
    };
    let Some(control_plane) = state.deps.control_plane.as_ref() else {
        return wire_status(cloud_disabled());
    };
    match control_plane.members(&principal, &organization, query.cursor.as_deref(), limit) {
        Ok(Page { items, next_cursor }) => Json(serde_json::json!({
            "ok": true,
            "items": items
                .into_iter()
                .map(|member| serde_json::json!({
                    "kind": "user",
                    "membershipId": member.membership.id,
                    "userId": member.user.id,
                    "email": member.user.email,
                    "displayName": member.user.display_name,
                    "role": member.membership.role,
                    "createdAtMs": member.membership.created_ms,
                }))
                .collect::<Vec<_>>(),
            "nextCursor": next_cursor,
        }))
        .into_response(),
        Err(e) => wire_status(control_plane_err(e)),
    }
}

/// `POST /native/orgs/{id}/members` — invite one email (idempotency-keyed).
pub(crate) async fn native_org_members_invite(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<InviteMemberBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid invite body (strict DTO)")),
    };
    let principal = match require_principal(&state, &headers) {
        Ok(p) => p,
        Err(e) => return wire_status(e),
    };
    let idempotency_key = match require_idempotency_key(&headers) {
        Ok(k) => k,
        Err(e) => return wire_status(e),
    };
    let organization = match parse_org_id(&id) {
        Ok(o) => o,
        Err(e) => return wire_status(e),
    };
    let role = match parse_role(&body.role) {
        Ok(r) => r,
        Err(e) => return wire_status(e),
    };
    let Some(control_plane) = state.deps.control_plane.as_ref() else {
        return wire_status(cloud_disabled());
    };
    match control_plane.invite(
        &principal,
        &organization,
        &body.email,
        role,
        &idempotency_key,
    ) {
        Ok(issued) => Json(serde_json::json!({
            "ok": true,
            "idempotent": issued.token.is_none(),
            "invitationId": issued.invitation.id,
            "email": issued.invitation.email,
            "role": issued.invitation.role,
            "status": issued.invitation.status,
            "expiresAtMs": issued.invitation.expires_ms,
            "token": issued.token.map(|t| t.expose().to_string()),
        }))
        .into_response(),
        Err(e) => wire_status(control_plane_err(e)),
    }
}

/// `GET /native/repositories` — one cursor page of the caller's
/// organization's synced repositories.
pub(crate) async fn native_repositories(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<PageQuery>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(p) => p,
        Err(e) => return wire_status(e),
    };
    let limit = match page_limit(query.limit) {
        Ok(l) => l,
        Err(e) => return wire_status(e),
    };
    if let Err(denied) = faktor_cloud::authorize(
        &principal,
        &principal.organization,
        Resource::Repository,
        Action::RepositoryRead,
    ) {
        return wire_status(control_plane_err(denied.into()));
    }
    let Some(scm) = state.deps.scm.as_ref() else {
        return wire_status(scm_disabled());
    };
    let after_id: i64 = match query.cursor.as_deref() {
        None => 0,
        Some(cursor) => match cursor.parse::<i64>() {
            Ok(value) if value >= 0 => value,
            _ => return wire_status(malformed_body("cursor must be a non-negative integer")),
        },
    };
    match scm.repositories_for_organization(
        principal.organization.as_str(),
        after_id,
        limit.saturating_add(1),
    ) {
        Ok(mut rows) => {
            let has_more = rows.len() > limit;
            rows.truncate(limit);
            let next_cursor = if has_more {
                rows.last().map(|row| row.id.to_string())
            } else {
                None
            };
            Json(serde_json::json!({
                "ok": true,
                "items": rows
                    .into_iter()
                    .map(|row| serde_json::json!({
                        "id": row.id,
                        "installationId": row.installation_id,
                        "organization": row.organization_id,
                        "owner": row.owner,
                        "name": row.name,
                        "fullName": row.full_name,
                        "defaultBranch": row.default_branch,
                        "private": row.private,
                        "archived": row.archived,
                    }))
                    .collect::<Vec<_>>(),
                "nextCursor": next_cursor,
            }))
            .into_response()
        }
        Err(e) => wire_status(ApiError {
            code: "internal",
            message: e.to_string(),
            http_status: 500,
            retryable: true,
        }),
    }
}

/// `GET /native/approvals` — one cursor page of the organization's
/// approvals, optionally filtered by status.
pub(crate) async fn native_approvals_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ApprovalsQuery>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(p) => p,
        Err(e) => return wire_status(e),
    };
    let limit = match page_limit(query.limit) {
        Ok(l) => l,
        Err(e) => return wire_status(e),
    };
    let status = match query.status.as_deref() {
        None => None,
        Some(raw) => match ApprovalStatus::parse(raw) {
            Some(status) => Some(status),
            None => {
                return wire_status(malformed_body(
                    "status must be one of open|approved|rejected",
                ))
            }
        },
    };
    let Some(control_plane) = state.deps.control_plane.as_ref() else {
        return wire_status(cloud_disabled());
    };
    match control_plane.approvals(
        &principal,
        &principal.organization,
        status,
        query.cursor.as_deref(),
        limit,
    ) {
        Ok(Page { items, next_cursor }) => Json(serde_json::json!({
            "ok": true,
            "items": items,
            "nextCursor": next_cursor,
        }))
        .into_response(),
        Err(e) => wire_status(control_plane_err(e)),
    }
}

/// `POST /native/approvals` — request one approval (idempotency-keyed).
pub(crate) async fn native_approvals_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<CreateApprovalBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid approval request body (strict DTO)")),
    };
    let principal = match require_principal(&state, &headers) {
        Ok(p) => p,
        Err(e) => return wire_status(e),
    };
    let idempotency_key = match require_idempotency_key(&headers) {
        Ok(k) => k,
        Err(e) => return wire_status(e),
    };
    let action = match parse_action(&body.action) {
        Ok(a) => a,
        Err(e) => return wire_status(e),
    };
    let Some(control_plane) = state.deps.control_plane.as_ref() else {
        return wire_status(cloud_disabled());
    };
    match control_plane.request_approval(
        &principal,
        &principal.organization,
        action,
        &body.resource,
        &body.reason,
        &idempotency_key,
    ) {
        Ok(approval) => (
            StatusCode::CREATED,
            Json(serde_json::json!({ "ok": true, "approval": approval })),
        )
            .into_response(),
        Err(e) => wire_status(control_plane_err(e)),
    }
}

/// `POST /native/approvals/{id}/decide` — decide one approval exactly once.
pub(crate) async fn native_approvals_decide(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<DecideApprovalBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => {
            return wire_status(malformed_body(
                "invalid approval decision body (strict DTO)",
            ))
        }
    };
    let principal = match require_principal(&state, &headers) {
        Ok(p) => p,
        Err(e) => return wire_status(e),
    };
    let approval_id = match faktor_cloud::ApprovalId::try_new(id) {
        Ok(id) => id,
        Err(e) => return wire_status(control_plane_err(e)),
    };
    let Some(control_plane) = state.deps.control_plane.as_ref() else {
        return wire_status(cloud_disabled());
    };
    match control_plane.decide_approval(&principal, &approval_id, body.approved, &body.note) {
        Ok(approval) => {
            Json(serde_json::json!({ "ok": true, "approval": approval })).into_response()
        }
        Err(e) => wire_status(control_plane_err(e)),
    }
}

/// The `ServerDeps` accessor used by the tests to assert the cloud-disabled
/// shape (no control plane wired = every route answers 409).
#[cfg(test)]
pub(crate) fn control_plane_enabled(deps: &crate::api::ServerDeps) -> bool {
    deps.control_plane.is_some()
}
