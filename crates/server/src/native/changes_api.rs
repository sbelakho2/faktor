//! Native API module split from foundation_ext.rs.

use super::foundation::foundation_error;
use super::foundation_ext::{conflict, foundation, resolve_handle, unavailable};
use super::*;

// ------------------------------------------------------------ change diff

const CHANGE_DIFF_MAX_BYTES: u64 = 512 * 1024;

fn read_change_side(root: &std::path::Path, path: &std::path::Path) -> Result<Vec<u8>, String> {
    use std::path::Component;
    if path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err("change-set path is not a plain relative path".to_string());
    }
    let target = root.join(path);
    match std::fs::metadata(&target) {
        Ok(meta) if meta.len() > CHANGE_DIFF_MAX_BYTES => Err(format!(
            "file exceeds the {CHANGE_DIFF_MAX_BYTES}-byte diff bound"
        )),
        Ok(_) => std::fs::read(&target).map_err(|e| format!("read {}: {e}", target.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("stat {}: {e}", target.display())),
    }
}

/// `GET /native/session/{id}/changes` — the candidate change set with the
/// per-file line hunks the UI can accept individually (F201). The response
/// carries the CONTENT identity; a hunk acceptance must name it, so a stale
/// selection can never land a different candidate.
pub(crate) async fn native_changes(
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
    let Some(shadows) = state.deps.tasks.shadows() else {
        return unavailable("the shadow change-set authority is not wired in this host");
    };
    let row = match shadows.active_shadow(handle.id()) {
        Ok(Some(row)) if row.state.is_live() => row,
        Ok(_) => {
            return wire_status(ApiError {
                code: "no_live_change_set",
                message: "this session has no live candidate change set".into(),
                http_status: 404,
                retryable: false,
            })
        }
        Err(e) => return super::agents::exec_error_response(&e),
    };
    let change_set = match shadows.present_change_set(handle.id()) {
        Ok(change_set) => change_set,
        Err(e) => return super::agents::exec_error_response(&e),
    };
    let base_root = std::path::PathBuf::from(&row.base_root);
    let child_root = std::path::PathBuf::from(&row.root);
    let mut files = Vec::with_capacity(change_set.files.len());
    for entry in &change_set.files {
        let base = read_change_side(&base_root, &entry.path);
        let child = read_change_side(&child_root, &entry.path);
        let (base, child, note) = match (base, child) {
            (Ok(base), Ok(child)) => (base, child, None),
            (Err(base), Err(child)) => (
                Vec::new(),
                Vec::new(),
                Some(format!("base: {base}; candidate: {child}")),
            ),
            (Err(e), _) | (_, Err(e)) => (Vec::new(), Vec::new(), Some(e)),
        };
        let status = match (entry.base_state(), entry.child_state()) {
            (None, Some(_)) => "added",
            (Some(_), None) => "deleted",
            _ => "modified",
        };
        let hunks = if note.is_none() {
            let outcome = faktor_edit::diff::diff_hunks(&base, &child);
            outcome
                .hunks
                .iter()
                .enumerate()
                .map(|(index, hunk)| {
                    let lines: Vec<serde_json::Value> = hunk
                        .lines
                        .iter()
                        .map(|line| match line {
                            faktor_edit::diff::DiffLine::Context(text) => {
                                serde_json::json!({ "kind": "context", "text": text })
                            }
                            faktor_edit::diff::DiffLine::Removed(text) => {
                                serde_json::json!({ "kind": "removed", "text": text })
                            }
                            faktor_edit::diff::DiffLine::Added(text) => {
                                serde_json::json!({ "kind": "added", "text": text })
                            }
                        })
                        .collect();
                    serde_json::json!({
                        "index": index,
                        "oldStart": hunk.old_start,
                        "oldCount": hunk.old_count,
                        "newStart": hunk.new_start,
                        "newCount": hunk.new_count,
                        "lines": lines,
                    })
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        files.push(serde_json::json!({
            "path": entry.path.to_string_lossy(),
            "status": status,
            "baseDigest": entry.base_state().and_then(|s| s.payload_digest_hex()),
            "candidateDigest": entry.child_state().and_then(|s| s.payload_digest_hex()),
            "hunks": hunks,
            "diffUnavailable": note,
            "unified": if note.is_none() {
                let outcome = faktor_edit::diff::diff_hunks(&base, &child);
                Some(faktor_edit::diff::render_unified(&outcome.hunks))
            } else {
                None
            },
        }));
    }
    Json(serde_json::json!({
        "changeSetId": format!("{}-cs", change_set.base_id),
        "candidateIdentity": change_set.id(),
        "baseId": change_set.base_id,
        "childId": change_set.child_id,
        "files": files,
    }))
    .into_response()
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NativeHunkAcceptBody {
    pub candidate_identity: String,
    #[serde(default)]
    pub hunks: std::collections::BTreeMap<String, Vec<usize>>,
    #[serde(default)]
    pub rejected: Vec<String>,
}

/// `POST /native/session/{id}/changes/accept-hunks` — land a per-hunk
/// selection through the candidate rewrite + transactional acceptance
/// authority (F202). The response records the hunk selection, the NEW
/// candidate identity and `requiresReverification: true`.
pub(crate) async fn native_change_accept_hunks(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<NativeHunkAcceptBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid hunk acceptance body")),
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
    let current_identity = change_set.id();
    if body.candidate_identity != current_identity {
        return conflict(
            "candidate_moved",
            format!(
                "the candidate identity changed since the hunks were chosen (expected {}, current {current_identity}); refresh the diff and choose again",
                body.candidate_identity
            ),
        );
    }
    let foundation = match foundation(&state) {
        Ok(f) => f,
        Err(r) => return r,
    };
    let receipt_key = format!("{}-cs-hunks-{}", change_set.base_id, current_identity);
    let digest = faktor_foundation::hash_hex(
        serde_json::json!({
            "candidateIdentity": body.candidate_identity,
            "hunks": body.hunks,
            "rejected": body.rejected,
        })
        .to_string()
        .as_bytes(),
    );
    match foundation
        .session_settings
        .change_receipt(handle.id().raw(), &receipt_key)
    {
        Ok(Some((stored, receipt))) if stored == digest => return Json(receipt).into_response(),
        Ok(Some(_)) => {
            return conflict(
                "hunk_selection_conflict",
                "this candidate identity was already accepted with a different hunk selection",
            )
        }
        Ok(None) => {}
        Err(e) => return foundation_error(e),
    }
    let mut selections: std::collections::BTreeMap<std::path::PathBuf, Vec<usize>> =
        std::collections::BTreeMap::new();
    for (path, indices) in &body.hunks {
        selections.insert(std::path::PathBuf::from(path), indices.clone());
    }
    let rejected: Vec<std::path::PathBuf> =
        body.rejected.iter().map(std::path::PathBuf::from).collect();
    let outcome = match shadows.accept_selected_hunks(handle.id(), &selections, &rejected) {
        Ok(outcome) => outcome,
        Err(e) => return super::agents::exec_error_response(&e),
    };
    let receipt = serde_json::json!({
        "accepted": true,
        "changeSetId": outcome.change_set_id,
        "candidateIdentity": outcome.candidate_identity,
        "selectedHunks": body.hunks,
        "selectedPaths": outcome.selected,
        "candidateFiles": outcome.candidate_files,
        "applied": outcome.applied,
        "alreadyCurrent": outcome.already_current,
        "rejected": body.rejected,
        "integrated": outcome.integrated,
        "proof": {
            "note": "per-hunk acceptance produced a NEW candidate identity; the prior full-candidate verification is stale",
            "requiresReverification": true,
        },
    });
    if let Err(e) = foundation.session_settings.put_change_receipt(
        handle.id().raw(),
        &receipt_key,
        &digest,
        &receipt,
    ) {
        return foundation_error(e);
    }
    Json(receipt).into_response()
}
