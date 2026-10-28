//! Revision 8 native foundation routes (daemon half of S02–S20, S27,
//! S41–S48, S53–S54, S60–S69).
//!
//! Additive to the Edition-5 foundation handlers: durable instruction/rule
//! records and captured bundles, agent preset envelopes and admission
//! bindings, runtime executable trust, browser adapter/session/observation
//! lifecycle, context pins/export, measured efficiency effects, task budget
//! revision, and settings provenance/reset. Every handler is password-gated
//! and fails closed with a typed 503 when the foundation is not wired.
#![allow(clippy::result_large_err)]

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use faktor_core::id::{SessionId, TaskId};
use faktor_foundation::context::PinAction;
use faktor_foundation::instructions::{InstructionDraft, InstructionKind, InstructionOrigin};
use faktor_foundation::presets::PresetDraft;
use faktor_protocol::error::ApiError;

use super::foundation::{foundation, foundation_error};
use super::*;
use crate::api::AppState;

fn malformed(message: impl Into<String>) -> Response {
    wire_status(ApiError {
        code: "malformed",
        message: message.into(),
        http_status: 400,
        retryable: false,
    })
}

fn conflict(code: &'static str, message: impl Into<String>) -> Response {
    wire_status(ApiError {
        code,
        message: message.into(),
        http_status: 409,
        retryable: false,
    })
}

fn session_from(raw: &str) -> Result<SessionId, Response> {
    parse_session_id(raw).map_err(wire_status)
}

fn task_from(raw: &str) -> Result<TaskId, Response> {
    let raw: u64 = raw
        .parse()
        .map_err(|_| malformed(format!("invalid task id {raw:?}")))?;
    TaskId::try_from(raw).map_err(|e| {
        wire_status(ApiError {
            code: "malformed",
            message: e.to_string(),
            http_status: 400,
            retryable: false,
        })
    })
}

fn known_tools(state: &AppState) -> Vec<String> {
    state.deps.agent.deps().tools.names()
}

// ------------------------------------------------------- instructions

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct InstructionListQuery {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    origin: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    limit: Option<u64>,
}

