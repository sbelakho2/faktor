// TEST-SEPARATION: oracle=external-fixture isolation=isolated-data-dir
//! faktor-provider — the common LLM provider interface hub.
//!
//! The agent depends on this trait; the transport families (ollama, openai,
//! anthropic, google, deepseek, gateway) implement it. Requests pass through:
//!
//! ```text
//! Generic Agent Request
//!         ↓
//! Capability Validation
//!         ↓
//! Provider Normalizer
//!         ↓
//! Wire Serializer   (inside each adapter)
//!         ↓
//! HTTP Transport    (inside each adapter)
//! ```
//!
//! Provider quirks stay inside adapters. There is **no `if provider == "…"`**
//! in the agent — behavior is decided by `ModelCapabilities`.

use std::collections::HashMap;
use std::pin::Pin;

use faktor_core::cancellation::CancellationToken;
use faktor_core::error::{Error, ErrorKind};
use faktor_core::id::{OpId, SessionId};
use faktor_core::model::{CapabilityResolution, ModelCapabilities, PricingSnapshot, ReasoningMode};
use futures::Stream;
#[cfg(test)]
use futures::StreamExt;

use crate::catalog::{ModelCatalogEntry, PricingState, Provenance, QualityPrior};

pub mod catalog;

pub mod classify;
/// Provider configuration hardening: wrapped credentials
/// ([`config::SecretValue`]), validated extra headers
/// ([`config::ExtraHeaders`]) and redacted configuration errors
/// ([`config::ProviderConfigError`]).
pub mod config;
pub mod resolver;
/// Provider error-body sanitization: every adapter passes upstream error
/// text through the shared registered secret scrubber before storing it.
pub mod sanitize;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    System,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ContentPart {
    pub kind: ContentKind,
    /// For tool_result parts: which tool call this answers.
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentKind {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
    },
    /// A URL-shaped image reference (remote URL or `data:` URL). Kept for
    /// callers that already hold a URL; attachment bytes ride
    /// [`ContentKind::ImageData`] instead.
    Image {
        url: String,
    },
    /// Resolved attachment bytes (CAS → memory at request construction):
    /// `mime` is the canonical lowercase media type from the durable
    /// [`faktor_core::attachment::AttachmentId`] row and `data` is the
    /// BOUNDED byte carrier. This part never survives a JSON roundtrip —
    /// see [`MediaBytes`] — so durable task JSON keeps only attachment
    /// ids and every request re-resolves from the CAS.
    ImageData {
        mime: String,
        data: MediaBytes,
    },
    /// Resolved NON-IMAGE DOCUMENT attachment bytes (CAS → memory at request
    /// construction): `mime` is one of [`SUPPORTED_DOCUMENT_MIMES`],
    /// `filename` is the durable row's optional display label (the wire
    /// part's filename), and `data` is the same BOUNDED [`MediaBytes`]
    /// carrier as [`ContentKind::ImageData`] — durable state keeps the
    /// attachment id and every request re-resolves from the CAS. Delivery
    /// is gated on the provider's [`Provider::document_capable`] flag, the
    /// vision-like capability gate for document parts.
    FileData {
        mime: String,
        filename: Option<String>,
        data: MediaBytes,
    },
    ToolCall {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        content: String,
        is_error: bool,
    },
}

impl ContentPart {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            kind: ContentKind::Text { text: text.into() },
            tool_call_id: None,
        }
    }

    /// One resolved image attachment part. `mime` must be a canonical
    /// lowercase `image/*` type and the bytes are bounded by
    /// [`MAX_MEDIA_BYTES_HARD`]; every violation (including zero bytes) is a
    /// typed refusal at CONSTRUCTION (before any provider call).
    pub fn image_data(mime: impl AsRef<str>, bytes: Vec<u8>) -> Result<Self, faktor_core::Error> {
        let mime = mime.as_ref().to_string();
        faktor_core::attachment::validate_mime(&mime)?;
        if !mime.starts_with("image/") {
            return Err(Error::new(
                ErrorKind::Malformed,
                format!("media part mime {mime:?} is not an image/* type"),
            ));
        }
        if bytes.is_empty() {
            return Err(Error::new(
                ErrorKind::Malformed,
                "media part carries zero bytes; an empty image can never be valid",
            ));
        }
        Ok(Self {
            kind: ContentKind::ImageData {
                mime,
                data: MediaBytes::new(bytes)?,
            },
            tool_call_id: None,
        })
    }

    /// One resolved non-image DOCUMENT attachment part (PDF/plain text).
    /// `mime` must be canonical lowercase and one of the
    /// [`SUPPORTED_DOCUMENT_MIMES`]; the optional `filename` is a display
    /// label (never a path) validated with the attachment rules; the bytes
    /// are bounded by [`MAX_MEDIA_BYTES_HARD`]. Every violation (including
    /// zero bytes, an unsupported type and non-UTF-8 `text/plain`) is a
    /// typed refusal at CONSTRUCTION, before any provider call.
    pub fn file_data(
        mime: impl AsRef<str>,
        filename: Option<&str>,
        bytes: Vec<u8>,
    ) -> Result<Self, faktor_core::Error> {
        let mime = mime.as_ref().to_string();
        faktor_core::attachment::validate_mime(&mime)?;
        if !is_supported_document_mime(&mime) {
            return Err(Error::new(
                ErrorKind::Malformed,
                format!(
                    "document part mime {mime:?} is not deliverable (supported: {})",
                    SUPPORTED_DOCUMENT_MIMES.join(", ")
                ),
            ));
        }
        if let Some(name) = filename {
            faktor_core::attachment::validate_filename(name)?;
        }
        if bytes.is_empty() {
            return Err(Error::new(
                ErrorKind::Malformed,
                "document part carries zero bytes; an empty document can never be valid",
            ));
        }
        // `text/plain` is defined as UTF-8 text: a non-UTF-8 payload can
        // never be lowered byte-exactly as a text document by every family,
        // so it is a typed refusal at CONSTRUCTION instead of a silent lossy
        // re-encode inside an adapter.
        if mime == "text/plain" && std::str::from_utf8(&bytes).is_err() {
            return Err(Error::new(
                ErrorKind::Malformed,
                "text/plain document part is not valid UTF-8",
            ));
        }
        Ok(Self {
            kind: ContentKind::FileData {
                mime,
                filename: filename.map(str::to_string),
                data: MediaBytes::new(bytes)?,
            },
            tool_call_id: None,
        })
    }

    pub fn reasoning(text: impl Into<String>) -> Self {
        Self {
            kind: ContentKind::Reasoning { text: text.into() },
            tool_call_id: None,
        }
    }

    pub fn tool_call(
        id: impl Into<String>,
        name: impl Into<String>,
        input: serde_json::Value,
    ) -> Self {
        Self {
            kind: ContentKind::ToolCall {
                id: id.into(),
                name: name.into(),
                input,
            },
            tool_call_id: None,
        }
    }

    pub fn tool_result(
        content: impl Into<String>,
        is_error: bool,
        tool_call_id: impl Into<String>,
    ) -> Self {
        Self {
            kind: ContentKind::ToolResult {
                content: content.into(),
                is_error,
            },
            tool_call_id: Some(tool_call_id.into()),
        }
    }
}

/// Absolute structural ceiling of one [`MediaBytes`] carrier (raw bytes):
/// mirrors the attachment/CAS ceiling, so any resolvable attachment can be
/// carried. Per-provider DELIVERY bounds are tighter
/// ([`Provider::max_image_bytes`], [`MAX_MODEL_IMAGE_BYTES`] default).
pub const MAX_MEDIA_BYTES_HARD: usize = faktor_core::attachment::MAX_ATTACHMENT_BYTES as usize;

/// Daemon-wide DEFAULT ceiling of ONE resolved image part (raw bytes)
/// delivered to a provider. Providers may advertise a tighter or
/// (documented API limits) slightly larger bound via
/// [`Provider::max_image_bytes`]; admission validates the attachment
/// against the CHOSEN provider's value.
pub const MAX_MODEL_IMAGE_BYTES: usize = 5 * 1024 * 1024;

/// Daemon-wide ceiling of ALL resolved image bytes in ONE provider request
/// (the sum across image parts). Bounds the in-memory media of a request
/// even when each individual image is under its provider bound.
pub const MAX_REQUEST_IMAGE_BYTES: usize = 16 * 1024 * 1024;

/// Conservative token estimate of ONE image part for the context budget.
/// Real image tokenization is provider- and resolution-dependent (tiles /
/// patches); the planner charges a fixed upper-bound estimate so an image
/// can never be free in the budget and never scales with byte length.
pub const IMAGE_PART_TOKEN_ESTIMATE: u64 = 4_096;

/// Image media types the daemon will deliver to providers. Deliberately a
/// closed allowlist: SVG (scriptable), BMP/TIFF (rarely accepted) and
/// `image/*`-shaped junk are refused loudly at admission instead of being
/// forwarded to a provider that will reject or mis-handle them.
pub const SUPPORTED_IMAGE_MIMES: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp"];

/// True when `mime` is one of the [`SUPPORTED_IMAGE_MIMES`].
pub fn is_supported_image_mime(mime: &str) -> bool {
    SUPPORTED_IMAGE_MIMES.contains(&mime)
}

/// Non-image DOCUMENT media types the daemon will deliver to providers.
/// Deliberately a closed allowlist mirroring the image one: PDF and plain
/// text are the documented `input_file` / `document` / `inline_data`
/// payloads every supporting family accepts; anything else stays CAS-only
/// (never a silently dropped or renamed part).
pub const SUPPORTED_DOCUMENT_MIMES: &[&str] = &["application/pdf", "text/plain"];

/// True when `mime` is one of the [`SUPPORTED_DOCUMENT_MIMES`].
pub fn is_supported_document_mime(mime: &str) -> bool {
    SUPPORTED_DOCUMENT_MIMES.contains(&mime)
}

/// Daemon-wide DEFAULT ceiling of ONE resolved document part (raw bytes)
/// delivered to a provider. Providers may advertise a tighter or
/// (documented API limits) slightly larger bound via
/// [`Provider::max_document_bytes`]; admission validates the attachment
/// against the CHOSEN provider's value.
pub const MAX_MODEL_DOCUMENT_BYTES: usize = 8 * 1024 * 1024;

/// Daemon-wide ceiling of ALL resolved document bytes in ONE provider
/// request (the sum across document parts). Bounds the in-memory document
/// payload of a request even when each part is under its provider bound.
pub const MAX_REQUEST_DOCUMENT_BYTES: usize = 16 * 1024 * 1024;

/// Conservative token estimate of ONE document part for the context budget.
/// Real document tokenization is provider- and format-dependent (PDF page
/// extraction, text chunking); the planner charges a fixed upper-bound
/// estimate so a document can never be free in the budget.
pub const DOCUMENT_PART_TOKEN_ESTIMATE: u64 = 16_384;

// ------------------------------------------------- attachment admission limits

/// One per-MIME entry of the advertised attachment admission contract: the
/// canonical media type plus the RAW byte ceiling admission enforces for it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AttachmentMimeLimit {
    pub mime: String,
    pub max_bytes: u64,
}

/// The advertised IMAGE attachment admission contract of one provider/model:
/// the closed deliverable MIME allowlist with its per-MIME byte ceiling and
/// the request-wide image byte bound. Clients consume these values instead
/// of mirroring daemon constants.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AttachmentImageLimits {
    pub mimes: Vec<AttachmentMimeLimit>,
    pub max_request_bytes: u64,
}

/// The advertised DOCUMENT attachment admission contract of one
/// provider/model: the model's [`Provider::document_capable`] gate, the
/// closed deliverable MIME allowlist with its per-MIME byte ceiling and the
/// request-wide document byte bound.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AttachmentDocumentLimits {
    pub capable: bool,
    pub mimes: Vec<AttachmentMimeLimit>,
    pub max_request_bytes: u64,
}

