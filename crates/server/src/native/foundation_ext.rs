//! Edition 5 foundation surfaces that sit beside session/tool authorities:
//! MCP/skills/hooks lifecycle, memory CRUD/retention, durable session
//! metadata/search/parallel listing, transcript search, the tool catalog,
//! browser boundary introspection and checkpoint fork/restore + partial
//! merge. Every mutation is bounded and returns the durable identity it
//! wrote; every listing is paged.
#![allow(clippy::result_large_err)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use faktor_foundation::customization::{
    HookDraft, McpServerDraft, McpServerRecord, SkillDraft, ORIGIN_PROJECT,
};
use faktor_foundation::session_meta::SessionMetaPatch;
use faktor_foundation::{Foundation, FoundationError, ReviewCommentDraft, ReviewCommentPatch};
use faktor_hooks::{HookEvent, HookInput, HookRegistry, HookSpec};
use faktor_instructions::SkillRegistry;
use faktor_mcp::{McpConfig, McpServer};
use faktor_protocol::error::ApiError;
use faktor_terminal::ProcessSupervisor;

use super::foundation::foundation_error;
use super::*;
use crate::api::AppState;

fn foundation(state: &AppState) -> Result<&Foundation, Response> {
    state.deps.foundation.as_deref().ok_or_else(|| {
        wire_status(ApiError {
            code: "foundation_disabled",
            message: "the Edition 5 foundation is not wired in this host".into(),
            http_status: 503,
            retryable: false,
        })
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

fn unavailable(message: impl Into<String>) -> Response {
    wire_status(ApiError {
        code: "unavailable",
        message: message.into(),
        http_status: 503,
        retryable: false,
    })
}

fn resolve_handle(state: &AppState, id: &str) -> Result<faktor_session::SessionHandle, Response> {
    native_resolve_session(state, id).map_err(|r| *r)
}

fn session_u64(id: SessionId) -> u64 {
    id.raw()
}

// --------------------------------------------------------------- sessions

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeSessionListQuery {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    archived: Option<bool>,
    #[serde(default)]
    pinned: Option<bool>,
    #[serde(default)]
    limit: Option<u64>,
}

/// `GET /native/sessions?q=&workspace=&state=&archived=&pinned=&limit=` —
/// one SQL listing + one metadata read, filtered and bounded. Archived
/// sessions are hidden unless `archived=true`; `q` searches durable titles.
#[allow(dead_code)] // filtered/parallel listing contract; the primary route owns GET /native/sessions.
pub(crate) async fn native_sessions_filtered(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<NativeSessionListQuery>, axum::extract::rejection::QueryRejection>,
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
    let limit = q.limit.unwrap_or(1000).clamp(1, 1000);
    let rows = if let Some(query) = q.q.as_deref().filter(|s| !s.trim().is_empty()) {
        match state
            .deps
            .session
            .store()
            .search_session_titles(query, limit)
        {
            Ok(rows) => rows,
            Err(e) => return api_err(&store_err_to_core(e)),
        }
    } else {
        match state.deps.session.store().list_sessions(None) {
            Ok(rows) => rows,
            Err(e) => return api_err(&store_err_to_core(e)),
        }
    };
    let meta = match foundation.session_meta.all() {
        Ok(meta) => meta,
        Err(e) => return foundation_error(e),
    };
    let workspace_filter = q.workspace.as_deref();
    let state_filter = q.state.as_deref();
    let archived_filter = q.archived.unwrap_or(false);
    let pinned_filter = q.pinned;
    let mut out = Vec::new();
    for row in rows {
        let m = meta
            .get(&session_u64(row.id))
            .cloned()
            .unwrap_or(faktor_foundation::SessionMeta {
                session_id: session_u64(row.id),
                pinned: false,
                archived: false,
                updated_ms: 0,
            });
        if m.archived != archived_filter {
            continue;
        }
        if let Some(pinned) = pinned_filter {
            if m.pinned != pinned {
                continue;
            }
        }
        if let Some(workspace) = workspace_filter {
            if row.workspace_id.raw().to_string() != workspace
                && row.worktree_id.raw().to_string() != workspace
            {
                continue;
            }
        }
        if let Some(state_filter) = state_filter {
            if agent_state_tag(row.state) != state_filter {
                continue;
            }
        }
        out.push(serde_json::json!({
            "id": row.id.to_string(),
            "title": row.title,
            "provider": row.provider,
            "model": row.model,
            "state": agent_state_tag(row.state),
            "active": row.state.is_active(),
            "workspaceId": row.workspace_id.raw(),
            "worktreeId": row.worktree_id.raw(),
            "taskId": row.task_id.raw(),
            "createdMs": row.created_ms,
            "updatedMs": row.updated_ms,
            "pinned": m.pinned,
            "archived": m.archived,
        }));
        if out.len() >= limit as usize {
            break;
        }
    }
    Json(serde_json::json!({ "sessions": out, "limit": limit })).into_response()
}

/// `GET /native/sessions/parallel` — the parallel-conversation view: one
/// row per session with active-run and pin/archive badges, grouped counts.
pub(crate) async fn native_sessions_parallel(
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
    let rows = match state.deps.session.store().list_sessions(None) {
        Ok(rows) => rows,
        Err(e) => return api_err(&store_err_to_core(e)),
    };
    let meta = match foundation.session_meta.all() {
        Ok(meta) => meta,
        Err(e) => return foundation_error(e),
    };
    let mut active = 0usize;
    let mut by_workspace: BTreeMap<u64, usize> = BTreeMap::new();
    let mut sessions = Vec::new();
    for row in rows.iter().take(1000) {
        let m = meta
            .get(&session_u64(row.id))
            .cloned()
            .unwrap_or(faktor_foundation::SessionMeta {
                session_id: session_u64(row.id),
                pinned: false,
                archived: false,
                updated_ms: 0,
            });
        if row.state.is_active() {
            active += 1;
        }
        *by_workspace.entry(row.workspace_id.raw()).or_insert(0) += 1;
        sessions.push(serde_json::json!({
            "id": row.id.to_string(),
            "title": row.title,
            "workspaceId": row.workspace_id.raw(),
            "worktreeId": row.worktree_id.raw(),
            "taskId": row.task_id.raw(),
            "state": agent_state_tag(row.state),
            "active": row.state.is_active(),
            "pinned": m.pinned,
            "archived": m.archived,
            "updatedMs": row.updated_ms,
        }));
    }
    Json(serde_json::json!({
        "sessions": sessions,
        "activeRuns": active,
        "byWorkspace": by_workspace
            .into_iter()
            .map(|(id, count)| serde_json::json!({ "workspaceId": id, "sessions": count }))
            .collect::<Vec<_>>(),
    }))
    .into_response()
}

/// `GET /native/sessions/{id}/meta`
pub(crate) async fn native_session_meta(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let sid = match parse_session_id(&id) {
        Ok(s) => s,
        Err(e) => return wire_status(e),
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation.session_meta.get(session_u64(sid)) {
        Ok(meta) => Json(meta).into_response(),
        Err(e) => foundation_error(e),
    }
}

/// `POST /native/sessions/{id}/meta` — pin/archive; `title` renames through
/// the session store's durable title authority.
pub(crate) async fn native_session_meta_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<NativeSessionMetaBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid session meta body")),
    };
    let handle = match resolve_handle(&state, &id) {
        Ok(h) => h,
        Err(r) => return r,
    };
    if let Some(title) = req.title.as_deref() {
        if title.trim().is_empty() || title.len() > 256 {
            return wire_status(malformed_body("title must be 1..=256 bytes"));
        }
        match state
            .deps
            .session
            .store()
            .update_session_title(handle.id(), title.trim())
        {
            Ok(true) => {}
            Ok(false) => return wire_status(not_found("session")),
            Err(e) => return api_err(&store_err_to_core(e)),
        }
    }
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    if req.pinned.is_none() && req.archived.is_none() {
        return Json(serde_json::json!({ "ok": true })).into_response();
    }
    match foundation.session_meta.update(
        session_u64(handle.id()),
        &SessionMetaPatch {
            pinned: req.pinned,
            archived: req.archived,
        },
    ) {
        Ok(meta) => Json(meta).into_response(),
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeSessionMetaBody {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    pinned: Option<bool>,
    #[serde(default)]
    archived: Option<bool>,
}

// ------------------------------------------------------------ transcript

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeTranscriptSearchQuery {
    session: String,
    q: String,
    #[serde(default)]
    limit: Option<u64>,
}

/// `GET /native/transcript/search?session=&q=&limit=` — durable search over
/// stored message payloads (reaches unloaded history), newest-first, bounded.
pub(crate) async fn native_transcript_search(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<NativeTranscriptSearchQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(q) => q,
        Err(_) => return wire_status(malformed_body("invalid query parameters")),
    };
    let handle = match resolve_handle(&state, &q.session) {
        Ok(h) => h,
        Err(r) => return r,
    };
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    match state
        .deps
        .session
        .store()
        .search_messages(handle.id(), &q.q, limit)
    {
        Ok(rows) => {
            let needle = q.q.to_ascii_lowercase();
            // A match may live in `message.data` (user prompts) or in a part
            // row (`text`/`reasoning`/`tool_result`), which is where every
            // assistant answer is stored. The snippet must quote the row that
            // actually matched, not an empty assistant envelope. Part reads
            // are bounded to this page (never the whole transcript) and a
            // read failure fails the page loudly instead of guessing text.
            let mut matches: Vec<serde_json::Value> = Vec::with_capacity(rows.len());
            for row in &rows {
                let mut text = row
                    .data
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                if !text.to_ascii_lowercase().contains(&needle) {
                    let parts = match state.deps.session.store().parts_of(row.id) {
                        Ok(parts) => parts,
                        Err(e) => return api_err(&store_err_to_core(e)),
                    };
                    text = parts
                        .iter()
                        .filter_map(|part| {
                            let body = &part.data;
                            let candidate = body
                                .get("text")
                                .or_else(|| body.get("excerpt"))
                                .or_else(|| body.get("output"))
                                .and_then(|v| v.as_str())
                                .map(str::to_string)
                                .unwrap_or_else(|| body.to_string());
                            candidate
                                .to_ascii_lowercase()
                                .contains(&needle)
                                .then_some(candidate)
                        })
                        .next()
                        .unwrap_or_else(|| row.data.to_string());
                }
                let lower = text.to_ascii_lowercase();
                let offset = lower.find(&needle).unwrap_or(0);
                // `offset` is a byte index; skip CHARS, not bytes, so a
                // Unicode prefix cannot split the snippet.
                let before_chars = text[..offset].chars().count();
                matches.push(serde_json::json!({
                    "seq": row.seq,
                    "messageId": row.id,
                    "role": row.role,
                    "createdMs": row.created_ms,
                    "snippet": text.chars().skip(before_chars.saturating_sub(80)).take(240).collect::<String>(),
                    "matchOffset": offset,
                    "textLength": text.len(),
                }));
            }
            Json(serde_json::json!({
                "sessionId": handle.id().to_string(),
                "query": q.q,
                "matches": matches,
                "hasMore": rows.len() as u64 == limit,
            }))
            .into_response()
        }
        Err(e) => api_err(&store_err_to_core(e)),
    }
}

// ---------------------------------------------------------------- memory

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeMemoryQuery {
    session: String,
    #[serde(default, rename = "afterMs")]
    after_ms: Option<i64>,
    #[serde(default, rename = "afterKind")]
    after_kind: Option<String>,
    #[serde(default, rename = "afterKey")]
    after_key: Option<String>,
    #[serde(default)]
    limit: Option<u64>,
}

/// `GET /native/memory?session=&afterMs=&afterKind=&afterKey=&limit=`
pub(crate) async fn native_memory_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<NativeMemoryQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(q) => q,
        Err(_) => return wire_status(malformed_body("invalid query parameters")),
    };
    let handle = match resolve_handle(&state, &q.session) {
        Ok(h) => h,
        Err(r) => return r,
    };
    let cursor = match (q.after_ms, q.after_kind, q.after_key) {
        (Some(ms), Some(kind), Some(key)) => Some((ms, kind, key)),
        (None, None, None) => None,
        _ => {
            return wire_status(malformed_body(
                "memory cursor requires afterMs, afterKind and afterKey together",
            ))
        }
    };
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    match state
        .deps
        .session
        .store()
        .memory_facts_page(handle.id(), cursor.as_ref(), limit)
    {
        Ok((rows, has_more)) => Json(serde_json::json!({
            "facts": rows.iter().map(|r| serde_json::json!({
                "kind": r.kind,
                "key": r.key,
                "value": r.value,
                "updatedMs": r.updated_ms,
            })).collect::<Vec<_>>(),
            "hasMore": has_more,
            "nextCursor": rows.last().map(|r| serde_json::json!({
                "afterMs": r.updated_ms,
                "afterKind": r.kind,
                "afterKey": r.key,
            })),
        }))
        .into_response(),
        Err(e) => api_err(&store_err_to_core(e)),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeMemoryWriteBody {
    session: String,
    kind: String,
    key: String,
    value: String,
    #[serde(default, rename = "expectedUpdatedMs")]
    expected_updated_ms: Option<i64>,
}

/// `POST /native/memory` — create/update one fact with an optional exact
/// timestamp CAS. Scope is the PATHS session; nothing can redirect storage.
pub(crate) async fn native_memory_write(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<NativeMemoryWriteBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid memory body")),
    };
    let handle = match resolve_handle(&state, &req.session) {
        Ok(h) => h,
        Err(r) => return r,
    };
    if let Some(expected) = req.expected_updated_ms {
        let live =
            match state
                .deps
                .session
                .store()
                .get_memory_fact(handle.id(), &req.kind, &req.key)
            {
                Ok(row) => row,
                Err(e) => return api_err(&store_err_to_core(e)),
            };
        match live {
            Some(row) if row.updated_ms == expected => {}
            Some(_) => {
                return conflict(
                    "revision_conflict",
                    "the memory fact changed since it was read; re-read and merge",
                )
            }
            None => {
                return conflict(
                    "revision_conflict",
                    "the memory fact no longer exists; re-read",
                )
            }
        }
    }
    match handle.upsert_memory_fact(&req.kind, &req.key, &req.value) {
        Ok(()) => {
            let row = state
                .deps
                .session
                .store()
                .get_memory_fact(handle.id(), &req.kind, &req.key)
                .ok()
                .flatten();
            Json(serde_json::json!({
                "saved": true,
                "fact": row.map(|r| serde_json::json!({
                    "kind": r.kind, "key": r.key, "value": r.value, "updatedMs": r.updated_ms,
                })),
            }))
            .into_response()
        }
        Err(e) => api_err(&e),
    }
}

/// `DELETE /native/memory/{kind}/{key}?session=&expectedUpdatedMs=` — delete
/// with a receipt: future use stops at the current durable event revision and
/// previously injected copies are disclosed as retained (never falsely
/// claimed erased from historical prompts/artifacts).
pub(crate) async fn native_memory_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((kind, key)): Path<(String, String)>,
    query: Result<Query<NativeMemoryDeleteQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(q) => q,
        Err(_) => return wire_status(malformed_body("invalid query parameters")),
    };
    let handle = match resolve_handle(&state, &q.session) {
        Ok(h) => h,
        Err(r) => return r,
    };
    let live = match state
        .deps
        .session
        .store()
        .get_memory_fact(handle.id(), &kind, &key)
    {
        Ok(row) => row,
        Err(e) => return api_err(&store_err_to_core(e)),
    };
    let Some(live) = live else {
        return wire_status(not_found("memory fact"));
    };
    if let Some(expected) = q.expected_updated_ms {
        if live.updated_ms != expected {
            return conflict(
                "revision_conflict",
                "the memory fact changed since it was read; re-read and merge",
            );
        }
    }
    let revision = handle
        .last_event_seq()
        .ok()
        .flatten()
        .map(|s| s.raw())
        .unwrap_or(0);
    match state
        .deps
        .session
        .store()
        .delete_memory_fact(handle.id(), &kind, &key)
    {
        Ok(Some(deleted)) => Json(serde_json::json!({
            "deleted": true,
            "receipt": {
                "kind": deleted.kind,
                "key": deleted.key,
                "deletedValueHash": faktor_foundation::hash_hex(deleted.value.as_bytes()),
                "deletedAtRevision": revision,
                "futureUseStoppedAtRevision": revision,
                "retainedPriorCopies": true,
                "retainedPriorArtifactRefs": [],
                "retentionNote": "prior prompt/artifact copies already materialized are retained by their owning stores; this deletion stops future memory reads",
            },
        }))
        .into_response(),
        Ok(None) => wire_status(not_found("memory fact")),
        Err(e) => api_err(&store_err_to_core(e)),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeMemoryDeleteQuery {
    session: String,
    #[serde(default, rename = "expectedUpdatedMs")]
    expected_updated_ms: Option<i64>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeRetentionBody {
    session: String,
    #[serde(rename = "olderThanDays")]
    older_than_days: u64,
    #[serde(default, rename = "dryRun")]
    dry_run: bool,
    #[serde(default)]
    limit: Option<u64>,
}

/// `POST /native/memory/retention` — bounded retention pass (dry-run lists
/// the exact rows a real pass would delete).
pub(crate) async fn native_memory_retention(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<NativeRetentionBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid retention body")),
    };
    if req.older_than_days > 3_650 {
        return wire_status(malformed_body("olderThanDays must be <= 3650"));
    }
    let handle = match resolve_handle(&state, &req.session) {
        Ok(h) => h,
        Err(r) => return r,
    };
    let cutoff = faktor_foundation::db::now_ms()
        .saturating_sub(req.older_than_days.saturating_mul(86_400_000) as i64);
    let limit = req.limit.unwrap_or(500).clamp(1, 1000);
    let rows = match state
        .deps
        .session
        .store()
        .memory_facts_page(handle.id(), None, limit)
    {
        Ok((rows, _)) => rows,
        Err(e) => return api_err(&store_err_to_core(e)),
    };
    let candidates: Vec<serde_json::Value> = rows
        .iter()
        .filter(|r| r.updated_ms < cutoff)
        .map(|r| {
            serde_json::json!({
                "kind": r.kind, "key": r.key, "updatedMs": r.updated_ms,
            })
        })
        .collect();
    if req.dry_run {
        return Json(serde_json::json!({
            "dryRun": true,
            "cutoffMs": cutoff,
            "wouldDelete": candidates.len(),
            "candidates": candidates,
        }))
        .into_response();
    }
    match state
        .deps
        .session
        .store()
        .delete_memory_facts_before(handle.id(), cutoff, limit)
    {
        Ok(deleted) => Json(serde_json::json!({
            "dryRun": false,
            "cutoffMs": cutoff,
            "deleted": deleted.len(),
            "deletedFacts": deleted.iter().map(|r| serde_json::json!({
                "kind": r.kind, "key": r.key, "updatedMs": r.updated_ms,
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => api_err(&store_err_to_core(e)),
    }
}

// ------------------------------------------------------------------- MCP

/// Owns the live stdio MCP children for this daemon. Starts are serialized
/// and explicit; stopping kills the child's process group through the
/// daemon's ONE supervisor.
pub struct McpRuntime {
    supervisor: Option<Arc<ProcessSupervisor>>,
    live: tokio::sync::Mutex<HashMap<String, Arc<McpServer>>>,
}

impl McpRuntime {
    pub fn new(supervisor: Option<Arc<ProcessSupervisor>>) -> Self {
        Self {
            supervisor,
            live: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    async fn start(
        &self,
        record: &McpServerRecord,
        env: Vec<(String, String)>,
    ) -> Result<Vec<serde_json::Value>, String> {
        if !record.trusted {
            return Err("this MCP server is untrusted; review executable/origin and trust it before starting".into());
        }
        if record.transport != "stdio" {
            return Err(
                "this daemon build supports bounded stdio MCP only; streamable_http requires a negotiated transport adapter"
                    .into(),
            );
        }
        let supervisor = self
            .supervisor
            .clone()
            .ok_or_else(|| "the daemon process supervisor is not wired".to_string())?;
        let mut live = self.live.lock().await;
        if let Some(server) = live.get(&record.id) {
            if server.is_alive() {
                let tools = server.list_tools().await.map_err(|e| e.to_string())?;
                return tools_json_bounded(tools, record.output_cap_bytes);
            }
            live.remove(&record.id);
        }
        let server = McpServer::connect(
            McpConfig {
                name: record.name.clone(),
                command: record.command.clone(),
                args: record.args.clone(),
                env,
            },
            supervisor,
        )
        .await
        .map_err(|e| format!("mcp connect: {e}"))?;
        let tools = server
            .list_tools()
            .await
            .map_err(|e| format!("mcp tools/list: {e}"))?;
        let tools = tools_json_bounded(tools, record.output_cap_bytes);
        if tools.is_err() {
            let _ = server.close().await;
            return tools;
        }
        live.insert(record.id.clone(), server);
        tools
    }

    async fn stop(&self, id: &str) -> bool {
        let server = {
            let mut live = self.live.lock().await;
            live.remove(id)
        };
        if let Some(server) = server {
            let _ = server.close().await;
            true
        } else {
            false
        }
    }

    async fn tools(
        &self,
        id: &str,
        cap: Option<u64>,
    ) -> Result<Option<Vec<serde_json::Value>>, String> {
        let server = {
            let live = self.live.lock().await;
            live.get(id).cloned()
        };
        match server {
            Some(server) if server.is_alive() => {
                let tools = server
                    .list_tools()
                    .await
                    .map_err(|e| format!("mcp tools/list: {e}"))?;
                let values = tools_json(tools)?;
                if let Some(cap) = cap {
                    let bytes = serde_json::to_vec(&values)
                        .map_err(|e| format!("tools/list encoding: {e}"))?
                        .len() as u64;
                    if bytes > cap {
                        return Err(format!(
                            "tools/list output is {bytes} bytes, over the configured {cap}-byte cap"
                        ));
                    }
                }
                Ok(Some(values))
            }
            _ => Ok(None),
        }
    }

    async fn health(&self, id: &str) -> Option<bool> {
        let live = self.live.lock().await;
        live.get(id).map(|server| server.is_alive())
    }
}

fn tools_json(tools: Vec<faktor_mcp::McpTool>) -> Result<Vec<serde_json::Value>, String> {
    Ok(tools
        .into_iter()
        .map(|t| {
            serde_json::json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": t.input_schema,
            })
        })
        .collect())
}

fn tools_json_bounded(
    tools: Vec<faktor_mcp::McpTool>,
    cap: Option<u64>,
) -> Result<Vec<serde_json::Value>, String> {
    let values = tools_json(tools)?;
    if let Some(cap) = cap {
        let bytes = serde_json::to_vec(&values)
            .map_err(|e| format!("tools/list encoding: {e}"))?
            .len() as u64;
        if bytes > cap {
            return Err(format!(
                "tools/list output is {bytes} bytes, over the configured {cap}-byte cap; the catalog was not exposed"
            ));
        }
    }
    Ok(values)
}

fn resolve_mcp_env(
    foundation: &Foundation,
    entries: &[(String, String)],
) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for (name, reference) in entries {
        if reference.is_empty() {
            if let Ok(value) = std::env::var(name) {
                out.push((name.clone(), value));
            }
            continue;
        }
        if let Some(rest) = reference.strip_prefix("vault://") {
            let bytes = foundation
                .vault
                .reveal(&format!("vault://{rest}"))
                .map_err(|e| format!("environment reference for {name}: {e}"))?;
            out.push((name.clone(), String::from_utf8_lossy(&bytes).into_owned()));
        } else if let Some(env_name) = reference.strip_prefix("env:") {
            let value = std::env::var(env_name)
                .map_err(|_| format!("environment reference for {name} is unset"))?;
            out.push((name.clone(), value));
        } else {
            return Err(format!(
                "environment entry {name:?} is not an opaque secret reference"
            ));
        }
    }
    Ok(out)
}

fn mcp_view(
    record: &McpServerRecord,
    health: Option<bool>,
    tools: Option<Vec<serde_json::Value>>,
) -> serde_json::Value {
    let state = match health {
        Some(true) => "ready",
        Some(false) => "failed",
        None => "stopped",
    };
    serde_json::json!({
        "record": record,
        "health": {
            "state": state,
            "note": if health.is_none() {
                "not started; starting is an explicit action"
            } else if health == Some(true) {
                "initialize handshake completed and the child answered tools/list"
            } else {
                "the child process is not alive"
            },
        },
        "tools": tools,
        "transport": record.transport,
        "auth": {
            "state": record.auth_state,
            "account": record.auth_account,
            "scopes": record.auth_scopes,
            "hasToken": record.auth_token_ref.is_some(),
        },
        "bounds": {
            "timeoutMs": record.timeout_ms,
            "outputCapBytes": record.output_cap_bytes,
            "stderrCapBytes": record.stderr_cap_bytes,
        },
        "remoteTransport": {
            "supported": false,
            "reason": "this daemon build supports bounded stdio MCP only; remote/OAuth transport requires a negotiated adapter",
        },
        "logs": {
            "available": false,
            "reason": "stdio children run with captured pipes owned by the MCP client; no retained log buffer exists",
        },
    })
}

/// `GET /native/mcp/servers` — bounded lifecycle listing with live health.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeMcpSearchQuery {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    limit: Option<u64>,
}

pub(crate) async fn native_mcp_servers(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<NativeMcpSearchQuery>, axum::extract::rejection::QueryRejection>,
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
    let records = match foundation.customization.mcp_servers() {
        Ok(records) => records,
        Err(e) => return foundation_error(e),
    };
    let needle = q.q.as_deref().unwrap_or("").to_ascii_lowercase();
    let limit = q.limit.unwrap_or(500).clamp(1, 500) as usize;
    let mut out = Vec::new();
    for record in records {
        if !needle.is_empty() {
            let haystack =
                format!("{} {} {}", record.id, record.name, record.command).to_ascii_lowercase();
            if !haystack.contains(&needle) {
                continue;
            }
        }
        let health = state.deps.mcp_runtime.health(&record.id).await;
        let tools = if health == Some(true) {
            state
                .deps
                .mcp_runtime
                .tools(&record.id, record.output_cap_bytes)
                .await
                .unwrap_or_default()
        } else {
            None
        };
        out.push(mcp_view(&record, health, tools));
        if out.len() >= limit {
            break;
        }
    }
    Json(serde_json::json!({ "servers": out })).into_response()
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeMcpSaveBody {
    #[serde(flatten)]
    draft: McpServerDraft,
    #[serde(default, rename = "expectedRevision")]
    expected_revision: Option<u64>,
}

/// `POST /native/mcp/servers` — validated stdio spec; never starts.
pub(crate) async fn native_mcp_server_save(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<NativeMcpSaveBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid MCP server body")),
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation
        .customization
        .save_mcp_server(&req.draft, req.expected_revision)
    {
        Ok(record) => Json(mcp_view(&record, None, None)).into_response(),
        Err(e) => foundation_error(e),
    }
}

/// `POST /native/mcp/servers/{id}/start` — explicit start; untrusted/
/// unsupported servers refuse typed, never a fake ready.
pub(crate) async fn native_mcp_server_start(
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
    let record = match foundation.customization.mcp_server(&id) {
        Ok(Some(record)) => record,
        Ok(None) => return wire_status(not_found("MCP server")),
        Err(e) => return foundation_error(e),
    };
    if !record.enabled {
        return conflict(
            "mcp_disabled",
            "this MCP server is saved but disabled; enable it before starting",
        );
    }
    let env = match resolve_mcp_env(foundation, &record.env) {
        Ok(env) => env,
        Err(message) => return conflict("mcp_environment", message),
    };
    let timeout = Duration::from_millis(record.timeout_ms.unwrap_or(10_000));
    match tokio::time::timeout(timeout, state.deps.mcp_runtime.start(&record, env)).await {
        Ok(Ok(tools)) => Json(mcp_view(&record, Some(true), Some(tools))).into_response(),
        Ok(Err(message)) => conflict("mcp_start_failed", message),
        Err(_) => {
            let _ = state.deps.mcp_runtime.stop(&id).await;
            conflict(
                "mcp_timeout",
                format!(
                    "start exceeded the configured {} ms bound; the child was stopped (no tool call was admitted)",
                    timeout.as_millis()
                ),
            )
        }
    }
}

/// `POST /native/mcp/servers/{id}/stop`
pub(crate) async fn native_mcp_server_stop(
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
    let record = match foundation.customization.mcp_server(&id) {
        Ok(Some(record)) => record,
        Ok(None) => return wire_status(not_found("MCP server")),
        Err(e) => return foundation_error(e),
    };
    let stopped = state.deps.mcp_runtime.stop(&id).await;
    Json(serde_json::json!({
        "record": record,
        "stopped": stopped,
        "health": { "state": "stopped" },
    }))
    .into_response()
}

/// `GET /native/mcp/servers/{id}/tools`
pub(crate) async fn native_mcp_server_tools(
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
    if !matches!(foundation.customization.mcp_server(&id), Ok(Some(_))) {
        return wire_status(not_found("MCP server"));
    }
    let cap = foundation
        .customization
        .mcp_server(&id)
        .ok()
        .flatten()
        .and_then(|record| record.output_cap_bytes);
    match state.deps.mcp_runtime.tools(&id, cap).await {
        Ok(Some(tools)) => {
            Json(serde_json::json!({ "serverId": id, "tools": tools })).into_response()
        }
        Ok(None) => conflict(
            "mcp_not_ready",
            "the server is not started; start it explicitly to discover tools",
        ),
        Err(message) => conflict("mcp_tools_failed", message),
    }
}

// SECRET-FIELD-GATE-WIRE-DTO: MCP OAuth callback body; the token is written
// straight to the vault and only the opaque handle is persisted.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct NativeMcpAuthBody {
    /// `connect` | `reconnect` | `revoke`
    action: String,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    account: Option<String>,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    expected_revision: Option<u64>,
}

/// `GET /native/mcp/servers/{id}/auth` — the durable auth binding without
/// any token material.
pub(crate) async fn native_mcp_server_auth_get(
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
    match foundation.customization.mcp_server(&id) {
        Ok(Some(record)) => Json(serde_json::json!({
            "serverId": record.id,
            "transport": record.transport,
            "state": record.auth_state,
            "account": record.auth_account,
            "scopes": record.auth_scopes,
            "hasToken": record.auth_token_ref.is_some(),
            "revision": record.revision,
        }))
        .into_response(),
        Ok(None) => wire_status(not_found("MCP server")),
        Err(e) => foundation_error(e),
    }
}

/// `POST /native/mcp/servers/{id}/auth` — OAuth/token lifecycle. The token
/// is written to the vault FIRST and only its opaque handle is persisted.
/// A failed operation leaves the previous binding untouched; `revoke`
/// removes the vault material and returns to `signed_out`.
pub(crate) async fn native_mcp_server_auth(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<NativeMcpAuthBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid MCP auth body")),
    };
    if !matches!(body.action.as_str(), "connect" | "reconnect" | "revoke") {
        return wire_status(malformed_body(
            "action must be connect, reconnect or revoke",
        ));
    }
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let record = match foundation.customization.mcp_server(&id) {
        Ok(Some(record)) => record,
        Ok(None) => return wire_status(not_found("MCP server")),
        Err(e) => return foundation_error(e),
    };
    if record.transport != "streamable_http" {
        return conflict(
            "mcp_oauth_unsupported",
            "OAuth binding applies to streamable_http servers; stdio servers use environment secret references",
        );
    }
    if body.action == "revoke" {
        if let Some(handle) = record.auth_token_ref.clone() {
            if let Err(e) = foundation.vault.delete(&handle) {
                return foundation_error(e);
            }
        }
        return match foundation.customization.set_mcp_auth(
            &id,
            None,
            None,
            Vec::new(),
            body.expected_revision,
        ) {
            Ok(record) => Json(serde_json::json!({
                "serverId": record.id,
                "state": record.auth_state,
                "account": record.auth_account,
                "scopes": record.auth_scopes,
                "revoked": true,
            }))
            .into_response(),
            Err(e) => foundation_error(e),
        };
    }
    let Some(token) = body.access_token.as_deref() else {
        return wire_status(malformed_body(
            "connect/reconnect requires the OAuth access token from the auth callback",
        ));
    };
    if token.is_empty() || token.len() > 8_192 || token.chars().any(char::is_control) {
        return wire_status(malformed_body(
            "accessToken must be 1..=8192 non-control bytes",
        ));
    }
    let secret = match foundation.vault.store(
        "mcp_oauth",
        &format!("MCP OAuth token for {}", record.name),
        token.as_bytes(),
    ) {
        Ok(secret) => secret,
        Err(e) => return foundation_error(e),
    };
    // Reconnect rotates: the OLD material is revoked only after the new
    // binding is durably written.
    let previous = record.auth_token_ref.clone();
    match foundation.customization.set_mcp_auth(
        &id,
        Some(secret.handle.clone()),
        body.account,
        body.scopes,
        body.expected_revision,
    ) {
        Ok(record) => {
            if let (Some(previous), true) = (previous, body.action == "reconnect") {
                let _ = foundation.vault.delete(&previous);
            }
            Json(serde_json::json!({
                "serverId": record.id,
                "transport": record.transport,
                "state": record.auth_state,
                "account": record.auth_account,
                "scopes": record.auth_scopes,
                "hasToken": true,
                "revision": record.revision,
                "connected": false,
                "note": "credential authorized; remote MCP transport is not negotiated by this build",
            }))
            .into_response()
        }
        Err(e) => {
            let _ = foundation.vault.delete(&secret.handle);
            foundation_error(e)
        }
    }
}

/// `POST /native/mcp/servers/{id}/connect` — remote (streamable HTTP)
/// transport is not part of this daemon build. The refusal is typed and
/// names the missing negotiated adapter: no connection state is invented.
pub(crate) async fn native_mcp_server_connect(
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
    match foundation.customization.mcp_server(&id) {
        Ok(Some(_)) => conflict(
            "mcp_remote_unsupported",
            "this daemon build supports bounded stdio MCP only; streamable HTTP requires a negotiated transport adapter",
        ),
        Ok(None) => wire_status(not_found("MCP server")),
        Err(e) => foundation_error(e),
    }
}

/// `POST /native/mcp/servers/{id}/oauth/revoke` — revoke the vault material
/// and durably return the server to `signed_out`.
pub(crate) async fn native_mcp_server_oauth_revoke(
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
    let record = match foundation.customization.mcp_server(&id) {
        Ok(Some(record)) => record,
        Ok(None) => return wire_status(not_found("MCP server")),
        Err(e) => return foundation_error(e),
    };
    if record.transport != "streamable_http" {
        return conflict(
            "mcp_oauth_unsupported",
            "OAuth binding applies to streamable_http servers only",
        );
    }
    if let Some(handle) = record.auth_token_ref.clone() {
        if let Err(e) = foundation.vault.delete(&handle) {
            return foundation_error(e);
        }
    }
    match foundation
        .customization
        .set_mcp_auth(&id, None, None, Vec::new(), None)
    {
        Ok(record) => Json(serde_json::json!({
            "serverId": record.id,
            "state": record.auth_state,
            "revoked": true,
        }))
        .into_response(),
        Err(e) => foundation_error(e),
    }
}

/// `GET /native/mcp/servers/{id}/resources` — MCP resources/prompts are not
/// negotiated by the stdio adapter; the typed refusal never fabricates a
/// catalog.
pub(crate) async fn native_mcp_server_resources(
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
    match foundation.customization.mcp_server(&id) {
        Ok(Some(_)) => conflict(
            "mcp_resources_unsupported",
            "this daemon build negotiates tools only; resources and prompts are not advertised by the stdio adapter",
        ),
        Ok(None) => wire_status(not_found("MCP server")),
        Err(e) => foundation_error(e),
    }
}

/// `DELETE /native/mcp/servers/{id}` — stops the child FIRST, then removes
/// the durable spec (a removed server can never keep running).
pub(crate) async fn native_mcp_server_delete(
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
    let stopped = state.deps.mcp_runtime.stop(&id).await;
    match foundation.customization.delete_mcp_server(&id) {
        Ok(()) => Json(serde_json::json!({ "deleted": true, "stopped": stopped })).into_response(),
        Err(e) => foundation_error(e),
    }
}

// ---------------------------------------------------------------- skills

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeSkillQuery {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    session: Option<String>,
    /// Bounded page size for DISCOVERED skills (default 500, max 500).
    /// Installed records are already bounded at 500. The response declares
    /// `discoveredTotal`/`hasMore` so a 500-skill workspace is never a
    /// silent 200-skill lie (the old hardcoded `.take(200)` was).
    #[serde(default)]
    limit: Option<u64>,
}

/// `GET /native/skills?q=&session=` — installed trusted records plus
/// discovered-but-not-installed workspace skills (metadata only).
pub(crate) async fn native_skills(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<NativeSkillQuery>, axum::extract::rejection::QueryRejection>,
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
    let records = match foundation.customization.skills() {
        Ok(records) => records,
        Err(e) => return foundation_error(e),
    };
    let needle = q.q.as_deref().unwrap_or("").to_ascii_lowercase();
    let discovered_limit = q.limit.unwrap_or(500).clamp(1, 500) as usize;
    let mut discovered_total: u64 = 0;
    let mut discovered_truncated = false;
    let mut installed: Vec<serde_json::Value> = records
        .iter()
        .filter(|r| {
            needle.is_empty()
                || r.name.to_ascii_lowercase().contains(&needle)
                || r.source_uri.to_ascii_lowercase().contains(&needle)
        })
        .take(500)
        .map(|r| {
            serde_json::json!({
                "id": r.id,
                "name": r.name,
                "origin": r.origin,
                "sourceUri": r.source_uri,
                "version": r.version,
                "contentHash": r.content_hash,
                "trusted": r.trusted,
                "invocation": r.invocation,
                "requiredCapabilities": r.required_capabilities,
                "updatedMs": r.updated_ms,
                "installed": true,
            })
        })
        .collect();
    if let Some(session) = q.session.as_deref() {
        let handle = match resolve_handle(&state, session) {
            Ok(h) => h,
            Err(r) => return r,
        };
        if let Ok(Some(root)) = state.deps.session.resolve_workspace_root(handle.id()) {
            let registry = SkillRegistry::discover(&root);
            let installed_names: BTreeSet<&str> = records.iter().map(|r| r.name.as_str()).collect();
            let candidates: Vec<&faktor_instructions::SkillMeta> = registry
                .entries()
                .iter()
                .filter(|e| !installed_names.contains(e.name.as_str()))
                .filter(|e| needle.is_empty() || e.name.to_ascii_lowercase().contains(&needle))
                .collect();
            discovered_total = candidates.len() as u64;
            discovered_truncated = candidates.len() > discovered_limit;
            let discovered: Vec<serde_json::Value> = candidates
                .into_iter()
                .take(discovered_limit)
                .map(|e| {
                    serde_json::json!({
                        "name": e.name,
                        "description": e.description,
                        "path": e.path,
                        "summary": e.summary,
                        "keywords": e.keywords,
                        "installed": false,
                    })
                })
                .collect();
            if !discovered.is_empty() {
                installed.extend(discovered);
            }
        }
    }
    Json(serde_json::json!({
        "skills": installed,
        "discoveredTotal": discovered_total,
        "hasMore": discovered_truncated,
    }))
    .into_response()
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeSkillInstallBody {
    session: String,
    name: String,
    #[serde(default)]
    trust: bool,
    #[serde(default)]
    invocation: Option<String>,
}

/// `POST /native/skills/install` — read the discovered body through the
/// rooted authority and persist a versioned, initially-untrusted record.
pub(crate) async fn native_skill_install(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<NativeSkillInstallBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid skill install body")),
    };
    let handle = match resolve_handle(&state, &req.session) {
        Ok(h) => h,
        Err(r) => return r,
    };
    let root = match state.deps.session.resolve_workspace_root(handle.id()) {
        Ok(Some(root)) => root,
        Ok(None) => return wire_status(not_found("session workspace")),
        Err(e) => return api_err(&e),
    };
    let registry = SkillRegistry::discover(&root);
    let Some(meta) = registry.entries().iter().find(|e| e.name == req.name) else {
        return wire_status(not_found("discovered skill"));
    };
    let Some(text) = registry.load_skill(&req.name) else {
        return conflict(
            "skill_unreadable",
            "the skill body could not be read through the workspace authority",
        );
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let draft = SkillDraft {
        name: meta.name.clone(),
        origin: Some(ORIGIN_PROJECT.into()),
        source_uri: meta.path.clone(),
        text,
        trusted: Some(false),
        invocation: req.invocation.or_else(|| Some("manual".into())),
        required_capabilities: Vec::new(),
    };
    match foundation.customization.save_skill(&draft, None) {
        Ok(mut record) => {
            if req.trust {
                record = match foundation.customization.trust_skill(&record.id, true) {
                    Ok(record) => record,
                    Err(e) => return foundation_error(e),
                };
            }
            Json(record).into_response()
        }
        Err(e) => foundation_error(e),
    }
}

/// `GET /native/skills/{id}` — the full manifest + body (bounded).
pub(crate) async fn native_skill_get(
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
    match foundation.customization.skill(&id) {
        Ok(Some(record)) => Json(record).into_response(),
        Ok(None) => wire_status(not_found("skill")),
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeSkillTrustBody {
    trusted: bool,
}

/// `POST /native/skills/{id}/trust`
pub(crate) async fn native_skill_trust(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<NativeSkillTrustBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid skill trust body")),
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation.customization.trust_skill(&id, req.trusted) {
        Ok(record) => Json(record).into_response(),
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeSkillInvocationBody {
    invocation: String,
}

/// `POST /native/skills/{id}/invocation`
pub(crate) async fn native_skill_invocation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<NativeSkillInvocationBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid skill invocation body")),
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation
        .customization
        .set_skill_invocation(&id, &req.invocation)
    {
        Ok(record) => Json(record).into_response(),
        Err(e) => foundation_error(e),
    }
}

/// `DELETE /native/skills/{id}`
pub(crate) async fn native_skill_delete(
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
    match foundation.customization.delete_skill(&id) {
        Ok(()) => Json(serde_json::json!({ "deleted": true })).into_response(),
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeSkillInvokeBody {
    #[serde(default)]
    agent_selected: bool,
    #[serde(default)]
    variables: BTreeMap<String, String>,
}

/// `POST /native/skills/{id}/invoke` — return the resolved instruction draft
/// and declared capabilities. Execution still goes through the normal task
/// admission and permission policy; this endpoint grants nothing.
pub(crate) async fn native_skill_invoke(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<NativeSkillInvokeBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid skill invoke body")),
    };
    if req.variables.len() > 32 {
        return wire_status(malformed_body("at most 32 template variables"));
    }
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let record = match foundation.customization.skill(&id) {
        Ok(Some(record)) => record,
        Ok(None) => return wire_status(not_found("skill")),
        Err(e) => return foundation_error(e),
    };
    if !record.trusted {
        return conflict(
            "skill_untrusted",
            "review the skill source and trust it before invocation",
        );
    }
    if record.invocation == "disabled" || (req.agent_selected && record.invocation == "manual") {
        return conflict(
            "skill_policy",
            "skill invocation policy refuses this caller",
        );
    }
    let mut text = record.text.clone();
    let mut unresolved = Vec::new();
    for (key, value) in &req.variables {
        if key.is_empty()
            || key.len() > 64
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return wire_status(malformed_body("invalid template variable name"));
        }
        if value.len() > 4096 {
            return wire_status(malformed_body("template variable exceeds 4096 bytes"));
        }
        let needle = format!("{{{{{key}}}}}");
        if !text.contains(&needle) {
            unresolved.push(key.clone());
        }
        text = text.replace(&needle, value);
    }
    Json(serde_json::json!({
        "id": record.id,
        "name": record.name,
        "contentHash": record.content_hash,
        "version": record.version,
        "trusted": record.trusted,
        "invocation": record.invocation,
        "requiredCapabilities": record.required_capabilities,
        "prompt": text,
        "unresolvedVariables": unresolved,
        "grants": [],
        "note": "invoking returns instruction text only; tool/file/network authority is decided at task admission by the effective permission policy",
    }))
    .into_response()
}

// ----------------------------------------------------------------- hooks

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeHookSaveBody {
    #[serde(flatten)]
    draft: HookDraft,
    #[serde(default, rename = "expectedRevision")]
    expected_revision: Option<u64>,
}

fn hook_registry(state: &AppState) -> Result<&Arc<HookRegistry>, Response> {
    state.deps.agent.deps().hooks.as_ref().ok_or_else(|| {
        unavailable(
            "no lifecycle-hook registry is wired in this daemon generation; configure a hook registry before managing hooks",
        )
    })
}

fn parse_hook_spec(value: &serde_json::Value) -> Result<HookSpec, Response> {
    let spec: HookSpec = serde_json::from_value(value.clone())
        .map_err(|e| wire_status(malformed_body(&format!("invalid hook spec: {e}"))))?;
    if spec.command.is_empty() || spec.command.len() > 4096 {
        return Err(wire_status(malformed_body(
            "hook command must be 1..=4096 bytes",
        )));
    }
    if spec.deadline_ms == 0 || spec.deadline_ms > 300_000 {
        return Err(wire_status(malformed_body(
            "hook deadline must be in (0, 300000] ms",
        )));
    }
    Ok(spec)
}

/// `GET /native/hooks` — durable specs + live registration state.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeHookSearchQuery {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    limit: Option<u64>,
}

pub(crate) async fn native_hooks(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<NativeHookSearchQuery>, axum::extract::rejection::QueryRejection>,
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
    let records = match foundation.customization.hooks() {
        Ok(records) => records,
        Err(e) => return foundation_error(e),
    };
    let registry = state.deps.agent.deps().hooks.as_ref();
    let events: Vec<&str> = HookEvent::ALL.iter().map(|e| hook_event_name(*e)).collect();
    let needle = q.q.as_deref().unwrap_or("").to_ascii_lowercase();
    let limit = q.limit.unwrap_or(500).clamp(1, 500) as usize;
    Json(serde_json::json!({
        "hooks": records.iter().filter(|r| {
            needle.is_empty()
                || r.id.to_ascii_lowercase().contains(&needle)
                || serde_json::to_string(&r.spec)
                    .map(|e| e.to_ascii_lowercase().contains(&needle))
                    .unwrap_or(false)
        }).take(limit).map(|r| serde_json::json!({
            "record": r,
            "registered": registry.map(|g| g.is_registered(&r.id)).unwrap_or(false),
        })).collect::<Vec<_>>(),
        "events": events,
        "registryWired": registry.is_some(),
    }))
    .into_response()
}

fn hook_event_name(event: HookEvent) -> &'static str {
    match event {
        HookEvent::SessionStart => "session_start",
        HookEvent::SessionResume => "session_resume",
        HookEvent::TaskStart => "task_start",
        HookEvent::PreModel => "pre_model",
        HookEvent::PostModel => "post_model",
        HookEvent::PreTool => "pre_tool",
        HookEvent::PostTool => "post_tool",
        HookEvent::ToolError => "tool_error",
        HookEvent::PreEdit => "pre_edit",
        HookEvent::PostEdit => "post_edit",
        HookEvent::PreCommit => "pre_commit",
        HookEvent::SubagentStart => "subagent_start",
        HookEvent::SubagentStop => "subagent_stop",
        HookEvent::AgentError => "agent_error",
        HookEvent::AgentStop => "agent_stop",
        HookEvent::TaskComplete => "task_complete",
        HookEvent::SessionEnd => "session_end",
    }
}

/// `POST /native/hooks` — persist a spec and (when enabled) register it in
/// the live registry. Registration failure leaves the durable spec disabled.
pub(crate) async fn native_hook_save(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<NativeHookSaveBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid hook body")),
    };
    let spec = match parse_hook_spec(&req.draft.spec) {
        Ok(spec) => spec,
        Err(r) => return r,
    };
    if spec.id != req.draft.id {
        return wire_status(malformed_body("hook spec id must equal the draft id"));
    }
    if req.draft.enabled.unwrap_or(false) && req.draft.trusted == Some(false) {
        return conflict("hook_untrusted", "an untrusted hook cannot be enabled");
    }
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    // Persist disabled first; enable only after a successful registration.
    let mut disabled = req.draft.clone();
    disabled.enabled = Some(false);
    let saved = match foundation
        .customization
        .save_hook(&disabled, req.expected_revision)
    {
        Ok(saved) => saved,
        Err(e) => return foundation_error(e),
    };
    if !req.draft.enabled.unwrap_or(false) {
        if let Some(registry) = state.deps.agent.deps().hooks.as_ref() {
            let _ = registry.unregister(&saved.id);
        }
        return Json(serde_json::json!({
            "record": saved,
            "registered": false,
        }))
        .into_response();
    }
    let registry = match hook_registry(&state) {
        Ok(registry) => registry,
        Err(r) => return r,
    };
    let _ = registry.unregister(&saved.id);
    if let Err(message) = registry.register(spec) {
        return conflict("hook_register_failed", message);
    }
    let enabled = match foundation.customization.save_hook(
        &HookDraft {
            id: saved.id.clone(),
            spec: saved.spec.clone(),
            trusted: Some(saved.trusted),
            enabled: Some(true),
        },
        Some(saved.revision),
    ) {
        Ok(record) => record,
        Err(e) => {
            let _ = registry.unregister(&saved.id);
            return foundation_error(e);
        }
    };
    Json(serde_json::json!({
        "record": enabled,
        "registered": true,
    }))
    .into_response()
}

/// `DELETE /native/hooks/{id}` — unregister first, then remove the record.
pub(crate) async fn native_hook_delete(
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
    let unregistered = match state.deps.agent.deps().hooks.as_ref() {
        Some(registry) => registry.unregister(&id).unwrap_or(false),
        None => false,
    };
    match foundation.customization.delete_hook(&id) {
        Ok(()) => Json(serde_json::json!({
            "deleted": true,
            "unregistered": unregistered,
        }))
        .into_response(),
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeHookTestBody {
    event: String,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    payload: serde_json::Value,
}

/// `POST /native/hooks/{id}/test` — dry-run INPUT through the exact
/// `run_one` path (deadline/caps/failure policy/audit), not a simulation.
pub(crate) async fn native_hook_test(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<NativeHookTestBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid hook test body")),
    };
    let event: HookEvent =
        match serde_json::from_value(serde_json::Value::String(req.event.clone())) {
            Ok(event) => event,
            Err(_) => {
                return wire_status(malformed_body(
                    "event must be one of the 17 snake_case hook events",
                ))
            }
        };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let record = match foundation.customization.hook(&id) {
        Ok(Some(record)) => record,
        Ok(None) => return wire_status(not_found("hook")),
        Err(e) => return foundation_error(e),
    };
    if !record.trusted {
        return conflict(
            "hook_untrusted",
            "review and trust the hook before testing it",
        );
    }
    let spec = match parse_hook_spec(&record.spec) {
        Ok(spec) => spec,
        Err(r) => return r,
    };
    let registry = match hook_registry(&state) {
        Ok(registry) => registry,
        Err(r) => return r,
    };
    let (session_id, workspace_root) = match req.session.as_deref() {
        Some(session) => {
            let handle = match resolve_handle(&state, session) {
                Ok(h) => h,
                Err(r) => return r,
            };
            let root = state
                .deps
                .session
                .resolve_workspace_root(handle.id())
                .ok()
                .flatten();
            (Some(handle.id().to_string()), root)
        }
        None => (
            None,
            state.deps.directory.as_ref().map(std::path::PathBuf::from),
        ),
    };
    let input = HookInput {
        event,
        session_id,
        task_id: None,
        operation_id: Some(format!("hook-test-{id}")),
        workspace_root,
        payload: req.payload,
    };
    let verdict = registry.run_one(&spec, &input);
    Json(serde_json::json!({
        "hookId": id,
        "event": req.event,
        "verdict": verdict,
        "audit": registry.audit().last().cloned(),
    }))
    .into_response()
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeHookAuditQuery {
    #[serde(default)]
    limit: Option<u64>,
}

/// `GET /native/hooks/audit?limit=` — the live registry's bounded ring.
pub(crate) async fn native_hook_audit(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<NativeHookAuditQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(q) => q,
        Err(_) => return wire_status(malformed_body("invalid query parameters")),
    };
    let registry = match hook_registry(&state) {
        Ok(registry) => registry,
        Err(r) => return r,
    };
    let limit = q.limit.unwrap_or(50).clamp(1, 200) as usize;
    let mut audit = registry.audit();
    audit.reverse();
    audit.truncate(limit);
    Json(serde_json::json!({ "records": audit })).into_response()
}

// ----------------------------------------------------------------- tools

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeToolQuery {
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    limit: Option<u64>,
}

/// `GET /native/tools?q=&limit=` — bounded built-in + live MCP catalog with
/// namespaced tool identity.
pub(crate) async fn native_tools(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<NativeToolQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(q) => q,
        Err(_) => return wire_status(malformed_body("invalid query parameters")),
    };
    let limit = q.limit.unwrap_or(200).clamp(1, 500) as usize;
    let needle = q.q.as_deref().unwrap_or("").to_ascii_lowercase();
    let registry = &state.deps.agent.deps().tools;
    let mut out = Vec::new();
    for name in registry.names() {
        if !needle.is_empty() && !name.to_ascii_lowercase().contains(&needle) {
            continue;
        }
        if let Some(tool) = registry.get(&name) {
            out.push(serde_json::json!({
                "name": name,
                "namespace": "builtin",
                "description": tool.description,
                "inputSchema": tool.input_schema,
                "source": "builtin",
                "health": "ready",
            }));
        }
        if out.len() >= limit {
            break;
        }
    }
    if out.len() < limit {
        if let Some(foundation) = state.deps.foundation.as_deref() {
            if let Ok(records) = foundation.customization.mcp_servers() {
                for record in records {
                    let Some(tools) = state
                        .deps
                        .mcp_runtime
                        .tools(&record.id, record.output_cap_bytes)
                        .await
                        .unwrap_or_default()
                    else {
                        continue;
                    };
                    for tool in tools {
                        let Some(name) = tool.get("name").and_then(|v| v.as_str()) else {
                            continue;
                        };
                        let namespaced = format!("mcp:{}:{name}", record.name);
                        if !needle.is_empty() && !namespaced.to_ascii_lowercase().contains(&needle)
                        {
                            continue;
                        }
                        out.push(serde_json::json!({
                            "name": namespaced,
                            "namespace": format!("mcp:{}", record.name),
                            "description": tool.get("description").cloned().unwrap_or_default(),
                            "inputSchema": tool.get("inputSchema").cloned().unwrap_or_default(),
                            "source": "mcp",
                            "health": "ready",
                        }));
                        if out.len() >= limit {
                            break;
                        }
                    }
                    if out.len() >= limit {
                        break;
                    }
                }
            }
        }
    }
    Json(serde_json::json!({ "tools": out, "limit": limit })).into_response()
}

// --------------------------------------------------------------- browser

/// `GET /native/browser/status` — honest adapter availability and configured
/// capability. No browser authority is launched by listing.
pub(crate) async fn native_browser_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let enabled = state
        .deps
        .foundation
        .as_deref()
        .and_then(|f| f.config.value("faktor.browser.enabled").ok().flatten())
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    Json(serde_json::json!({
        "enabled": enabled,
        "available": false,
        "adapters": [{
            "id": "chromium",
            "families": ["chromium"],
            "state": if enabled { "unsupported" } else { "disabled" },
            "detail": if enabled {
                "the browser crate is not attached to this daemon's native surface; no launch authority is wired"
            } else {
                "enable faktor.browser.enabled and restart with a browser authority to use the browser panel"
            },
        }],
        "attached": false,
    }))
    .into_response()
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeUrlBody {
    url: String,
}

/// `POST /native/browser/validate-url` — scheme/host boundary preview. A
/// hostname is left `unresolved` (no DNS here); a literal address is
/// classified by the one egress authority.
pub(crate) async fn native_browser_validate_url(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<NativeUrlBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid URL body")),
    };
    let url = match url::Url::parse(&req.url) {
        Ok(url) => url,
        Err(e) => return wire_status(malformed_body(&format!("invalid URL: {e}"))),
    };
    let scheme_ok = matches!(url.scheme(), "http" | "https");
    let host = url.host_str().unwrap_or_default().to_string();
    let looback_allowed = state
        .deps
        .foundation
        .as_deref()
        .and_then(|f| {
            f.config
                .value("faktor.sandbox.allowLoopback")
                .ok()
                .flatten()
        })
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let (class, decision, reason) = if !scheme_ok {
        (
            "unsupported".to_string(),
            "deny",
            "only http/https destinations are supported".to_string(),
        )
    } else if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        let class = faktor_security::network::classify_ip(ip);
        let (decision, reason) = match class {
            faktor_security::network::AddressClass::Global => {
                ("allow", "globally routable destination")
            }
            faktor_security::network::AddressClass::Loopback => {
                if looback_allowed {
                    ("allow", "loopback explicitly enabled")
                } else {
                    ("deny", "loopback requires an explicit opt-in")
                }
            }
            _ => (
                "deny",
                "the destination class is not permitted by the network boundary",
            ),
        };
        (
            format!("{class:?}").to_ascii_lowercase(),
            decision,
            reason.to_string(),
        )
    } else {
        (
            "unresolved".to_string(),
            "resolve",
            "hostname not resolved here; the egress broker resolves and classifies every answer at connect time"
                .to_string(),
        )
    };
    Json(serde_json::json!({
        "url": req.url,
        "scheme": url.scheme(),
        "host": host,
        "class": class,
        "decision": decision,
        "reason": reason,
        "port": url.port_or_known_default(),
    }))
    .into_response()
}

// ---------------------------------------------------------- session ops

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeForkBody {
    #[serde(default)]
    title: Option<String>,
}

/// `POST /native/session/{id}/fork` — durable new branch lineage over the
/// source's bounded history. The fork starts at the source head.
pub(crate) async fn native_session_fork(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<NativeForkBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let req = match body {
        Ok(Json(body)) => body,
        Err(_) => NativeForkBody { title: None },
    };
    let sid = match parse_session_id(&id) {
        Ok(s) => s,
        Err(e) => return wire_status(e),
    };
    let source = match state.deps.session.get_session(sid) {
        Ok(Some(handle)) => handle,
        Ok(None) => return wire_status(not_found("session")),
        Err(e) => return api_err(&e),
    };
    let title = req
        .title
        .filter(|t| !t.trim().is_empty() && t.len() <= 256)
        .unwrap_or_else(|| {
            format!(
                "{} (fork)",
                source.row().map(|r| r.title).unwrap_or_default()
            )
        });
    match state.deps.session.fork_session(sid, &title) {
        Ok(fork) => Json(serde_json::json!({
            "sessionId": fork.id().to_string(),
            "title": title,
            "forkedFrom": sid.to_string(),
            "forkedAt": "head",
        }))
        .into_response(),
        Err(e) => api_err(&e),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeRestoreBody {
    #[serde(default, rename = "direction")]
    direction: Option<String>,
    #[serde(default, rename = "expectedCurrentHash")]
    expected_current_hash: Option<String>,
}

/// `POST /native/session/{id}/checkpoints/{sequence}/restore` — CAS-checked
/// content restore (undo) or redo. A concurrent manual edit is a typed
/// conflict, never overwritten.
pub(crate) async fn native_checkpoint_restore(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, sequence)): Path<(String, i64)>,
    body: Result<Json<NativeRestoreBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    use faktor_core::WorkspaceIdentity;

    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let req = match body {
        Ok(Json(body)) => body,
        Err(_) => NativeRestoreBody {
            direction: None,
            expected_current_hash: None,
        },
    };
    let handle = match resolve_handle(&state, &id) {
        Ok(h) => h,
        Err(r) => return r,
    };
    let Some(snapshots) = state.deps.snapshots.clone() else {
        return unavailable("the native snapshot authority is not wired in this host");
    };
    let Some(fs) = state.deps.fs.clone() else {
        return unavailable("the workspace file authority is not wired in this host");
    };
    let row = match handle.row() {
        Ok(row) => row,
        Err(e) => return api_err(&e),
    };
    let root = match state.deps.session.resolve_workspace_root(handle.id()) {
        Ok(Some(root)) => root,
        Ok(None) => return wire_status(not_found("session workspace")),
        Err(e) => return api_err(&e),
    };
    let workspace = match fs.open(row.workspace_id, root) {
        Ok(workspace) => workspace,
        Err(e) => return api_err(&e),
    };
    let identity = WorkspaceIdentity::new(row.workspace_id, row.worktree_id, row.task_id);
    let checkpoint = match handle.checkpoints_of() {
        Ok(rows) => rows.into_iter().find(|c| c.sequence == sequence),
        Err(e) => return api_err(&e),
    };
    let Some(checkpoint) = checkpoint else {
        return wire_status(not_found("checkpoint"));
    };
    if let Some(expected) = &req.expected_current_hash {
        let Some(expected_hash) = faktor_core::hash::FileHash::from_hex(expected) else {
            return wire_status(malformed_body(
                "expectedCurrentHash must be a 64-character hex digest",
            ));
        };
        let rel = std::path::Path::new(&checkpoint.path);
        let current = match workspace.hash_file_streaming(rel, None) {
            Ok((_, hash)) => Some(hash),
            Err(_) => None,
        };
        if current != Some(expected_hash) {
            return conflict(
                "checkpoint_conflict",
                "the file changed since the checkpoint; preview and reconcile before restoring",
            );
        }
    }
    let direction = req.direction.as_deref().unwrap_or("undo");
    let outcome = match direction {
        "undo" => snapshots.rollback(&workspace, &identity, handle.id(), checkpoint.id),
        "redo" => snapshots.redo(&workspace, &identity, handle.id(), checkpoint.id),
        _ => return wire_status(malformed_body("direction must be `undo` or `redo`")),
    };
    match outcome {
        Ok(faktor_snapshot::RollbackOutcome::Restored { path, hash }) => Json(serde_json::json!({
            "restored": true,
            "path": path,
            "hash": hash.map(|h| h.to_hex()),
            "direction": direction,
            "checkpointId": checkpoint.id,
            "sequence": sequence,
        }))
        .into_response(),
        Ok(faktor_snapshot::RollbackOutcome::Conflict {
            path,
            current,
            expected_after,
        }) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "code": "checkpoint_conflict",
                "message": "the file changed since the checkpoint; nothing was restored",
                "path": path,
                "current": format!("{current:?}"),
                "expected": format!("{expected_after:?}"),
            })),
        )
            .into_response(),
        Err(e) => api_err(&e),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeMergeBody {
    #[serde(rename = "changeSetId")]
    change_set_id: String,
    #[serde(default)]
    approved: Vec<String>,
    #[serde(default)]
    rejected: Vec<String>,
}

fn agent_paths(paths: &[String]) -> Result<Vec<std::path::PathBuf>, Response> {
    if paths.len() > 1_000 {
        return Err(wire_status(malformed_body(
            "at most 1000 paths per decision",
        )));
    }
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        if path.is_empty() || path.len() > 4096 || path.contains('\u{0}') {
            return Err(wire_status(malformed_body("invalid path in decision")));
        }
        let p = std::path::Path::new(path);
        if p.is_absolute() || path.split('/').any(|part| part == "..") {
            return Err(wire_status(malformed_body(
                "decision paths must be workspace-relative without `..`",
            )));
        }
        out.push(p.to_path_buf());
    }
    Ok(out)
}

/// `POST /native/agents/{child_id}/stage` — stage the child's change set.
pub(crate) async fn native_agent_stage(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(child_id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    match state.deps.orchestrator.stage_child_changes(&child_id) {
        Ok(change_set) => Json(serde_json::json!({
            "changeSetId": change_set.id(),
            "changeSet": change_set,
        }))
        .into_response(),
        Err(e) => exec_error_response(&e),
    }
}

/// `POST /native/agents/{child_id}/merge` — per-file partial acceptance:
/// exactly the approved paths land; rejected paths are preserved in the
/// child's overlay and reported.
pub(crate) async fn native_agent_merge(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(child_id): Path<String>,
    body: Result<Json<NativeMergeBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid merge body")),
    };
    if req.change_set_id.is_empty() || req.change_set_id.len() > 256 {
        return wire_status(malformed_body("changeSetId must be 1..=256 bytes"));
    }
    let approved = match agent_paths(&req.approved) {
        Ok(paths) => paths,
        Err(r) => return r,
    };
    let rejected = match agent_paths(&req.rejected) {
        Ok(paths) => paths,
        Err(r) => return r,
    };
    match state
        .deps
        .orchestrator
        .approve_and_merge(&child_id, &req.change_set_id, &approved, &rejected)
        .await
    {
        Ok(outcome) => Json(serde_json::json!({
            "merged": outcome.merged,
            "rejected": outcome.rejected,
            "conflicts": outcome.conflicts,
        }))
        .into_response(),
        Err(e) => exec_error_response(&e),
    }
}

// keep imports used
#[allow(dead_code)]
fn _unused(_: &FoundationError, _: Duration) {}

// ------------------------------------------------------- review comments

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct NativeReviewQuery {
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    limit: Option<u64>,
}

fn latest_change_hash(handle: &faktor_session::SessionHandle, path: &str) -> Option<String> {
    let rows = handle.checkpoints_of().ok()?;
    rows.into_iter()
        .filter(|row| row.path == path)
        .max_by_key(|row| row.sequence)
        .map(|row| row.after_hash)
}

fn task_belongs_to_session(
    state: &AppState,
    handle: &faktor_session::SessionHandle,
    task: &str,
) -> Result<bool, Response> {
    let raw: u64 = task
        .parse()
        .map_err(|_| wire_status(malformed_body("task id must be a positive integer")))?;
    let task_id = faktor_core::id::TaskId::try_from(raw)
        .map_err(|_| wire_status(malformed_body("task id must be a positive integer")))?;
    state
        .deps
        .session
        .store()
        .get_task(handle.id(), task_id)
        .map(|row| row.is_some())
        .map_err(|e| api_err(&store_err_to_core(e)))
}

/// `GET /native/session/{id}/review/comments?task=&path=&limit=` — durable
/// comments with their current-vs-anchored change digest (stale is explicit,
/// never silently rebased).
pub(crate) async fn native_review_comments_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    query: Result<Query<NativeReviewQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(q) => q,
        Err(_) => return wire_status(malformed_body("invalid query parameters")),
    };
    let handle = match resolve_handle(&state, &id) {
        Ok(handle) => handle,
        Err(r) => return r,
    };
    if let Some(task) = q.task.as_deref() {
        match task_belongs_to_session(&state, &handle, task) {
            Ok(true) => {}
            Ok(false) => return wire_status(not_found("task of this session")),
            Err(r) => return r,
        }
    }
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation.review.list(
        handle.id().raw(),
        q.task.as_deref(),
        q.path.as_deref(),
        q.limit.unwrap_or(200).clamp(1, 500) as usize,
    ) {
        Ok(comments) => {
            let rows: Vec<serde_json::Value> = comments
                .into_iter()
                .map(|comment| {
                    let current = latest_change_hash(&handle, &comment.path);
                    let stale = comment
                        .content_hash
                        .as_deref()
                        .is_some_and(|anchored| current.as_deref() != Some(anchored));
                    serde_json::json!({
                        "comment": comment,
                        "currentChangeHash": current,
                        "stale": stale,
                    })
                })
                .collect();
            Json(serde_json::json!({ "sessionId": id, "comments": rows })).into_response()
        }
        Err(e) => foundation_error(e),
    }
}

