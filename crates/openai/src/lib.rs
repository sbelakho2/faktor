//! faktor-openai — OpenAI Chat Completions and the native OpenAI Responses
//! API (spec §12). The adapter owns provider quirks; the agent never sees
//! them. The wire serializer produces exactly the frozen OpenAI shapes —
//! internal option names can never leak onto the wire (locked by tests).
//!
//! Two selectable wire families ride [`OpenAiFamily`]:
//!
//! - [`OpenAiFamily::Chat`]: `POST /chat/completions` with chat-shaped
//!   bodies (`messages`), the chat SSE parser, and `prompt_tokens` usage
//!   semantics. This stays the default for OpenAI-COMPATIBLE endpoints
//!   (DeepSeek, gateways, local servers) that rarely implement `/responses`.
//! - [`OpenAiFamily::Responses`]: `POST /responses` with the native item
//!   protocol (`input` items, top-level `function_call` /
//!   `function_call_output` rows, flattened function tools) and the
//!   `response.*` event parser: reasoning deltas, item-keyed function-call
//!   assembly (fragmented arguments, multiple parallel calls), canonical
//!   usage from `response.completed`, and typed terminal semantics.
//!
//! The family is chosen at construction ([`OpenAiConfig::chat`] /
//! [`OpenAiConfig::responses`] / the `family` field); both families share
//! the same transport, deadline, cancellation, and HTTP retry
//! classification behavior.

use std::collections::HashMap;
use std::fmt;
use std::pin::Pin;
use std::sync::Arc;

use faktor_core::model::ModelCapabilities;
use faktor_provider::classify;
use faktor_provider::config::{bearer_auth_header, ExtraHeaders, ProviderConfigError};
#[cfg(test)]
use faktor_provider::egress::PolicyCheckedHttpTransport;
use faktor_provider::egress::{execute_post_json_with_extras, EgressError, HttpTransport};
use faktor_provider::egress::{BudgetComponent, CheckedResponse, ResponseBudget};
use faktor_provider::sanitize::{auth_shaped_text, ErrorScrubber};
use faktor_provider::transport::{
    guarded_lines, utf8_line_stream, StreamDeadlines, MAX_LINE_BYTES, PROVIDER_CEILING_MS,
};
use faktor_security::secret::SecretValue;
use futures::Stream;

/// Stream hang controls: first-byte / idle bounds from the transport
/// defaults (audit round 9). The OVERALL bound now rides the operation
/// deadline the runtime stamped into `RequestMeta::deadline_ms` (audit
/// round 15): `0` keeps streams unbounded overall (defaults only), any
/// positive value caps the stream's whole lifetime at
/// `min(deadline_ms, PROVIDER_CEILING_MS)` — a stuck server can never
/// outlive the operation that started the request.
fn stream_deadlines(request: &GenericAgentRequest) -> StreamDeadlines {
    let mut deadlines = StreamDeadlines::default();
    if request.meta.deadline_ms > 0 {
        deadlines.overall_ms = request.meta.deadline_ms.min(PROVIDER_CEILING_MS);
    }
    deadlines
}
use faktor_provider::{
    CanonicalUsage, ContentKind, ContentPart, GenericAgentRequest, Provider, ProviderChunk,
    ProviderError, ProviderErrorKind, ProviderStream, RequestMessage, Role,
};

/// Documented OpenAI per-image ceiling for chat/responses image parts
/// (raw bytes before base64 inflation). The agent/admission paths stay at
/// the daemon default unless this adapter's value authorizes more.
pub const OPENAI_MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenAiFamily {
    /// POST /chat/completions (OpenAI, DeepSeek, most compatible servers).
    Chat,
    /// POST /responses (native Responses codec: serializer + SSE parser).
    Responses,
}

/// Adapter-level quirks that change wire lowering. DeepSeek-style endpoints
/// require the assistant's prior `reasoning_content` to be replayed on
/// subsequent tool iterations and a non-null `content` next to `tool_calls`;
/// plain OpenAI endpoints must never see those extensions. Defaults are the
/// plain OpenAI behavior (all flags off).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OpenAiQuirks {
    /// Replay assistant reasoning as a message-level `reasoning_content`
    /// field (Chat Completions style) instead of content blocks.
    pub requires_reasoning_replay_with_tools: bool,
    /// Always emit a `content` field (empty string when there is no text)
    /// on assistant messages that carry `tool_calls`.
    pub requires_assistant_content_with_tool_calls: bool,
}

/// OpenAI adapter configuration. `api_key` is wrapped in
/// [`SecretValue`] so no derived/custom formatting can print it; the custom
/// [`fmt::Debug`] below keeps every other field inspectable.
#[derive(Clone)]
pub struct OpenAiConfig {
    pub base_url: String,
    pub api_key: Option<SecretValue>,
    pub family: OpenAiFamily,
    /// Explicit capability overrides per model; defaults are conservative.
    pub models: HashMap<String, ModelCapabilities>,
}

impl fmt::Debug for OpenAiConfig {
    /// Redacting `Debug`: the API key never renders (its wrapper prints
    /// `SecretValue([redacted])`); everything else stays diagnosable.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OpenAiConfig")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key)
            .field("family", &self.family)
            .field("models", &self.models)
            .finish()
    }
}

impl OpenAiConfig {
    pub fn chat(base_url: impl Into<String>, api_key: Option<SecretValue>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key,
            family: OpenAiFamily::Chat,
            models: HashMap::new(),
        }
    }

    /// Declare additional served model ids, each carrying the endpoint's
    /// generic OpenAI-compatible capabilities (the same values an unlisted
    /// id would get). Session/model validation and routing then SEE the
    /// declared ids instead of silently substituting them.
    pub fn with_models<I: IntoIterator<Item = String>>(mut self, models: I) -> Self {
        for model in models {
            self.models
                .entry(model)
                .or_insert_with(Self::generic_capabilities);
        }
        self
    }

    /// The generic capability set for an unlisted model on an
    /// OpenAI-compatible endpoint.
    pub fn generic_capabilities() -> ModelCapabilities {
        ModelCapabilities {
            context: 128_000,
            max_output: 16_384,
            tools: true,
            parallel_tools: true,
            thinking: false,
            vision: true,
            json_schema: true,
            streaming: true,
            embeddings: false,
            reasoning: true,
        }
    }

    /// Selects the Responses family (native Responses codec).
    pub fn responses(base_url: impl Into<String>, api_key: Option<SecretValue>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key,
            family: OpenAiFamily::Responses,
            models: HashMap::new(),
        }
    }

    pub fn with_model(mut self, model: &str, caps: ModelCapabilities) -> Self {
        self.models.insert(model.to_string(), caps);
        self
    }

    pub fn with_default_caps(mut self, caps: ModelCapabilities) -> Self {
        self.models.insert("*".to_string(), caps);
        self
    }
}

/// Authorization headers for a bearer API key (empty map when keyless).
/// The key is exposed only for the length of this construction, into a
/// zeroized transient inside [`bearer_auth_header`]. An unencodable
/// credential is a typed [`ProviderConfigError`] — the request MUST fail,
/// never silently go anonymous.
pub fn authorization_headers(
    api_key: Option<&SecretValue>,
) -> Result<reqwest::header::HeaderMap, ProviderConfigError> {
    let mut h = reqwest::header::HeaderMap::new();
    if let Some(key) = api_key {
        h.insert(reqwest::header::AUTHORIZATION, bearer_auth_header(key)?);
    }
    Ok(h)
}

