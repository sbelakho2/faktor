//! The enterprise plane's native routes: retention artifacts + GC, the
//! audit-ledger cursor export, deletion jobs, admin settings and the
//! effective-config attestation.
//!
//! Contract (the same one the control-plane surface keeps):
//!
//! - every route sits behind the daemon password AND a control-plane
//!   principal (`x-faktor-control-token`); the service layer applies the
//!   role matrix, so a viewer/member can never write;
//! - with no `[enterprise]` service wired every route answers a typed 409
//!   `enterprise_disabled` and the daemon is otherwise byte-identical;
//! - mutating routes require an `Idempotency-Key` header; bodies are strict
//!   DTOs (a typo is a 400);
//! - the GC route runs the retention service over the REAL durable
//!   reference scanner (`faktor_session::LiveReferenceScanner`) and the REAL
//!   CAS guarded delete, so a digest referenced by an open landing
//!   transaction, a verification record, a run base or an open edit
//!   transaction is refused at both layers.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use faktor_cas::retention::{
    BlobDeleteOutcome, ProtectedReference, RetentionGuard, RetentionScanError,
};
use faktor_cloud::enterprise::{BlobDeletion, BlobStoreError};
use faktor_cloud::{
    resolve_layers, ArtifactId, ArtifactKind, ConfigLayer, DeletionScope, EnterpriseService,
    GcReport, NewArtifact, OrgSettings, ReferenceKind as CloudReferenceKind, RetentionBlobStore,
    RetentionClass, RetentionReferenceOracle, SsoConfigRef,
};
use faktor_protocol::error::ApiError;

use super::billing::billing_disabled;
use super::control_plane::{control_plane_err, require_idempotency_key, require_principal};
use super::{authed, malformed_body, value_entitlement_refused, wire_status};
use crate::api::AppState;

/// The extended historical read bound of `/native/enterprise/audit`
/// (`extended=true`): the `audit_retention_history` paid entitlement permits
/// one deeper historical response — assembled from the service's bounded
/// pages (each <= `faktor_cloud::MAX_AUDIT_PAGE`) — up to this many events.
pub(crate) const MAX_EXTENDED_AUDIT_PAGE: u64 = 2000;

/// The strict query of `/native/enterprise/audit`: the base cursor export
/// (bounded to the configured retention window) plus the `extended`
/// historical read.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnterpriseAuditQuery {
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    limit: Option<u64>,
    /// Request the EXTENDED historical read (the
    /// `audit_retention_history` paid value entitlement): the single-page
    /// bound rises to [`MAX_EXTENDED_AUDIT_PAGE`] and events older than the
    /// configured base retention window become readable. Without the
    /// entitlement the request is refused typed 403
    /// `value_entitlement_refused`; with no billing authority wired it is
    /// the shared typed 409 `billing_disabled`. Base
    /// (`extended` absent/false) reads are bounded to the window: an
    /// explicit cursor older than the window is gated exactly the same way
    /// (and answers 400 `audit_window_requires_extended` once entitled).
    #[serde(default)]
    extended: Option<bool>,
}

/// The enterprise page bound (mirrors the control-plane cap; kept local so
/// the two surfaces evolve independently).
fn enterprise_page_limit(limit: Option<u64>) -> Result<usize, ApiError> {
    match limit {
        None => Ok(faktor_cloud::MAX_ARTIFACT_PAGE),
        Some(0) => Err(malformed_body("limit must be >= 1")),
        Some(limit) if limit > faktor_cloud::MAX_ARTIFACT_PAGE as u64 => {
            Err(malformed_body(&format!(
                "limit {limit} exceeds the enterprise page bound {}",
                faktor_cloud::MAX_ARTIFACT_PAGE
            )))
        }
        Some(limit) => Ok(limit as usize),
    }
}

/// 409 when no enterprise service is wired.
fn enterprise_disabled() -> ApiError {
    ApiError {
        code: "enterprise_disabled",
        message: "the enterprise plane is disabled (enable the [enterprise] section to use it)"
            .into(),
        http_status: 409,
        retryable: false,
    }
}

/// 409 when the retention runtime (session store + CAS) is not wired.
fn retention_disabled() -> ApiError {
    ApiError {
        code: "retention_runtime_disabled",
        message: "the retention runtime (session store + CAS) is not wired into this daemon".into(),
        http_status: 409,
        retryable: false,
    }
}