/// `POST /native/session/{id}/review/comments` — create one durable comment.
pub(crate) async fn native_review_comment_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<ReviewCommentDraft>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(draft) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid review comment body")),
    };
    let handle = match resolve_handle(&state, &id) {
        Ok(handle) => handle,
        Err(r) => return r,
    };
    if let Some(task) = draft.task_id.as_deref() {
        match task_belongs_to_session(&state, &handle, task) {
            Ok(true) => {}
            Ok(false) => return wire_status(not_found("task of this session")),
            Err(r) => return r,
        }
    }
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation.review.create(handle.id().raw(), &draft) {
        Ok(comment) => {
            let current = latest_change_hash(&handle, &comment.path);
            let stale = comment
                .content_hash
                .as_deref()
                .is_some_and(|anchored| current.as_deref() != Some(anchored));
            Json(serde_json::json!({
                "comment": comment,
                "currentChangeHash": current,
                "stale": stale,
            }))
            .into_response()
        }
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct NativeReviewUpdateBody {
    #[serde(flatten)]
    patch: ReviewCommentPatch,
    #[serde(default)]
    expected_revision: Option<u64>,
}

/// `PATCH /native/session/{id}/review/comments/{comment}` — edit/resolve
/// under an optional revision fence.
pub(crate) async fn native_review_comment_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, comment)): Path<(String, String)>,
    body: Result<Json<NativeReviewUpdateBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid review comment patch")),
    };
    let handle = match resolve_handle(&state, &id) {
        Ok(handle) => handle,
        Err(r) => return r,
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation.review.update(
        handle.id().raw(),
        &comment,
        body.expected_revision,
        &body.patch,
    ) {
        Ok(comment) => Json(comment).into_response(),
        Err(e) => foundation_error(e),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct NativeReviewDeleteQuery {
    #[serde(default)]
    expected_revision: Option<u64>,
}

/// `DELETE /native/session/{id}/review/comments/{comment}`.
pub(crate) async fn native_review_comment_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, comment)): Path<(String, String)>,
    query: Result<Query<NativeReviewDeleteQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(q) => q,
        Err(_) => return wire_status(malformed_body("invalid query parameters")),
    };
    let handle = match resolve_handle(&state, &id) {
        Ok(handle) => handle,
        Err(r) => return r,
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation
        .review
        .delete(handle.id().raw(), &comment, q.expected_revision)
    {
        Ok(()) => Json(serde_json::json!({ "deleted": true, "comment": comment })).into_response(),
        Err(e) => foundation_error(e),
    }
}

