//! The native updater surface: status, check, stage, apply, rollback.
//!
//! Contract:
//!
//! - every route stays behind the daemon's password auth (`authed`) AND a
//!   control-plane principal (`x-faktor-control-token`), because applying an
//!   update is a role-gated control-plane operation: `updater.read` needs the
//!   viewer role, `updater.stage` the member role, and `updater.apply` the
//!   ADMIN role. There is no anonymous apply;
//! - when the `[updater]` section is disabled (the default), every route
//!   answers a typed 409 `updater_disabled` and NOTHING else in the daemon
//!   changes; when the updater is enabled but the control plane is not, the
//!   routes answer a typed 409 `cloud_disabled` (role gating needs a
//!   principal);
//! - mutating routes (`stage`/`apply`/`rollback`) require an
//!   `Idempotency-Key` header (bounded, printable ASCII); `apply` also
//!   requires `confirm: true` — an update is never applied by accident;
//! - the manifest travels in the request body and is verified from those
//!   exact bytes (signature, expiry, channel, compatibility) before anything
//!   is downloaded, staged or swapped.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use axum::Json;
use faktor_cloud::{Action, Resource};
use faktor_protocol::error::ApiError;
use serde::Deserialize;

use super::control_plane::{CONTROL_TOKEN_HEADER, IDEMPOTENCY_KEY_HEADER};
use super::{authed, control_plane_err, malformed_body, wire_status};
use crate::api::AppState;

/// `GET /native/updater/status` (viewer).
pub(crate) async fn native_updater_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    let principal = match updater_gate(&state, &headers, Action::UpdaterRead, false) {
        Ok(principal) => principal,
        Err(e) => return wire_status(e),
    };
    let Some(updater) = state.deps.updater.as_ref() else {
        return wire_status(updater_disabled());
    };
    match updater.status(now_ms()) {
        Ok(status) => Json(serde_json::json!({
            "ok": true,
            "principal": principal.role.as_str(),
            "status": status,
        }))
        .into_response(),
        Err(e) => wire_status(update_err(e)),
    }
}

/// `POST /native/updater/check` (viewer) — authenticate a manifest and check
/// the running components; never downloads.
pub(crate) async fn native_updater_check(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<ManifestBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let _principal = match updater_gate(&state, &headers, Action::UpdaterRead, false) {
        Ok(principal) => principal,
        Err(e) => return wire_status(e),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid updater check body (strict DTO)")),
    };
    let Some(updater) = state.deps.updater.as_ref() else {
        return wire_status(updater_disabled());
    };
    let running = match resolve_components(updater, &body.components) {
        Ok(running) => running,
        Err(e) => return wire_status(e),
    };
    let bytes = match manifest_bytes(&body.manifest) {
        Ok(bytes) => bytes,
        Err(e) => return wire_status(e),
    };
    match updater.check(&bytes, &running, now_ms()) {
        Ok(outcome) => Json(serde_json::json!({ "ok": true, "check": outcome })).into_response(),
        Err(e) => wire_status(update_err(e)),
    }
}

/// `POST /native/updater/stage` (member, Idempotency-Key) — download through
/// the checked transport, verify the digest, publish content-addressed.
pub(crate) async fn native_updater_stage(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<ManifestBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let _principal = match updater_gate(&state, &headers, Action::UpdaterStage, true) {
        Ok(principal) => principal,
        Err(e) => return wire_status(e),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid updater stage body (strict DTO)")),
    };
    let key = match idempotency_key(&headers) {
        Ok(key) => key,
        Err(e) => return wire_status(e),
    };
    let Some(updater) = state.deps.updater.as_ref() else {
        return wire_status(updater_disabled());
    };
    let running = match resolve_components(updater, &body.components) {
        Ok(running) => running,
        Err(e) => return wire_status(e),
    };
    let bytes = match manifest_bytes(&body.manifest) {
        Ok(bytes) => bytes,
        Err(e) => return wire_status(e),
    };
    match updater.stage(&bytes, &running, Some(&key), now_ms()).await {
        Ok(outcome) => Json(serde_json::json!({ "ok": true, "stage": outcome })).into_response(),
        Err(e) => wire_status(update_err(e)),
    }
}

/// `POST /native/updater/apply` (ADMIN, Idempotency-Key, `confirm: true`) —
/// the atomic swap plus the post-swap health probe with automatic rollback.
pub(crate) async fn native_updater_apply(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<ApplyBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let _principal = match updater_gate(&state, &headers, Action::UpdaterApply, true) {
        Ok(principal) => principal,
        Err(e) => return wire_status(e),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid updater apply body (strict DTO)")),
    };
    if !body.confirm {
        return wire_status(ApiError {
            code: "confirmation_required",
            message: "apply requires {\"confirm\": true}: an update is never applied implicitly"
                .into(),
            http_status: 400,
            retryable: false,
        });
    }
    if body.components.is_some() {
        return wire_status(malformed_body(
            "apply takes no components: stage already verified compatibility",
        ));
    }
    if let Err(e) = idempotency_key(&headers) {
        return wire_status(e);
    }
    let Some(updater) = state.deps.updater.as_ref() else {
        return wire_status(updater_disabled());
    };
    match updater.apply(now_ms()) {
        Ok(outcome) => Json(serde_json::json!({ "ok": true, "apply": outcome })).into_response(),
        Err(e) => wire_status(update_err(e)),
    }
}