/// Shared HTTP-status classifier for BOTH wire families, delegated to the
/// provider crate's single classifier ([`provider_error_for_http`]): the
/// status taxonomy plus the structured body hint (`error.code` /
/// `error.status` / `error.type`, never message text) decide the kind, and
/// retryability comes from the provider crate's
/// [`ProviderErrorKind::retryable`], so the chat and responses paths can
/// never drift apart. The error message is the scrubbed, bounded diagnostic
/// ([`ErrorScrubber::diagnostic`]): raw upstream bodies (which can echo
/// request credentials) never reach the error.
fn classify_http_status(
    status: reqwest::StatusCode,
    body: String,
    scrubber: &ErrorScrubber,
) -> ProviderError {
    classify::provider_error_for_http_with_scrubber(status.as_u16(), &body, scrubber)
}

/// Hard bound on the provider-native error `code` echoed into a
/// [`ProviderError`] from an in-stream event. The code is scrubbed first
/// (it is upstream-controlled text); the byte bound keeps a hostile event
/// from inflating the error.
const MAX_ERROR_CODE_BYTES: usize = 128;

/// Typed `Malformed` error for an SSE data line that is not valid JSON. The
/// raw line is hostile: it is scrubbed with the request's registered
/// credentials plus the frozen patterns and bounded; an auth-shaped line
/// (e.g. a truncated `authentication_error` payload) withholds the upstream
/// text entirely — the in-stream equivalent of the 401/403 body rule.
fn bad_sse_line_error(data: &str, scrubber: &ErrorScrubber) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Malformed,
        scrubber.event_diagnostic("bad SSE line", data, auth_shaped_text(data)),
    )
}

/// Hard bound on one classified error body: `Response::text()` would buffer
/// a hostile endpoint's unbounded body into RAM, so the classifier reads at
/// most this many bytes (chunked reads, partial body dropped) and appends a
/// typed truncation note. The status-derived classification is unaffected.
pub const OPENAI_ERROR_BODY_MAX_BYTES: usize = 64 * 1024;

/// Await response HEADERS under the stream's existing first-byte deadline
/// (default 60 s), capped by the overall deadline when the operation set
/// one. The egress client only bounds connect, so without this a server
/// that accepts and never answers would pin the stream forever. `0` on both
/// knobs keeps the historical unbounded behavior, but
/// [`stream_deadlines`] always starts from the documented defaults.
fn request_head_timeout_ms(deadlines: StreamDeadlines) -> u64 {
    match (deadlines.first_byte_ms, deadlines.overall_ms) {
        (0, 0) => 0,
        (0, overall) => overall,
        (first, 0) => first,
        (first, overall) => first.min(overall),
    }
}

/// Execute one stream request, bounding the wait for response headers by
/// [`request_head_timeout_ms`] (a typed `Timeout` on breach).
async fn execute_with_head_timeout(
    fut: impl std::future::Future<Output = Result<CheckedResponse, EgressError>>,
    deadlines: StreamDeadlines,
) -> Result<CheckedResponse, ProviderError> {
    let bound_ms = request_head_timeout_ms(deadlines);
    if bound_ms == 0 {
        return fut.await.map_err(ProviderError::from);
    }
    match tokio::time::timeout(std::time::Duration::from_millis(bound_ms), fut).await {
        Ok(result) => result.map_err(ProviderError::from),
        Err(_) => Err(ProviderError::new(
            ProviderErrorKind::Timeout,
            format!("openai response headers exceeded the {bound_ms} ms server bound"),
        )),
    }
}

/// Outcome of one bounded error-body read (never a full materialization).
enum ErrorBodyRead {
    /// Complete body within the byte cap.
    Complete(Vec<u8>),
    /// The byte cap was reached; the rest of the body is dropped.
    Truncated(Vec<u8>),
    /// No complete read inside the wall-clock bound.
    Stalled,
}

/// Read an error body under a hard BYTE cap and a wall-clock bound through
/// the shared budget-aware reader: at most `cap` bytes retained (the rest
/// is dropped, never buffered), and a typed note appended when the body was
/// truncated or the read stalled. The HTTP status still classifies the
/// error. The budget is REQUIRED — an adapter never reads a response body
/// directly.
async fn read_error_body_bounded(resp: CheckedResponse, cap: usize, bound_ms: u64) -> String {
    let budget_ms = if bound_ms == 0 {
        PROVIDER_CEILING_MS
    } else {
        bound_ms
    };
    let budget = ResponseBudget::from_millis(budget_ms, budget_ms, budget_ms, cap as u64, None);
    let read = async {
        let mut body = resp.into_budgeted(budget);
        let mut out: Vec<u8> = Vec::new();
        loop {
            match body.next_chunk().await {
                Ok(Some(chunk)) => {
                    if out.len().saturating_add(chunk.len()) > cap {
                        let keep = cap.saturating_sub(out.len());
                        out.extend_from_slice(&chunk[..keep]);
                        return ErrorBodyRead::Truncated(out);
                    }
                    out.extend_from_slice(&chunk);
                }
                // The budget refused the chunk that would cross the cap:
                // exactly the historical truncation semantics.
                Err(EgressError::ResponseBudgetExceeded {
                    component: BudgetComponent::Bytes,
                    ..
                }) => return ErrorBodyRead::Truncated(out),
                // A stalled read keeps the historical "stalled" note.
                Err(EgressError::ResponseBudgetExceeded { .. }) => {
                    return ErrorBodyRead::Stalled;
                }
                Ok(None) | Err(_) => return ErrorBodyRead::Complete(out),
            }
        }
    };
    let outcome = if bound_ms == 0 {
        read.await
    } else {
        match tokio::time::timeout(std::time::Duration::from_millis(bound_ms), read).await {
            Ok(outcome) => outcome,
            Err(_) => ErrorBodyRead::Stalled,
        }
    };
    let (mut text, note) = match outcome {
        ErrorBodyRead::Complete(bytes) => (String::from_utf8_lossy(&bytes).into_owned(), None),
        ErrorBodyRead::Truncated(bytes) => (
            String::from_utf8_lossy(&bytes).into_owned(),
            Some(format!("[truncated at {cap} bytes]")),
        ),
        ErrorBodyRead::Stalled => (
            String::new(),
            Some(format!("[body read exceeded the {bound_ms} ms bound]")),
        ),
    };
    if let Some(note) = note {
        if !text.is_empty() && !text.ends_with(' ') {
            text.push(' ');
        }
        text.push_str(&note);
    }
    text
}

pub struct OpenAiProvider {
    config: OpenAiConfig,
    transport: Arc<dyn HttpTransport>,
    quirks: OpenAiQuirks,
}

impl OpenAiProvider {
    /// The ONLY production constructor. The egress transport is injected —
    /// the daemon passes the policy-checked transport built from its sandbox
    /// network gate + outbound secret scan; there is no permissive default a
    /// production path could silently fall back to.
    pub fn build(config: OpenAiConfig, transport: Arc<dyn HttpTransport>) -> Arc<dyn Provider> {
        Self::build_with_quirks(config, OpenAiQuirks::default(), transport)
    }

