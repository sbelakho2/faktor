//! Native binary-attachment surface (additive, strict): upload ONE
//! `data_base64` payload into the session's durable CAS-backed attachment
//! store, resolve it by digest (metadata and bytes), and the ONE wire
//! admission rule every task-start DTO uses.
//!
//! - Bytes are validated (mime/filename/size bounds), written to the CAS,
//!   and persisted as a typed `AttachmentId { digest, mime, filename, size }`
//!   REFERENCE row BEFORE any task admission; an EXACT-metadata re-upload is
//!   a dedupe hit returning that reference, while identical bytes under a
//!   different mime/filename return their OWN distinct reference (a
//!   rename/re-select is repairable, never silently inherited).
//! - The upload body is bounded by [`MAX_ATTACHMENT_UPLOAD_BYTES`] so its
//!   base64 form plus the JSON envelope stays under the daemon's 10 MiB
//!   request cap (`crate::api::MAX_BODY_BYTES`). The session/CAS ceiling
//!   remains `MAX_ATTACHMENT_BYTES` for programmatic callers.
//! - IMAGE ADMISSION IS MODEL-AWARE: images are admitted (stored) and then
//!   validated at task admission against the CHOSEN model — `vision` must
//!   be advertised, the mime must be a deliverable image type, and the size
//!   must fit the provider's per-image bound. A refusal keeps the draft and
//!   the durable bytes intact; nothing is cleared.
//! - The strict protocol base64 is CANONICAL: `data_base64` is decoded
//!   directly from its own bytes into a single pre-sized destination and
//!   whitespace or any non-canonical form (bad padding, non-zero trailing
//!   bits, foreign alphabet) is a typed 400 — the protocol never accepts a
//!   whitespace-tolerant variant of the canonical bytes.
//! - DOCUMENT ADMISSION IS MODEL-AWARE like images: `application/pdf` and
//!   `text/plain` attachments are admitted (stored) and then validated at
//!   task admission against the CHOSEN model — the provider's
//!   `document_capable` gate, the document MIME allowlist and the per-part
//!   and request-wide byte bounds. Ordinary workspace source files stay on
//!   the repository-context `files` path and are never uploaded blindly.
//! - Hostile DTOs (unknown fields, non-string members, malformed base64,
//!   traversal filenames, hostile mimes, oversized payloads) are typed
//!   400/413s; an unknown digest resolves to a typed 404, never a phantom.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::Engine as _;
use faktor_core::attachment::{validate_mime, AttachmentId, MAX_ATTACHMENT_BYTES};
use faktor_core::error::{Error, ErrorKind};
use faktor_core::hash::FileHash;
use faktor_protocol::error::ApiError;

use super::*;
use crate::api::AppState;

/// Decoded-byte ceiling of one HTTP upload. Base64 inflates by 4/3, so
/// 7 MiB decodes to ~9.33 MiB encoded — with the JSON envelope this stays
/// under the daemon's 10 MiB `MAX_BODY_BYTES` cap. Larger payloads are a
/// typed 413 before any decode; the session/CAS ceiling
/// (`MAX_ATTACHMENT_BYTES`) is unchanged for programmatic callers.
pub const MAX_ATTACHMENT_UPLOAD_BYTES: usize = 7 * 1024 * 1024;

/// Strict request DTO of one native attachment upload. `data_base64` is the
/// standard-alphabet base64 of the raw bytes; unknown members, missing
/// members and non-string values are plain 400s (`deny_unknown_fields`).
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeAttachmentUpload {
    mime: String,
    #[serde(default)]
    filename: Option<String>,
    data_base64: String,
}

/// Map one session-layer attachment refusal onto the wire: the DTO
/// admission class is a client 400 (oversized keeps its distinct code),
/// never a 5xx.
fn admission_error(e: &Error) -> ApiError {
    let code = match e.kind {
        ErrorKind::Oversized => "oversized",
        _ => "malformed",
    };
    ApiError {
        code,
        message: e.message.clone(),
        http_status: 400,
        retryable: false,
    }
}

/// The typed wire refusal for an image the CHOSEN model cannot consume:
/// `unsupported`, 400. The draft and the durable CAS bytes stay intact —
/// admission clears nothing, so the client can retry with a vision model.
fn media_model_unsupported(model: &str, reason: &str) -> ApiError {
    ApiError {
        code: "unsupported",
        message: format!(
            "image attachment cannot be delivered to model {model}: {reason}; the draft and attachment bytes were kept — select a vision model or remove the image"
        ),
        http_status: 400,
        retryable: false,
    }
}

/// The typed wire refusal for a document the CHOSEN model cannot consume
/// (`unsupported`, 400). Mirrors [`media_model_unsupported`]: nothing is
/// cleared, so the client can retry with a document-capable model.
fn document_model_unsupported(model: &str, reason: &str) -> ApiError {
    ApiError {
        code: "unsupported",
        message: format!(
            "document attachment cannot be delivered to model {model}: {reason}; the draft and attachment bytes were kept — select a document-capable model or remove the document"
        ),
        http_status: 400,
        retryable: false,
    }
}

/// The typed 400 refusal for one strict-protocol base64 payload.
fn base64_refusal(message: String) -> ApiError {
    ApiError {
        code: "malformed",
        message,
        http_status: 400,
        retryable: false,
    }
}

