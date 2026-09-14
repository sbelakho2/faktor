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
use std::pin::Pin;
use std::sync::Arc;

use faktor_core::model::ModelCapabilities;
use faktor_provider::egress::{
    execute_post_json_with_extras, HttpTransport, PolicyCheckedHttpTransport,
};
use faktor_provider::transport::{
    guarded_lines, utf8_line_stream, StreamDeadlines, MAX_LINE_BYTES, PROVIDER_CEILING_MS,
};
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

#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub family: OpenAiFamily,
    /// Explicit capability overrides per model; defaults are conservative.
    pub models: HashMap<String, ModelCapabilities>,
}

impl OpenAiConfig {
    pub fn chat(base_url: impl Into<String>, api_key: Option<String>) -> Self {
        Self {
            base_url: base_url.into(),
            api_key,
            family: OpenAiFamily::Chat,
            models: HashMap::new(),
        }
    }

    /// Selects the Responses family (native Responses codec).
    pub fn responses(base_url: impl Into<String>, api_key: Option<String>) -> Self {
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

/// The standard permissive checked transport (no destination policy
/// installed => default-allow, documented). The daemon config sites must
/// replace this with `PolicyCheckedHttpTransport::with_policy(...)` once
/// the sandbox network gate is threaded into provider construction.
fn default_transport() -> Arc<dyn HttpTransport> {
    Arc::new(PolicyCheckedHttpTransport::permissive())
}

/// Authorization headers for a bearer API key (empty map when keyless).
pub fn authorization_headers(api_key: Option<&str>) -> reqwest::header::HeaderMap {
    let mut h = reqwest::header::HeaderMap::new();
    if let Some(key) = api_key {
        if let Ok(v) = format!("Bearer {key}").parse() {
            h.insert("authorization", v);
        }
    }
    h
}

/// Shared HTTP-status classifier for BOTH wire families. Retryability comes
/// from the provider crate's [`ProviderErrorKind::retryable`] (429 and 5xx
/// retry with backoff; auth failures and every other 4xx are terminal), so
/// the chat and responses paths can never drift apart.
fn classify_http_status(status: reqwest::StatusCode, body: String) -> ProviderError {
    let kind = match status.as_u16() {
        401 | 403 => ProviderErrorKind::Auth,
        429 => ProviderErrorKind::RateLimited,
        408 | 504 => ProviderErrorKind::Timeout,
        500..=599 => ProviderErrorKind::Server,
        _ => ProviderErrorKind::BadRequest,
    };
    ProviderError::with_code(kind, status.as_u16().to_string(), body)
}

pub struct OpenAiProvider {
    config: OpenAiConfig,
    transport: Arc<dyn HttpTransport>,
    quirks: OpenAiQuirks,
}

impl OpenAiProvider {
    pub fn build(config: OpenAiConfig) -> Arc<dyn Provider> {
        Self::build_with_quirks(config, OpenAiQuirks::default())
    }

    /// Build with adapter-level quirks (DeepSeek profiles set these; plain
    /// OpenAI endpoints keep the defaults).
    pub fn build_with_quirks(config: OpenAiConfig, quirks: OpenAiQuirks) -> Arc<dyn Provider> {
        Self::build_with_quirks_and_transport(config, quirks, default_transport())
    }

    /// Build with an injected transport (policy-checked in production,
    /// mock in tests).
    pub fn build_with_transport(
        config: OpenAiConfig,
        transport: Arc<dyn HttpTransport>,
    ) -> Arc<dyn Provider> {
        Self::build_with_quirks_and_transport(config, OpenAiQuirks::default(), transport)
    }

    /// Full constructor: quirks + explicit egress transport.
    pub fn build_with_quirks_and_transport(
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
        async move {
            let (mut lines, mut pending, mut calls, mut finished) = match stage {
                Stage::Fresh => {
                    let resp = execute_post_json_with_extras(
                        transport.as_ref(),
                        &url,
                        headers,
                        &[],
                        &body,
                    )
                    .await;
                    match resp {
                        Ok(r) => {
                            let status = r.status();
                            if !status.is_success() {
                                let msg = r.text().await.unwrap_or_default();
                                return Some((Err(classify_http_status(status, msg)), Stage::Done));
                            }
                            let lines: LineStream = Box::pin(guarded_lines(
                                utf8_line_stream(r.bytes_stream(), MAX_LINE_BYTES),
                                deadlines,
                                cancel,
                            ));
                            (lines, std::collections::VecDeque::new(), Vec::new(), false)
                        }
                        Err(e) => {
                            return Some((Err(ProviderError::from(e)), Stage::Done));
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
                    // The inner stream is FINISHED: never re-poll it. Any
                    // accumulated calls still complete; then exactly one Done.
                    for call in calls.drain(..) {
                        if let Some(chunk) = function_call_chunk(&call) {
                            pending.push_back(chunk);
                        }
                    }
                    finished = true;
                    continue;
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
                        return Some((
                            Err(ProviderError::new(
                                ProviderErrorKind::Malformed,
                                format!("bad SSE line: {data:?}"),
                            )),
                            Stage::Done,
                        ));
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
                        return Some((Err(responses_event_error(&ev)), Stage::Done));
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
/// Message text is never scanned for classification.
fn responses_event_error(ev: &serde_json::Value) -> ProviderError {
    let err = ev
        .get("error")
        .or_else(|| ev.get("response").and_then(|r| r.get("error")));
    let message = err
        .and_then(|e| e.get("message"))
        .and_then(|m| m.as_str())
        .or_else(|| ev.get("message").and_then(|m| m.as_str()))
        .unwrap_or("responses stream error")
        .to_string();
    let code = err
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        .or_else(|| ev.get("code").and_then(|c| c.as_str()))
        .unwrap_or_default();
    let folded: String = code
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect();
    let kind = if folded.starts_with("ratelimit")
        || folded.starts_with("toomany")
        || folded.starts_with("quota")
        || folded.starts_with("resourceexhausted")
        || folded.starts_with("throttl")
    {
        ProviderErrorKind::RateLimited
    } else if folded.contains("auth") || folded.contains("apikey") || folded == "invalidkey" {
        ProviderErrorKind::Auth
    } else {
        ProviderErrorKind::BadRequest
    };
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
    let messages = lower_chat_messages(&req.messages, quirks);
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
        let headers = authorization_headers(self.config.api_key.as_deref());
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
                    Vec::new(),
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

/// OpenAI SSE transport. `extra_headers` (name/value) are applied to the
/// request before send — used by the gateway path, empty elsewhere.
pub fn openai_stream(
    transport: Arc<dyn HttpTransport>,
    url: String,
    headers: reqwest::header::HeaderMap,
    extra_headers: Vec<(String, String)>,
    body: serde_json::Value,
    deadlines: StreamDeadlines,
    cancel: Option<faktor_core::cancellation::CancellationToken>,
) -> impl Stream<Item = Result<ProviderChunk, ProviderError>> {
    use futures::StreamExt as _;
    type LineStream = Pin<Box<dyn Stream<Item = Result<String, ProviderError>> + Send>>;

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
        async move {
            // Lazily send the request on the first poll.
            let (mut lines, mut accs, mut pending) = match stage {
                Stage::Fresh => {
                    let resp = execute_post_json_with_extras(
                        transport.as_ref(),
                        &url,
                        headers,
                        &extra_headers,
                        &body,
                    )
                    .await;
                    match resp {
                        Ok(r) => {
                            let status = r.status();
                            if !status.is_success() {
                                let text = r.text().await.unwrap_or_default();
                                return Some((
                                    Err(classify_http_status(status, text)),
                                    Stage::Done,
                                ));
                            }
                            let lines: LineStream = Box::pin(guarded_lines(
                                utf8_line_stream(r.bytes_stream(), MAX_LINE_BYTES),
                                deadlines,
                                cancel.clone(),
                            ));
                            (lines, Vec::new(), std::collections::VecDeque::new())
                        }
                        Err(e) => {
                            return Some((Err(ProviderError::from(e)), Stage::Done));
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
                    // Stream end: a server that never sent finish_reason must
                    // still complete its tool calls here. The inner stream is
                    // FINISHED — never re-poll it (audit streams suite).
                    let drained: LineStream = Box::pin(futures::stream::empty());
                    if let Some(chunk) = flush_and_pop(&mut accs, &mut pending) {
                        return Some((
                            Ok(chunk),
                            Stage::Streaming {
                                lines: drained,
                                accs,
                                pending,
                            },
                        ));
                    }
                    return Some((Ok(ProviderChunk::Done), Stage::Done));
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
                    return Some((
                        Err(ProviderError::new(
                            ProviderErrorKind::Malformed,
                            format!("bad SSE line: {data:?}"),
                        )),
                        Stage::Done,
                    ));
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
mod tests {
    use super::*;
    use faktor_core::cancellation::CancellationToken;
    use faktor_core::id::{OpId, SessionId};
    use faktor_provider::egress::MockHttpTransport;
    use faktor_provider::testing::{sse_body, MockAction, MockServer};
    use faktor_provider::{ContentPart, RequestMessage, RequestMeta, ToolSpec};
    use faktor_security::destination::DestinationPolicy;
    use futures::StreamExt;

    fn req(model: &str) -> GenericAgentRequest {
        GenericAgentRequest {
            model: model.into(),
            system: "sys".into(),
            messages: vec![RequestMessage {
                role: Role::User,
                content: vec![ContentPart::text("hi")],
            }],
            tools: vec![ToolSpec {
                name: "read_file".into(),
                description: "read".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }],
            max_output: Some(1000),
            reasoning: None,
            stream: true,
            meta: RequestMeta {
                operation_id: OpId::new(1),
                session_id: SessionId::new(1),
                provider: "openai".into(),
                attempt: 0,
                deadline_ms: 5000,
                cancellation: CancellationToken::new(),
            },
        }
    }

    #[tokio::test]
    async fn wire_body_has_no_internal_leakage() {
        let server = MockServer::new();
        server.route(
            "POST",
            "/chat/completions",
            MockAction::AssertThenRespond {
                status: 200,
                body: sse_body(&[
                    serde_json::json!({"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}),
                ]),
                assert: Arc::new(|body: &serde_json::Value| {
                    // Frozen wire shape: exactly the OpenAI fields.
                    assert_eq!(body["model"], "m1");
                    assert!(body["messages"].is_array());
                    assert_eq!(body["messages"][0]["role"], "user");
                    assert!(body["stream"].as_bool().unwrap());
                    assert_eq!(body["max_tokens"], 1000);
                    assert_eq!(body["tools"][0]["type"], "function");
                    assert_eq!(body["tool_choice"], "auto");
                    // Internal fields must NEVER appear on the wire.
                    for leaked in ["operation_id", "session_id", "attempt", "deadline_ms", "cancellation", "system", "op_id"] {
                        assert!(!body.as_object().unwrap().contains_key(leaked), "{leaked} leaked!");
                    }
                    // The `system` prompt must not leak as a top-level field.
                    assert!(!body.as_object().unwrap().contains_key("system"));
                }),
            },
        );
        let base = server.base_url().await;
        let provider = OpenAiProvider::build(OpenAiConfig::chat(base, Some("sk-test".into())));
        let mut stream = provider.stream(req("m1"));
        let mut texts = String::new();
        while let Some(chunk) = stream.next().await {
            match chunk.unwrap() {
                ProviderChunk::Text { text } => texts.push_str(&text),
                ProviderChunk::Done => break,
                _ => {}
            }
        }
        assert_eq!(texts, "ok");
    }

    /// Resolved image attachments lower BYTE-EXACTLY on both wire families:
    /// Chat gets `image_url` with a base64 data URL, Responses gets
    /// `input_image` with the same data URL. The base64 and media type are
    /// asserted exactly, not approximately.
    #[tokio::test]
    async fn image_data_lowers_byte_exact_on_chat_and_responses() {
        let png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 7, 8, 9];
        let media = faktor_provider::MediaBytes::new(png.clone()).unwrap();
        let expected_url = media.to_data_url("image/png");
        assert_eq!(
            expected_url,
            format!("data:image/png;base64,{}", media.to_base64()),
            "the data URL is the standard-alphabet base64 of the raw bytes"
        );
        // Chat family.
        let server = MockServer::new();
        let expected_chat = expected_url.clone();
        server.route(
            "POST",
            "/chat/completions",
            MockAction::AssertThenRespond {
                status: 200,
                body: sse_body(&[
                    serde_json::json!({"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}),
                ]),
                assert: Arc::new(move |body: &serde_json::Value| {
                    assert_eq!(
                        body["messages"][1]["content"],
                        serde_json::json!([
                            { "type": "text", "text": "look" },
                            {
                                "type": "image_url",
                                "image_url": { "url": expected_chat }
                            }
                        ]),
                        "Chat image lowering must be byte-exact"
                    );
                }),
            },
        );
        let base = server.base_url().await;
        let provider = OpenAiProvider::build(OpenAiConfig::chat(base, None));
        assert_eq!(provider.max_image_bytes(), OPENAI_MAX_IMAGE_BYTES);
        let mut r = req("m1");
        r.messages.push(RequestMessage {
            role: Role::User,
            content: vec![
                ContentPart::text("look"),
                ContentPart::image_data("image/png", png.clone()).unwrap(),
            ],
        });
        let mut stream = provider.stream(r);
        while let Some(chunk) = stream.next().await {
            if matches!(chunk.unwrap(), ProviderChunk::Done) {
                break;
            }
        }
        assert_eq!(server.request_count(), 1);

        // Responses family: the native item protocol.
        let server = MockServer::new();
        let expected_responses = expected_url.clone();
        server.route(
            "POST",
            "/responses",
            MockAction::AssertThenRespond {
                status: 200,
                body: sse_body(&[
                    serde_json::json!({"type":"response.output_text.delta","delta":"ok"}),
                    serde_json::json!({"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}),
                ]),
                assert: Arc::new(move |body: &serde_json::Value| {
                    assert_eq!(
                        body["input"][1]["content"],
                        serde_json::json!([
                            { "type": "input_text", "text": "look" },
                            {
                                "type": "input_image",
                                "image_url": expected_responses
                            }
                        ]),
                        "Responses image lowering must be byte-exact"
                    );
                }),
            },
        );
        let base = server.base_url().await;
        let provider = OpenAiProvider::build(OpenAiConfig::responses(base, None));
        let mut r = req("m1");
        r.messages.push(RequestMessage {
            role: Role::User,
            content: vec![
                ContentPart::text("look"),
                ContentPart::image_data("image/png", png).unwrap(),
            ],
        });
        let mut stream = provider.stream(r);
        while let Some(chunk) = stream.next().await {
            if matches!(chunk.unwrap(), ProviderChunk::Done) {
                break;
            }
        }
        assert_eq!(server.request_count(), 1);
    }

    /// Resolved DOCUMENT attachments lower BYTE-EXACTLY on both wire
    /// families: Chat gets `{type: "file"}` with the display filename and a
    /// base64 data URL, Responses gets `input_file` with the same fields.
    #[tokio::test]
    async fn document_data_lowers_byte_exact_on_chat_and_responses() {
        let pdf: Vec<u8> = b"%PDF-1.4\n1 0 obj\n<<>>\nendobj\ntrailer\n%%EOF".to_vec();
        let media = faktor_provider::MediaBytes::new(pdf.clone()).unwrap();
        let expected_url = media.to_data_url("application/pdf");
        // Chat family.
        let server = MockServer::new();
        let expected_chat = expected_url.clone();
        server.route(
            "POST",
            "/chat/completions",
            MockAction::AssertThenRespond {
                status: 200,
                body: sse_body(&[
                    serde_json::json!({"choices":[{"delta":{"content":"ok"},"finish_reason":"stop"}]}),
                ]),
                assert: Arc::new(move |body: &serde_json::Value| {
                    assert_eq!(
                        body["messages"][1]["content"],
                        serde_json::json!([
                            { "type": "text", "text": "read" },
                            {
                                "type": "file",
                                "file": {
                                    "filename": "spec.pdf",
                                    "file_data": expected_chat
                                }
                            }
                        ]),
                        "Chat document lowering must be byte-exact"
                    );
                }),
            },
        );
        let base = server.base_url().await;
        let provider = OpenAiProvider::build(OpenAiConfig::chat(base, None));
        assert!(provider.document_capable("m1"));
        let mut r = req("m1");
        r.messages.push(RequestMessage {
            role: Role::User,
            content: vec![
                ContentPart::text("read"),
                ContentPart::file_data("application/pdf", Some("spec.pdf"), pdf.clone()).unwrap(),
            ],
        });
        let mut stream = provider.stream(r);
        while let Some(chunk) = stream.next().await {
            if matches!(chunk.unwrap(), ProviderChunk::Done) {
                break;
            }
        }
        assert_eq!(server.request_count(), 1);

        // Responses family.
        let server = MockServer::new();
        let expected_responses = expected_url.clone();
        server.route(
            "POST",
            "/responses",
            MockAction::AssertThenRespond {
                status: 200,
                body: sse_body(&[
                    serde_json::json!({"type":"response.output_text.delta","delta":"ok"}),
                    serde_json::json!({"type":"response.completed","response":{"usage":{"input_tokens":1,"output_tokens":1}}}),
                ]),
                assert: Arc::new(move |body: &serde_json::Value| {
                    assert_eq!(
                        body["input"][1]["content"],
                        serde_json::json!([
                            { "type": "input_text", "text": "read" },
                            {
                                "type": "input_file",
                                "filename": "spec.pdf",
                                "file_data": expected_responses
                            }
                        ]),
                        "Responses document lowering must be byte-exact"
                    );
                }),
            },
        );
        let base = server.base_url().await;
        let provider = OpenAiProvider::build(OpenAiConfig::responses(base, None));
        let mut r = req("m1");
        r.messages.push(RequestMessage {
            role: Role::User,
            content: vec![
                ContentPart::text("read"),
                ContentPart::file_data("application/pdf", Some("spec.pdf"), pdf).unwrap(),
            ],
        });
        let mut stream = provider.stream(r);
        while let Some(chunk) = stream.next().await {
            if matches!(chunk.unwrap(), ProviderChunk::Done) {
                break;
            }
        }
        assert_eq!(server.request_count(), 1);
    }

    /// A vision-less model and an over-bound image are refused by the
    /// adapter delivery gate BEFORE any wire byte (request count stays 0).
    #[tokio::test]
    async fn image_delivery_gate_is_typed_and_pre_wire() {
        let server = MockServer::new();
        let base = server.base_url().await;
        let provider = OpenAiProvider::build(OpenAiConfig::chat(base.clone(), None).with_model(
            "m1",
            ModelCapabilities {
                vision: false,
                ..Default::default()
            },
        ));
        let mut r = req("m1");
        r.messages.push(RequestMessage {
            role: Role::User,
            content: vec![
                ContentPart::image_data("image/png", vec![0x89, b'P', b'N', b'G']).unwrap(),
            ],
        });
        let err = provider.stream(r).next().await.unwrap().unwrap_err();
        assert_eq!(err.kind, ProviderErrorKind::BadRequest);
        assert!(err.message.contains("vision"), "{err}");
        assert_eq!(server.request_count(), 0, "no wire byte on a gate refusal");

        // Over the provider's own per-image bound: typed, pre-wire.
        let provider = OpenAiProvider::build(OpenAiConfig::chat(base, None));
        let mut r = req("m1");
        r.messages.push(RequestMessage {
            role: Role::User,
            content: vec![ContentPart {
                kind: ContentKind::ImageData {
                    mime: "image/png".into(),
                    data: faktor_provider::MediaBytes::new(vec![0u8; OPENAI_MAX_IMAGE_BYTES + 1])
                        .unwrap(),
                },
                tool_call_id: None,
            }],
        });
        let err = provider.stream(r).next().await.unwrap().unwrap_err();
        assert_eq!(err.kind, ProviderErrorKind::BadRequest);
        assert!(err.message.contains("exceeds"), "{err}");
        assert_eq!(
            server.request_count(),
            0,
            "no wire byte on an oversize refusal"
        );
    }

    #[test]
    fn usage_parse_splits_cached_input_and_hostile_rows_error() {
        // Audit Phase-1 item C: `prompt_tokens` is the TOTAL input INCLUDING
        // the cached portion — the canonical frame must arrive with the
        // cached tokens split into `cache_read_tokens` (billed at the cache
        // line) and the remainder as `uncached_input_tokens`. Reasoning
        // rides inside `completion_tokens`; the detail is informational.
        let frame = serde_json::json!({
            "id": "chatcmpl-9",
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "prompt_tokens_details": {"cached_tokens": 40},
                "completion_tokens_details": {"reasoning_tokens": 30}
            }
        });
        let mut accs = Vec::new();
        let mut pending = std::collections::VecDeque::new();
        let chunk = parse_chat_chunk(&frame, &mut accs, &mut pending)
            .expect("usage chunk")
            .expect("usage frame");
        assert_eq!(
            chunk,
            ProviderChunk::Usage(CanonicalUsage {
                uncached_input_tokens: 60,
                cache_read_tokens: 40,
                cache_write_tokens: 0,
                output_tokens: 50,
                reasoning_tokens: 30,
                reported_cost: None,
                request_id: Some("chatcmpl-9".into()),
            })
        );
        // Missing cache details: the conservative category is uncached =
        // the reported total (never invent a cheaper cache line).
        let no_cache = serde_json::json!({
            "usage": {"prompt_tokens": 1000, "completion_tokens": 50}
        });
        let chunk = parse_chat_chunk(&no_cache, &mut accs, &mut pending)
            .expect("usage chunk")
            .expect("usage frame");
        assert!(matches!(
            chunk,
            ProviderChunk::Usage(CanonicalUsage {
                uncached_input_tokens: 1000,
                cache_read_tokens: 0,
                output_tokens: 50,
                request_id: None,
                ..
            })
        ));
        // Hostile: cached tokens CANNOT exceed the input total they are a
        // subset of — typed Malformed, never a silent zero/saturate.
        let hostile = serde_json::json!({
            "usage": {
                "prompt_tokens": 0,
                "completion_tokens": 0,
                "prompt_tokens_details": {"cached_tokens": 7}
            }
        });
        let err = parse_chat_chunk(&hostile, &mut accs, &mut pending)
            .expect_err("cache > input total must be Malformed");
        assert_eq!(err.kind, ProviderErrorKind::Malformed);
        assert!(!err.retryable);
        // Hostile: reasoning detail exceeding the output total.
        let hostile_reasoning = serde_json::json!({
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 3,
                "completion_tokens_details": {"reasoning_tokens": 30}
            }
        });
        let err = parse_chat_chunk(&hostile_reasoning, &mut accs, &mut pending)
            .expect_err("reasoning > output must be Malformed");
        assert_eq!(err.kind, ProviderErrorKind::Malformed);
        // An all-zero envelope carries nothing: no chunk.
        let zero = serde_json::json!({"usage": {}});
        assert!(parse_chat_chunk(&zero, &mut accs, &mut pending)
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn tool_call_accumulates_and_completes() {
        let server = MockServer::new();
        let body = sse_body(&[
            serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"read_file","arguments":"{\"path\":"}}]}}]}),
            serde_json::json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"a.rs\"}"}}]}}]}),
            serde_json::json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
        ]);
        server.route(
            "POST",
            "/chat/completions",
            MockAction::Respond { status: 200, body },
        );
        let base = server.base_url().await;
        let provider = OpenAiProvider::build(OpenAiConfig::chat(base, None));
        let mut stream = provider.stream(req("m"));
        let mut call = None;
        while let Some(chunk) = stream.next().await {
            match chunk.unwrap() {
                ProviderChunk::ToolCall {
                    name,
                    input,
                    complete,
                    ..
                } => {
                    assert!(complete);
                    call = Some((name, input));
                }
                ProviderChunk::Done => break,
                _ => {}
            }
        }
        let (name, input) = call.expect("tool call emitted");
        assert_eq!(name, "read_file");
        assert_eq!(input["path"], "a.rs");
    }

    #[tokio::test]
    async fn rate_limit_and_auth_mapped() {
        let server = MockServer::new();
        server.route(
            "POST",
            "/chat/completions",
            MockAction::Respond {
                status: 429,
                body: r#"{"error":{"message":"rate limited"}}"#.into(),
            },
        );
        let base = server.base_url().await;
        let provider = OpenAiProvider::build(OpenAiConfig::chat(base, Some("k".into())));
        let mut stream = provider.stream(req("m"));
        let err = stream.next().await.unwrap().unwrap_err();
        assert_eq!(err.kind, ProviderErrorKind::RateLimited);
        assert!(err.retryable);
    }

    #[tokio::test]
    async fn malformed_sse_line_is_malformed_error() {
        let server = MockServer::new();
        server.route(
            "POST",
            "/chat/completions",
            MockAction::Respond {
                status: 200,
                body: "data: {not json}\n\ndata: [DONE]\n\n".into(),
            },
        );
        let base = server.base_url().await;
        let provider = OpenAiProvider::build(OpenAiConfig::chat(base, None));
        let mut stream = provider.stream(req("m"));
        let first = stream.next().await.unwrap();
        assert!(first.is_err(), "malformed SSE must be an error");
    }

    #[tokio::test]
    async fn stream_ends_without_done_still_terminates() {
        let server = MockServer::new();
        server.route(
            "POST",
            "/chat/completions",
            MockAction::Respond {
                status: 200,
                body: "data: {\"choices\":[{\"delta\":{\"content\":\"x\"}}]}\n\n".into(),
            },
        );
        let base = server.base_url().await;
        let provider = OpenAiProvider::build(OpenAiConfig::chat(base, None));
        let mut stream = provider.stream(req("m"));
        let mut done = false;
        let mut got_text = false;
        while let Some(chunk) = stream.next().await {
            match chunk.unwrap() {
                ProviderChunk::Text { .. } => got_text = true,
                ProviderChunk::Done => {
                    done = true;
                    break;
                }
                _ => {}
            }
        }
        assert!(done && got_text, "stream must terminate with Done");
    }

    #[tokio::test]
    async fn network_death_maps_to_network_error() {
        // No server listening on this port: connect error.
        let provider = OpenAiProvider::build(OpenAiConfig::chat("http://127.0.0.1:1", None));
        let mut stream = provider.stream(req("m"));
        let first = stream.next().await.unwrap();
        assert!(first.is_err());
        assert_eq!(first.unwrap_err().kind, ProviderErrorKind::Network);
    }

    #[test]
    fn capabilities_default_and_override() {
        let p = OpenAiProvider::build(OpenAiConfig::chat("http://x", None));
        let caps = p.capabilities("unknown-model");
        assert!(caps.tools);
        assert_eq!(caps.context, 128_000);
        let p = OpenAiProvider::build(OpenAiConfig::chat("http://x", None).with_model(
            "small",
            ModelCapabilities {
                context: 8192,
                tools: false,
                ..Default::default()
            },
        ));
        assert!(!p.capabilities("small").tools);
        assert_eq!(p.capabilities("small").context, 8192);
    }

    #[tokio::test]
    async fn responses_body_is_native_items_not_chat_shape() {
        // The Responses codec lowers to the model's OWN item protocol:
        // system -> top-level instructions; text/images -> input_text /
        // input_image; assistant tool calls -> top-level function_call
        // items; results -> function_call_output keyed by call_id; tools ->
        // the flattened Responses tool shape. A chat-shaped body (messages /
        // nested function wrapper / assistant tool_calls) fails these
        // assertions.
        let server = MockServer::new();
        let asserted = Arc::new(std::sync::Mutex::new(None::<serde_json::Value>));
        {
            let asserted = asserted.clone();
            server.route(
                "POST",
                "/responses",
                MockAction::AssertThenRespond {
                    status: 200,
                    body: "{}".into(),
                    assert: Arc::new(move |body| {
                        *asserted.lock().unwrap() = Some(body.clone());
                    }),
                },
            );
        }
        let base = server.base_url().await;
        let provider = OpenAiProvider::build(OpenAiConfig::responses(base, None));
        let mut g = req("m");
        g.reasoning = Some(faktor_core::model::ReasoningMode::Medium);
        g.messages = vec![
            RequestMessage {
                role: Role::User,
                content: vec![
                    ContentPart::text("list src/"),
                    ContentPart {
                        kind: ContentKind::Image {
                            url: "https://example.test/a.png".into(),
                        },
                        tool_call_id: None,
                    },
                ],
            },
            RequestMessage {
                role: Role::Assistant,
                content: vec![
                    ContentPart::text("calling"),
                    ContentPart::tool_call(
                        "call_1",
                        "read_file",
                        serde_json::json!({"path": "src/a.rs"}),
                    ),
                ],
            },
            RequestMessage {
                role: Role::User,
                content: vec![ContentPart::tool_result("fn a() {}", false, "call_1")],
            },
        ];
        let mut stream = provider.stream(g);
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), stream.next()).await;
        let body = asserted.lock().unwrap().clone().expect("request asserted");
        assert_eq!(body["model"], "m");
        assert_eq!(body["stream"], true);
        // The cacheable prefix rides top-level `instructions`, never an
        // input message.
        assert_eq!(body["instructions"], "sys");
        // Native tools: flattened function rows, never chat nested ones.
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert_eq!(body["tools"][0]["parameters"]["type"], "object");
        assert!(
            body["tools"][0].get("function").is_none(),
            "tools must not use the chat wrapper: {}",
            body["tools"][0]
        );
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["max_output_tokens"], 1000);
        assert_eq!(body["reasoning"]["effort"], "medium");
        let input = body["input"].as_array().unwrap();
        assert!(
            input.iter().any(|i| i["role"] == "user"
                && i["content"][0]["type"] == "input_text"
                && i["content"][0]["text"] == "list src/"
                && i["content"][1]["type"] == "input_image"
                && i["content"][1]["image_url"] == "https://example.test/a.png"),
            "user text+images lower to native input parts: {input:?}"
        );
        let assistant = input
            .iter()
            .find(|i| i["role"] == "assistant")
            .expect("assistant text item present");
        assert_eq!(assistant["content"][0]["type"], "output_text");
        assert_eq!(assistant["content"][0]["text"], "calling");
        assert!(
            assistant.get("tool_calls").is_none(),
            "function calls are top-level items, never assistant tool_calls"
        );
        let call = input
            .iter()
            .find(|i| i["type"] == "function_call")
            .expect("top-level function_call item present");
        assert_eq!(call["call_id"], "call_1");
        assert_eq!(call["name"], "read_file");
        let args: serde_json::Value =
            serde_json::from_str(call["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["path"], "src/a.rs");
        assert!(
            input.iter().any(|i| i["type"] == "function_call_output"
                && i["call_id"] == "call_1"
                && i["output"] == "fn a() {}"),
            "tool results lower to function_call_output: {input:?}"
        );
        // Nothing chat-shaped may appear anywhere in the body.
        let rendered = serde_json::to_string(&body).unwrap();
        for banned in ["tool_result", "tool_calls", "\"function\":{", "max_tokens"] {
            assert!(!rendered.contains(banned), "{banned} leaked: {rendered}");
        }
        // Internal request metadata never leaks either.
        for leaked in ["operation_id", "session_id", "deadline_ms", "cancellation"] {
            assert!(!rendered.contains(leaked), "{leaked} leaked: {rendered}");
        }
    }

    /// One `data: <json>` SSE frame.
    fn ev(v: serde_json::Value) -> String {
        format!("data: {v}\n\n")
    }

    fn responses_provider(base: String) -> Arc<dyn Provider> {
        OpenAiProvider::build(OpenAiConfig::responses(base, None))
    }

    /// Drain a stream to its full item sequence (errors preserved).
    async fn drain(mut stream: ProviderStream) -> Vec<Result<ProviderChunk, ProviderError>> {
        let mut out = Vec::new();
        while let Some(item) = stream.next().await {
            out.push(item);
        }
        out
    }

    /// A raw HTTP/1.1 server that answers the first request with SSE chunked
    /// headers, writes `chunks` (each its own HTTP chunk), then either stalls
    /// forever with the body open (`stall = true`) or terminates the chunked
    /// body. Lets cancellation/idle tests run against a genuinely LIVE
    /// stream, not a completed one.
    async fn slow_sse_server(chunks: Vec<Vec<u8>>, stall: bool) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut head = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let Ok(n) = socket.read(&mut buf).await else {
                    return;
                };
                if n == 0 {
                    return;
                }
                head.extend_from_slice(&buf[..n]);
                if head.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let _ = socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
                )
                .await;
            for chunk in &chunks {
                let _ = socket
                    .write_all(format!("{:x}\r\n", chunk.len()).as_bytes())
                    .await;
                let _ = socket.write_all(chunk).await;
                let _ = socket.write_all(b"\r\n").await;
                let _ = socket.flush().await;
            }
            if stall {
                let mut sink = [0u8; 1024];
                while let Ok(n) = socket.read(&mut sink).await {
                    if n == 0 {
                        break;
                    }
                }
            } else {
                let _ = socket.write_all(b"0\r\n\r\n").await;
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn responses_stream_parses_text_reasoning_and_tool_calls() {
        // Native events: both reasoning delta variants, text deltas, a
        // function call whose ITEM id differs from its wire call_id and
        // whose arguments arrive fragmented, an authoritative item-done,
        // completed with usage. Ordered chunks, one assembled call, usage
        // LAST, exactly one Done.
        let server = MockServer::new();
        server.route(
            "POST",
            "/responses",
            MockAction::Sse {
                status: 200,
                events: vec![
                    ev(serde_json::json!({"type": "response.created", "response": {"id": "r1"}})),
                    ev(serde_json::json!({"type": "response.reasoning_text.delta", "item_id": "rs_1", "delta": "thinking "})),
                    ev(serde_json::json!({"type": "response.reasoning_summary_text.delta", "item_id": "rs_1", "delta": "hard"})),
                    // The real wire frames named events (`event:` line then
                    // `data:`); the named line must be skipped, never
                    // mistaken for data.
                    "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"item_id\":\"msg_1\",\"delta\":\"hello \"}\n\n".to_string(),
                    ev(serde_json::json!({"type": "response.output_text.delta", "item_id": "msg_1", "delta": "world"})),
                    ev(serde_json::json!({"type": "response.output_item.added", "output_index": 1, "item": {"type": "function_call", "id": "fc_item_1", "call_id": "call_1", "name": "read_file", "arguments": ""}})),
                    ev(serde_json::json!({"type": "response.function_call_arguments.delta", "item_id": "fc_item_1", "delta": "{\"path\":"})),
                    ev(serde_json::json!({"type": "response.function_call_arguments.delta", "item_id": "fc_item_1", "delta": "\"a.rs\"}"})),
                    ev(serde_json::json!({"type": "response.output_item.done", "item": {"type": "function_call", "id": "fc_item_1", "call_id": "call_1", "name": "read_file", "arguments": "{\"path\":\"a.rs\"}"}})),
                    ev(serde_json::json!({"type": "response.completed", "response": {"id": "r1", "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15}}})),
                    "data: [DONE]\n\n".to_string(),
                ],
            },
        );
        let base = server.base_url().await;
        let items = drain(responses_provider(base).stream(req("m"))).await;
        let text: String = items
            .iter()
            .filter_map(|i| match i {
                Ok(ProviderChunk::Text { text }) => Some(text.clone()),
                _ => None,
            })
            .collect();
        let reasoning: String = items
            .iter()
            .filter_map(|i| match i {
                Ok(ProviderChunk::Reasoning { text }) => Some(text.clone()),
                _ => None,
            })
            .collect();
        let calls: Vec<&ProviderChunk> = items
            .iter()
            .filter_map(|i| match i {
                Ok(c @ ProviderChunk::ToolCall { .. }) => Some(c),
                _ => None,
            })
            .collect();
        assert_eq!(text, "hello world");
        assert_eq!(reasoning, "thinking hard");
        assert_eq!(calls.len(), 1, "one assembled call: {items:?}");
        match calls[0] {
            ProviderChunk::ToolCall {
                id,
                name,
                input,
                complete,
            } => {
                assert_eq!(id, "call_1", "the generic id is the wire call_id");
                assert_eq!(name, "read_file");
                assert_eq!(input, &serde_json::json!({"path": "a.rs"}));
                assert!(*complete);
            }
            _ => unreachable!(),
        }
        // Usage: 10 total input / 0 cached -> uncached 10; request id kept.
        let usage_at = items
            .iter()
            .position(|i| matches!(i, Ok(ProviderChunk::Usage(_))))
            .expect("usage frame");
        assert_eq!(
            items[usage_at],
            Ok(ProviderChunk::Usage(CanonicalUsage {
                uncached_input_tokens: 10,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                output_tokens: 5,
                reasoning_tokens: 0,
                reported_cost: None,
                request_id: Some("r1".into()),
            }))
        );
        // Terminal semantics: exactly one Done, last, usage immediately
        // before it, nothing after.
        assert_eq!(
            items
                .iter()
                .filter(|i| matches!(i, Ok(ProviderChunk::Done)))
                .count(),
            1
        );
        assert_eq!(items.last(), Some(&Ok(ProviderChunk::Done)));
        assert_eq!(usage_at + 1, items.len() - 1);
        assert!(items[..usage_at].iter().all(|i| i.is_ok()));
    }

    #[tokio::test]
    async fn responses_stream_assembles_multiple_calls_with_byte_split_arguments() {
        // TWO simultaneous function calls with interleaved fragments, the
        // whole SSE body delivered in 7-byte HTTP chunks (splits fall
        // mid-line, mid-JSON and mid-string). Item ids differ from call
        // ids; call B's fragments are overwritten by an authoritative
        // `function_call_arguments.done`; call A completes from fragments +
        // `output_item.done`. Both calls must assemble independently.
        let frame = |v: serde_json::Value| format!("data: {v}\n\n");
        let frames: Vec<String> = vec![
            frame(
                serde_json::json!({"type": "response.output_item.added", "item": {"type": "function_call", "id": "fc_a", "call_id": "call_a", "name": "read_file", "arguments": ""}}),
            ),
            frame(
                serde_json::json!({"type": "response.output_item.added", "item": {"type": "function_call", "id": "fc_b", "call_id": "call_b", "name": "list_dir", "arguments": ""}}),
            ),
            frame(
                serde_json::json!({"type": "response.function_call_arguments.delta", "item_id": "fc_a", "delta": "{\"path\":"}),
            ),
            frame(
                serde_json::json!({"type": "response.function_call_arguments.delta", "item_id": "fc_b", "delta": "{\"depth\":"}),
            ),
            frame(
                serde_json::json!({"type": "response.function_call_arguments.delta", "item_id": "fc_a", "delta": "\"src/"}),
            ),
            frame(
                serde_json::json!({"type": "response.function_call_arguments.delta", "item_id": "fc_b", "delta": "9}"}),
            ),
            frame(
                serde_json::json!({"type": "response.function_call_arguments.done", "item_id": "fc_b", "arguments": "{\"depth\":2}"}),
            ),
            frame(
                serde_json::json!({"type": "response.function_call_arguments.delta", "item_id": "fc_a", "delta": "a.rs\"}"}),
            ),
            frame(
                serde_json::json!({"type": "response.output_item.done", "item": {"type": "function_call", "id": "fc_a", "call_id": "call_a", "name": "read_file", "arguments": "{\"path\":\"src/a.rs\"}"}}),
            ),
            frame(serde_json::json!({"type": "response.completed", "response": {"id": "r2"}})),
            "data: [DONE]\n\n".to_string(),
        ];
        let full = frames.concat().into_bytes();
        let chunks: Vec<Vec<u8>> = full.chunks(7).map(|c| c.to_vec()).collect();
        let server = MockServer::new();
        server.route(
            "POST",
            "/responses",
            MockAction::ChunkedSse {
                status: 200,
                chunks,
            },
        );
        let base = server.base_url().await;
        let items = drain(responses_provider(base).stream(req("m"))).await;
        let calls: Vec<&ProviderChunk> = items
            .iter()
            .filter_map(|i| match i {
                Ok(c @ ProviderChunk::ToolCall { .. }) => Some(c),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 2, "both calls assemble: {items:?}");
        match calls[0] {
            ProviderChunk::ToolCall {
                id, name, input, ..
            } => {
                assert_eq!(id, "call_a");
                assert_eq!(name, "read_file");
                assert_eq!(input, &serde_json::json!({"path": "src/a.rs"}));
            }
            _ => unreachable!(),
        }
        match calls[1] {
            ProviderChunk::ToolCall {
                id, name, input, ..
            } => {
                assert_eq!(id, "call_b");
                assert_eq!(name, "list_dir");
                assert_eq!(
                    input,
                    &serde_json::json!({"depth": 2}),
                    "the authoritative done event replaces fragments"
                );
            }
            _ => unreachable!(),
        }
        assert_eq!(
            items
                .iter()
                .filter(|i| matches!(i, Ok(ProviderChunk::Done)))
                .count(),
            1
        );
        assert_eq!(items.last(), Some(&Ok(ProviderChunk::Done)));
    }

    #[tokio::test]
    async fn responses_usage_flushes_after_tool_calls_and_before_done() {
        // A completed event carrying BOTH an unflushed function call and a
        // usage frame: the call comes first, usage is the LAST chunk before
        // the single Done.
        let server = MockServer::new();
        server.route(
            "POST",
            "/responses",
            MockAction::Sse {
                status: 200,
                events: vec![
                    ev(serde_json::json!({"type": "response.output_item.added", "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "echo", "arguments": "{\"x\":1}"}})),
                    ev(serde_json::json!({"type": "response.completed", "response": {"id": "r1", "usage": {"input_tokens": 7, "input_tokens_details": {"cached_tokens": 2}, "output_tokens": 3, "output_tokens_details": {"reasoning_tokens": 1}}}})),
                ],
            },
        );
        let base = server.base_url().await;
        let items = drain(responses_provider(base).stream(req("m"))).await;
        assert_eq!(items.len(), 3, "{items:?}");
        assert!(matches!(items[0], Ok(ProviderChunk::ToolCall { .. })));
        assert_eq!(
            items[1],
            Ok(ProviderChunk::Usage(CanonicalUsage {
                uncached_input_tokens: 5,
                cache_read_tokens: 2,
                cache_write_tokens: 0,
                output_tokens: 3,
                reasoning_tokens: 1,
                reported_cost: None,
                request_id: Some("r1".into()),
            }))
        );
        assert_eq!(items[2], Ok(ProviderChunk::Done));
    }

    #[tokio::test]
    async fn responses_malformed_frame_is_typed_malformed_and_terminal() {
        // A data line that is not JSON is a broken stream: exactly one typed
        // Malformed error, no chunks after it.
        let server = MockServer::new();
        server.route(
            "POST",
            "/responses",
            MockAction::Respond {
                status: 200,
                body: "data: {not json}\n\ndata: [DONE]\n\n".into(),
            },
        );
        let base = server.base_url().await;
        let items = drain(responses_provider(base).stream(req("m"))).await;
        assert_eq!(items.len(), 1, "{items:?}");
        let err = items[0].as_ref().expect_err("malformed");
        assert_eq!(err.kind, ProviderErrorKind::Malformed);
        assert!(!err.retryable);
    }

    #[tokio::test]
    async fn responses_unknown_item_fragment_is_dropped_never_injects() {
        // A fragment for a function call the server never announced is
        // DROPPED (event-order processing): no call is fabricated and no
        // stored call is injected; the announced call still completes.
        let server = MockServer::new();
        server.route(
            "POST",
            "/responses",
            MockAction::Sse {
                status: 200,
                events: vec![
                    ev(serde_json::json!({"type": "response.output_item.added", "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "echo", "arguments": ""}})),
                    ev(serde_json::json!({"type": "response.function_call_arguments.delta", "item_id": "fc_ghost", "delta": "{\"evil\":1}"})),
                    ev(serde_json::json!({"type": "response.function_call_arguments.delta", "item_id": "fc_1", "delta": "{\"x\":1}"})),
                    ev(serde_json::json!({"type": "response.completed", "response": {"id": "r1"}})),
                ],
            },
        );
        let base = server.base_url().await;
        let items = drain(responses_provider(base).stream(req("m"))).await;
        assert_eq!(items.len(), 2, "one call then Done: {items:?}");
        assert_eq!(
            items[0],
            Ok(ProviderChunk::ToolCall {
                id: "call_1".into(),
                name: "echo".into(),
                input: serde_json::json!({"x": 1}),
                complete: true,
            })
        );
        assert_eq!(items[1], Ok(ProviderChunk::Done));
    }

    #[tokio::test]
    async fn responses_error_events_are_typed_by_structured_code() {
        // SSE `error` events carry structured codes: auth is terminal, rate
        // limits stay retryable, unknown codes are terminal BadRequest. The
        // stream ends at the error: later deltas never surface.
        for (code, expect_kind, expect_retryable) in [
            ("invalid_api_key", ProviderErrorKind::Auth, false),
            ("rate_limit_exceeded", ProviderErrorKind::RateLimited, true),
            ("server_error", ProviderErrorKind::BadRequest, false),
        ] {
            let server = MockServer::new();
            server.route(
                "POST",
                "/responses",
                MockAction::Sse {
                    status: 200,
                    events: vec![
                        ev(serde_json::json!({"type": "response.output_text.delta", "item_id": "m1", "delta": "partial"})),
                        ev(serde_json::json!({"type": "error", "code": code, "message": "boom"})),
                        ev(serde_json::json!({"type": "response.output_text.delta", "item_id": "m1", "delta": "after"})),
                    ],
                },
            );
            let base = server.base_url().await;
            let items = drain(responses_provider(base).stream(req("m"))).await;
            assert_eq!(items.len(), 2, "{code}: {items:?}");
            assert!(matches!(items[0], Ok(ProviderChunk::Text { .. })));
            let err = items[1].as_ref().expect_err("typed error");
            assert_eq!(err.kind, expect_kind, "{code}");
            assert_eq!(err.retryable, expect_retryable, "{code}");
            assert_eq!(err.code.as_deref(), Some(code));
        }
    }

    #[tokio::test]
    async fn responses_no_chunks_survive_any_terminal_event() {
        // completion is a hard terminal: later text/reasoning deltas and a
        // trailing [DONE] never produce chunks.
        let server = MockServer::new();
        server.route(
            "POST",
            "/responses",
            MockAction::Sse {
                status: 200,
                events: vec![
                    ev(serde_json::json!({"type": "response.output_text.delta", "item_id": "m1", "delta": "before"})),
                    ev(serde_json::json!({"type": "response.completed", "response": {"id": "r1"}})),
                    ev(serde_json::json!({"type": "response.output_text.delta", "item_id": "m1", "delta": "after"})),
                    ev(serde_json::json!({"type": "response.reasoning_text.delta", "item_id": "rs", "delta": "after"})),
                    "data: [DONE]\n\n".to_string(),
                ],
            },
        );
        let base = server.base_url().await;
        let items = drain(responses_provider(base).stream(req("m"))).await;
        assert_eq!(
            items,
            vec![
                Ok(ProviderChunk::Text {
                    text: "before".into()
                }),
                Ok(ProviderChunk::Done)
            ]
        );
    }

    #[tokio::test]
    async fn responses_cancellation_mid_stream_is_cancelled_and_terminal() {
        // The server sends one real delta and then holds the stream open.
        // Cancelling mid-stream must surface one typed Cancelled error
        // promptly, then end the stream.
        let base = slow_sse_server(
            vec![ev(serde_json::json!({"type": "response.output_text.delta", "item_id": "m1", "delta": "partial"})).into_bytes()],
            true,
        )
        .await;
        let cancel = CancellationToken::new();
        let mut g = req("m");
        g.meta.cancellation = cancel.clone();
        let mut stream = responses_provider(base).stream(g);
        match stream.next().await {
            Some(Ok(ProviderChunk::Text { text })) => assert_eq!(text, "partial"),
            other => panic!("expected the first delta, got {other:?}"),
        }
        let t0 = std::time::Instant::now();
        cancel.cancel();
        let item = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("cancellation must wake the live stream")
            .expect("an error item");
        let err = item.expect_err("cancelled");
        assert_eq!(err.kind, ProviderErrorKind::Cancelled);
        assert!(!err.retryable);
        assert!(
            t0.elapsed() < std::time::Duration::from_millis(1500),
            "cancel must surface promptly: {:?}",
            t0.elapsed()
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), stream.next())
                .await
                .expect("terminal error ends the stream")
                .is_none()
        );
    }

    #[tokio::test]
    async fn responses_request_meta_deadline_bounds_silent_stream() {
        // `RequestMeta::deadline_ms` is the operation deadline: a silent
        // server must time out at the overall bound (retryable Timeout),
        // never wait out the 60s first-byte default, and end the stream.
        let server = MockServer::new();
        server.route("POST", "/responses", MockAction::Silent { status: 200 });
        let base = server.base_url().await;
        let mut g = req("m");
        g.meta.deadline_ms = 1200;
        let t0 = std::time::Instant::now();
        let mut stream = responses_provider(base).stream(g);
        let item = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("the meta deadline must terminate the silent stream")
            .expect("an error item");
        let err = item.expect_err("must be a timeout");
        assert_eq!(err.kind, ProviderErrorKind::Timeout);
        assert!(err.retryable);
        assert!(
            t0.elapsed() < std::time::Duration::from_millis(2500),
            "meta deadline must fire at its overall bound: {:?}",
            t0.elapsed()
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), stream.next())
                .await
                .expect("stream must end after the terminal timeout")
                .is_none()
        );
    }

    #[tokio::test]
    async fn responses_first_byte_and_idle_deadlines_fire() {
        // First byte: connected + silent -> Timeout before any item.
        let server = MockServer::new();
        server.route("POST", "/responses", MockAction::Silent { status: 200 });
        let base = server.base_url().await;
        let transport: Arc<dyn HttpTransport> = Arc::new(PolicyCheckedHttpTransport::permissive());
        let mut stream = Box::pin(responses_stream(
            transport,
            format!("{base}/responses"),
            authorization_headers(None),
            responses_body(&req("m")),
            StreamDeadlines {
                first_byte_ms: 300,
                idle_ms: 3000,
                overall_ms: 0,
            },
            None,
        ));
        let item = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("first-byte bound must fire")
            .expect("an error item");
        let err = item.expect_err("timeout");
        assert_eq!(err.kind, ProviderErrorKind::Timeout);
        assert!(err.retryable);

        // Idle: one delta arrives, then silence -> the idle bound fires.
        let base = slow_sse_server(
            vec![ev(serde_json::json!({"type": "response.output_text.delta", "item_id": "m1", "delta": "tick"})).into_bytes()],
            true,
        )
        .await;
        let transport: Arc<dyn HttpTransport> = Arc::new(PolicyCheckedHttpTransport::permissive());
        let mut stream = Box::pin(responses_stream(
            transport,
            format!("{base}/responses"),
            authorization_headers(None),
            responses_body(&req("m")),
            StreamDeadlines {
                first_byte_ms: 5000,
                idle_ms: 300,
                overall_ms: 0,
            },
            None,
        ));
        match stream.next().await {
            Some(Ok(ProviderChunk::Text { text })) => assert_eq!(text, "tick"),
            other => panic!("expected the first delta, got {other:?}"),
        }
        let t0 = std::time::Instant::now();
        let item = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("idle bound must fire")
            .expect("an error item");
        let err = item.expect_err("idle timeout");
        assert_eq!(err.kind, ProviderErrorKind::Timeout);
        assert!(err.retryable);
        assert!(
            t0.elapsed() < std::time::Duration::from_millis(1500),
            "idle bound must fire promptly: {:?}",
            t0.elapsed()
        );
    }

    #[tokio::test]
    async fn http_status_retry_classification_is_shared_by_both_families() {
        // ONE status classifier serves both wire families: 429/5xx are
        // retryable, auth and every other 4xx are terminal, and the
        // envelope's retryability is the provider crate's shared
        // `ProviderErrorKind::retryable()`.
        for (status, expect_kind, expect_retryable) in [
            (429u16, ProviderErrorKind::RateLimited, true),
            (500, ProviderErrorKind::Server, true),
            (503, ProviderErrorKind::Server, true),
            (400, ProviderErrorKind::BadRequest, false),
            (401, ProviderErrorKind::Auth, false),
            (403, ProviderErrorKind::Auth, false),
            (404, ProviderErrorKind::BadRequest, false),
        ] {
            for responses in [false, true] {
                let server = MockServer::new();
                let path = if responses {
                    "/responses"
                } else {
                    "/chat/completions"
                };
                server.route(
                    "POST",
                    path,
                    MockAction::Respond {
                        status,
                        body: r#"{"error":{"message":"nope"}}"#.into(),
                    },
                );
                let base = server.base_url().await;
                let provider: Arc<dyn Provider> = if responses {
                    OpenAiProvider::build(OpenAiConfig::responses(base, None))
                } else {
                    OpenAiProvider::build(OpenAiConfig::chat(base, None))
                };
                let mut stream = provider.stream(req("m"));
                let err = stream.next().await.unwrap().unwrap_err();
                assert_eq!(
                    err.kind, expect_kind,
                    "status {status} responses={responses}"
                );
                assert_eq!(
                    err.retryable, expect_retryable,
                    "status {status} responses={responses}"
                );
                assert_eq!(err.retryable, err.kind.retryable());
                assert_eq!(err.code.as_deref(), Some(status.to_string().as_str()));
            }
        }
    }

    #[tokio::test]
    async fn openai_family_dispatch_selects_body_and_endpoint() {
        // wire_body dispatch: Responses -> native item body, Chat -> chat
        // body; never both shapes.
        let responses = OpenAiProvider {
            config: OpenAiConfig::responses("http://x", None),
            transport: default_transport(),
            quirks: OpenAiQuirks::default(),
        };
        let chat = OpenAiProvider {
            config: OpenAiConfig::chat("http://x", None),
            transport: default_transport(),
            quirks: OpenAiQuirks::default(),
        };
        let r = responses.wire_body(&req("m"));
        assert!(
            r.get("input").is_some() && r.get("messages").is_none(),
            "responses body: {r}"
        );
        assert_eq!(r["stream"], true);
        let c = chat.wire_body(&req("m"));
        assert!(
            c.get("messages").is_some() && c.get("input").is_none(),
            "chat body: {c}"
        );
        assert_eq!(c["stream"], true);

        // stream() dispatch: the family picks the endpoint + parser pair
        // (chat keeps its own URL, locked here for the pair).
        for (responses, expected_path) in [
            (true, "http://mock.invalid/responses"),
            (false, "http://mock.invalid/chat/completions"),
        ] {
            let mock = Arc::new(MockHttpTransport::new(200, "data: [DONE]\n\n"));
            let transport: Arc<dyn HttpTransport> = mock.clone();
            let config = if responses {
                OpenAiConfig::responses("http://mock.invalid", None)
            } else {
                OpenAiConfig::chat("http://mock.invalid", None)
            };
            let provider = OpenAiProvider::build_with_transport(config, transport);
            let items = drain(provider.stream(req("m"))).await;
            assert_eq!(items, vec![Ok(ProviderChunk::Done)]);
            assert_eq!(
                mock.requests(),
                vec![("POST".to_string(), expected_path.to_string())],
                "responses={responses}"
            );
        }
    }
    #[tokio::test]
    async fn assistant_tool_call_and_tool_result_lower_to_wire_messages() {
        // (a) The exact request shape after a tool runs: the assistant turn
        // carries `tool_calls` (NOT {type:"tool_call"} content blocks) and
        // the result is a separate role:"tool" message (NOT a
        // {type:"tool_result"} block inside a user message).
        let server = MockServer::new();
        server.route(
            "POST",
            "/chat/completions",
            MockAction::AssertThenRespond {
                status: 200,
                body: String::new(),
                assert: Arc::new(|body: &serde_json::Value| {
                    let raw = serde_json::to_string(body).unwrap();
                    for banned in ["\"type\":\"tool_call\"", "\"type\":\"tool_result\""] {
                        assert!(
                            !raw.contains(banned),
                            "lowered body must not contain {banned}: {raw}"
                        );
                    }
                    let msgs = body["messages"].as_array().expect("messages array");
                    assert_eq!(msgs.len(), 2, "assistant + tool message");
                    assert_eq!(msgs[0]["role"], "assistant");
                    // content is text-only; the call rides tool_calls.
                    assert_eq!(msgs[0]["content"].as_array().unwrap().len(), 1);
                    assert_eq!(msgs[0]["content"][0]["type"], "text");
                    let tc = &msgs[0]["tool_calls"][0];
                    assert_eq!(tc["id"], "call_1");
                    assert_eq!(tc["type"], "function");
                    assert_eq!(tc["function"]["name"], "echo");
                    assert_eq!(
                        tc["function"]["arguments"], r#"{"x":1}"#,
                        "arguments must be the JSON STRING of the object"
                    );
                    assert_eq!(msgs[1]["role"], "tool");
                    assert_eq!(msgs[1]["tool_call_id"], "call_1");
                    assert_eq!(msgs[1]["content"], "echo: {\"x\":1}");
                    assert!(
                        msgs[1].get("tool_calls").is_none(),
                        "tool messages never carry tool_calls"
                    );
                }),
            },
        );
        let base = server.base_url().await;
        let provider = OpenAiProvider::build(OpenAiConfig::chat(base, None));
        let mut r = req("m");
        r.messages = vec![
            RequestMessage {
                role: Role::Assistant,
                content: vec![
                    ContentPart::text("calling now"),
                    ContentPart::tool_call("call_1", "echo", serde_json::json!({"x": 1})),
                ],
            },
            RequestMessage {
                role: Role::User,
                content: vec![ContentPart::tool_result("echo: {\"x\":1}", false, "call_1")],
            },
        ];
        let mut stream = provider.stream(r);
        let mut done = false;
        while let Some(chunk) = stream.next().await {
            if let Ok(ProviderChunk::Done) = chunk {
                done = true;
                break;
            }
        }
        assert!(done);
    }

    #[tokio::test]
    async fn parallel_tool_calls_lower_to_two_tool_calls_entries() {
        // (b) One assistant turn with two parallel calls → two tool_calls
        // entries with distinct ids and stringified JSON arguments; content
        // stays text-only.
        let server = MockServer::new();
        server.route(
            "POST",
            "/chat/completions",
            MockAction::AssertThenRespond {
                status: 200,
                body: String::new(),
                assert: Arc::new(|body: &serde_json::Value| {
                    let raw = serde_json::to_string(body).unwrap();
                    for banned in ["\"type\":\"tool_call\"", "\"type\":\"tool_result\""] {
                        assert!(!raw.contains(banned), "no tool blocks allowed: {raw}");
                    }
                    let msgs = body["messages"].as_array().unwrap();
                    assert_eq!(msgs.len(), 1);
                    let calls = msgs[0]["tool_calls"].as_array().unwrap();
                    assert_eq!(calls.len(), 2);
                    let ids: Vec<&str> = calls.iter().map(|c| c["id"].as_str().unwrap()).collect();
                    assert_eq!(ids, vec!["call_a", "call_b"], "ids must stay distinct");
                    assert_eq!(calls[0]["function"]["name"], "read_file");
                    assert_eq!(calls[0]["function"]["arguments"], r#"{"path":"a.rs"}"#);
                    assert_eq!(calls[1]["function"]["name"], "list_dir");
                    assert_eq!(
                        calls[1]["function"]["arguments"],
                        r#"{"path":"src","depth":2}"#
                    );
                    let content = msgs[0]["content"].as_array().unwrap();
                    assert_eq!(content.len(), 1, "text-only content");
                    assert_eq!(content[0]["type"], "text");
                }),
            },
        );
        let base = server.base_url().await;
        let provider = OpenAiProvider::build(OpenAiConfig::chat(base, None));
        let mut r = req("m");
        r.messages = vec![RequestMessage {
            role: Role::Assistant,
            content: vec![
                ContentPart::text("two calls"),
                ContentPart::tool_call("call_a", "read_file", serde_json::json!({"path": "a.rs"})),
                ContentPart::tool_call(
                    "call_b",
                    "list_dir",
                    serde_json::json!({"path": "src", "depth": 2}),
                ),
            ],
        }];
        let mut stream = provider.stream(r);
        while let Some(chunk) = stream.next().await {
            if let Ok(ProviderChunk::Done) = chunk {
                break;
            }
        }
    }

    #[tokio::test]
    async fn two_index_keyed_tool_calls_accumulate_independently() {
        // (c) Two SIMULTANEOUS tool calls whose fragments interleave across
        // frames: each index accumulates its own arguments and both complete
        // at the finishing marker, in index order.
        let server = MockServer::new();
        let body = sse_body(&[
            serde_json::json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"id":"c1","function":{"name":"read_file","arguments":"{\"path\":"}},
                {"index":1,"id":"c2","function":{"name":"sum","arguments":"{\"nums\":"}}
            ]}}]}),
            serde_json::json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"function":{"arguments":"\"a.rs\""}},
                {"index":1,"function":{"arguments":"[1,2]"}}
            ]}}]}),
            serde_json::json!({"choices":[{"delta":{"tool_calls":[
                {"index":0,"function":{"arguments":"}"}},
                {"index":1,"function":{"arguments":"}"}}
            ]}}]}),
            serde_json::json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}),
        ]);
        server.route(
            "POST",
            "/chat/completions",
            MockAction::Respond { status: 200, body },
        );
        let base = server.base_url().await;
        let provider = OpenAiProvider::build(OpenAiConfig::chat(base, None));
        let mut stream = provider.stream(req("m"));
        let mut calls: Vec<(String, serde_json::Value, bool)> = Vec::new();
        while let Some(chunk) = stream.next().await {
            match chunk.unwrap() {
                ProviderChunk::ToolCall {
                    id,
                    name: _,
                    input,
                    complete,
                } => calls.push((id, input, complete)),
                ProviderChunk::Done => break,
                _ => {}
            }
        }
        assert_eq!(calls.len(), 2, "both calls must complete: {calls:?}");
        assert!(
            calls.iter().all(|(_, _, complete)| *complete),
            "chunks appear only at the finishing marker, marked complete"
        );
        assert_eq!(calls[0].0, "c1", "index 0 flushes first");
        assert_eq!(calls[0].1["path"], "a.rs");
        assert_eq!(calls[1].0, "c2");
        assert_eq!(calls[1].1["nums"], serde_json::json!([1, 2]));
    }

    #[tokio::test]
    async fn sse_frame_split_across_http_chunks_assembles() {
        // Adversarial transport: a well-behaved server whose frame is
        // fragmented MID-LINE by HTTP chunking. The old per-chunk
        // `.lines()` code corrupted this into garbage lines.
        let server = MockServer::new();
        server.route(
            "POST",
            "/chat/completions",
            MockAction::ChunkedSse {
                status: 200,
                chunks: vec![
                    b"data: {\"choices\":[{\"delta\":{\"content\":\"par".to_vec(),
                    b"tial reply\"}}]}".to_vec(),
                    b"\n\n".to_vec(),
                    b"data: [DONE]\n\n".to_vec(),
                ],
            },
        );
        let base = server.base_url().await;
        let provider = OpenAiProvider::build(OpenAiConfig::chat(base, None));
        let mut stream = provider.stream(req("gpt-x"));
        let mut text = String::new();
        let mut done = false;
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(ProviderChunk::Text { text: t }) => text.push_str(&t),
                Ok(ProviderChunk::Done) => {
                    done = true;
                    break;
                }
                Err(e) => panic!("fragmented SSE must assemble, got {e:?}"),
                _ => {}
            }
        }
        assert!(done);
        assert_eq!(text, "partial reply");
    }

    #[tokio::test]
    async fn multibyte_rune_split_across_http_chunks_assembles() {
        // Split "héllo" between the two bytes of é across HTTP chunks.
        let server = MockServer::new();
        let e = "é".as_bytes();
        let mut c1 = b"data: {\"choices\":[{\"delta\":{\"content\":\"h".to_vec();
        c1.push(e[0]);
        let mut c2 = vec![e[1]];
        c2.extend_from_slice(b"llo\"}}]}");
        server.route(
            "POST",
            "/chat/completions",
            MockAction::ChunkedSse {
                status: 200,
                chunks: vec![c1, c2, b"\n\n".to_vec(), b"data: [DONE]\n\n".to_vec()],
            },
        );
        let base = server.base_url().await;
        let provider = OpenAiProvider::build(OpenAiConfig::chat(base, None));
        let mut stream = provider.stream(req("gpt-x"));
        let mut text = String::new();
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(ProviderChunk::Text { text: t }) => text.push_str(&t),
                Ok(ProviderChunk::Done) => break,
                Err(e) => panic!("split rune must assemble, got {e:?}"),
                _ => {}
            }
        }
        assert_eq!(text, "héllo");
    }

    #[tokio::test]
    async fn oversized_sse_line_is_loud_error_and_stream_ends() {
        // Hostile: one giant unbroken line. Bounded memory + loud error.
        let mut big = "data: ".to_string();
        big.extend(std::iter::repeat_n('x', 2 * 1024 * 1024));
        let server = MockServer::new();
        server.route(
            "POST",
            "/chat/completions",
            MockAction::ChunkedSse {
                status: 200,
                chunks: vec![big.into_bytes(), b"\n\ndata: [DONE]\n\n".to_vec()],
            },
        );
        let base = server.base_url().await;
        let provider = OpenAiProvider::build(OpenAiConfig::chat(base, None));
        let mut stream = provider.stream(req("gpt-x"));
        let mut saw_err = false;
        while let Some(chunk) = stream.next().await {
            match chunk {
                Err(e) if e.kind == ProviderErrorKind::Malformed => {
                    saw_err = true;
                    break;
                }
                Ok(_) => {}
                Err(e) => panic!("expected Malformed, got {e:?}"),
            }
        }
        assert!(saw_err, "oversized SSE line must be a loud Malformed error");
    }

    #[tokio::test]
    async fn request_meta_deadline_bounds_silent_stream() {
        // Audit round 15: `RequestMeta::deadline_ms` is the operation
        // deadline. A silent server with meta.deadline_ms = 1200 must error
        // Timeout at the overall bound (~1.2s, well inside 2.5s) instead of
        // waiting out the 60s first-byte default.
        let server = MockServer::new();
        server.route(
            "POST",
            "/chat/completions",
            MockAction::Silent { status: 200 },
        );
        let base = server.base_url().await;
        let provider = OpenAiProvider::build(OpenAiConfig::chat(base, None));
        let mut g = req("gpt-x");
        g.meta.deadline_ms = 1200;
        let mut stream = provider.stream(g);
        let t0 = std::time::Instant::now();
        let item = tokio::time::timeout(std::time::Duration::from_secs(5), stream.next())
            .await
            .expect("the meta deadline must terminate the silent stream")
            .expect("an error item");
        let err = item.expect_err("must be a timeout");
        assert_eq!(err.kind, ProviderErrorKind::Timeout);
        assert!(err.retryable);
        assert!(
            t0.elapsed() < std::time::Duration::from_millis(2500),
            "meta deadline must fire at its overall bound: {:?}",
            t0.elapsed()
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(300), stream.next())
                .await
                .expect("the stream must end after the terminal error")
                .is_none(),
            "no further events after the meta-deadline timeout"
        );
    }

    #[tokio::test]
    async fn silent_server_never_hangs_the_stream() {
        // Audit round 9 (P1): an already-connected provider that sends
        // nothing must surface a retryable Timeout — the transport guard's
        // first-byte deadline — instead of hanging the turn forever.
        let server = MockServer::new();
        server.route(
            "POST",
            "/chat/completions",
            MockAction::Silent { status: 200 },
        );
        let base = server.base_url().await;
        let transport: Arc<dyn HttpTransport> = Arc::new(PolicyCheckedHttpTransport::permissive());
        let headers = authorization_headers(None);
        let deadlines = faktor_provider::transport::StreamDeadlines {
            first_byte_ms: 300,
            idle_ms: 300,
            overall_ms: 3000,
        };
        let body =
            serde_json::json!({"model": "gpt-x", "messages": [{"role": "user", "content": "hi"}]});
        let mut stream = Box::pin(openai_stream(
            transport,
            format!("{base}/chat/completions"),
            headers,
            vec![],
            body,
            deadlines,
            None,
        ));
        let item = tokio::time::timeout(std::time::Duration::from_secs(10), stream.next())
            .await
            .expect("silent server must terminate via the transport guard")
            .expect("an error item");
        let err = item.expect_err("must be an error");
        assert!(
            matches!(err.kind, ProviderErrorKind::Timeout),
            "expected retryable timeout: {err:?}"
        );
    }

    // ------------------------------------------------------- egress (P0-36)

    fn allow_only(port: u16) -> Arc<dyn HttpTransport> {
        Arc::new(PolicyCheckedHttpTransport::with_policy(Some(
            DestinationPolicy::parse_lines([&format!("http://127.0.0.1:{port}")]).unwrap(),
        )))
    }

    fn https_only(port: u16) -> Arc<dyn HttpTransport> {
        Arc::new(PolicyCheckedHttpTransport::with_policy(Some(
            DestinationPolicy::parse_lines([&format!("https://127.0.0.1:{port}")]).unwrap(),
        )))
    }

    async fn first_error(mut stream: ProviderStream) -> ProviderError {
        stream
            .next()
            .await
            .expect("an item")
            .expect_err("the first item must be the deny error")
    }

    #[tokio::test]
    async fn egress_allowlist_gates_the_chat_path_before_connect() {
        let server = MockServer::new();
        server.route(
            "POST",
            "/chat/completions",
            MockAction::Respond {
                status: 200,
                body: sse_body(&[serde_json::json!({
                    "choices":[{"delta":{"content":"allowed"},"finish_reason":"stop"}]
                })]),
            },
        );
        let base = server.base_url().await;
        let port = reqwest::Url::parse(&base).unwrap().port().unwrap();

        // Allowed: the mock is exactly the allowlisted destination; the
        // SSE response streams normally through the injected transport.
        let provider = OpenAiProvider::build_with_transport(
            OpenAiConfig::chat(base.clone(), None),
            allow_only(port),
        );
        let mut stream = provider.stream(req("gpt-x"));
        let mut text = String::new();
        while let Some(chunk) = stream.next().await {
            match chunk.unwrap() {
                ProviderChunk::Text { text: t } => text.push_str(&t),
                ProviderChunk::Done => break,
                _ => {}
            }
        }
        assert_eq!(text, "allowed");
        assert_eq!(server.request_count(), 1);

        // A second instance whose policy allows a DIFFERENT port: the same
        // request is denied BEFORE any network byte (counter stays at 1).
        let denied = OpenAiProvider::build_with_transport(
            OpenAiConfig::chat(base.clone(), None),
            allow_only(port.wrapping_add(1)),
        );
        let err = first_error(denied.stream(req("gpt-x"))).await;
        assert!(err.message.contains("denied"), "{}", err.message);
        assert!(!err.retryable, "denied destinations are never retried");
        assert_eq!(server.request_count(), 1, "deny happened before connect");

        // https-to-http mismatch: an https-only allowlist rule denies the
        // plain-http request before connect.
        let mismatch =
            OpenAiProvider::build_with_transport(OpenAiConfig::chat(base, None), https_only(port));
        let err = first_error(mismatch.stream(req("gpt-x"))).await;
        assert!(err.message.contains("denied"), "{}", err.message);
        assert_eq!(server.request_count(), 1, "scheme mismatch: no connect");
    }

    #[tokio::test]
    async fn egress_allowlist_gates_the_responses_codec_path_too() {
        // The Responses family runs a SECOND request path (/responses); the
        // injected transport must gate it identically.
        let server = MockServer::new();
        server.route(
            "POST",
            "/responses",
            MockAction::Sse {
                status: 200,
                events: vec![
                    "data: {\"type\":\"response.output_text.delta\",\"item_id\":\"m1\",\"delta\":\"hi\"}\n\n".into(),
                    "data: {\"type\":\"response.completed\"}\n\n".into(),
                    "data: [DONE]\n\n".into(),
                ],
            },
        );
        let base = server.base_url().await;
        let port = reqwest::Url::parse(&base).unwrap().port().unwrap();
        let provider = OpenAiProvider::build_with_transport(
            OpenAiConfig::responses(base.clone(), None),
            allow_only(port),
        );
        let mut stream = provider.stream(req("m"));
        let mut text = String::new();
        while let Some(chunk) = stream.next().await {
            match chunk.unwrap() {
                ProviderChunk::Text { text: t } => text.push_str(&t),
                ProviderChunk::Done => break,
                _ => {}
            }
        }
        assert_eq!(text, "hi");
        assert_eq!(server.request_count(), 1);

        let denied = OpenAiProvider::build_with_transport(
            OpenAiConfig::responses(base, None),
            allow_only(port.wrapping_add(1)),
        );
        let err = first_error(denied.stream(req("m"))).await;
        assert!(err.message.contains("denied"), "{}", err.message);
        assert_eq!(server.request_count(), 1, "no connect on the deny path");
    }

    #[tokio::test]
    async fn non_sse_response_and_mock_transport_prove_no_real_http_needed() {
        // Non-SSE (plain JSON) endpoint response: the transport is used and
        // the stream still terminates cleanly.
        let server = MockServer::new();
        server.route(
            "POST",
            "/chat/completions",
            MockAction::Respond {
                status: 200,
                body: "{\"ok\": true}".into(),
            },
        );
        let base = server.base_url().await;
        let port = reqwest::Url::parse(&base).unwrap().port().unwrap();
        let provider = OpenAiProvider::build_with_transport(
            OpenAiConfig::chat(base.clone(), None),
            allow_only(port),
        );
        let mut stream = provider.stream(req("gpt-x"));
        let mut done = false;
        while let Some(chunk) = stream.next().await {
            if let Ok(ProviderChunk::Done) = chunk {
                done = true;
                break;
            }
        }
        assert!(done, "a plain (non-SSE) response must end the stream");
        assert_eq!(
            server.request_count(),
            1,
            "the transport carried the request"
        );

        // MockHttpTransport: a canned SSE body drives the adapter's parser
        // with NO network involved at all (no server exists here).
        let canned = sse_body(&[serde_json::json!({
            "choices":[{"delta":{"content":"canned"},"finish_reason":"stop"}]
        })]);
        let mock = Arc::new(MockHttpTransport::new(200, canned));
        let as_transport: Arc<dyn HttpTransport> = mock.clone();
        let provider = OpenAiProvider::build_with_transport(
            OpenAiConfig::chat("http://mock.invalid", None),
            as_transport,
        );
        let mut stream = provider.stream(req("gpt-x"));
        let mut text = String::new();
        while let Some(chunk) = stream.next().await {
            match chunk.unwrap() {
                ProviderChunk::Text { text: t } => text.push_str(&t),
                ProviderChunk::Done => break,
                _ => {}
            }
        }
        assert_eq!(text, "canned");
        assert_eq!(mock.request_count(), 1, "the transport was executed");
        assert_eq!(
            mock.requests(),
            vec![(
                "POST".to_string(),
                "http://mock.invalid/chat/completions".to_string()
            )]
        );
    }

    // ------------------------------------------------- canonical usage

    /// Shared canonical-usage conformance for the Chat Completions wire
    /// (audit Phase-1 item C): mock frames shaped exactly like real SSE
    /// usage frames (`prompt_tokens` totals including the cached portion,
    /// detail objects, top-level request id) drive the REAL provider.
    mod canonical_usage_conformance {
        use super::*;
        use faktor_provider::canonical_usage_conformance;
        use faktor_provider::CanonicalUsage;

        /// A real-wire chat usage frame. `junk` adds unknown fields at
        /// every level (unknown fields must never panic).
        fn usage_frame(
            prompt: u64,
            completion: u64,
            cached: Option<u64>,
            reasoning: Option<u64>,
            id: Option<&str>,
            junk: bool,
        ) -> serde_json::Value {
            let mut usage = serde_json::json!({
                "prompt_tokens": prompt,
                "completion_tokens": completion,
            });
            if let Some(c) = cached {
                usage["prompt_tokens_details"] =
                    serde_json::json!({"cached_tokens": c, "audio_tokens": 0});
            }
            if let Some(r) = reasoning {
                usage["completion_tokens_details"] = serde_json::json!({"reasoning_tokens": r});
            }
            if junk {
                usage["prompt_tokens_details"] =
                    serde_json::json!({"cached_tokens": 0, "totally_unknown": {"n": [1, 2]}});
                usage["unknown_usage_field"] = serde_json::json!("x");
                usage["cost"] = serde_json::json!({"currency": "usd", "amount": 0.01});
            }
            let mut frame = serde_json::json!({"choices": [{"delta": {}}], "usage": usage});
            if let Some(id) = id {
                frame["id"] = serde_json::json!(id);
            }
            if junk {
                frame["unknown_top"] = serde_json::json!([1, {"a": true}]);
                frame["created"] = serde_json::json!(0);
            }
            frame
        }

        fn sse(v: serde_json::Value) -> String {
            sse_body(&[v])
        }

        fn exp(
            uncached: u64,
            cache_read: u64,
            output: u64,
            reasoning: u64,
            request_id: Option<&str>,
        ) -> CanonicalUsage {
            CanonicalUsage {
                uncached_input_tokens: uncached,
                cache_read_tokens: cache_read,
                cache_write_tokens: 0,
                output_tokens: output,
                reasoning_tokens: reasoning,
                reported_cost: None,
                request_id: request_id.map(str::to_string),
            }
        }

        canonical_usage_conformance! {
            driver: chat_family_canonical_usage_conformance,
            family: faktor_provider::usage_conformance::WireFamily::InclusiveTotal,
            label: "openai chat completions",
            request: || req("m1"),
            provider: |base: String| OpenAiProvider::build(OpenAiConfig::chat(base, None)),
            method: "POST",
            path: "/chat/completions",
            cases: vec![
                faktor_provider::usage_conformance::WireUsageCase::frame(
                    "total_incl_cached_split",
                    sse(usage_frame(1000, 50, Some(600), None, None, false)),
                    exp(400, 600, 50, 0, None),
                ),
                faktor_provider::usage_conformance::WireUsageCase::frame(
                    "cache_detail_missing_uncached_total",
                    sse(usage_frame(1000, 50, None, None, None, false)),
                    exp(1000, 0, 50, 0, None),
                ),
                faktor_provider::usage_conformance::WireUsageCase::malformed(
                    "hostile_cache_over_total",
                    sse(usage_frame(100, 50, Some(600), None, None, false)),
                ),
                faktor_provider::usage_conformance::WireUsageCase::frame(
                    "reasoning_subset_inside_output",
                    sse(usage_frame(1000, 50, None, Some(30), None, false)),
                    exp(1000, 0, 50, 30, None),
                ),
                faktor_provider::usage_conformance::WireUsageCase::malformed(
                    "hostile_reasoning_over_output",
                    sse(usage_frame(1000, 20, None, Some(30), None, false)),
                ),
                faktor_provider::usage_conformance::WireUsageCase::frame(
                    "unknown_fields_never_panic",
                    sse(usage_frame(1000, 50, Some(0), None, None, true)),
                    exp(1000, 0, 50, 0, None),
                ),
                faktor_provider::usage_conformance::WireUsageCase::frame(
                    "request_id_preserved",
                    sse(usage_frame(1000, 50, None, None, Some("chatcmpl-conf-1"), false)),
                    exp(1000, 0, 50, 0, Some("chatcmpl-conf-1")),
                ),
            ]
        }
    }

    /// Canonical-usage conformance for the native Responses wire: the usage
    /// envelope rides `response.completed.response.usage`, and `input_tokens`
    /// totals INCLUDE the cached portion (InclusiveTotal family). The driver
    /// proves each frame is canonical and is the LAST chunk before Done;
    /// hostile rows are typed Malformed.
    mod responses_canonical_usage_conformance {
        use super::*;
        use faktor_provider::canonical_usage_conformance;
        use faktor_provider::CanonicalUsage;

        /// A real-wire `response.completed` envelope. `junk` adds unknown
        /// fields at every level (unknown fields must never panic).
        fn completed(
            input: u64,
            output: u64,
            cached: Option<u64>,
            reasoning: Option<u64>,
            id: Option<&str>,
            junk: bool,
        ) -> String {
            let mut usage = serde_json::json!({
                "input_tokens": input,
                "output_tokens": output,
                "total_tokens": input + output,
            });
            if let Some(c) = cached {
                usage["input_tokens_details"] =
                    serde_json::json!({"cached_tokens": c, "audio_tokens": 0});
            }
            if let Some(r) = reasoning {
                usage["output_tokens_details"] = serde_json::json!({"reasoning_tokens": r});
            }
            if junk {
                usage["unknown_usage_field"] = serde_json::json!("x");
                usage["input_tokens_details"] =
                    serde_json::json!({"cached_tokens": 0, "totally_unknown": {"n": [1, 2]}});
            }
            let mut response = serde_json::json!({"usage": usage, "status": "completed"});
            if let Some(id) = id {
                response["id"] = serde_json::json!(id);
            }
            if junk {
                response["unknown_response_field"] = serde_json::json!([1, {"a": true}]);
            }
            let mut event = serde_json::json!({"type": "response.completed", "response": response});
            if junk {
                event["unknown_event_field"] = serde_json::json!({"z": 1});
            }
            format!("data: {event}\n\ndata: [DONE]\n\n")
        }

        fn exp(
            uncached: u64,
            cache_read: u64,
            output: u64,
            reasoning: u64,
            request_id: Option<&str>,
        ) -> CanonicalUsage {
            CanonicalUsage {
                uncached_input_tokens: uncached,
                cache_read_tokens: cache_read,
                cache_write_tokens: 0,
                output_tokens: output,
                reasoning_tokens: reasoning,
                reported_cost: None,
                request_id: request_id.map(str::to_string),
            }
        }

        canonical_usage_conformance! {
            driver: responses_family_canonical_usage_conformance,
            family: faktor_provider::usage_conformance::WireFamily::InclusiveTotal,
            label: "openai responses",
            request: || req("m1"),
            provider: |base: String| OpenAiProvider::build(OpenAiConfig::responses(base, None)),
            method: "POST",
            path: "/responses",
            cases: vec![
                faktor_provider::usage_conformance::WireUsageCase::frame(
                    "total_incl_cached_split",
                    completed(1000, 50, Some(600), None, None, false),
                    exp(400, 600, 50, 0, None),
                ),
                faktor_provider::usage_conformance::WireUsageCase::frame(
                    "cache_detail_missing_uncached_total",
                    completed(1000, 50, None, None, None, false),
                    exp(1000, 0, 50, 0, None),
                ),
                faktor_provider::usage_conformance::WireUsageCase::malformed(
                    "hostile_cache_over_total",
                    completed(100, 50, Some(600), None, None, false),
                ),
                faktor_provider::usage_conformance::WireUsageCase::frame(
                    "reasoning_subset_inside_output",
                    completed(1000, 50, None, Some(30), None, false),
                    exp(1000, 0, 50, 30, None),
                ),
                faktor_provider::usage_conformance::WireUsageCase::malformed(
                    "hostile_reasoning_over_output",
                    completed(1000, 20, None, Some(30), None, false),
                ),
                faktor_provider::usage_conformance::WireUsageCase::frame(
                    "unknown_fields_never_panic",
                    completed(1000, 50, Some(0), None, None, true),
                    exp(1000, 0, 50, 0, None),
                ),
                faktor_provider::usage_conformance::WireUsageCase::frame(
                    "request_id_preserved",
                    completed(1000, 50, None, None, Some("resp-conf-1"), false),
                    exp(1000, 0, 50, 0, Some("resp-conf-1")),
                ),
            ]
        }
    }
}
