//! Rev8 instruction inspection, precedence and editing (S11/S69/I-11).
//!
//! The loaded rule tree remains the authority (`faktor-instructions`): this
//! surface only projects it and writes ONE validated, atomic source edit.
//! Nothing here executes repository text as authority, and an edit can never
//! rewrite the instructions already captured by a running turn.
#![allow(clippy::result_large_err)]

use std::path::PathBuf;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use faktor_foundation::hash_hex;
use faktor_instructions::{
    kind_for_rel_path, LoadedInstructions, RuleSourceKind, MAX_RULE_BYTES, PRECEDENCE,
};
use faktor_protocol::error::ApiError;

use super::*;
use crate::api::AppState;

const MAX_INSTRUCTION_PREVIEW_BYTES: usize = 64 * 1024;
const MAX_INSTRUCTION_CONTENT_PREVIEW: usize = 2 * 1024;
const MAX_TOUCHED_PATHS: usize = 32;
const MAX_INSTRUCTION_PATH_BYTES: usize = 1024;

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

fn unavailable(message: impl Into<String>) -> Response {
    wire_status(ApiError {
        code: "unavailable",
        message: message.into(),
        http_status: 503,
        retryable: false,
    })
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeInstructionsQuery {
    session: String,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    touched: Option<String>,
}

fn touched_paths(raw: Option<&str>) -> Vec<String> {
    raw.map(|raw| {
        raw.split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .take(MAX_TOUCHED_PATHS)
            .map(str::to_string)
            .collect()
    })
    .unwrap_or_default()
}

struct LoadedInstructionView {
    loaded: LoadedInstructions,
    epoch: Option<String>,
    bundle_hash: String,
}

fn load_instructions(
    state: &AppState,
    handle: &faktor_session::SessionHandle,
) -> Result<LoadedInstructionView, Response> {
    let row = handle.row().map_err(|e| api_err(&e))?;
    let resolver = &state.deps.agent.deps().instructions_resolver;
    let loaded = resolver
        .resolve_for_session(handle.id().raw(), row.workspace_id.raw(), None)
        .map_err(|e| conflict("instructions_unavailable", e.to_string()))?;
    let epoch = loaded.epoch().map(|epoch| epoch.to_string());
    let bundle_hash = match loaded.instructions() {
        Some(instructions) => {
            let mut material = Vec::new();
            for instruction in instructions.all() {
                material.extend_from_slice(instruction.path.as_bytes());
                material.push(0);
                material.extend_from_slice(instruction.hash.to_string().as_bytes());
                material.push(0);
            }
            hash_hex(&material)
        }
        None => hash_hex(b""),
    };
    Ok(LoadedInstructionView {
        loaded,
        epoch,
        bundle_hash,
    })
}

fn precedence_json() -> Vec<serde_json::Value> {
    PRECEDENCE
        .iter()
        .map(|(kind, priority)| {
            serde_json::json!({
                "source": kind,
                "priority": priority,
                "origin": "repository",
            })
        })
        .collect()
}

fn binding_json(
    state: &AppState,
    handle: &faktor_session::SessionHandle,
    view: &LoadedInstructionView,
) -> serde_json::Value {
    let captured = state
        .deps
        .session
        .store()
        .instruction_binding_get(handle.id())
        .ok()
        .flatten();
    // `is_active()` is true for ReadyForNextTurn (idle between turns); an
    // ACTIVE RUN is a turn actually in flight, so its captured bundle must
    // not be rewritten.
    let active_run = handle
        .state()
        .map(|state| {
            matches!(
                state,
                faktor_core::state::AgentState::Preparing
                    | faktor_core::state::AgentState::BuildingContext
                    | faktor_core::state::AgentState::WaitingForModel
                    | faktor_core::state::AgentState::Streaming
                    | faktor_core::state::AgentState::ToolRequested
                    | faktor_core::state::AgentState::WaitingForPermission
                    | faktor_core::state::AgentState::ExecutingTool
                    | faktor_core::state::AgentState::Validating
                    | faktor_core::state::AgentState::UpdatingMemory
            )
        })
        .unwrap_or(false);
    let matches = captured
        .as_ref()
        .map(|binding| Some(&binding.instruction_epoch) == view.epoch.as_ref())
        .unwrap_or(false);
    let apply_timing = if active_run && !matches {
        "restart_required"
    } else {
        "next_task"
    };
    serde_json::json!({
        "capturedEpoch": captured.as_ref().map(|b| b.instruction_epoch.clone()),
        "capturedBundleHash": captured.as_ref().map(|b| b.bundle_hash.clone()),
        "capturedAtMs": captured.as_ref().map(|b| b.captured_ms),
        "capturedTurnOpId": captured.as_ref().and_then(|b| b.turn_op_id.map(|v| v.to_string())),
        "currentEpoch": view.epoch,
        "currentBundleHash": view.bundle_hash,
        "matches": matches,
        "activeRun": active_run,
        "applyTiming": apply_timing,
        "restartRequired": apply_timing == "restart_required",
    })
}

fn instruction_json(
    instruction: &faktor_instructions::Instruction,
    active: Option<&faktor_instructions::Instruction>,
) -> serde_json::Value {
    serde_json::json!({
        "source": instruction.source,
        "path": instruction.path,
        "scope": instruction.scope,
        "priority": instruction.priority,
        "hash": instruction.hash,
        "bytes": instruction.content.len(),
        "active": active.is_some(),
        "reasonLoaded": active.map(|a| a.reason_loaded.clone()),
        "contentPreview": instruction
            .content
            .chars()
            .take(MAX_INSTRUCTION_CONTENT_PREVIEW)
            .collect::<String>(),
    })
}

fn conflict_json(
    winner: &faktor_instructions::Instruction,
    loser: &faktor_instructions::Instruction,
    reason: &str,
) -> serde_json::Value {
    serde_json::json!({
        "scope": if winner.scope.is_empty() { loser.scope.clone() } else { winner.scope.clone() },
        "winner": {"path": winner.path, "source": winner.source, "priority": winner.priority},
        "loser": {"path": loser.path, "source": loser.source, "priority": loser.priority},
        "reason": reason,
    })
}

/// `GET /native/instructions` — loaded sources by origin/scope, the active
/// set for the prompt, conflicts, skips, precedence and the I-11 binding.
pub(crate) async fn native_instructions_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<NativeInstructionsQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(query) => query,
        Err(_) => return malformed("invalid instructions query"),
    };
    let handle = match native_resolve_session(&state, &q.session) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    let view = match load_instructions(&state, &handle) {
        Ok(view) => view,
        Err(response) => return response,
    };
    let prompt = q
        .prompt
        .clone()
        .unwrap_or_else(|| handle.title().unwrap_or_default());
    let touched = touched_paths(q.touched.as_deref());
    let active = view.loaded.active_for(&prompt, &touched);
    let instructions: Vec<serde_json::Value> = view
        .loaded
        .instructions()
        .map(|tree| {
            tree.all()
                .iter()
                .map(|instruction| {
                    let active = active
                        .iter()
                        .find(|a| a.path == instruction.path && a.hash == instruction.hash);
                    instruction_json(instruction, active)
                })
                .collect()
        })
        .unwrap_or_default();
    // Conflicts: two ACTIVE rules covering the same non-empty scope (or the
    // same keyword directive) where a higher-priority source wins. The UI
    // shows the winner and reason; no silent resolution.
    let mut conflicts = Vec::new();
    for (index, winner) in active.iter().enumerate() {
        for loser in active.iter().skip(index + 1) {
            let scope_overlap = !winner.scope.is_empty() && winner.scope == loser.scope;
            let keyword_overlap = winner
                .content
                .lines()
                .take(8)
                .filter_map(|line| line.trim_start().strip_prefix("# Scope:"))
                .any(|line| {
                    loser
                        .content
                        .lines()
                        .take(8)
                        .any(|other| other.trim_start().strip_prefix("# Scope:") == Some(line))
                });
            if !scope_overlap && !keyword_overlap {
                continue;
            }
            let (winner, loser) = if winner.priority >= loser.priority {
                (winner, loser)
            } else {
                (loser, winner)
            };
            conflicts.push(conflict_json(
                winner,
                loser,
                "higher-precedence source wins for the overlapping scope",
            ));
            if conflicts.len() >= 32 {
                break;
            }
        }
        if conflicts.len() >= 32 {
            break;
        }
    }
    let skips: Vec<serde_json::Value> = view
        .loaded
        .skipped()
        .iter()
        .map(|skip| {
            serde_json::json!({
                "path": skip.path,
                "bytes": skip.bytes,
                "reason": skip.reason,
            })
        })
        .collect();
    let managed_authority = state.deps.control_plane.as_ref().map(|_| {
        serde_json::json!({
            "kind": "organization",
            "locks": [],
            "note": "no instruction-level lock is published by the control plane; managed policy is enforced by the effective permission/config layer",
        })
    });
    Json(serde_json::json!({
        "sessionId": handle.id().to_string(),
        "epoch": view.epoch,
        "bundleHash": view.bundle_hash,
        "instructions": instructions,
        "activeCount": active.len(),
        "skips": skips,
        "precedence": precedence_json(),
        "conflicts": conflicts,
        "managedAuthority": managed_authority,
        "binding": binding_json(&state, &handle, &view),
        "applyTiming": binding_json(&state, &handle, &view)["applyTiming"].clone(),
    }))
    .into_response()
}