    /// Build with adapter-level quirks and an explicit egress transport
    /// (DeepSeek profiles set the quirks; plain OpenAI endpoints keep the
    /// defaults).
    pub fn build_with_quirks(
        config: OpenAiConfig,
        quirks: OpenAiQuirks,
        transport: Arc<dyn HttpTransport>,
    ) -> Arc<dyn Provider> {
        Arc::new(Self {
            config,
            transport,
            quirks,
        })
    }

    /// Test-only default-allow constructor: production code MUST inject a
    /// policy-checked transport, so this helper exists solely so in-crate
    /// tests can drive the adapter against local mock servers.
    #[cfg(test)]
    pub fn permissive_for_tests(config: OpenAiConfig) -> Arc<dyn Provider> {
        Self::build(config, Arc::new(PolicyCheckedHttpTransport::permissive()))
    }

    /// Test-only default-allow constructor with quirks (see
    /// [`Self::permissive_for_tests`]).
    #[cfg(test)]
    pub fn permissive_for_tests_with_quirks(
        config: OpenAiConfig,
        quirks: OpenAiQuirks,
    ) -> Arc<dyn Provider> {
        Self::build_with_quirks(
            config,
            quirks,
            Arc::new(PolicyCheckedHttpTransport::permissive()),
        )
    }

    fn wire_body(&self, req: &GenericAgentRequest) -> serde_json::Value {
        match self.config.family {
            OpenAiFamily::Chat => chat_completions_body(req, &self.quirks),
            OpenAiFamily::Responses => responses_body(req),
        }
    }
}

// ------------------------------------------------------------ chat lowering

/// Emit accumulated tool-result messages. Consecutive tool results bound to
/// the SAME call id merge into one `role: "tool"` message (crash-retry
/// duplicates stay valid for the API); distinct call ids are separate
/// messages, in order.
fn flush_tool_results(out: &mut Vec<serde_json::Value>, chain: &mut Vec<(String, String)>) {
    for (call_id, content) in chain.drain(..) {
        out.push(serde_json::json!({
            "role": "tool",
            "tool_call_id": call_id,
            "content": content,
        }));
    }
}

/// Lower one generic message into a wire message (never into tool blocks:
/// Chat Completions assistant messages carry `tool_calls`, tool results are
/// `role: "tool"` messages, reasoning rides `reasoning_content` when the
/// endpoint requires replay).
fn lower_role_message(
    m: &RequestMessage,
    parts: &[&ContentPart],
    quirks: &OpenAiQuirks,
) -> serde_json::Value {
    match m.role {
        Role::System => {
            let mut content: Vec<serde_json::Value> = Vec::new();
            for p in parts.iter().copied() {
                if let ContentKind::Text { text } = &p.kind {
                    content.push(serde_json::json!({ "type": "text", "text": text }));
                }
            }
            serde_json::json!({ "role": "system", "content": content })
        }
        Role::User => {
            let mut content: Vec<serde_json::Value> = Vec::new();
            for p in parts.iter().copied() {
                match &p.kind {
                    ContentKind::Text { text } => {
                        content.push(serde_json::json!({ "type": "text", "text": text }));
                    }
                    ContentKind::Image { url } => {
                        content.push(serde_json::json!({
                            "type": "image_url",
                            "image_url": { "url": url }
                        }));
                    }
                    ContentKind::ImageData { mime, data } => {
                        // Resolved attachment bytes: Chat Completions takes a
                        // data URL under the same `image_url` shape as a
                        // remote URL.
                        content.push(serde_json::json!({
                            "type": "image_url",
                            "image_url": { "url": data.to_data_url(mime) }
                        }));
                    }
                    ContentKind::FileData {
                        mime,
                        filename,
                        data,
                    } => {
                        // Resolved non-image DOCUMENT bytes: the documented
                        // Chat Completions file part takes the display
                        // filename plus a base64 data URL (`file_data`).
                        content.push(serde_json::json!({
                            "type": "file",
                            "file": {
                                "filename": filename.as_deref().unwrap_or("document"),
                                "file_data": data.to_data_url(mime),
                            }
                        }));
                    }
                    _ => {} // reasoning/tool parts are not user wire content
                }
            }
            serde_json::json!({ "role": "user", "content": content })
        }
        Role::Assistant => {
            let mut text: Vec<serde_json::Value> = Vec::new();
            let mut reasoning: Vec<String> = Vec::new();
            let mut calls: Vec<serde_json::Value> = Vec::new();
            for p in parts.iter().copied() {
                match &p.kind {
                    ContentKind::Text { text: t } => {
                        text.push(serde_json::json!({ "type": "text", "text": t }));
                    }
                    ContentKind::Reasoning { text: t } => reasoning.push(t.clone()),
                    ContentKind::ToolCall { id, name, input } => {
                        // arguments MUST be a JSON string of the object.
                        let arguments =
                            serde_json::to_string(&input).unwrap_or_else(|_| "{}".into());
                        calls.push(serde_json::json!({
                            "id": id,
                            "type": "function",
                            "function": { "name": name, "arguments": arguments }
                        }));
                    }
                    _ => {}
                }
            }
            let mut msg = serde_json::json!({ "role": "assistant" });
            if quirks.requires_reasoning_replay_with_tools {
                // DeepSeek-style: reasoning is replayed at message level and
                // never appears as a content block.
                if !reasoning.is_empty() {
                    msg["reasoning_content"] = serde_json::json!(reasoning.join("\n"));
                }
                let content = if text.is_empty() {
                    serde_json::Value::String(String::new())
                } else {
                    serde_json::Value::Array(text)
                };
                msg["content"] = content;
            } else if !calls.is_empty() {
                // Chat Completions: an assistant tool-calling message carries
                // TEXT-ONLY content blocks (plus tool_calls below) — reasoning
                // is skipped for families that do not replay it.
                msg["content"] = serde_json::Value::Array(text);
            } else {
                // Keep prior reasoning mapped as content blocks per the API
                // when the family supports them (no tool calls involved).
                for t in reasoning {
                    text.push(serde_json::json!({ "type": "reasoning", "text": t }));
                }
                msg["content"] = serde_json::Value::Array(text);
            }
            if !calls.is_empty() {
                msg["tool_calls"] = serde_json::Value::Array(calls);
            }
            msg
        }
    }
}

/// Lower the generic history into Chat Completions wire messages. Tool
/// results never become `{type: "tool_result"}` content blocks inside user
/// messages; they are separate `role: "tool"` messages.
fn lower_chat_messages(
    messages: &[RequestMessage],
    quirks: &OpenAiQuirks,
) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = Vec::new();
    let mut tool_chain: Vec<(String, String)> = Vec::new();
    for m in messages {
        let mut rest: Vec<&ContentPart> = Vec::new();
        let mut results: Vec<&ContentPart> = Vec::new();
        for part in &m.content {
            if matches!(part.kind, ContentKind::ToolResult { .. }) {
                results.push(part);
            } else {
                rest.push(part);
            }
        }
        if !rest.is_empty() {
            flush_tool_results(&mut out, &mut tool_chain);
            out.push(lower_role_message(m, &rest, quirks));
            flush_tool_results(&mut out, &mut tool_chain);
        }
        for part in results {
            let content = match &part.kind {
                ContentKind::ToolResult { content, .. } => content.clone(),
                _ => continue,
            };
            let call_id = part.tool_call_id.clone().unwrap_or_default();
            if let Some((last_id, last_content)) = tool_chain.last_mut() {
                if *last_id == call_id {
                    last_content.push('\n');
                    last_content.push_str(&content);
                    continue;
                }
            }
            tool_chain.push((call_id, content));
        }
    }
    flush_tool_results(&mut out, &mut tool_chain);
    out
}

