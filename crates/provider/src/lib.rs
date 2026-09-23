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
use faktor_core::model::{ModelCapabilities, PricingSnapshot, ReasoningMode};
use futures::Stream;
#[cfg(test)]
use futures::StreamExt;

use crate::catalog::{ModelCatalogEntry, PricingState, Provenance, QualityPrior};

pub mod catalog;

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

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderErrorKind {
    Network,
    Timeout,
    RateLimited,
    BadRequest,
    Auth,
    Server,
    Cancelled,
    Malformed,
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
        let row = catalog::builtin::lookup(self.id(), model);
        match row {
            Some(row) => ModelCatalogEntry {
                provider: self.identity().instance_id,
                model: model.to_string(),
                capabilities: self.capabilities(model),
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
                capabilities: self.capabilities(model),
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

    fn known_models(&self) -> Vec<String> {
        // Delegate: an instance-wrapped adapter reports its OWN model list
        // (the trait default of ["default"] would collapse every custom
        // endpoint's real catalog on the daemon's routing graph).
        self.inner.known_models()
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
        /// The wire reports a single input count with NO cache split at all
        /// (ollama `prompt_eval_count`): the conservative category is
        /// uncached = the reported count, cache lines zero.
        NoCacheDetail,
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
            WireFamily::NoCacheDetail => &[
                "counts_map_uncached_total",
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
}

impl CatalogProvider {
    /// A catalog with no known models yet (`default_caps` answers
    /// `capabilities` for models the catalog does not name).
    pub fn new(id: impl Into<String>, default_caps: ModelCapabilities) -> Self {
        Self {
            id: id.into(),
            default_caps,
            models: Vec::new(),
        }
    }

    /// Add one known model with its own capabilities. Appends after the
    /// models added before it: `known_models()` reports insertion order.
    pub fn add_model(&mut self, model: impl Into<String>, caps: ModelCapabilities) -> &mut Self {
        self.models.push((model.into(), caps));
        self
    }
}

impl Provider for CatalogProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        self.models
            .iter()
            .find(|(m, _)| m == model)
            .map(|(_, caps)| caps.clone())
            .unwrap_or_else(|| self.default_caps.clone())
    }

    fn known_models(&self) -> Vec<String> {
        self.models.iter().map(|(m, _)| m.clone()).collect()
    }

    fn stream(&self, _req: GenericAgentRequest) -> ProviderStream {
        Box::pin(futures::stream::empty())
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
        // the agent loop forever re-executing the same calls).
        let script = std::mem::take(&mut *self.script.lock().unwrap());
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
mod tests {
    use super::*;
    use faktor_core::cancellation::CancellationToken;
    use faktor_core::error::ErrorKind;
    use faktor_core::id::{OpId, SessionId};

    fn req() -> GenericAgentRequest {
        GenericAgentRequest {
            model: "m".into(),
            system: "s".into(),
            messages: vec![],
            tools: vec![],
            max_output: None,
            reasoning: None,
            stream: true,
            meta: RequestMeta {
                operation_id: OpId::new(1),
                session_id: SessionId::new(1),
                provider: "fake".into(),
                attempt: 0,
                deadline_ms: 1000,
                cancellation: CancellationToken::new(),
            },
        }
    }

    #[test]
    fn canonical_usage_fields_round_trip_with_defaults() {
        // Audit Phase-1 item C: a usage frame is the canonical category
        // split — uncached input, cache lines and output. Older additive
        // shapes are gone: a legacy `tokens_in` envelope is NOT a usage
        // frame (it carries no canonical categories).
        let missing = serde_json::json!({"type": "usage"});
        let usage: ProviderChunk = serde_json::from_value(missing).unwrap();
        match usage {
            ProviderChunk::Usage(usage) => {
                assert!(usage.is_zero());
                assert_eq!(usage.reported_cost, None);
                assert_eq!(usage.request_id, None);
            }
            other => panic!("usage frame mis-parsed: {other:?}"),
        }
        // And the canonical fields round-trip.
        let rich = ProviderChunk::Usage(CanonicalUsage {
            uncached_input_tokens: 10,
            cache_read_tokens: 7,
            cache_write_tokens: 2,
            output_tokens: 5,
            reasoning_tokens: 3,
            reported_cost: Some(ReportedCost {
                micro_usd: 42,
                currency: ReportedCurrency::Usd,
                source: ReportedCostSource::ProviderUsage,
                request_id: Some("req_1".into()),
            }),
            request_id: Some("req_1".into()),
        });
        let back: ProviderChunk =
            serde_json::from_value(serde_json::to_value(&rich).unwrap()).unwrap();
        assert_eq!(back, rich);
    }

    #[test]
    fn total_including_cache_splits_and_refuses_hostile_rows() {
        // Wire total INCLUDING cached input: 1000 total / 600 cached ->
        // uncached 400 + cache_read 600, output untouched.
        let u = CanonicalUsage::from_total_including_cache(1000, 600, 0, 50, 0).unwrap();
        assert_eq!(
            u,
            CanonicalUsage {
                uncached_input_tokens: 400,
                cache_read_tokens: 600,
                ..CanonicalUsage::new(0, 0, 0, 50)
            }
        );
        // A full cache hit is legal (uncached 0)...
        let hit = CanonicalUsage::from_total_including_cache(600, 600, 0, 50, 0).unwrap();
        assert_eq!(hit.uncached_input_tokens, 0);
        // ...but cache > total is hostile and typed, never saturated.
        assert_eq!(
            CanonicalUsage::from_total_including_cache(100, 600, 0, 50, 0).unwrap_err(),
            UsageSplitError::CacheReadsExceedInput {
                total_input_tokens: 100,
                cache_read_tokens: 600,
            }
        );
        // Reasoning must be an informational subset of output.
        assert_eq!(
            CanonicalUsage::from_total_including_cache(1000, 0, 0, 20, 30).unwrap_err(),
            UsageSplitError::ReasoningExceedsOutput {
                output_tokens: 20,
                reasoning_tokens: 30,
            }
        );
        // Cache writes must fit the remainder after reads.
        assert_eq!(
            CanonicalUsage::from_total_including_cache(500, 400, 200, 50, 0).unwrap_err(),
            UsageSplitError::CacheWritesExceedInput {
                total_input_tokens: 500,
                cache_write_tokens: 200,
            }
        );
        // A wire that reports reasoning must have folded it into output.
        let split = CanonicalUsage {
            output_tokens: 0,
            reasoning_tokens: 3,
            ..CanonicalUsage::ZERO
        };
        assert_eq!(
            split.validate().unwrap_err(),
            UsageSplitError::ReasoningExceedsOutput {
                output_tokens: 0,
                reasoning_tokens: 3,
            }
        );
    }

    #[test]
    fn capability_validation_rejects_tools_on_tool_less_model() {
        let caps = ModelCapabilities {
            tools: false,
            ..Default::default()
        };
        let mut r = req();
        r.tools.push(ToolSpec {
            name: "read_file".into(),
            description: "d".into(),
            input_schema: serde_json::json!({}),
        });
        let err = CapabilityValidator::validate(&r, &caps).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Malformed);
    }

    #[test]
    fn capability_validation_rejects_reasoning_and_oversized_output() {
        let caps = ModelCapabilities {
            reasoning: false,
            thinking: false,
            max_output: 1000,
            ..Default::default()
        };
        let mut r = req();
        r.reasoning = Some(ReasoningMode::High);
        assert!(CapabilityValidator::validate(&r, &caps).is_err());
        r.reasoning = None;
        r.max_output = Some(2000);
        let err = CapabilityValidator::validate(&r, &caps).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Oversized);
        r.max_output = Some(1000);
        assert!(CapabilityValidator::validate(&r, &caps).is_ok());
    }

    /// Resolved media is vision-gated at capability validation (before any
    /// adapter lowering) and is a typed refusal for a vision-less model.
    #[test]
    fn capability_validation_refuses_resolved_media_for_visionless_models() {
        let mut r = req();
        r.messages.push(RequestMessage {
            role: Role::User,
            content: vec![
                ContentPart::text("what is this?"),
                ContentPart::image_data("image/png", vec![0x89, b'P', b'N', b'G']).unwrap(),
            ],
        });
        let visionless = ModelCapabilities {
            vision: false,
            ..Default::default()
        };
        let err = CapabilityValidator::validate(&r, &visionless).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Malformed);
        assert!(err.message.contains("does not support vision"), "{err}");
        let vision = ModelCapabilities {
            vision: true,
            ..Default::default()
        };
        CapabilityValidator::validate(&r, &vision).expect("vision model admits media");
        // The legacy URL-shaped part is untouched by the media gate (it
        // carries no bytes and predates the attachment path).
        assert!(ContentPart::image_data("text/plain", vec![1]).is_err());
        assert!(ContentPart::image_data("IMAGE/PNG", vec![1]).is_err());
        assert!(ContentPart::image_data("image/png", Vec::new()).is_err());
    }

    #[test]
    fn media_bytes_json_never_carries_expanded_bytes_and_refuses_decode() {
        let bytes = vec![0x89, b'P', b'N', b'G', 1, 2, 3, 4];
        let media = MediaBytes::new(bytes.clone()).unwrap();
        let json = serde_json::to_value(&media).unwrap();
        assert_eq!(json["size"], 8);
        assert_eq!(
            json["digest"],
            serde_json::json!(media.digest().to_hex()),
            "the JSON carrier is content-addressed"
        );
        let obj = json.as_object().unwrap();
        assert_eq!(obj.len(), 2, "only digest+size may cross the JSON boundary");
        assert!(
            obj.values().all(|v| !v.is_array()),
            "no expanded byte array may appear in JSON: {json}"
        );
        // Decoding bytes from JSON is impossible by construction: durable
        // state stores attachment ids and requests re-resolve from the CAS.
        assert!(serde_json::from_value::<MediaBytes>(json).is_err());
        // A whole message containing media serializes to the digest carrier
        // and can never be decoded back into expanded bytes.
        let message = RequestMessage {
            role: Role::User,
            content: vec![
                ContentPart::text("see"),
                ContentPart::image_data("image/png", media.as_slice().to_vec()).unwrap(),
            ],
        };
        let msg_json = serde_json::to_string(&message).unwrap();
        assert!(!msg_json.contains("base64"), "{msg_json}");
        assert!(serde_json::from_str::<RequestMessage>(&msg_json).is_err());
        // The structural hard cap holds; per-provider bounds are separate.
        assert!(MediaBytes::new(vec![0u8; MAX_MEDIA_BYTES_HARD + 1]).is_err());
    }

    /// The adapter delivery gate: vision, mime allowlist and the provider's
    /// own per-image bound, checked BEFORE any wire byte.
    #[test]
    fn media_delivery_gate_checks_vision_mime_and_size() {
        let mut r = req();
        r.messages.push(RequestMessage {
            role: Role::User,
            content: vec![
                ContentPart::image_data("image/png", vec![0x89, b'P', b'N', b'G']).unwrap(),
            ],
        });
        let vision = ModelCapabilities {
            vision: true,
            ..Default::default()
        };
        validate_media_delivery(&r, &vision, 8).expect("png of 4 bytes under an 8-byte bound");
        // Too small a provider bound refuses typedly (no truncation).
        let err = validate_media_delivery(&r, &vision, 3).unwrap_err();
        assert_eq!(err.kind, ProviderErrorKind::BadRequest);
        assert!(err.message.contains("provider bound"), "{err}");
        // Vision-less refuses even when the size fits.
        let err = validate_media_delivery(
            &r,
            &ModelCapabilities {
                vision: false,
                ..Default::default()
            },
            1 << 20,
        )
        .unwrap_err();
        assert_eq!(err.kind, ProviderErrorKind::BadRequest);
        assert!(err.message.contains("vision"), "{err}");
        // An image mime outside the closed allowlist refuses typedly.
        let mut svg = req();
        svg.messages.push(RequestMessage {
            role: Role::User,
            content: vec![ContentPart::image_data("image/svg+xml", b"<svg/>".to_vec()).unwrap()],
        });
        let err = validate_media_delivery(&svg, &vision, 1 << 20).unwrap_err();
        assert!(err.message.contains("image/svg+xml"), "{err}");
    }

    /// Document parts: construction is a closed-allowlist typed gate, and
    /// the adapter delivery gate refuses document-less models, junk mimes,
    /// per-document overruns and request-wide overruns BEFORE any wire byte.
    #[test]
    fn document_parts_are_typed_and_delivery_gated() {
        let pdf = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\ntrailer\n%%EOF".to_vec();
        // Construction refuses non-documents, zero bytes and hostile names.
        assert!(ContentPart::file_data("application/zip", Some("a.zip"), pdf.clone()).is_err());
        assert!(ContentPart::file_data("application/pdf", Some("a.pdf"), Vec::new()).is_err());
        assert!(ContentPart::file_data("application/pdf", Some("../x.pdf"), pdf.clone()).is_err());
        assert!(ContentPart::file_data("APPLICATION/PDF", None, pdf.clone()).is_err());
        // `text/plain` must be UTF-8 to lower byte-exactly everywhere.
        assert!(ContentPart::file_data("text/plain", None, vec![0xff, 0xfe]).is_err());
        ContentPart::file_data("text/plain", None, b"plain text".to_vec()).unwrap();
        let part =
            ContentPart::file_data("application/pdf", Some("spec.pdf"), pdf.clone()).unwrap();
        match &part.kind {
            ContentKind::FileData {
                mime,
                filename,
                data,
            } => {
                assert_eq!(mime, "application/pdf");
                assert_eq!(filename.as_deref(), Some("spec.pdf"));
                assert_eq!(data.as_slice(), pdf.as_slice());
            }
            other => panic!("expected a FileData part, got {other:?}"),
        }
        // The JSON carrier keeps only digest+size and never decodes back.
        let json = serde_json::to_string(&part).unwrap();
        assert!(!json.contains("base64"), "{json}");
        assert!(!json.contains("PDF"), "bytes must never ride JSON: {json}");
        assert!(serde_json::from_str::<ContentPart>(&json).is_err());

        let req_with_doc = || {
            let mut r = req();
            r.messages.push(RequestMessage {
                role: Role::User,
                content: vec![ContentPart::file_data(
                    "application/pdf",
                    Some("spec.pdf"),
                    pdf.clone(),
                )
                .unwrap()],
            });
            r
        };
        let r = req_with_doc();
        validate_document_delivery(&r, true, 1 << 20).expect("a capable model admits the PDF");
        let err = validate_document_delivery(&r, false, 1 << 20).unwrap_err();
        assert_eq!(err.kind, ProviderErrorKind::BadRequest);
        assert!(err.message.contains("document input"), "{err}");
        let err = validate_document_delivery(&r, true, 4).unwrap_err();
        assert!(err.message.contains("provider bound"), "{err}");
        // A directly-built hostile mime (bypassing `file_data`) is refused
        // by the delivery gate's allowlist.
        let mut hostile = req_with_doc();
        hostile.messages[0].content[0].kind = ContentKind::FileData {
            mime: "application/zip".into(),
            filename: None,
            data: MediaBytes::new(b"PK".to_vec()).unwrap(),
        };
        let err = validate_document_delivery(&hostile, true, 1 << 20).unwrap_err();
        assert!(err.message.contains("not deliverable"), "{err}");
        // The request-wide bound refuses a set whose parts each fit.
        let mut two = req_with_doc();
        two.messages.push(RequestMessage {
            role: Role::User,
            content: vec![ContentPart::file_data(
                "application/pdf",
                None,
                vec![b'x'; MAX_REQUEST_DOCUMENT_BYTES + 1],
            )
            .unwrap()],
        });
        let err = validate_document_delivery(&two, true, MAX_MEDIA_BYTES_HARD).unwrap_err();
        assert!(
            err.message.contains("request bound"),
            "the request-wide document bound must apply: {err}"
        );
        // Capability validation: a document-less provider refuses the part
        // at request construction, a capable one admits it.
        let err = CapabilityValidator::validate_documents(&r, false).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Malformed);
        assert!(
            err.message.contains("does not support document input"),
            "{err}"
        );
        CapabilityValidator::validate_documents(&r, true).expect("capable provider admits");
        CapabilityValidator::validate_documents(&req(), false).expect("no documents, no gate");
    }

    /// Embedding request/response construction enforces every bound with
    /// typed errors and the response can never be silently truncated.
    #[test]
    fn embedding_request_and_response_bounds_are_typed() {
        assert!(EmbeddingRequest::new("m", vec![]).is_err());
        assert!(EmbeddingRequest::new("", vec!["a".into()]).is_err());
        assert!(EmbeddingRequest::new("m", vec![String::new()]).is_err());
        assert!(EmbeddingRequest::new("m", vec!["a".into(); MAX_EMBEDDING_INPUTS + 1]).is_err());
        assert!(
            EmbeddingRequest::new("m", vec!["x".repeat(MAX_EMBEDDING_INPUT_BYTES + 1)]).is_err()
        );
        let big = "x".repeat(MAX_EMBEDDING_TOTAL_INPUT_BYTES / MAX_EMBEDDING_INPUTS + 1);
        assert!(EmbeddingRequest::new("m", vec![big; MAX_EMBEDDING_INPUTS]).is_err());
        let ok = EmbeddingRequest::new("embed-1", vec!["alpha".into(), "beta".into()]).unwrap();
        assert_eq!(ok.inputs.len(), 2);
        // Lineage is additive: a request without meta reports no deadline
        // and equality ignores meta (retry scripts keep matching).
        assert_eq!(ok.deadline_ms(), 0);
        assert!(ok.meta.is_none());
        let meta = RequestMeta {
            operation_id: OpId::new(7),
            session_id: SessionId::new(3),
            provider: "p".into(),
            attempt: 0,
            deadline_ms: 1234,
            cancellation: CancellationToken::new(),
        };
        let with_meta = EmbeddingRequest::new("embed-1", vec!["alpha".into(), "beta".into()])
            .unwrap()
            .with_meta(meta);
        assert_eq!(with_meta.deadline_ms(), 1234);
        assert_eq!(with_meta.meta.as_ref().unwrap().operation_id, OpId::new(7));
        assert_eq!(with_meta, ok, "meta is lineage, never request identity");

        assert!(EmbeddingResponse::new(vec![]).is_err());
        assert!(EmbeddingResponse::new(vec![vec![]]).is_err());
        assert!(EmbeddingResponse::new(vec![vec![0.0, 0.0], vec![0.0]]).is_err());
        assert!(EmbeddingResponse::new(vec![vec![f32::NAN]]).is_err());
        assert!(EmbeddingResponse::new(vec![vec![0.0; MAX_EMBEDDING_DIMENSIONS + 1]]).is_err());
        let response = EmbeddingResponse::new(vec![vec![0.25, -0.5], vec![1.0, 0.0]]).unwrap();
        response.validate_for(2).unwrap();
        assert!(response.validate_for(3).is_err());
    }

    /// The fake provider's embedding surface: capability flag + scripted
    /// responses consumed in order with captured requests.
    #[test]
    fn fake_provider_embeddings_are_capability_gated_scripted_and_captured() {
        let unsupported = FakeProvider::new("f", ModelCapabilities::default());
        assert!(!unsupported.supports_embeddings("e"));
        let err = unsupported
            .embed(EmbeddingRequest::new("e", vec!["a".into()]).unwrap())
            .unwrap_err();
        assert_eq!(err.kind, ProviderErrorKind::BadRequest);

        let fake = FakeProvider::new("f", ModelCapabilities::default()).with_embeddings(
            "e",
            vec![Ok(EmbeddingResponse::new(vec![vec![1.0, 2.0]]).unwrap())],
        );
        assert!(fake.supports_embeddings("e"));
        assert!(!fake.supports_embeddings("other"));
        let out = fake
            .embed(EmbeddingRequest::new("e", vec!["a".into()]).unwrap())
            .unwrap();
        assert_eq!(out.vectors, vec![vec![1.0, 2.0]]);
        assert_eq!(fake.embedding_call_count(), 1);
        assert_eq!(fake.embedding_requests()[0].inputs, vec!["a".to_string()]);
        // The script is consumed exactly once: an exhausted fake refuses.
        let err = fake
            .embed(EmbeddingRequest::new("e", vec!["b".into()]).unwrap())
            .unwrap_err();
        assert_eq!(err.kind, ProviderErrorKind::Malformed);
        assert_eq!(fake.embedding_call_count(), 2);
    }

    #[test]
    fn normalizer_never_carries_internal_meta() {
        let r = req();
        let n = RequestNormalizer::normalize(&r);
        // Internal fields (op/session/deadline/cancellation/attempt) simply
        // do not exist on the normalized request — they cannot leak.
        let json = serde_json::to_value(&n).unwrap();
        let obj = json.as_object().unwrap();
        for leaked in [
            "operation_id",
            "session_id",
            "attempt",
            "deadline_ms",
            "cancellation",
        ] {
            assert!(!obj.contains_key(leaked), "internal field {leaked} leaked");
        }
        assert_eq!(obj.len(), 7, "frozen normalized shape");
    }

    #[test]
    fn registry_dynamic_and_capability_source_of_truth() {
        let mut reg = ProviderRegistry::new();
        let fake = FakeProvider::with_script(
            "test",
            ModelCapabilities {
                context: 262144,
                tools: true,
                ..Default::default()
            },
            vec![ScriptedResponse::Text("hi".into()), ScriptedResponse::End],
        );
        reg.try_register(Arc::new(fake)).unwrap();
        assert_eq!(reg.ids(), vec!["test"]);
        let caps = reg.capabilities("test", "qwen3.8").unwrap();
        assert_eq!(caps.context, 262144);
        assert!(caps.tools);
        assert!(reg.capabilities("missing", "x").is_none());
    }

    #[test]
    fn two_openai_compatible_instances_both_register_and_resolve_by_id() {
        // Two OpenAI-compatible endpoints (family "openai") with distinct
        // configured instance ids: on the old registry both inserted under
        // the family id and the second silently overwrote the first.
        let mut reg = ProviderRegistry::new();
        let caps = ModelCapabilities::default();
        for id in ["a-proxy", "b-proxy"] {
            let fake = FakeProvider::with_script(
                "openai",
                caps.clone(),
                vec![ScriptedResponse::Text(id.into()), ScriptedResponse::End],
            );
            reg.try_register(InstanceProvider::wrap(Arc::new(fake), id))
                .unwrap();
        }
        assert_eq!(reg.ids(), vec!["a-proxy", "b-proxy"]);
        assert_eq!(reg.len(), 2, "both instances must survive registration");
        assert!(reg.get("a-proxy").is_some());
        assert!(reg.get("b-proxy").is_some());
        assert!(reg.capabilities("a-proxy", "gpt-5").is_some());
    }

    #[test]
    fn configured_instance_id_resolves_not_family_id() {
        // A provider configured with id "corp-proxy" (family "openai"):
        // sessions configured with "corp-proxy" resolve, and the family id
        // "openai" must NOT resolve to it (the old registry keyed the
        // adapter's family id, so custom ids never looked up).
        let fake = FakeProvider::with_script(
            "openai",
            ModelCapabilities::default(),
            vec![ScriptedResponse::Text("hi".into()), ScriptedResponse::End],
        );
        let wrapped = InstanceProvider::wrap(Arc::new(fake), "corp-proxy");
        assert_eq!(wrapped.identity().instance_id, "corp-proxy");
        assert_eq!(wrapped.identity().family, "openai");
        assert_eq!(
            wrapped.id(),
            "openai",
            "family id stays for capability queries"
        );

        let mut reg = ProviderRegistry::new();
        reg.try_register(wrapped).unwrap();
        assert_eq!(reg.ids(), vec!["corp-proxy"]);
        assert!(reg.get("corp-proxy").is_some());
        assert!(
            reg.get("openai").is_none(),
            "family id must not shadow the instance"
        );
    }

    #[test]
    fn default_identity_is_instance_per_family() {
        // An unwrapped adapter gets instance_id == family: existing single-
        // instance deployments keep resolving exactly as before.
        let fake = FakeProvider::new("ollama", ModelCapabilities::default());
        assert_eq!(fake.identity(), ProviderIdentity::new("ollama", "ollama"));
    }

    #[test]
    fn provider_error_retryability_matches_kind() {
        assert!(ProviderErrorKind::Network.retryable());
        assert!(ProviderErrorKind::Timeout.retryable());
        assert!(ProviderErrorKind::RateLimited.retryable());
        assert!(ProviderErrorKind::Server.retryable());
        assert!(!ProviderErrorKind::BadRequest.retryable());
        assert!(!ProviderErrorKind::Auth.retryable());
        assert!(!ProviderErrorKind::Cancelled.retryable());
        assert!(!ProviderErrorKind::Malformed.retryable());
    }

    #[tokio::test]
    async fn fake_provider_stream_contract() {
        let caps = ModelCapabilities::default();
        let fake = FakeProvider::with_script(
            "f",
            caps,
            vec![
                ScriptedResponse::Text("a".into()),
                ScriptedResponse::ToolCall {
                    id: "c1".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({"path": "x"}),
                },
                ScriptedResponse::End,
            ],
        );
        let chunks: Vec<_> = fake.stream(req()).collect().await;
        assert_eq!(chunks.len(), 3);
        assert!(matches!(chunks[0], Ok(ProviderChunk::Text { .. })));
        assert!(matches!(chunks[1], Ok(ProviderChunk::ToolCall { .. })));
        assert_eq!(chunks[2], Ok(ProviderChunk::Done));
    }

    #[tokio::test]
    async fn fake_provider_dies_mid_stream() {
        let fake = FakeProvider::die_mid_stream("f", ModelCapabilities::default());
        let chunks: Vec<_> = fake.stream(req()).collect().await;
        assert!(chunks.len() == 2 && chunks[0].is_ok() && chunks[1].is_err());
        assert_eq!(
            chunks[1].as_ref().unwrap_err().kind,
            ProviderErrorKind::Network
        );
    }

    #[test]
    fn duplicate_instance_id_is_conflict_and_first_entry_wins() {
        let caps_a = ModelCapabilities {
            context: 1000,
            ..Default::default()
        };
        let caps_b = ModelCapabilities {
            context: 2000,
            ..Default::default()
        };
        let first = FakeProvider::with_script(
            "family",
            caps_a.clone(),
            vec![
                ScriptedResponse::Text("first".into()),
                ScriptedResponse::End,
            ],
        );
        let first: Arc<dyn Provider> = Arc::new(first);
        let second = FakeProvider::with_script(
            "family",
            caps_b,
            vec![
                ScriptedResponse::Text("second".into()),
                ScriptedResponse::End,
            ],
        );
        let mut reg = ProviderRegistry::new();
        assert!(reg.try_register(first.clone()).is_ok());
        // Same id, a DIFFERENT instance (fresh Arc over equal content) is a
        // typed Conflict and the FIRST registration stays untouched.
        let err = reg.try_register(Arc::new(second)).unwrap_err();
        assert_eq!(
            err.kind,
            faktor_core::error::ErrorKind::Conflict,
            "duplicate instance id is a Conflict"
        );
        assert_eq!(reg.len(), 1, "the first entry is never replaced");
        let caps = reg.capabilities("family", "m").unwrap();
        assert_eq!(
            caps.context, 1000,
            "capabilities come from the FIRST registration"
        );
        assert!(
            Arc::ptr_eq(&reg.get("family").unwrap(), &first),
            "the stored instance is exactly the first Arc, never a replacement"
        );
        // Same id, the SAME instance (a clone of the first Arc) is an
        // idempotent no-op: Ok, nothing changes, nothing is duplicated.
        assert!(reg.try_register(first.clone()).is_ok());
        assert_eq!(reg.len(), 1);
        assert_eq!(reg.capabilities("family", "m").unwrap().context, 1000);
        assert!(
            Arc::ptr_eq(&reg.get("family").unwrap(), &first),
            "the idempotent re-registration kept the original instance"
        );
        // Distinct instance ids of the same family still coexist.
        let wrapped = InstanceProvider::wrap(
            Arc::new(FakeProvider::new("family", ModelCapabilities::default())),
            "second-instance",
        );
        assert!(reg.try_register(wrapped).is_ok());
        assert_eq!(reg.len(), 2);
    }

    #[test]
    fn hostile_and_case_variant_instance_ids_are_typed_refusals() {
        use faktor_core::error::ErrorKind;
        // Empty instance ids never enter the map.
        let mut reg = ProviderRegistry::new();
        let empty = Arc::new(FakeProvider::with_script(
            "",
            ModelCapabilities::default(),
            vec![ScriptedResponse::End],
        ));
        let err = reg.try_register(empty).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Malformed, "{err}");
        assert_eq!(reg.len(), 0, "the hostile registration never lands");
        // Oversized instance ids are refused with the typed Oversized kind.
        let huge = Arc::new(FakeProvider::with_script(
            &"x".repeat(MAX_PROVIDER_INSTANCE_ID_BYTES + 1),
            ModelCapabilities::default(),
            vec![ScriptedResponse::End],
        ));
        let err = reg.try_register(huge).unwrap_err();
        assert_eq!(err.kind, ErrorKind::Oversized, "{err}");
        assert_eq!(reg.len(), 0);
        // The boundary is exact: MAX bytes is still a valid id.
        let ok = Arc::new(FakeProvider::with_script(
            &"y".repeat(MAX_PROVIDER_INSTANCE_ID_BYTES),
            ModelCapabilities::default(),
            vec![ScriptedResponse::End],
        ));
        assert!(reg.try_register(ok).is_ok());
        assert_eq!(reg.len(), 1);
        // An empty id must not be recoverable through the idempotent path
        // either: hostile ids are refused before any duplicate logic runs.
        assert!(reg
            .try_register(Arc::new(FakeProvider::with_script(
                "",
                ModelCapabilities::default(),
                vec![ScriptedResponse::End],
            )))
            .is_err());

        // Duplicate-key case variants: an id that differs from a registered
        // key ONLY by case is a typed Conflict; the FIRST registration is
        // kept and the case variant never lands.
        let mut reg = ProviderRegistry::new();
        let first: Arc<dyn Provider> = Arc::new(FakeProvider::with_script(
            "Corp-Proxy",
            ModelCapabilities {
                context: 3000,
                ..Default::default()
            },
            vec![ScriptedResponse::End],
        ));
        assert!(reg.try_register(first.clone()).is_ok());
        for hostile in ["corp-proxy", "CORP-PROXY", "cOrP-pRoXy"] {
            let variant = Arc::new(FakeProvider::with_script(
                hostile,
                ModelCapabilities {
                    context: 4000,
                    ..Default::default()
                },
                vec![ScriptedResponse::End],
            ));
            let err = reg.try_register(variant).unwrap_err();
            assert_eq!(err.kind, ErrorKind::Conflict, "{hostile:?}: {err}");
        }
        assert_eq!(reg.len(), 1, "no case variant ever registers");
        assert_eq!(reg.capabilities("Corp-Proxy", "m").unwrap().context, 3000);
        assert!(
            Arc::ptr_eq(&reg.get("Corp-Proxy").unwrap(), &first),
            "the FIRST registration survives every case-variant attack"
        );
        // A genuinely distinct id in the same family still registers.
        assert!(reg
            .try_register(InstanceProvider::wrap(
                Arc::new(FakeProvider::new(
                    "Corp-Proxy",
                    ModelCapabilities::default()
                )),
                "second-proxy",
            ))
            .is_ok());
        assert_eq!(reg.len(), 2);
        // Resolution stays exact-case: hostile lookups of the registered
        // canonical key are what the registry serves, and case-swapped
        // lookups miss (there is no silent canonicalization of lookups).
        assert!(reg.get("Corp-Proxy").is_some());
        assert!(reg.get("corp-proxy").is_none());
    }

    #[test]
    fn infallible_provider_registration_api_is_gone_from_the_crate_source() {
        // P0-41 compile proof: the infallible duplicate-accepting
        // `ProviderRegistry::register` shim (warn-on-conflict) no longer
        // exists anywhere in this crate's source. If a future wave re-adds
        // an infallible registration API, this test fails at compile/run
        // time by scanning the crate sources.
        let mut scanned = 0usize;
        let reg_sig = ["fn ", "register"].concat();
        let warn_marker = ["provider ", "registration ", "rejected"].concat();
        for entry in std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/src")).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            scanned += 1;
            let src = std::fs::read_to_string(&path).unwrap();
            for line in src.lines() {
                // A registration method for Arc<dyn Provider> that does NOT
                // return Result is the infallible footgun.
                if line.contains(&reg_sig) && line.contains("Arc<dyn Provider>") {
                    assert!(
                        line.contains("-> Result"),
                        "{}:{}: infallible provider registration API re-added: {line}",
                        path.display(),
                        line
                    );
                }
                // The warn-on-duplicate shim marker must never reappear.
                assert!(
                    !line.contains(&warn_marker),
                    "{}:{}: warn-on-duplicate registration shim re-added",
                    path.display(),
                    line
                );
            }
        }
        assert!(
            scanned >= 2,
            "the source scan must actually cover the provider crate files (found {scanned})"
        );
    }

    #[test]
    fn tokenizer_mapping_matrix_rows() {
        use TokenFamily as F;
        // o200k rows: gpt-4o/gpt-4.1/o1/o3/gpt-5 families. Specific
        // prefixes must win over the generic `gpt-4` cl100k row.
        for model in [
            "gpt-4o",
            "gpt-4o-mini",
            "gpt-4o1",
            "gpt-4.1",
            "gpt-4.1-mini",
            "o1",
            "o1-mini",
            "o1-pro",
            "o3",
            "o3-mini",
            "gpt-5",
            "gpt-5-mini",
            "GPT-5",
            "gpt-5-codex",
        ] {
            let id = tokenizer_for(model, None);
            assert_eq!(
                id,
                TokenizerId::O200K_BASE,
                "{model} must map to o200k_base (got {id})"
            );
            assert_eq!(id.family, F::O200kBase);
            assert_eq!(id.version, 1, "all named rows are generation 1");
        }
        // cl100k rows: gpt-3.5 / gpt-4 (incl. turbo + 4.5 leftovers).
        for model in [
            "gpt-4",
            "gpt-4-turbo",
            "gpt-4-1106-preview",
            "gpt-4.5",
            "gpt-3.5",
            "gpt-3.5-turbo",
            "GPT-4",
        ] {
            assert_eq!(
                tokenizer_for(model, None),
                TokenizerId::CL100K_BASE,
                "{model} must map to cl100k_base"
            );
        }
        // Anthropic / Gemini / Llama rows.
        for model in [
            "claude-3-5-sonnet",
            "claude-3-7-sonnet",
            "claude-sonnet-4-5",
            "claude-opus-4-1",
            "Claude-Opus-4-1",
            "claude-haiku-4-5",
        ] {
            assert_eq!(
                tokenizer_for(model, None),
                TokenizerId::ANTHROPIC,
                "{model} must map to anthropic"
            );
        }
        for model in [
            "gemini-2.5-pro",
            "gemini-2.5-flash",
            "gemini-3",
            "Gemini-2.5-Pro",
        ] {
            assert_eq!(
                tokenizer_for(model, None),
                TokenizerId::GEMINI,
                "{model} must map to gemini"
            );
        }
        for model in [
            "llama-3.3-70b",
            "Llama-3.1-8B",
            "qwen3.8",
            "qwen3-coder",
            "qwen-2.5-72b",
        ] {
            assert_eq!(
                tokenizer_for(model, None),
                TokenizerId::LLAMA,
                "{model} must map to llama"
            );
        }
        // Slash-qualified routed model strings resolve by the last segment.
        assert_eq!(
            tokenizer_for("anthropic/claude-sonnet-4-5", None),
            TokenizerId::ANTHROPIC
        );
        assert_eq!(tokenizer_for("openai/gpt-5", None), TokenizerId::O200K_BASE);
        assert_eq!(tokenizer_for("ollama/qwen3.8", None), TokenizerId::LLAMA);
    }

    #[test]
    fn tokenizer_mapping_unknown_models_are_generic_and_never_upgraded() {
        // Unknown/empty/hostile model strings conservatively map to the
        // GenericEstimator fallback — a hint NEVER upgrades them to a named
        // family (an OpenAI-compatible endpoint is not an OpenAI tokenizer).
        for model in [
            "",
            "/",
            "default",
            "my-custom-model",
            "gpt",
            "gpt-x",
            "o",
            "o0",
            "xqwen",
            "gemma-2-9b",
            "mistral-large",
            "deepseek-chat",
            "deepseek-reasoner",
            "gpt5",
            "gpt_5",
            "😀-model",
        ] {
            let hint = Some("openai");
            assert_eq!(
                tokenizer_for(model, hint),
                TokenizerId::GENERIC_ESTIMATOR,
                "{model:?} must conservatively map to generic_estimator"
            );
        }
    }

    #[test]
    fn tokenizer_mapping_deepseek_rows_depend_on_the_deployment_hint() {
        // deepseek weights are llama-family ONLY when a llama-family runtime
        // (ollama/llama.cpp) serves them; the official deepseek API keeps
        // its own non-llama tokenizer → conservative generic fallback.
        for model in ["deepseek-chat", "deepseek-reasoner", "deepseek-v3"] {
            assert_eq!(
                tokenizer_for(model, None),
                TokenizerId::GENERIC_ESTIMATOR,
                "{model} on the official API is NOT llama-family"
            );
            assert_eq!(
                tokenizer_for(model, Some("deepseek")),
                TokenizerId::GENERIC_ESTIMATOR
            );
            assert_eq!(
                tokenizer_for(model, Some("ollama")),
                TokenizerId::LLAMA,
                "{model} under a llama-family runtime maps to llama"
            );
            assert_eq!(
                tokenizer_for(model, Some("local-llama-cpp")),
                TokenizerId::LLAMA
            );
        }
    }

    #[test]
    fn tokenizer_id_is_total_order_display_and_serde_stable() {
        use TokenFamily as F;
        // Derived Ord: variant declaration order, then version. A sorted
        // vec is deterministic across processes (cache keys rely on it).
        let mut ids = vec![
            TokenizerId::GENERIC_ESTIMATOR,
            TokenizerId {
                family: F::O200kBase,
                version: 2,
            },
            TokenizerId::CL100K_BASE,
            TokenizerId::LLAMA,
            TokenizerId::GEMINI,
            TokenizerId::ANTHROPIC,
            TokenizerId::O200K_BASE,
        ];
        let expected = vec![
            TokenizerId::O200K_BASE,
            TokenizerId {
                family: F::O200kBase,
                version: 2,
            },
            TokenizerId::CL100K_BASE,
            TokenizerId::ANTHROPIC,
            TokenizerId::GEMINI,
            TokenizerId::LLAMA,
            TokenizerId::GENERIC_ESTIMATOR,
        ];
        ids.sort();
        assert_eq!(ids, expected, "deterministic total order");
        assert!(
            TokenizerId::O200K_BASE
                < TokenizerId {
                    family: F::O200kBase,
                    version: 2,
                }
        );
        // Display.
        assert_eq!(TokenizerId::O200K_BASE.to_string(), "o200k_base@v1");
        assert_eq!(
            TokenizerId::GENERIC_ESTIMATOR.to_string(),
            "generic_estimator@v1"
        );
        assert_eq!(TokenFamily::Anthropic.to_string(), "anthropic");
        // Serde round-trips with the frozen wire names (snake_case).
        let json = serde_json::to_value(TokenizerId::O200K_BASE).unwrap();
        assert_eq!(
            json,
            serde_json::json!({"family": "o200k_base", "version": 1})
        );
        assert_eq!(
            serde_json::from_value::<TokenizerId>(json).unwrap(),
            TokenizerId::O200K_BASE
        );
        assert_eq!(
            serde_json::from_value::<TokenizerId>(serde_json::json!(
                {"family": "cl100k_base", "version": 1}
            ))
            .unwrap(),
            TokenizerId::CL100K_BASE
        );
        // Hostile json: unknown family is a serde error, never a silent map.
        assert!(serde_json::from_value::<TokenizerId>(serde_json::json!(
            {"family": "not_a_family", "version": 1}
        ))
        .is_err());
        // Default is the conservative fallback.
        assert_eq!(TokenizerId::default(), TokenizerId::GENERIC_ESTIMATOR);
    }

    #[test]
    fn tokenizer_mapping_case_and_whitespace_hostile_rows() {
        // Case folding and trim are part of the mapping contract; whitespace
        // INSIDE the name is not stripped (hostile rows stay generic).
        assert_eq!(tokenizer_for("  GPT-5  ", None), TokenizerId::O200K_BASE);
        assert_eq!(
            tokenizer_for("\tclaude-sonnet-4", None),
            TokenizerId::ANTHROPIC
        );
        assert_eq!(tokenizer_for("gpt-5\n", None), TokenizerId::O200K_BASE);
        assert_eq!(tokenizer_for("gpt-4o\n ", None), TokenizerId::O200K_BASE);
        assert_eq!(
            tokenizer_for("gpt-4o", None),
            tokenizer_for(" GPT-4O\n", None),
            "leading whitespace, case and trailing newlines never change the identity"
        );
        assert_eq!(
            tokenizer_for("qwen3.8", None),
            tokenizer_for("ollama/qwen3.8", None),
            "the provider prefix before the last '/' never changes the identity"
        );
    }
}