/// `POST /native/updater/rollback` (ADMIN, Idempotency-Key) — explicit
/// rollback to the artifact the last applied update replaced.
pub(crate) async fn native_updater_rollback(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<RollbackBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let _principal = match updater_gate(&state, &headers, Action::UpdaterApply, true) {
        Ok(principal) => principal,
        Err(e) => return wire_status(e),
    };
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid updater rollback body (strict DTO)")),
    };
    if !body.confirm {
        return wire_status(ApiError {
            code: "confirmation_required",
            message: "rollback requires {\"confirm\": true}".into(),
            http_status: 400,
            retryable: false,
        });
    }
    if let Err(e) = idempotency_key(&headers) {
        return wire_status(e);
    }
    let Some(updater) = state.deps.updater.as_ref() else {
        return wire_status(updater_disabled());
    };
    match updater.rollback(now_ms()) {
        Ok(outcome) => Json(serde_json::json!({ "ok": true, "apply": outcome })).into_response(),
        Err(e) => wire_status(update_err(e)),
    }
}

// ------------------------------------------------------------------ DTOs

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManifestBody {
    /// The manifest object exactly as the release script produced it (the
    /// signature covers its canonical form, so re-serialization is safe).
    manifest: serde_json::Value,
    #[serde(default)]
    components: Option<ComponentsBody>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ComponentsBody {
    cli: Option<String>,
    daemon: Option<String>,
    vscode: Option<String>,
    jetbrains: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ApplyBody {
    confirm: bool,
    /// Accepted only as `None` (rejected when present) so a stale client
    /// cannot smuggle compatibility data past `stage`.
    #[serde(default)]
    components: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RollbackBody {
    confirm: bool,
}

// -------------------------------------------------------------- plumbing

fn updater_disabled() -> ApiError {
    ApiError {
        code: "updater_disabled",
        message: "the updater is disabled (enable the [updater] section to use it)".into(),
        http_status: 409,
        retryable: false,
    }
}

fn cloud_disabled() -> ApiError {
    ApiError {
        code: "cloud_disabled",
        message: "the control plane is disabled; updater routes are role-gated and need a \
                  control-plane principal"
            .into(),
        http_status: 409,
        retryable: false,
    }
}

/// Map one updater failure onto the frozen API error surface.
pub(crate) fn update_err(e: faktor_updater::UpdateError) -> ApiError {
    ApiError {
        code: e.code(),
        message: e.to_string(),
        http_status: e.http_status(),
        retryable: e.retryable(),
    }
}

/// The auth + role gate shared by every updater route. The error is an
/// [`ApiError`] (mapped by the caller through [`wire_status`]) so the
/// `Result` stays small enough to satisfy `clippy::result_large_err`.
fn updater_gate(
    state: &AppState,
    headers: &HeaderMap,
    action: Action,
    require_key: bool,
) -> Result<faktor_cloud::Principal, ApiError> {
    authed(headers, state)?;
    if state.deps.updater.is_none() {
        return Err(updater_disabled());
    }
    let Some(control_plane) = state.deps.control_plane.as_ref() else {
        return Err(cloud_disabled());
    };
    let Some(token) = headers
        .get(CONTROL_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
    else {
        return Err(ApiError {
            code: "unauthorized",
            message: format!("missing {CONTROL_TOKEN_HEADER} control-plane credential"),
            http_status: 401,
            retryable: false,
        });
    };
    let principal = control_plane
        .authenticate(token)
        .map_err(control_plane_err)?;
    if let Err(denied) = faktor_cloud::authorize(
        &principal,
        &principal.organization,
        Resource::Updater,
        action,
    ) {
        return Err(control_plane_err(denied.into()));
    }
    if require_key
        && headers
            .get(IDEMPOTENCY_KEY_HEADER)
            .and_then(|v| v.to_str().ok())
            .is_none()
    {
        return Err(malformed_body(&format!(
            "missing {IDEMPOTENCY_KEY_HEADER} (required on mutating updater requests)"
        )));
    }
    Ok(principal)
}

fn idempotency_key(headers: &HeaderMap) -> Result<String, ApiError> {
    let Some(key) = headers
        .get(IDEMPOTENCY_KEY_HEADER)
        .and_then(|v| v.to_str().ok())
    else {
        return Err(malformed_body(&format!(
            "missing {IDEMPOTENCY_KEY_HEADER} (required on mutating updater requests)"
        )));
    };
    faktor_cloud::ControlPlane::validate_idempotency_key(key).map_err(control_plane_err)
}

fn manifest_bytes(manifest: &serde_json::Value) -> Result<Vec<u8>, ApiError> {
    serde_json::to_vec(manifest)
        .map_err(|e| malformed_body(&format!("manifest is not serializable: {e}")))
}

fn resolve_components(
    updater: &faktor_updater::Updater,
    components: &Option<ComponentsBody>,
) -> Result<faktor_updater::RunningComponents, ApiError> {
    let (cli, daemon, vscode, jetbrains) = match components {
        None => (None, None, None, None),
        Some(components) => (
            components.cli.as_deref(),
            components.daemon.as_deref(),
            components.vscode.as_deref(),
            components.jetbrains.as_deref(),
        ),
    };
    updater
        .running_components(cli, daemon, vscode, jetbrains)
        .map_err(update_err)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Test accessor: whether an updater is wired.
#[cfg(test)]
pub(crate) fn updater_enabled(deps: &crate::api::ServerDeps) -> bool {
    deps.updater.is_some()
}