// ================================================================ Responses

/// Native Responses-API request body: the model's own item protocol, never a
/// chat-shaped body.
///
/// - the cacheable system prefix rides the top-level `instructions` field;
/// - user text/images lower to `role: "user"` input items with `input_text` /
///   `input_image` parts, tool results to `function_call_output` items keyed
///   by `call_id`;
/// - assistant text lowers to a `role: "assistant"` item with `output_text`
///   parts and assistant tool calls to top-level `function_call` items (the
///   exact rows the stream parser produces and replays);
/// - function tools use the FLATTENED Responses shape (`type`/`name`/
///   `description`/`parameters`, not the Chat `function: {...}` wrapper);
/// - `stream` is always true here: this adapter's only entry point is the
///   streaming transport.
pub fn responses_body(req: &GenericAgentRequest) -> serde_json::Value {
    let mut input: Vec<serde_json::Value> = Vec::new();
    for m in &req.messages {
        match m.role {
            Role::System => {
                // Additional system turns (the primary prefix rides
                // top-level `instructions`) stay native input items.
                let mut content: Vec<serde_json::Value> = Vec::new();
                for part in &m.content {
                    if let ContentKind::Text { text } = &part.kind {
                        content.push(serde_json::json!({ "type": "input_text", "text": text }));
                    }
                }
                if !content.is_empty() {
                    input.push(serde_json::json!({ "role": "system", "content": content }));
                }
            }
            Role::User => {
                let mut content: Vec<serde_json::Value> = Vec::new();
                for part in &m.content {
                    match &part.kind {
                        ContentKind::Text { text } => {
                            if !text.is_empty() {
                                content.push(
                                    serde_json::json!({ "type": "input_text", "text": text }),
                                );
                            }
                        }
                        ContentKind::Image { url } => {
                            content.push(serde_json::json!({
                                "type": "input_image",
                                "image_url": url,
                            }));
                        }
                        ContentKind::ImageData { mime, data } => {
                            // Resolved attachment bytes: the native Responses
                            // `input_image` part accepts a data URL.
                            content.push(serde_json::json!({
                                "type": "input_image",
                                "image_url": data.to_data_url(mime),
                            }));
                        }
                        ContentKind::FileData {
                            mime,
                            filename,
                            data,
                        } => {
                            // Resolved non-image DOCUMENT bytes: the native
                            // Responses `input_file` part takes the display
                            // filename plus a base64 data URL (`file_data`).
                            content.push(serde_json::json!({
                                "type": "input_file",
                                "filename": filename.as_deref().unwrap_or("document"),
                                "file_data": data.to_data_url(mime),
                            }));
                        }
                        _ => {}
                    }
                }
                if !content.is_empty() {
                    input.push(serde_json::json!({ "role": "user", "content": content }));
                }
                for part in &m.content {
                    if let ContentKind::ToolResult { content, .. } = &part.kind {
                        if let Some(id) = part.tool_call_id.as_deref() {
                            input.push(serde_json::json!({
                                "type": "function_call_output",
                                "call_id": id,
                                "output": content,
                            }));
                        }
                    }
                }
            }
            Role::Assistant => {
                let mut content: Vec<serde_json::Value> = Vec::new();
                for part in &m.content {
                    if let ContentKind::Text { text } = &part.kind {
                        if !text.is_empty() {
                            content
                                .push(serde_json::json!({ "type": "output_text", "text": text }));
                        }
                    }
                }
                if !content.is_empty() {
                    input.push(serde_json::json!({ "role": "assistant", "content": content }));
                }
                // Function calls are TOP-LEVEL items in the native protocol
                // (never a `tool_calls` array on the assistant message).
                for part in &m.content {
                    if let ContentKind::ToolCall {
                        id,
                        name,
                        input: args,
                    } = &part.kind
                    {
                        input.push(serde_json::json!({
                            "type": "function_call",
                            "call_id": id,
                            "name": name,
                            "arguments": serde_json::to_string(args)
                                .unwrap_or_else(|_| "{}".to_string()),
                        }));
                    }
                }
            }
        }
    }
    let mut body = serde_json::json!({
        "model": req.model,
        "input": input,
        "stream": true,
    });
    if !req.system.is_empty() {
        body["instructions"] = serde_json::json!(req.system);
    }
    if !req.tools.is_empty() {
        let tools: Vec<serde_json::Value> = req
            .tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                })
            })
            .collect();
        body["tools"] = serde_json::Value::Array(tools);
        body["tool_choice"] = serde_json::json!("auto");
    }
    if let Some(max_out) = req.max_output {
        body["max_output_tokens"] = serde_json::json!(max_out);
    }
    if let Some(reasoning) = req.reasoning {
        match reasoning {
            faktor_core::model::ReasoningMode::Off => {}
            faktor_core::model::ReasoningMode::Low => {
                body["reasoning"] = serde_json::json!({ "effort": "low" });
            }
            faktor_core::model::ReasoningMode::Medium => {
                body["reasoning"] = serde_json::json!({ "effort": "medium" });
            }
            faktor_core::model::ReasoningMode::High => {
                body["reasoning"] = serde_json::json!({ "effort": "high" });
            }
        }
    }
    body
}

