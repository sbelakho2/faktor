//! The remote/VPC worker-plane routes (additive native control-plane
//! surface):
//!
//! Worker-token surface (the token IS the credential; no daemon password is
//! involved, so a remote worker never holds the daemon secret):
//!
//! - `POST /native/workers/register` — consume one org-scoped registration
//!   token and reconcile the versioned capability advertisement;
//! - `POST /native/workers/{id}/heartbeat` — renew one lease of the exact
//!   generation (bounded staleness; a missed window loses the lease);
//! - `POST /native/jobs/{id}/result` — land one result ONLY while the lease
//!   is live and its generation is current (a superseded result is the
//!   typed `superseded_lease`, journaled, never landed).
//!
//! Operator surface (daemon password + control-plane principal; the
//! organization is fixed by the token, so a foreign org is the byte-
//! identical 404 a missing row answers):
//!
//! - `GET /native/workers?cursor=&limit=` — one cursor page (WorkerRead);
//! - `POST /native/workers/tokens` — mint one registration token (admin);
//! - `POST /native/workers/{id}/revoke` — revoke a worker (admin);
//! - `GET /native/jobs/{id}` — durable state + generations + attempts
//!   (WorkerRead).
//!
//! When no worker plane is wired (`[workers] enabled = false`, the default)
//! every route answers a typed 409 `workers_disabled` and the daemon is
//! otherwise unchanged.

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use faktor_cloud::{Action, Resource, SecretToken};
use faktor_protocol::error::ApiError;
use faktor_worker::{ExecutionJobId, JobGeneration, WorkerError, WorkerId, WorkerLeaseId};

use super::control_plane::require_principal;
use super::{authed, malformed_body, wire_status};
use crate::api::AppState;

/// The strict cursor+limit query of the worker listing.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkersQuery {
    pub(crate) cursor: Option<String>,
    pub(crate) limit: Option<u64>,
}

fn workers_disabled() -> ApiError {
    ApiError {
        code: "workers_disabled",
        message: "the worker plane is disabled (enable the [workers] section to use it)".into(),
        http_status: 409,
        retryable: false,
    }
}

fn not_found(message: &str) -> ApiError {
    ApiError {
        code: "not_found",
        message: message.to_string(),
        http_status: 404,
        retryable: false,
    }
}

/// Map one worker-plane failure onto the frozen API error surface. The
/// generation/lease/protocol refusals keep their OWN machine codes (a stale
/// worker result is never collapsed into a generic conflict).
pub(crate) fn worker_err(e: WorkerError) -> ApiError {
    let (code, http_status, retryable) = match &e {
        WorkerError::Backend(_) => ("internal", 500, true),
        WorkerError::Malformed(_) => ("malformed", 400, false),
        WorkerError::UnknownToken => ("unknown_worker_token", 401, false),
        WorkerError::TokenRevoked => ("worker_token_revoked", 401, false),
        WorkerError::TokenAlreadyUsed(_) => ("worker_token_reused", 409, false),
        WorkerError::TokenOrgMismatch { .. } => ("foreign_worker_token", 403, false),
        WorkerError::UnknownWorker(_) => ("not_found", 404, false),
        WorkerError::WorkerRevoked(_) => ("worker_revoked", 403, false),
        WorkerError::AlreadyRevoked(_) => ("worker_already_revoked", 409, false),
        WorkerError::WorkerAlreadyBound { .. } => ("worker_already_bound", 409, false),
        WorkerError::WorkerSaturated { .. } => ("worker_saturated", 409, false),
        WorkerError::ProtocolSkew { .. } => ("protocol_skew", 409, false),
        WorkerError::ForeignTrustDomain { .. } => ("foreign_trust_domain", 403, false),
        WorkerError::SupersededLease { .. } => ("superseded_lease", 409, false),
        WorkerError::LeaseAlreadyTaken { .. } => ("lease_already_taken", 409, false),
        WorkerError::UnknownLease { .. } => ("unknown_lease", 404, false),
        WorkerError::LeaseExpired { .. } => ("lease_expired", 409, false),
        WorkerError::LeaseNotLive { .. } => ("lease_not_live", 409, false),
        WorkerError::NotAssignedToWorker { .. } => ("not_assigned_to_worker", 409, false),
        WorkerError::NotLeasable { .. } => ("not_leasable", 409, false),
        WorkerError::NoEligibleWorker { .. } => ("no_eligible_worker", 409, false),
        WorkerError::CapabilityMismatch { .. } => ("capability_mismatch", 409, false),
        WorkerError::AttemptsExhausted { .. } => ("attempts_exhausted", 409, false),
        WorkerError::AlreadyAccepted { .. } => ("result_already_accepted", 409, false),
        WorkerError::ResultConflict { .. } => ("result_conflict", 409, false),
        WorkerError::DigestMismatch { .. } => ("result_digest_mismatch", 409, false),
        WorkerError::UnknownJob(_) | WorkerError::UnknownGeneration { .. } => {
            ("not_found", 404, false)
        }
        WorkerError::WorkerVanished(_) => ("internal", 500, false),
        WorkerError::JobTerminal { .. } => ("job_terminal", 409, false),
    };
    ApiError {
        code,
        message: e.to_string(),
        http_status,
        retryable,
    }
}