// ------------------------------------------------------- change acceptance

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct NativeChangeAcceptBody {
    #[serde(default)]
    approved: Vec<String>,
    #[serde(default)]
    rejected: Vec<String>,
}

fn change_receipt_digest(body: &NativeChangeAcceptBody, change_set_id: &str) -> String {
    let canonical = serde_json::json!({
        "changeSetId": change_set_id,
        "approved": body.approved,
        "rejected": body.rejected,
    });
    faktor_foundation::hash_hex(canonical.to_string().as_bytes())
}

/// `POST /native/session/{id}/changes/accept` — per-file/selected-change
/// acceptance through the shadow CAS merge authority. Every changed file
/// must be decided; rejected paths are never written; a replay of the same
/// decision returns the SAME durable receipt (a different decision for the
/// same change set is a typed conflict).
pub(crate) async fn native_change_accept(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<NativeChangeAcceptBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid change acceptance body")),
    };
    let handle = match resolve_handle(&state, &id) {
        Ok(handle) => handle,
        Err(r) => return r,
    };
    let Some(shadows) = state.deps.tasks.shadows() else {
        return unavailable("the shadow change-set authority is not wired in this host");
    };
    let change_set = match shadows.present_change_set(handle.id()) {
        Ok(change_set) => change_set,
        Err(e) => return super::agents::exec_error_response(&e),
    };
    let change_set_id = format!("{}-cs", change_set.base_id);
    let digest = change_receipt_digest(&body, &change_set_id);
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation
        .session_settings
        .change_receipt(handle.id().raw(), &change_set_id)
    {
        Ok(Some((stored_digest, receipt))) => {
            if stored_digest != digest {
                return conflict(
                    "change_accept_conflict",
                    "this change set was already decided with a different file selection; nothing was applied",
                );
            }
            return Json(receipt).into_response();
        }
        Ok(None) => {}
        Err(e) => return foundation_error(e),
    }
    let approved: Vec<std::path::PathBuf> =
        body.approved.iter().map(std::path::PathBuf::from).collect();
    let rejected: Vec<std::path::PathBuf> =
        body.rejected.iter().map(std::path::PathBuf::from).collect();
    let outcome = match shadows.accept_change_set(handle.id(), &approved, &rejected) {
        Ok(outcome) => outcome,
        Err(e) => return super::agents::exec_error_response(&e),
    };
    let receipt = serde_json::json!({
        "accepted": true,
        "changeSetId": outcome.change_set_id,
        "applied": outcome.applied,
        "alreadyCurrent": outcome.already_current,
        "rejected": outcome.rejected,
        "integrated": outcome.integrated,
        "idempotent": true,
        "proof": {
            "note": "partial acceptance is a NEW candidate state; prior full-candidate verification does not certify this subset",
            "requiresReverification": true,
        },
    });
    if let Err(e) = foundation.session_settings.put_change_receipt(
        handle.id().raw(),
        &outcome.change_set_id,
        &digest,
        &receipt,
    ) {
        return foundation_error(e);
    }
    Json(receipt).into_response()
}