/// Responses SSE transport: the SAME line framing + deadlines as chat, with
/// the native `response.*` event parser.
///
/// Function-call items accumulate per ITEM id (`item_id` on fragment events,
/// deliberately distinct from the `call_id` a `function_call_output` must
/// echo); an authoritative `response.output_item.done` replaces fragments.
/// Terminal events (`response.completed`, `response.incomplete`, `[DONE]`, or
/// stream end) flush accumulated calls, then the canonical usage frame from
/// `response.completed` (`input_tokens` is the TOTAL including the cached
/// portion, exactly the chat wire semantics), then exactly one `Done`.
/// Malformed frames and error events are typed errors that END the stream —
/// no chunk ever follows a terminal item.
pub fn responses_stream(
    transport: Arc<dyn HttpTransport>,
    url: String,
    headers: reqwest::header::HeaderMap,
    body: serde_json::Value,
    deadlines: StreamDeadlines,
    cancel: Option<faktor_core::cancellation::CancellationToken>,
) -> impl Stream<Item = Result<ProviderChunk, ProviderError>> {
    use futures::StreamExt as _;
    type LineStream = Pin<Box<dyn Stream<Item = Result<String, ProviderError>> + Send>>;

    // Registered secret scrubber for this request: every credential the
    // request actually carries (authorization header, credential-named
    // query parameters) plus the frozen pattern policy.
    let scrubber = ErrorScrubber::new().with_request_credentials(&headers, &url);

    enum Stage {
        Fresh,
        Streaming {
            lines: LineStream,
            pending: std::collections::VecDeque<ProviderChunk>,
            calls: Vec<serde_json::Value>,
            /// A terminal event was seen: no line is ever read again; the
            /// queued chunks drain first, then `Done`.
            finished: bool,
        },
        Done,
    }
    futures::stream::unfold(Stage::Fresh, move |stage| {
        let transport = transport.clone();
        let url = url.clone();
        let headers = headers.clone();
        let body = body.clone();
        let cancel = cancel.clone();
        let scrubber = scrubber.clone();
        async move {
            let (mut lines, mut pending, mut calls, mut finished) = match stage {
                Stage::Fresh => {
                    let no_extra_headers = ExtraHeaders::empty();
                    let resp = execute_with_head_timeout(
                        execute_post_json_with_extras(
                            transport.as_ref(),
                            &url,
                            headers,
                            &no_extra_headers,
                            &body,
                        ),
                        deadlines,
                    )
                    .await;
                    match resp {
                        Ok(r) => {
                            let status = r.status();
                            if !status.is_success() {
                                let msg = read_error_body_bounded(
                                    r,
                                    OPENAI_ERROR_BODY_MAX_BYTES,
                                    request_head_timeout_ms(deadlines),
                                )
                                .await;
                                return Some((
                                    Err(classify_http_status(status, msg, &scrubber)),
                                    Stage::Done,
                                ));
                            }
                            let lines: LineStream = Box::pin(guarded_lines(
                                utf8_line_stream(
                                    r.stream_frames(&deadlines.response_budget()),
                                    MAX_LINE_BYTES,
                                ),
                                deadlines,
                                cancel,
                            ));
                            (lines, std::collections::VecDeque::new(), Vec::new(), false)
                        }
                        Err(e) => {
                            return Some((Err(e), Stage::Done));
                        }
                    }
                }
                Stage::Streaming {
                    lines,
                    pending,
                    calls,
                    finished,
                } => (lines, pending, calls, finished),
                Stage::Done => return None,
            };

            loop {
                if let Some(chunk) = pending.pop_front() {
                    return Some((
                        Ok(chunk),
                        Stage::Streaming {
                            lines,
                            pending,
                            calls,
                            finished,
                        },
                    ));
                }
                if finished {
                    return Some((Ok(ProviderChunk::Done), Stage::Done));
                }
                let Some(line) = lines.next().await else {
                    // The body ended before any terminal event
                    // (`response.completed`/`response.failed`/`[DONE]`): a
                    // dropped connection must be a typed failure, never a
                    // completed turn with truncated text or partial calls.
                    return Some((
                        Err(ProviderError::new(
                            ProviderErrorKind::Malformed,
                            "upstream stream ended before a terminal response event",
                        )),
                        Stage::Done,
                    ));
                };
                let line = match line {
                    Ok(l) => l,
                    Err(e) => return Some((Err(e), Stage::Done)),
                };
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data == "[DONE]" {
                    for call in calls.drain(..) {
                        if let Some(chunk) = function_call_chunk(&call) {
                            pending.push_back(chunk);
                        }
                    }
                    finished = true;
                    continue;
                }
                let ev: serde_json::Value = match serde_json::from_str(data) {
                    Ok(v) => v,
                    // A data line that is not JSON is a broken stream, not
                    // forward compatibility: typed Malformed, then done.
                    Err(_) => {
                        return Some((Err(bad_sse_line_error(data, &scrubber)), Stage::Done));
                    }
                };
                let kind = ev.get("type").and_then(|t| t.as_str()).unwrap_or("");
                match kind {
                    "response.output_text.delta" => {
                        let t = ev
                            .get("delta")
                            .and_then(|d| d.as_str())
                            .unwrap_or_default()
                            .to_string();
                        if !t.is_empty() {
                            return Some((
                                Ok(ProviderChunk::Text { text: t }),
                                Stage::Streaming {
                                    lines,
                                    pending,
                                    calls,
                                    finished,
                                },
                            ));
                        }
                    }
                    "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
                        let t = ev
                            .get("delta")
                            .and_then(|d| d.as_str())
                            .unwrap_or_default()
                            .to_string();
                        if !t.is_empty() {
                            return Some((
                                Ok(ProviderChunk::Reasoning { text: t }),
                                Stage::Streaming {
                                    lines,
                                    pending,
                                    calls,
                                    finished,
                                },
                            ));
                        }
                    }
                    "response.output_item.added" | "response.output_item.done" => {
                        if let Some(item) = ev.get("item") {
                            if item.get("type").and_then(|t| t.as_str()) == Some("function_call") {
                                let item_id =
                                    item.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                                let call_id = item
                                    .get("call_id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or_default();
                                let name = item
                                    .get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or_default();
                                let args = item
                                    .get("arguments")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or_default();
                                let slot = call_slot(&mut calls, item_id, call_id);
                                if !item_id.is_empty() {
                                    slot["item_id"] = serde_json::json!(item_id);
                                }
                                if !call_id.is_empty() {
                                    slot["call_id"] = serde_json::json!(call_id);
                                }
                                if !name.is_empty() {
                                    slot["name"] = serde_json::json!(name);
                                }
                                // A done item is AUTHORITATIVE: its full
                                // arguments replace accumulated fragments; an
                                // added item only seeds the slot.
                                if !args.is_empty() {
                                    slot["arguments"] = serde_json::json!(args);
                                }
                            }
                        }
                    }
                    "response.function_call_arguments.delta"
                    | "response.function_call_arguments.done" => {
                        let item_id = ev
                            .get("item_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default();
                        let frag = ev.get("delta").and_then(|v| v.as_str()).unwrap_or_default();
                        let done_args = ev.get("arguments").and_then(|v| v.as_str());
                        // An unknown item id has no accumulation slot: the
                        // fragment is DROPPED (it can never inject into a
                        // stored call). Event-order processing, not sequence
                        // validation.
                        if !item_id.is_empty() {
                            if let Some(slot) = find_call_slot(&mut calls, item_id) {
                                if let Some(full) = done_args.filter(|a| !a.is_empty()) {
                                    slot["arguments"] = serde_json::json!(full);
                                } else if !frag.is_empty() {
                                    let cur = slot["arguments"].as_str().unwrap_or_default();
                                    slot["arguments"] =
                                        serde_json::Value::String(format!("{cur}{frag}"));
                                }
                            }
                        }
                    }
                    "response.completed" | "response.incomplete" => {
                        let usage = match responses_usage(&ev) {
                            Ok(u) => u,
                            Err(e) => return Some((Err(e), Stage::Done)),
                        };
                        for call in calls.drain(..) {
                            if let Some(chunk) = function_call_chunk(&call) {
                                pending.push_back(chunk);
                            }
                        }
                        // Usage rides LAST so it is always the single final
                        // chunk before Done.
                        if let Some(u) = usage {
                            pending.push_back(ProviderChunk::Usage(u));
                        }
                        finished = true;
                    }
                    "response.failed" | "error" => {
                        return Some((Err(responses_event_error(&ev, &scrubber)), Stage::Done));
                    }
                    _ => {}
                }
            }
        }
    })
}

/// Get-or-create the accumulation slot for one function-call item. Slots are
/// keyed by item id then wire call id; both identifiers match so a server
/// that reuses one for the other still assembles a single call.
fn call_slot<'a>(
    calls: &'a mut Vec<serde_json::Value>,
    item_id: &str,
    call_id: &str,
) -> &'a mut serde_json::Value {
    let found = calls.iter().position(|c| {
        let id_matches = |id: &str| {
            !id.is_empty()
                && (c.get("item_id").and_then(|v| v.as_str()) == Some(id)
                    || c.get("call_id").and_then(|v| v.as_str()) == Some(id))
        };
        id_matches(item_id) || id_matches(call_id)
    });
    match found {
        Some(i) => &mut calls[i],
        None => {
            calls.push(serde_json::json!({
                "item_id": item_id,
                "call_id": call_id,
                "name": "",
                "arguments": "",
            }));
            calls.last_mut().expect("just pushed the slot")
        }
    }
}