/// The worker-token half of an operator surface: the plane must be wired and
/// the token must parse.
fn plane(state: &AppState) -> Result<&std::sync::Arc<faktor_worker::WorkerPlane>, ApiError> {
    state.deps.workers.as_ref().ok_or_else(workers_disabled)
}

fn parse_worker_id(id: &str) -> Result<WorkerId, ApiError> {
    WorkerId::try_new(id.to_string()).map_err(worker_err)
}

fn parse_job_id(id: &str) -> Result<ExecutionJobId, ApiError> {
    ExecutionJobId::try_new(id.to_string()).map_err(worker_err)
}

fn worker_token(raw: &str) -> Result<SecretToken, ApiError> {
    SecretToken::try_new(raw.to_string())
        .map_err(|_| malformed_body("worker token must be 1..=512 non-whitespace bytes"))
}

/// Authorize one operator action on the worker plane (org-scoped by the
/// principal's own organization; a foreign tenant learns nothing).
fn authorize_worker(principal: &faktor_cloud::Principal, action: Action) -> Result<(), ApiError> {
    match faktor_cloud::authorize(principal, &principal.organization, Resource::Worker, action) {
        Ok(()) => Ok(()),
        Err(faktor_cloud::Denied::NotFound { .. }) => Err(not_found("worker plane not found")),
        Err(faktor_cloud::Denied::Forbidden { message }) => Err(ApiError {
            code: "permission_denied",
            message,
            http_status: 403,
            retryable: false,
        }),
        Err(faktor_cloud::Denied::Malformed { message }) => Err(malformed_body(&message)),
    }
}

/// One bounded cursor page of workers (`limit` above the bound is a 400,
/// never a silent clamp).
fn page_limit(limit: Option<u64>) -> Result<usize, ApiError> {
    match limit {
        None => Ok(50),
        Some(0) => Err(malformed_body("limit must be >= 1")),
        Some(l) if l as usize > faktor_worker::MAX_WORKER_PAGE => Err(malformed_body(&format!(
            "limit {l} exceeds the worker page bound {}",
            faktor_worker::MAX_WORKER_PAGE
        ))),
        Some(l) => Ok(l as usize),
    }
}

