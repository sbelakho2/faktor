//! Edition 5 foundation surfaces that sit beside session/tool authorities:
//! skills/hooks lifecycle (MCP lives in [`super::mcp_surface`]), memory
//! CRUD/retention, durable session
//! metadata/search/parallel listing, transcript search, the tool catalog,
//! browser boundary introspection and checkpoint fork/restore + partial
//! merge. Every mutation is bounded and returns the durable identity it
//! wrote; every listing is paged.
#![allow(clippy::result_large_err)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use faktor_foundation::customization::{HookDraft, SkillDraft, ORIGIN_PROJECT};
use faktor_foundation::session_meta::SessionMetaPatch;
use faktor_foundation::{Foundation, FoundationError, ReviewCommentDraft, ReviewCommentPatch};
use faktor_hooks::{HookEvent, HookInput, HookRegistry, HookSpec};
use faktor_instructions::SkillRegistry;
use faktor_protocol::error::ApiError;
use faktor_security::{redact, CompiledSecretPolicy, SecretPolicy};

use super::foundation::foundation_error;
use super::*;
use crate::api::AppState;

pub(crate) fn foundation(state: &AppState) -> Result<&Foundation, Response> {
    state.deps.foundation.as_deref().ok_or_else(|| {
        wire_status(ApiError {
            code: "foundation_disabled",
            message: "the Edition 5 foundation is not wired in this host".into(),
            http_status: 503,
            retryable: false,
        })
    })
}

pub(crate) fn conflict(code: &'static str, message: impl Into<String>) -> Response {
    wire_status(ApiError {
        code,
        message: message.into(),
        http_status: 409,
        retryable: false,
    })
}

pub(crate) fn unavailable(message: impl Into<String>) -> Response {
    wire_status(ApiError {
        code: "unavailable",
        message: message.into(),
        http_status: 503,
        retryable: false,
    })
}

pub(crate) fn resolve_handle(
    state: &AppState,
    id: &str,
) -> Result<faktor_session::SessionHandle, Response> {
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
    pub(crate) q: Option<String>,
    #[serde(default)]
    pub(crate) workspace: Option<String>,
    #[serde(default)]
    pub(crate) state: Option<String>,
    #[serde(default)]
    pub(crate) archived: Option<bool>,
    #[serde(default)]
    pub(crate) pinned: Option<bool>,
    #[serde(default)]
    pub(crate) limit: Option<u64>,
}