/// The complete attachment admission contract the daemon advertises for one
/// provider/model on the native model-catalog surfaces. This is the ONE Rust
/// source of truth clients consume; they keep only a conservative emergency
/// ceiling for the window before the catalog is read.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AttachmentLimits {
    /// Decoded-byte ceiling of ONE HTTP attachment upload
    /// (`faktor_server::api::MAX_BODY_BYTES`-derived).
    pub max_upload_bytes: u64,
    /// Whole HTTP request body ceiling of the daemon.
    pub max_request_bytes: u64,
    /// Structural per-attachment ceiling (session/CAS layer).
    pub max_attachment_bytes: u64,
    pub image: AttachmentImageLimits,
    pub document: AttachmentDocumentLimits,
}

impl AttachmentLimits {
    /// Build the advertised contract for one provider/model from the
    /// daemon's own numbers: the HTTP upload/request ceilings (server
    /// constants) and the provider's per-image/document delivery bounds
    /// (caps by [`MAX_MEDIA_BYTES_HARD`], the structural ceiling every
    /// resolvable attachment fits).
    pub fn for_model(
        max_upload_bytes: u64,
        max_request_bytes: u64,
        provider_max_image_bytes: usize,
        document_capable: bool,
        provider_max_document_bytes: usize,
    ) -> Self {
        let image_bound = provider_max_image_bytes.min(MAX_MEDIA_BYTES_HARD) as u64;
        let document_bound = provider_max_document_bytes.min(MAX_MEDIA_BYTES_HARD) as u64;
        Self {
            max_upload_bytes,
            max_request_bytes,
            max_attachment_bytes: faktor_core::attachment::MAX_ATTACHMENT_BYTES,
            image: AttachmentImageLimits {
                mimes: SUPPORTED_IMAGE_MIMES
                    .iter()
                    .map(|mime| AttachmentMimeLimit {
                        mime: (*mime).to_string(),
                        max_bytes: image_bound,
                    })
                    .collect(),
                max_request_bytes: MAX_REQUEST_IMAGE_BYTES as u64,
            },
            document: AttachmentDocumentLimits {
                capable: document_capable,
                mimes: SUPPORTED_DOCUMENT_MIMES
                    .iter()
                    .map(|mime| AttachmentMimeLimit {
                        mime: (*mime).to_string(),
                        max_bytes: document_bound,
                    })
                    .collect(),
                max_request_bytes: MAX_REQUEST_DOCUMENT_BYTES as u64,
            },
        }
    }
}

/// Resolved attachment bytes carried by [`ContentKind::ImageData`].
///
/// This is the media part's bounded byte carrier:
///
/// - construction enforces [`MAX_MEDIA_BYTES_HARD`];
/// - [`serde::Serialize`] writes only `{"digest", "size"}` — expanded
///   bytes NEVER travel through JSON, so durable task state keeps
///   attachment ids and every request re-resolves from the CAS. The
///   content digest keeps the JSON form content-sensitive for prompt
///   accounting;
/// - [`serde::Deserialize`] is a typed refusal: a JSON document can never
///   smuggle expanded bytes back into a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaBytes(Vec<u8>);

impl MediaBytes {
    /// Wrap resolved bytes, enforcing [`MAX_MEDIA_BYTES_HARD`] (typed
    /// `Oversized`, never a truncation). Per-provider delivery bounds are
    /// enforced later by [`validate_media_delivery`].
    pub fn new(bytes: Vec<u8>) -> Result<Self, Error> {
        if bytes.len() > MAX_MEDIA_BYTES_HARD {
            return Err(Error::new(
                ErrorKind::Oversized,
                format!(
                    "resolved media of {} bytes exceeds MAX_MEDIA_BYTES_HARD ({MAX_MEDIA_BYTES_HARD})",
                    bytes.len()
                ),
            ));
        }
        Ok(Self(bytes))
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// BLAKE3 address of the bytes (the attachment digest, as carried by
    /// the durable row this part was resolved from).
    pub fn digest(&self) -> faktor_core::hash::FileHash {
        faktor_core::hash::FileHash::from(*blake3::hash(&self.0).as_bytes())
    }

    /// Standard-alphabet base64 of the raw bytes (adapter lowering).
    pub fn to_base64(&self) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(&self.0)
    }

    /// `data:<mime>;base64,<b64>` URL (OpenAI-compatible wires).
    pub fn to_data_url(&self, mime: &str) -> String {
        format!("data:{mime};base64,{}", self.to_base64())
    }
}

impl serde::Serialize for MediaBytes {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct as _;
        let mut st = s.serialize_struct("MediaBytes", 2)?;
        st.serialize_field("digest", &self.digest().to_hex())?;
        st.serialize_field("size", &self.0.len())?;
        st.end()
    }
}

impl<'de> serde::Deserialize<'de> for MediaBytes {
    fn deserialize<D: serde::Deserializer<'de>>(_d: D) -> Result<Self, D::Error> {
        Err(serde::de::Error::custom(
            "resolved media bytes never travel through JSON: durable state stores attachment ids and requests re-resolve them from the CAS",
        ))
    }
}