// ---------------------------------------------------------- restore preview

/// `POST /native/session/{id}/checkpoints/{sequence}/restore/preview` —
/// dry-run restore (V-05). NOTHING is written; the response is the exact
/// plan: current/expected/restored states, content digests, per-file action
/// and whether the transaction can proceed. The subsequent restore enforces
/// the same CAS preconditions.
pub(crate) async fn native_checkpoint_restore_preview(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, sequence)): Path<(String, i64)>,
    body: Result<Json<NativeRestoreBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    use faktor_core::WorkspaceIdentity;

    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let req = match body {
        Ok(Json(body)) => body,
        Err(_) => NativeRestoreBody {
            direction: None,
            expected_current_hash: None,
        },
    };
    let direction = req.direction.as_deref().unwrap_or("undo");
    if direction != "undo" && direction != "redo" {
        return wire_status(malformed_body("direction must be `undo` or `redo`"));
    }
    let handle = match resolve_handle(&state, &id) {
        Ok(h) => h,
        Err(r) => return r,
    };
    let Some(snapshots) = state.deps.snapshots.clone() else {
        return unavailable("the native snapshot authority is not wired in this host");
    };
    let Some(fs) = state.deps.fs.clone() else {
        return unavailable("the workspace file authority is not wired in this host");
    };
    let row = match handle.row() {
        Ok(row) => row,
        Err(e) => return api_err(&e),
    };
    let root = match state.deps.session.resolve_workspace_root(handle.id()) {
        Ok(Some(root)) => root,
        Ok(None) => return wire_status(not_found("session workspace")),
        Err(e) => return api_err(&e),
    };
    let workspace = match fs.open(row.workspace_id, root) {
        Ok(workspace) => workspace,
        Err(e) => return api_err(&e),
    };
    let identity = WorkspaceIdentity::new(row.workspace_id, row.worktree_id, row.task_id);
    let checkpoint = match handle.checkpoints_of() {
        Ok(rows) => rows.into_iter().find(|c| c.sequence == sequence),
        Err(e) => return api_err(&e),
    };
    let Some(checkpoint) = checkpoint else {
        return wire_status(not_found("checkpoint"));
    };
    let preview = match snapshots.preview_restore(
        &workspace,
        &identity,
        handle.id(),
        checkpoint.id,
        direction,
    ) {
        Ok(preview) => preview,
        Err(e) => return api_err(&e),
    };
    let files: Vec<serde_json::Value> = preview
        .files
        .iter()
        .map(|file| {
            serde_json::json!({
                "path": file.path,
                "action": file.action.as_str(),
                "current": { "exists": file.current.exists, "hash": file.current.hash.map(|h| h.to_hex()) },
                "expected": { "exists": file.expected.exists, "hash": file.expected.hash.map(|h| h.to_hex()) },
                "restore": { "exists": file.restore.exists, "hash": file.restore.hash.map(|h| h.to_hex()) },
                "contentDigest": file.content_digest,
            })
        })
        .collect();
    Json(serde_json::json!({
        "dryRun": true,
        "writes": 0,
        "sessionId": preview.session_id.raw(),
        "checkpointId": preview.checkpoint_id,
        "sequence": preview.sequence,
        "direction": preview.direction,
        "files": files,
        "writable": preview.writable,
        "conflictCount": preview.files.iter().filter(|f| f.action.as_str() == "conflict").count(),
    }))
    .into_response()
}

