//! Edition 5 daemon foundation native surface.
//!
//! Settings/runtime contract endpoints: typed descriptors, one versioned
//! snapshot, atomic preview/commit, linear rollback, runtime acknowledgement,
//! search/import/export; provider connections + vault + staged health; model
//! parameter descriptors + effective-request compile + call receipts; durable
//! context manifests and `contextUsage`; the persistent permission policy;
//! redacted diagnostics. Every handler fails closed when the host wired no
//! foundation (`503 foundation_disabled`) and never fabricates an applied,
//! measured or verified state.
#![allow(clippy::result_large_err)]

use std::collections::BTreeMap;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use faktor_foundation::context::{Basis, Manifest, Source};
use faktor_foundation::policy::Action;
use faktor_foundation::{
    ConfigBundle, ConnectionDraft, Foundation, FoundationError, Patch, RuleDraft,
};
use faktor_protocol::error::ApiError;

use super::*;
use crate::api::AppState;

// ------------------------------------------------------------- shared glue

pub(crate) fn foundation(state: &AppState) -> Result<&Foundation, Response> {
    state.deps.foundation.as_deref().ok_or_else(|| {
        wire_status(ApiError {
            code: "foundation_disabled",
            message:
                "the Edition 5 foundation is not wired in this host (configuration, providers, context, policy and diagnostics are unavailable)"
                    .into(),
            http_status: 503,
            retryable: false,
        })
    })
}

pub(crate) fn foundation_error(e: FoundationError) -> Response {
    let status = StatusCode::from_u16(e.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut body = serde_json::json!({
        "code": e.code(),
        "message": e.to_string(),
        "retryable": false,
    });
    let fields = e.field_errors();
    if !fields.is_empty() {
        body["fieldErrors"] = serde_json::to_value(fields).unwrap_or(serde_json::Value::Null);
    }
    (status, Json(body)).into_response()
}

fn session_u64(id: SessionId) -> u64 {
    id.raw()
}

// --------------------------------------------------------------- config

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeConfigSearchQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    limit: Option<u64>,
}

/// `GET /native/config/descriptors` — the typed settings catalog with the
/// effective snapshot revision (one read).
pub(crate) async fn native_config_descriptors(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let snapshot = match foundation.config.snapshot() {
        Ok(s) => s,
        Err(e) => return foundation_error(e),
    };
    let descriptors: Vec<serde_json::Value> = foundation
        .config
        .descriptors()
        .values()
        .map(|d| {
            let effective = snapshot.values.get(&d.key);
            serde_json::json!({
                "key": d.key,
                "label": d.label,
                "description": d.description,
                "category": d.category,
                "kind": d.kind,
                "default": d.default,
                "allowedScopes": d.allowed_scopes,
                "writable": d.writable,
                "applyTiming": d.apply_timing,
                "secretReferenceOnly": d.secret_reference_only,
                "aliasGroup": d.alias_group,
                "effective": effective,
            })
        })
        .collect();
    Json(serde_json::json!({
        "revision": snapshot.revision,
        "runtimeRevision": snapshot.runtime_revision,
        "descriptors": descriptors,
    }))
    .into_response()
}

/// `GET /native/config/snapshot` — one versioned effective settings snapshot.
pub(crate) async fn native_config_snapshot(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    match foundation(&state).map(|f| f.config.snapshot()) {
        Err(r) => r,
        Ok(Err(e)) => foundation_error(e),
        Ok(Ok(snapshot)) => Json(snapshot).into_response(),
    }
}

/// `POST /native/config/preview` — validate a complete patch without
/// persisting anything. `200 {valid:true, changed}` or `409` with fieldErrors.
pub(crate) async fn native_config_preview(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<Patch>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(patch) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid configuration patch body")),
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation.config.validate(&patch) {
        Ok(()) => {
            // A preview is a DIFF, not just a count: every change names the
            // effective current value, the proposed value (or the default
            // when the change removes an override), the scope it writes and
            // the descriptor's apply timing. Read-only (no write).
            let snapshot = match foundation.config.snapshot() {
                Ok(snapshot) => snapshot,
                Err(e) => return foundation_error(e),
            };
            let mut changes = Vec::with_capacity(patch.changes.len());
            let mut scopes: Vec<String> = Vec::new();
            for change in &patch.changes {
                let descriptor = foundation.config.descriptors().get(&change.key);
                let current = snapshot.values.get(&change.key);
                let next = change
                    .value
                    .clone()
                    .or_else(|| descriptor.map(|descriptor| descriptor.default.clone()));
                let redact = descriptor.is_some_and(|d| d.secret_reference_only);
                let mask = |value: Option<&serde_json::Value>| {
                    if redact {
                        serde_json::Value::String("[secret-reference]".into())
                    } else {
                        value.cloned().unwrap_or(serde_json::Value::Null)
                    }
                };
                let scope = match serde_json::to_value(change.scope) {
                    Ok(value) => value,
                    Err(_) => serde_json::Value::Null,
                };
                if let Some(name) = scope.as_str() {
                    if !scopes.iter().any(|s| s == name) {
                        scopes.push(name.to_string());
                    }
                }
                changes.push(serde_json::json!({
                    "key": change.key,
                    "scope": scope,
                    "current": mask(current.map(|value| &value.value)),
                    "next": mask(next.as_ref()),
                    "currentSource": current.map(|value| value.source.clone()),
                    "changed": current.map(|value| Some(&value.value)) != Some(change.value.as_ref()),
                    "applyTiming": descriptor.map(|d| serde_json::json!(d.apply_timing)),
                    "writable": descriptor.map(|d| d.writable),
                    "secretReferenceOnly": redact,
                }));
            }
            Json(serde_json::json!({
                "valid": true,
                "changed": patch.changes.len(),
                "expectedRevision": patch.expected_revision,
                "affectedScopes": scopes,
                "changes": changes,
            }))
            .into_response()
        }
        Err(errors) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "code": "validation",
                "valid": false,
                "message": "the patch is not applicable as written",
                "fieldErrors": errors,
            })),
        )
            .into_response(),
    }
}