fn service(state: &AppState) -> Result<&Arc<EnterpriseService>, Box<Response>> {
    state
        .deps
        .enterprise
        .as_ref()
        .ok_or_else(|| Box::new(wire_status(enterprise_disabled())))
}

fn retention(state: &AppState) -> Result<&Arc<RetentionRuntime>, Box<Response>> {
    state
        .deps
        .retention
        .as_ref()
        .ok_or_else(|| Box::new(wire_status(retention_disabled())))
}

// --------------------------------------------------- retention runtime

/// The durable reference scanner state of ONE GC request: either the
/// precomputed protected-digest set or the typed scan failure. A failed scan
/// refuses every delete (fail closed) and is surfaced as a scan failure in
/// the report + audit ledger.
pub enum ScanGuard {
    Ready(faktor_session::LiveReferenceSet),
    Failed(String),
}

impl RetentionGuard for ScanGuard {
    fn protect(&self, digest_hex: &str) -> Result<Option<ProtectedReference>, RetentionScanError> {
        match self {
            ScanGuard::Ready(set) => set.protect(digest_hex),
            ScanGuard::Failed(message) => Err(RetentionScanError::Unavailable(message.clone())),
        }
    }
}

fn map_reference(reference: ProtectedReference) -> faktor_cloud::ArtifactReference {
    let kind = match reference.kind {
        faktor_cas::retention::ProtectionKind::IntegrationTxn => CloudReferenceKind::IntegrationTxn,
        faktor_cas::retention::ProtectionKind::VerificationRecord => {
            CloudReferenceKind::VerificationRecord
        }
        faktor_cas::retention::ProtectionKind::RunBase => CloudReferenceKind::RunBase,
        faktor_cas::retention::ProtectionKind::OpenEditTxn => CloudReferenceKind::OpenEditTxn,
    };
    faktor_cloud::ArtifactReference {
        kind,
        reference: reference.reference,
        detail: reference.detail,
    }
}

/// The daemon-side retention runtime: the session store the reference
/// scanner walks and the CAS the guarded delete runs against. Hosts wire the
/// SAME instances the rest of the daemon uses.
pub struct RetentionRuntime {
    store: Arc<faktor_store::Store>,
    cas: Arc<faktor_cas::Cas>,
}

impl RetentionRuntime {
    pub fn new(store: Arc<faktor_store::Store>, cas: Arc<faktor_cas::Cas>) -> Self {
        Self { store, cas }
    }

    /// Build the per-request scan guard (bounded, loud).
    pub fn scan_guard(&self) -> ScanGuard {
        match faktor_session::LiveReferenceScanner::new(self.store.as_ref()).scan() {
            Ok(set) => ScanGuard::Ready(set),
            Err(error) => ScanGuard::Failed(error.to_string()),
        }
    }

    /// Run one GC pass with the real scanner + guarded CAS delete.
    pub fn gc(
        &self,
        service: &EnterpriseService,
        principal: &faktor_cloud::Principal,
        limit: usize,
    ) -> Result<GcReport, faktor_cloud::ControlPlaneError> {
        let guard = Arc::new(self.scan_guard());
        let oracle = SessionOracle(guard.clone());
        let blobs = CasBlobs {
            cas: self.cas.clone(),
            guard,
        };
        service.gc_pass(principal, limit, &oracle, &blobs)
    }

    /// Advance one deletion job by one durable step with the real scanner +
    /// guarded CAS delete.
    pub fn advance(
        &self,
        service: &EnterpriseService,
        principal: &faktor_cloud::Principal,
        id: &faktor_cloud::DeletionJobId,
    ) -> Result<faktor_cloud::DeletionJob, faktor_cloud::ControlPlaneError> {
        let guard = Arc::new(self.scan_guard());
        let oracle = SessionOracle(guard.clone());
        let blobs = CasBlobs {
            cas: self.cas.clone(),
            guard,
        };
        service.advance_deletion(principal, id, &oracle, &blobs)
    }
}

struct SessionOracle(Arc<ScanGuard>);