/// Defense-in-depth delivery gate every adapter runs BEFORE lowering wire
/// bytes: `caps.vision` must be advertised, the mime must be one of the
/// [`SUPPORTED_IMAGE_MIMES`], and the raw bytes must fit the provider's own
/// per-image bound ([`Provider::max_image_bytes`], itself capped by the
/// structural [`MAX_MEDIA_BYTES_HARD`]). The agent's
/// [`CapabilityValidator`] already refuses vision-less requests at request
/// construction; adapters re-check so a directly-constructed or hostile
/// request can never leak an image to a wire that would reject it — or,
/// worse, silently drop it.
pub fn validate_media_delivery(
    req: &GenericAgentRequest,
    caps: &ModelCapabilities,
    max_image_bytes: usize,
) -> Result<(), ProviderError> {
    let bound = max_image_bytes.min(MAX_MEDIA_BYTES_HARD);
    for m in &req.messages {
        for part in &m.content {
            let ContentKind::ImageData { mime, data } = &part.kind else {
                continue;
            };
            if !caps.vision {
                return Err(ProviderError::new(
                    ProviderErrorKind::BadRequest,
                    format!(
                        "model {} does not support vision; refusing to lower a resolved image part",
                        req.model
                    ),
                ));
            }
            if !is_supported_image_mime(mime) {
                return Err(ProviderError::new(
                    ProviderErrorKind::BadRequest,
                    format!(
                        "image mime {mime:?} is not deliverable (supported: {})",
                        SUPPORTED_IMAGE_MIMES.join(", ")
                    ),
                ));
            }
            if data.len() > bound {
                return Err(ProviderError::new(
                    ProviderErrorKind::BadRequest,
                    format!(
                        "resolved image of {} bytes exceeds the provider bound ({bound}); refusing to lower it",
                        data.len()
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Defense-in-depth delivery gate every adapter runs BEFORE lowering wire
/// bytes: the provider's own [`Provider::document_capable`] flag must be
/// advertised for the model, the mime must be one of the
/// [`SUPPORTED_DOCUMENT_MIMES`], the raw bytes must fit the provider's own
/// per-document bound ([`Provider::max_document_bytes`], itself capped by
/// the structural [`MAX_MEDIA_BYTES_HARD`]), and the request-wide document
/// total must fit [`MAX_REQUEST_DOCUMENT_BYTES`]. The agent's document gate
/// already refuses document-less requests at request construction; adapters
/// re-check so a directly-constructed or hostile request can never leak a
/// document to a wire that would reject it — or silently drop it.
pub fn validate_document_delivery(
    req: &GenericAgentRequest,
    document_capable: bool,
    max_document_bytes: usize,
) -> Result<(), ProviderError> {
    let bound = max_document_bytes.min(MAX_MEDIA_BYTES_HARD);
    let mut total: u64 = 0;
    for m in &req.messages {
        for part in &m.content {
            let ContentKind::FileData { mime, data, .. } = &part.kind else {
                continue;
            };
            if !document_capable {
                return Err(ProviderError::new(
                    ProviderErrorKind::BadRequest,
                    format!(
                        "model {} does not support document input; refusing to lower a resolved document part",
                        req.model
                    ),
                ));
            }
            if !is_supported_document_mime(mime) {
                return Err(ProviderError::new(
                    ProviderErrorKind::BadRequest,
                    format!(
                        "document mime {mime:?} is not deliverable (supported: {})",
                        SUPPORTED_DOCUMENT_MIMES.join(", ")
                    ),
                ));
            }
            if data.len() > bound {
                return Err(ProviderError::new(
                    ProviderErrorKind::BadRequest,
                    format!(
                        "resolved document of {} bytes exceeds the provider bound ({bound}); refusing to lower it",
                        data.len()
                    ),
                ));
            }
            total = total.saturating_add(data.len() as u64);
            if total > MAX_REQUEST_DOCUMENT_BYTES as u64 {
                return Err(ProviderError::new(
                    ProviderErrorKind::BadRequest,
                    format!(
                        "resolved document set totals {total} bytes, exceeding the request bound ({MAX_REQUEST_DOCUMENT_BYTES}); refusing to lower it"
                    ),
                ));
            }
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ embeddings

/// Hard bound on the number of inputs of ONE embedding request. The CLI's
/// configured embedder batches larger corpora into several requests, so a
/// provider never receives an unbounded batch.
pub const MAX_EMBEDDING_INPUTS: usize = 64;
/// Hard bound on ONE embedding input (bytes).
pub const MAX_EMBEDDING_INPUT_BYTES: usize = 16 * 1024;
/// Hard bound on the summed input bytes of ONE embedding request.
pub const MAX_EMBEDDING_TOTAL_INPUT_BYTES: usize = 256 * 1024;
/// Hard bound on one embedding vector's dimensions.
pub const MAX_EMBEDDING_DIMENSIONS: usize = 8_192;

/// One provider-agnostic embedding request: the embedding MODEL and the
/// ordered inputs to embed, plus the OPTIONAL operation [`RequestMeta`] that
/// lineage (operation id, session id, deadline) flows through — exactly like
/// a chat request. Construction validates every bound (empty inputs,
/// empty/oversized members, oversized totals and an empty model are typed
/// refusals BEFORE any provider call) — a hostile caller can never hand a
/// provider an unbounded batch. `meta` is caller context, not part of the
/// request's identity: equality compares model + inputs only (so retry
/// scripts written before deadlines existed keep matching).
#[derive(Debug, Clone)]
pub struct EmbeddingRequest {
    pub model: String,
    pub inputs: Vec<String>,
    /// Operation lineage (deadline in ms, op/session ids) when the caller
    /// has one. `None` = legacy caller: adapters fall back to their own
    /// transport bounds.
    pub meta: Option<RequestMeta>,
}

impl PartialEq for EmbeddingRequest {
    fn eq(&self, other: &Self) -> bool {
        self.model == other.model && self.inputs == other.inputs
    }
}

impl Eq for EmbeddingRequest {}

impl EmbeddingRequest {
    /// The operation deadline in ms remaining when this request was built;
    /// `0` = no operation-level bound (adapter defaults apply).
    pub fn deadline_ms(&self) -> u64 {
        self.meta.as_ref().map(|m| m.deadline_ms).unwrap_or(0)
    }

    /// Attach operation lineage (deadline/op/session ids) additively.
    pub fn with_meta(mut self, meta: RequestMeta) -> Self {
        self.meta = Some(meta);
        self
    }

    pub fn new(model: impl Into<String>, inputs: Vec<String>) -> Result<Self, faktor_core::Error> {
        let model = model.into();
        if model.is_empty() {
            return Err(Error::new(ErrorKind::Malformed, "embedding model is empty"));
        }
        if inputs.is_empty() {
            return Err(Error::new(
                ErrorKind::Malformed,
                "embedding request carries no inputs",
            ));
        }
        if inputs.len() > MAX_EMBEDDING_INPUTS {
            return Err(Error::new(
                ErrorKind::Oversized,
                format!(
                    "embedding request carries {} inputs, over the cap of {MAX_EMBEDDING_INPUTS}",
                    inputs.len()
                ),
            ));
        }
        let mut total: usize = 0;
        for input in &inputs {
            if input.is_empty() {
                return Err(Error::new(ErrorKind::Malformed, "embedding input is empty"));
            }
            if input.len() > MAX_EMBEDDING_INPUT_BYTES {
                return Err(Error::new(
                    ErrorKind::Oversized,
                    format!(
                        "embedding input of {} bytes exceeds MAX_EMBEDDING_INPUT_BYTES ({MAX_EMBEDDING_INPUT_BYTES})",
                        input.len()
                    ),
                ));
            }
            total = total.saturating_add(input.len());
        }
        if total > MAX_EMBEDDING_TOTAL_INPUT_BYTES {
            return Err(Error::new(
                ErrorKind::Oversized,
                format!(
                    "embedding inputs total {total} bytes, over the cap of {MAX_EMBEDDING_TOTAL_INPUT_BYTES}"
                ),
            ));
        }
        Ok(Self {
            model,
            inputs,
            meta: None,
        })
    }
}

/// One provider-agnostic embedding response: exactly one finite vector per
/// request input, all of the same non-zero dimension.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingResponse {
    pub vectors: Vec<Vec<f32>>,
}

impl EmbeddingResponse {
    pub fn new(vectors: Vec<Vec<f32>>) -> Result<Self, ProviderError> {
        let response = Self { vectors };
        response.validate()?;
        Ok(response)
    }

    /// The response's own structural invariants: non-empty, uniform,
    /// bounded and finite vectors. A hostile/truncated response is a typed
    /// `Malformed` error, never a silent zero vector downstream.
    pub fn validate(&self) -> Result<(), ProviderError> {
        let Some(first) = self.vectors.first() else {
            return Err(ProviderError::new(
                ProviderErrorKind::Malformed,
                "embedding response carries no vectors",
            ));
        };
        let dim = first.len();
        if dim == 0 {
            return Err(ProviderError::new(
                ProviderErrorKind::Malformed,
                "embedding response carries a zero-length vector",
            ));
        }
        if dim > MAX_EMBEDDING_DIMENSIONS {
            return Err(ProviderError::new(
                ProviderErrorKind::Malformed,
                format!(
                    "embedding vector of {dim} dimensions exceeds MAX_EMBEDDING_DIMENSIONS ({MAX_EMBEDDING_DIMENSIONS})"
                ),
            ));
        }
        for vector in &self.vectors {
            if vector.len() != dim {
                return Err(ProviderError::new(
                    ProviderErrorKind::Malformed,
                    format!(
                        "embedding response mixes dimensions ({} vs {dim})",
                        vector.len()
                    ),
                ));
            }
            if vector.iter().any(|v| !v.is_finite()) {
                return Err(ProviderError::new(
                    ProviderErrorKind::Malformed,
                    "embedding response carries a non-finite component",
                ));
            }
        }
        Ok(())
    }

    /// Validate the response against the request's input count: the provider
    /// must return exactly one vector per input, in input order.
    pub fn validate_for(&self, inputs: usize) -> Result<(), ProviderError> {
        self.validate()?;
        if self.vectors.len() != inputs {
            return Err(ProviderError::new(
                ProviderErrorKind::Malformed,
                format!(
                    "embedding response carries {} vectors for {inputs} inputs",
                    self.vectors.len()
                ),
            ));
        }
        Ok(())
    }
}

/// A one-frame stream carrying a typed refusal produced BEFORE any wire
/// byte was sent (adapter delivery gates). The error is terminal: nothing
/// was attempted, so nothing may be retried on the same request.
pub fn provider_error_stream(err: ProviderError) -> ProviderStream {
    Box::pin(futures::stream::once(async move { Err(err) }))
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RequestMessage {
    pub role: Role,
    pub content: Vec<ContentPart>,
}

/// Serde default for every [`ToolEffects`] axis: TRUE. A missing axis (or a
/// whole missing `effects` object on an old serialized [`ToolSpec`]) decodes
/// as MUTATING — unknown effects grant no conversational authority.
fn default_effect_true() -> bool {
    true
}

/// Explicit EFFECT metadata for a tool — the AUTHORITY axis of the
/// three-concept separation (audit item 9):
///
/// - `faktor_core::resource::ResourceClass` is a SCHEDULING / CONCURRENCY
///   class only: it decides how many operations of a kind may run at once.
///   It is NOT an effect system and grants NO authority. A `Network`-class
///   tool that mutates the external world must never pass an authority
///   filter by virtue of its scheduling class.
/// - [`ToolEffects`] (this type) is AUTHORITY: what an Ask (conversational)
///   turn may run. Every axis a tool can affect is DECLARED from the tool's
///   actual implementation behavior; anything not explicitly declared
///   read-only is treated as mutating.
/// - Capabilities (`CapabilitySet` / the permission hop) are PERMISSION:
///   the session/sandbox envelope that gates every executed call.
///
/// The serde default is deliberately CONSERVATIVE on every axis: old
/// serialized tool specs (JSON without `effects`) decode as all-mutating
/// and are refused on conversational turns — unknown ⇒ mutating.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolEffects {
    /// Reads workspace files/state. Read axes never disqualify a tool from
    /// conversational authority.
    #[serde(default = "default_effect_true")]
    pub workspace_read: bool,
    /// Creates, updates or deletes workspace files.
    #[serde(default = "default_effect_true")]
    pub workspace_write: bool,
    /// Spawns an arbitrary child process or shell.
    #[serde(default = "default_effect_true")]
    pub process_exec: bool,
    /// Reads from the network (ingress). Read axes never disqualify a tool
    /// from conversational authority.
    #[serde(default = "default_effect_true")]
    pub network_read: bool,
    /// Sends data or requests out over the network (egress).
    #[serde(default = "default_effect_true")]
    pub network_write: bool,
    /// Mutates durable state outside the workspace/git/network axes —
    /// external services, marketplaces, approval queues, coordination
    /// boards and any other out-of-model store.
    #[serde(default = "default_effect_true")]
    pub external_mutation: bool,
    /// Mutates git state (commits, branches, index, history).
    #[serde(default = "default_effect_true")]
    pub git_mutation: bool,
}

impl ToolEffects {
    /// Every axis declared mutating/unknown. This is both the serde decode
    /// default for absent axes and the shape old serialized specs decode
    /// to; an effects-free tool is NEVER conversational-authority-clean.
    pub const CONSERVATIVE: Self = Self {
        workspace_read: true,
        workspace_write: true,
        process_exec: true,
        network_read: true,
        network_write: true,
        external_mutation: true,
        git_mutation: true,
    };

    /// Declared read-only: reads the workspace, mutates nothing on any
    /// mutating axis.
    pub const READ_ONLY: Self = Self {
        workspace_read: true,
        workspace_write: false,
        process_exec: false,
        network_read: false,
        network_write: false,
        external_mutation: false,
        git_mutation: false,
    };

    /// Declared read-only with network INGRESS only (e.g. a fetch /
    /// read-only acquisition tool): no workspace access, no mutation on any
    /// mutating axis.
    pub const NETWORK_READ_ONLY: Self = Self {
        workspace_read: false,
        workspace_write: false,
        process_exec: false,
        network_read: true,
        network_write: false,
        external_mutation: false,
        git_mutation: false,
    };

    /// The AUTHORITY predicate: TRUE only when the tool is declared
    /// read-only on EVERY mutating axis (`workspace_write`, `process_exec`,
    /// `network_write`, `external_mutation`, `git_mutation` all false).
    /// Read axes (`workspace_read`, `network_read`) never disqualify a
    /// tool: a conversational turn may read the workspace or fetch
    /// read-only data. Anything undeclared decays to `true` per axis via
    /// the serde default, so it fails this predicate.
    pub fn is_read_only(&self) -> bool {
        !(self.workspace_write
            || self.process_exec
            || self.network_write
            || self.external_mutation
            || self.git_mutation)
    }
}

impl Default for ToolEffects {
    fn default() -> Self {
        Self::CONSERVATIVE
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    /// The tool's AUTHORITY metadata (see [`ToolEffects`]): what an Ask may
    /// run, decided from declared effects — never from the scheduling
    /// `ResourceClass`. The serde default is the CONSERVATIVE shape, so old
    /// serialized specs (JSON without `effects`) decode as mutating and are
    /// refused on conversational turns.
    #[serde(default)]
    pub effects: ToolEffects,
}

/// Metadata attached to every request. The wire serializer never sees these
/// fields — they exist for retry state, deadlines, and circuit breakers.
#[derive(Debug, Clone)]
pub struct RequestMeta {
    pub operation_id: OpId,
    pub session_id: SessionId,
    pub provider: String,
    pub attempt: u32,
    /// The operation deadline in ms remaining when this request was built
    /// (audit round 15). Adapters honor it as the stream's OVERALL bound:
    /// `overall_ms = min(deadline_ms, transport::PROVIDER_CEILING_MS)`.
    /// `0` means "no operation-level overall bound" (the transport's
    /// first-byte/idle defaults still apply).
    pub deadline_ms: u64,
    pub cancellation: CancellationToken,
}

/// A normalized agent-level request. Capability validation happens on this
/// type; normalization turns it into wire shapes inside adapters.
#[derive(Debug, Clone)]
pub struct GenericAgentRequest {
    pub model: String,
    /// Cacheable prefix (system instructions, tools, project rules, task state).
    pub system: String,
    pub messages: Vec<RequestMessage>,
    pub tools: Vec<ToolSpec>,
    pub max_output: Option<usize>,
    pub reasoning: Option<ReasoningMode>,
    pub stream: bool,
    pub meta: RequestMeta,
}

/// Numerator of the Faktor-side output-ceiling safety factor.
pub const OUTPUT_LIMIT_SAFETY_NUMERATOR: u64 = 3;
/// Denominator of the Faktor-side output-ceiling safety factor.
pub const OUTPUT_LIMIT_SAFETY_DENOMINATOR: u64 = 2;
/// Flat token headroom added on top of the scaled resolved cap.
pub const OUTPUT_LIMIT_HEADROOM_TOKENS: u64 = 256;
/// Absolute-byte ceiling expressed per budgeted token.
pub const OUTPUT_LIMIT_BYTES_PER_TOKEN: u64 = 8;
/// Floor of the absolute-byte ceiling (small models still get a usable bound).
pub const OUTPUT_LIMIT_MIN_BYTES: u64 = 64 * 1024;

/// The hard local envelope of ONE physical provider attempt, derived from
/// the resolved wire `max_output`:
///
/// ```text
/// token_ceiling = ceil(max_output * 3 / 2) + 256
/// byte_ceiling  = max(token_ceiling * 8, 64 KiB)
/// ```
///
/// The `3/2` factor and `256` headroom absorb tokenizer disagreement and
/// per-message structural tokens; the byte ceiling independently bounds
/// multi-byte UTF-8 / JSON payload bytes. This type lives in the provider
/// crate because BOTH the agent (cumulative stream accounting) and the
/// adapters (their own tool-argument accumulators) enforce the same
/// envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputEnvelope {
    /// The resolved `max_output` the wire request carried (never zero).
    pub resolved_max_output: u64,
    /// Hard cumulative token ceiling (`3/2` cap + headroom).
    pub token_ceiling: u64,
    /// Hard cumulative byte ceiling (`8` bytes/token, `64 KiB` floor).
    pub byte_ceiling: u64,
}

impl OutputEnvelope {
    /// Derive the attempt envelope from the resolved wire cap. A zero cap is
    /// raised to one token: an unbounded stream is never a valid envelope.
    pub fn from_max_output(max_output: usize) -> Self {
        let resolved_max_output = u64::try_from(max_output).unwrap_or(u64::MAX).max(1);
        let token_ceiling = resolved_max_output
            .saturating_mul(OUTPUT_LIMIT_SAFETY_NUMERATOR)
            .div_ceil(OUTPUT_LIMIT_SAFETY_DENOMINATOR)
            .saturating_add(OUTPUT_LIMIT_HEADROOM_TOKENS);
        let byte_ceiling = token_ceiling
            .saturating_mul(OUTPUT_LIMIT_BYTES_PER_TOKEN)
            .max(OUTPUT_LIMIT_MIN_BYTES);
        Self {
            resolved_max_output,
            token_ceiling,
            byte_ceiling,
        }
    }

    /// Derive from the request's resolved cap, falling back to the model
    /// profile's cap when a caller somehow built a request without one.
    pub fn from_request(max_output: Option<usize>, fallback_max_output: usize) -> Self {
        Self::from_max_output(max_output.unwrap_or(fallback_max_output))
    }
}

/// The surface a provider-reported cost rode in on.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportedCostSource {
    /// The usage envelope of the provider's own response payload.
    ProviderUsage,
    /// A provider billing header on the response.
    ProviderBillingHeader,
    /// A provider reconciliation/billing surface (usage endpoint, invoice).
    ProviderReconciliation,
}

/// The currency of a [`ReportedCost`] amount. Only USD-compatible values
/// may override route-snapshot estimation — adapters always label what they
/// forward and the runtime keeps the refusal rule for anything else.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportedCurrency {
    Usd,
    Other { code: String },
}

/// A provider-reported cost with its currency and provenance. `micro_usd`
/// is denominated in `currency` (named `micro_usd` because the value is a
/// micro-unit amount; the field's meaning is "micro units of `currency`" —
/// in practice every adapter that reports a cost today reports USD).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReportedCost {
    pub micro_usd: u64,
    pub currency: ReportedCurrency,
    pub source: ReportedCostSource,
    pub request_id: Option<String>,
}

impl ReportedCost {
    /// An authoritative USD cost reported on the wire.
    pub fn usd(micro_usd: u64, source: ReportedCostSource) -> Self {
        Self {
            micro_usd,
            currency: ReportedCurrency::Usd,
            source,
            request_id: None,
        }
    }

    /// True only for USD-compatible amounts: only these are authoritative
    /// overrides of route-snapshot estimation.
    pub fn is_usd(&self) -> bool {
        self.currency == ReportedCurrency::Usd
    }
}

/// Why a canonical usage split was refused: the wire row is impossible
/// (a cache line larger than the input total it must be a subset of, or an
/// informational reasoning subset larger than the output total). Adapters
/// surface this as a typed [`ProviderErrorKind::Malformed`] stream error —
/// never a silent zero and never a panic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageSplitError {
    CacheReadsExceedInput {
        total_input_tokens: u64,
        cache_read_tokens: u64,
    },
    CacheWritesExceedInput {
        total_input_tokens: u64,
        cache_write_tokens: u64,
    },
    ReasoningExceedsOutput {
        output_tokens: u64,
        reasoning_tokens: u64,
    },
}

impl std::fmt::Display for UsageSplitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UsageSplitError::CacheReadsExceedInput {
                total_input_tokens,
                cache_read_tokens,
            } => write!(
                f,
                "cache read tokens {cache_read_tokens} exceed the reported input total {total_input_tokens}"
            ),
            UsageSplitError::CacheWritesExceedInput {
                total_input_tokens,
                cache_write_tokens,
            } => write!(
                f,
                "cache write tokens {cache_write_tokens} exceed the reported input total {total_input_tokens}"
            ),
            UsageSplitError::ReasoningExceedsOutput {
                output_tokens,
                reasoning_tokens,
            } => write!(
                f,
                "reasoning tokens {reasoning_tokens} exceed the reported output total {output_tokens}"
            ),
        }
    }
}

/// ONE canonical usage frame (audit Phase-1 item C): non-overlapping token
/// categories every provider wire is mapped into at the adapter boundary,
/// so the runtime never has to guess what `tokens_in` meant on a given
/// wire. The old `tokens_in`/`tokens_out` pair is gone — it meant different
/// things on different wires (one provider folds cached input into its
/// input total, another excludes it) and the runtime double-billed cache
/// reads.
///
/// Category contract:
/// - `uncached_input_tokens` NEVER contains cache reads or cache writes.
///   For wires whose input total INCLUDES the cached portion the adapter
///   must split it out ([`CanonicalUsage::from_total_including_cache`]);
///   wires that already report the uncached remainder map as-is.
/// - `cache_read_tokens` / `cache_write_tokens` are purely additive lines
///   priced at their own frozen route-time quote lines.
/// - `output_tokens` already includes reasoning tokens whenever the
///   provider's output charge does; `reasoning_tokens` is an INFORMATIONAL
///   subset that is never billed a second time (no adapter below has a
///   separately-priced reasoning line).
/// - `reported_cost`: the provider-reported authoritative cost when the
///   wire carries one, WITH its currency and source (only USD-compatible
///   values may override route-snapshot estimation; the runtime refuses
///   the rest).
/// - `request_id`: preserved from the wire frame when it carries one.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CanonicalUsage {
    #[serde(default)]
    pub uncached_input_tokens: u64,
    #[serde(default)]
    pub cache_read_tokens: u64,
    #[serde(default)]
    pub cache_write_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: u64,
    #[serde(default)]
    pub reported_cost: Option<ReportedCost>,
    #[serde(default)]
    pub request_id: Option<String>,
}

impl Default for CanonicalUsage {
    fn default() -> Self {
        Self::ZERO
    }
}

impl CanonicalUsage {
    pub const ZERO: Self = Self {
        uncached_input_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        output_tokens: 0,
        reasoning_tokens: 0,
        reported_cost: None,
        request_id: None,
    };

    /// The four priced token categories (reasoning folds into output at
    /// settlement — the informational subset stays zero here).
    pub fn new(
        uncached_input_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        output_tokens: u64,
    ) -> Self {
        Self {
            uncached_input_tokens,
            cache_read_tokens,
            cache_write_tokens,
            output_tokens,
            ..Self::ZERO
        }
    }

    /// Canonicalize a wire whose input TOTAL already includes its cached
    /// portion (openai `prompt_tokens`, gemini `promptTokenCount`): the
    /// uncached remainder is `total - cache reads - cache writes`. A
    /// hostile row whose cache lines exceed the reported total — or whose
    /// informational reasoning subset exceeds the output total — is a
    /// typed [`UsageSplitError`], never a silent saturate.
    pub fn from_total_including_cache(
        total_input_tokens: u64,
        cache_read_tokens: u64,
        cache_write_tokens: u64,
        output_tokens: u64,
        reasoning_tokens: u64,
    ) -> Result<Self, UsageSplitError> {
        if cache_read_tokens > total_input_tokens {
            return Err(UsageSplitError::CacheReadsExceedInput {
                total_input_tokens,
                cache_read_tokens,
            });
        }
        let remainder = total_input_tokens - cache_read_tokens;
        if cache_write_tokens > remainder {
            return Err(UsageSplitError::CacheWritesExceedInput {
                total_input_tokens,
                cache_write_tokens,
            });
        }
        if reasoning_tokens > output_tokens {
            return Err(UsageSplitError::ReasoningExceedsOutput {
                output_tokens,
                reasoning_tokens,
            });
        }
        Ok(Self {
            uncached_input_tokens: remainder - cache_write_tokens,
            cache_read_tokens,
            cache_write_tokens,
            output_tokens,
            reasoning_tokens,
            ..Self::ZERO
        })
    }

    /// Validate the shared cross-wire invariants of an already-built frame
    /// (the informational reasoning subset never exceeds the output total;
    /// a wire that reports reasoning must have folded it into output).
    /// Adapters that assemble frames field-by-field (anthropic-style split
    /// wires, ollama counts) call this before emission.
    pub fn validate(&self) -> Result<(), UsageSplitError> {
        if self.reasoning_tokens > self.output_tokens {
            return Err(UsageSplitError::ReasoningExceedsOutput {
                output_tokens: self.output_tokens,
                reasoning_tokens: self.reasoning_tokens,
            });
        }
        Ok(())
    }

    /// Every priced category is zero (nothing to settle).
    pub fn is_zero(&self) -> bool {
        self.uncached_input_tokens == 0
            && self.cache_read_tokens == 0
            && self.cache_write_tokens == 0
            && self.output_tokens == 0
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderChunk {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        /// Accumulated so far; `complete` toggles on the final delta.
        input: serde_json::Value,
        complete: bool,
    },
    /// Terminal usage settlement: one canonical usage frame per stream,
    /// usually the LAST one wins. Adapters map their WIRE usage to
    /// [`CanonicalUsage`] at their own boundary (read/write/uncached lines
    /// are already split, output already includes reasoning) — the agent
    /// consumes the canonical categories directly and never re-derives
    /// provider semantics.
    Usage(CanonicalUsage),
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorKind {
    Network,
    Timeout,
    RateLimited,
    BadRequest,
    Auth,
    Server,
    Cancelled,
    Malformed,
    /// The stream breached the Faktor-side cumulative output envelope
    /// derived from the resolved request `max_output`. Terminal and never
    /// retryable: replaying the same request against a provider that just
    /// ignored its cap would only repeat the violation.
    OutputLimitExceeded,
}

impl ProviderErrorKind {
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            ProviderErrorKind::Network
                | ProviderErrorKind::Timeout
                | ProviderErrorKind::RateLimited
                | ProviderErrorKind::Server
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub message: String,
    pub retryable: bool,
    /// Provider-native code (http status, ollama error, ...).
    pub code: Option<String>,
}

impl ProviderError {
    pub fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        let retryable = kind.retryable();
        Self {
            kind,
            message: message.into(),
            retryable,
            code: None,
        }
    }

    pub fn with_code(
        kind: ProviderErrorKind,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        let retryable = kind.retryable();
        Self {
            kind,
            message: message.into(),
            retryable,
            code: Some(code.into()),
        }
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for ProviderError {}

impl From<UsageSplitError> for ProviderError {
    fn from(e: UsageSplitError) -> Self {
        ProviderError::new(
            ProviderErrorKind::Malformed,
            format!("hostile usage frame: {e}"),
        )
    }
}

pub type ProviderStream = Pin<Box<dyn Stream<Item = Result<ProviderChunk, ProviderError>> + Send>>;

/// Registry identity of one provider instance. Adapters report their
/// transport family (`id()` = "openai"), but a daemon can register several
/// OpenAI-compatible endpoints (two proxies, corp gateways, ...). The
/// registry keys by `instance_id` so every configured instance resolves;
/// the family id stays the capability/label face of the provider.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ProviderIdentity {
    pub instance_id: String,
    pub family: String,
}

impl ProviderIdentity {
    pub fn new(instance_id: impl Into<String>, family: impl Into<String>) -> Self {
        Self {
            instance_id: instance_id.into(),
            family: family.into(),
        }
    }

    /// Default identity: one instance per family (instance_id == family).
    pub fn from_family(family: impl Into<String>) -> Self {
        let family = family.into();
        Self {
            instance_id: family.clone(),
            family,
        }
    }
}

/// One transport family. Implementations are stateless except config.
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;

    /// Capabilities for a model; discovered by probing, never hard-coded
    /// lists in the agent.
    fn capabilities(&self, model: &str) -> ModelCapabilities;

    /// The capability lookup WITH its authority state: `Known` only for a
    /// model-specific profile (probed for this exact id or declared per
    /// exact id), `DeclaredGeneric` for an endpoint-declared generic
    /// assumption, `Unknown` when neither matched. The default is
    /// [`CapabilityAuthority::Unknown`] — an adapter that truly knows its
    /// models MUST override; a generic fallback can never silently present
    /// itself as validated capabilities. Consumers decide from
    /// [`CapabilityResolution::effective`], so `Unknown` gates model
    /// features off instead of assuming them.
    fn capability_resolution(&self, model: &str) -> CapabilityResolution {
        CapabilityResolution::unknown(self.capabilities(model))
    }

    /// The CONCRETE served model id this provider will put on the wire for a
    /// requested id. Adapters with synthetic aliases (the Ollama `default`
    /// id) resolve them here; every other adapter returns the request id
    /// unchanged. The runtime records the resolved id in the durable turn
    /// envelope, provider-call rows and receipts, so an alias can never be
    /// reported as the actual model.
    fn resolved_model(&self, model: &str) -> String {
        model.to_string()
    }

    /// A live runtime context bound for `model` in tokens, when the provider
    /// can report one (e.g. an Ollama `/api/ps` allocation clamped by the
    /// probed model maximum — the window actually loaded for the model can
    /// sit far below the advertised maximum). `None` means no override: the
    /// agent budgets from [`ModelCapabilities::context`] as usual (safe
    /// direction when no live data exists). Never exceeds the model's
    /// advertised maximum; the agent takes `min(caps.context, limit)`
    /// defensively anyway. Must be cheap: called synchronously on every turn
    /// plan, so adapters serve the CACHED last-refreshed value.
    fn runtime_context_limit(&self, _model: &str) -> Option<usize> {
        None
    }

    /// Maximum RAW bytes of ONE image part this provider accepts. The
    /// daemon-wide default is [`MAX_MODEL_IMAGE_BYTES`]; adapters with
    /// documented per-image API limits override it (Anthropic 5 MiB,
    /// OpenAI/Google 20 MiB). Admission validates every image attachment
    /// against the CHOSEN provider's value before any run/task row exists.
    fn max_image_bytes(&self) -> usize {
        MAX_MODEL_IMAGE_BYTES
    }

    /// Vision-like DOCUMENT capability gate: true when this provider can
    /// lower a resolved [`ContentKind::FileData`] part for `model` onto its
    /// wire (`input_file` / `file` / `document` / `inline_data`). The
    /// daemon-wide default is `false`: a family that has not implemented
    /// document parts refuses typedly instead of silently dropping them.
    /// Adapters whose wire supports documents override it.
    fn document_capable(&self, _model: &str) -> bool {
        false
    }

    /// Maximum RAW bytes of ONE document part this provider accepts. The
    /// daemon-wide default is [`MAX_MODEL_DOCUMENT_BYTES`]; adapters with
    /// documented API limits may override it. Admission validates every
    /// document attachment against the CHOSEN provider's value.
    fn max_document_bytes(&self) -> usize {
        MAX_MODEL_DOCUMENT_BYTES
    }

    /// Embedding capability flag: true when this provider serves embeddings
    /// for `model` through [`Provider::embed`]. Default `false` — a family
    /// without an embedding surface admits the CLI's strict embedding
    /// selection as an honest unsupported refusal rather than a fabricated
    /// vector.
    fn supports_embeddings(&self, _model: &str) -> bool {
        false
    }

    /// ONE embedding call for `model`. Bounded by
    /// [`EmbeddingRequest`]/[`EmbeddingResponse`] construction; the DEFAULT
    /// is a typed `BadRequest` refusal, so a family that has not implemented
    /// embeddings can never fabricate vectors. Implementations must be
    /// synchronous, bounded and non-panicking; the configured embedder owns
    /// retries and input batching.
    fn embed(&self, req: EmbeddingRequest) -> Result<EmbeddingResponse, ProviderError> {
        Err(ProviderError::new(
            ProviderErrorKind::BadRequest,
            format!(
                "provider {:?} does not implement embeddings for model {:?}",
                self.id(),
                req.model
            ),
        ))
    }

    /// The models this provider can serve (configured + discovered +
    /// probed). Feeds the model-selector surface; never a fabricated list
    /// in the agent. Default: only the "default" entry.
    fn known_models(&self) -> Vec<String> {
        vec!["default".into()]
    }

    /// The real model-catalog row of one model (audit P0-1/wave-B item C):
    /// pricing state, quality priors, provenance and the pricing epoch the
    /// routing graph consumes. Adapters with real knowledge override this
    /// (Ollama rows are [`PricingState::LocalZero`]); the DEFAULT first
    /// consults the versioned built-in list-price table
    /// ([`catalog::builtin`]) by (provider-family, model): a documented
    /// model returns [`PricingState::Known`] with its EXACT
    /// per-million-token quote at epoch
    /// [`catalog::CATALOG_FIRST_EPOCH`] and source
    /// [`catalog::BUILTIN_SOURCE_ID`]. Everything else derives a
    /// conservative row with [`PricingState::Unknown`] — **never a zero or
    /// 1-microUSD fake price** — provenance [`Provenance::BuiltIn`] and
    /// epoch [`catalog::CATALOG_FIRST_EPOCH`]. Legacy adapters compile
    /// unchanged and their undocumented models read as Unknown until
    /// priced by config ([`catalog::PricingOverrides`]).
    fn catalog_entry(&self, model: &str) -> ModelCatalogEntry {
        // The row advertises what the runtime will ACTUALLY enable for this
        // id: `capability_resolution(...).effective()` gates an Unknown
        // lookup exactly like the runtime does, so routing can never select
        // a candidate for features the runtime would strip.
        let caps = self.capability_resolution(model).effective();
        let row = catalog::builtin::lookup(self.id(), model);
        match row {
            Some(row) => ModelCatalogEntry {
                provider: self.identity().instance_id,
                model: model.to_string(),
                capabilities: caps.clone(),
                pricing: PricingState::Known(PricingSnapshot::exact(
                    catalog::builtin::quote_of(&row),
                    catalog::CATALOG_FIRST_EPOCH,
                    catalog::BUILTIN_SOURCE_ID.to_string(),
                )),
                quality_prior: QualityPrior::default(),
                source_epoch: catalog::CATALOG_FIRST_EPOCH,
                provenance: Provenance::BuiltIn,
            },
            None => ModelCatalogEntry {
                provider: self.identity().instance_id,
                model: model.to_string(),
                capabilities: caps,
                pricing: PricingState::Unknown,
                quality_prior: QualityPrior::default(),
                source_epoch: catalog::CATALOG_FIRST_EPOCH,
                provenance: Provenance::BuiltIn,
            },
        }
    }

    fn stream(&self, req: GenericAgentRequest) -> ProviderStream;

    /// Registry identity. The default is one instance per family; daemon
    /// wiring overrides `instance_id` with the configured provider id so
    /// two OpenAI-compatible endpoints never overwrite each other.
    fn identity(&self) -> ProviderIdentity {
        ProviderIdentity::from_family(self.id())
    }
}

/// Wraps an adapter with an explicit registry instance id while keeping the
/// adapter's family `id()` for capability queries and wire metadata. The
/// CLI builds one wrapper per configured provider entry.
pub struct InstanceProvider {
    inner: Arc<dyn Provider>,
    instance_id: String,
}

impl InstanceProvider {
    pub fn wrap(inner: Arc<dyn Provider>, instance_id: impl Into<String>) -> Arc<dyn Provider> {
        Arc::new(Self {
            inner,
            instance_id: instance_id.into(),
        })
    }
}

impl Provider for InstanceProvider {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn identity(&self) -> ProviderIdentity {
        ProviderIdentity::new(self.instance_id.clone(), self.id())
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        self.inner.capabilities(model)
    }

    fn capability_resolution(&self, model: &str) -> CapabilityResolution {
        // Delegate: an instance-wrapped adapter's authority state is its
        // own (the wrapper must not collapse a known family to Unknown).
        self.inner.capability_resolution(model)
    }

    fn known_models(&self) -> Vec<String> {
        // Delegate: an instance-wrapped adapter reports its OWN model list
        // (the trait default of ["default"] would collapse every custom
        // endpoint's real catalog on the daemon's routing graph).
        self.inner.known_models()
    }

    fn resolved_model(&self, model: &str) -> String {
        self.inner.resolved_model(model)
    }

    fn runtime_context_limit(&self, model: &str) -> Option<usize> {
        // Delegate (audit round 11): an instance-wrapped Ollama provider
        // must still shrink the budget to the /api/ps allocation.
        self.inner.runtime_context_limit(model)
    }

    fn max_image_bytes(&self) -> usize {
        // Delegate: the wrapper must not mask a family's documented
        // per-image API limit (admission reads this through the registry).
        self.inner.max_image_bytes()
    }

    fn document_capable(&self, model: &str) -> bool {
        // Delegate: an instance-wrapped document-capable family must keep
        // its document gate (the trait default false would silently make
        // every wrapped endpoint document-less).
        self.inner.document_capable(model)
    }

    fn max_document_bytes(&self) -> usize {
        // Delegate: never mask a family's documented per-document limit.
        self.inner.max_document_bytes()
    }

    fn supports_embeddings(&self, model: &str) -> bool {
        // Delegate: the configured embedder resolves embeddings through the
        // SAME registry instance, so the wrapper must not mask the flag.
        self.inner.supports_embeddings(model)
    }

    fn embed(&self, req: EmbeddingRequest) -> Result<EmbeddingResponse, ProviderError> {
        self.inner.embed(req)
    }

    fn catalog_entry(&self, model: &str) -> ModelCatalogEntry {
        // Delegate the row and rewrite its provider to THIS instance id:
        // catalog rows must name the registry key the daemon resolves
        // (two OpenAI-compatible endpoints never share rows).
        let mut entry = self.inner.catalog_entry(model);
        entry.provider = self.instance_id.clone();
        entry
    }

    fn stream(&self, req: GenericAgentRequest) -> ProviderStream {
        self.inner.stream(req)
    }
}

// ------------------------------------------------------------------ pipeline

/// Step 1: validate a request against known capabilities *before* any wire
/// call. Violations are loud errors, never silent truncation.
pub struct CapabilityValidator;

impl CapabilityValidator {
    pub fn validate(
        req: &GenericAgentRequest,
        caps: &ModelCapabilities,
    ) -> Result<(), faktor_core::Error> {
        use faktor_core::error::{Error, ErrorKind};
        if !req.tools.is_empty() && !caps.tools {
            return Err(Error::new(
                ErrorKind::Malformed,
                format!(
                    "model {} does not support tools, but {} tool(s) requested",
                    req.model,
                    req.tools.len()
                ),
            ));
        }
        if req.reasoning.is_some() && !(caps.reasoning || caps.thinking) {
            return Err(Error::new(
                ErrorKind::Malformed,
                format!("model {} does not support reasoning", req.model),
            ));
        }
        if let Some(max_out) = req.max_output {
            if max_out > caps.max_output {
                return Err(Error::new(
                    ErrorKind::Oversized,
                    format!(
                        "requested max_output {max_out} exceeds model cap {}",
                        caps.max_output
                    ),
                ));
            }
        }
        // Resolved attachment media is only deliverable to a model that
        // advertises vision; the refusal happens BEFORE any adapter
        // lowering, so a vision-less model can never receive a silently
        // dropped (or fabricated) image part.
        let image_parts = req
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter(|p| matches!(p.kind, ContentKind::ImageData { .. }))
            .count();
        if image_parts > 0 && !caps.vision {
            return Err(Error::new(
                ErrorKind::Malformed,
                format!(
                    "model {} does not support vision, but {image_parts} image part(s) were resolved from attachments",
                    req.model
                ),
            ));
        }
        Ok(())
    }

    /// The vision-like DOCUMENT gate: a request carrying resolved
    /// [`ContentKind::FileData`] parts is refused typedly for a model whose
    /// provider does not advertise document support. Kept separate from
    /// [`Self::validate`] because document capability lives on the provider
    /// (per wire family), not on [`ModelCapabilities`].
    pub fn validate_documents(
        req: &GenericAgentRequest,
        document_capable: bool,
    ) -> Result<(), faktor_core::Error> {
        use faktor_core::error::{Error, ErrorKind};
        let document_parts = req
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter(|p| matches!(p.kind, ContentKind::FileData { .. }))
            .count();
        if document_parts > 0 && !document_capable {
            return Err(Error::new(
                ErrorKind::Malformed,
                format!(
                    "the provider of model {} does not support document input, but {document_parts} document part(s) were resolved from attachments",
                    req.model
                ),
            ));
        }
        Ok(())
    }
}

/// Step 2: ensure internal option names never leak onto wire APIs. The
/// normalizer strips anything not on the explicit whitelist and enforces
/// bound clamps. Adapters additionally translate to their wire vocabulary.
pub struct RequestNormalizer;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct NormalizedRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<RequestMessage>,
    pub tools: Vec<ToolSpec>,
    pub max_output: Option<usize>,
    pub reasoning: Option<ReasoningMode>,
    pub stream: bool,
}

impl RequestNormalizer {
    /// Whitelisted internal fields (the frozen set). Anything else that ever
    /// sneaks into `GenericAgentRequest` will simply not exist on the wire —
    /// this is the structural fix for leaked compaction/option names.
    pub fn normalize(req: &GenericAgentRequest) -> NormalizedRequest {
        NormalizedRequest {
            model: req.model.clone(),
            system: req.system.clone(),
            messages: req.messages.clone(),
            tools: req.tools.clone(),
            max_output: req.max_output,
            reasoning: req.reasoning,
            stream: req.stream,
        }
    }
}

/// Hard bound on one provider instance id, in bytes (P0-41). The registry
/// refuses longer ids with a typed [`ErrorKind::Oversized`] error — hostile
/// or corrupt wiring never populates the map with unbounded keys. Mirrors
/// the 256-byte provider-name bound the session layer enforces.
pub const MAX_PROVIDER_INSTANCE_ID_BYTES: usize = 256;

/// Dynamic model registry: providers register their models; the agent asks
/// the registry, never the provider string.
#[derive(Default)]
pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn Provider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// The ONE registration API (P0-41) — fallible and typed; there is no
    /// infallible duplicate-accepting shim anymore.
    ///
    /// Providers are keyed by their INSTANCE id (never the family id: two
    /// OpenAI-compatible endpoints with distinct configured ids must both
    /// register). The audited semantics:
    ///
    /// | registration | result |
    /// |---|---|
    /// | fresh id | `Ok`, inserted |
    /// | same id, SAME instance (`Arc::ptr_eq`) | `Ok`, no-op — idempotent |
    /// | same id, DIFFERENT instance | `Err(Conflict)`, FIRST entry kept, never replaced |
    /// | id differing only by case from an existing key | `Err(Conflict)`, FIRST entry kept |
    /// | empty id | `Err(Malformed)`, nothing inserted |
    /// | id over [`MAX_PROVIDER_INSTANCE_ID_BYTES`] | `Err(Oversized)`, nothing inserted |
    pub fn try_register(&mut self, p: Arc<dyn Provider>) -> Result<(), Error> {
        let id = p.identity().instance_id;
        if id.is_empty() {
            return Err(Error::new(
                ErrorKind::Malformed,
                "provider instance id is empty; refusing to register",
            ));
        }
        if id.len() > MAX_PROVIDER_INSTANCE_ID_BYTES {
            return Err(Error::new(
                ErrorKind::Oversized,
                format!(
                    "provider instance id is {} bytes, over the cap of {MAX_PROVIDER_INSTANCE_ID_BYTES}",
                    id.len()
                ),
            ));
        }
        if let Some(existing) = self.providers.get(&id) {
            if Arc::ptr_eq(existing, &p) {
                return Ok(());
            }
            return Err(Error::conflict(format!(
                "provider {id:?} already registered by a DIFFERENT instance; keeping the first entry"
            )));
        }
        if let Some(first) = self
            .providers
            .keys()
            .find(|k| k.to_lowercase() == id.to_lowercase())
        {
            return Err(Error::conflict(format!(
                "provider {id:?} is a case variant of already-registered {first:?}; keeping the first entry"
            )));
        }
        self.providers.insert(id, p);
        Ok(())
    }

    /// Every registered provider (daemon warm-up / diagnostics).
    pub fn all(&self) -> Vec<Arc<dyn Provider>> {
        self.providers.values().cloned().collect()
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn Provider>> {
        self.providers.get(id).cloned()
    }

    pub fn ids(&self) -> Vec<String> {
        let mut v: Vec<String> = self.providers.keys().cloned().collect();
        v.sort();
        v
    }

    pub fn capabilities(&self, provider: &str, model: &str) -> Option<ModelCapabilities> {
        self.get(provider).map(|p| p.capabilities(model))
    }

    pub fn len(&self) -> usize {
        self.providers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.providers.is_empty()
    }
}

// ------------------------------------------------------------------ tokenizer identity

/// Tokenizer family of a provider-known model (P0-81). The family is the
/// *static* identity a model's tokenizer is known by (tiktoken's o200k_base
/// for the modern GPT family, Anthropic's own tokenizer, ...); a real local
/// tokenizer implementation may one day name itself by family + version.
/// `GenericEstimator` is the conservative fallback — no exact tokenizer
/// exists for it, only the bounded generic estimator.
///
/// Variant order is the [`Ord`] order (deterministic; never change it).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum TokenFamily {
    /// OpenAI o200k_base (gpt-4o / gpt-4.1 / o1 / o3 / gpt-5 families).
    O200kBase,
    /// OpenAI cl100k_base (gpt-3.5 / gpt-4 families).
    Cl100kBase,
    /// Anthropic's tokenizer (claude models).
    Anthropic,
    /// Google's tokenizer (gemini models).
    Gemini,
    /// Meta/Llama-family BPE (llama, qwen, and llama-hosted distills).
    Llama,
    /// No provider-known tokenizer: the conservative generic estimator.
    GenericEstimator,
}