/// Decoded length of a CANONICAL standard-alphabet base64 payload — or the
/// typed refusal for whitespace/non-canonical input. ONE allocation-free
/// pass: the strict protocol accepts exactly the canonical encoding
/// (`len % 4 == 0`, alphabet bytes only, at most two trailing `=` with the
/// unused trailing bits zero). Whitespace is refused explicitly (it is the
/// classic tolerant variant) instead of being compacted away.
fn canonical_decoded_len(encoded: &[u8]) -> Result<usize, ApiError> {
    if let Some(offset) = encoded.iter().position(|b| b.is_ascii_whitespace()) {
        return Err(base64_refusal(format!(
            "attachment data_base64 is not canonical standard base64: whitespace at byte {offset} is not allowed"
        )));
    }
    if !encoded.len().is_multiple_of(4) {
        return Err(base64_refusal(format!(
            "attachment data_base64 is not canonical standard base64: {} bytes is not a multiple of 4",
            encoded.len()
        )));
    }
    let mut padding = 0usize;
    let mut last_value = 0u8;
    for (index, byte) in encoded.iter().enumerate() {
        let value = match *byte {
            b'A'..=b'Z' => *byte - b'A',
            b'a'..=b'z' => *byte - b'a' + 26,
            b'0'..=b'9' => *byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => {
                padding += 1;
                if padding > 2 {
                    return Err(base64_refusal(format!(
                        "attachment data_base64 is not canonical standard base64: more than two padding bytes (byte {index})"
                    )));
                }
                continue;
            }
            other => {
                return Err(base64_refusal(format!(
                    "attachment data_base64 is not canonical standard base64: byte {other:#04x} at {index} is not in the standard alphabet"
                )));
            }
        };
        if padding > 0 {
            return Err(base64_refusal(format!(
                "attachment data_base64 is not canonical standard base64: data byte at {index} after padding"
            )));
        }
        last_value = value;
    }
    // Canonical trailing bits: the unused low bits of the last data symbol
    // must be zero (one pad byte leaves 2 bits, two pad bytes leave 4).
    let trailing_mask = match padding {
        0 => 0,
        1 => 0b11,
        _ => 0b1111,
    };
    if trailing_mask != 0 && last_value & trailing_mask != 0 {
        return Err(base64_refusal(format!(
            "attachment data_base64 is not canonical standard base64: non-zero trailing bits with {padding} padding byte(s)"
        )));
    }
    Ok(encoded.len() / 4 * 3 - padding)
}

/// Decode ONE strict-protocol base64 payload DIRECTLY from its own bytes
/// into a single pre-sized bounded destination: exactly one allocation of
/// exactly the decoded length, with no compacted or intermediate copy.
/// Whitespace and every non-canonical form are typed 400 refusals; the
/// caller enforces the encoded/decoded byte ceilings before/after.
///
/// Public because the single-allocation property is certified by an
/// integration test with a counting allocator
/// (`crates/server/tests/attachment_decode_alloc.rs`).
pub fn decode_attachment_base64(encoded: &[u8]) -> Result<Vec<u8>, ApiError> {
    let decoded_len = canonical_decoded_len(encoded)?;
    let mut out = vec![0u8; decoded_len];
    match base64::engine::general_purpose::STANDARD.decode_slice(encoded, &mut out) {
        Ok(written) if written == decoded_len => Ok(out),
        Ok(written) => Err(base64_refusal(format!(
            "attachment data_base64 decoded to {written} bytes, not the canonical {decoded_len}"
        ))),
        Err(e) => Err(base64_refusal(format!(
            "attachment data_base64 is not valid canonical base64: {e}"
        ))),
    }
}