impl RetentionReferenceOracle for SessionOracle {
    fn references(
        &self,
        _organization: &faktor_cloud::OrganizationId,
        digest: &str,
    ) -> Result<Vec<faktor_cloud::ArtifactReference>, faktor_cloud::ReferenceScanError> {
        match self.0.protect(digest) {
            Ok(Some(reference)) => Ok(vec![map_reference(reference)]),
            Ok(None) => Ok(Vec::new()),
            Err(error) => Err(match error {
                RetentionScanError::Unavailable(message) => {
                    faktor_cloud::ReferenceScanError::Unavailable(message)
                }
                RetentionScanError::BoundExceeded(message) => {
                    faktor_cloud::ReferenceScanError::BoundExceeded(message)
                }
                RetentionScanError::Malformed(message) => {
                    faktor_cloud::ReferenceScanError::Malformed(message)
                }
            }),
        }
    }
}

struct CasBlobs {
    cas: Arc<faktor_cas::Cas>,
    guard: Arc<ScanGuard>,
}

impl RetentionBlobStore for CasBlobs {
    fn delete_blob(&self, digest: &str) -> Result<BlobDeletion, BlobStoreError> {
        let Some(hash) = faktor_session::retention::parse_blob_hash(digest) else {
            return Err(BlobStoreError::Unavailable(format!(
                "{digest:?} is not a canonical blob digest"
            )));
        };
        match self.cas.delete_blob_guarded(hash, self.guard.as_ref()) {
            Ok(BlobDeleteOutcome::Deleted { .. }) => Ok(BlobDeletion::Deleted),
            Ok(BlobDeleteOutcome::Absent) => Ok(BlobDeletion::Absent),
            Ok(BlobDeleteOutcome::Refused { reference }) => Ok(BlobDeletion::Refused {
                reference: map_reference(reference),
            }),
            Err(error) => Err(BlobStoreError::Unavailable(error.to_string())),
        }
    }
}