/// Find the accumulated call slot matching `id` by item id OR wire call id.
fn find_call_slot<'a>(
    calls: &'a mut [serde_json::Value],
    id: &str,
) -> Option<&'a mut serde_json::Value> {
    calls.iter_mut().find(|c| {
        c.get("item_id").and_then(|v| v.as_str()) == Some(id)
            || c.get("call_id").and_then(|v| v.as_str()) == Some(id)
    })
}

/// Canonical usage from a Responses terminal event. Responses reports
/// `input_tokens` as the TOTAL input INCLUDING
/// `input_tokens_details.cached_tokens` (exactly the chat `prompt_tokens`
/// semantics), and `output_tokens` as the total output including the
/// informational `output_tokens_details.reasoning_tokens` subset. A hostile
/// row (cache > input, reasoning > output) is typed Malformed, never
/// saturated; an all-zero row carries nothing.
fn responses_usage(ev: &serde_json::Value) -> Result<Option<CanonicalUsage>, ProviderError> {
    // Usage rides the terminal response envelope (`response.usage`); a
    // bare event-level usage object is not part of the Responses wire and
    // is deliberately ignored.
    let response = ev.get("response");
    let Some(usage) = response.and_then(|r| r.get("usage")) else {
        return Ok(None);
    };
    let total_input_tokens = usage
        .get("input_tokens")
        .and_then(|t| t.as_u64())
        .unwrap_or(0);
    let total_output_tokens = usage
        .get("output_tokens")
        .and_then(|t| t.as_u64())
        .unwrap_or(0);
    let cache_read_tokens = usage
        .get("input_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(|t| t.as_u64())
        .unwrap_or(0);
    let reasoning_tokens = usage
        .get("output_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(|t| t.as_u64())
        .unwrap_or(0);
    if total_input_tokens == 0
        && total_output_tokens == 0
        && cache_read_tokens == 0
        && reasoning_tokens == 0
    {
        return Ok(None);
    }
    let mut canonical = CanonicalUsage::from_total_including_cache(
        total_input_tokens,
        cache_read_tokens,
        0,
        total_output_tokens,
        reasoning_tokens,
    )
    .map_err(ProviderError::from)?;
    canonical.request_id = response
        .and_then(|r| r.get("id"))
        .and_then(|i| i.as_str())
        .map(str::to_string);
    Ok(Some(canonical))
}

/// Typed error for a Responses `error` / `response.failed` event. The
/// structured `code` decides the retry class: auth failures are terminal,
/// rate-limit codes stay retryable; anything else is a terminal BadRequest.
/// Message text is never scanned for classification — but both the message
/// and the code are scrubbed (registered request credentials + frozen
/// patterns) and bounded before they enter the error, and an auth-shaped
/// event withholds the upstream message entirely, exactly like the
/// 401/403 body rule of [`ErrorScrubber::diagnostic`].
fn responses_event_error(ev: &serde_json::Value, scrubber: &ErrorScrubber) -> ProviderError {
    let err = ev
        .get("error")
        .or_else(|| ev.get("response").and_then(|r| r.get("error")));
    let raw_message = err
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .or_else(|| ev.get("message").and_then(|m| m.as_str()))
        .unwrap_or_default();
    let code = err
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        .or_else(|| ev.get("code").and_then(|c| c.as_str()))
        .unwrap_or_default();
    // Shared structured classification (the same token table HTTP bodies
    // use): the nested `error` object when present, else the event's
    // top-level fields. Unknown codes keep the terminal BadRequest kind.
    let hint = err
        .and_then(classify::value_error_hint)
        .or_else(|| classify::value_error_hint(ev));
    let kind =
        classify::provider_error_for_hint(hint.as_deref()).unwrap_or(ProviderErrorKind::BadRequest);
    let message = scrubber.event_diagnostic(
        "responses stream error",
        raw_message,
        kind == ProviderErrorKind::Auth,
    );
    let code = scrubber.scrub_bounded(code, MAX_ERROR_CODE_BYTES);
    if code.is_empty() {
        ProviderError::new(kind, message)
    } else {
        ProviderError::with_code(kind, code, message)
    }
}

/// Lower one accumulated function-call slot into its terminal chunk. The
/// generic call id is the wire `call_id` (what a `function_call_output` must
/// echo back); the item id is the fallback when a server omits `call_id`.
fn function_call_chunk(call: &serde_json::Value) -> Option<ProviderChunk> {
    let name = call
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or_default();
    if name.is_empty() {
        return None;
    }
    let call_id = call
        .get("call_id")
        .and_then(|c| c.as_str())
        .unwrap_or_default();
    let item_id = call
        .get("item_id")
        .and_then(|c| c.as_str())
        .unwrap_or_default();
    let id = if !call_id.is_empty() {
        call_id.to_string()
    } else if !item_id.is_empty() {
        item_id.to_string()
    } else {
        format!("fc_{name}")
    };
    let args: serde_json::Value = serde_json::from_str(
        call.get("arguments")
            .and_then(|a| a.as_str())
            .unwrap_or("{}"),
    )
    .unwrap_or(serde_json::Value::Null);
    Some(ProviderChunk::ToolCall {
        id,
        name: name.to_string(),
        input: args,
        complete: true,
    })
}

pub fn chat_completions_body(
    req: &GenericAgentRequest,
    quirks: &OpenAiQuirks,
) -> serde_json::Value {
    // Chat Completions carries the cacheable system prefix as the FIRST
    // message (`role: "system"`), never as a top-level field and never
    // after the conversation. OpenAI's model-gated `developer` role has no
    // capability signal on this compatible-endpoint surface, so the
    // universally implemented `system` role is the one this family emits
    // (compatible servers commonly reject `developer`).
    let mut messages: Vec<serde_json::Value> = Vec::with_capacity(req.messages.len() + 1);
    if !req.system.is_empty() {
        messages.push(serde_json::json!({ "role": "system", "content": req.system }));
    }
    messages.extend(lower_chat_messages(&req.messages, quirks));
    let tools: Vec<serde_json::Value> = req
        .tools
        .iter()
        .map(|t| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                }
            })
        })
        .collect();
    let mut body = serde_json::json!({
        "model": req.model,
        "messages": messages,
        "stream": req.stream,
    });
    if !tools.is_empty() {
        body["tools"] = serde_json::Value::Array(tools);
        body["tool_choice"] = serde_json::json!("auto");
    }
    if let Some(max_out) = req.max_output {
        body["max_tokens"] = serde_json::json!(max_out);
    }
    if let Some(reasoning) = req.reasoning {
        match reasoning {
            faktor_core::model::ReasoningMode::Off => {}
            faktor_core::model::ReasoningMode::Low => {
                body["reasoning_effort"] = serde_json::json!("low");
            }
            faktor_core::model::ReasoningMode::Medium => {
                body["reasoning_effort"] = serde_json::json!("medium");
            }
            faktor_core::model::ReasoningMode::High => {
                body["reasoning_effort"] = serde_json::json!("high");
            }
        }
    }
    body
}

impl Provider for OpenAiProvider {
    fn id(&self) -> &str {
        "openai"
    }