/// `POST /native/config/commit` — all-or-nothing commit against the exact
/// expected revision. Returns per-field apply state; `runtimeRevision` only
/// advances for fields the daemon could apply synchronously.
pub(crate) async fn native_config_commit(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<Patch>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(patch) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid configuration patch body")),
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation.config.commit(&patch) {
        Ok(receipt) => Json(receipt).into_response(),
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeConfigRollbackBody {
    #[serde(rename = "expectedRevision")]
    expected_revision: u64,
    #[serde(rename = "targetRevision")]
    target_revision: u64,
}

/// `POST /native/config/rollback` — restore the exact target revision as a
/// NEW revision (linear history; no history is rewritten).
pub(crate) async fn native_config_rollback(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<NativeConfigRollbackBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid rollback body")),
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation
        .config
        .rollback(req.expected_revision, req.target_revision)
    {
        Ok(receipt) => Json(receipt).into_response(),
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeConfigAckBody {
    #[serde(rename = "expectedRevision")]
    expected_revision: u64,
}

/// `POST /native/config/runtime-ack` — the runtime observed the saved values.
pub(crate) async fn native_config_runtime_ack(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<NativeConfigAckBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid runtime-ack body")),
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation.config.acknowledge_runtime(req.expected_revision) {
        Ok(revision) => Json(serde_json::json!({ "runtimeRevision": revision })).into_response(),
        Err(e) => foundation_error(e),
    }
}

/// `GET /native/config/search` — bounded effective-settings search with the
/// winning source and apply timing.
pub(crate) async fn native_config_search(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<NativeConfigSearchQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(q) => q,
        Err(_) => return wire_status(malformed_body("invalid query parameters")),
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let limit = q.limit.unwrap_or(50).clamp(1, 200) as usize;
    match foundation.config.search(&q.q, q.category.as_deref(), limit) {
        Ok(hits) => Json(serde_json::json!({
            "query": q.q,
            "limit": limit,
            "hits": hits,
        }))
        .into_response(),
        Err(e) => foundation_error(e),
    }
}

/// `GET /native/config/export` — redacted schema-validated bundle.
pub(crate) async fn native_config_export(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation.config.export() {
        Ok(bundle) => Json(bundle).into_response(),
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeConfigImportBody {
    bundle: ConfigBundle,
    #[serde(default, rename = "expectedRevision")]
    expected_revision: Option<u64>,
}

/// `POST /native/config/import` — one atomic patch; a malformed or unknown
/// key persists nothing.
pub(crate) async fn native_config_import(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<NativeConfigImportBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid configuration bundle body")),
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation.config.import(&req.bundle, req.expected_revision) {
        Ok(receipt) => Json(receipt).into_response(),
        Err(e) => foundation_error(e),
    }
}

// ------------------------------------------------------------- providers

/// `GET /native/providers/connections` — durable lifecycle records (runtime
/// registry health remains on `GET /native/providers`).
pub(crate) async fn native_provider_connections(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation.providers.list() {
        Ok(connections) => Json(serde_json::json!({ "connections": connections })).into_response(),
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeProviderSaveBody {
    #[serde(flatten)]
    draft: ConnectionDraft,
    #[serde(default, rename = "expectedRevision")]
    expected_revision: Option<u64>,
}

/// `POST /native/providers/connections` — validate + save. Saved is NOT
/// applied: the response carries `applyState: restart_required` until the
/// daemon generation rebuilds its provider graph.
pub(crate) async fn native_provider_connection_save(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<NativeProviderSaveBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid provider connection body")),
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation.providers.save(&req.draft, req.expected_revision) {
        Ok(connection) => Json(connection).into_response(),
        Err(e) => foundation_error(e),
    }
}

/// `DELETE /native/providers/connections/{id}` — local record removal (never
/// a claim of remote revocation).
pub(crate) async fn native_provider_connection_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation.providers.delete(&id) {
        Ok(()) => {
            Json(serde_json::json!({ "deleted": true, "remoteRevoked": false })).into_response()
        }
        Err(e) => foundation_error(e),
    }
}

/// `POST /native/providers/connections/{id}/test` — bounded staged probe.
pub(crate) async fn native_provider_connection_test(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation.providers.probe(&id).await {
        Ok(health) => Json(health).into_response(),
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct NativeCatalogProbeBody {
    connection_id: String,
    #[serde(default)]
    model: Option<String>,
}

/// `POST /native/catalog/probe` — explicit bounded live probe of one
/// connection (reachability, authentication, model list) plus the SERVED
/// capability descriptor of one exact model. The probe never certifies
/// tool-call or coding reliability: `certification` says exactly what was
/// observed. A context-allocation probe is not a capability certification.
pub(crate) async fn native_catalog_probe(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<NativeCatalogProbeBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid catalog probe body")),
    };
    if body.connection_id.is_empty() || body.connection_id.len() > 128 {
        return wire_status(malformed_body("connectionId must be 1..=128 bytes"));
    }
    if let Some(model) = &body.model {
        if model.is_empty() || model.len() > 256 {
            return wire_status(malformed_body("model must be 1..=256 bytes"));
        }
    }
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let connection = match foundation.providers.get(&body.connection_id) {
        Ok(Some(connection)) => connection,
        Ok(None) => return wire_status(not_found("provider connection")),
        Err(e) => return foundation_error(e),
    };
    let health = match foundation.providers.probe(&body.connection_id).await {
        Ok(health) => health,
        Err(e) => return foundation_error(e),
    };
    let model = body.model.clone();
    let served = model.as_ref().and_then(|model| {
        state
            .deps
            .agent
            .deps()
            .providers
            .capabilities(&connection.id, model)
    });
    if let Some(model) = &model {
        let declared = connection.models.iter().any(|m| m == model);
        if served.is_none() && !declared {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "code": "unserved_model",
                    "message": format!("model {model:?} is neither served by the registry nor declared on connection {:?}", connection.id),
                    "health": health,
                })),
            )
                .into_response();
        }
    }
    Json(serde_json::json!({
        "connectionId": connection.id,
        "provider": connection.family,
        "state": health.state,
        "stages": health.stages,
        "observedMs": health.observed_ms,
        "detail": health.detail,
        "models": connection.models,
        "model": model,
        "capability": served.map(|caps| serde_json::json!({
            "contextLimit": caps.context,
            "maxOutput": caps.max_output,
            "tools": caps.tools,
            "parallelTools": caps.parallel_tools,
            "thinking": caps.thinking,
        })),
        "certification": "observed_reachability_and_model_list_only",
    }))
    .into_response()
}

/// `GET /native/providers/connections/{id}/health` — last OBSERVED health.
pub(crate) async fn native_provider_connection_health(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation.providers.health(&id) {
        Ok(Some(health)) => Json(health).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "code": "no_observation",
                "message": "this connection has not been probed since it was saved",
            })),
        )
            .into_response(),
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeSecretBody {
    kind: String,
    label: String,
    secret: String,
}

/// `POST /native/providers/credentials` — store secret material in the
/// machine vault; the response is the opaque reference only.
pub(crate) async fn native_provider_credential_store(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<NativeSecretBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid credential body")),
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let mut bytes = req.secret.into_bytes();
    let result = foundation.vault.store(&req.kind, &req.label, &bytes);
    bytes.fill(0);
    match result {
        Ok(reference) => Json(reference).into_response(),
        Err(e) => foundation_error(e),
    }
}

/// `GET /native/providers/credentials` — opaque references + verification
/// state only.
pub(crate) async fn native_provider_credentials(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation.vault.list() {
        Ok(references) => Json(serde_json::json!({ "credentials": references })).into_response(),
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeSecretRotateBody {
    handle: String,
    secret: String,
}

/// `POST /native/providers/credentials/rotate` — new generation, same handle.
pub(crate) async fn native_provider_credential_rotate(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<NativeSecretRotateBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid credential rotate body")),
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let mut bytes = req.secret.into_bytes();
    let result = foundation.vault.rotate(&req.handle, &bytes);
    bytes.fill(0);
    match result {
        Ok(reference) => Json(reference).into_response(),
        Err(e) => foundation_error(e),
    }
}

/// `DELETE /native/providers/credentials/{handle}` — local deletion only.
pub(crate) async fn native_provider_credential_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(handle): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let handle = format!("vault://{}", handle.trim_start_matches("vault://"));
    match foundation.vault.delete(&handle) {
        Ok(reference) => {
            Json(serde_json::json!({ "reference": reference, "remoteRevoked": false }))
                .into_response()
        }
        Err(e) => foundation_error(e),
    }
}

// ---------------------------------------------------------------- models

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeModelQuery {
    provider: String,
    model: String,
}

fn model_descriptor(
    state: &AppState,
    provider: &str,
    model: &str,
) -> Result<Option<faktor_foundation::ModelTuningDescriptor>, Response> {
    let registry = state.deps.agent.deps().providers.clone();
    let Some(provider_impl) = registry
        .all()
        .into_iter()
        .find(|p| p.identity().instance_id == provider)
    else {
        return Ok(None);
    };
    let caps = provider_impl.capabilities(model);
    let resolution = provider_impl.capability_resolution(model);
    Ok(Some(faktor_foundation::tuning_descriptor(
        provider,
        model,
        &caps,
        resolution.authority,
        resolution.is_gated(),
    )))
}

/// `GET /native/model-parameters?provider=&model=` — capability-gated
/// adapter parameters for one exact model.
pub(crate) async fn native_model_parameters(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<NativeModelQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(q) => q,
        Err(_) => return wire_status(malformed_body("invalid query parameters")),
    };
    match model_descriptor(&state, &q.provider, &q.model) {
        Err(r) => r,
        Ok(None) => wire_status(not_found(&format!(
            "provider instance {} / model {}",
            q.provider, q.model
        ))),
        Ok(Some(descriptor)) => Json(descriptor).into_response(),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeCompileRequest {
    provider: String,
    model: String,
    #[serde(default)]
    parameters: BTreeMap<String, serde_json::Value>,
    #[serde(default, rename = "capabilityRevision")]
    capability_revision: u64,
}

/// `POST /native/model-parameters/compile` — normalize a requested bundle,
/// refusing unsupported fields BEFORE dispatch.
pub(crate) async fn native_model_compile(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<NativeCompileRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid model compile body")),
    };
    let descriptor = match model_descriptor(&state, &req.provider, &req.model) {
        Err(r) => return r,
        Ok(None) => {
            return wire_status(not_found(&format!(
                "provider instance {} / model {}",
                req.provider, req.model
            )))
        }
        Ok(Some(d)) => d,
    };
    match faktor_foundation::compile_effective_request(
        &descriptor,
        &req.parameters,
        req.capability_revision,
    ) {
        Ok(effective) => Json(effective).into_response(),
        Err(errors) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "code": "unsupported_parameter",
                "message": "the requested parameters are not supported by this model",
                "errors": errors,
            })),
        )
            .into_response(),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeReceiptQuery {
    session: String,
    #[serde(default)]
    after: Option<i64>,
    #[serde(default)]
    limit: Option<u64>,
}

/// `GET /native/call-receipts?session=&after=&limit=` — normalized requested
/// vs effective request receipts (immutable once written).
pub(crate) async fn native_call_receipts(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<NativeReceiptQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(q) => q,
        Err(_) => return wire_status(malformed_body("invalid query parameters")),
    };
    let sid = match parse_session_id(&q.session) {
        Ok(s) => s,
        Err(e) => return wire_status(e),
    };
    if let Err(e) = native_resolve_session(&state, &q.session) {
        return *e;
    }
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let limit = q.limit.unwrap_or(50).clamp(1, 200) as usize;
    match foundation.receipts.page(session_u64(sid), q.after, limit) {
        Ok(receipts) => Json(serde_json::json!({ "receipts": receipts })).into_response(),
        Err(e) => foundation_error(e),
    }
}