// ------------------------------------------------------------- DTOs

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CursorQuery {
    cursor: Option<String>,
    limit: Option<u64>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RegisterArtifactBody {
    id: String,
    session: Option<String>,
    task: Option<u64>,
    owner: Option<String>,
    kind: String,
    digest: String,
    size: u64,
    retention_class: String,
    ttl_ms: Option<i64>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UpdateSettingsBody {
    #[serde(default)]
    allowed_providers: Vec<String>,
    #[serde(default)]
    allowed_models: Vec<String>,
    #[serde(default)]
    retention_overrides: std::collections::BTreeMap<String, i64>,
    #[serde(default)]
    sso: Option<SsoConfigRef>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GcBody {
    limit: Option<u64>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeletionScopeBody {
    scope: String,
    user: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EffectiveConfigBody {
    layers: Vec<ConfigLayer>,
}

fn parse_artifact_body(body: RegisterArtifactBody) -> Result<NewArtifact, ApiError> {
    let kind = ArtifactKind::parse(&body.kind)
        .ok_or_else(|| malformed_body(&format!("unknown artifact kind {:?}", body.kind)))?;
    let retention_class = RetentionClass::parse(&body.retention_class).ok_or_else(|| {
        malformed_body(&format!(
            "unknown retention class {:?}",
            body.retention_class
        ))
    })?;
    Ok(NewArtifact {
        id: ArtifactId::try_new(body.id).map_err(control_plane_err)?,
        session: body.session,
        task: body.task,
        owner: body.owner,
        kind,
        digest: body.digest,
        size: body.size,
        retention_class,
        ttl_ms: body.ttl_ms,
    })
}

// -------------------------------------------------------------- handlers

/// `GET /native/enterprise/status` — retention class table + ledger heads.
pub(crate) async fn native_enterprise_status(
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
    let service = match service(&state) {
        Ok(service) => service,
        Err(response) => return *response,
    };
    match service.status(&principal, &principal.organization) {
        Ok(status) => Json(serde_json::json!({ "ok": true, "status": status })).into_response(),
        Err(e) => wire_status(control_plane_err(e)),
    }
}

/// The `audit_retention_history` paid boundary of the enterprise audit
/// read: the entitlement is consulted BEFORE any ledger read (a denial is
/// the typed 403 `value_entitlement_refused`; no billing authority is the
/// shared typed 409 `billing_disabled`).
fn retention_history_gate(
    state: &AppState,
    principal: &faktor_cloud::Principal,
) -> Result<(), Box<Response>> {
    let Some(billing) = state.deps.billing.as_ref() else {
        return Err(Box::new(wire_status(billing_disabled())));
    };
    billing
        .check_value_entitlement(
            &principal.organization,
            faktor_cloud::ValueEntitlement::AuditRetentionHistory,
        )
        .map_err(|refusal| Box::new(wire_status(value_entitlement_refused(refusal))))
}

/// `GET /native/enterprise/audit` — one cursor page of the enterprise audit
/// ledger (separate from every engineering proof row).
///
/// Retention gate: the BASE read is bounded to the configured retention
/// window ([`EnterpriseService::audit_window`]) — items older than the
/// window are never served without the `audit_retention_history` paid
/// entitlement. The `extended=true` historical read (up to
/// [`MAX_EXTENDED_AUDIT_PAGE`]) and any explicit base cursor older than the
/// window are gated: denied plans are the typed 403
/// `value_entitlement_refused`, no billing authority is the shared typed 409
/// `billing_disabled`. An entitled plan pages older history with
/// `extended=true`.
pub(crate) async fn native_enterprise_audit(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<EnterpriseAuditQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(p) => p,
        Err(e) => return wire_status(e),
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return wire_status(malformed_body("invalid query parameters (strict DTO)")),
    };
    let extended = query.extended.unwrap_or(false);
    if extended {
        // The paid EXTENDED historical read boundary: entitlement BEFORE any
        // ledger read.
        if let Err(response) = retention_history_gate(&state, &principal) {
            return *response;
        }
    }
    let limit = if extended {
        match query.limit {
            None => MAX_EXTENDED_AUDIT_PAGE as usize,
            Some(0) => return wire_status(malformed_body("limit must be >= 1")),
            Some(limit) if limit > MAX_EXTENDED_AUDIT_PAGE => {
                return wire_status(malformed_body(&format!(
                    "limit {limit} exceeds the extended audit page bound {MAX_EXTENDED_AUDIT_PAGE}"
                )))
            }
            Some(limit) => limit as usize,
        }
    } else {
        match enterprise_page_limit(query.limit) {
            Ok(limit) => limit,
            Err(e) => return wire_status(e),
        }
    };
    let service = match service(&state) {
        Ok(service) => service,
        Err(response) => return *response,
    };
    let cursor = match query.cursor.as_deref() {
        None => None,
        Some(raw) => match raw.parse::<i64>() {
            Ok(value) if value >= 0 => Some(value),
            _ => return wire_status(malformed_body("cursor must be a non-negative integer")),
        },
    };
    if extended {
        // The EXTENDED historical read: one deeper response assembled from
        // the service's bounded pages (each <= its own MAX_AUDIT_PAGE cap),
        // up to the extended bound — never an unbounded ledger scan.
        let mut events: Vec<faktor_cloud::AuditEvent> = Vec::new();
        let mut after = cursor;
        let mut head_seq = 0i64;
        while events.len() < limit {
            let page = (limit - events.len()).min(faktor_cloud::MAX_AUDIT_PAGE);
            match service.audit_export(&principal, &principal.organization, after, page) {
                Ok(export) => {
                    head_seq = export.head_seq;
                    let next = export.next_cursor;
                    events.extend(export.events);
                    match next {
                        Some(raw) => after = raw.parse::<i64>().ok(),
                        None => break,
                    }
                    if after.is_none() {
                        break;
                    }
                }
                Err(e) => return wire_status(control_plane_err(e)),
            }
        }
        let next_cursor = events
            .last()
            .filter(|event| event.seq < head_seq)
            .map(|event| event.seq.to_string());
        return Json(serde_json::json!({
            "ok": true,
            "items": events,
            "nextCursor": next_cursor,
            "headSeq": head_seq,
        }))
        .into_response();
    }
    // The BASE read is bounded to the configured retention window: the ONE
    // window computation decides the clamp AND the gate, so a caller can
    // never address an event older than the window without the entitlement.
    let window = match service.audit_window(&principal, &principal.organization) {
        Ok(window) => window,
        Err(e) => return wire_status(control_plane_err(e)),
    };
    let beyond_window = cursor
        .map(|cursor| cursor < window.last_out_of_window_seq)
        .unwrap_or(false);
    if beyond_window {
        // An explicit cursor older than the window asks for pre-window
        // history on the base read: the paid boundary. A denial is typed
        // here; an entitled plan pages older history with `extended=true`.
        if let Err(response) = retention_history_gate(&state, &principal) {
            return *response;
        }
        return wire_status(ApiError {
            code: "audit_window_requires_extended",
            message: format!(
                "events older than the base retention window ({} ms) require extended=true",
                service.audit_base_window_ms()
            ),
            http_status: 400,
            retryable: false,
        });
    }
    // A fresh base walk (no cursor) starts at the oldest IN-WINDOW event: the
    // clamp makes the base read usable without the entitlement while never
    // serving older history.
    let effective_cursor = match cursor {
        Some(cursor) if cursor >= window.last_out_of_window_seq => Some(cursor),
        _ => Some(window.last_out_of_window_seq),
    };
    match service.audit_export(&principal, &principal.organization, effective_cursor, limit) {
        Ok(export) => Json(serde_json::json!({
            "ok": true,
            "items": export.events,
            "nextCursor": export.next_cursor,
            "headSeq": export.head_seq,
            "baseWindowStartMs": window.window_start_ms,
            "olderEventsGated": window.last_out_of_window_seq > 0,
        }))
        .into_response(),
        Err(e) => wire_status(control_plane_err(e)),
    }
}

/// `GET /native/enterprise/settings` — the organization's admin settings.
pub(crate) async fn native_enterprise_settings_get(
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
    let service = match service(&state) {
        Ok(service) => service,
        Err(response) => return *response,
    };
    match service.settings(&principal, &principal.organization) {
        Ok(settings) => {
            Json(serde_json::json!({ "ok": true, "settings": settings })).into_response()
        }
        Err(e) => wire_status(control_plane_err(e)),
    }
}

/// `PUT /native/enterprise/settings` — replace the settings (admin+; every
/// retention override is bounded by its class ceiling).
pub(crate) async fn native_enterprise_settings_put(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<UpdateSettingsBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(p) => p,
        Err(e) => return wire_status(e),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid settings body (strict DTO)")),
    };
    if let Err(e) = require_idempotency_key(&headers) {
        return wire_status(e);
    }
    let service = match service(&state) {
        Ok(service) => service,
        Err(response) => return *response,
    };
    let mut retention_overrides = std::collections::BTreeMap::new();
    for (class, ttl) in body.retention_overrides {
        let Some(class) = RetentionClass::parse(&class) else {
            return wire_status(malformed_body(&format!(
                "unknown retention class {class:?}"
            )));
        };
        retention_overrides.insert(class, ttl);
    }
    let settings = OrgSettings {
        organization: principal.organization.clone(),
        revision: 0,
        allowed_providers: body.allowed_providers,
        allowed_models: body.allowed_models,
        retention_overrides,
        sso: body.sso,
        updated_at_ms: 0,
    };
    match service.set_settings(&principal, settings) {
        Ok(settings) => {
            Json(serde_json::json!({ "ok": true, "settings": settings })).into_response()
        }
        Err(e) => wire_status(control_plane_err(e)),
    }
}

/// `GET /native/enterprise/artifacts` — one cursor page of artifacts.
pub(crate) async fn native_enterprise_artifacts_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<CursorQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(p) => p,
        Err(e) => return wire_status(e),
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return wire_status(malformed_body("invalid query parameters (strict DTO)")),
    };
    let limit = match enterprise_page_limit(query.limit) {
        Ok(limit) => limit,
        Err(e) => return wire_status(e),
    };
    let service = match service(&state) {
        Ok(service) => service,
        Err(response) => return *response,
    };
    match service.artifacts(
        &principal,
        &principal.organization,
        query.cursor.as_deref(),
        limit,
    ) {
        Ok(page) => Json(serde_json::json!({
            "ok": true,
            "items": page.items,
            "nextCursor": page.next_cursor,
        }))
        .into_response(),
        Err(e) => wire_status(control_plane_err(e)),
    }
}

/// `POST /native/enterprise/artifacts` — register one retention artifact.
pub(crate) async fn native_enterprise_artifacts_register(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<RegisterArtifactBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(p) => p,
        Err(e) => return wire_status(e),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid artifact body (strict DTO)")),
    };
    if let Err(e) = require_idempotency_key(&headers) {
        return wire_status(e);
    }
    let service = match service(&state) {
        Ok(service) => service,
        Err(response) => return *response,
    };
    let new = match parse_artifact_body(body) {
        Ok(new) => new,
        Err(e) => return wire_status(e),
    };
    match service.register_artifact(&principal, new) {
        Ok(record) => (
            StatusCode::CREATED,
            Json(serde_json::json!({ "ok": true, "artifact": record })),
        )
            .into_response(),
        Err(e) => wire_status(control_plane_err(e)),
    }
}