impl TokenFamily {
    /// Machine-readable family name (stable, lowercase, snake_case).
    pub const fn as_str(self) -> &'static str {
        match self {
            TokenFamily::O200kBase => "o200k_base",
            TokenFamily::Cl100kBase => "cl100k_base",
            TokenFamily::Anthropic => "anthropic",
            TokenFamily::Gemini => "gemini",
            TokenFamily::Llama => "llama",
            TokenFamily::GenericEstimator => "generic_estimator",
        }
    }
}

impl std::fmt::Display for TokenFamily {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Versioned identity of the tokenizer a model family maps to (P0-81).
/// `version` distinguishes tokenizer API generations of one family — it is
/// frozen at `1` for every family today and must bump if a provider's
/// tokenizer vocabulary/API changes (cache entries and exact counts are
/// keyed by the full identity, so a version bump invalidates stale counts
/// instead of silently reusing them).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct TokenizerId {
    pub family: TokenFamily,
    pub version: u32,
}

impl TokenizerId {
    /// o200k_base, generation 1 (the tiktoken vocabulary as frozen today).
    pub const O200K_BASE: TokenizerId = TokenizerId {
        family: TokenFamily::O200kBase,
        version: 1,
    };
    /// cl100k_base, generation 1.
    pub const CL100K_BASE: TokenizerId = TokenizerId {
        family: TokenFamily::Cl100kBase,
        version: 1,
    };
    /// Anthropic's tokenizer, generation 1.
    pub const ANTHROPIC: TokenizerId = TokenizerId {
        family: TokenFamily::Anthropic,
        version: 1,
    };
    /// Google's tokenizer, generation 1.
    pub const GEMINI: TokenizerId = TokenizerId {
        family: TokenFamily::Gemini,
        version: 1,
    };
    /// Llama-family BPE, generation 1.
    pub const LLAMA: TokenizerId = TokenizerId {
        family: TokenFamily::Llama,
        version: 1,
    };
    /// The conservative fallback: no exact tokenizer, only the generic
    /// estimator. This is what unknown models map to.
    pub const GENERIC_ESTIMATOR: TokenizerId = TokenizerId {
        family: TokenFamily::GenericEstimator,
        version: 1,
    };
}