/// The ONE wire admission rule for a task's binary attachment set: bounded
/// count/structural validity/durable byte-identical resolution via the
/// session layer, plus MODEL-AWARE media validation of every image and
/// deliverable document against the chosen model's capabilities. Runs
/// BEFORE any run/task row so a refused start leaves no partial durable
/// admission (and never touches the stored bytes or the composer draft).
///
/// Non-image attachments whose mime is not one of the daemon's deliverable
/// document types (archives, opaque binaries) are NOT model content: they
/// stay CAS-only and are never lowered into a prompt. Ordinary workspace
/// source files arrive on the repository-context `files` path, not here.
pub(crate) fn validate_wire_attachments(
    state: &AppState,
    handle: &faktor_session::SessionHandle,
    model_override: Option<&str>,
    ids: &[AttachmentId],
) -> Result<(), ApiError> {
    handle
        .resolve_attachments(ids)
        .map_err(|e| admission_error(&e))?;
    let images: Vec<&AttachmentId> = ids.iter().filter(|id| id.is_image()).collect();
    let documents: Vec<&AttachmentId> = ids
        .iter()
        .filter(|id| !id.is_image() && faktor_provider::is_supported_document_mime(&id.mime))
        .collect();
    if images.is_empty() && documents.is_empty() {
        return Ok(());
    }
    let provider = handle.provider().map_err(|e| admission_error(&e))?;
    let model = match model_override {
        Some(m) if !m.is_empty() => m.to_string(),
        _ => handle.model().map_err(|e| admission_error(&e))?,
    };
    // Fail closed when the provider is not registered: a fabricated
    // assume-capability default would claim delivery that cannot happen.
    let entry = state
        .deps
        .agent
        .deps()
        .providers
        .get(&provider)
        .ok_or_else(|| ApiError {
            code: "unsupported",
            message: format!(
                "provider {provider:?} of session {} is not registered; cannot validate attachment delivery to model {model}",
                handle.id()
            ),
            http_status: 400,
            retryable: false,
        })?;
    if !images.is_empty() {
        let caps = entry.capabilities(&model);
        if !caps.vision {
            return Err(media_model_unsupported(
                &model,
                "the model does not advertise vision capability",
            ));
        }
        let per_image = entry
            .max_image_bytes()
            .min(faktor_provider::MAX_MEDIA_BYTES_HARD);
        let mut total: u64 = 0;
        for id in images {
            id.validate().map_err(|e| admission_error(&e))?;
            if !faktor_provider::is_supported_image_mime(&id.mime) {
                return Err(media_model_unsupported(
                    &model,
                    &format!(
                        "mime {:?} is not a deliverable image type ({})",
                        id.mime,
                        faktor_provider::SUPPORTED_IMAGE_MIMES.join(", ")
                    ),
                ));
            }
            if id.size > per_image as u64 {
                return Err(ApiError {
                    code: "oversized",
                    message: format!(
                        "image attachment {} is {} bytes, over the {per_image} byte bound of provider {provider:?}",
                        id.digest, id.size
                    ),
                    http_status: 413,
                    retryable: false,
                });
            }
            total = total.saturating_add(id.size);
            if total > faktor_provider::MAX_REQUEST_IMAGE_BYTES as u64 {
                return Err(ApiError {
                    code: "oversized",
                    message: format!(
                        "image attachment set totals {total} bytes, over the {} byte request media bound",
                        faktor_provider::MAX_REQUEST_IMAGE_BYTES
                    ),
                    http_status: 413,
                    retryable: false,
                });
            }
        }
    }
    if !documents.is_empty() {
        if !entry.document_capable(&model) {
            return Err(document_model_unsupported(
                &model,
                "the model does not advertise document input",
            ));
        }
        let per_document = entry
            .max_document_bytes()
            .min(faktor_provider::MAX_MEDIA_BYTES_HARD);
        let mut total: u64 = 0;
        for id in documents {
            id.validate().map_err(|e| admission_error(&e))?;
            if id.size > per_document as u64 {
                return Err(ApiError {
                    code: "oversized",
                    message: format!(
                        "document attachment {} is {} bytes, over the {per_document} byte bound of provider {provider:?}",
                        id.digest, id.size
                    ),
                    http_status: 413,
                    retryable: false,
                });
            }
            total = total.saturating_add(id.size);
            if total > faktor_provider::MAX_REQUEST_DOCUMENT_BYTES as u64 {
                return Err(ApiError {
                    code: "oversized",
                    message: format!(
                        "document attachment set totals {total} bytes, over the {} byte request document bound",
                        faktor_provider::MAX_REQUEST_DOCUMENT_BYTES
                    ),
                    http_status: 413,
                    retryable: false,
                });
            }
        }
    }
    Ok(())
}

/// Parse one digest path segment strictly: 64 lowercase/uppercase hex chars.
fn parse_digest(raw: &str) -> Result<FileHash, ApiError> {
    FileHash::from_hex(raw).ok_or_else(|| ApiError {
        code: "malformed",
        message: format!("{raw:?} is not a 64-char hex BLAKE3 digest"),
        http_status: 400,
        retryable: false,
    })
}

/// `POST /native/session/{id}/attachments` — upload ONE bounded attachment.
/// Returns the durable typed [`AttachmentId`] for THIS exact reference (same
/// digest + mime + filename + size → same id; same bytes with other metadata
/// → that metadata's own reference).
pub(crate) async fn native_attachment_upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Result<Json<NativeAttachmentUpload>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let Json(upload) = match body {
        Ok(b) => b,
        Err(_) => return wire_status(malformed_body("invalid native attachment upload body")),
    };
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    // Canonicalize exactly like the store write: a hostile or non-canonical
    // mime is a typed 400 before any decode/CAS write. Images are STORED
    // here and validated against the chosen model at task admission.
    let mime = upload.mime.trim().to_ascii_lowercase();
    if let Err(e) = validate_mime(&mime) {
        return wire_status(admission_error(&e));
    }
    if let Some(name) = upload.filename.as_deref() {
        if let Err(e) = faktor_core::attachment::validate_filename(name) {
            return wire_status(admission_error(&e));
        }
    }
    // Decode with an explicit ceiling BEFORE materializing: the CANONICAL
    // base64 form is pre-checked (4/3 + padding) and the decoded length is
    // checked twice (bounded form + absolute attachment ceiling). The
    // decoder reads `data_base64`'s own bytes directly into ONE pre-sized
    // destination — no whitespace compaction, no intermediate copy; any
    // whitespace or non-canonical input is a typed 400 refusal.
    let encoded = upload.data_base64.as_bytes();
    let max_encoded = MAX_ATTACHMENT_UPLOAD_BYTES.div_ceil(3) * 4 + 4;
    if encoded.len() > max_encoded {
        return wire_status(ApiError {
            code: "oversized",
            message: format!(
                "attachment upload of {} base64 bytes exceeds the {} byte bound",
                encoded.len(),
                max_encoded
            ),
            http_status: 413,
            retryable: false,
        });
    }
    let bytes = match decode_attachment_base64(encoded) {
        Ok(bytes) => bytes,
        Err(e) => return wire_status(e),
    };
    if bytes.len() > MAX_ATTACHMENT_UPLOAD_BYTES || bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
        return wire_status(ApiError {
            code: "oversized",
            message: format!(
                "attachment of {} bytes exceeds the upload bound ({MAX_ATTACHMENT_UPLOAD_BYTES})",
                bytes.len()
            ),
            http_status: 413,
            retryable: false,
        });
    }
    match handle.put_attachment(&mime, upload.filename.as_deref(), &bytes) {
        Ok(stored) => {
            Json(serde_json::to_value(&stored).unwrap_or(serde_json::Value::Null)).into_response()
        }
        Err(e) => api_err(&e),
    }
}