/// `POST /native/enterprise/artifacts/{id}/eligible` — promote an expired
/// artifact to the eligible state.
pub(crate) async fn native_enterprise_artifact_eligible(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(p) => p,
        Err(e) => return wire_status(e),
    };
    if let Err(e) = require_idempotency_key(&headers) {
        return wire_status(e);
    }
    let service = match service(&state) {
        Ok(service) => service,
        Err(response) => return *response,
    };
    let id = match ArtifactId::try_new(id) {
        Ok(id) => id,
        Err(e) => return wire_status(control_plane_err(e)),
    };
    match service.mark_eligible(&principal, &id) {
        Ok(promoted) => {
            Json(serde_json::json!({ "ok": true, "eligible": promoted })).into_response()
        }
        Err(e) => wire_status(control_plane_err(e)),
    }
}

/// `POST /native/enterprise/retention/gc` — one guarded GC pass with the
/// real durable reference scanner and the guarded CAS delete.
pub(crate) async fn native_enterprise_gc(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<GcBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(p) => p,
        Err(e) => return wire_status(e),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid gc body (strict DTO)")),
    };
    if let Err(e) = require_idempotency_key(&headers) {
        return wire_status(e);
    }
    let service = match service(&state) {
        Ok(service) => service,
        Err(response) => return *response,
    };
    let runtime = match retention(&state) {
        Ok(runtime) => runtime,
        Err(response) => return *response,
    };
    let limit = match body.limit {
        None => faktor_cloud::MAX_GC_PASS,
        Some(0) => return wire_status(malformed_body("limit must be >= 1")),
        Some(limit) => (limit as usize).min(faktor_cloud::MAX_GC_PASS),
    };
    match runtime.gc(service, &principal, limit) {
        Ok(report) => Json(serde_json::json!({
            "ok": report.is_clean(),
            "report": report,
        }))
        .into_response(),
        Err(e) => wire_status(control_plane_err(e)),
    }
}