/// `GET /native/instructions/preview` — the bounded merged bundle that the
/// next task will receive, with per-source provenance.
pub(crate) async fn native_instructions_preview(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<NativeInstructionsQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Query(q) = match query {
        Ok(query) => query,
        Err(_) => return malformed("invalid instructions query"),
    };
    let handle = match native_resolve_session(&state, &q.session) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    let view = match load_instructions(&state, &handle) {
        Ok(view) => view,
        Err(response) => return response,
    };
    let prompt = q
        .prompt
        .unwrap_or_else(|| handle.title().unwrap_or_default());
    let touched = touched_paths(q.touched.as_deref());
    let active = view.loaded.active_for(&prompt, &touched);
    let mut text = String::new();
    let mut truncated = false;
    let mut sources = Vec::new();
    for instruction in &active {
        let block = format!(
            "\n## {} ({}, loaded: {})\n{}\n",
            instruction.path,
            instruction.reason_loaded,
            instruction.reason_loaded,
            instruction.content
        );
        if text.len().saturating_add(block.len()) > MAX_INSTRUCTION_PREVIEW_BYTES {
            truncated = true;
            break;
        }
        text.push_str(&block);
        sources.push(serde_json::json!({
            "path": instruction.path,
            "source": instruction.source,
            "scope": instruction.scope,
            "hash": instruction.hash,
            "reasonLoaded": instruction.reason_loaded,
        }));
    }
    Json(serde_json::json!({
        "sessionId": handle.id().to_string(),
        "epoch": view.epoch,
        "bundleHash": view.bundle_hash,
        "text": text,
        "bytes": text.len(),
        "truncated": truncated,
        "sources": sources,
        "binding": binding_json(&state, &handle, &view),
    }))
    .into_response()
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeInstructionSourceBody {
    session: String,
    path: String,
    content: String,
    mode: String,
    #[serde(default, rename = "expectedHash")]
    expected_hash: Option<String>,
}

fn normalize_source_path(path: &str, create: bool) -> Result<PathBuf, Response> {
    if path.is_empty() || path.len() > MAX_INSTRUCTION_PATH_BYTES {
        return Err(malformed("instruction path must be 1..=1024 bytes"));
    }
    let normalized = path.replace('\\', "/");
    if normalized.starts_with('/')
        || normalized.contains("..")
        || normalized.chars().any(char::is_control)
    {
        return Err(malformed(
            "instruction path must be relative, inside the workspace, and control-free",
        ));
    }
    let rel = PathBuf::from(&normalized);
    let Some(kind) = kind_for_rel_path(&rel) else {
        return Err(conflict(
            "instruction_source_not_editable",
            "the path is not a known instruction source",
        ));
    };
    if create && kind != RuleSourceKind::FaktorNative {
        return Err(conflict(
            "instruction_source_not_editable",
            "new rules may only be created under .faktor/rules",
        ));
    }
    Ok(rel)
}

fn workspace_root_for(
    state: &AppState,
    handle: &faktor_session::SessionHandle,
) -> Result<PathBuf, Response> {
    state
        .deps
        .session
        .resolve_workspace_root(handle.id())
        .map_err(|e| api_err(&e))?
        .ok_or_else(|| {
            conflict(
                "workspace_root_unavailable",
                "the session has no durable workspace root",
            )
        })
}

/// `PUT /native/instructions/source` — one bounded, atomic instruction edit.
/// `mode=create` requires an absent path; `mode=update` requires the exact
/// content hash the editor read. Running work keeps its captured bundle.
pub(crate) async fn native_instructions_source_put(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Json<NativeInstructionSourceBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(_) => return malformed("invalid instruction source body"),
    };
    let handle = match native_resolve_session(&state, &req.session) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    if req.content.len() > MAX_RULE_BYTES {
        return wire_status(ApiError {
            code: "instruction_oversized",
            message: format!("instruction content exceeds MAX_RULE_BYTES ({MAX_RULE_BYTES})"),
            http_status: 413,
            retryable: false,
        });
    }
    let create = match req.mode.as_str() {
        "create" => true,
        "update" => false,
        _ => return malformed("mode must be create or update"),
    };
    let rel = match normalize_source_path(&req.path, create) {
        Ok(rel) => rel,
        Err(response) => return response,
    };
    let root = match workspace_root_for(&state, &handle) {
        Ok(root) => root,
        Err(response) => return response,
    };
    let rooted = match faktor_fs::rooted::RootedDir::open(&root) {
        Ok(rooted) => rooted,
        Err(e) => return unavailable(format!("workspace root cannot be opened: {e}")),
    };
    let existing = rooted.read(&rel, MAX_RULE_BYTES + 1).ok();
    let (previous_content_hash, previous_content) = match &existing {
        Some(data) => (
            Some(hash_hex(&data.bytes)),
            String::from_utf8_lossy(&data.bytes).to_string(),
        ),
        None => (None, String::new()),
    };
    if create {
        if existing.is_some() {
            return conflict(
                "instruction_source_exists",
                "the instruction source already exists; use update with its current hash",
            );
        }
        if req.expected_hash.is_some() {
            return malformed("create must not carry expectedHash");
        }
    } else {
        let Some(expected) = req.expected_hash.as_deref() else {
            return malformed("update requires expectedHash from the last read");
        };
        let Some(actual) = previous_content_hash.as_deref() else {
            return conflict(
                "instruction_source_missing",
                "the instruction source is gone",
            );
        };
        if expected != actual {
            return conflict(
                "instruction_source_conflict",
                "the instruction source changed since it was read",
            );
        }
    }
    // No-op edit is a true no-op: the same content keeps the same hash/epoch.
    if previous_content == req.content {
        let view = match load_instructions(&state, &handle) {
            Ok(view) => view,
            Err(response) => return response,
        };
        return Json(serde_json::json!({
            "saved": true,
            "changed": false,
            "path": rel.to_string_lossy(),
            "contentHash": previous_content_hash,
            "epoch": view.epoch,
            "bundleHash": view.bundle_hash,
            "applyTiming": binding_json(&state, &handle, &view)["applyTiming"].clone(),
            "binding": binding_json(&state, &handle, &view),
        }))
        .into_response();
    }
    if let Some(parent) = rel.parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(e) = rooted.create_dir_all(parent) {
                return unavailable(format!("cannot create the instruction directory: {e}"));
            }
        }
    }
    let temp = rel.with_file_name(format!(
        ".faktor-tmp-{}-{:016x}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let write = (|| -> Result<(), String> {
        use std::io::Write;
        let mut file = rooted
            .open_create_new(&temp)
            .map_err(|e| format!("create temp: {e}"))?;
        file.write_all(req.content.as_bytes())
            .map_err(|e| format!("write temp: {e}"))?;
        file.sync_all().map_err(|e| format!("sync temp: {e}"))?;
        drop(file);
        rooted
            .atomic_publish(&temp, &rel)
            .map_err(|e| format!("publish: {e}"))
    })();
    if let Err(message) = write {
        let _ = rooted.remove_file(&temp);
        return unavailable(message);
    }
    let view = match load_instructions(&state, &handle) {
        Ok(view) => view,
        Err(response) => return response,
    };
    let binding = binding_json(&state, &handle, &view);
    Json(serde_json::json!({
        "saved": true,
        "changed": true,
        "path": rel.to_string_lossy(),
        "contentHash": hash_hex(req.content.as_bytes()),
        "bytes": req.content.len(),
        "epoch": view.epoch,
        "bundleHash": view.bundle_hash,
        "applyTiming": binding["applyTiming"].clone(),
        "restartRequired": binding["restartRequired"].clone(),
        "binding": binding,
    }))
    .into_response()
}