// --------------------------------------------------------------- context

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeManifestBody {
    #[serde(rename = "operationId")]
    operation_id: String,
    #[serde(default, rename = "callId")]
    call_id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    capacity: Option<u64>,
    #[serde(default)]
    input: Option<u64>,
    #[serde(default, rename = "outputReserve")]
    output_reserve: Option<u64>,
    #[serde(default, rename = "safetyReserve")]
    safety_reserve: Option<u64>,
    #[serde(default)]
    basis: Option<Basis>,
    #[serde(default)]
    sources: Vec<Source>,
}

fn manifest_from_body(
    foundation: &Foundation,
    sid: SessionId,
    body: NativeManifestBody,
) -> Result<Manifest, FoundationError> {
    let config = &foundation.config;
    let model = body.model.clone().unwrap_or_else(|| "unknown".to_string());
    let capacity = match body.capacity {
        Some(c) => c,
        None => config
            .u64_value("faktor.model.contextLimit")?
            .unwrap_or(32_768),
    };
    let output_reserve = match body.output_reserve {
        Some(v) => v,
        None => config
            .u64_value("faktor.context.outputReserve")?
            .unwrap_or(4_096),
    };
    let safety_reserve = match body.safety_reserve {
        Some(v) => v,
        None => config
            .u64_value("faktor.context.safetyReserve")?
            .unwrap_or(2_048),
    };
    // C-08: the durable ignore/protected pattern descriptors are the ONE
    // policy the context pipeline honors. A protected match wins over an
    // ignore match; ignoring is a RETRIEVAL exclusion, never a security
    // deny (an authorized shell can still read the file).
    let ignores = config_string_list(config, "faktor.context.ignorePatterns")?;
    let protected = config_string_list(config, "faktor.context.protectedPatterns")?;
    let sources: Vec<Source> = body
        .sources
        .into_iter()
        .filter(|source| {
            let path = source_workspace_path(&source.uri);
            if path.is_empty() {
                return true;
            }
            let is_protected = protected.iter().any(|pattern| glob_match(pattern, &path));
            if is_protected {
                return true;
            }
            !ignores.iter().any(|pattern| glob_match(pattern, &path))
        })
        .collect();
    let mut input = body.input;
    if input.is_none() && !sources.is_empty() && sources.iter().all(|s| s.tokens.is_some()) {
        input = Some(
            sources
                .iter()
                .filter_map(|s| s.tokens)
                .try_fold(0_u64, |sum, t| sum.checked_add(t))
                .ok_or_else(|| FoundationError::Oversized("context token overflow".into()))?,
        );
    }
    let basis = body.basis.unwrap_or(Basis::Estimated {
        method: "wire-plan-estimator".into(),
        version: "1".into(),
    });
    Ok(Manifest {
        session_id: session_u64(sid),
        revision: 0,
        operation_id: body.operation_id,
        call_id: body.call_id,
        model,
        capacity,
        input,
        output_reserve,
        safety_reserve,
        basis,
        sources,
        created_ms: 0,
    })
}