/// `GET /native/enterprise/deletion-jobs`.
pub(crate) async fn native_enterprise_deletion_jobs_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<CursorQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(p) => p,
        Err(e) => return wire_status(e),
    };
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => return wire_status(malformed_body("invalid query parameters (strict DTO)")),
    };
    let limit = match enterprise_page_limit(query.limit) {
        Ok(limit) => limit,
        Err(e) => return wire_status(e),
    };
    let service = match service(&state) {
        Ok(service) => service,
        Err(response) => return *response,
    };
    match service.deletion_jobs(&principal, query.cursor.as_deref(), limit) {
        Ok(page) => Json(serde_json::json!({
            "ok": true,
            "items": page.items,
            "nextCursor": page.next_cursor,
        }))
        .into_response(),
        Err(e) => wire_status(control_plane_err(e)),
    }
}

/// `POST /native/enterprise/deletion-jobs` — start (or return) the
/// deterministic deletion job for a scope (owner only).
pub(crate) async fn native_enterprise_deletion_jobs_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<DeletionScopeBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(p) => p,
        Err(e) => return wire_status(e),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid deletion scope body (strict DTO)")),
    };
    if let Err(e) = require_idempotency_key(&headers) {
        return wire_status(e);
    }
    let service = match service(&state) {
        Ok(service) => service,
        Err(response) => return *response,
    };
    let scope = match body.scope.as_str() {
        "organization" => {
            if body.user.is_some() {
                return wire_status(malformed_body("an organization deletion carries no user"));
            }
            DeletionScope::Organization
        }
        "account" => {
            let Some(user) = body.user else {
                return wire_status(malformed_body("an account deletion requires `user`"));
            };
            let user = match faktor_cloud::UserId::try_new(user) {
                Ok(user) => user,
                Err(e) => return wire_status(control_plane_err(e)),
            };
            DeletionScope::Account { user }
        }
        other => {
            return wire_status(malformed_body(&format!(
                "scope must be organization|account, got {other:?}"
            )));
        }
    };
    match service.start_deletion(&principal, scope) {
        Ok(job) => (
            StatusCode::CREATED,
            Json(serde_json::json!({ "ok": true, "job": job })),
        )
            .into_response(),
        Err(e) => wire_status(control_plane_err(e)),
    }
}