impl Default for TokenizerId {
    fn default() -> Self {
        TokenizerId::GENERIC_ESTIMATOR
    }
}

impl std::fmt::Display for TokenizerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@v{}", self.family, self.version)
    }
}

/// Match `s` after the LAST `/` (routed model strings are often
/// `provider/model`), trimmed and lowercased.
fn tokenizer_model_base(model: &str) -> String {
    model
        .rsplit('/')
        .next()
        .unwrap_or(model)
        .trim()
        .to_ascii_lowercase()
}

fn starts_with_any(s: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|p| s.starts_with(p))
}

/// The PURE model → tokenizer mapping (P0-81): decides which
/// [`TokenizerId`] a model string names, statically, from model-name
/// prefixes. Provider behavior is never probed here and no remote
/// tokenizer API is ever consulted.
///
/// Ordering contract (longest/specific prefixes first — `gpt-4o` MUST be
/// tested before `gpt-4`, or every gpt-4o row would mis-map to cl100k):
///
/// | prefix (lowercased, after the last `/`) | family |
/// |---|---|
/// | `gpt-4o`, `gpt-4.1`, `gpt-5`, `o1`, `o3` | `O200kBase` |
/// | `gpt-3.5`, `gpt-4` | `Cl100kBase` |
/// | `claude` | `Anthropic` |
/// | `gemini` | `Gemini` |
/// | `llama`, `qwen` | `Llama` |
/// | `deepseek` (only under a llama-family deployment hint) | `Llama` |
/// | anything else | `GenericEstimator` |
///
/// `catalog_hint` is an optional deployment hint (typically the provider
/// family id, e.g. `"ollama"`). It never UPGRADES an unknown model to a
/// named family — an OpenAI-compatible endpoint is not an OpenAI tokenizer
/// — and is consulted only where a model string is genuinely ambiguous
/// (`deepseek-*` weights served by a llama-family local runtime map to the
/// Llama tokenizer; `deepseek-*` on the official API keeps DeepSeek's own
/// tokenizer, which is NOT llama's, so it conservatively maps to
/// `GenericEstimator`). Every row maps to `version: 1` today.
pub fn tokenizer_for(model: &str, catalog_hint: Option<&str>) -> TokenizerId {
    let base = tokenizer_model_base(model);
    let hint = catalog_hint.unwrap_or("").to_ascii_lowercase();
    if starts_with_any(&base, &["gpt-4o", "gpt-4.1", "gpt-5", "o1", "o3"]) {
        TokenizerId::O200K_BASE
    } else if starts_with_any(&base, &["gpt-3.5", "gpt-4"]) {
        TokenizerId::CL100K_BASE
    } else if base.starts_with("claude") {
        TokenizerId::ANTHROPIC
    } else if base.starts_with("gemini") {
        TokenizerId::GEMINI
    } else if (base.starts_with("llama") || base.starts_with("qwen"))
        || (base.starts_with("deepseek") && hint.contains("llama"))
    {
        TokenizerId::LLAMA
    } else {
        TokenizerId::GENERIC_ESTIMATOR
    }
}