/// `GET /native/instructions/records` — the durable operator-authored
/// instruction/rule/template catalog, filtered and bounded.
pub(crate) async fn native_instruction_records(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<InstructionListQuery>, axum::extract::rejection::QueryRejection>,
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
    let limit = q.limit.unwrap_or(200).clamp(1, 500) as usize;
    let kind = match q.kind.as_deref().and_then(InstructionKind::parse) {
        Some(kind) => Some(kind),
        None if q.kind.is_some() => return malformed("unknown instruction kind"),
        None => None,
    };
    let origin = match q.origin.as_deref().and_then(InstructionOrigin::parse) {
        Some(origin) => Some(origin),
        None if q.origin.is_some() => return malformed("unknown instruction origin"),
        None => None,
    };
    let needle = q.q.as_deref().map(str::to_ascii_lowercase);
    match foundation.instructions.list() {
        Ok(records) => {
            let records: Vec<_> = records
                .into_iter()
                .filter(|r| kind.is_none_or(|k| r.kind == k))
                .filter(|r| origin.is_none_or(|o| r.origin == o))
                .filter(|r| q.enabled.is_none_or(|enabled| r.enabled == enabled))
                .filter(|r| {
                    needle.as_deref().is_none_or(|needle| {
                        r.title.to_ascii_lowercase().contains(needle)
                            || r.content.to_ascii_lowercase().contains(needle)
                    })
                })
                .take(limit)
                .collect();
            Json(serde_json::json!({ "records": records, "count": records.len() })).into_response()
        }
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct InstructionUpsertBody {
    #[serde(flatten)]
    draft: InstructionDraft,
    #[serde(default)]
    expected_revision: Option<u64>,
}

pub(crate) async fn native_instruction_upsert(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<InstructionUpsertBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return malformed("invalid instruction body"),
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation
        .instructions
        .upsert(&req.draft, req.expected_revision)
    {
        Ok(record) => Json(record).into_response(),
        Err(e) => foundation_error(e),
    }
}

pub(crate) async fn native_instruction_get(
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
    match foundation.instructions.get(&id) {
        Ok(Some(record)) => Json(record).into_response(),
        Ok(None) => wire_status(not_found(&format!("instruction {id}"))),
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ExpectedRevisionQuery {
    expected_revision: u64,
}

pub(crate) async fn native_instruction_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    query: Result<Query<ExpectedRevisionQuery>, axum::extract::rejection::QueryRejection>,
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
    match foundation.instructions.delete(&id, q.expected_revision) {
        Ok(revision) => {
            Json(serde_json::json!({ "deleted": true, "revision": revision })).into_response()
        }
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct InstructionPreviewBody {
    #[serde(default)]
    prompt: String,
    #[serde(default)]
    touched_paths: Vec<String>,
    #[serde(default)]
    selected_ids: Vec<String>,
}

pub(crate) async fn native_instruction_preview(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<InstructionPreviewBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return malformed("invalid instruction preview body"),
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation
        .instructions
        .preview(&req.prompt, &req.touched_paths, &req.selected_ids)
    {
        Ok(bundle) => Json(bundle).into_response(),
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct InstructionImportBody {
    format: String,
    bundle: serde_json::Value,
}

pub(crate) async fn native_instruction_import(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<InstructionImportBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return malformed("invalid instruction import body"),
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    Json(foundation.instructions.import(&req.format, &req.bundle)).into_response()
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct InstructionBindingQuery {
    session: String,
    #[serde(default)]
    revision: Option<u64>,
    #[serde(default)]
    operation_id: Option<String>,
    #[serde(default)]
    before: Option<u64>,
    #[serde(default)]
    limit: Option<u64>,
}

pub(crate) async fn native_instruction_bindings(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<InstructionBindingQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(q) => q,
        Err(_) => return wire_status(malformed_body("invalid query parameters")),
    };
    let sid = match session_from(&q.session) {
        Ok(sid) => sid,
        Err(r) => return r,
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    if q.revision.is_some() || q.operation_id.is_some() {
        return match foundation.instructions.binding(
            sid.raw(),
            q.revision,
            q.operation_id.as_deref(),
        ) {
            Ok(Some(binding)) => Json(binding).into_response(),
            Ok(None) => wire_status(not_found("instruction binding")),
            Err(e) => foundation_error(e),
        };
    }
    let limit = q.limit.unwrap_or(50).clamp(1, 200) as usize;
    match foundation.instructions.bindings(sid.raw(), q.before, limit) {
        Ok(bindings) => Json(serde_json::json!({ "bindings": bindings })).into_response(),
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct InstructionBindBody {
    session: String,
    operation_id: String,
    #[serde(default)]
    prompt: String,
    #[serde(default)]
    touched_paths: Vec<String>,
    #[serde(default)]
    selected_ids: Vec<String>,
}

pub(crate) async fn native_instruction_bind(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<InstructionBindBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return malformed("invalid instruction binding body"),
    };
    let sid = match session_from(&req.session) {
        Ok(sid) => sid,
        Err(r) => return r,
    };
    if let Err(e) = native_resolve_session(&state, &req.session) {
        return *e;
    }
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let bundle =
        match foundation
            .instructions
            .preview(&req.prompt, &req.touched_paths, &req.selected_ids)
        {
            Ok(bundle) => bundle,
            Err(e) => return foundation_error(e),
        };
    match foundation
        .instructions
        .bind(sid.raw(), &req.operation_id, &bundle)
    {
        Ok(binding) => Json(binding).into_response(),
        Err(e) => foundation_error(e),
    }
}

// ------------------------------------------------------------ presets

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PresetListQuery {
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    limit: Option<u64>,
}

pub(crate) async fn native_presets(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<PresetListQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(q) => q,
        Err(_) => return wire_status(malformed_body("invalid query parameters")),
    };
    let mode = match q.mode.as_deref() {
        Some(raw) => match faktor_foundation::PresetMode::parse(raw) {
            Some(mode) => Some(mode),
            None => return malformed("unknown preset mode"),
        },
        None => None,
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let limit = q.limit.unwrap_or(200).clamp(1, 500) as usize;
    match foundation.presets.list() {
        Ok(records) => {
            let records: Vec<_> = records
                .into_iter()
                .filter(|r| mode.is_none_or(|mode| r.mode == mode))
                .take(limit)
                .collect();
            Json(serde_json::json!({ "presets": records, "count": records.len() })).into_response()
        }
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PresetUpsertBody {
    #[serde(flatten)]
    draft: PresetDraft,
    #[serde(default)]
    expected_revision: Option<u64>,
}

pub(crate) async fn native_preset_upsert(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<PresetUpsertBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return malformed("invalid preset body"),
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation.presets.upsert(&req.draft, req.expected_revision) {
        Ok(record) => Json(record).into_response(),
        Err(e) => foundation_error(e),
    }
}

pub(crate) async fn native_preset_get(
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
    match foundation.presets.get(&id) {
        Ok(Some(record)) => Json(record).into_response(),
        Ok(None) => wire_status(not_found(&format!("preset {id}"))),
        Err(e) => foundation_error(e),
    }
}

pub(crate) async fn native_preset_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    query: Result<Query<ExpectedRevisionQuery>, axum::extract::rejection::QueryRejection>,
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
    match foundation.presets.delete(&id, q.expected_revision) {
        Ok(revision) => {
            Json(serde_json::json!({ "deleted": true, "revision": revision })).into_response()
        }
        Err(e) => foundation_error(e),
    }
}

pub(crate) async fn native_preset_envelope(
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
    let tools = known_tools(&state);
    match foundation.presets.envelope_with(&id, &tools) {
        Ok(envelope) => Json(envelope).into_response(),
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PresetBindingQuery {
    session: String,
    submission_id: String,
}

pub(crate) async fn native_preset_binding(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<PresetBindingQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(q) => q,
        Err(_) => return wire_status(malformed_body("invalid query parameters")),
    };
    let sid = match session_from(&q.session) {
        Ok(sid) => sid,
        Err(r) => return r,
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation.presets.binding(sid.raw(), &q.submission_id) {
        Ok(Some(binding)) => Json(binding).into_response(),
        Ok(None) => wire_status(not_found("preset binding")),
        Err(e) => foundation_error(e),
    }
}

// ------------------------------------------------------ runtime trust

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RuntimeTrustQuery {
    path: String,
}

pub(crate) async fn native_runtime_trust_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<RuntimeTrustQuery>, axum::extract::rejection::QueryRejection>,
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
    match foundation.runtime_trust.record(&q.path) {
        Ok(Some(record)) => Json(record).into_response(),
        Ok(None) => wire_status(not_found("runtime trust record")),
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RuntimeTrustConfirmBody {
    executable_path: String,
    #[serde(default)]
    publisher: Option<String>,
}

pub(crate) async fn native_runtime_trust_confirm(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<RuntimeTrustConfirmBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return malformed("invalid runtime trust body"),
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation
        .runtime_trust
        .confirm(&req.executable_path, req.publisher.as_deref())
    {
        Ok(record) => Json(record).into_response(),
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RuntimeTrustCheckBody {
    executable_path: String,
}

pub(crate) async fn native_runtime_trust_check(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<RuntimeTrustCheckBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return malformed("invalid runtime trust check body"),
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    Json(foundation.runtime_trust.check_fresh(&req.executable_path)).into_response()
}

/// `GET /native/runtime/status` — readiness/trust facts for S34/S35/S56.
/// The sandbox dimensions are reported SEPARATELY and never collapsed into
/// an "isolated" badge.
pub(crate) async fn native_runtime_status(
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
    let executable = std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let trust = foundation.runtime_trust.check(&executable);
    let sandbox = &state.deps.terminal_policy.sandbox;
    let shell_state = sandbox.shell_execution_state();
    Json(serde_json::json!({
        "version": state.deps.version,
        "protocol": "v1",
        "executablePath": executable,
        "dataDir": state.deps.session.store().root().to_string_lossy(),
        "startedMs": state.deps.started_ms,
        "trust": trust,
        "shellIsolation": {
            "mode": serde_json::to_value(shell_state.mode).unwrap_or(serde_json::Value::Null),
            "networkGuarantee": serde_json::to_value(shell_state.network_guarantee).unwrap_or(serde_json::Value::Null),
            "filesystemGuarantee": serde_json::to_value(sandbox.filesystem_guarantee).unwrap_or(serde_json::Value::Null),
            "filesystemRequirement": format!("{:?}", sandbox.spawn_filesystem_requirement()),
            "externalRead": format!("{:?}", sandbox.read_external),
            "externalWrite": format!("{:?}", sandbox.write_external),
        },
    }))
    .into_response()
}

// ----------------------------------------------------------- browser

pub(crate) async fn native_browser_adapters(
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
    let surface = &state.deps.browser;
    let available = surface.ensure(foundation).is_ok();
    if available {
        if let Err(reason) = surface.advertise(foundation) {
            return conflict("browser_adapter_failed", reason);
        }
    }
    match foundation.browser.adapters() {
        Ok(adapters) => {
            let advertised = adapters.iter().any(|a| a.advertised);
            Json(serde_json::json!({
                "adapters": adapters,
                "advertised": advertised,
                "state": if advertised { "advertised" } else { "unavailable" },
                "reason": if advertised { serde_json::Value::Null } else {
                    serde_json::json!(surface.reason().unwrap_or_else(|| "no browser adapter is advertised by this daemon".to_string()))
                },
            }))
            .into_response()
        }
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct BrowserSessionsQuery {
    session: String,
}

pub(crate) async fn native_browser_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<BrowserSessionsQuery>, axum::extract::rejection::QueryRejection>,
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
    match foundation.browser.sessions(handle.id().raw()) {
        Ok(sessions) => Json(serde_json::json!({ "sessions": sessions })).into_response(),
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct BrowserSessionBody {
    adapter_id: String,
    session: String,
    url: String,
}

pub(crate) async fn native_browser_session_request(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<BrowserSessionBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return malformed("invalid browser session body"),
    };
    let sid = match session_from(&req.session) {
        Ok(sid) => sid,
        Err(r) => return r,
    };
    if let Err(e) = native_resolve_session(&state, &req.session) {
        return *e;
    }
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let surface = &state.deps.browser;
    if req.adapter_id == super::browser_surface::adapter_id() {
        if let Err(reason) = surface.ensure(foundation) {
            return conflict("browser_unavailable", reason);
        }
        return match surface.open(foundation, sid.raw(), &req.url).await {
            Ok(session) => Json(session).into_response(),
            Err(reason) => conflict("browser_session_failed", reason),
        };
    }
    match foundation
        .browser
        .request_session(&req.adapter_id, sid.raw(), &req.url)
    {
        Ok(session) => Json(session).into_response(),
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct BrowserObservationQuery {
    #[serde(default)]
    severity: Option<String>,
    #[serde(default)]
    after: Option<i64>,
    #[serde(default)]
    limit: Option<u64>,
}

pub(crate) async fn native_browser_observations(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    query: Result<Query<BrowserObservationQuery>, axum::extract::rejection::QueryRejection>,
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
    let surface = &state.deps.browser;
    if surface.available() {
        let _ = surface.sync_observations(foundation, &id).await;
    }
    let limit = q.limit.unwrap_or(200).clamp(1, 500) as usize;
    match foundation
        .browser
        .observations(&id, q.severity.as_deref(), q.after, limit)
    {
        Ok(observations) => Json(serde_json::json!({
            "observations": observations,
            "count": observations.len(),
        }))
        .into_response(),
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct BrowserScreenshotBody {
    mime: String,
    data_base64: String,
    #[serde(default)]
    in_model_context: bool,
}

pub(crate) async fn native_browser_screenshot(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<BrowserScreenshotBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return malformed("invalid browser screenshot body"),
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let bytes = if req.data_base64.is_empty() {
        if req.mime != "image/png" && req.mime != "image/jpeg" && req.mime != "image/webp" {
            return malformed("screenshot mime must be image/png, image/jpeg or image/webp");
        }
        let format = req.mime.rsplit('/').next().unwrap_or("png").to_string();
        let surface = &state.deps.browser;
        match surface.screenshot(&id, &format).await {
            Ok(bytes) => bytes,
            Err(reason) => return conflict("browser_screenshot_failed", reason),
        }
    } else {
        match decode_attachment_base64(req.data_base64.as_bytes()) {
            Ok(bytes) => bytes,
            Err(e) => return wire_status(e),
        }
    };
    match foundation
        .browser
        .store_screenshot(&id, &req.mime, &bytes, req.in_model_context)
    {
        Ok(artifact) => Json(artifact).into_response(),
        Err(e) => foundation_error(e),
    }
}

pub(crate) async fn native_browser_artifact(
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
    match foundation.browser.artifact_bytes(&id) {
        Ok(Some((artifact, _))) => Json(artifact).into_response(),
        Ok(None) => wire_status(not_found("browser artifact")),
        Err(e) => foundation_error(e),
    }
}

pub(crate) async fn native_browser_artifact_bytes(
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
    match foundation.browser.artifact_bytes(&id) {
        Ok(Some((artifact, bytes))) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, artifact.mime)],
            bytes,
        )
            .into_response(),
        Ok(None) => wire_status(not_found("browser artifact")),
        Err(e) => foundation_error(e),
    }
}

// ----------------------------------------------------------- context

pub(crate) async fn native_context_pins(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let handle = match native_resolve_session(&state, &id) {
        Ok(handle) => handle,
        Err(r) => return *r,
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation.context.pins(handle.id().raw()) {
        Ok(pins) => Json(serde_json::json!({ "pins": pins })).into_response(),
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ContextPinBody {
    source_id: String,
    action: String,
}

pub(crate) async fn native_context_pin_set(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<ContextPinBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return malformed("invalid context pin body"),
    };
    let handle = match native_resolve_session(&state, &id) {
        Ok(handle) => handle,
        Err(r) => return *r,
    };
    let Some(action) = PinAction::parse(&req.action) else {
        return malformed("action must be pin or exclude");
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation
        .context
        .set_pin(handle.id().raw(), &req.source_id, action)
    {
        Ok(pin) => Json(pin).into_response(),
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ContextPinDeleteQuery {
    source_id: String,
}

pub(crate) async fn native_context_pin_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    query: Result<Query<ContextPinDeleteQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(q) => q,
        Err(_) => return wire_status(malformed_body("invalid query parameters")),
    };
    let handle = match native_resolve_session(&state, &id) {
        Ok(handle) => handle,
        Err(r) => return *r,
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation
        .context
        .remove_pin(handle.id().raw(), &q.source_id)
    {
        Ok(removed) => Json(serde_json::json!({ "removed": removed })).into_response(),
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ContextExportQuery {
    session: String,
}

pub(crate) async fn native_context_export(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<ContextExportQuery>, axum::extract::rejection::QueryRejection>,
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
    match foundation.context.export(handle.id().raw()) {
        Ok(export) => Json(export).into_response(),
        Err(e) => foundation_error(e),
    }
}

// -------------------------------------------------------- efficiency

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct EfficiencyQuery {
    #[serde(default)]
    limit: Option<u64>,
}

/// `GET /native/session/{id}/efficiency` — recorded compaction effects,
/// cache observations and routing outcomes, each with its own honest state.
/// Absent measurements are `unavailable`, never a fabricated zero.
pub(crate) async fn native_session_efficiency(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    query: Result<Query<EfficiencyQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(q) => q,
        Err(_) => return wire_status(malformed_body("invalid query parameters")),
    };
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    // Compaction events are read through a bounded journal window. Anchor
    // the window to the durable head so a long session still shows its
    // NEWEST effects (the earlier window would silently stop at 50k).
    let head = match state.deps.session.store().last_event_seq(handle.id()) {
        Ok(head) => head.map(|seq| seq.raw()).unwrap_or(0),
        Err(e) => return api_err(&store_err_to_core(e)),
    };
    let from = head.saturating_sub(49_999);
    let events = match state
        .deps
        .session
        .store()
        .events_range(handle.id(), from, Some(50_000))
    {
        Ok(events) => events,
        Err(e) => return api_err(&store_err_to_core(e)),
    };
    let mut compactions = Vec::new();
    for event in events.iter().rev().take(limit as usize) {
        if matches!(
            event.kind,
            faktor_core::EventKind::ContextCompacted | faktor_core::EventKind::CompactRejected
        ) {
            let payload = event.payload.clone().unwrap_or(serde_json::Value::Null);
            compactions.push(serde_json::json!({
                "seq": event.seq.raw(),
                "observedMs": event.ts_ms,
                "accepted": payload.get("accepted").and_then(|v| v.as_bool()),
                "before": payload.get("before"),
                "after": payload.get("after"),
                "target": payload.get("target"),
                "reduction": payload.get("reduction"),
                "strategy": payload.get("strategy"),
                "reason": payload.get("reason"),
            }));
        }
    }
    let receipts = match foundation
        .receipts
        .page(handle.id().raw(), None, limit as usize)
    {
        Ok(receipts) => receipts,
        Err(e) => return foundation_error(e),
    };
    let mut routes: std::collections::BTreeMap<String, u64> = std::collections::BTreeMap::new();
    let mut tokens_in = 0u64;
    let mut tokens_out = 0u64;
    let mut token_calls = 0u64;
    for receipt in &receipts {
        *routes.entry(receipt.route.clone()).or_default() += 1;
        if let (Some(input), Some(output)) = (receipt.tokens_in, receipt.tokens_out) {
            tokens_in = tokens_in.saturating_add(input);
            tokens_out = tokens_out.saturating_add(output);
            token_calls += 1;
        }
    }
    Json(serde_json::json!({
        "session": id,
        "compaction": compactions,
        "cache": {
            "state": "unavailable",
            "reason": "provider cache read/write observations are not persisted by this daemon",
        },
        "routing": {
            "recordedReceipts": receipts.len(),
            "routes": routes,
            "outcomes": {
                "state": "unavailable",
                "reason": "routing outcomes are verified through the outcome ledger, not inferred from receipts",
            },
        },
        "tokens": if token_calls == 0 {
            serde_json::json!({"state": "unavailable", "reason": "no receipt carries both token counts"})
        } else {
            serde_json::json!({
                "state": "observed",
                "input": tokens_in,
                "output": tokens_out,
                "calls": token_calls,
                "basis": "call receipts",
            })
        },
        "localOperatingCost": {
            "state": "unavailable",
            "reason": "no measured local operating cost is recorded",
        },
    }))
    .into_response()
}

// ------------------------------------------------------------ budget

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct BudgetQuery {
    task_id: String,
}

fn budget_state_json(budget: faktor_session::budget::BudgetState) -> serde_json::Value {
    match budget {
        faktor_session::budget::BudgetState::Known(view) => serde_json::json!({
            "state": "known",
            "maxCostMicro": view.max_cost_micro.map(|v| v.to_string()),
            "spentCostMicro": view.spent_cost_micro.to_string(),
            "openReservedMicro": view.open_reserved_micro.to_string(),
            "openReservations": view.open_reservations,
            "uncertainReservedMicro": view.uncertain_reserved_micro.to_string(),
            "uncertainReservations": view.uncertain_reservations,
            "settledReservations": view.settled_count,
            "freeMicro": view.free().to_string(),
        }),
        faktor_session::budget::BudgetState::Unavailable { reason } => serde_json::json!({
            "state": "unavailable",
            "reason": reason,
        }),
    }
}

pub(crate) async fn native_task_budget_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    query: Result<Query<BudgetQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(q) => q,
        Err(_) => return wire_status(malformed_body("invalid query parameters")),
    };
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    let task = match task_from(&q.task_id) {
        Ok(task) => task,
        Err(r) => return r,
    };
    let budget = state.deps.session.budget_state(handle.id(), task).await;
    Json(budget_state_json(budget)).into_response()
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct BudgetRevisionBody {
    task_id: String,
    #[serde(default, with = "faktor_cloud::money::option")]
    max_cost_micro: Option<u64>,
    /// Explicitly remove the cap. Mutually exclusive with maxCostMicro.
    #[serde(default)]
    clear: bool,
}

/// `PUT /native/session/{id}/budget` — a durable budget revision. Lowering
/// below settled spend or open/uncertain reservations is a typed 409 from
/// the ledger; settled spend is never erased.
pub(crate) async fn native_task_budget_put(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<BudgetRevisionBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return malformed("invalid budget revision body"),
    };
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    let task = match task_from(&req.task_id) {
        Ok(task) => task,
        Err(r) => return r,
    };
    let cap = match (req.clear, req.max_cost_micro) {
        (true, None) => None,
        // The durable task-row cap is 1..=i64::MAX: zero is ambiguous (the
        // store reads NULL/0 as unlimited) and must use clear:true instead,
        // while a larger value cannot be stored without clamping.
        (false, Some(cap)) if cap == 0 || cap > faktor_session::budget::MAX_TASK_COST_CAP_MICRO => {
            return malformed(
                "maxCostMicro must be 1..=i64::MAX; use clear:true to remove the cap",
            );
        }
        (false, Some(cap)) => Some(cap),
        _ => {
            return malformed("send exactly one of maxCostMicro or clear:true");
        }
    };
    match state.deps.budgets.set_task_max_cost(handle.id(), task, cap) {
        Ok(()) => {
            let budget = state.deps.session.budget_state(handle.id(), task).await;
            Json(serde_json::json!({
                "acknowledged": true,
                "taskId": task.raw().to_string(),
                "budget": budget_state_json(budget),
            }))
            .into_response()
        }
        Err(e) => wire_status(ApiError {
            code: "budget_revision_refused",
            message: e.to_string(),
            http_status: 409,
            retryable: false,
        }),
    }
}

// ------------------------------------------------- config provenance

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ProvenanceQuery {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    limit: Option<u64>,
}

/// `GET /native/config/provenance` — every effective key with its winning
/// origin, all shadowed origins and the descriptor's scope/apply contract.
pub(crate) async fn native_config_provenance(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<ProvenanceQuery>, axum::extract::rejection::QueryRejection>,
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
    let snapshot = match foundation.config.snapshot() {
        Ok(snapshot) => snapshot,
        Err(e) => return foundation_error(e),
    };
    let needle = q.q.as_deref().map(str::to_ascii_lowercase);
    let limit = q.limit.unwrap_or(200).clamp(1, 500) as usize;
    let mut rows = Vec::new();
    for (key, descriptor) in foundation.config.descriptors() {
        if q.category
            .as_deref()
            .is_some_and(|c| c != descriptor.category)
        {
            continue;
        }
        if needle.as_deref().is_some_and(|needle| {
            !key.to_ascii_lowercase().contains(needle)
                && !descriptor.label.to_ascii_lowercase().contains(needle)
        }) {
            continue;
        }
        let effective = snapshot.values.get(key);
        rows.push(serde_json::json!({
            "key": key,
            "label": descriptor.label,
            "category": descriptor.category,
            "default": descriptor.default,
            "allowedScopes": descriptor.allowed_scopes,
            "writable": descriptor.writable,
            "applyTiming": descriptor.apply_timing,
            "runtimeConsumer": faktor_foundation::config_consumers::runtime_consumer(key),
            "effective": effective,
        }));
        if rows.len() >= limit {
            break;
        }
    }
    Json(serde_json::json!({
        "revision": snapshot.revision,
        "runtimeRevision": snapshot.runtime_revision,
        "rows": rows,
        "conflicts": snapshot.conflicts,
    }))
    .into_response()
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ConfigResetBody {
    expected_revision: u64,
    key: String,
    scope: faktor_foundation::Scope,
}

/// `POST /native/config/reset` — deliberately remove ONE scope override as
/// a validated atomic patch; the inherited value wins afterwards.
pub(crate) async fn native_config_reset(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<ConfigResetBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return malformed("invalid config reset body"),
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let patch = faktor_foundation::Patch {
        expected_revision: req.expected_revision,
        changes: vec![faktor_foundation::Change {
            key: req.key,
            scope: req.scope,
            value: None,
        }],
    };
    match foundation.config.commit(&patch) {
        Ok(receipt) => Json(receipt).into_response(),
        Err(e) => foundation_error(e),
    }
}

#[cfg(test)]
#[path = "foundation_r8_tests.rs"]
pub(crate) mod foundation_r8_tests;

// ------------------------------------------------ preset roles/fallback

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PresetRolesBody {
    #[serde(default)]
    role_models: std::collections::BTreeMap<String, faktor_foundation::RoleModel>,
    #[serde(default)]
    expected_revision: Option<u64>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PresetFallbackBody {
    #[serde(default)]
    fallback_chain: Vec<faktor_foundation::FallbackCandidate>,
    #[serde(default)]
    expected_revision: Option<u64>,
}

/// Validate every binding against the SERVED registry. A role/fallback
/// naming an unregistered provider or an unserved model is refused before
/// persistence, so the runtime never applies an unresolvable preference.
fn validate_served_bindings(
    state: &AppState,
    role_models: &std::collections::BTreeMap<String, faktor_foundation::RoleModel>,
    fallback: &[faktor_foundation::FallbackCandidate],
) -> Result<(), Response> {
    let providers = state.deps.agent.deps().providers.clone();
    let check = |provider: &str, model: &str| -> Result<(), Response> {
        if providers.get(provider).is_none() {
            return Err(wire_status(ApiError {
                code: "unknown_provider",
                message: format!("provider {provider:?} is not registered with this daemon"),
                http_status: 409,
                retryable: false,
            }));
        }
        if providers.capabilities(provider, model).is_none() {
            return Err(wire_status(ApiError {
                code: "unserved_model",
                message: format!("model {model:?} is not served by provider {provider:?}"),
                http_status: 409,
                retryable: false,
            }));
        }
        Ok(())
    };
    for binding in role_models.values() {
        check(&binding.provider, &binding.model)?;
    }
    for candidate in fallback {
        check(&candidate.provider, &candidate.model)?;
    }
    Ok(())
}

/// `GET /native/presets/{id}/roles` — the persisted per-role model map with
/// the served/resolvable status of every binding.
pub(crate) async fn native_preset_roles_get(
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
    let record = match foundation.presets.get(&id) {
        Ok(Some(record)) => record,
        Ok(None) => return wire_status(not_found(&format!("preset {id}"))),
        Err(e) => return foundation_error(e),
    };
    let providers = state.deps.agent.deps().providers.clone();
    let served: serde_json::Map<String, serde_json::Value> = record
        .role_models
        .iter()
        .map(|(role, binding)| {
            let caps = providers.capabilities(&binding.provider, &binding.model);
            let limits = caps.map(|c| (c.context, c.max_output));
            (
                role.clone(),
                serde_json::json!({
                    "provider": binding.provider,
                    "model": binding.model,
                    "reasoningEffort": binding.reasoning_effort,
                    "served": limits.is_some(),
                    "contextLimit": limits.map(|(context, _)| context),
                    "maxOutput": limits.map(|(_, max_output)| max_output),
                }),
            )
        })
        .collect();
    Json(serde_json::json!({
        "presetId": record.id,
        "revision": record.revision,
        "roleModels": record.role_models,
        "served": served,
    }))
    .into_response()
}

/// `POST /native/presets/{id}/roles` — replace the role map atomically.
pub(crate) async fn native_preset_roles_set(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<PresetRolesBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid preset roles body")),
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    if let Err(response) = validate_served_bindings(&state, &body.role_models, &[]) {
        return response;
    }
    match foundation
        .presets
        .set_role_models(&id, body.role_models, body.expected_revision)
    {
        Ok(record) => Json(serde_json::json!({
            "applied": true,
            "applyTiming": "next_child_or_model_call",
            "presetId": record.id,
            "revision": record.revision,
            "version": record.version,
            "roleModels": record.role_models,
        }))
        .into_response(),
        Err(e) => foundation_error(e),
    }
}

/// `GET /native/presets/{id}/fallback` — the ordered chain with per-candidate
/// served status; order is authoritative (never re-sorted).
pub(crate) async fn native_preset_fallback_get(
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
    let record = match foundation.presets.get(&id) {
        Ok(Some(record)) => record,
        Ok(None) => return wire_status(not_found(&format!("preset {id}"))),
        Err(e) => return foundation_error(e),
    };
    let providers = state.deps.agent.deps().providers.clone();
    let candidates: Vec<serde_json::Value> = record
        .fallback_chain
        .iter()
        .enumerate()
        .map(|(index, candidate)| {
            let caps = providers.capabilities(&candidate.provider, &candidate.model);
            serde_json::json!({
                "index": index,
                "provider": candidate.provider,
                "model": candidate.model,
                "reason": candidate.reason,
                "qualityFloorPpm": candidate.quality_floor_ppm,
                "served": caps.is_some(),
            })
        })
        .collect();
    Json(serde_json::json!({
        "presetId": record.id,
        "revision": record.revision,
        "fallbackChain": record.fallback_chain,
        "candidates": candidates,
    }))
    .into_response()
}

/// `POST /native/presets/{id}/fallback` — replace the ordered chain.
pub(crate) async fn native_preset_fallback_set(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<PresetFallbackBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid preset fallback body")),
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    if let Err(response) = validate_served_bindings(
        &state,
        &std::collections::BTreeMap::new(),
        &body.fallback_chain,
    ) {
        return response;
    }
    match foundation
        .presets
        .set_fallback_chain(&id, body.fallback_chain, body.expected_revision)
    {
        Ok(record) => Json(serde_json::json!({
            "applied": true,
            "applyTiming": "next_routing_decision",
            "presetId": record.id,
            "revision": record.revision,
            "version": record.version,
            "fallbackChain": record.fallback_chain,
        }))
        .into_response(),
        Err(e) => foundation_error(e),
    }
}

/// `GET /native/telemetry` — the explicit opt-in telemetry settings and the
/// exact bounded payload preview. There is deliberately no upload endpoint in
/// this build: the response says so and never turns "off" into a green
/// connected state.
pub(crate) async fn native_telemetry_preview(
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
    let enabled = foundation
        .config
        .value("faktor.telemetry.enabled")
        .ok()
        .flatten()
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let categories = foundation
        .config
        .value("faktor.telemetry.categories")
        .ok()
        .flatten()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|value| value.as_str().map(str::to_string))
        .collect::<Vec<_>>();
    Json(serde_json::json!({
        "enabled": enabled,
        "state": if enabled { "local_only" } else { "disabled" },
        "categories": categories,
        "endpoint": null,
        "endpointScope": "none: no upload endpoint is configured; local-only counters",
        "payloadPreview": {
            "schema": "faktor.telemetry-preview/v1",
            "fields": ["category", "event", "count", "duration_ms", "version"],
            "excluded": ["prompt text", "source code", "file contents", "secrets", "credentials", "tool arguments"],
            "bounded": true,
        },
        "notes": [
            "Telemetry is opt-in and defaults off; enabling it only permits the listed local counters.",
            "No remote upload, feedback endpoint or compliance badge is claimed by this daemon build."
        ],
    }))
    .into_response()
}