/// `GET /native/enterprise/deletion-jobs/{id}`.
pub(crate) async fn native_enterprise_deletion_job_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(p) => p,
        Err(e) => return wire_status(e),
    };
    let service = match service(&state) {
        Ok(service) => service,
        Err(response) => return *response,
    };
    let id = match faktor_cloud::DeletionJobId::try_new(id) {
        Ok(id) => id,
        Err(e) => return wire_status(control_plane_err(e)),
    };
    match service.deletion_job(&principal, &id) {
        Ok(job) => Json(serde_json::json!({ "ok": true, "job": job })).into_response(),
        Err(e) => wire_status(control_plane_err(e)),
    }
}

/// `POST /native/enterprise/deletion-jobs/{id}/advance` — one durable,
/// idempotent step of the deletion workflow.
pub(crate) async fn native_enterprise_deletion_job_advance(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(p) => p,
        Err(e) => return wire_status(e),
    };
    if let Err(e) = require_idempotency_key(&headers) {
        return wire_status(e);
    }
    let service = match service(&state) {
        Ok(service) => service,
        Err(response) => return *response,
    };
    let runtime = match retention(&state) {
        Ok(runtime) => runtime,
        Err(response) => return *response,
    };
    let id = match faktor_cloud::DeletionJobId::try_new(id) {
        Ok(id) => id,
        Err(e) => return wire_status(control_plane_err(e)),
    };
    match runtime.advance(service, &principal, &id) {
        Ok(job) => Json(serde_json::json!({ "ok": true, "job": job })).into_response(),
        Err(e) => wire_status(control_plane_err(e)),
    }
}

/// `GET /native/enterprise/tombstones/{scope_key}` — the redacted tombstone
/// of a completed deletion.
pub(crate) async fn native_enterprise_tombstone(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(scope_key): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(p) => p,
        Err(e) => return wire_status(e),
    };
    let service = match service(&state) {
        Ok(service) => service,
        Err(response) => return *response,
    };
    match service.tombstone(&principal, &scope_key) {
        Ok(Some(tombstone)) => {
            Json(serde_json::json!({ "ok": true, "tombstone": tombstone })).into_response()
        }
        Ok(None) => wire_status(ApiError {
            code: "not_found",
            message: "tombstone not found".into(),
            http_status: 404,
            retryable: false,
        }),
        Err(e) => wire_status(control_plane_err(e)),
    }
}

/// `POST /native/enterprise/effective-config` — resolve ordered layers with
/// the policy/preference semantics and return the attributable digest.
pub(crate) async fn native_enterprise_effective_config(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<EffectiveConfigBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let principal = match require_principal(&state, &headers) {
        Ok(p) => p,
        Err(e) => return wire_status(e),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid effective-config body (strict DTO)")),
    };
    if let Err(denied) = faktor_cloud::authorize(
        &principal,
        &principal.organization,
        faktor_cloud::Resource::Enterprise,
        faktor_cloud::Action::SettingsRead,
    ) {
        return wire_status(control_plane_err(denied.into()));
    }
    match resolve_layers(&body.layers) {
        Ok(effective) => Json(serde_json::json!({
            "ok": true,
            "digest": effective.digest,
            "attestation": effective.attestation(),
            "policies": effective.policies,
            "preferences": effective.preferences,
        }))
        .into_response(),
        Err(error) => {
            let api = match &error {
                faktor_cloud::LayeredConfigError::Malformed(message) => malformed_body(message),
                _ => ApiError {
                    code: "policy_refused",
                    message: error.to_string(),
                    http_status: 409,
                    retryable: false,
                },
            };
            wire_status(api)
        }
    }
}