pub use std::sync::Arc;

pub mod transport;

/// Parsed destination gate + checked outbound HTTP client (audits 36-37):
/// every egress decision runs against a parsed (scheme, host, port) triple
/// before the connection is attempted.
pub mod egress;

/// Adversarial wire-testing harness (mock HTTP server).
pub mod testing;

/// Shared canonical-usage conformance support (audit Phase-1 item C). The
/// [`canonical_usage_conformance!`](crate::canonical_usage_conformance)
/// macro is the driver; this module carries the per-wire-family required
/// case tables and the case row type. Adapters instantiate the macro in a
/// `#[cfg(test)]` module with mock wire bodies that match their REAL wire
/// shapes; the driver asserts every required case of the adapter's family
/// is present and that each case's stream yields exactly the expected
/// canonical frame (or a typed `Malformed` error for hostile rows).
#[doc(hidden)]
pub mod usage_conformance {
    use super::{CanonicalUsage, ProviderChunk};

    /// Which wire semantics the adapter's usage envelope has. Every
    /// instantiation must carry a [`WireUsageCase`] per required name of
    /// its family (enforced by the driver macro), so the audit case list is
    /// exercised once per adapter against that adapter's true wire shape.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum WireFamily {
        /// The wire input total INCLUDES cache reads (openai `prompt_tokens`
        /// with `prompt_tokens_details.cached_tokens`, gemini
        /// `promptTokenCount` with `cachedContentTokenCount`):
        /// canonicalization subtracts the cached portion, and a cached>total
        /// row is Malformed.
        InclusiveTotal,
        /// The wire input counter EXCLUDES cache reads and reports cache
        /// creation/write lines separately (anthropic `input_tokens` /
        /// `cache_read_input_tokens` / `cache_creation_input_tokens`):
        /// canonical categories map as-is, modulo the creation lines that
        /// ride inside `input_tokens`.
        SplitCache,
        /// The wire reports ONE inclusive input total with an OPTIONAL
        /// cache-detail counter (ollama `prompt_eval_count` plus
        /// `prompt_eval_cached_count`): when the detail is present the
        /// cached prefix is split out of the total; when it is absent (older
        /// runtimes) the conservative category is uncached = the reported
        /// count with cache lines zero. A detail counter above the total is
        /// ignored (never subtracted into a wrapped count).
        OptionalCacheDetail,
    }

    /// Required case names per wire family. These ARE the audit Phase-1
    /// item C cases: total-including-cache split vs. pre-split identity,
    /// cache-detail missing (uncached = total), hostile cache>total typed
    /// Malformed, reasoning subset never double-billed, hostile reasoning
    /// subset, unknown fields never panicking, request id preservation, and
    /// the wire-family-specific rows.
    pub fn required_cases(family: WireFamily) -> &'static [&'static str] {
        match family {
            WireFamily::InclusiveTotal => &[
                "total_incl_cached_split",
                "cache_detail_missing_uncached_total",
                "hostile_cache_over_total",
                "reasoning_subset_inside_output",
                "hostile_reasoning_over_output",
                "unknown_fields_never_panic",
                "request_id_preserved",
            ],
            WireFamily::SplitCache => &[
                "split_input_cache_identity",
                "cache_write_inside_input_total_split",
                "hostile_cache_write_over_input",
                "cache_detail_missing_uncached_total",
                "cache_only_frame_no_input",
                "unknown_fields_never_panic",
            ],
            WireFamily::OptionalCacheDetail => &[
                "counts_map_uncached_total",
                "cache_detail_split_when_present",
                "hostile_cache_over_total_ignored",
                "thinking_included_in_output_never_double_billed",
                "hostile_junk_counts_never_panic",
                "zero_counts_no_usage_frame",
            ],
        }
    }

    /// What one conformance case must produce.
    #[derive(Debug, Clone, PartialEq)]
    pub enum WireUsageExpectation {
        /// The stream must surface exactly one canonical usage frame equal
        /// to this (any leading text/reasoning/tool chunks are allowed —
        /// e.g. ollama's thinking/content share the final frame), followed
        /// by `Done`, with no errors anywhere.
        Frame(CanonicalUsage),
        /// The stream must fail exactly once with a typed `Malformed`
        /// error and emit no usage frame.
        Malformed,
        /// The stream must complete cleanly with no usage frame at all
        /// (hostile junk rows an adapter ignores, all-zero counter rows).
        NoUsageFrame,
    }

    /// One conformance row: a name from the family's required table (the
    /// driver asserts presence), the full wire body bytes, and what the
    /// stream must yield.
    #[derive(Debug, Clone)]
    pub struct WireUsageCase {
        pub name: &'static str,
        pub body: String,
        pub expect: WireUsageExpectation,
    }

    impl WireUsageCase {
        pub fn frame(name: &'static str, body: impl Into<String>, usage: CanonicalUsage) -> Self {
            Self {
                name,
                body: body.into(),
                expect: WireUsageExpectation::Frame(usage),
            }
        }

        pub fn malformed(name: &'static str, body: impl Into<String>) -> Self {
            Self {
                name,
                body: body.into(),
                expect: WireUsageExpectation::Malformed,
            }
        }

        pub fn no_usage(name: &'static str, body: impl Into<String>) -> Self {
            Self {
                name,
                body: body.into(),
                expect: WireUsageExpectation::NoUsageFrame,
            }
        }
    }

    /// Reduce one driven stream for failure assertions.
    pub fn usage_index(items: &[Result<ProviderChunk, super::ProviderError>]) -> Option<usize> {
        items
            .iter()
            .position(|i| matches!(i, Ok(ProviderChunk::Usage(_))))
    }
}