// ------------------------------------------------ session delete / export

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct NativeSessionDeleteQuery {
    #[serde(default)]
    abort: Option<bool>,
    #[serde(default)]
    reason: Option<String>,
}

/// `DELETE /native/session/{id}?abort=&reason=` — durable, owner-scoped
/// deletion (D-06). A live turn is REFUSED unless `abort=true`; on abort the
/// turn is cancelled first, then the session is Closed and its content is
/// purged transactionally. The session row and the `session_deletion`
/// tombstone remain. Idempotent once deleted.
pub(crate) async fn native_session_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    query: Result<Query<NativeSessionDeleteQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(q) => q,
        Err(_) => return wire_status(malformed_body("invalid query parameters")),
    };
    let reason = q.reason.as_deref().unwrap_or("user_requested");
    if reason.is_empty() || reason.len() > 512 || reason.chars().any(char::is_control) {
        return wire_status(malformed_body("reason must be 1..=512 non-control bytes"));
    }
    let session_id = match parse_session_id(&id) {
        Ok(session_id) => session_id,
        Err(e) => return wire_status(e),
    };
    let store = state.deps.session.store();
    // The tombstone check precedes session OPEN: after a purge the event
    // journal is intentionally gone, so opening the handle would (correctly)
    // fail verification. An already-deleted session answers idempotently.
    match store.session_deletion(session_id) {
        Ok(Some(tombstone)) => {
            return Json(serde_json::json!({
                "deleted": true,
                "idempotent": true,
                "sessionId": id,
                "deletedMs": tombstone.deleted_ms,
                "purged": tombstone.purged,
            }))
            .into_response()
        }
        Ok(None) => {}
        Err(e) => return api_err(&store_err_to_core(e)),
    }
    let handle = match resolve_handle(&state, &id) {
        Ok(handle) => handle,
        Err(r) => return r,
    };
    let row = match handle.row() {
        Ok(row) => row,
        Err(e) => return api_err(&e),
    };
    let active = row.state.is_active();
    if active && q.abort != Some(true) {
        return conflict(
            "session_active",
            "the session has a live turn; pass abort=true to stop it before deletion",
        );
    }
    if active {
        if let Err(e) = handle.abort(None) {
            return api_err(&e);
        }
    }
    if !row.lifecycle.is_terminal() {
        if let Err(e) = state.deps.session.delete_session(session_id) {
            return api_err(&e);
        }
    }
    match store.purge_session_content(session_id, reason) {
        Ok(tombstone) => Json(serde_json::json!({
            "deleted": true,
            "idempotent": false,
            "sessionId": id,
            "deletedMs": tombstone.deleted_ms,
            "purged": tombstone.purged,
            "retained": {
                "sessionRow": "closed tombstone",
                "workspace": true,
                "forks": "independent sessions are not deleted",
                "casBlobs": "reference-counted content-addressed storage is not garbage-collected here",
            },
            "audit": "session_deletion",
        }))
        .into_response(),
        Err(e) => api_err(&store_err_to_core(e)),
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct NativeSessionExportBody {
    #[serde(default)]
    redact: Option<bool>,
    #[serde(default)]
    max_bytes: Option<u64>,
}

fn redact_export(value: &mut serde_json::Value, redacted_keys: &mut usize) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                let lower = key.to_ascii_lowercase().replace(['-', '_'], "");
                let sensitive = matches!(
                    lower.as_str(),
                    "authorization"
                        | "proxyauthorization"
                        | "cookie"
                        | "setcookie"
                        | "wwwauthenticate"
                        | "xapikey"
                        | "apikey"
                        | "accesstoken"
                        | "refreshtoken"
                        | "idtoken"
                        | "password"
                        | "secret"
                        | "clientsecret"
                ) || lower.ends_with("token")
                    || lower.ends_with("secret");
                if sensitive {
                    *child = serde_json::Value::String("[redacted]".into());
                    *redacted_keys += 1;
                } else {
                    redact_export(child, redacted_keys);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for child in items {
                redact_export(child, redacted_keys);
            }
        }
        serde_json::Value::String(text) if text.starts_with("Bearer ") && text.len() > 16 => {
            *text = "[redacted]".into();
            *redacted_keys += 1;
        }
        _ => {}
    }
}