    fn known_models(&self) -> Vec<String> {
        let mut out: Vec<String> = self.config.models.keys().cloned().collect();
        if !out.contains(&"default".to_string()) {
            out.push("default".into());
        }
        out.sort();
        out
    }

    fn capabilities(&self, model: &str) -> ModelCapabilities {
        if let Some(caps) = self.config.models.get(model) {
            return caps.clone();
        }
        if let Some(caps) = self.config.models.get("*") {
            return caps.clone();
        }
        ModelCapabilities {
            context: 128_000,
            max_output: 16_384,
            tools: true,
            parallel_tools: true,
            thinking: false,
            vision: true,
            json_schema: true,
            streaming: true,
            embeddings: false,
            reasoning: false,
        }
    }

    fn max_image_bytes(&self) -> usize {
        OPENAI_MAX_IMAGE_BYTES
    }

    fn document_capable(&self, _model: &str) -> bool {
        // Both wired families carry document parts: Chat Completions lowers
        // `{type: "file"}`, the native Responses API lowers `input_file`.
        true
    }

    fn stream(&self, req: GenericAgentRequest) -> ProviderStream {
        let deadlines = stream_deadlines(&req);
        let cancel = req.meta.cancellation.clone();
        let transport = self.transport.clone();
        // An unencodable credential fails the request with a typed terminal
        // error. It must NEVER be skipped: a dropped Authorization header
        // would silently turn an authenticated request anonymous.
        let headers = match authorization_headers(self.config.api_key.as_ref()) {
            Ok(headers) => headers,
            Err(e) => {
                return faktor_provider::provider_error_stream(ProviderError::new(
                    ProviderErrorKind::Auth,
                    e.to_string(),
                ));
            }
        };
        // Delivery gate BEFORE any wire decision: vision capability, image
        // mime allowlist and the provider's per-image byte bound. A refusal
        // is a typed terminal error frame (nothing was sent).
        let caps = self.capabilities(&req.model);
        if let Err(e) =
            faktor_provider::validate_media_delivery(&req, &caps, self.max_image_bytes())
        {
            return faktor_provider::provider_error_stream(e);
        }
        // The vision-like document gate: the family-level capability flag,
        // the document mime allowlist, the per-document and request-wide
        // bounds — also BEFORE any wire byte.
        if let Err(e) = faktor_provider::validate_document_delivery(
            &req,
            self.document_capable(&req.model),
            self.max_document_bytes(),
        ) {
            return faktor_provider::provider_error_stream(e);
        }
        // The family decides the wire body AND the endpoint + parser pair.
        let body = self.wire_body(&req);
        match self.config.family {
            // Native Responses codec: the item-protocol serializer + the
            // `response.*` stream parser; never a chat-shaped body on
            // /responses.
            OpenAiFamily::Responses => {
                let url = format!("{}/responses", self.config.base_url);
                Box::pin(responses_stream(
                    transport,
                    url,
                    headers,
                    body,
                    deadlines,
                    Some(cancel),
                ))
            }
            OpenAiFamily::Chat => {
                let url = format!("{}/chat/completions", self.config.base_url);
                Box::pin(openai_stream(
                    transport,
                    url,
                    headers,
                    ExtraHeaders::empty(),
                    body,
                    deadlines,
                    Some(cancel),
                ))
            }
        }
    }
}

/// Flush all accumulated tool-call fragments into the pending queue (index
/// order) and pop the next complete call, if any.
fn flush_and_pop(
    accs: &mut Vec<serde_json::Value>,
    pending: &mut std::collections::VecDeque<serde_json::Value>,
) -> Option<ProviderChunk> {
    if !accs.is_empty() {
        accs.sort_by_key(|a| a.get("index").and_then(|i| i.as_u64()).unwrap_or(0));
        pending.extend(accs.drain(..));
    }
    while let Some(tc) = pending.pop_front() {
        if let Some(chunk) = tool_chunk(&tc) {
            return Some(chunk);
        }
    }
    None
}

/// OpenAI SSE transport. `extra_headers` are applied to the request before
/// send — used by the gateway path, empty elsewhere. The type is already
/// validated, so no header can be silently dropped here.
pub fn openai_stream(
    transport: Arc<dyn HttpTransport>,
    url: String,
    headers: reqwest::header::HeaderMap,
    extra_headers: ExtraHeaders,
    body: serde_json::Value,
    deadlines: StreamDeadlines,
    cancel: Option<faktor_core::cancellation::CancellationToken>,
) -> impl Stream<Item = Result<ProviderChunk, ProviderError>> {
    use futures::StreamExt as _;
    type LineStream = Pin<Box<dyn Stream<Item = Result<String, ProviderError>> + Send>>;

    // Registered secret scrubber for this request (see `responses_stream`).
    let scrubber = ErrorScrubber::new().with_request_credentials(&headers, &url);

    // None = request not sent yet; Some = streaming lines. Tool-call
    // fragments accumulate PER INDEX (parallel calls never collide); the
    // pending queue drains complete calls in index order once a finishing
    // marker (finish_reason, [DONE], or stream end) appears.
    enum Stage {
        Fresh,
        Streaming {
            lines: LineStream,
            accs: Vec<serde_json::Value>,
            pending: std::collections::VecDeque<serde_json::Value>,
        },
        Done,
    }
    futures::stream::unfold(Stage::Fresh, move |stage| {
        let transport = transport.clone();
        let url = url.clone();
        let headers = headers.clone();
        let extra_headers = extra_headers.clone();
        let body = body.clone();
        let deadlines = deadlines;
        let cancel = cancel.clone();
        let scrubber = scrubber.clone();
        async move {
            // Lazily send the request on the first poll.
            let (mut lines, mut accs, mut pending) = match stage {
                Stage::Fresh => {
                    let resp = execute_with_head_timeout(
                        execute_post_json_with_extras(
                            transport.as_ref(),
                            &url,
                            headers,
                            &extra_headers,
                            &body,
                        ),
                        deadlines,
                    )
                    .await;
                    match resp {
                        Ok(r) => {
                            let status = r.status();
                            if !status.is_success() {
                                let text = read_error_body_bounded(
                                    r,
                                    OPENAI_ERROR_BODY_MAX_BYTES,
                                    request_head_timeout_ms(deadlines),
                                )
                                .await;
                                return Some((
                                    Err(classify_http_status(status, text, &scrubber)),
                                    Stage::Done,
                                ));
                            }
                            let lines: LineStream = Box::pin(guarded_lines(
                                utf8_line_stream(
                                    r.stream_frames(&deadlines.response_budget()),
                                    MAX_LINE_BYTES,
                                ),
                                deadlines,
                                cancel.clone(),
                            ));
                            (lines, Vec::new(), std::collections::VecDeque::new())
                        }
                        Err(e) => {
                            return Some((Err(e), Stage::Done));
                        }
                    }
                }
                Stage::Streaming {
                    lines,
                    accs,
                    pending,
                } => (lines, accs, pending),
                Stage::Done => return None,
            };

            // Consume lines until a chunk is produced (or the stream ends).
            loop {
                if let Some(tc) = pending.pop_front() {
                    if let Some(chunk) = tool_chunk(&tc) {
                        return Some((
                            Ok(chunk),
                            Stage::Streaming {
                                lines,
                                accs,
                                pending,
                            },
                        ));
                    }
                    continue;
                }
                let Some(next) = lines.next().await else {
                    // The body ended WITHOUT `finish_reason` or `[DONE]`: a
                    // dropped connection mid-response must be a typed failure,
                    // never a completed turn with truncated text — and never
                    // a partially accumulated tool call.
                    return Some((
                        Err(ProviderError::new(
                            ProviderErrorKind::Malformed,
                            "upstream stream ended before finish_reason or [DONE]",
                        )),
                        Stage::Done,
                    ));
                };
                let line = match next {
                    Ok(l) => l,
                    Err(e) => return Some((Err(e), Stage::Done)),
                };
                let line = line.trim();
                if !line.starts_with("data:") {
                    continue;
                }
                let data = line[5..].trim();
                if data == "[DONE]" {
                    if let Some(chunk) = flush_and_pop(&mut accs, &mut pending) {
                        return Some((
                            Ok(chunk),
                            Stage::Streaming {
                                lines,
                                accs,
                                pending,
                            },
                        ));
                    }
                    return Some((Ok(ProviderChunk::Done), Stage::Done));
                }
                let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
                    return Some((Err(bad_sse_line_error(data, &scrubber)), Stage::Done));
                };
                match parse_chat_chunk(&value, &mut accs, &mut pending) {
                    Ok(Some(chunk)) => {
                        let stage = match chunk {
                            ProviderChunk::Done => Stage::Done,
                            _ => Stage::Streaming {
                                lines,
                                accs,
                                pending,
                            },
                        };
                        return Some((Ok(chunk), stage));
                    }
                    Ok(None) => {}
                    // A hostile usage row (cache lines exceeding the input
                    // total, reasoning exceeding output) fails the stream
                    // with a typed Malformed error — never a silent zero.
                    Err(e) => return Some((Err(e), Stage::Done)),
                }
            }
        }
    })
}