/// Adversarial canonical-usage conformance driver (shared test harness).
///
/// Expand once per adapter inside a `#[cfg(test)]` module:
///
/// ```ignore
/// canonical_usage_conformance! {
///     driver: openai_chat_usage_conformance,
///     family: faktor_provider::usage_conformance::WireFamily::InclusiveTotal,
///     label: "openai chat completions",
///     request: || req("m1"),
///     provider: |base: String| OpenAiProvider::permissive_for_tests(OpenAiConfig::chat(base, None)),
///     method: "POST",
///     path: "/chat/completions",
///     cases: vec![ /* one WireUsageCase per required case name */ ],
/// }
/// ```
///
/// The driver asserts (1) the adapter's case table covers EVERY required
/// case name of its wire family, and (2) each case driven over a real
/// provider stream against a mock HTTP server yields exactly the expected
/// canonical usage frame then `Done` — or fails exactly once with a typed
/// `Malformed` error for hostile rows, or completes cleanly without a
/// usage frame where the expectation says so. Unknown-field payloads and
/// absurd values can never panic: a panic fails the test loudly.
#[macro_export]
macro_rules! canonical_usage_conformance {
    (
        driver: $driver:ident,
        family: $family:expr,
        label: $label:expr,
        request: $request:expr,
        provider: $provider:expr,
        method: $method:expr,
        path: $path:expr,
        cases: $cases:expr
    ) => {
        #[::tokio::test]
        async fn $driver() {
            use ::futures::StreamExt as _;
            use $crate::usage_conformance::{
                required_cases, WireUsageExpectation, WireUsageCase,
            };
            let cases: Vec<WireUsageCase> = $cases;
            assert!(
                !cases.is_empty(),
                "{}: at least one conformance case is required",
                $label
            );
            for (i, c) in cases.iter().enumerate() {
                for (j, other) in cases.iter().enumerate() {
                    assert!(
                        i == j || c.name != other.name,
                        "{}: duplicate conformance case name {:?}",
                        $label,
                        c.name
                    );
                }
            }
            let have: Vec<&str> = cases.iter().map(|c| c.name).collect();
            let required = required_cases($family);
            for want in required {
                assert!(
                    have.contains(want),
                    "{} conformance is missing required case {want:?} (have {have:?})",
                    $label
                );
            }
            for case in &cases {
                let server = $crate::testing::MockServer::new();
                server.route(
                    $method,
                    $path,
                    $crate::testing::MockAction::Respond {
                        status: 200,
                        body: case.body.clone(),
                    },
                );
                let base = server.base_url().await;
                let provider = $provider(base);
                let mut stream = provider.stream($request());
                let mut items: Vec<Result<$crate::ProviderChunk, $crate::ProviderError>> =
                    Vec::new();
                while let Some(item) = stream.next().await {
                    items.push(item);
                }
                match &case.expect {
                    WireUsageExpectation::Frame(expected) => {
                        let usage_at = $crate::usage_conformance::usage_index(&items);
                        let usage_at = usage_at.unwrap_or_else(|| {
                            panic!(
                                "{} case {:?} must emit a canonical usage frame; got {items:?}",
                                $label, case.name
                            )
                        });
                        for (k, item) in items[..usage_at].iter().enumerate() {
                            assert!(
                                matches!(
                                    item,
                                    Ok($crate::ProviderChunk::Text { .. })
                                        | Ok($crate::ProviderChunk::Reasoning { .. })
                                        | Ok($crate::ProviderChunk::ToolCall { .. })
                                ),
                                "{} case {:?}: unexpected item before the usage frame at \
                                 index {k}: {item:?}",
                                $label,
                                case.name
                            );
                        }
                        assert_eq!(
                            items[usage_at],
                            Ok($crate::ProviderChunk::Usage(expected.clone())),
                            "{} case {:?}: canonical usage mismatch",
                            $label,
                            case.name
                        );
                        assert_eq!(
                            usage_at + 1,
                            items.len().saturating_sub(1),
                            "{} case {:?}: usage must be the last chunk before Done \
                             (exactly one usage frame per stream); got {items:?}",
                            $label,
                            case.name
                        );
                        assert_eq!(
                            items.last(),
                            Some(&Ok($crate::ProviderChunk::Done)),
                            "{} case {:?}: stream must end with Done",
                            $label,
                            case.name
                        );
                    }
                    WireUsageExpectation::Malformed => {
                        let errs: Vec<&$crate::ProviderError> =
                            items.iter().filter_map(|i| i.as_ref().err()).collect();
                        assert_eq!(
                            errs.len(),
                            1,
                            "{} case {:?}: a hostile row must fail exactly once; got {items:?}",
                            $label,
                            case.name
                        );
                        assert_eq!(
                            errs[0].kind,
                            $crate::ProviderErrorKind::Malformed,
                            "{} case {:?}: hostile usage rows are typed Malformed; got {:?}",
                            $label,
                            case.name,
                            errs[0]
                        );
                        assert!(
                            $crate::usage_conformance::usage_index(&items).is_none(),
                            "{} case {:?}: no usage frame may follow a Malformed error; got {items:?}",
                            $label,
                            case.name
                        );
                    }
                    WireUsageExpectation::NoUsageFrame => {
                        assert!(
                            items.iter().all(|i| i.is_ok()),
                            "{} case {:?}: hostile junk must never error or panic; got {items:?}",
                            $label,
                            case.name
                        );
                        assert!(
                            $crate::usage_conformance::usage_index(&items).is_none(),
                            "{} case {:?}: no usage frame expected; got {items:?}",
                            $label,
                            case.name
                        );
                        assert_eq!(
                            items.last(),
                            Some(&Ok($crate::ProviderChunk::Done)),
                            "{} case {:?}: stream must still end with Done",
                            $label,
                            case.name
                        );
                    }
                }
            }
        }
    };
}

// ------------------------------------------------------------------ fake provider for tests

/// Scripted multi-model catalog provider (registry-mirror helper for the
/// economy certification suite): every known model carries its OWN
/// capabilities, `known_models` reports catalog insertion order and
/// `capabilities` resolves per model. There is no streaming behavior —
/// catalog/registry-mirror tests never settle a paid call; an empty stream
/// is served if one is ever requested.
pub struct CatalogProvider {
    id: String,
    default_caps: ModelCapabilities,
    models: Vec<(String, ModelCapabilities)>,
    /// Model-name → row index (complexity audit): `capabilities`/
    /// `capability_resolution` are called per candidate during routing-graph
    /// construction, and the pre-fix form scanned the whole Vec per call.
    index: std::collections::HashMap<String, usize>,
}

impl CatalogProvider {
    /// A catalog with no known models yet (`default_caps` answers
    /// `capabilities` for models the catalog does not name).
    pub fn new(id: impl Into<String>, default_caps: ModelCapabilities) -> Self {
        Self {
            id: id.into(),
            default_caps,
            models: Vec::new(),
            index: std::collections::HashMap::new(),
        }
    }

    /// Add one known model with its own capabilities. Appends after the
    /// models added before it: `known_models()` reports insertion order. A
    /// repeated model id keeps the FIRST row's capabilities (the pre-fix
    /// linear scan was first-match-wins); the duplicate still appears in
    /// `known_models()` exactly as before.
    pub fn add_model(&mut self, model: impl Into<String>, caps: ModelCapabilities) -> &mut Self {
        let model = model.into();
        if !self.index.contains_key(&model) {
            self.index.insert(model.clone(), self.models.len());
        }
        self.models.push((model, caps));
        self
    }
}