// --------------------------------------------------------------------- DTOs

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RegisterBody {
    pub(crate) token: String,
    pub(crate) worker_id: String,
    pub(crate) display_name: String,
    pub(crate) capabilities: faktor_worker::WorkerCapabilities,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HeartbeatBody {
    pub(crate) token: String,
    pub(crate) protocol_version: u32,
    pub(crate) lease_id: String,
    pub(crate) generation: u64,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResultBody {
    pub(crate) token: String,
    pub(crate) protocol_version: u32,
    pub(crate) generation: u64,
    pub(crate) lease_id: String,
    pub(crate) digest: String,
    pub(crate) outcome: faktor_worker::JobResultOutcome,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MintTokenBody {
    #[serde(default)]
    pub(crate) label: Option<String>,
    #[serde(default)]
    pub(crate) trust_domain: Option<String>,
}

// ------------------------------------------------------------------ handlers

/// `POST /native/workers/register` — the registration bootstrap. The token
/// itself is the credential and fixes the organization/trust domain.
pub(crate) async fn native_worker_register(
    State(state): State<AppState>,
    body: Result<Json<RegisterBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => {
            return wire_status(malformed_body(
                "invalid worker registration body (strict DTO)",
            ))
        }
    };
    let plane = match plane(&state) {
        Ok(plane) => plane,
        Err(e) => return wire_status(e),
    };
    let token = match worker_token(&body.token) {
        Ok(token) => token,
        Err(e) => return wire_status(e),
    };
    let worker_id = match parse_worker_id(&body.worker_id) {
        Ok(id) => id,
        Err(e) => return wire_status(e),
    };
    let (organization, _trust_domain) = match plane.token_organization(&token) {
        Ok(resolved) => resolved,
        Err(e) => return wire_status(worker_err(e)),
    };
    match plane.register(
        &organization,
        &worker_id,
        &token,
        body.capabilities,
        &body.display_name,
    ) {
        Ok(outcome) => Json(serde_json::json!({
            "ok": true,
            "worker": outcome.worker,
            "capabilitiesReconciled": outcome.capabilities_reconciled,
        }))
        .into_response(),
        Err(e) => wire_status(worker_err(e)),
    }
}

/// `POST /native/workers/{id}/heartbeat` — renew one lease of the exact
/// generation. A missed heartbeat window makes the lease expire, the
/// attempt terminal and the job requeue under its bounded policy.
pub(crate) async fn native_worker_heartbeat(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Result<Json<HeartbeatBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid heartbeat body (strict DTO)")),
    };
    let plane = match plane(&state) {
        Ok(plane) => plane,
        Err(e) => return wire_status(e),
    };
    let worker_id = match parse_worker_id(&id) {
        Ok(id) => id,
        Err(e) => return wire_status(e),
    };
    let token = match worker_token(&body.token) {
        Ok(token) => token,
        Err(e) => return wire_status(e),
    };
    let lease_id = match WorkerLeaseId::try_new(body.lease_id.clone()) {
        Ok(id) => id,
        Err(e) => return wire_status(worker_err(e)),
    };
    let organization = match plane.worker_organization(&worker_id) {
        Ok(org) => org,
        Err(e) => return wire_status(worker_err(e)),
    };
    match plane.heartbeat(
        &organization,
        &worker_id,
        &token,
        body.protocol_version,
        &lease_id,
        JobGeneration(body.generation),
    ) {
        Ok(outcome) => Json(serde_json::json!({
            "ok": true,
            "leaseId": outcome.lease_id,
            "generation": outcome.generation.as_u64(),
            "heartbeatIntervalMs": outcome.heartbeat_interval_ms,
            "expiresAtMs": outcome.expires_at_ms,
        }))
        .into_response(),
        Err(e) => wire_status(worker_err(e)),
    }
}

/// `GET /native/workers` — one cursor page of the caller organization's
/// worker registrations.
pub(crate) async fn native_workers_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<WorkersQuery>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(p) => p,
        Err(e) => return wire_status(e),
    };
    if let Err(e) = authorize_worker(&principal, Action::WorkerRead) {
        return wire_status(e);
    }
    let limit = match page_limit(query.limit) {
        Ok(limit) => limit,
        Err(e) => return wire_status(e),
    };
    let plane = match plane(&state) {
        Ok(plane) => plane,
        Err(e) => return wire_status(e),
    };
    match plane.list_workers(&principal.organization, query.cursor.as_deref(), limit) {
        Ok(page) => Json(serde_json::json!({
            "ok": true,
            "items": page.items,
            "nextCursor": page.next_cursor,
        }))
        .into_response(),
        Err(e) => wire_status(worker_err(e)),
    }
}

/// `POST /native/workers/tokens` — mint one org-scoped registration token
/// (admin). The plaintext is returned exactly once and never stored.
pub(crate) async fn native_worker_token_mint(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<MintTokenBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid token mint body (strict DTO)")),
    };
    let principal = match require_principal(&state, &headers) {
        Ok(p) => p,
        Err(e) => return wire_status(e),
    };
    if let Err(e) = authorize_worker(&principal, Action::WorkerManage) {
        return wire_status(e);
    }
    let plane = match plane(&state) {
        Ok(plane) => plane,
        Err(e) => return wire_status(e),
    };
    let label = body.label.unwrap_or_else(|| "worker".to_string());
    let trust_domain = body
        .trust_domain
        .unwrap_or_else(|| principal.organization.as_str().to_string());
    match plane.mint_registration_token(&principal.organization, &trust_domain, &label) {
        Ok(issued) => Json(serde_json::json!({
            "ok": true,
            "token": issued.token.expose(),
            "tokenHash": issued.token_hash,
            "organization": issued.organization_id,
            "trustDomain": issued.trust_domain,
            "label": issued.label,
        }))
        .into_response(),
        Err(e) => wire_status(worker_err(e)),
    }
}