/// The metadata projection of one resolved attachment (no bytes).
fn attachment_meta(id: &AttachmentId) -> serde_json::Value {
    serde_json::json!({
        "digest": id.digest.to_hex(),
        "mime": id.mime,
        "filename": id.filename,
        "size": id.size,
    })
}

/// `GET /native/session/{id}/attachments/{digest}` — resolve ONE durable
/// attachment row by digest (restart-safe). Unknown digests are typed 404s.
/// When several references share a blob the first-inserted (lowest-id)
/// reference is the deterministic answer (an upload of the exact reference
/// returns that reference directly).
pub(crate) async fn native_attachment_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, digest)): Path<(String, String)>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let hash = match parse_digest(&digest) {
        Ok(h) => h,
        Err(e) => return wire_status(e),
    };
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    match handle.attachment(hash) {
        Ok(Some(stored)) => Json(attachment_meta(&stored)).into_response(),
        Ok(None) => wire_status(not_found(&format!(
            "attachment {digest} in session {}",
            handle.id()
        ))),
        Err(e) => api_err(&e),
    }
}

/// `GET /native/session/{id}/attachments/{digest}/bytes` — the verified
/// bytes of ONE durable attachment (CAS re-hash; corruption is loud).
pub(crate) async fn native_attachment_bytes(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, digest)): Path<(String, String)>,
) -> Response {
    if let Err(e) = authed(&headers, &state) {
        return (StatusCode::UNAUTHORIZED, Json(e.to_json())).into_response();
    }
    let hash = match parse_digest(&digest) {
        Ok(h) => h,
        Err(e) => return wire_status(e),
    };
    let handle = match native_resolve_session(&state, &id) {
        Ok(h) => h,
        Err(r) => return *r,
    };
    let stored = match handle.attachment(hash) {
        Ok(Some(stored)) => stored,
        Ok(None) => {
            return wire_status(not_found(&format!(
                "attachment {digest} in session {}",
                handle.id()
            )))
        }
        Err(e) => return api_err(&e),
    };
    match handle.attachment_bytes(&stored, MAX_ATTACHMENT_UPLOAD_BYTES) {
        Ok(bytes) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, stored.mime.as_str())],
            bytes,
        )
            .into_response(),
        Err(e) => api_err(&e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ServerDeps;
    use faktor_agent::{AgentDeps, AgentRuntime, NoEvidence, ToolCallMode, ToolRegistry};
    use faktor_core::model::ModelCapabilities;
    use faktor_core::time::SystemClock;
    use faktor_provider::{FakeProvider, ProviderRegistry};
    use faktor_session::SessionManager;
    use std::sync::Arc;

    fn test_state(root: &std::path::Path) -> (AppState, faktor_session::SessionHandle) {
        let session = SessionManager::open(root.join("store"), root.join("cas"), true).unwrap();
        let permissions =
            crate::permission::ChannelPermissionRequester::new(std::time::Duration::from_secs(5));
        let mut registry = ProviderRegistry::new();
        registry
            .try_register(Arc::new(
                FakeProvider::new(
                    "fake",
                    ModelCapabilities {
                        vision: true,
                        ..Default::default()
                    },
                )
                .with_documents(),
            ))
            .unwrap();
        registry
            .try_register(Arc::new(FakeProvider::new(
                "novision",
                ModelCapabilities::default(),
            )))
            .unwrap();
        let mut deps = ServerDeps::new(
            session.clone(),
            {
                AgentRuntime::new(AgentDeps {
                    session: session.clone(),
                    providers: Arc::new(registry),
                    chunk_sink: None,
                    permission_requester: permissions.clone(),
                    evidence: Arc::new(NoEvidence),
                    tools: Arc::new(ToolRegistry::new()),
                    cas: None,
                    workspaces: faktor_fs::WorkspaceFileService::new(),
                    edit: None,
                    snapshots: None,
                    sandbox: None,
                    supervisor: None,
                    verification: faktor_agent::VerificationService::disabled(),
                    model: "m".into(),
                    compaction_model: None,
                    compact_at_usage: 0.65,
                    instructions: "i".into(),
                    hooks: None,
                    instructions_resolver: faktor_instructions::no_roots_resolver(),
                    routing: faktor_agent::FixedRoutingPolicy::passthrough(),
                    budgets: Arc::new(faktor_session::NoopBudget),
                    clock: Arc::new(SystemClock),
                    tool_call_mode: ToolCallMode::Native,
                    tool_deadline_ms: 1000,
                    retry_policy: faktor_core::retry::RetryPolicy::default(),
                    semantic: faktor_agent::fallback_semantic_registry(),
                    context_prior: None,
                    efficiency: Default::default(),
                })
                .unwrap()
            },
            permissions,
        )
        .unwrap();
        deps.directory = Some(root.to_string_lossy().into_owned());
        let ws = session.create_workspace(root.to_str().unwrap()).unwrap();
        let created = session
            .create_session(ws, "attachment-test", "fake", "m")
            .unwrap();
        let handle = session.get_session(created.id()).unwrap().unwrap();
        let state = AppState {
            deps: Arc::new(deps),
            auth: Arc::new(std::sync::RwLock::new(None)),
            terminal_events: Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
            next_terminal_event_id: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            ready: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        (state, handle)
    }

    fn auth_headers(state: &AppState) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-faktor-server-password",
            state
                .deps
                .server_password
                .as_str()
                .parse::<axum::http::HeaderValue>()
                .unwrap(),
        );
        headers
    }

    fn upload(mime: &str, filename: Option<&str>, bytes: &[u8]) -> NativeAttachmentUpload {
        NativeAttachmentUpload {
            mime: mime.into(),
            filename: filename.map(str::to_string),
            data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        }
    }

    #[tokio::test]
    async fn upload_resolve_and_reference_identity_roundtrip_over_the_wire() {
        let dir = tempfile::tempdir().unwrap();
        let (state, handle) = test_state(dir.path());
        let sid = handle.id().to_string();
        let headers = auth_headers(&state);
        let first = native_attachment_upload(
            State(state.clone()),
            headers.clone(),
            Path(sid.clone()),
            Ok(Json(upload(
                "application/pdf",
                Some("spec.pdf"),
                b"%PDF-1.4",
            ))),
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        let body = axum::body::to_bytes(first.into_body(), 1 << 20)
            .await
            .unwrap();
        let id: AttachmentId = serde_json::from_slice(&body).unwrap();
        assert_eq!(id.mime, "application/pdf");
        assert_eq!(id.size, 8);
        // Identical bytes under a DIFFERENT filename return a distinct
        // reference with its own metadata (the CAS blob is shared) — a
        // rename/re-select is repairable, never silently inherited.
        let renamed = native_attachment_upload(
            State(state.clone()),
            headers.clone(),
            Path(sid.clone()),
            Ok(Json(upload(
                "application/pdf",
                Some("other.pdf"),
                b"%PDF-1.4",
            ))),
        )
        .await;
        let body = axum::body::to_bytes(renamed.into_body(), 1 << 20)
            .await
            .unwrap();
        let renamed_id: AttachmentId = serde_json::from_slice(&body).unwrap();
        assert_ne!(renamed_id, id, "metadata-distinct references are distinct");
        assert_eq!(renamed_id.digest, id.digest, "the one CAS blob is shared");
        assert_eq!(renamed_id.filename.as_deref(), Some("other.pdf"));
        // An EXACT-metadata re-upload is the only dedupe hit.
        let again = native_attachment_upload(
            State(state.clone()),
            headers.clone(),
            Path(sid.clone()),
            Ok(Json(upload(
                "application/pdf",
                Some("spec.pdf"),
                b"%PDF-1.4",
            ))),
        )
        .await;
        let body = axum::body::to_bytes(again.into_body(), 1 << 20)
            .await
            .unwrap();
        let deduped: AttachmentId = serde_json::from_slice(&body).unwrap();
        assert_eq!(deduped, id);
        // Resolve metadata and bytes by digest (the lowest-id reference is
        // deterministic: the first upload).
        let meta = native_attachment_get(
            State(state.clone()),
            headers.clone(),
            Path((sid.clone(), id.digest.to_hex())),
        )
        .await;
        assert_eq!(meta.status(), StatusCode::OK);
        let bytes = native_attachment_bytes(
            State(state.clone()),
            headers.clone(),
            Path((sid.clone(), id.digest.to_hex())),
        )
        .await;
        assert_eq!(bytes.status(), StatusCode::OK);
        let body = axum::body::to_bytes(bytes.into_body(), 1 << 20)
            .await
            .unwrap();
        assert_eq!(&body[..], b"%PDF-1.4");
        // Unknown digest: typed 404, never a phantom.
        let missing = native_attachment_get(
            State(state.clone()),
            headers.clone(),
            Path((sid.clone(), "0".repeat(64))),
        )
        .await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn hostile_uploads_are_typed_refusals() {
        let dir = tempfile::tempdir().unwrap();
        let (state, handle) = test_state(dir.path());
        let sid = handle.id().to_string();
        let headers = auth_headers(&state);
        let expect_status = |response: Response, status: StatusCode| async move {
            assert_eq!(response.status(), status);
            axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap()
        };
        // Uploads of IMAGES succeed: storage is model-agnostic and the
        // vision/deliverability validation happens at task admission.
        let image = native_attachment_upload(
            State(state.clone()),
            headers.clone(),
            Path(sid.clone()),
            Ok(Json(upload("image/png", Some("shot.png"), b"\x89PNG"))),
        )
        .await;
        assert_eq!(image.status(), StatusCode::OK);
        let body = axum::body::to_bytes(image.into_body(), 1 << 20)
            .await
            .unwrap();
        let image_id: AttachmentId = serde_json::from_slice(&body).unwrap();
        assert_eq!(image_id.mime, "image/png");
        assert!(image_id.is_image());
        // The stored image round-trips over the wire (bytes are reachable).
        let bytes = native_attachment_bytes(
            State(state.clone()),
            headers.clone(),
            Path((sid.clone(), image_id.digest.to_hex())),
        )
        .await;
        assert_eq!(bytes.status(), StatusCode::OK);
        let body = axum::body::to_bytes(bytes.into_body(), 1 << 20)
            .await
            .unwrap();
        assert_eq!(&body[..], b"\x89PNG");
        // Hostile mime, traversal filename and malformed base64 are 400s.
        // (Uppercase is NOT hostile: it is canonicalized to lowercase before
        // validation, exactly like the session write path.)
        for hostile in [
            upload("", None, b"x"),
            upload("text", None, b"x"),
            upload("text/", None, b"x"),
            upload("text/plain/extra", None, b"x"),
            upload("bad mime/type", None, b"x"),
        ] {
            let response = native_attachment_upload(
                State(state.clone()),
                headers.clone(),
                Path(sid.clone()),
                Ok(Json(hostile)),
            )
            .await;
            expect_status(response, StatusCode::BAD_REQUEST).await;
        }
        let traversal = native_attachment_upload(
            State(state.clone()),
            headers.clone(),
            Path(sid.clone()),
            Ok(Json(upload("text/plain", Some("../secrets"), b"x"))),
        )
        .await;
        expect_status(traversal, StatusCode::BAD_REQUEST).await;
        let mut bad_b64 = upload("text/plain", None, b"x");
        bad_b64.data_base64 = "@@@@".into();
        let response = native_attachment_upload(
            State(state.clone()),
            headers.clone(),
            Path(sid.clone()),
            Ok(Json(bad_b64)),
        )
        .await;
        expect_status(response, StatusCode::BAD_REQUEST).await;
        // Oversized payloads are typed 413s BEFORE any CAS write.
        let mut big = upload("application/octet-stream", None, b"");
        big.data_base64 = "A".repeat(MAX_ATTACHMENT_UPLOAD_BYTES.div_ceil(3) * 4 + 8);
        let response = native_attachment_upload(
            State(state.clone()),
            headers.clone(),
            Path(sid.clone()),
            Ok(Json(big)),
        )
        .await;
        expect_status(response, StatusCode::PAYLOAD_TOO_LARGE).await;
        // The hostile attempts left exactly the (lawful) image row behind:
        // nothing durable was created by any malformed payload.
        assert_eq!(handle.list_attachments(16).unwrap(), vec![image_id]);
    }

    #[test]
    fn upload_dto_is_strict() {
        // Unknown and missing members are rejected by serde itself.
        assert!(
            serde_json::from_value::<NativeAttachmentUpload>(serde_json::json!({
                "mime": "text/plain",
                "data_base64": "eA==",
                "extra": 1
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<NativeAttachmentUpload>(serde_json::json!({
                "mime": "text/plain"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<NativeAttachmentUpload>(serde_json::json!({
                "mime": "text/plain",
                "filename": 7,
                "data_base64": "eA=="
            }))
            .is_err()
        );
        let ok: NativeAttachmentUpload = serde_json::from_value(serde_json::json!({
            "mime": "text/plain",
            "data_base64": "eA=="
        }))
        .unwrap();
        assert!(ok.filename.is_none());
    }

    #[test]
    fn wire_admission_is_model_aware_and_keeps_bytes_on_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let (state, handle) = test_state(dir.path());
        // A second session whose provider advertises NO vision.
        let manager = state.deps.session.clone();
        let ws = manager
            .create_workspace(dir.path().to_str().unwrap())
            .unwrap();
        let created = manager
            .create_session(ws, "novision-test", "novision", "m")
            .unwrap();
        let novision = manager.get_session(created.id()).unwrap().unwrap();
        let stored = handle
            .put_attachment("application/pdf", Some("spec.pdf"), b"%PDF")
            .unwrap();
        // A durable byte-identical id admits; an unknown digest is a 400
        // (client error) with the not-found cause preserved.
        validate_wire_attachments(&state, &handle, None, std::slice::from_ref(&stored))
            .expect("durable id admits");
        let unknown = AttachmentId {
            digest: FileHash::from([8; 32]),
            ..stored.clone()
        };
        let err = validate_wire_attachments(&state, &handle, None, &[unknown])
            .expect_err("unknown digest");
        assert_eq!(err.http_status, 400);
        assert!(err.message.contains("not stored"), "{err:?}");
        // An image admits for the vision session, with a model override too.
        let image = handle
            .put_attachment("image/png", None, b"\x89PNG")
            .unwrap();
        validate_wire_attachments(&state, &handle, Some("m"), &[image.clone(), stored.clone()])
            .expect("vision model admits the image");
        // The VISION-LESS session refuses typedly and keeps the draft and
        // the durable bytes: the row still resolves and the bytes are still
        // served. Nothing is cleared by a refusal.
        novision
            .inherit_attachment(&image)
            .expect("inherit for the second session");
        let err = validate_wire_attachments(&state, &novision, None, std::slice::from_ref(&image))
            .expect_err("vision-less model");
        assert_eq!(err.code, "unsupported");
        assert_eq!(err.http_status, 400);
        assert!(err.message.contains("vision"), "{err:?}");
        assert_eq!(
            novision.list_attachments(16).unwrap(),
            vec![image.clone()],
            "the refusal keeps the durable row"
        );
        assert_eq!(
            novision.attachment_bytes(&image, 1 << 20).unwrap(),
            b"\x89PNG",
            "the refusal keeps the durable bytes"
        );
        // An image mime that no provider consumes is refused typedly.
        let svg = handle
            .put_attachment("image/svg+xml", None, b"<svg/>")
            .unwrap();
        let err = validate_wire_attachments(&state, &handle, None, std::slice::from_ref(&svg))
            .expect_err("unsupported image mime");
        assert_eq!(err.code, "unsupported");
        assert!(err.message.contains("image/svg+xml"), "{err:?}");
        // An image over the provider's per-image bound is a typed 413 and is
        // kept durably (upload succeeded; only admission refused).
        let oversized = handle
            .put_attachment(
                "image/png",
                None,
                &vec![0u8; faktor_provider::MAX_MODEL_IMAGE_BYTES + 1],
            )
            .unwrap();
        let err =
            validate_wire_attachments(&state, &handle, None, std::slice::from_ref(&oversized))
                .expect_err("oversized image");
        assert_eq!(err.code, "oversized");
        assert_eq!(err.http_status, 413);
        assert_eq!(
            handle.attachment(oversized.digest).unwrap(),
            Some(oversized.clone())
        );
        // A vision-less model is ALSO refused on the model-override path.
        let err =
            validate_wire_attachments(&state, &novision, Some("m"), std::slice::from_ref(&image))
                .expect_err("override cannot rescue a vision-less provider");
        assert_eq!(err.code, "unsupported");
    }

    #[test]
    fn strict_protocol_base64_is_canonical_and_whitespace_refusing() {
        use base64::engine::general_purpose::STANDARD;
        // Every canonical encoding decodes byte-exact, including the empty
        // payload (the session layer owns the empty-bytes refusal).
        for bytes in [
            b"".as_slice(),
            b"x".as_slice(),
            b"xy".as_slice(),
            b"xyz".as_slice(),
            b"\x00\xff\x10\x7f".as_slice(),
        ] {
            let encoded = STANDARD.encode(bytes);
            assert_eq!(
                decode_attachment_base64(encoded.as_bytes()).unwrap(),
                bytes,
                "canonical payload {encoded:?}"
            );
        }
        let base = STANDARD.encode(b"hello");
        // Whitespace is refused explicitly wherever it appears: the strict
        // protocol never tolerates a compacted variant.
        let mut internal = base.clone();
        internal.insert(2, '\n');
        for hostile in [
            format!("{base}\n"),
            format!("\n{base}"),
            format!(" {base}"),
            format!("{base} "),
            format!("{base}\t"),
            format!("{base}\r\n"),
            internal,
        ] {
            let err = decode_attachment_base64(hostile.as_bytes())
                .expect_err("whitespace must be refused");
            assert_eq!(err.http_status, 400, "{hostile:?}");
            assert_eq!(err.code, "malformed", "{hostile:?}");
            assert!(err.message.contains("whitespace"), "{hostile:?}: {err:?}");
        }
        // Non-canonical forms: bad length, misplaced/short padding, more
        // than two pad bytes, non-zero trailing bits, foreign alphabet.
        let non_canonical = [
            base.trim_end_matches('=').to_string(), // missing padding
            "eA=".to_string(),                      // length not a multiple of 4
            "eA===".to_string(),                    // length not a multiple of 4
            "eA=A".to_string(),                     // data after padding
            "=AAA".to_string(),                     // padding before data
            "====".to_string(),                     // only padding
            "eB==".to_string(),                     // non-zero trailing bits (4-bit)
            "eB=".to_string(),                      // non-zero trailing bits (2-bit)
            "eA-_".to_string(),                     // URL-safe alphabet
            "eA+.".to_string(),                     // stray '.' byte
            "éA==".to_string(),                     // non-ASCII byte
        ];
        for hostile in non_canonical {
            let err = decode_attachment_base64(hostile.as_bytes())
                .expect_err("non-canonical must be refused");
            assert_eq!(err.http_status, 400, "{hostile:?}");
            assert_eq!(err.code, "malformed", "{hostile:?}");
            assert!(
                err.message.contains("not canonical standard base64"),
                "{hostile:?}: {err:?}"
            );
        }
        // Exactly the canonical encoding of the non-zero trailing-bit case
        // still decodes (only the zero-bits variant is canonical).
        assert_eq!(
            decode_attachment_base64(STANDARD.encode(b"xy").as_bytes()).unwrap(),
            b"xy"
        );
    }

    #[tokio::test]
    async fn upload_accepts_documents_and_refuses_non_canonical_base64() {
        let dir = tempfile::tempdir().unwrap();
        let (state, handle) = test_state(dir.path());
        let sid = handle.id().to_string();
        let headers = auth_headers(&state);
        // application/pdf and text/plain uploads are accepted (storage is
        // model-agnostic; the chosen model's document gate runs at start).
        for (mime, name, bytes) in [
            ("application/pdf", "spec.pdf", b"%PDF-1.4".as_slice()),
            ("text/plain", "notes.txt", b"plain text".as_slice()),
        ] {
            let response = native_attachment_upload(
                State(state.clone()),
                headers.clone(),
                Path(sid.clone()),
                Ok(Json(upload(mime, Some(name), bytes))),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK, "{mime}");
            let body = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap();
            let id: AttachmentId = serde_json::from_slice(&body).unwrap();
            assert_eq!(id.mime, mime);
            assert_eq!(id.size, bytes.len() as u64);
            assert!(!id.is_image());
        }
        // A whitespace-padded base64 body is a typed 400 BEFORE any decode
        // or CAS write (the strict protocol has no tolerant variant).
        let mut spaced = upload("text/plain", Some("notes.txt"), b"plain");
        spaced.data_base64.push('\n');
        let response = native_attachment_upload(
            State(state.clone()),
            headers.clone(),
            Path(sid.clone()),
            Ok(Json(spaced)),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        let err: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(err["error"]["code"], "malformed");
        assert!(
            err["error"]["message"]
                .as_str()
                .unwrap()
                .contains("whitespace"),
            "{err:?}"
        );
        let mut non_canonical = upload("text/plain", Some("notes.txt"), b"plain");
        non_canonical.data_base64 = "eB==".into();
        let response = native_attachment_upload(
            State(state.clone()),
            headers.clone(),
            Path(sid.clone()),
            Ok(Json(non_canonical)),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        // The refused payloads left exactly the two lawful document rows.
        assert_eq!(handle.list_attachments(16).unwrap().len(), 2);
    }

    #[test]
    fn wire_admission_validates_documents_against_the_chosen_model() {
        let dir = tempfile::tempdir().unwrap();
        let (state, handle) = test_state(dir.path());
        // A non-document-capable session (registered provider, no documents).
        let manager = state.deps.session.clone();
        let ws = manager
            .create_workspace(dir.path().to_str().unwrap())
            .unwrap();
        let created = manager
            .create_session(ws, "novision-test", "novision", "m")
            .unwrap();
        let novision = manager.get_session(created.id()).unwrap().unwrap();
        // The document-capable session admits PDF and text/plain.
        let pdf = handle
            .put_attachment("application/pdf", Some("spec.pdf"), b"%PDF-1.4")
            .unwrap();
        let text = handle
            .put_attachment("text/plain", Some("notes.txt"), b"plain text")
            .unwrap();
        validate_wire_attachments(&state, &handle, None, &[pdf.clone(), text.clone()])
            .expect("a document-capable model admits pdf + text");
        // A non-document model refuses typedly and keeps everything durable.
        novision
            .inherit_attachment(&pdf)
            .expect("inherit for the second session");
        let err = validate_wire_attachments(&state, &novision, None, std::slice::from_ref(&pdf))
            .expect_err("document-less model");
        assert_eq!(err.code, "unsupported");
        assert_eq!(err.http_status, 400);
        assert!(err.message.contains("document"), "{err:?}");
        assert_eq!(
            novision.list_attachments(16).unwrap(),
            vec![pdf.clone()],
            "the refusal keeps the durable row"
        );
        assert_eq!(
            novision.attachment_bytes(&pdf, 1 << 20).unwrap(),
            b"%PDF-1.4",
            "the refusal keeps the durable bytes"
        );
        // A document over the provider's per-document bound is a typed 413
        // and stays durable (the upload succeeded; only admission refused).
        let oversized = handle
            .put_attachment(
                "application/pdf",
                None,
                &vec![0u8; faktor_provider::MAX_MODEL_DOCUMENT_BYTES + 1],
            )
            .unwrap();
        let err =
            validate_wire_attachments(&state, &handle, None, std::slice::from_ref(&oversized))
                .expect_err("oversized document");
        assert_eq!(err.code, "oversized");
        assert_eq!(err.http_status, 413);
        assert_eq!(
            handle.attachment(oversized.digest).unwrap(),
            Some(oversized.clone())
        );
        // The request-wide document total is bounded: three 6 MiB documents
        // exceed the 16 MiB request bound even though each fits its own.
        let each = 6 * 1024 * 1024;
        let docs: Vec<AttachmentId> = (0..3)
            .map(|i| {
                handle
                    .put_attachment("application/pdf", None, &vec![i as u8; each])
                    .unwrap()
            })
            .collect();
        let err = validate_wire_attachments(&state, &handle, None, &docs)
            .expect_err("request document total");
        assert_eq!(err.code, "oversized");
        assert_eq!(err.http_status, 413);
        assert!(err.message.contains("totals"), "{err:?}");
        // Opaque non-document binaries (archives) are NOT model content:
        // they are never blind-delivered and never document-validated.
        let zip = handle
            .put_attachment("application/zip", Some("src.zip"), b"PK\x03\x04")
            .unwrap();
        novision
            .inherit_attachment(&zip)
            .expect("inherit the archive for the second session");
        validate_wire_attachments(&state, &novision, None, std::slice::from_ref(&zip))
            .expect("opaque binaries stay CAS-only on every model");
    }
}