/// `POST /native/session/{id}/context/preview` — exact accounting preview;
/// nothing is persisted.
pub(crate) async fn native_context_preview(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<NativeManifestBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid context manifest body")),
    };
    let handle = match native_resolve_session(&state, &id) {
        Ok(handle) => handle,
        Err(r) => return *r,
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match manifest_from_body(foundation, handle.id(), body) {
        Err(e) => foundation_error(e),
        Ok(manifest) => match manifest.validate() {
            Ok(()) => Json(manifest.usage()).into_response(),
            Err(FoundationError::Validation(errors)) => (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "code": "overflow",
                    "message": "the context does not fit the model capacity",
                    "fieldErrors": errors,
                })),
            )
                .into_response(),
            Err(e) => foundation_error(e),
        },
    }
}

/// `POST /native/session/{id}/context/manifests` — capture one immutable
/// manifest revision.
pub(crate) async fn native_context_capture(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<NativeManifestBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid context manifest body")),
    };
    let sid = match parse_session_id(&id) {
        Ok(s) => s,
        Err(e) => return wire_status(e),
    };
    if let Err(e) = native_resolve_session(&state, &id) {
        return *e;
    }
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let manifest = match manifest_from_body(foundation, sid, body) {
        Ok(m) => m,
        Err(e) => return foundation_error(e),
    };
    match foundation.context.capture(manifest) {
        Ok(saved) => Json(saved).into_response(),
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeManifestQuery {
    session: String,
    #[serde(default)]
    revision: Option<u64>,
}

/// `GET /native/context/manifest?session=&revision=` — one manifest (latest
/// when revision is absent) plus its `contextUsage` projection.
pub(crate) async fn native_context_manifest(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<NativeManifestQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(q) => q,
        Err(_) => return wire_status(malformed_body("invalid query parameters")),
    };
    let handle = match native_resolve_session(&state, &q.session) {
        Ok(handle) => handle,
        Err(r) => return *r,
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let found = match q.revision {
        Some(revision) => foundation
            .context
            .revision(session_u64(handle.id()), revision),
        None => foundation.context.latest(session_u64(handle.id())),
    };
    match found {
        Ok(Some(manifest)) => Json(serde_json::json!({
            "manifest": manifest,
            "contextUsage": manifest.usage(),
        }))
        .into_response(),
        Ok(None) => wire_status(not_found("context manifest")),
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeManifestPageQuery {
    session: String,
    #[serde(default)]
    before: Option<u64>,
    #[serde(default)]
    limit: Option<u64>,
}

/// `GET /native/context/manifests?session=&before=&limit=` — newest-first
/// manifest page (bounded).
pub(crate) async fn native_context_manifests(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<NativeManifestPageQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(q) => q,
        Err(_) => return wire_status(malformed_body("invalid query parameters")),
    };
    let handle = match native_resolve_session(&state, &q.session) {
        Ok(handle) => handle,
        Err(r) => return *r,
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let limit = q.limit.unwrap_or(50).clamp(1, 200) as usize;
    match foundation
        .context
        .page(session_u64(handle.id()), q.before, limit)
    {
        Ok(manifests) => Json(serde_json::json!({ "manifests": manifests })).into_response(),
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeCompactBody {
    #[serde(rename = "operationId")]
    operation_id: String,
    #[serde(rename = "targetTokens")]
    target_tokens: u64,
    #[serde(default, rename = "minReductionRatio")]
    min_reduction_ratio: Option<f64>,
    #[serde(default, rename = "maxHistoryMessages")]
    max_history_messages: Option<u64>,
}

/// `POST /native/session/{id}/context/compact` — deterministic compaction
/// with the hard minimum-reduction invariant, a retained-facts manifest and
/// the durable compaction record. A rejected plan never claims success.
pub(crate) async fn native_context_compact(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<NativeCompactBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    use faktor_context::compactor::{CompactionRequest, Compactor};
    use faktor_context::ledger::TaskLedger;
    use faktor_context::RecentTurn;

    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid compaction body")),
    };
    if req.operation_id.is_empty() || req.operation_id.len() > 256 {
        return wire_status(malformed_body("operationId must be 1..=256 bytes"));
    }
    if req.target_tokens == 0 {
        return wire_status(malformed_body("targetTokens must be > 0"));
    }
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let min_reduction = req
        .min_reduction_ratio
        .or_else(|| {
            foundation
                .config
                .value("faktor.context.compactionMinReduction")
                .ok()
                .flatten()
                .and_then(|v| v.as_f64())
        })
        .unwrap_or(0.25);
    if !(0.0..1.0).contains(&min_reduction) {
        return wire_status(malformed_body("minReductionRatio must be in [0, 1)"));
    }
    let max_history = req.max_history_messages.unwrap_or(2_000).clamp(1, 10_000);
    let store = state.deps.session.store();
    let mut newest_first = Vec::new();
    let mut cursor = None;
    let mut remaining = max_history;
    while remaining > 0 {
        let page = match store.messages_before(handle.id(), cursor, remaining.min(200)) {
            Ok(page) => page,
            Err(e) => return api_err(&store_err_to_core(e)),
        };
        if page.is_empty() {
            break;
        }
        remaining = remaining.saturating_sub(page.len() as u64);
        cursor = page.last().map(|r| r.seq);
        newest_first.extend(page);
        if remaining == 0 {
            break;
        }
    }
    let mut turns: Vec<RecentTurn> = newest_first
        .iter()
        .rev()
        .map(|row| {
            let text = row
                .data
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            RecentTurn {
                role: row.role.clone(),
                text,
            }
        })
        .filter(|turn| !turn.text.is_empty())
        .collect();
    // Cap per-turn text at a bounded prefix so the plan cannot allocate
    // unbounded strings from hostile rows.
    for turn in &mut turns {
        if turn.text.len() > 256 * 1024 {
            let mut cut = 256 * 1024;
            while !turn.text.is_char_boundary(cut) {
                cut -= 1;
            }
            turn.text.truncate(cut);
        }
    }
    let ledger: TaskLedger = handle
        .get_task_ledger()
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    let before = (faktor_context::estimate_history_tokens(&turns)
        + faktor_context::estimate_history_tokens(&[RecentTurn {
            role: "system".into(),
            text: ledger.compact_render(),
        }])) as u64;
    let target = req.target_tokens.min(before.saturating_sub(1));
    let plan = Compactor::deterministic_only().deterministic_prune(
        &turns,
        &ledger,
        &CompactionRequest {
            before_tokens: before as usize,
            target_tokens: target as usize,
            min_reduction_ratio: min_reduction,
            summarizer_residue_budget_tokens:
                faktor_context::DEFAULT_SUMMARIZER_RESIDUE_BUDGET_TOKENS,
        },
    );
    let compaction = match handle.record_compaction(
        plan.before_tokens as i64,
        plan.after_tokens as i64,
        plan.target_tokens as i64,
        "deterministic_pruning",
        &faktor_session::compaction::CompactionPolicy {
            min_reduction_ratio: min_reduction,
        },
    ) {
        Ok(record) => record,
        Err(e) => return api_err(&e),
    };
    let reduction = if plan.before_tokens == 0 {
        0.0
    } else {
        1.0 - (plan.after_tokens as f64 / plan.before_tokens as f64)
    };
    Json(serde_json::json!({
        "operationId": req.operation_id,
        "accepted": compaction.accepted,
        "before": plan.before_tokens,
        "after": plan.after_tokens,
        "target": plan.target_tokens,
        "reduction": reduction,
        "minimumReduction": min_reduction,
        "strategy": match plan.strategy {
            faktor_context::CompactionStrategy::LlmSummary => "llm_summary",
            faktor_context::CompactionStrategy::DeterministicPruning => "deterministic_pruning",
            faktor_context::CompactionStrategy::Rejected => "rejected",
        },
        "rejectedReason": compaction.reason,
        "retainedFacts": {
            "goal": plan.ledger.goal,
            "openSteps": plan.ledger.open_steps,
            "decisions": plan.ledger.decisions,
            "knownFailures": plan.ledger.known_failures,
            "changedFiles": plan.ledger.changed_files,
        },
        "keptRecentTurns": plan.kept_recent.len(),
        "archivedRefs": plan.archived.len(),
        "basis": {
            "kind": "estimated",
            "method": "history-estimator",
            "version": "1",
        },
    }))
    .into_response()
}

// -------------------------------------------------------------- policy

/// `GET /native/permissions/policy` — rules, managed denies, revision.
pub(crate) async fn native_permissions_policy(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation.policy.policy() {
        Ok(policy) => Json(policy).into_response(),
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeRuleBody {
    #[serde(flatten)]
    draft: RuleDraft,
    #[serde(rename = "expectedRevision")]
    expected_revision: u64,
}

/// `POST /native/permissions/rules` — create/update one rule atomically.
pub(crate) async fn native_permission_rule_save(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<NativeRuleBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid permission rule body")),
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation
        .policy
        .upsert_rule(&req.draft, req.expected_revision)
    {
        Ok(rule) => Json(rule).into_response(),
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeRuleDeleteQuery {
    #[serde(rename = "expectedRevision")]
    expected_revision: u64,
}

/// `DELETE /native/permissions/rules/{id}?expectedRevision=`
pub(crate) async fn native_permission_rule_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    query: Result<Query<NativeRuleDeleteQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(q) => q,
        Err(_) => return wire_status(malformed_body("invalid query parameters")),
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation.policy.delete_rule(&id, q.expected_revision) {
        Ok(revision) => {
            Json(serde_json::json!({ "deleted": true, "revision": revision })).into_response()
        }
        Err(e) => foundation_error(e),
    }
}

/// `POST /native/permissions/preview` — evaluate the exact canonical action
/// dispatch will evaluate, recorded with `source: preview`.
pub(crate) async fn native_permission_preview(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<Action>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(action) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid permission preview body")),
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation.policy.evaluate_recorded(&action, "preview") {
        Ok(verdict) => Json(verdict).into_response(),
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeAuditQuery {
    #[serde(default)]
    before: Option<i64>,
    #[serde(default)]
    limit: Option<u64>,
}

/// `GET /native/permissions/audit?before=&limit=` — bounded decision log.
pub(crate) async fn native_permission_audit(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<NativeAuditQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(q) => q,
        Err(_) => return wire_status(malformed_body("invalid query parameters")),
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let limit = q.limit.unwrap_or(50).clamp(1, 200) as usize;
    match foundation.policy.audit_page(q.before, limit) {
        Ok(entries) => Json(serde_json::json!({ "entries": entries })).into_response(),
        Err(e) => foundation_error(e),
    }
}

// ---------------------------------------------------------- diagnostics

fn binary_checksum() -> Option<String> {
    static CACHE: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| {
            let path = std::env::current_exe().ok()?;
            let metadata = std::fs::metadata(&path).ok()?;
            if metadata.len() > 256 * 1024 * 1024 {
                return None;
            }
            let bytes = std::fs::read(&path).ok()?;
            Some(faktor_foundation::hash_hex(&bytes))
        })
        .clone()
}

fn diagnostics_facts(
    state: &AppState,
    foundation: &Foundation,
) -> faktor_foundation::DiagnosticsFacts {
    use faktor_foundation::diagnostics::{
        ComponentStatus, ProviderSummary, SettingSummary, ToolSummary,
    };
    let snapshot = foundation.config.snapshot().ok();
    let mut settings = Vec::new();
    if let Some(snapshot) = &snapshot {
        for (key, value) in &snapshot.values {
            settings.push(SettingSummary {
                key: key.clone(),
                source: value.source.clone(),
                scope: value.scope.as_str().to_string(),
                secret_reference: foundation
                    .config
                    .descriptors()
                    .get(key)
                    .map(|d| d.secret_reference_only)
                    .unwrap_or(false),
            });
        }
    }
    let mut providers = Vec::new();
    for p in state.deps.agent.deps().providers.all() {
        let identity = p.identity();
        providers.push(ProviderSummary {
            id: identity.instance_id.clone(),
            family: identity.family.clone(),
            endpoint: None,
            credential_kind: "runtime".into(),
            health: "registered".into(),
            model_count: p.known_models().len(),
        });
    }
    if let Ok(connections) = foundation.providers.list() {
        // ONE health read for the whole diagnostics projection (the
        // historical loop opened a reader connection per row).
        let health = foundation.providers.health_all().unwrap_or_default();
        for c in connections {
            providers.push(ProviderSummary {
                id: c.id.clone(),
                family: c.family.as_str().into(),
                endpoint: c.endpoint.clone(),
                credential_kind: if c.credential_ref.is_some() {
                    "vault".into()
                } else {
                    "none".into()
                },
                health: health
                    .get(&c.id)
                    .map(|h| h.state.as_str().to_string())
                    .unwrap_or_else(|| "unprobated".into()),
                model_count: c.models.len(),
            });
        }
    }
    let registry = &state.deps.agent.deps().tools;
    let tools = registry
        .names()
        .into_iter()
        .take(500)
        .map(|name| {
            let schema = registry
                .get(&name)
                .and_then(|tool| serde_json::to_string(&tool.input_schema).ok())
                .unwrap_or_else(|| "{}".into());
            ToolSummary {
                name,
                source: "builtin".into(),
                health: "ready".into(),
                schema,
            }
        })
        .collect();
    let data_dir = state
        .deps
        .session
        .store()
        .root()
        .to_string_lossy()
        .to_string();
    faktor_foundation::DiagnosticsFacts {
        daemon_version: state.deps.version.clone(),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        binary_path: std::env::current_exe()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default(),
        install_root: std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_string_lossy().to_string()))
            .unwrap_or_default(),
        data_dir,
        binary_checksum: binary_checksum(),
        started_ms: state.deps.started_ms,
        components: vec![ComponentStatus {
            name: "foundation".into(),
            state: "ready".into(),
            detail: None,
        }],
        providers,
        tools,
        settings,
        log_tail: Vec::new(),
    }
}

/// `GET /native/diagnostics/preview` — exact allowlist, cap and omission
/// disclosure before export.
pub(crate) async fn native_diagnostics_preview(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let facts = diagnostics_facts(&state, foundation);
    Json(serde_json::json!({
        "categories": [
            "runtime_version", "runtime_paths", "binary_checksum", "component_health",
            "provider_health", "tool_catalog", "effective_setting_keys"
        ],
        "omittedCategories": faktor_foundation::diagnostics::OMITTED_CATEGORIES,
        "maxBytes": foundation.diagnostics_cap(),
        "settingsCount": facts.settings.len(),
        "providersCount": facts.providers.len(),
        "toolsCount": facts.tools.len(),
    }))
    .into_response()
}

/// `POST /native/diagnostics/export` — redacted, bounded bundle with checksum.
pub(crate) async fn native_diagnostics_export(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let facts = diagnostics_facts(&state, foundation);
    match faktor_foundation::export_diagnostics(&facts, foundation.diagnostics_cap()) {
        Ok(bundle) => Json(bundle).into_response(),
        Err(e) => foundation_error(e),
    }
}

/// Read one bounded string-list descriptor, treating a legacy scalar as a
/// single-element list and refusing a malformed stored value.
fn config_string_list(
    config: &faktor_foundation::ConfigService,
    key: &str,
) -> Result<Vec<String>, FoundationError> {
    let Some(value) = config.value(key)? else {
        return Ok(Vec::new());
    };
    match value {
        serde_json::Value::Array(items) => Ok(items
            .into_iter()
            .filter_map(|item| item.as_str().map(str::to_string))
            .filter(|item| !item.is_empty() && item.len() <= 512)
            .take(128)
            .collect()),
        serde_json::Value::String(item) if !item.is_empty() => Ok(vec![item]),
        serde_json::Value::Null => Ok(Vec::new()),
        _ => Err(FoundationError::Corrupt(format!(
            "{key} must be a string list"
        ))),
    }
}

fn source_workspace_path(uri: &str) -> String {
    let without_fragment = uri.split(['#', '?']).next().unwrap_or(uri);
    let without_scheme = without_fragment
        .strip_prefix("file://")
        .or_else(|| without_fragment.strip_prefix("workspace://"))
        .or_else(|| without_fragment.strip_prefix("workspace:"))
        .unwrap_or(without_fragment);
    without_scheme.trim_start_matches('/').to_string()
}

#[derive(Clone, Copy)]
enum GlobToken {
    Literal(char),
    AnyOne,
    Star,
    DoubleStar,
    /// `**/`: zero or more whole segments (zero segments included).
    DoubleStarSlash,
}

fn glob_tokens(pattern: &str) -> Vec<GlobToken> {
    let mut tokens = Vec::new();
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' if chars.peek() == Some(&'*') => {
                while chars.peek() == Some(&'*') {
                    chars.next();
                }
                if chars.peek() == Some(&'/') {
                    chars.next();
                    tokens.push(GlobToken::DoubleStarSlash);
                } else {
                    tokens.push(GlobToken::DoubleStar);
                }
            }
            '*' => tokens.push(GlobToken::Star),
            '?' => tokens.push(GlobToken::AnyOne),
            other => tokens.push(GlobToken::Literal(other)),
        }
    }
    tokens
}

/// Workspace-relative glob matching for the retrieval policy: `**` crosses
/// separators, `*`/`?` do not. Linear in pattern × path (both bounded), with
/// one DP row, so a hostile pattern/path cannot grow memory or blow up
/// exponentially.
fn glob_match(pattern: &str, path: &str) -> bool {
    let tokens = glob_tokens(pattern);
    let text: Vec<char> = path.chars().collect();
    let mut previous = vec![false; text.len() + 1];
    previous[0] = true;
    for token in tokens {
        let mut current = vec![false; text.len() + 1];
        match token {
            GlobToken::Star => {
                current[0] = previous[0];
                for j in 1..=text.len() {
                    current[j] = previous[j] || (current[j - 1] && text[j - 1] != '/');
                }
            }
            GlobToken::DoubleStar => {
                current[0] = previous[0];
                for j in 1..=text.len() {
                    current[j] = previous[j] || current[j - 1];
                }
            }
            GlobToken::DoubleStarSlash => {
                current[0] = previous[0];
                for j in 1..=text.len() {
                    current[j] = previous[j] || current[j - 1];
                }
            }
            GlobToken::AnyOne => {
                for j in 1..=text.len() {
                    current[j] = previous[j - 1] && text[j - 1] != '/';
                }
            }
            GlobToken::Literal(expected) => {
                for j in 1..=text.len() {
                    current[j] = previous[j - 1] && text[j - 1] == expected;
                }
            }
        }
        previous = current;
    }
    previous[text.len()]
}

/// Production receipt sink: persists every settled model call into the
/// foundation `call_receipt` authority (F048/F050/F090). The agent emits the
/// normalized requested/effective record; this adapter only maps it, so no
/// provider semantics leak into the runtime.
struct FoundationCallReceiptSink {
    receipts: std::sync::Arc<faktor_foundation::models::CallReceiptStore>,
}

impl faktor_agent::CallReceiptSink for FoundationCallReceiptSink {
    fn record(&self, record: &faktor_agent::CallReceiptRecord) {
        let receipt = faktor_foundation::models::CallReceipt {
            session_id: record.session_id,
            call_id: record.call_id.clone(),
            op_id: record.op_id.clone(),
            provider: record.provider.clone(),
            model: record.model.clone(),
            route: record.route.clone(),
            capability_revision: 0,
            requested: record.requested.clone(),
            effective: record.effective.clone(),
            inherited: record.inherited.clone(),
            started_ms: record.started_ms,
            ended_ms: record.ended_ms,
            status: record.status.clone(),
            tokens_in: record.tokens_in,
            tokens_out: record.tokens_out,
            error: record.error.clone(),
        };
        if let Err(e) = self.receipts.record(&receipt) {
            tracing::warn!(
                session = record.session_id,
                call = %record.call_id,
                "call receipt write failed: {e}"
            );
        }
    }
}

/// Install the foundation-backed receipt writer on the daemon's ONE runtime.
pub fn install_call_receipt_sink(
    agent: &std::sync::Arc<faktor_agent::AgentRuntime>,
    foundation: &faktor_foundation::Foundation,
) {
    agent.set_receipt_sink(std::sync::Arc::new(FoundationCallReceiptSink {
        receipts: foundation.receipts.clone(),
    }));
}

#[cfg(test)]
mod context_glob_tests {
    use super::glob_match;

    #[test]
    fn double_star_crosses_separators_and_single_star_does_not() {
        assert!(glob_match("**/*.log", "logs/today/app.log"));
        assert!(glob_match("**/*.log", "app.log"));
        assert!(!glob_match("**/*.log", "logs/app.txt"));
        assert!(glob_match("src/*.rs", "src/lib.rs"));
        assert!(!glob_match("src/*.rs", "src/nested/lib.rs"));
        assert!(glob_match("src/**/lib.rs", "src/a/b/lib.rs"));
        assert!(glob_match("?.md", "a.md"));
        assert!(!glob_match("?.md", "ab.md"));
        assert!(!glob_match("a?c", "a/c"));
        // Hostile pattern/path sizes stay linear and bounded (no panic).
        let pattern = format!("{}{}", "*".repeat(512), "x");
        let path = "y".repeat(4_096);
        assert!(!glob_match(&pattern, &path));
    }
}