/// `POST /native/workers/{id}/revoke` — revoke one worker (admin): its
/// token dies with it and every live lease is ended, its attempt terminal
/// and its job requeued under the bounded policy.
pub(crate) async fn native_worker_revoke(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(p) => p,
        Err(e) => return wire_status(e),
    };
    if let Err(e) = authorize_worker(&principal, Action::WorkerManage) {
        return wire_status(e);
    }
    let plane = match plane(&state) {
        Ok(plane) => plane,
        Err(e) => return wire_status(e),
    };
    let worker_id = match parse_worker_id(&id) {
        Ok(id) => id,
        Err(e) => return wire_status(e),
    };
    match plane.revoke_worker(&principal.organization, &worker_id) {
        Ok(report) => Json(serde_json::json!({
            "ok": true,
            "revoked": worker_id,
            "expiredLeases": report.expired,
            "requeued": report.requeued,
            "exhausted": report.exhausted,
        }))
        .into_response(),
        Err(e) => wire_status(worker_err(e)),
    }
}

/// `GET /native/jobs/{id}` — durable state, generations, attempts, lease
/// and result of one job (org-scoped; a foreign org is the same 404 a
/// missing job answers).
pub(crate) async fn native_job_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(p) => p,
        Err(e) => return wire_status(e),
    };
    if let Err(e) = authorize_worker(&principal, Action::WorkerRead) {
        return wire_status(e);
    }
    let plane = match plane(&state) {
        Ok(plane) => plane,
        Err(e) => return wire_status(e),
    };
    let job_id = match parse_job_id(&id) {
        Ok(job_id) => job_id,
        Err(e) => return wire_status(e),
    };
    match plane.job_status(&principal.organization, &job_id) {
        Ok(status) => Json(serde_json::json!({
            "ok": true,
            "job": status.job,
            "generations": status.generations,
            "attempts": status.attempts,
            "lease": status.lease,
            "result": status.result,
        }))
        .into_response(),
        Err(e) => wire_status(worker_err(e)),
    }
}

/// `POST /native/jobs/{id}/result` — land one worker result. Accepted ONLY
/// while the lease is live and its generation is current; a superseded
/// result is the typed 409 `superseded_lease`, journaled and never landed.
pub(crate) async fn native_job_result(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Result<Json<ResultBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => {
            return wire_status(malformed_body("invalid job result body (strict DTO)"));
        }
    };
    let plane = match plane(&state) {
        Ok(plane) => plane,
        Err(e) => return wire_status(e),
    };
    let job_id = match parse_job_id(&id) {
        Ok(job_id) => job_id,
        Err(e) => return wire_status(e),
    };
    let token = match worker_token(&body.token) {
        Ok(token) => token,
        Err(e) => return wire_status(e),
    };
    let lease_id = match WorkerLeaseId::try_new(body.lease_id.clone()) {
        Ok(id) => id,
        Err(e) => return wire_status(worker_err(e)),
    };
    // The worker is authenticated by the token; the organization is NOT
    // taken from the request. `worker_id` rides the token hash lookup below
    // through the job's own org: the caller names no organization at all.
    let worker_id = match plane.worker_for_token(&token) {
        Ok(worker_id) => worker_id,
        Err(e) => return wire_status(worker_err(e)),
    };
    let organization = match plane.worker_organization(&worker_id) {
        Ok(org) => org,
        Err(e) => return wire_status(worker_err(e)),
    };
    match plane.submit_result(
        &organization,
        &worker_id,
        &token,
        body.protocol_version,
        &job_id,
        JobGeneration(body.generation),
        &lease_id,
        &body.digest,
        body.outcome,
    ) {
        Ok(outcome) => Json(serde_json::json!({
            "ok": true,
            "outcome": outcome,
        }))
        .into_response(),
        Err(e) => wire_status(worker_err(e)),
    }
}

#[cfg(test)]
#[path = "workers_tests.rs"]
mod workers_tests;