/// `POST /native/session/{id}/export` — bounded bundle (metadata,
/// transcript, checkpoint/diff identity, proof) with a content digest and
/// optional recursive redaction of credential-shaped keys/values. Never
/// includes vault material, cookies or auth headers.
pub(crate) async fn native_session_export(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<NativeSessionExportBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let req = match body {
        Ok(Json(body)) => body,
        Err(_) => NativeSessionExportBody {
            redact: None,
            max_bytes: None,
        },
    };
    let max_bytes = req
        .max_bytes
        .unwrap_or(1_048_576)
        .clamp(65_536, 8 * 1024 * 1024);
    let redact = req.redact.unwrap_or(true);
    let session_id = match parse_session_id(&id) {
        Ok(session_id) => session_id,
        Err(e) => return wire_status(e),
    };
    if let Ok(Some(_)) = state.deps.session.store().session_deletion(session_id) {
        return conflict(
            "session_deleted",
            "the session content was deleted; only the closed tombstone remains",
        );
    }
    let handle = match resolve_handle(&state, &id) {
        Ok(handle) => handle,
        Err(r) => return r,
    };
    let row = match handle.row() {
        Ok(row) => row,
        Err(e) => return api_err(&e),
    };
    let store = state.deps.session.store();
    let mut transcript = Vec::new();
    let mut transcript_bytes = 0usize;
    let mut truncated = false;
    let rows = match store.messages_before(handle.id(), None, 1_000) {
        Ok(rows) => rows,
        Err(e) => return api_err(&store_err_to_core(e)),
    };
    let mut ordered = rows;
    ordered.reverse();
    let total_messages = ordered.len();
    for message in &ordered {
        let parts = store.parts_of(message.id).unwrap_or_default();
        let entry = serde_json::json!({
            "seq": message.seq,
            "role": message.role,
            "createdMs": message.created_ms,
            "data": message.data,
            "parts": parts.iter().map(|p| serde_json::json!({
                "kind": p.kind, "data": p.data, "createdMs": p.created_ms,
            })).collect::<Vec<_>>(),
        });
        let size = serde_json::to_vec(&entry).map(|b| b.len()).unwrap_or(0);
        if transcript_bytes + size > (max_bytes as usize) / 2 || transcript.len() >= 500 {
            truncated = true;
            break;
        }
        transcript_bytes += size;
        transcript.push(entry);
    }
    let checkpoints = handle.checkpoints_of().unwrap_or_default();
    let diffs: Vec<serde_json::Value> = checkpoints
        .iter()
        .take(200)
        .map(|c| {
            serde_json::json!({
                "sequence": c.sequence,
                "path": c.path,
                "beforeHash": c.before_hash,
                "afterHash": c.after_hash,
                "createdMs": c.created_ms,
                "restoredMs": c.restored_ms,
            })
        })
        .collect();
    // The latest real diff text when the snapshot authority is wired; the
    // checkpoint list above still carries the exact before/after identity.
    let latest_diff = match (state.deps.snapshots.as_ref(), state.deps.fs.as_ref()) {
        (Some(snapshots), Some(fs)) => state
            .deps
            .session
            .resolve_workspace_root(handle.id())
            .ok()
            .flatten()
            .and_then(|root| fs.open(row.workspace_id, root).ok())
            .and_then(|workspace| {
                let identity = faktor_core::WorkspaceIdentity::new(
                    row.workspace_id,
                    row.worktree_id,
                    row.task_id,
                );
                snapshots
                    .diff_latest(&workspace, &identity, handle.id())
                    .ok()
                    .flatten()
            })
            .map(|diff| {
                let status = diff.status.as_str();
                let mut text = diff.diff;
                let mut truncated = false;
                if text.len() > 65_536 {
                    let mut cut = 65_536;
                    while !text.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    text.truncate(cut);
                    truncated = true;
                }
                serde_json::json!({
                    "path": diff.path,
                    "status": status,
                    "diff": text,
                    "truncated": truncated,
                })
            }),
        _ => None,
    };
    let mut proof = Vec::new();
    if let Ok(tasks) = store.list_tasks(handle.id()) {
        for task in tasks.iter().take(50) {
            if let Ok(records) = store.verification_record_list_by_task(task.task_id) {
                for record in records.iter().take(20) {
                    proof.push(serde_json::json!({
                        "taskId": task.task_id.raw(),
                        "revision": record.revision,
                        "status": record.status,
                    }));
                }
            }
        }
    }
    let mut bundle = serde_json::json!({
        "schema": "faktor.session-export/v1",
        "metadata": {
            "sessionId": handle.id().raw(),
            "title": row.title,
            "provider": row.provider,
            "model": row.model,
            "state": format!("{:?}", row.state),
            "lifecycle": format!("{:?}", row.lifecycle),
            "workspaceId": row.workspace_id.raw(),
            "worktreeId": row.worktree_id.raw(),
            "taskId": row.task_id.raw(),
            "createdMs": row.created_ms,
            "updatedMs": row.updated_ms,
        },
        "transcript": {
            "messages": transcript,
            "totalMessages": total_messages,
            "truncated": truncated,
        },
        "changes": { "checkpoints": diffs, "latestDiff": latest_diff },
        "proof": proof,
        "redacted": redact,
    });
    let mut redacted_keys = 0usize;
    if redact {
        redact_export(&mut bundle, &mut redacted_keys);
    }
    let bytes = match serde_json::to_vec(&bundle) {
        Ok(bytes) => bytes,
        Err(e) => return api_err(&faktor_core::Error::internal(format!("export encode: {e}"))),
    };
    if bytes.len() > max_bytes as usize {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(serde_json::json!({
                "code": "export_too_large",
                "message": format!("the bounded export is {} bytes, over the requested {max_bytes}-byte cap", bytes.len()),
            })),
        )
            .into_response();
    }
    let digest = faktor_foundation::hash_hex(&bytes);
    Json(serde_json::json!({
        "sessionId": id,
        "digest": digest,
        "bytes": bytes.len(),
        "redacted": redact,
        "redactedFields": redacted_keys,
        "truncated": truncated,
        "bundle": bundle,
    }))
    .into_response()
}

