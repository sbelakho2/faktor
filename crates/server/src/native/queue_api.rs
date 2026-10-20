//! Durable steering-queue native routes (F064/F084), split out of
//! `session.rs` to stay below the source-size authority.

use super::*;

// ---------------------------------------------------------------- queue

fn queue_row_json(row: &faktor_store::QueuedPrompt) -> serde_json::Value {
    serde_json::json!({
        "queueSeq": row.queue_seq,
        "opId": row.op_id.raw().to_string(),
        "prompt": row.prompt,
        "files": row.files,
        "model": row.model,
        "variant": row.variant,
        "agent": row.agent,
        "status": row.status,
        "requestedAt": row.requested_at,
    })
}

/// `GET /native/session/{id}/queue` — the durable steering queue in FIFO
/// order with each row's exact status (F064/F084).
pub(crate) async fn native_session_queue(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    match handle.queue_rows(MAX_NATIVE_LIST) {
        Ok(rows) => Json(serde_json::json!({
            "sessionId": id,
            "nextStepTiming": "rows admit in queue order after the active turn settles",
            "queue": rows.iter().map(queue_row_json).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => api_err(&e),
    }
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeQueueEditBody {
    pub prompt: String,
    #[serde(default)]
    pub files: Option<Vec<String>>,
}

#[allow(clippy::result_large_err)]
fn parse_op_id(raw: &str) -> Result<faktor_core::id::OpId, Response> {
    raw.parse::<u64>()
        .map(faktor_core::id::OpId::new)
        .map_err(|_| {
            wire_status(ApiError {
                code: "malformed",
                message: format!("operation id {raw:?} is not a u64"),
                http_status: 400,
                retryable: false,
            })
        })
}

/// `PATCH /native/session/{id}/queue/{op}` — edit a still-pending queued
/// prompt. A claimed/running row is a typed 409: steering can never rewrite
/// a turn that already produced effects.
pub(crate) async fn native_session_queue_edit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, op)): Path<(String, String)>,
    body: Result<Json<NativeQueueEditBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(_) => return wire_status(malformed_body("invalid queue edit body")),
    };
    if body.prompt.trim().is_empty() {
        return wire_status(malformed_body("a queued prompt cannot be empty"));
    }
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    let op = match parse_op_id(&op) {
        Ok(op) => op,
        Err(r) => return r,
    };
    let row = match handle.queue_row_by_op(op) {
        Ok(Some(row)) => row,
        Ok(None) => return wire_status(not_found("queued operation")),
        Err(e) => return api_err(&e),
    };
    if row.status != "pending" {
        return crate::native::foundation_ext::conflict(
            "queue_row_not_editable",
            format!(
                "queued operation {op} is already {}; only pending rows can be edited",
                row.status
            ),
        );
    }
    let files = body.files.unwrap_or(row.files);
    match handle.edit_queued_prompt(op, &body.prompt, &files) {
        Ok(true) => match handle.queue_row_by_op(op) {
            Ok(Some(row)) => Json(queue_row_json(&row)).into_response(),
            Ok(None) => wire_status(not_found("queued operation")),
            Err(e) => api_err(&e),
        },
        Ok(false) => crate::native::foundation_ext::conflict(
            "queue_row_not_editable",
            "the row was admitted while it was being edited; the edit was not applied",
        ),
        Err(e) => api_err(&e),
    }
}

/// `DELETE /native/session/{id}/queue/{op}` — remove (cancel) one queued
/// prompt. The row is durably cancelled before the response, so it can never
/// run later.
pub(crate) async fn native_session_queue_remove(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, op)): Path<(String, String)>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    let op = match parse_op_id(&op) {
        Ok(op) => op,
        Err(r) => return r,
    };
    let row = match handle.queue_row_by_op(op) {
        Ok(Some(row)) => row,
        Ok(None) => return wire_status(not_found("queued operation")),
        Err(e) => return api_err(&e),
    };
    if !matches!(row.status.as_str(), "pending" | "claimed" | "running") {
        return crate::native::foundation_ext::conflict(
            "queue_row_not_removable",
            format!("queued operation {op} is already {}", row.status),
        );
    }
    match handle.cancel_queued_ops(&[op]) {
        Ok(removed) => Json(serde_json::json!({
            "removed": removed > 0,
            "opId": op.raw().to_string(),
            "queueSeq": row.queue_seq,
        }))
        .into_response(),
        Err(e) => api_err(&e),
    }
}