impl Provider for CatalogProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        self.caps_for(model)
            .unwrap_or_else(|| self.default_caps.clone())
    }

    fn capability_resolution(&self, model: &str) -> CapabilityResolution {
        // The catalog's named models are model-specific rows (`Known`); the
        // constructor-provided `default_caps` answers unnamed ids as an
        // EXPLICIT declared generic profile, never as validated caps.
        self.caps_for(model)
            .map(CapabilityResolution::known)
            .unwrap_or_else(|| CapabilityResolution::declared_generic(self.default_caps.clone()))
    }

    fn known_models(&self) -> Vec<String> {
        self.models.iter().map(|(m, _)| m.clone()).collect()
    }

    fn stream(&self, _req: GenericAgentRequest) -> ProviderStream {
        Box::pin(futures::stream::empty())
    }
}

impl CatalogProvider {
    /// The row for `model`, first-insertion-wins (the pre-fix linear scan
    /// returned the first equal id; the index pins the same row).
    fn caps_for(&self, model: &str) -> Option<ModelCapabilities> {
        self.index
            .get(model)
            .and_then(|index| self.models.get(*index))
            .map(|(_, caps)| caps.clone())
    }
}

/// Scripted provider for adversarial agent/server tests. Responses are
/// user-controlled; streams can be made to die mid-flight, return malformed
/// tool calls, rate-limit, etc.
pub struct FakeProvider {
    pub id: String,
    pub caps: ModelCapabilities,
    pub script: std::sync::Mutex<Vec<ScriptedResponse>>,
    pub fail_after_chunks: Option<usize>,
    /// Document gate the fake advertises (default `false`).
    pub document_capable: bool,
    /// Embedding model the fake serves; `None` = the capability flag is
    /// false and `embed` falls back to the trait's typed refusal.
    pub embedding_model: Option<String>,
    /// One-shot: the FIRST stream call errors before any chunk; later
    /// calls delegate to the script (state-aware-retry tests).
    fail_once_before: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// The `model` of the most recent request streamed through this
    /// provider (test hook: asserts what the agent actually sent).
    last_model: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// The cancellation token of the most recent request (test hook:
    /// asserts the provider request shares the turn's cancellation lineage).
    last_cancellation: std::sync::Arc<std::sync::Mutex<Option<CancellationToken>>>,
    /// Scripted embedding responses, consumed in call order (an exhausted
    /// script is a typed `Malformed` refusal).
    embed_script: std::sync::Arc<std::sync::Mutex<Vec<Result<EmbeddingResponse, ProviderError>>>>,
    /// Every embedding request the fake received, in call order (test hook).
    embed_requests: std::sync::Arc<std::sync::Mutex<Vec<EmbeddingRequest>>>,
    /// Total embedding calls the fake observed (test hook for retry
    /// policy assertions: a retried call is several calls).
    embed_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[derive(Debug, Clone)]
pub enum ScriptedResponse {
    Text(String),
    Reasoning(String),
    ToolCall {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// Ends the stream cleanly.
    End,
    /// Simulates a network/stream death (error mid-flight).
    Die(ProviderError),
}

impl FakeProvider {
    fn empty(id: &str, caps: ModelCapabilities, script: Vec<ScriptedResponse>) -> Self {
        Self {
            id: id.to_string(),
            caps,
            script: std::sync::Mutex::new(script),
            fail_after_chunks: None,
            document_capable: false,
            embedding_model: None,
            fail_once_before: None,
            last_model: std::sync::Arc::new(std::sync::Mutex::new(None)),
            last_cancellation: std::sync::Arc::new(std::sync::Mutex::new(None)),
            embed_script: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            embed_requests: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            embed_calls: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    pub fn new(id: &str, caps: ModelCapabilities) -> Self {
        Self::empty(id, caps, vec![ScriptedResponse::End])
    }

    pub fn with_script(id: &str, caps: ModelCapabilities, script: Vec<ScriptedResponse>) -> Self {
        Self::empty(id, caps, script)
    }

    /// A fake that advertises document capability for every model.
    pub fn with_documents(mut self) -> Self {
        self.document_capable = true;
        self
    }

    /// A fake that serves embeddings for `model` from a scripted response
    /// list, consumed one per call.
    pub fn with_embeddings(
        mut self,
        model: &str,
        script: Vec<Result<EmbeddingResponse, ProviderError>>,
    ) -> Self {
        self.embedding_model = Some(model.to_string());
        *self.embed_script.lock().unwrap() = script;
        self
    }

    /// Fail the FIRST stream call with a retryable network error BEFORE
    /// any chunk (the state-aware-retry test's pre-accept failure); later
    /// streams serve the script normally.
    pub fn die_before_stream(
        id: &str,
        caps: ModelCapabilities,
        script: Vec<ScriptedResponse>,
    ) -> Self {
        let mut fake = Self::empty(id, caps, script);
        fake.fail_once_before = Some(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
            false,
        )));
        fake
    }

    pub fn die_mid_stream(id: &str, caps: ModelCapabilities) -> Self {
        let mut fake = Self::empty(
            id,
            caps,
            vec![ScriptedResponse::Text("partial reply…".into())],
        );
        fake.fail_after_chunks = Some(1);
        fake
    }

    /// The model of the last request this provider was asked to stream
    /// (`None` when nothing was streamed yet).
    pub fn last_request_model(&self) -> Option<String> {
        self.last_model.lock().unwrap().clone()
    }

    /// The cancellation token of the last request this provider was asked to
    /// stream (`None` when nothing was streamed yet).
    pub fn last_request_cancellation(&self) -> Option<CancellationToken> {
        self.last_cancellation.lock().unwrap().clone()
    }

    /// Every embedding request this fake received, in call order.
    pub fn embedding_requests(&self) -> Vec<EmbeddingRequest> {
        self.embed_requests.lock().unwrap().clone()
    }

    /// Total embedding calls the fake observed (including failed/refused
    /// ones) — the retry-policy assertion hook.
    pub fn embedding_call_count(&self) -> usize {
        self.embed_calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// If true, the next call fails with RateLimited (and the script is
    /// untouched) — used for retry tests.
    pub fn inject_rate_limit(&self) {
        self.script.lock().unwrap().insert(
            0,
            ScriptedResponse::Die(ProviderError::new(
                ProviderErrorKind::RateLimited,
                "429 too many",
            )),
        );
    }
}

impl Clone for FakeProvider {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            caps: self.caps.clone(),
            script: std::sync::Mutex::new(self.script.lock().unwrap().clone()),
            fail_after_chunks: self.fail_after_chunks,
            document_capable: self.document_capable,
            embedding_model: self.embedding_model.clone(),
            fail_once_before: self.fail_once_before.clone(),
            last_model: self.last_model.clone(),
            last_cancellation: self.last_cancellation.clone(),
            embed_script: self.embed_script.clone(),
            embed_requests: self.embed_requests.clone(),
            embed_calls: self.embed_calls.clone(),
        }
    }
}

impl Provider for FakeProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self, _model: &str) -> ModelCapabilities {
        self.caps.clone()
    }

    fn capability_resolution(&self, _model: &str) -> CapabilityResolution {
        // A scripted fake is constructed with its exact profile by the
        // test: model-specific by construction.
        CapabilityResolution::known(self.caps.clone())
    }

    fn document_capable(&self, _model: &str) -> bool {
        self.document_capable
    }

    fn supports_embeddings(&self, model: &str) -> bool {
        self.embedding_model
            .as_deref()
            .is_some_and(|m| m == model || m == "*")
    }

    fn embed(&self, req: EmbeddingRequest) -> Result<EmbeddingResponse, ProviderError> {
        self.embed_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.embed_requests.lock().unwrap().push(req.clone());
        if !self.supports_embeddings(&req.model) {
            return Err(ProviderError::new(
                ProviderErrorKind::BadRequest,
                format!(
                    "fake provider does not serve embeddings for {:?}",
                    req.model
                ),
            ));
        }
        let next = if self.embed_script.lock().unwrap().is_empty() {
            None
        } else {
            Some(self.embed_script.lock().unwrap().remove(0))
        };
        match next {
            Some(result) => result,
            None => Err(ProviderError::new(
                ProviderErrorKind::Malformed,
                "fake embedding script exhausted",
            )),
        }
    }

    fn stream(&self, req: GenericAgentRequest) -> ProviderStream {
        // One-shot pre-accept failure (state-aware retry tests).
        if let Some(flag) = &self.fail_once_before {
            if !flag.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return Box::pin(futures::stream::iter(vec![Err(ProviderError::new(
                    ProviderErrorKind::Network,
                    "connection reset (injected once)",
                ))]));
            }
        }
        // Test hook: record exactly which model the agent sent.
        *self.last_model.lock().unwrap() = Some(req.model.clone());
        // Test hook: record the request's cancellation token so tests can
        // assert it shares the turn's cancellation lineage.
        *self.last_cancellation.lock().unwrap() = Some(req.meta.cancellation.clone());
        // Scripts are consumed exactly once (a replaying provider would let
        // the agent loop forever re-executing the same calls). ONE stream is
        // one model response: a leading run of tool calls (optionally with
        // preceding text) is one response; text/reasoning that FOLLOWS a
        // tool call belongs to the next response and stays queued. Without
        // this boundary a tool-using turn consumed the whole script on its
        // first call and the tool-continuation call saw an empty script.
        let script = {
            let mut guard = self.script.lock().unwrap();
            let mut split = 0usize;
            let mut saw_tool = false;
            for (index, response) in guard.iter().enumerate() {
                match response {
                    ScriptedResponse::ToolCall { .. } => {
                        saw_tool = true;
                        split = index + 1;
                    }
                    ScriptedResponse::End | ScriptedResponse::Die(_) => {
                        split = index + 1;
                        break;
                    }
                    ScriptedResponse::Text(_) | ScriptedResponse::Reasoning(_) => {
                        if saw_tool {
                            break;
                        }
                        split = index + 1;
                    }
                }
            }
            if split == 0 {
                Vec::new()
            } else {
                guard.drain(..split).collect()
            }
        };
        let fail_after = self.fail_after_chunks;
        let stream = futures::stream::unfold(
            (script.into_iter(), 0usize, fail_after, false),
            move |(mut remaining, mut emitted, fail_after, ended)| async move {
                if ended {
                    return None; // exactly one terminal item, then end
                }
                if let Some(limit) = fail_after {
                    if emitted >= limit {
                        return Some((
                            Err(ProviderError::new(
                                ProviderErrorKind::Network,
                                "connection vanished mid-stream (injected)",
                            )),
                            (remaining, emitted, fail_after, true),
                        ));
                    }
                }
                match remaining.next() {
                    Some(ScriptedResponse::Text(t)) => {
                        emitted += 1;
                        Some((
                            Ok(ProviderChunk::Text { text: t }),
                            (remaining, emitted, fail_after, false),
                        ))
                    }
                    Some(ScriptedResponse::Reasoning(t)) => {
                        emitted += 1;
                        Some((
                            Ok(ProviderChunk::Reasoning { text: t }),
                            (remaining, emitted, fail_after, false),
                        ))
                    }
                    Some(ScriptedResponse::ToolCall { id, name, input }) => {
                        emitted += 1;
                        Some((
                            Ok(ProviderChunk::ToolCall {
                                id,
                                name,
                                input,
                                complete: true,
                            }),
                            (remaining, emitted, fail_after, false),
                        ))
                    }
                    Some(ScriptedResponse::Die(e)) => {
                        emitted += 1;
                        Some((Err(e), (remaining, emitted, fail_after, true)))
                    }
                    Some(ScriptedResponse::End) | None => {
                        let _ = req.meta.deadline_ms;
                        Some((
                            Ok(ProviderChunk::Done),
                            (remaining, emitted, fail_after, true),
                        ))
                    }
                }
            },
        );
        Box::pin(stream)
    }
}

#[cfg(test)]
#[path = "adversarial_transport_tests.rs"]
mod adversarial_transport_tests;

#[cfg(test)]
#[path = "adversarial_egress_tests.rs"]
mod adversarial_egress_tests;

#[cfg(test)]
mod lib_tests;