// ------------------------------------------------------- session settings

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct NativeSessionSettingsBody {
    #[serde(default)]
    compaction_model: Option<String>,
    #[serde(default)]
    max_turns: Option<u64>,
    #[serde(default)]
    max_consecutive_failures: Option<u64>,
    #[serde(default)]
    unset: Vec<String>,
}

/// `GET /native/session/{id}/settings` — durable selectable settings
/// (compaction model, task turn bound, consecutive-failure guard).
pub(crate) async fn native_session_settings_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let handle = match resolve_handle(&state, &id) {
        Ok(handle) => handle,
        Err(r) => return r,
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let session = handle.id().raw();
    let compaction_model = foundation.session_settings.get(
        session,
        faktor_foundation::session_settings::COMPACTION_MODEL_KEY,
    );
    let max_turns = foundation
        .session_settings
        .get(session, faktor_foundation::session_settings::MAX_TURNS_KEY);
    let max_failures = foundation.session_settings.get(
        session,
        faktor_foundation::session_settings::MAX_CONSECUTIVE_FAILURES_KEY,
    );
    match (compaction_model, max_turns, max_failures) {
        (Ok(compaction_model), Ok(max_turns), Ok(max_consecutive_failures)) => {
            Json(serde_json::json!({
                "sessionId": id,
                "compactionModel": compaction_model,
                "maxTurns": max_turns,
                "maxConsecutiveFailures": max_consecutive_failures,
                "applyTiming": {
                    "compactionModel": "next_compaction",
                    "maxTurns": "next_task",
                    "maxConsecutiveFailures": "next_task",
                },
            }))
            .into_response()
        }
        (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => foundation_error(e),
    }
}

/// `PUT /native/session/{id}/settings` — atomic validated update. Unknown
/// keys, out-of-range caps and unserved compaction models are typed 400/409.
pub(crate) async fn native_session_settings_put(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<NativeSessionSettingsBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid session settings body")),
    };
    let handle = match resolve_handle(&state, &id) {
        Ok(handle) => handle,
        Err(r) => return r,
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let mut applied = Vec::new();
    let session = handle.id().raw();
    if let Some(model) = body.compaction_model.as_deref() {
        if model == "inherit" {
            let _ = foundation.session_settings.clear(
                session,
                faktor_foundation::session_settings::COMPACTION_MODEL_KEY,
            );
            applied.push("compactionModel=inherit");
        } else {
            let Some((provider, model_id)) = model.split_once('/') else {
                return wire_status(malformed_body(
                    "compactionModel must be `inherit` or `provider/model`",
                ));
            };
            if state
                .deps
                .agent
                .deps()
                .providers
                .capabilities(provider, model_id)
                .is_none()
            {
                return conflict(
                    "unserved_compaction_model",
                    format!("compaction model {model:?} is not served by this daemon"),
                );
            }
            if let Err(e) = foundation.session_settings.set(
                session,
                faktor_foundation::session_settings::COMPACTION_MODEL_KEY,
                &serde_json::Value::String(model.to_string()),
            ) {
                return foundation_error(e);
            }
            applied.push("compactionModel");
        }
    }
    if let Some(turns) = body.max_turns {
        if let Err(e) = foundation.session_settings.set(
            session,
            faktor_foundation::session_settings::MAX_TURNS_KEY,
            &serde_json::json!(turns),
        ) {
            return foundation_error(e);
        }
        applied.push("maxTurns");
    }
    if let Some(failures) = body.max_consecutive_failures {
        if let Err(e) = foundation.session_settings.set(
            session,
            faktor_foundation::session_settings::MAX_CONSECUTIVE_FAILURES_KEY,
            &serde_json::json!(failures),
        ) {
            return foundation_error(e);
        }
        applied.push("maxConsecutiveFailures");
    }
    for key in &body.unset {
        let storage_key = match key.as_str() {
            "compactionModel" => faktor_foundation::session_settings::COMPACTION_MODEL_KEY,
            "maxTurns" => faktor_foundation::session_settings::MAX_TURNS_KEY,
            "maxConsecutiveFailures" => {
                faktor_foundation::session_settings::MAX_CONSECUTIVE_FAILURES_KEY
            }
            _ => {
                return wire_status(ApiError {
                    code: "malformed",
                    message: format!("unknown setting {key:?}"),
                    http_status: 400,
                    retryable: false,
                })
            }
        };
        if let Err(e) = foundation.session_settings.clear(session, storage_key) {
            return foundation_error(e);
        }
        applied.push(key.as_str());
    }
    Json(serde_json::json!({
        "sessionId": id,
        "applied": applied,
        "applyTiming": {
            "compactionModel": "next_compaction",
            "maxTurns": "next_task",
            "maxConsecutiveFailures": "next_task",
        },
    }))
    .into_response()
}