/// Parse one SSE frame. Tool-call deltas accumulate PER `index` into `accs`
/// (parallel calls never clobber each other); a finishing marker
/// (`finish_reason: tool_calls|stop`) flushes complete calls into `pending`
/// and returns the first one. Frames without a chunk yield `Ok(None)`;
/// impossible usage rows (cache > input total, reasoning > output) yield a
/// typed Malformed error.
fn parse_chat_chunk(
    value: &serde_json::Value,
    accs: &mut Vec<serde_json::Value>,
    pending: &mut std::collections::VecDeque<serde_json::Value>,
) -> Result<Option<ProviderChunk>, ProviderError> {
    if let Some(choices) = value.get("choices").and_then(|c| c.as_array()) {
        let Some(choice) = choices.first() else {
            return Ok(None);
        };
        let Some(delta) = choice.get("delta") else {
            return Ok(None);
        };
        if let Some(text) = delta.get("content").and_then(|c| c.as_str()) {
            if !text.is_empty() {
                return Ok(Some(ProviderChunk::Text {
                    text: text.to_string(),
                }));
            }
        }
        if let Some(reasoning) = delta.get("reasoning_content").and_then(|c| c.as_str()) {
            if !reasoning.is_empty() {
                return Ok(Some(ProviderChunk::Reasoning {
                    text: reasoning.to_string(),
                }));
            }
        }
        if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
            for tc in tool_calls {
                let index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                let id = tc.get("id").and_then(|i| i.as_str()).unwrap_or_default();
                let name = tc
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .unwrap_or_default();
                let arguments = tc
                    .get("function")
                    .and_then(|f| f.get("arguments"))
                    .and_then(|a| a.as_str())
                    .unwrap_or_default();
                // Index-keyed slot: fragments of index N never mix with
                // fragments of a simultaneous call at index M.
                let slot = accs
                    .iter()
                    .position(|a| a.get("index").and_then(|i| i.as_u64()) == Some(index));
                let slot = match slot {
                    Some(s) => s,
                    None => {
                        accs.push(serde_json::json!({
                            "index": index,
                            "id": if id.is_empty() {
                                format!("call_{index}")
                            } else {
                                id.to_string()
                            },
                            "name": String::new(),
                            "arguments": String::new(),
                        }));
                        accs.len() - 1
                    }
                };
                if !id.is_empty() {
                    accs[slot]["id"] = serde_json::json!(id);
                }
                if !name.is_empty() {
                    accs[slot]["name"] = serde_json::json!(name);
                }
                if !arguments.is_empty() {
                    let cur = accs[slot]["arguments"].as_str().unwrap_or("").to_string();
                    accs[slot]["arguments"] = serde_json::json!(format!("{cur}{arguments}"));
                }
            }
        }
        // A call completes ONLY at a finishing marker — fragments keep
        // accumulating until then (finish_reason may ride a frame that
        // carries no tool_calls at all).
        if let Some(reason) = choice.get("finish_reason").and_then(|r| r.as_str()) {
            if reason == "tool_calls" || reason == "stop" {
                return Ok(flush_and_pop(accs, pending));
            }
        }
    }
    if let Some(usage) = value.get("usage") {
        // Wire semantics (audit Phase-1 item C): Chat Completions reports
        // `prompt_tokens` as the TOTAL input INCLUDING the cached portion;
        // `prompt_tokens_details.cached_tokens` splits the cached reads out
        // for cost attribution (they are billed at the cheaper cache line).
        // `completion_tokens` is the total output INCLUDING reasoning;
        // `completion_tokens_details.reasoning_tokens` reports the same
        // tokens as an informational subset — never billed a second time.
        // A row whose cache line exceeds the input total (or whose
        // reasoning exceeds the output) is impossible: typed Malformed.
        let total_input_tokens = usage
            .get("prompt_tokens")
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        let total_output_tokens = usage
            .get("completion_tokens")
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        let cache_read_tokens = usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        let reasoning_tokens = usage
            .get("completion_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        if total_input_tokens == 0
            && total_output_tokens == 0
            && cache_read_tokens == 0
            && reasoning_tokens == 0
        {
            return Ok(None);
        }
        let mut canonical = CanonicalUsage::from_total_including_cache(
            total_input_tokens,
            cache_read_tokens,
            0,
            total_output_tokens,
            reasoning_tokens,
        )
        .map_err(ProviderError::from)?;
        // Chat Completions frames carry the provider request id at the top
        // level of the same SSE object as the usage envelope.
        canonical.request_id = value
            .get("id")
            .and_then(|i| i.as_str())
            .map(|s| s.to_string());
        return Ok(Some(ProviderChunk::Usage(canonical)));
    }
    Ok(None)
}

fn tool_chunk(tc: &serde_json::Value) -> Option<ProviderChunk> {
    let id = tc
        .get("id")
        .and_then(|i| i.as_str())
        .unwrap_or("call_0")
        .to_string();
    let name = tc
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or_default()
        .to_string();
    if name.is_empty() {
        return None;
    }
    let arguments = tc.get("arguments").and_then(|a| a.as_str()).unwrap_or("");
    let input = if arguments.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(arguments).unwrap_or(serde_json::Value::Null)
    };
    Some(ProviderChunk::ToolCall {
        id,
        name,
        input,
        complete: true,
    })
}

/// Build messages from a generic request (shared by compatible adapters).
pub fn messages_from(req: &GenericAgentRequest) -> Vec<RequestMessage> {
    req.messages.clone()
}

#[cfg(test)]
#[path = "chat_lowering_tests.rs"]
mod chat_lowering_tests;

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