/// `GET /native/sessions?q=&workspace=&state=&archived=&pinned=&limit=` —
/// one SQL listing + one metadata read, filtered and bounded. Archived
/// sessions are hidden unless `archived=true`; `q` searches durable titles.
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
        // `faktor.session.titleSearch` is a real switch, not a decoration:
        // with it off, a title query refuses typed instead of returning an
        // empty list that looks like "no matches".
        let enabled = foundation
            .config
            .value("faktor.session.titleSearch")
            .ok()
            .flatten()
            .and_then(|value| value.as_bool())
            .unwrap_or(true);
        if !enabled {
            return conflict(
                "title_search_disabled",
                "title search is disabled by faktor.session.titleSearch; enable it or clear q",
            );
        }
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
    let max_parallel = foundation
        .config
        .u64_value("faktor.sessions.maxParallel")
        .ok()
        .flatten()
        .map(|value| value.clamp(1, 1_000) as usize)
        .unwrap_or(16);
    let mut active = 0usize;
    let mut by_workspace: BTreeMap<u64, usize> = BTreeMap::new();
    let mut sessions = Vec::new();
    let truncated = rows.len() > max_parallel;
    // Counts cover the WHOLE listing; only the rows are bounded. A truncated
    // page must never make the active/workspace totals look smaller.
    for row in rows.iter() {
        if row.state.is_active() {
            active += 1;
        }
        *by_workspace.entry(row.workspace_id.raw()).or_insert(0) += 1;
        if sessions.len() >= max_parallel {
            continue;
        }
        let m = meta
            .get(&session_u64(row.id))
            .cloned()
            .unwrap_or(faktor_foundation::SessionMeta {
                session_id: session_u64(row.id),
                pinned: false,
                archived: false,
                updated_ms: 0,
            });
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
        "totalSessions": rows.len(),
        "maxParallel": max_parallel,
        "truncated": truncated,
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
    let handle = match resolve_handle(&state, &id) {
        Ok(handle) => handle,
        Err(r) => return r,
    };
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    match foundation.session_meta.get(session_u64(handle.id())) {
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
    // F: the transcript page bound is a durable setting, not a hard-coded
    // 50. A request may ask for less; it can never exceed the setting or the
    // contract's own 200-row ceiling.
    let configured_limit = match foundation(&state) {
        Ok(foundation) => foundation
            .config
            .u64_value("faktor.transcript.searchLimit")
            .ok()
            .flatten()
            .unwrap_or(50),
        Err(_) => 50,
    };
    let limit = q
        .limit
        .unwrap_or(configured_limit)
        .min(configured_limit)
        .clamp(1, 200);
    // A miss walks the whole session window in SQLite (50k rows measured
    // ~65 ms single-request). Run the synchronous scan on the blocking pool
    // so the scan can never pin a Tokio worker for its whole duration; the
    // scan itself stays linear in rows (no O(n^2) curve).
    let store = state.deps.session.store();
    let session_id = handle.id();
    let query = q.q.clone();
    let scanned =
        tokio::task::spawn_blocking(move || store.search_messages(session_id, &query, limit)).await;
    match scanned {
        Ok(Ok(rows)) => {
            let needle = q.q.to_ascii_lowercase();
            // A match may live in `message.data` (user prompts) or in a part
            // row (`text`/`reasoning`/`tool_result`), which is where every
            // assistant answer is stored. The snippet must quote the row that
            // actually matched, not an empty assistant envelope. Part reads
            // are bounded to this page (never the whole transcript) and a
            // read failure fails the page loudly instead of guessing text.
            let ids: Vec<i64> = rows.iter().map(|row| row.id).collect();
            let mut parts_by_message = match state.deps.session.store().parts_for_messages(&ids) {
                Ok(parts) => parts,
                Err(e) => return api_err(&store_err_to_core(e)),
            };
            let mut matches: Vec<serde_json::Value> = Vec::with_capacity(rows.len());
            for row in &rows {
                let mut text = row
                    .data
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                if !text.to_ascii_lowercase().contains(&needle) {
                    let parts = parts_by_message.remove(&row.id).unwrap_or_default();
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
                "limit": limit,
                "matches": matches,
                "hasMore": rows.len() as u64 == limit,
            }))
            .into_response()
        }
        Ok(Err(e)) => api_err(&store_err_to_core(e)),
        Err(e) => wire_status(ApiError {
            code: "internal",
            message: format!("transcript search task failed: {e}"),
            http_status: 500,
            retryable: true,
        }),
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
    // ONE atomic writer transaction: the expected-stamp check and the
    // upsert can no longer interleave with another writer (the historical
    // read-then-write pair let two same-stamp writers both apply).
    match handle.upsert_memory_fact_cas(&req.kind, &req.key, &req.value, req.expected_updated_ms) {
        Ok(faktor_store::MemoryFactCas::Applied(row)) => Json(serde_json::json!({
            "saved": true,
            "fact": serde_json::json!({
                "kind": row.kind, "key": row.key, "value": row.value,
                "updatedMs": row.updated_ms,
            }),
        }))
        .into_response(),
        Ok(faktor_store::MemoryFactCas::Conflict) => conflict(
            "revision_conflict",
            "the memory fact changed since it was read; re-read and merge",
        ),
        Ok(faktor_store::MemoryFactCas::Missing) => conflict(
            "revision_conflict",
            "the memory fact no longer exists; re-read",
        ),
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
    let revision = handle
        .last_event_seq()
        .ok()
        .flatten()
        .map(|s| s.raw())
        .unwrap_or(0);
    // Atomic CAS delete: a concurrent update that lands between a separate
    // read and delete can no longer be removed under an older stamp.
    match handle.delete_memory_fact_cas(&kind, &key, q.expected_updated_ms) {
        Ok(faktor_store::MemoryFactDeleteCas::Deleted(deleted)) => Json(serde_json::json!({
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
        Ok(faktor_store::MemoryFactDeleteCas::Missing) => {
            wire_status(not_found("memory fact"))
        }
        Ok(faktor_store::MemoryFactDeleteCas::Conflict) => conflict(
            "revision_conflict",
            "the memory fact changed since it was read; re-read and merge",
        ),
        Err(e) => api_err(&e),
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

// ---------------------------------------------------------------- skills

/// Bounded discovery cache for the skills listing. A cursor walk issues one
/// HTTP request per page; each used to run a FULL [`SkillRegistry::discover`]
/// (every `SKILL.md` opened and parsed) even though the tree had not changed.
/// The cache is keyed by the admitted workspace root and validated by a
/// metadata fingerprint (path, kind, size, mtime of every skill entry and its
/// `SKILL.md`): a content or rename change yields a new fingerprint and a
/// fresh discovery, so listing semantics stay exact.
const MAX_CACHED_SKILL_TREES: usize = 8;
const SKILL_FINGERPRINT_ENTRIES: usize = 4096;

struct CachedSkillTree {
    fingerprint: u64,
    registry: Arc<SkillRegistry>,
    #[cfg(test)]
    discoveries: usize,
}

static SKILL_TREES: OnceLock<Mutex<HashMap<std::path::PathBuf, CachedSkillTree>>> = OnceLock::new();

fn skill_trees() -> &'static Mutex<HashMap<std::path::PathBuf, CachedSkillTree>> {
    SKILL_TREES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// One metadata-only fingerprint of the two skill directories. No file body
/// is read: a hit costs a bounded directory walk/stat pass, a miss pays the
/// real discovery parse.
fn skill_tree_fingerprint(root: &std::path::Path) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let rooted = faktor_fs::RootedDir::open(root).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for dir_rel in [".faktor/skills", ".claude/skills"] {
        let dir = std::path::Path::new(dir_rel);
        let listing = match rooted.list_entries(dir, SKILL_FINGERPRINT_ENTRIES) {
            Ok(listing) => listing,
            Err(error) => {
                // Include the failure in the fingerprint so a repair
                // (directory becomes readable) invalidates the cache.
                (true, error.message).hash(&mut hasher);
                continue;
            }
        };
        (dir_rel, listing.overflowed).hash(&mut hasher);
        let mut entries = listing.entries;
        entries.sort_by(|a, b| a.rel.cmp(&b.rel));
        for entry in entries {
            entry.rel.hash(&mut hasher);
            (entry.kind as u8).hash(&mut hasher);
            entry.size.hash(&mut hasher);
            entry.modified_ms.hash(&mut hasher);
            if entry.kind == faktor_fs::rooted::RootedEntryKind::Directory {
                match rooted.list_entries(&entry.rel, SKILL_FINGERPRINT_ENTRIES) {
                    Ok(inner) => {
                        inner.overflowed.hash(&mut hasher);
                        let mut inner_entries = inner.entries;
                        inner_entries.sort_by(|a, b| a.rel.cmp(&b.rel));
                        for inner_entry in inner_entries {
                            inner_entry.rel.hash(&mut hasher);
                            (inner_entry.kind as u8).hash(&mut hasher);
                            inner_entry.size.hash(&mut hasher);
                            inner_entry.modified_ms.hash(&mut hasher);
                        }
                    }
                    Err(error) => (true, error.message).hash(&mut hasher),
                }
            }
        }
    }
    Some(hasher.finish())
}

/// The cached registry for `root`; a fingerprint miss re-discovers. The
/// map is hard-bounded (one existing tree is evicted when full) and never
/// holds more than one registry per root.
fn skill_registry(root: &std::path::Path) -> Arc<SkillRegistry> {
    // One cache key per tree: the stored workspace root may be a
    // non-canonical spelling, so canonicalize before keying (the same
    // spelling the test probes use).
    let key = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let Some(fingerprint) = skill_tree_fingerprint(&key) else {
        // Unopenable root: discovery itself surfaces the skips; no cache.
        return Arc::new(SkillRegistry::discover(&key));
    };
    {
        let cache = skill_trees()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(cached) = cache.get(&key) {
            if cached.fingerprint == fingerprint {
                return Arc::clone(&cached.registry);
            }
        }
    }
    let registry = Arc::new(SkillRegistry::discover(&key));
    let mut cache = skill_trees()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if cache.len() >= MAX_CACHED_SKILL_TREES && !cache.contains_key(&key) {
        if let Some(oldest) = cache.keys().next().cloned() {
            cache.remove(&oldest);
        }
    }
    #[cfg(test)]
    let prior_discoveries = cache
        .get(&key)
        .map(|cached| cached.discoveries)
        .unwrap_or(0);
    cache.insert(
        key,
        CachedSkillTree {
            fingerprint,
            registry: Arc::clone(&registry),
            #[cfg(test)]
            discoveries: prior_discoveries + 1,
        },
    );
    registry
}

#[cfg(test)]
pub(crate) fn skill_discoveries_for(root: &std::path::Path) -> usize {
    skill_trees()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(root)
        .map(|cached| cached.discoveries)
        .unwrap_or(0)
}

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
    /// Cursor of the DISCOVERED listing: the last name served on the
    /// previous page. Names are sorted, so `after` + `limit` reaches every
    /// discovered skill (the old fixed `.take(500)` made 501+ unreachable).
    #[serde(default)]
    after: Option<String>,
}

/// `GET /native/skills?q=&session=&limit=&after=` — installed trusted
/// records plus discovered-but-not-installed workspace skills (metadata
/// only). `after` + `limit` page the sorted discovered list; `nextCursor`
/// is exact and `hasMore` names the remainder.
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
    if q.after.as_deref().is_some_and(|after| after.len() > 256) {
        return wire_status(malformed_body("skills cursor must be <= 256 bytes"));
    }
    let discovered_limit = q.limit.unwrap_or(500).clamp(1, 500) as usize;
    let mut discovered_total: u64 = 0;
    let mut discovered_truncated = false;
    let mut discovered_cursor: Option<String> = None;
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
            let registry = skill_registry(&root);
            let installed_names: BTreeSet<&str> = records.iter().map(|r| r.name.as_str()).collect();
            let mut candidates: Vec<&faktor_instructions::SkillMeta> = registry
                .entries()
                .iter()
                .filter(|e| !installed_names.contains(e.name.as_str()))
                .filter(|e| needle.is_empty() || e.name.to_ascii_lowercase().contains(&needle))
                .collect();
            // Deterministic cursor order: the registry walk order is
            // filesystem-dependent, so the cursor sorts by name.
            candidates.sort_by(|a, b| a.name.cmp(&b.name));
            let matching_total: u64 = candidates.len() as u64;
            discovered_total = matching_total;
            let start = match q.after.as_deref() {
                Some(after) => candidates
                    .iter()
                    .position(|e| e.name.as_str() > after)
                    .unwrap_or(candidates.len()),
                None => 0,
            };
            let remaining = candidates.len().saturating_sub(start);
            discovered_truncated = remaining > discovered_limit;
            discovered_cursor =
                discovered_truncated.then(|| candidates[start + discovered_limit - 1].name.clone());
            let discovered: Vec<serde_json::Value> = candidates
                .into_iter()
                .skip(start)
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
        "nextCursor": discovered_cursor,
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
    if spec.command.is_empty() || spec.command.len() > 4096 || spec.command.contains('\0') {
        return Err(wire_status(malformed_body(
            "hook command must be 1..=4096 bytes without NUL",
        )));
    }
    if spec.deadline_ms == 0 || spec.deadline_ms > 300_000 {
        return Err(wire_status(malformed_body(
            "hook deadline must be in (0, 300000] ms",
        )));
    }
    // The spec is authored by the API client. Cap the structural surface
    // here as well as in `HookRegistry::register` so a hostile persisted
    // spec cannot make `run_sync` retain an unbounded stream head.
    if spec.stdout_cap == 0
        || spec.stdout_cap > faktor_hooks::MAX_HOOK_OUTPUT_CAP
        || spec.stderr_cap == 0
        || spec.stderr_cap > faktor_hooks::MAX_HOOK_OUTPUT_CAP
    {
        return Err(wire_status(malformed_body(&format!(
            "hook output caps must be in 1..={} bytes",
            faktor_hooks::MAX_HOOK_OUTPUT_CAP
        ))));
    }
    if spec.args.len() > faktor_hooks::MAX_HOOK_ARGS
        || spec
            .args
            .iter()
            .any(|a| a.len() > faktor_hooks::MAX_HOOK_ARG_BYTES || a.contains('\0'))
    {
        return Err(wire_status(malformed_body(
            "hook args exceed the served structural bound",
        )));
    }
    if spec.events.len() > HookEvent::ALL.len() {
        return Err(wire_status(malformed_body(
            "a hook may subscribe to at most 17 event kinds",
        )));
    }
    if spec.matcher.tools.len() > faktor_hooks::MAX_HOOK_MATCHERS
        || spec.matcher.paths.len() > faktor_hooks::MAX_HOOK_MATCHERS
    {
        return Err(wire_status(malformed_body(
            "a hook matcher may carry at most 32 tool/path predicates",
        )));
    }
    for predicate in spec.matcher.tools.iter().chain(spec.matcher.paths.iter()) {
        if predicate.is_empty()
            || predicate.len() > faktor_hooks::MAX_HOOK_MATCHER_BYTES
            || predicate.contains('\u{0}')
            || predicate.chars().any(char::is_control)
        {
            return Err(wire_status(malformed_body(
                "hook matcher predicates must be 1..=512 bytes without control characters",
            )));
        }
    }
    if spec.env.len() > faktor_hooks::MAX_HOOK_ENV
        || spec.env.iter().any(|(name, value)| {
            name.is_empty()
                || name.len() > faktor_hooks::MAX_HOOK_ENV_NAME_BYTES
                || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
                || value.len() > faktor_hooks::MAX_HOOK_ENV_VALUE_BYTES
                || value.contains('\0')
        })
    {
        return Err(wire_status(malformed_body(
            "hook env entries exceed the served structural bound",
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
    if !spec.matches_input(&input) {
        return Json(serde_json::json!({
            "hookId": id,
            "event": req.event,
            "matched": false,
            "verdict": "not_matched",
            "reason": "the event/tool/path matcher did not select this input; nothing was spawned",
            "audit": null,
        }))
        .into_response();
    }
    let verdict = registry.run_one(&spec, &input);
    Json(serde_json::json!({
        "hookId": id,
        "event": req.event,
        "matched": true,
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
    #[serde(default)]
    include_schema: Option<bool>,
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
    let include_schema = q.include_schema.unwrap_or(true);
    let needle = q.q.as_deref().unwrap_or("").to_ascii_lowercase();
    let registry = &state.deps.agent.deps().tools;
    let mut out = Vec::new();
    for name in registry.names() {
        if !needle.is_empty() && !name.to_ascii_lowercase().contains(&needle) {
            continue;
        }
        if let Some(tool) = registry.get(&name) {
            // A lazy tool is registered and callable, but the model only sees
            // its complete schema after a deterministic activation signal.
            // The catalog must say that instead of the old unconditional
            // "ready" (which made Supply/Engineering/approval look eager).
            let lazy = registry.is_lazy(&name);
            let mut row = serde_json::json!({
                "name": name,
                "namespace": "builtin",
                "description": tool.description,
                "source": "builtin",
                "health": if lazy { "available_when_activated" } else { "ready" },
                "exposure": if lazy { "lazy" } else { "normal" },
            });
            if include_schema {
                row["inputSchema"] = tool.input_schema.clone();
            }
            out.push(row);
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
                        let mut row = serde_json::json!({
                            "name": namespaced,
                            "namespace": format!("mcp:{}", record.name),
                            "description": tool.get("description").cloned().unwrap_or_default(),
                            "source": "mcp",
                            "health": "ready",
                        });
                        if include_schema {
                            row["inputSchema"] =
                                tool.get("inputSchema").cloned().unwrap_or_default();
                        }
                        out.push(row);
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
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeUrlBody {
    url: String,
}

/// `POST /native/browser/validate-url` — scheme/host boundary preview. A
/// hostname is left `unresolved` (no DNS here); a literal address is
/// classified by the one egress authority.
///
/// The response never echoes the submitted URL: a URL can carry credentials
/// in userinfo, the path, the query or the fragment, and this payload is
/// rendered in a panel/log. Userinfo is refused outright and the echoed
/// destination is the canonical ORIGIN only (scheme/host/port), with
/// path-segment and query/fragment presence reported as counts/flags.
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
    if req.url.len() > 4096 {
        return wire_status(malformed_body("url must be at most 4096 bytes"));
    }
    let url = match url::Url::parse(&req.url) {
        Ok(url) => url,
        Err(e) => return wire_status(malformed_body(&format!("invalid URL: {e}"))),
    };
    if !url.username().is_empty() || url.password().is_some() {
        return wire_status(malformed_body(
            "URLs must not embed userinfo credentials; pass auth as a vault reference",
        ));
    }
    let scheme_ok = matches!(url.scheme(), "http" | "https");
    let host = url.host().map(|h| h.to_string()).unwrap_or_default();
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
    let class_of = |ip: std::net::IpAddr| faktor_security::network::classify_ip(ip);
    let (class, decision, reason) = if !scheme_ok {
        (
            "unsupported".to_string(),
            "deny",
            "only http/https destinations are supported".to_string(),
        )
    } else if let Some(host) = url.host() {
        let literal = match host {
            url::Host::Ipv4(ip) => Some(class_of(std::net::IpAddr::V4(ip))),
            url::Host::Ipv6(ip) => Some(class_of(std::net::IpAddr::V6(ip))),
            url::Host::Domain(domain) => {
                let lower = domain.trim_end_matches('.').to_ascii_lowercase();
                if lower == "localhost" || lower.ends_with(".localhost") {
                    Some(faktor_security::network::AddressClass::Loopback)
                } else {
                    None
                }
            }
        };
        match literal {
            Some(class) => {
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
                    class.as_str().to_string(),
                    decision,
                    reason.to_string(),
                )
            }
            None => (
                "unresolved".to_string(),
                "resolve",
                "hostname not resolved here; the egress broker resolves and classifies every answer at connect time"
                    .to_string(),
            ),
        }
    } else {
        (
            "unresolved".to_string(),
            "deny",
            "the URL has no host".to_string(),
        )
    };
    let path_segments = url
        .path_segments()
        .map(|segments| segments.count())
        .unwrap_or(0);
    Json(serde_json::json!({
        "url": url.origin().ascii_serialization(),
        "scheme": url.scheme(),
        "host": host,
        "class": class,
        "decision": decision,
        "reason": reason,
        "port": url.port_or_known_default(),
        "pathSegments": path_segments,
        "queryPresent": url.query().is_some(),
        "fragmentPresent": url.fragment().is_some(),
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
            "parentSessionId": sid.to_string(),
            "forkedAt": "head",
            "lineageDurable": true,
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

pub(crate) fn agent_paths(paths: &[String]) -> Result<Vec<std::path::PathBuf>, Response> {
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
        let traversal = path.contains('\\')
            || p.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            });
        if p.is_absolute() || traversal {
            return Err(wire_status(malformed_body(
                "decision paths must be workspace-relative without `..` (both separators)",
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

/// Latest checkpoint `after_hash` per path, read ONCE. The list endpoint
/// used to call the single-path lookup per comment, and each lookup loaded
/// EVERY checkpoint row of the session: O(comments x checkpoints) reads.
fn latest_change_hashes(
    handle: &faktor_session::SessionHandle,
) -> std::collections::HashMap<String, String> {
    let mut latest: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut sequences: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
    for row in handle.checkpoints_of().ok().unwrap_or_default() {
        let entry = sequences.entry(row.path.clone()).or_insert(i64::MIN);
        if row.sequence >= *entry {
            *entry = row.sequence;
            latest.insert(row.path, row.after_hash);
        }
    }
    latest
}

fn latest_change_hash(handle: &faktor_session::SessionHandle, path: &str) -> Option<String> {
    latest_change_hashes(handle).remove(path)
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
            let mut current_by_path = latest_change_hashes(&handle);
            let rows: Vec<serde_json::Value> = comments
                .into_iter()
                .map(|comment| {
                    let current = current_by_path.remove(&comment.path);
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
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    // Resolve the change-set identity from the DURABLE shadow row (the same
    // `base-<shadow_id>-cs` the orchestrator derives) and consult the
    // stored receipt FIRST. An accepted change set is Integrated, and
    // `present_change_set` rightly refuses to stage it again; a replay of
    // the SAME decision must still return the stored receipt instead of a
    // misleading conflict.
    let shadow_row = match shadows.active_shadow(handle.id()) {
        Ok(Some(row)) => row,
        Ok(None) => {
            return unavailable(
                "the session has no candidate change set; stage one before accepting",
            )
        }
        Err(e) => return super::agents::exec_error_response(&e),
    };
    let change_set_id = format!("base-{}-cs", shadow_row.shadow_id);
    let digest = change_receipt_digest(&body, &change_set_id);
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
    if let Err(e) = shadows.present_change_set(handle.id()) {
        return super::agents::exec_error_response(&e);
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
        "rejected": body.rejected,
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
        // `abort` deliberately leaves the session READY for the next prompt
        // (Stop cancels a turn, not the session). That landing state still
        // counts as active for `delete_session`, which would then refuse the
        // very deletion the caller explicitly authorized with abort=true.
        // Move it to a non-active lifecycle; a deletion is not a turn.
        let state = match handle.state() {
            Ok(state) => state,
            Err(e) => return api_err(&e),
        };
        if state.is_active() {
            if let Err(e) = handle.suspend() {
                return api_err(&e);
            }
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
    let policy = SecretPolicy::default();
    // Compile the frozen patterns ONCE per export: the recursive walk scans
    // every string in the transcript, and a per-string compile would make
    // redaction cost O(strings x patterns) instead of O(bytes).
    let compiled = CompiledSecretPolicy::try_from(policy.clone()).ok();
    redact_export_with(value, redacted_keys, &policy, compiled.as_ref());
}

/// Recursive export redaction: credential-shaped KEYS, credential-shaped
/// VALUES (the default secret-pattern set, including keys embedded in user
/// text), PEM private-key BLOCKS (header + body, not just the header) and
/// generic `Bearer` credentials. A session title or message part that
/// happens to contain a recognizable secret is redacted exactly like a
/// structured `apiKey` field — otherwise `redacted: true` would be a lie.
fn redact_export_with(
    value: &mut serde_json::Value,
    redacted_keys: &mut usize,
    policy: &SecretPolicy,
    compiled: Option<&CompiledSecretPolicy>,
) {
    match value {
        serde_json::Value::Object(map) => {
            let entries = std::mem::take(map);
            for (key, mut child) in entries {
                let redacted_key = match compiled {
                    Some(compiled) => compiled.redact_text(&key),
                    None => redact(&key, policy),
                };
                if redacted_key != key {
                    *redacted_keys += 1;
                }
                let lower = redacted_key.to_ascii_lowercase().replace(['-', '_'], "");
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
                        | "passwd"
                        | "secret"
                        | "clientsecret"
                        | "privatekey"
                        | "credential"
                        | "credentials"
                ) || lower.contains("apikey")
                    || lower.contains("secret")
                    || lower.contains("password")
                    || lower.contains("credential")
                    || lower.contains("privatekey")
                    || lower.ends_with("token");
                if sensitive {
                    child = serde_json::Value::String("[redacted]".into());
                    *redacted_keys += 1;
                } else {
                    redact_export_with(&mut child, redacted_keys, policy, compiled);
                }
                map.insert(redacted_key, child);
            }
        }
        serde_json::Value::Array(items) => {
            for child in items {
                redact_export_with(child, redacted_keys, policy, compiled);
            }
        }
        serde_json::Value::String(text) => {
            if text.starts_with("Bearer ") && text.len() > 16 {
                *text = "[redacted]".into();
                *redacted_keys += 1;
                return;
            }
            let without_keys = redact_private_key_blocks(text);
            let cleaned = redact(&without_keys, policy);
            if cleaned != *text {
                *text = cleaned;
                *redacted_keys += 1;
            }
        }
        _ => {}
    }
}

/// Replace every PEM private-key block (header through END marker) with one
/// marker. The pattern redactor only matches the `-----BEGIN … PRIVATE
/// KEY-----` header, which would leave the base64 key body in the export.
fn redact_private_key_blocks(text: &str) -> String {
    const BEGIN: &str = "-----BEGIN";
    const END: &str = "-----END";
    const KEY_SUFFIX: &str = "PRIVATE KEY-----";
    const MAX_HEADER: usize = 128;
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    while let Some(rel) = text[cursor..].find(BEGIN) {
        let start = cursor + rel;
        let window_end = (start + MAX_HEADER).min(text.len());
        let header = text[start..window_end].find(KEY_SUFFIX);
        let Some(header_rel) = header else {
            out.push_str(&text[cursor..start + BEGIN.len()]);
            cursor = start + BEGIN.len();
            continue;
        };
        if text[start..start + header_rel].contains('\n') {
            out.push_str(&text[cursor..start + BEGIN.len()]);
            cursor = start + BEGIN.len();
            continue;
        }
        let header_end = start + header_rel + KEY_SUFFIX.len();
        out.push_str(&text[cursor..start]);
        out.push_str("<redacted:pem_private_key>");
        let Some(end_rel) = text[header_end..].find(END) else {
            return out;
        };
        let end_start = header_end + end_rel;
        let Some(suffix_rel) = text[end_start..].find(KEY_SUFFIX) else {
            return out;
        };
        cursor = end_start + suffix_rel + KEY_SUFFIX.len();
    }
    out.push_str(&text[cursor..]);
    out
}

/// `POST /native/session/{id}/export` — bounded bundle (metadata,
/// transcript, checkpoint/diff identity, proof) with a content digest.
/// Secret-shaped material is ALWAYS redacted (credential-shaped keys,
/// recognizable secret values, PEM private-key blocks and `Bearer` tokens);
/// `redact:false` is accepted for wire compatibility but can no longer turn
/// that scan off. Never includes vault material, cookies or auth headers.
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
    match state.deps.session.store().session_deletion(session_id) {
        Ok(Some(_)) => {
            return conflict(
                "session_deleted",
                "the session content was deleted; only the closed tombstone remains",
            )
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
    let store = state.deps.session.store();
    let mut transcript = Vec::new();
    let mut transcript_bytes = 0usize;
    let mut truncated = false;
    // ONE page read + ONE bounded batch parts read (the historical export
    // ran a `parts_of` query per message: up to 1,000 round trips per
    // export).
    let mut ordered =
        match store.messages_window_with_parts(handle.id(), None, None, 1_000, u64::MAX) {
            Ok(rows) => rows,
            Err(e) => return api_err(&store_err_to_core(e)),
        };
    ordered.reverse();
    let total_messages = ordered.len();
    for (message, parts) in &ordered {
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
    let checkpoints = match handle.checkpoints_of() {
        Ok(checkpoints) => checkpoints,
        Err(e) => return api_err(&e),
    };
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
    {
        let tasks = match store.list_tasks(handle.id()) {
            Ok(tasks) => tasks,
            Err(e) => return api_err(&store_err_to_core(e)),
        };
        let tasks: Vec<_> = tasks.into_iter().take(50).collect();
        let ids: Vec<faktor_core::TaskId> = tasks.iter().map(|task| task.task_id).collect();
        // ONE batched read (first 20 records per task) instead of one query
        // per task; regrouped below in the original task order.
        let mut by_task: std::collections::HashMap<i64, Vec<(i64, String)>> =
            std::collections::HashMap::new();
        let records = match store.verification_record_head_by_tasks(handle.id(), &ids, 20) {
            Ok(records) => records,
            Err(e) => return api_err(&store_err_to_core(e)),
        };
        for (task_id, revision, status) in records {
            by_task.entry(task_id).or_default().push((revision, status));
        }
        for task in tasks {
            for (revision, status) in by_task
                .remove(&(task.task_id.raw() as i64))
                .unwrap_or_default()
            {
                proof.push(serde_json::json!({
                    "taskId": task.task_id.raw(),
                    "revision": revision,
                    "status": status,
                }));
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
        "redacted": true,
    });
    // Secret hygiene is not opt-out: the scan runs for every export so a
    // caller-supplied `redact:false` cannot exfiltrate a credential that a
    // message/title/diff happens to contain.
    let mut redacted_keys = 0usize;
    redact_export(&mut bundle, &mut redacted_keys);
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
        "redacted": true,
        "redactRequested": redact,
        "redactedFields": redacted_keys,
        "truncated": truncated,
        "bundle": bundle,
    }))
    .into_response()
}

// ------------------------------------------------------- session settings
